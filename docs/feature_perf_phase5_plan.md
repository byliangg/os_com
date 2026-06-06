# Asterinas ext4 性能优化 Phase 5 — 计划（延迟归因驱动）

首次创建时间：2026-06-02（Asia/Shanghai）

## 阶段定位

Phase 5 是 **性能优化主线**，承接已收口的 feature_pagecache_phase4（PageCache buffered I/O / mmap 接入，守底回归全绿）。Phase 4 已经把"PageCache 只是 buffered/mmap 一支、不解释 O_DIRECT fio"这件事用数据钉死，本阶段把优化目标拉回 **O_DIRECT / raw block / ext4 direct I/O 路径的端到端延迟**。

核心方法论（学长指导 + Claude/Codex 三方对齐结论）：

> **先做延迟归因（latency attribution），再做优化。** 不靠猜——统计读写操作端到端延迟，拆解各阶段在端到端延迟中的占比，定位真正的瓶颈段，再针对性优化。

关键认知更正（Phase 5 起点）：**所需的 instrumentation 已经端到端建好，不需要重造。** 真正缺的是"打开它、对着 sweep 的 workload 跑、把占比表收割出来"这个动作。详见 [§3 已有 profiling 盘点](#3-已有-profiling-基建盘点不要重造轮子)。

## 目标

1. fio 顺序读写守底（`direct=1, bs=1M, numjobs=1`，cache-off）：read 已达标（~102%），**write 从 ~51% 提升，冲 >= 90%**；
2. 解决小块（4K/16K）O_DIRECT 路径中 ext4 自身的 per-request 固定开销；
3. 排查并改善 O_DIRECT read 的多 job 退化（并发锁竞争）；
4. 满足赛题"探索的性能优化技术实现 >= 5% 性能提升、形成可复用 RustOS 文件系统优化方法"的优秀档与创新性要求；
5. 全程 correctness-first：Phase 2/3/4 守底回归不回退。

## 1. 诚实基线（cache-off 守底口径）

数据来源：`fio_direct_parameter_sweep_report.md`（2026-05-18/19，分支 `jbd-phase-4-pagecache` / `9cfb36a6d`）。

> **口径纠偏**：milestone 历史上挂的 `read 127% / write 39%` 是 speculative direct-read cache **开**的数，不能用于答辩。Phase 5 一律使用 cache-off 诚实口径：`EXT4_DIRECT_READ_CACHE=0` + `EXT4_PAGE_CACHE=0`。

官方守底（A 组，`direct=1, bs=1M, numjobs=1`）：

| case | rw | Asterinas MB/s | Linux MB/s | ratio |
|------|----|---------------:|-----------:|------:|
| A1 | write | 1707.0 | 3308.0 | **51.60%** |
| A2 | read | 2821.0 | 2768.0 | **101.91%** |

结论：read 基本能交差，**write 是主 blocker**。

## 2. 三个独立瓶颈（不是一个写问题）

把 sweep 的 B/C/F 三组叠起来看，性能差距来自三个互不相同、要用不同手段修的地方：

| 瓶颈 | 证据 | 在哪一层 | ext4 自身的锅？ |
|------|------|---------|----------------|
| **① 大块单 job 吞吐天花板** | raw write nj1 仅 **49–58%**，而 ext4j write / raw write = **105%**（B 组派生比） | **block / virtio-blk**，在 ext4 之下 | ❌ 不是。ext4 在 1M 上≈零开销叠在 raw 上 |
| **② 小块 per-request 开销** | 4K：raw write 52% → ext4 **20%**（保留 raw 的 37%）；4K read ext4 仅 raw 的 **18%**（C 组） | **ext4 direct 路径** | ✅ 是。每个 I/O 固定成本被小块放大 |
| **③ 并发扩展性** | write nj2 journaled 62% vs nojournal 87%（JBD2 竞争）；read nj1 100% → nj2 **68%** → nj4 72%（读随并发退化）（F 组） | **ext4 锁 / JBD2 提交** | ✅ 是 |
| ④ buffered / PageCache | D2 write 6%、D3 write 1.74%、D4 direct+pagecache write 1.30% | PageCache coherency | Phase 4 自己的债，**不在 Phase 5 主线** |

### 数据上已洗清嫌疑（不投精力）

1. **JBD2 不是单 job 写的瓶颈**：journaled 1839 ≈ nojournal 1852（差 0.7%）。学长时序图里"写日志三段串行"在单 job 1M 下几乎不花钱。
2. **ext4 大块 direct 路径不是瓶颈**：ext4 ≈ raw（105%）。1M 写慢是因为 raw 本身只有 49%——慢在 virtio-blk 单请求往返延迟（Asterinas 每个 1M 写延迟约 Linux 2 倍）。
3. **单 job 写的天花板在块层，多 job 能打满**：raw write nj2 **104%** / nj4 107%；ext4j write nj4 **95.01%**。缺的是请求 overlap / 队列深度，不是绝对带宽。

### F 组（numjobs sweep，最有信息量）

| target | rw | nj1 | nj2 | nj4 |
|--------|----|----:|----:|----:|
| raw | write | 58.20% | 104.27% | 107.15% |
| ext4j | write | 53.81% | 62.17% | 95.01% |
| ext4n | write | 65.60% | 87.51% | 88.58% |
| ext4j | read | 100.72% | 68.16% | 72.06% |

写随并发上升、读随并发**退化**——两个方向是两个问题。

## 3. 已有 profiling 基建盘点（不要重造轮子）

代码里已经有 **FS / virtio / 锁 / JBD2 四层** ns 级延迟统计，门控 `ext4fs.phase2_profile=1` + `set_write_bio_profile_enabled`：

| 层 | 统计 | 位置 |
|----|------|------|
| **ext4 写阶段** | `plan_ns / prepare_ns / data_bio_ns / bio_alloc_ns / bio_copy_ns / bio_submit_ns / bio_wait_ns / touch_ns / total_ns`，按 hit/miss 拆 + 记 max 尾延迟 | `kernel/src/fs/ext4/fs.rs` `DirectWriteProfileStats` |
| **ext4 读阶段** | `plan / alloc / submit / wait / copy_ns` | `DirectReadProfileStats` |
| **块 / virtio 写** | `submit_to_enqueue / queue_wait / dispatch / device_wait / irq_delivery / irq_reap / resp_sync / complete`，外加 large-bio 单独桶 | `kernel/comps/block/src/bio.rs` `[block-profile] write-bio` |
| **锁等待 / 持有** | `total_wait_ns / max_wait_ns / total_hold_ns / max_hold_ns` | `Ext4RsRuntimeLockStats` |
| **JBD2 op** | `start_handle / apply / finish_handle / finish_alloc / finish_io_ns` | `JournaledOpProfileStats` |

特别注意字段 `bio_wait_return_after_complete_ns`：测"bio 已完成但 wait() 还没返回"的纯调度 / 唤醒开销。若它占比高，问题在 waiter 唤醒路径（ext4 射程内、好修），而非 virtio 设备。

**缺口（唯一要补的代码）**：现有日志是"第一次 + 每 N 次"间隔采样（direct write 每 4096 次；runtime lock / JBD2 每 4096 次；direct-read 的 ext4 层 summary 甚至硬编码 `DIRECT_READ_PROFILE_LOG_ENABLED=false`）。fio 跑完需要一份**完整累计 summary**，应在 `sync()` / unmount 时强制 dump 一次，不靠间隔拼。

## Step 0：基线固化 & profile 盘点

**状态：** ✅ 完成
- 诚实基线见 §1；三瓶颈分解见 §2；profile 盘点见 §3；
- 完整 sweep 报告：`fio_direct_parameter_sweep_report.md`；
- 学长反馈与三方对齐：`fio_direct_senior_feedback_response.md`。

## Step 1：收尾 dump + 收割阶段占比表

**状态：** 待执行
**目标：** 加最小代码（final dump），跑两档矩阵，产出"端到端延迟占比"表，验证/推翻三瓶颈假设。

### 代码改动（最小、默认门控、关时零行为变化）

1. 给 `maybe_log_direct_write_profile` / `maybe_log_direct_read_profile` 加 `force` 旁路（不改间隔采样默认行为）；去掉 direct-read 硬编码 `DIRECT_READ_PROFILE_LOG_ENABLED=false`，改由 `phase2_profile_enabled` + `force` 控制；
2. 在 `bio.rs` 补 `pub fn dump_write_bio_profile()` / `dump_read_bio_profile()` 无条件打印，对称现有 `reset_*` API；
3. 新增 `Ext4Fs::dump_perf_summary()`，强制收割四层累计值；
4. 在 `FileSystem::sync()`（`kernel/src/fs/ext4/fs.rs`）末尾、非 shutdown 路径调用 `dump_perf_summary()`。

### 测试矩阵（两档，不是一档）

| bs | target | rw | 目的 |
|----|--------|----|------|
| **1M** | raw / ext4j / ext4n | write 优先，read 次之 | 验证"全压在 `bio_wait` → block `device_wait`"，即单 in-flight virtio 往返 |
| **4K + 16K** | raw / ext4j / ext4n | write + read | 抓 ext4 小块 per-request 开销（`prepare/plan/锁/JBD handle`） |

带 `ext4fs.phase2_profile=1` + 开 write/read-bio profile，收割 `[ext4-direct-write]` / `[ext4-phase2]` / `[block-profile]` 三类行，汇成阶段占比表。

### 验收

- 产出"端到端延迟占比"表，一次性回答：①1M 慢在 virtio 还是 waiter 唤醒；②小块慢在 ext4 哪个阶段；③锁等待占多少；
- final dump 默认关闭，`phase2_profile=0` 行为与 Phase 4 完全一致；
- 守底回归不回退。

## Step 2：定位 → 选优化点

**状态：** 待执行（依赖 Step 1 数据）

分支决策：

- 若 **1M 卡在 block `device_wait` / `bio_wait`** → 转向单 job 请求 overlap / 队列深度 / 异步提交 / waiter 唤醒路径；同时与学长对齐答辩定位（见下方"待拍板项"）。
- 若 **4K 卡在 `prepare/plan/锁/JBD handle`** → 优化 ext4 小块 direct 路径（这是最能讲"我们优化了 ext4"的射程内成果）。
- 若 **读多 job 退化 = 锁等待高** → 定位具体锁（per-inode correctness lock / direct read 调度 / runtime lock），做拆锁 / 缩短持有。

## Step 3：实施优化

**状态：** 待执行（具体手段待 Step 1/2 数据）

候选方向（按当前先验排序）：

1. **小块 ext4 path 瘦身**（射程内、故事性最好）：减少每 I/O 的 mapping / prepare / 锁开销。
2. **单 job 写 overlap**：在 direct write 路径引入有限的请求 pipeline / 提高 virtio 队列深度利用。
3. **waiter 唤醒路径**：若 `bio_wait_return_after_complete_ns` 高，优化 bio 完成→唤醒延迟。
4. **读并发拆锁**。

## Step 3c：小块读 per-request 开销（全路径归因 + atime 优化）

**状态：** 进行中（2026-06-04：全路径 profile 完成，atime 优化待实施）

### 全路径 profile 结论（4K read，每读 ~120µs）
新增 `read_direct_at` 墙钟 + atime 打点（`avg_total_us`/`avg_atime_us`/`avg_other_us`），拆出：

| 部分 | µs | 占比 | 层 | ext4 可修 |
|------|---:|---:|----|:--:|
| VFS / syscall / framekernel（read_direct_at 之上）| 66 | 55% | 平台 | ❌ |
| **atime（每读一次 `stat(ino)`）** | 31 | 26% | ext4 | ✅ |
| virtio 往返（wait）| 21 | 18% | 块层 | ⚠️ |

对照 Linux 整次 4K read = 18µs。

### 可行性判断：小块 read 到 85–90% 不现实
- 光 framekernel 每-syscall 开销 66µs 就是 Linux 整次读的 3.7×；即便 atime/virtio 清零，单这 66µs 也把 4K 卡在 ~27%。
- **结论**：小块 direct read 的根本限制是 **framekernel per-syscall 开销**（平台层，超 ext4/Phase 5 射程），作为"定位到平台瓶颈"的研究结论；ext4 层只能榨 atime 那块。

### atime 优化（本 step 唯一 ext4 射程内的实在点）
- **根因**：cache-off 口径下 atime 节流失效——外层 `touch_atime_after_direct_read` 用空的 `inode_direct_read_cache` 节流；内层 `touch_atime` 的 relatime 检查命中"无需更新"后**不写 `inode_atime_cache` 就 return** → 每次读都 `stat(ino)`（≈31µs）。
- **修法**：relatime"无需更新"决定按秒写入 `inode_atime_cache`，同秒后续读跳过 stat；写操作已 `remove` 该 cache（正确）。
- **预期**：4K read 120→~89µs（16.7%→~22–24%），全 bs 受益（1M 也有 39µs atime）；correctness-neutral（只缓存 relatime 决定）。

## Step 4：全量回归 + benchmark 收口

**状态：** 待执行

### 守底回归（不能回退）

| 测试项 | 最低要求 |
|--------|----------|
| `phase3_base_guard` | 不回退 |
| `phase4_good` | 不回退 |
| `phase6_good` | `25/25 PASS`（注意复查 `generic/011`，见下） |
| `jbd_phase1` | 有效样本 100% |
| JBD2 crash matrix | `18/18 PASS` |
| Phase 2 concurrency | `7/7 PASS` |
| `jbd_phase3_fsync_flush` | 0 FAIL |
| Phase 3 host-crash fsync matrix | `4/4 PASS` |
| `pagecache_phase4` | `FAIL=0`，有效样本 100% |

### benchmark

- A. O_DIRECT 守底（`direct=1, bs=1M, nj=1, cache-off`）：write/read 比对优化前后；
- B. 两档延迟占比表（1M / 4K / 16K）优化前后对照；
- C. 赛题 >= 5% 提升证据：明确给出优化项与量化提升。

## 待拍板项（需人工 / 学长确认，不阻塞收数据）

**1M 大块单 job 的定位**：数据大概率指向 virtio / waiter（ext4 之下），能解释 numjobs=4 达 95%，但未必算"ext4 优化成果"。答辩定位上——是讲成"我们诊断出平台块层瓶颈"（研究结论 / 创新性），还是只把 4K 小块那条讲成"我们优化了 ext4"（射程内成果）——需与学长对齐，免得把 block/virtio 问题误包装成 ext4 成果。

## 前置卡口

sweep G 组的 `phase6_good generic/011`（`rm ... Is a directory`）失败必须先复跑确认是偶发还是目录一致性隐患。**性能优化不能建立在 correctness 回退上**，此项应在 Step 3 动手优化前清掉。

## 注意事项继承

- fio `direct=1`，PageCache 对守底无效，必须继续维护 bio 直通；Phase 5 指标与 PageCache 指标分开；
- cache-off 默认口径：`EXT4_DIRECT_READ_CACHE=0` + `EXT4_PAGE_CACHE=0`；
- `bs=16K fsync=4` 旧高值不能用于性能宣传（fsync-heavy 下 Linux 被持久化成本压低）；
- 所有改动遵守 `asterinas/AGENTS.md` 编码规范（kernel 内禁 `unsafe` / 禁 `println!` 生产路径——profile dump 走 `warn!` / `aster_logger`）。

## 衍生任务：并发功能正确性 xfstests 专套件（Phase 5 诊断驱动，2026-06-05 立项）

来源：Phase 5 并发写诊断 + 学长提议"先补并发功能正确性 xfstests"。结论是我们已有分散的并发覆盖（generic/013/068/076/083/051/054/055/388/247/263 已 PASS），但无专门套件。

任务：
- 新建 `testcases/concurrency.list`：聚拢现有并发 case + 新增 generic/476（多线程全写压写路径）、generic/269（fsstress + ENOSPC 并行查一致性）。
- 新建 `blocked/concurrency_excluded.tsv`：记录刻意排除项（070/117 attrs、232/233/270 quota、475 dm-error）。
- guest `run_xfstests_test.sh` 注册 `concurrency` mode；host `run_phase4_part3.sh` 加 `RUN_CONCURRENCY`。
- **保留自研 `phase2_concurrency.c`**（确定性数据完整性校验，xfstests fsstress 不覆盖此项）——补充而非替换。
- 跑一轮，按结果修错（generic/476/269 是新增风险点，但同源 083/013 已过，概率低）。

验收：concurrency 套件有效样本 PASS，自研 phase2_concurrency 7/7 不回退。

**✅ 已完成（2026-06-05）**：concurrency mode 落地，**10 PASS / 0 FAIL / 100%**；新增 generic/476（多线程全写）、269（并行 ENOSPC）均 PASS → ext4 并发写路径健全。247/263（dio-vs-buffered 一致性，需 page_cache=1）确认归 pagecache_phase4。自研 phase2_concurrency.c 保留未改动。

## 衍生任务：SQLite 真实应用驱动的优化（2026-06-06 立项）

来源：SQLite speedtest1 真实应用 benchmark（见 `docs/sqlite_benchmark_report.md`）。读已追平 Linux（SELECT 1–4×），但 buffered write 灾难性（INSERT/CREATE INDEX 28–244×），且标准负载下 ext4 两种配置两个真实崩溃 bug。

优化主线（按优先级，**每步都不得破坏现有功能：xfstests 全集 / crash / concurrency / 自研 phase2 / fio 守底**）：

1. **Bug A — 分配器 block group 越界 panic**（`ext4_rs/src/ext4_defs/ext4.rs:32`，`bgid == block_group_count == 16` 越界）。正确性 bug，最高优先。SQLite ~2GB DB 涨到第 16 个 block group 边界时触发；xfstests 文件不够大没测出。需定位 bgid 计算 / block_group_count 来源，最小改动修正越界。
2. **Bug B — 内核堆 OOM**（page_cache=1，持续插入 ~600s 后 4608B 分配失败）。buffered write 无界内存增长，PageCache writeback/dirty 回收缺失。Phase 4 hardening 延伸。
3. **buffered write 吞吐**：28–244× 的根本（每事务 fsync + buffered 写元数据路径），对应 fio D2/D3 = 1–6% 遗留项。
4. 修完重测 speedtest1，争取完整跑完给出 TOTAL ratio（可答辩真实应用数据）。

验收：每步后跑守底回归（至少 phase6/jbd_phase1/crash/concurrency/fio 守底）确认不回退；最终 SQLite 能跑完。

### Bug A 深入根因分析 + 修复 plan（2026-06-06）

**症状**：SQLite 无序插入（speedtest1 test 120）时 `lock_block_group(16)` 越界 panic（盘只 16 组 0–15，fs 才用 ~10%）。

**代码研究结论（已读 balloc.rs 地址换算 / 各 alloc 函数 / file.rs initial_write_alloc_bgid）**：
- 地址换算 `get_bgid_of_block` / `bg_idx_to_addr` 对 4K 盘自洽；各 alloc 函数把 `idx_in_bg` 限在 `< blocks_per_group(32768)` 与 `< block_size*8`，单组内算出的块号合法。
- `balloc_alloc_block_from`（balloc.rs:447）**缺** `start_bgid >= block_group_count` 保护（而 `balloc_alloc_block_batch` line 736 有），所以一旦传入 bgid=16 直接在首次 lock 越界 panic。
- bgid=16 的源头 `initial_write_alloc_bgid`（file.rs）两条路径（`get_bgid_of_block(get_pblock_idx(lblock))` 与 `get_bgid_of_block(extent.pblock + len - 1)`）**都要求 extent 树里已存了一个越界物理块（≥ s_blocks_count=524288）**。
- **真正的根因 = 某处把越界块写进了 extent**：要么分配函数在某条件下返回了越界块，要么 extent split/merge/insert 写错 pblock。仅靠读码无法 100% 钉死，需 instrument 抓现场。
- 旁证：加 `start_bgid` 边界兜底后 panic 消失但 SQLite 报 `database disk image is malformed` → 底层确有真实坏块/损坏，兜底只是治标。

**修复路线（instrument-first，低风险）**：
1. **加只读 instrumentation**（不改行为，error 级日志）：①各 block 分配返回点（`balloc_alloc_block_from/batch`、`allocate_new_block`）若返回块 ≥ `s_blocks_count` 则 `log::error!` 带函数名/bgid/idx/inode；②extent insert / split 写入 pblock 时若 `pblock+len > s_blocks_count` 告警；③`get_pblock_idx` 返回越界 pblock 告警；④`lock_block_group` 越界时先 error 再 panic（带 bgid）。
2. **复现**：SQLite `page_cache=0`，boot 后 ~29s 崩在 test 120（快）。抓 instrumented 日志，定位**第一个**产生越界块的函数/行。
3. **精确修**那处计算（最小改动）：分配返回坏块→修地址计算 / 满则返回 ENOSPC 而非越界块；extent 写错→修 extent pblock/len。修完再把 `balloc_alloc_block_from` 的 `start_bgid` 边界保护作为 defense-in-depth 补上（对齐 batch），并确认不再 malformed。
4. **验证**：SQLite `page_cache=0` 必须越过 test 120 且无 panic、无 malformed；随后**完整守底回归全绿**（crash 18/18、xfstests 全 100%、concurrency 7/7）。
5. **记录** milestone + sqlite 报告。

**风险控制**：instrument 只读零风险；fix 针对定位到的具体行；守底回归门控；HEAD（8a0ef283b）为随时回退安全基线。
