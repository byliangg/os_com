# Asterinas ext4 性能优化 Phase 6 — 计划（SQLite 真实应用写优化主线）

首次创建时间：2026-06-09（Asia/Shanghai）
重大方向调整：2026-06-10（delalloc 实测被平台墙堵死 → 改 fsync 安全点 + 缓存层路线）
第二轮路线（2026-06-11）：S 系列收口后启动 **P 系列（90% 目标路线）**，见 §P 系列——已执行完毕并收敛，最终 **234.9s = 21.92%**，结论与 delalloc 解锁链见该节。

## 阶段定位

Phase 6 承接 feature_perf_phase5（O_DIRECT 读写守底已收口 75–123%，SQLite 真实应用已端到端跑通）。Phase 5 已修掉 SQLite 两个崩溃 bug（A1 / B）并落地覆盖写快路径，把 speedtest1 从 4773s 拉到 2022s（2.36×）。

但 **SQLite TOTAL 仍只有 Linux 的 2.97%**，慢项集中在写类。Phase 6 攻这条写路径。

**方法论铁律（Phase 5 + Phase 6 Stage 1a 双重教训）：**
> **先 profile 再优化；动写回路径前先钉死正确性不变量。** Phase 5 的"写回批量化"没 profile 先动手只拿 3% 被回退；Phase 6 的 delalloc 没先钉"途中写回是否安全"就实现，撞墙损坏数据。两次都是"绕过验证直接动手"的代价。

## 起点数据 & Step 0 归因（已实测，2026-06-09，详见 milestone）

口径：`page_cache=1`，`sqlite-speedtest1 --size 1000`，drop_caches，Linux 同口径。起点 **TOTAL 2010s = 2.97%**，`integrity_check` PASS。

**三 FS 诊断三角（决定性）**：ext4 **3.02%** / ext2 **94.91%** / ramfs **95.87%**。
- ext2 vs ramfs（95% vs 96%）= 平台地板**仅 ~12%，很薄**；ramfs vs Linux ~4%。
- → **~96% 的损失在我们 ext4 写路径，不是平台墙**。ext2 在**同一 Asterinas 平台**（同 virtio、同 PageCache、**无 journaling**）达 95%，证明 buffered 写回在本平台可达 Linux 水平。
- **诚实边界**：ext2 无日志，是"非日志写回天花板"+ 实现范例，**非 ext4 必达目标**——不得为提速砍日志（优秀档功能要求）。

**四层 profile 归因表（2057s profile run 拆分，占比对总）：**

| 桶 | 证据 | 时间 | 占比 | 攻击手段（新路线）|
|----|------|----:|----:|------------|
| **快路径覆盖**（无分配，每写仍 `ext4_map_blocks` 全 extent 树遍历 + 取全局锁 + stat）| 8.52M calls × 98us | **843s** | **41%** | **S5 元数据块/extent 缓存**（消除每写重读树块）+ **S3 删 fsync decommit**（消除 decommit→逐4KB回填）|
| **慢路径 journaled 分配**（追加/新块 prepare，写时分配）| 556K calls，710K blocks，avg 1193us | **664s** | **32%** | **S6 物理预分配**（一次分配 32–64 块，写时不延迟）|
| 慢路径其余 | — | 63s | 3% | 随 S6 |
| **COMMIT / journal-commit + fsync + 逐 4KB writeback bio** | 5931 commits；812K×4KB bio | **~487s** | **~24%** | **S4 fsync 安全点批量写回**（大 bio）+ 可选 group commit |
| 读（SELECT）| — | ~26s | 1% | 已追平，非目标 |

**关键修正（推翻原 plan 先验）**：最大单桶是**快路径覆盖 41%**（不是新分配慢路径）。Phase 5 覆盖快路径虽跳过分配，但**仍每写一次 `ext4_map_blocks` 全树遍历 + 取全局锁**（8.52M×98us=843s）——这正是"连 in-place UPDATE 都比 ext2 慢 15–35×"的根因。**注意**：此 843s 的成本是**锁内的串行工作（重读 extent 树块）**，不是锁争用（单线程 avg_wait=0）；外部 review 已核实 `write_range_fully_mapped` 走 `run_ext4_file_read_only`（持 `inner: Mutex<Ext4>`、**不持** `EXT4_RS_RUNTIME_LOCK`），大头是持锁期间从 virtio **重读 extent 树块**（`inode_extent_map_cache` 在 page_cache=1 时被 fs.rs:4643 `&& !page_cache_enabled` 整体旁路）→ 修法是**缓存树块**而非拆锁。

## 为什么不做 delalloc：实测被平台墙堵死（2026-06-10）

不是"选择降级"，是**实跑撞墙**。Step 2 曾定档完整 delalloc，推进到 Stage 1a 后：

1. **OOM（Stage 0 已诊断）**：去掉每写检查后写变快 → 生产 > 消费（仅 fsync drain）→ 脏页涨到 9.6GB > 8GB RAM。**delalloc 必须脏页节流**（途中强制写回控内存）。
2. **节流引入数据损坏（Stage 1a）**：加 256MB 脏页节流后内存稳住，但 ~64s SQLite 报 `database disk image is malformed`。三个隔离实验定位：
   - 基线写路径 + 每 4096 写强制 `evict_all` → **也损坏**（77s）→ **与 delalloc 无关**，是「途中写回」本身。
   - 改成"只写回不 decommit" → **仍损坏**（182s）→ **不只是 decommit**，是「在非 fsync 点驱动写回路径」本身不安全。
   - fsync 点写回**安全**（静默边界）。
3. **定论**：完整 delalloc 的脏页节流**必须**途中写回，而 **Asterinas ext4 的途中写回会损坏数据**。Linux delalloc 靠**后台 flusher 线程**在安全点写回，Asterinas **无此基础设施**（补它 = OSTD 跨子系统周级工程，赛期 ROI 极差）。→ **delalloc 在 Phase 6 范围内被此平台限制阻塞。**

**结论**：把所有优化收敛到**只在 fsync 安全点 + 写时预分配**——这两类操作 Stage 1a 已实证安全。delalloc 降为 **S7（仅当未来修好"途中写回安全"或加了后台 flusher 才解锁）**。

## 新技术路线（fsync 安全点 + 纯缓存层 + 写时预分配）

**核心**：所有改动落在两类已证安全的位置——① fsync 安全点（静默边界，写回安全）；② write() 时（不延迟分配）。**不引入任何"非 fsync 点的途中写回"**（那是 Stage 1a 撞死的墙）。每步不动持久化语义（数据仍在 commit record 前落盘）、走 profile 门控、过完整守底 + `integrity_check`。

### 第 0 周：去风险（天级，先做）

| 步 | 内容 | 状态/可行性 |
|----|------|------------|
| **S0** | 日志开销核查 | ✅ **已查清=无需做**：honest benchmark 用 `LOG_LEVEL=error`，`warn!` 在 error 级**不格式化不输出**（profile 的 warn 口径多 2.3% 仅 profile 时）→ review 的"砍 warn 白捡"对诚实跑分**零收益**，跳过 |
| **S1** | 给写回路径加正确性不变量断言/日志（门控、只读）：`write_page_async` 的 clamp/drop 触发即记（页 idx/file_size/offset）；checkpoint 写 home 块前断言目标块不在"最近 N 个被分配为数据块"集合。**钉死 Stage 1a 未钉死的 exact line，并为后续写回改动兜底** | 1–2 天，纯观测，低风险 |

### 主线（S3 优先级最高，每步之间：重跑 profile + 完整守底门控）

| 步 | 内容 | 攻击桶 | 前置/风险 | 可行性 |
|----|------|--------|----------|--------|
| **S3** | **fsync 保留 clean 页（删 `evict_all` 里的 `decommit_vmo_range`，只写回标 clean）**。decommit 仅留给 truncate/unlink/O_DIRECT/close 一致性点 | 41% 的回填部分 + 打断"每 COMMIT 清零→逐 4KB 同步回填"恶性循环（in-place UPDATE 慢 15–35× 直接原因）| **必须先查 decommit 在 fsync 的来历（git log）+ 必过 buffered/direct coherency + mmap 守底**；它是 fsync 安全点改动、**不是途中写回**，与 Stage 1a 的墙无关 | 高（单步收益预期最大）|
| **S4** | **fsync 安全点批量写回**：fsync 时收集脏页区间→合并→每连续区间一次轻量 handle（已分配块的纯数据写回**不需逐页 JBD2 handle**）→物理连续段合成大 bio（复用 O_DIRECT `submit_direct_write_mappings` 管道）→照旧 force-commit + flush | 24%（逐 4KB bio → 大 bio；逐页 handle → 每区间一次）| 仍在 fsync 安全点（安全）；mmap 脏页要确认仍被收集；**S1 断言先在位** | 中高 |
| **S5** | **映射检查去重读（攻 41% 的树块重读）**：S5a 把现成 `inode_extent_map_cache` 接进 buffered 路径（现被 fs.rs:4643 旁路），`write_range_fully_mapped` 先查缓存；S5b（若 S5a 命中率不足）有界元数据块 buffer cache（按 pblock 缓存树块/inode 块，LRU 几 MB，失效走 `run_journaled_ext4` 单点）| 41% 的树块重读部分 | ⚠️ **2a 已证"缓存查询结果"死路（命中 0.005%）**；S5a/S5b 是**不同粒度**（缓存 extent 记录/树块本身，非布尔结果）——**必须先测命中率，2a 是反例不可重蹈**；命中不足则 41% 可能需 S7 | 中（不确定，需实测）|
| **S6** | **物理预分配 + 追加快路径（攻 32%）**：顺序追加时一次分配 32–64 块（管道现成，file.rs:868-975），prealloc 块标 **unwritten**，写到时转 written；给"已映射但越 EOF"的追加开轻 handle 快路径 | 32%（写时每页分配 → 每段一次）| ⚠️ **已调查（2026-06-10）：ext4_rs 明确不支持 unwritten extent**（file.rs:11 注释 `does not model unwritten extents yet`；现 `WRITE_PREALLOC_BLOCKS=1` 正是因此）。extent **格式**支持（`is_unwritten`/`mark_unwritten`/`EXT_INIT_MAX_LEN` 编码在 extents.rs），但**读/写/map 路径不处理**（读不返回零、写不转换）。→ **S6 必须先实现 unwritten 支持**（读返回零 + 写 unwritten→written 转换 + map_blocks 处理），约 2–4 天，**且是 Linux 对齐真功能（答辩加分）** | 中（需先实现 unwritten，非快胜）|
| **S7** | **delalloc —— 仅当"途中写回安全"被解锁（修平台 bug 或加后台 flusher）且 S3–S6 后 profile 仍剩显著肉才做** | 残余 | 当前**被平台墙阻塞**，不在 Phase 6 范围 | 阻塞 |

### 并行正确性任务（不涨 SQLite 分，但保答辩，独立推进）

**revoke 修复（F1，外部 review 发现、已代码核实）**：`RevokeBlockHeader::new` 全仓零调用方 → **revoke 记录从不写入 journal**，但 recovery 完整实现了 revoke 扫描（`scan_revoke_blocks`/`parse_revoke_entries`）。后果：元数据块释放后重分配为数据块、tail 越过前崩溃 → replay 用陈旧元数据镜像静默覆盖用户数据。这是崩溃一致性叙事**唯一的真窟窿**（优秀档功能正确性被问到会翻车），且 S6 的块复用节奏会加大暴露面。
- **修法**：释放元数据块（truncate/rmdir/extent 树收缩）时记 revoke 集合 → commit 写 `JBD2_REVOKE_BLOCK` → **必须同时给 replay 加序列号过滤**（recovery.rs:48 现对所有事务生效，无序列号上界；朴素写 revoke 会把后续重新 journal 的合法元数据也跳掉——两者必须一起修，否则修一个洞开一个洞）。
- **验证**：加"块复用崩溃"用例进 crash matrix（建大目录→删→建大文件占同批块→fsync→replay 前杀 VM→remount 比对 CRC）。约 3–5 天。

## P 系列（90% 目标路线，2026-06-11 执行完毕）——合并自 `feature_sqlite_phase6_90pct_roadmap.md`

S 系列收口（S6，1332.2s=3.86%）后，以 90% 为目标做了第二轮"逐层代码排查 × Linux/ext2 对照 × profile 先行"的路线。**最终落点 234.9s = 21.92%（起点 2.97% 的 7.4×），守底全绿。**

### P 系列执行结果总表

| 步 | 内容 | 结果 | SQLite |
|----|------|------|-------:|
| P0 | S6 后 profile：**写 33GB 读 166GB（39.2M 次 4KB 读 ≈705s 设备等待）**——ext4_rs 全程零元数据缓存是单一最大瓶颈 | 归因 | 1332.2s |
| P1 | **adapter 层 DeviceBlockCache**（write-through 设备镜像，JBD2 overlay 之下，O_DIRECT bio 显式失效；ext4_rs 仅 get_inode_ref 改对齐读）。命中率 98.5% | ✅ −66% | **454.3s（11.26%）** |
| P2 | **写快路径 ext2 化**：WrittenCoverage 区间集（coverage⊆truth）+ VmReader 直写去双拷贝 + meta cache per-ino 失效。捎带修 S6 潜伏 388 panic（尾段部分插入失败双重释放） | ✅ −46% | **243.9s（20.88%）** |
| P3-1 | size-only append（写时不转换，推迟到 writeback） | ❌ 三配置实测净负（462.9/312.8/468.0），含未解的读侧 30% 回退，回退；patch 留存 `benchmark/logs/p3_1_size_only_append_attempt_20260611.patch` | — |
| P3b | journal commit 块合并大写（descriptor+payload 一次 bio） | ✅ 中性保留（证实 commit 桶大头不在 journal 块写） | 243.97s |
| P4 | 真 fdatasync | ❌ 动手前核实出局：inode_tids 已门控 modified_blocks>0、commit 由批量轮转驱动（rotations 4742/5131）、[ext4-fsync] 实测 fsync 桶仅 28.6s | — |
| P5a | **lean append prepare**（全已分配写单遍探测，3 遍树查→1 遍）+ insert_extent 损坏节点防御 + crash runner 端口碰撞修复 | ✅ −4% | **234.9s（21.92%）** |

### 最终结论：上限的理论依据（答辩可直接引用）

1. **不是根本极限**：Linux ext4 带完整 JBD2 = 51s（100%）。日志不贵，贵的是付税方式——Linux delalloc 把每页元数据成本摊到每 extent（≈0），我们"写时同步转换"每 4KB 全额支付。
2. **当前架构类别的数学下限**：workload 含 549K 次新分配写（SQLite 行为，不可减）。写时同步转换下每次 append 的不可压缩成本（零填设备写 ~30us + handle/overlay ~60-100us + 转换手术 ~30-50us）即便理想化到 100us：549K×100us ≈ 55s，**仅此一项就逼近 90% 目标总预算（57s）**。类别内理论最优 ≈ 110-120s（43-46%，无已知路径）；**现实可达 ≈ 190-210s（24-27%）**（P5b 脏页索引 ~12-15s + 零碎）。
3. **曲线实测趋平**：P1 −66% → P2 −46% → P5a −4%。

### delalloc 解锁链（通往 50-90% 的唯一通道，赛期外）

依赖链：**① 钉死 Stage 1a"途中写回损坏"exact-line 根因**（milestone 明示未钉死——可能是可修 bug 而非铁墙，是第一张多米诺）→ **② 安全写回驱动**（修好①或建后台 flusher，OSTD 级周级工程）→ **③ 解开 P3-1 读侧 30% 回退之谜**（先加细分 instrumentation：写回转换计时/populate/find_extent 计数）→ **④ delalloc 本体 + 全量崩溃语义门控**。估 2-4 周，①③带不确定性。**赛期内决策：24-27% 收口 + 三角归因叙事答辩。**

### P 系列方法论收获（负结果也是成果）

三次"先核实/先 profile"各省一轮实现：delalloc（Stage 1a 隔离实验）、P3-1（三配置对照后按铁律回退）、P4（计数器核实 0 实现成本出局）。两个压力潜伏 bug（388 双重释放修复、476 防御降级）+ harness 端口碰撞根治为守底基建净赚项。

## C 系列（并发 scale 主攻，2026-06-12 立项）

### C0 调研结论（代码 + sweep 数据闭环）

1. **fio numjobs 写同一个文件**（`-filename=/ext4/fio-test` 固定）；fio 测量前先 layout（fallocate→written extents）→ 测量阶段全是**纯覆盖 O_DIRECT 写**。
2. **per-inode correctness 锁（Mutex，独占）横跨整个 O_DIRECT 读/写路径，包括 ~190us/MB 的设备等待**（`write_direct_at` fs.rs:5152、`read_direct_at` fs.rs:5073-5074）→ 同文件并发被钉在单流水平 ~2800 MB/s，nj 无效。
3. 三证闭环：raw 线性 scale 至 5300+（117%，设备能力充足）；ext4 卡死 ~2800；**journaled ≈ nojournal 同水位 → 锁而非 JBD2**。
4. 对照 Linux：ext4 对 dio **overwrite** 用 shared `i_rwsem`（不改映射的写并发执行，元数据写才独占）。我们已有判定基础设施：`plan_direct_write_overwrite_cached`（pc=0 时活跃，要求映射全覆盖，extent map cache 命中则纯内存判定）。

### C1 实现：per-inode 锁 RwMutex 化 + dio overwrite/read 共享锁

- `inode_correctness_locks` / `dir_correctness_locks`：`Arc<Mutex<()>>` → `Arc<RwMutex<()>>`；所有现有持锁点改 `.write()`（语义不变）。
- `write_direct_at`（仅 `page_cache=0`）：先持 **shared** 做 overwrite 判定（判定在锁内 → 映射不可变，TOCTOU 安全：改映射的路径全部需独占）；命中 → shared 下提交数据 bio（同文件并发写并行，重叠 offset 的结果归应用负责 = POSIX dio 语义，fs 结构无损）；未命中 → 释放 shared 改持独占走 prepare（重做判定，无状态依赖）。
- `read_direct_at`（仅 `page_cache=0`，evict 为 no-op）：shared。`page_cache=1` 两路径维持独占（动页缓存，保守）。
- 共享锁下触碰的全部状态各有内部锁：extent map cache / direct read cache / coverage / jbd2_runtime（revoke）/ profile Atomic ✓。

### C2 验证门控

并发正确性双层（phase2_concurrency 7/7 + concurrency xfstests 10/10）是本改动的**专属把关**，加 crash matrix / fsync_flush / host-crash / 全 xfstests / fio 守底不回退；F 组 nj1/2/4 重测为收益判定（目标：ext4 write nj2/4 显著脱离 2800 向 raw 5300 靠近）。

### C3 残余串行点（按 C2 后 profile）

候选：`run_ext4_file_read_only` 的全局 `inner: Mutex<Ext4>`（shared 路径的 plan miss 时进入）、adapter 块缓存全局 Mutex、block 层提交路径。仅在 C1 后 F 组仍不 scale 时按归因继续。

## Step 3：全量回归 + SQLite 重测收口

**状态：** 待执行（每个 S3/S4/S5/S6 落地后都跑一次本节门控）

### 守底回归（不能回退）

| 测试项 | 最低要求 |
|--------|----------|
| `phase3_base_guard` / `phase4_good` | 不回退 |
| `phase6_good` | `25/25 PASS` |
| `jbd_phase1` | 有效样本 100% |
| JBD2 crash matrix | `18/18 PASS` |
| Phase 2 concurrency（自研 `phase2_concurrency.c`）| `7/7 PASS` |
| `concurrency` xfstests 套件 | `10/10 PASS` |
| `jbd_phase3_fsync_flush` | 0 FAIL |
| Phase 3 host-crash fsync matrix | `4/4 PASS` |
| `pagecache_phase4`（**S3/S4 重点：buffered/direct coherency + mmap**）| `FAIL=0` |
| fio O_DIRECT 守底（`direct=1, nj=1, cache-off`）| 读写不回退（75–123%）|

### SQLite 重测
- `speedtest1 --size 1000` page_cache=1，drop_caches，Linux 同口径，**`LOG_LEVEL=error` 诚实口径**；
- TOTAL ratio 2.97% → 后对比 + 逐写类 sub-test 前后表；`integrity_check` 必 PASS；
- 赛题 ≥5% 提升量化。

## 收益预期（诚实，非承诺）
- **2022s → 200–400s 量级（Linux 的 15–30%）**。到不了 ext2 的 95%，差距 = journal commit + 每 COMMIT fsync 屏障的固定成本 = **强一致性的诚实代价** → 答辩好故事：三 FS 对照 + 逐桶归因消除 + 剩余差距明确归属。
- 最大不确定点：① S5 命中率（2a 反例）；② S6 unwritten extent（有退路）；③ 各桶占比在 S3 后会重洗——**铁律：每步重 profile，不预支后面步骤收益**。

## 注意事项继承
- profile 先行；fio `direct=1` 绕 PageCache，改动不得回退 O_DIRECT 守底；
- 不动持久化语义，过 crash matrix + `integrity_check`，不得用 `bs=16K fsync=4` 不诚实口径；
- 不补 PageCache 平台基建（页回收 shrinker = OSTD 周级，赛期 ROI 差）→ 设计成在不完整 PageCache 里活着；**唯一承重墙 = S3 保留 clean 页后"工作集装进 RAM"**（ext2 已实证 8GB 装得下）→ 建议加 **cache 总量统计接口**兜底 OOM 观测；
- kernel 内禁 `unsafe` / 禁生产路径 `println!`，profile dump 走 `log` 宏；
- 根目录 `feature_sqlite_phase6_*.md` 与 `asterinas/docs/` 副本同步。

## 与 Phase 5 的边界
- Phase 5 = O_DIRECT fio 守底 + SQLite 崩溃 bug + 覆盖写快路径（已收口）。
- Phase 6 = SQLite 写吞吐（**fsync 安全点 + 缓存 + 预分配**路线；delalloc 被平台墙阻塞降 S7）+ revoke 正确性并行任务。
- O_DIRECT 守底数（75–123%）是 Phase 5 成果，Phase 6 只保证不回退。
