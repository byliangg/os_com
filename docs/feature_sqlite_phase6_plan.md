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

## Step 1：定位 → 选优化点

**状态：** 待执行（依赖 Step 0 数据）

分支决策：
- 若**慢在逐页 journaled 分配**（每页一 handle/锁/映射占大头）→ 主攻 ①delalloc 或 ②批量化（按风险选）。
- 若**慢在逐 4KB bio 往返**（writeback 没合并大 bio）→ delalloc 的"批量分配大 extent + 大 bio"同时解决。
- 若**慢在每事务 fsync 同步**（commit 往返占大头）→ 加 ③group commit 评估（需与学长对齐 crash 语义）。

**与学长对齐项**：delalloc 改写时机/ENOSPC 语义、group commit 改 fsync 时序——都触及崩溃恢复正确性，动手前过一遍。

## Step 2：实施优化（主线 delalloc / 备选批量化）

**状态：** 待执行（手段待 Step 0/1 定档）

实施原则（继承 Phase 5）：
- 最小改动、默认行为安全；delayed 块的 ENOSPC 必须预留（不能 writeback 时才发现没空间丢数据）；
- 参考 ext2 / Linux ext4 delalloc 语义；writeback 批量分配要与 extent 树、PageCache 脏页、JBD2 一致；
- 每个中间态可守底、可回退；HEAD（含 Phase 5 全部修复）为安全基线。

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
