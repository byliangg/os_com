# Asterinas ext4 性能优化 Phase 6 — 计划（SQLite 真实应用写优化主线）

首次创建时间：2026-06-09（Asia/Shanghai）

## 阶段定位

Phase 6 承接 feature_perf_phase5（O_DIRECT 读写守底已收口 75–123%，SQLite 真实应用已端到端跑通）。Phase 5 的衍生任务里已经把 SQLite 的两个崩溃 bug（A1 分配器下溢、B checkpoint 内存 OOM）修掉、并落地了**覆盖写快路径**（in-place rewrite 跳过 journaled 分配），把 SQLite speedtest1 从 4773s 拉到 2022s（2.36×）。

但 **SQLite TOTAL 仍只有 Linux 的 2.97%**：剩余慢项全部集中在**追加 / 新分配类写**（INSERT 触发新块、CREATE INDEX、VACUUM 重写），它们走的还是"每页一次完整 journaled 分配 + 每事务 fsync"慢路径。Phase 6 就是把这条慢路径作为**唯一主线**攻下来——这是赛题"使用 SQLite 等真实应用测试文件系统真实性能"维度里我们最弱、提升空间最大的一段。

核心方法论（Phase 5 用血的教训钉死的）：

> **先 profile 再优化。** Phase 5 的"写回批量化"就是没 profile 先动手、只拿到 ~3% 被回退；补 profile（BUFW probe）才定位到真瓶颈（每 write() 的 journaled prepare 占 83%）。Phase 6 每个优化点动手前必须有 profile 占比数支撑。

## 起点数据（Phase 5 收口，2026-06-07）

口径：`page_cache=1`（真实应用默认），`sqlite-speedtest1 --size 1000 /ext4/test.db`，drop_caches 公平基线，Linux 同口径。

| 阶段 | TOTAL | vs Linux 60s | 备注 |
|------|------:|-------------:|------|
| 修 bug 前 | 崩溃 | — | A1 panic / B OOM |
| 修 A1+B 后 | 4773s | 1.26% | 端到端跑通，`integrity_check` 过 |
| **覆盖写快路径后（Phase 6 起点）** | **2022s** | **2.97%** | 2.36× 提速 |

读写分野（speedtest1 逐 sub-test，越低越好）：

| 类型 | 操作 | 相对 Linux | Phase 6 是否目标 |
|------|------|-----------:|:----------------:|
| 读 | SELECT（无/有索引）| 1–4× 慢（25–100%）| ❌ 已追平，非目标 |
| 写·覆盖 | in-place UPDATE 等 | 已走快路径 | ❌ Phase 5 已修 |
| **写·追加/新分配** | INSERT 新块 / CREATE INDEX / VACUUM | **数十~两百× 慢** | ✅ **Phase 6 主线** |

## 目标

1. **SQLite 追加/新分配写类大幅提速**：把 INSERT(新块)/CREATE INDEX/VACUUM 从数十~244× 慢压到可答辩量级，SQLite speedtest1 TOTAL ratio 从 **2.97% 显著提升**（具体目标待 Step 0/1 profile 后定档，初步锚定写类 ≥3× 提速）；
2. 满足赛题"真实应用测试 + 探索的性能优化技术 ≥5% 提升 + 可复用 RustOS FS 优化方法"优秀档与创新性要求，给出可答辩的真实应用前后对比；
3. 全程 correctness-first：Phase 2/3/4/5 守底回归 + crash 恢复 + `integrity_check` 不回退；
4. （次要）清理 A2（page_cache=0 旧 Vec 路径 corruption），让两种配置都健全——低优先，page_cache=1 不受影响。

## 1. 慢路径根因（Phase 5 已研读代码确认，零猜测）

追加/新分配写为什么慢两个数量级，已在 Phase 5 plan 末尾定位，Phase 6 直接继承：

- **每个新分配的 4KB 页跑一遍完整 journaled 写事务**：`write_at_page_cache` 慢路径 → `run_journaled_ext4(JournaledOp::Write)`（JBD2 handle 起停 + `EXT4_RS_RUNTIME_LOCK` + `ext4_map_blocks` 分配 + meta cache clear）+ `ext4_set_inode_times`。覆盖写已被快路径绕过，但**新分配仍每页一次**。
- **每事务 fsync 串行持久化**：SQLite 每个 COMMIT 一次 fsync，叠加上面每页的 journaled 分配 → 放大成数十~244×。无序插入（test 120, 243×）最惨——打散到多 block group，元数据 I/O 最碎。
- **VACUUM 重写几十万页**：几十万次"逐页 journaled 分配 + 逐 4KB virtio 往返"，实测 ~1 MB/s，而 virtio 写带宽地板 ~2800 MB/s → **三个数量级头部空间全在写回/分配代码**。

## 2. 候选优化方向（按预期收益/工程量排序，待 profile 定档）

| # | 方向 | 机制 | 预期收益 | 工程量/风险 | 备注 |
|---|------|------|---------|------------|------|
| **①** | **delalloc 延迟分配** | write() 时只占页缓存不分配磁盘块（标记 delayed），writeback 时对连续脏页**批量分配一大段 extent + 一次 journaled op + 一个大 bio** | **最高**（把"每页一事务一往返"压成"每 run 一次"，直击 VACUUM/批量 INSERT）| 大（需 delayed-block 记账、ENOSPC 预留、writeback 批量分配、与 PageCache/extent 一致性）| Linux ext4 默认机制，最正统、最有故事 |
| ② | 慢路径 journaled prepare 批量化 | 同一 writeback run 内多页合并成一次 `run_journaled_ext4` + 一次 `ext4_map_blocks`（不改"写时分配"语义，只合并相邻页）| 中-高 | 中（不引入 delayed 记账，比 delalloc 轻）| delalloc 的"轻量前身"，可作为 delalloc 落地前的中间收益或回退方案 |
| ③ | group commit / fsync 合并 | 多个并发事务的 fsync 合并成一次 journal commit（Linux JBD2 group-commit）| 中（SQLite 单线程 speedtest1 收益有限，多线程/并发场景大）| 中（改 commit 时序，需 crash matrix 严格门控）| 与 Phase 3 fsync 语义强相关，谨慎 |
| ④ | A2 清理（page_cache=0 corruption）| 旧 Vec buffered 路径写坏数据 | 正确性收尾，非性能 | 中 | 低优先，page_cache=1 不触发 |

**先验主线 = ①delalloc**，但**必须 Step 1 profile 先确认**"慢在逐页 journaled 分配 + 逐 4KB bio"占比足够高，再决定直接上 delalloc 还是先做 ②作为低风险中间收益。

## Step 0：起点固化 & SQLite profile 盘点

**状态：** 待执行
**目标：** 锁定 Phase 6 起点（2022s / 2.97%），跑一次带 profile 的 SQLite，拆出 TOTAL 时间在各 sub-test / 各路径阶段的分布。

- 复用 Phase 5 四层 profile（门控 `ext4fs.phase2_profile=1`）+ Phase 5 末尾的 BUFW probe（`write_at_page_cache` 计数/耗时）。
- 复用已有 `Ext4PageCacheBackend::write_page_async` / writeback 路径打点；如缺"分配 vs 覆盖"分流计数，补**只读** instrument（区分慢路径 journaled-alloc 次数/耗时 vs 快路径覆盖次数）。
- 产出：①各写类 sub-test（100/110/120/150/180/VACUUM）各自耗时；②慢路径每页 journaled-alloc 的 count×avg；③writeback 每页 bio 大小分布（确认是否逐 4KB）。

### Step 0a：三 FS 诊断三角（ext4 / ext2 / ramfs，harness 已存在，先跑）

在 profile 之前先拿一张**三 FS 同口径对比**，用来判定 2.97% 里多少是"我们 ext4 写路径可优化的"、多少是"平台地板改不动的"——这直接决定 delalloc 该不该重仓。

**harness 已存在，不需新建**（`test/initramfs/src/benchmark/sqlite/` 下三个 case，均 `sqlite-speedtest1 --size 1000`）：

| case | DB 路径 | 隔离出什么 |
|------|---------|-----------|
| `ext4_speedtest1` | `/ext4/test.db` | 我们的 ext4（journaled）= 现状 2.97% |
| `ext2_benchmarks` | `/ext2/test.db` | ext2 参考实现（PageCache buffered，**无 journaling**）|
| `ramfs_benchmarks` | `/tmp/test.db` | tmpfs 纯内存（无块设备）|

跑法：`bench_linux_and_aster.sh sqlite/ext2_benchmarks x86_64` / `sqlite/ramfs_benchmarks x86_64`（与 ext4 同；可把 `run_sqlite_summary.sh` 泛化成 `FS_LIST="ext4 ext2 ramfs"` 一条命令三发，待 Step 0 落地时决定）。三个都对 Linux 同口径 drop_caches。

**诊断三角**（差值的含义）：
- **ext4 vs ext2** = 我们 journaling + 每页 journaled 分配的**净代价**（← Phase 6 真正可攻、可优化的那块）；
- **ext2 vs ramfs** = virtio 块往返 + PageCache 写回的**平台地板**（跨 FS 通用，改不动）；
- **ramfs vs Linux** = framekernel 每-syscall 开销（更底，改不动）。

**判定**：若 ext2 远高于 ext4（如 ext2 ~40% vs ext4 3%）→ 大头在我们写路径，delalloc 头部空间大、值得重仓；若 ext2 也只有个位数 → 大头是平台墙，delalloc 收益有限，转向更轻的批量化 / 与学长重定方向。

**诚实警告（防误借鉴）**：ext2 **无 journaling**，它快一部分是省了日志成本，**不是"ext4 应追平 ext2"的目标**——ext4 故意为崩溃一致性付日志代价，那是优秀档功能要求，**不得为提速砍日志**。该借鉴的是 ext2 的**写回/分配模式**（buffered write → PageCache → writeback 批量分配大 extent + 大 bio = delalloc 的形状），ext2 是 Asterinas 里这条路做得最正的范例，作 Phase 6 实现参照代码。

**验收：** 一张"SQLite TOTAL 时间归因表"（含 ext4/ext2/ramfs 三角 + 四层 profile），明确剩余时间里"逐页 journaled 分配 + 逐 4KB bio"占多少、其中多少是 ext4 可优化 vs 平台地板 → 定 delalloc / 批量化的预期上限。

## Step 1：定位 → 选优化点（已定档，2026-06-09）

**状态：** ✅ 已完成。Step 0 归因表（见 milestone）+ 两次失败尝试已把方向钉死。

**Step 0 归因**（profile run，占比对总）：快路径覆盖（每写 `ext4_map_blocks` 全 extent 树遍历 + 全局 `EXT4_RS_RUNTIME_LOCK`）**41%** + 慢路径每页 journaled 分配 **32%** + 每 COMMIT 的 journal-commit/fsync + 逐 4KB bio **24%**；virtio 平台往返仅数十 s（薄地板）。三 FS 三角：ext4 3.02% / ext2 94.91% / ramfs 95.87% → **损失几乎全在 ext4 写路径，平台不背锅**。

**两次失败尝试（记录教训，不可重蹈）：**
1. **2a：缓存覆盖写的映射检查**（前缀/窗口 cache，3 个变体）→ **死路**。SQLite B 树散点 + truncate 扩容 → 任何前缀 cache 命中率 0.005%；whole-file extent-list 在碎片文件上 >MAX_CACHED_EXTENTS → 每 miss 重走整文件 → 灾难性回退（被 tripwire 在 commit 前抓住，已回退）。**结论：那 41% 必须"去掉检查"而非"缓存检查"。**
2. **2b-1：直接去掉 size 以内的映射检查**（让写回分配）→ **540s OOM 崩溃**。去掉检查后，size 以内填空洞的写不再在 write() 时分配，磁盘满（free_blocks=1637）时写回分配失败 → 脏页无法 drain → 内核堆耗尽。**结论：块预留（reservation）是必须的，不是可选——这正是完整 delalloc 的核心。**

**最终定档 = 完整 Linux delalloc**（写时预留 + 写回批量分配 + 脏页节流），保稀疏语义、收益最大、最正统。详见 Step 2 分阶段计划。

**已读码确认的架构事实（Step 2 实现依据）：**
- **fsync 链已 delalloc-friendly**：`Ext4Inode::sync_all` → `sync_page_cache_for_inode`（`evict_all` → 逐页 `write_page_async` → `write_page_cache_data_at` → `run_journaled_ext4(ext4_write_at)`，`ext4_write_at` 经 `ensure_write_range_mapped` **按需分配**）→ `fsync_regular_file`（journal force-commit）→ `block_device().sync()`（设备 flush）。即「fsync 强制延迟页落盘 + 日志 + flush」已天然满足。
- **写回是逐页的**：`page_cache.rs::evict_range` 对 `[start,end)` 逐 idx 调 `write_page_async` → 逐 4KB bio（24% 桶）；要大 bio 需新增按 run 的批量写回路径。
- **分配无批量**：`ensure_write_range_mapped` 的 `WRITE_PREALLOC_BLOCKS=1`，逐块分配（32% 桶）。
- **可预留**：`super_block.free_blocks_count()` 可查；`decrease/increase_free_blocks_count` 已有 → 预留可用「一个内存 `delayed_reserved_blocks` 原子计数叠在 free_blocks 之上」实现。
- **OOM 已有界基建**：Phase 5 的 `JOURNAL_CHECKPOINT_MAX_DEPTH` bound 了 journal/checkpoint 内存；2b-1 的 OOM 是**脏页**侧（非 journal 侧），需脏页节流补位。

**与学长对齐项（动手阶段 1/2 前过一遍）**：① 写时块预留的 ENOSPC 语义（过预留导致的提前 ENOSPC 是否可接受）；② 写回批量分配改 journal 事务边界、与 crash 恢复的一致性；③ 脏页节流阈值。

## Step 2：实施优化 —— 完整 delalloc，分阶段（主线）

**状态：** 🔄 进行中（按下列 Stage 0→3 顺序执行，每 Stage 独立守底 + 可回退 + 存档 commit）。

**总原则**：每个 Stage 都是一个能编译、能跑、能守底、能回退的中间态；HEAD（Step 0 存档 `8394f31a6`，含 Phase 5 全部修复）为安全基线；任一守底 / `integrity_check` / O_DIRECT 守底回退 = 该 Stage 改错，立即回退。**铁律：不为提速砍日志。**

### Stage 0：OOM 根因确认（只读诊断，先做）

- **目标**：用只读 instrument（脏页数 / 待写回页数 / 预留缺口 / 当前 free_blocks）+ 短复现，确认 2b-1 的 OOM 是「磁盘满 → 脏页堆积」而非「journal/checkpoint 内存增长」。
- **为什么先做**：决定 delalloc 是否被一个 journal-内存暗坑直接打死；便宜（崩在 ~540s，十几分钟）。
- **涉及文件**：`kernel/src/fs/ext4/fs.rs`（只读计数器，门控 `phase2_profile`）。
- **验收**：明确 OOM 归因；若确属脏页侧 → Stage 1 的预留 + 节流可根治，delalloc 放行。

### Stage 1：写时预留 + 延迟写（攻 41%，保 ENOSPC，防 OOM）

- **目标**：`write_at_page_cache` 改为「纯页缓存写 + 块预留 + （追加时）journaled size 更新」，**去掉每写的 `ext4_map_blocks` 检查和 `ext4_prepare_write_at` 分配**。分配全部推迟到写回。
- **机制**：
  - 新增内存计数 `delayed_reserved_blocks`（原子）。write() 对其将脏的页**悲观预留**（每脏一页预留 1 块；已脏页不重复预留——用 PageCache 脏位判断 clean→dirty 跃迁）；若 `free_blocks_count - delayed_reserved_blocks < 需求` → 立即返回 `ENOSPC`（语义正确）。
  - 写回 `write_page_cache_data_at` 分配成功后 **释放对应预留**（真分配的从 free_blocks 扣、预留归还）；覆盖写（本就已分配）也归还预留（over-reserve true-up，照 Linux）。
  - **脏页节流**：当 `delayed_reserved_blocks` / 脏页数超阈值，write() 先触发该 inode 的 `evict`（写回 drain）再接受新写 → bound 内存，根治 2b-1 的 OOM。
  - 追加（write_end > cur_size）：仍需 journaled **size 更新**（不分配块，轻量），保证 stat/读/写回看到正确 size。
- **涉及文件**：`kernel/src/fs/ext4/fs.rs`（`write_at_page_cache`、预留计数、节流、`write_page_cache_data_at` 释放）；可能 `kernel/src/fs/utils/page_cache.rs`（脏位/写回钩子）；`kernel/libs/ext4_rs`（free_blocks 查询接口，若缺）。
- **ENOSPC / 崩溃**：预留保证 write() 正确报满；未 fsync 的延迟数据崩溃丢失 = POSIX 合法；fsync 链已强制落盘+日志+flush。
- **验收门控**：build → SQLite（41% 是否消失？是否不再 OOM 跑完？`integrity_check` PASS）→ 完整守底矩阵（重点 ENOSPC / 稀疏 / crash matrix）→ 绿则 commit 存档。
- **预期**：SQLite TOTAL 显著下降（吃掉 41%）；32%/24% 仍在（Stage 2 攻）。

### Stage 2：写回批量分配 + 大 bio（攻 32% + 24%）

- **目标**：写回时把一个 inode 的**连续延迟脏页**合并 → **一次 `run_journaled_ext4` 分配一大段 extent**（`ensure_write_range_mapped` 按 run 长度分配 / 提高 prealloc）→ **一个大 bio**。
- **机制**：新增按 run 的批量写回路径（`evict_range` 传区间给 backend，或 backend 侧合并连续脏页）；批量分配走单个 journaled 事务（与 alloc_guard / JBD2 handle 协议一致）；大 bio 替代逐 4KB。
- **涉及文件**：`kernel/src/fs/ext4/fs.rs`（批量写回）；`kernel/src/fs/utils/page_cache.rs`（区间写回 API）；`kernel/libs/ext4_rs`（按 run 分配 / prealloc）；`kernel/comps/block`（大 bio 提交，复用 Phase 5 路径）。
- **崩溃**：写回分配改 journal 事务边界 → **必过 crash matrix + `integrity_check`**。
- **验收门控**：build → SQLite（32%/24% 是否下降？）→ 完整守底 + crash matrix → 绿则 commit。

### Stage 3：调参 + 硬化 + 收口

- **目标**：调脏页节流阈值 / prealloc 大小；复跑完整守底 + crash matrix + O_DIRECT 守底（不回退）；SQLite 终测出诚实 TOTAL（error 口径）+ 逐写类前后表。
- **验收**：守底全绿 + `integrity_check` PASS + O_DIRECT 75–123% 不回退 + SQLite ≥5% 提升量化 → milestone 收口。

## Step 3：全量回归 + SQLite 重测收口

**状态：** 待执行

### 守底回归（不能回退）

| 测试项 | 最低要求 |
|--------|----------|
| `phase3_base_guard` | 不回退 |
| `phase4_good` | 不回退 |
| `phase6_good` | `25/25 PASS` |
| `jbd_phase1` | 有效样本 100% |
| JBD2 crash matrix | `18/18 PASS` |
| Phase 2 concurrency（自研 `phase2_concurrency.c`）| `7/7 PASS` |
| `concurrency` xfstests 套件 | `10/10 PASS` |
| `jbd_phase3_fsync_flush` | 0 FAIL |
| Phase 3 host-crash fsync matrix | `4/4 PASS` |
| `pagecache_phase4` | `FAIL=0` |
| fio O_DIRECT 守底（`direct=1, nj=1, cache-off`）| 读写不回退（Phase 5 的 75–123%）|

### SQLite 重测
- `speedtest1 --size 1000` page_cache=1，drop_caches，与 Linux 同口径；
- 给出 TOTAL ratio 优化前（2.97%）→ 后对比 + 逐写类 sub-test 前后表；
- `PRAGMA integrity_check` 必须仍 PASS（数据无损铁证）；
- 赛题 ≥5% 提升证据：明确写类优化项 + 量化。

## 前置卡口 / 注意事项继承

- **profile 先行**（Phase 5 教训）：任何优化前先有占比数；
- fio `direct=1` 绕过 PageCache，delalloc/buffered 改动不得回退 O_DIRECT 守底；PageCache 指标与 O_DIRECT 指标分开统计；
- delalloc/group-commit 触及持久化语义，必须过 crash matrix + `integrity_check`，不得用 `bs=16K fsync=4` 类不诚实口径宣传；
- 改动遵守 `asterinas/AGENTS.md` 编码规范（kernel 内禁 `unsafe` / 禁 `println!` 生产路径，profile dump 走 `log` 宏）；
- 根目录 `feature_sqlite_phase6_*.md` 与仓库内 `asterinas/docs/feature_sqlite_phase6_*.md` 需同步维护。

## 与 Phase 5 的边界

- Phase 5 = O_DIRECT fio 读写守底 + SQLite 崩溃 bug + 覆盖写快路径（**已收口**）。
- Phase 6 = SQLite 追加/新分配写吞吐（delalloc 主线）+ A2 清理（**本阶段**）。
- O_DIRECT 守底数（75–123%）是 Phase 5 成果，Phase 6 只保证不回退，不再优化。
