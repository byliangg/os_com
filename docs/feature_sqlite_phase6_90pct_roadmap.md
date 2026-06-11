# SQLite → Linux 90% 路线方案与可行性评估（Phase 6 后半程）

创建时间：2026-06-11（Asia/Shanghai）
现状：S6 收口后 SQLite speedtest1 TOTAL **1332.2s = Linux 的 3.86%**（Linux 51.4s）。目标 90% ≈ **57s，需 ~23×**。
方法：逐层代码排查（我方 ext4 全写路径 + fsync/jbd2 commit 路径）× 对照（Linux ext4 机制 / 同平台 ext2 实现）× S6 后新 profile（§2）。

## 1. 结论先行

**90% 不能承诺；50–80% 是有把握的目标带，90% 是边缘可能。** 依据：

1. **平台不挡路**：同平台 ext2（无日志）= 94.91%，ramfs = 95.87% → virtio/PageCache/syscall 地板都很薄。Linux ext4 带完整 JBD2 也能跑到 60s → "带日志达到高分" 有存在性证明。
2. **差距是机制级、可枚举的**（§3 五大项），每一项都有 Linux/ext2 的对照修法，没有发现新的"平台墙"（Stage 1a 的"途中写回"墙不在本路线依赖里——所有写回仍只发生在 fsync 安全点）。
3. **不可消除的日志税决定上限**：ext2（完全无日志）只有 95%；我们保住优秀档功能（JBD2 + 崩溃一致性）后，每事务多付 1 次 flush + journal 块写 + handle 簿记。按 ~6k commit 估 5–15s 固定税 → 理论天花板 ~80–88%。**逼近 90% 需要非日志路径做到与 ext2 完全同速 + 日志税压到 ~3s**，两者都到边缘才行。

诚实口径下建议把阶段目标定为：**M1: ≥15%（~340s）→ M2: ≥35%（~150s）→ M3: ≥60%（~85s）→ M4: 冲 75–90%**。每级有独立价值（M1 即是 5× 提升）。

## 2. S6 后新 profile（2026-06-11，铁律：先归因再动手）

口径：`page_cache=1`，`LOG_LEVEL=warn` + `ext4fs.phase2_profile=1`，profile run TOTAL=**1371.2s**（vs 诚实 error 口径 1332.2s，+2.9%，占比代表性成立）。日志：`benchmark/logs/sqlite_20260611_071634/sqlite_ext4_pc1.log`。

**末尾累计读数：**
- `[ext4-bufw] calls=9,074,952 fast_calls=8,526,085 avg_fast_us=93 total_fast_ms=798,628 | slow_calls=548,867 avg_slow_prepare_us=552 (S6 前 1193) avg_slow_us=660 total_slow_ms=362,604`
- `[ext4-phase2] runtime_lock_acquires=1.29M avg_hold_us=305 (→锁内串行 ~395s) journaled write_ops=571,521 avg_total_us=648 commits_finished=5155 checkpoints=5155 overlay_reads=38.58M overlay_hits=22.17M metadata_writes=1.91M`
- `[block-profile] write-bio bios=336,397 avg_bytes=9,304（S4 批量生效）device_wait 合计 ≈10s；**read-bio bios=39,183,581 bytes=166.1GB avg_bytes=4,239 avg_device_wait_us=18 → 设备读等待 ≈705s**`

**归因表（按桶，对 1371s）：**

| 桶 | 证据 | 时间 | 占比 | 攻击手段 |
|----|------|----:|----:|---------|
| **fast path 覆盖写检查**（每写 map_blocks+stat 的元数据设备读）| 8.53M×93us；39M 读 bio 即其来源 | **798.6s** | **58%** | **P1 元数据缓存**（~81us/次是设备读）+ P2 形态瘦身 |
| **slow path journaled 写**（S6 后：转换+handle+探测）| 549K×660us | **362.6s** | **26%** | P1（探测/树读走缓存）+ P2 区间集 + P4 |
| commit/checkpoint/fsync/写回 | 5155 commits（checkpoints=5155 疑似每 commit 一次，待核实）；写 bio 仅 ~10s | **~180s** | **13%** | P3（脏页索引、commit 大 bio、sb/checkpoint 节流）+ P4 |
| 读（SELECT）| 三角实测 | ~26s | ~2% | 已追平 |

**决定性结论：一个写 33GB 的负载从设备读了 166GB（39.2M 次 ~4KB 读）——元数据零缓存（差距 1）是当前的单一最大瓶颈，P1 同时削 58% 桶的主体与 26% 桶的内部读。** S6 把 slow prepare 从 1193→552us 砍半但调用数未变（追加写仍逐写走慢路径），印证 P2 的"快路径接纳已预分配追加"也有肉。

## 3. 代码排查：五大机制级差距（按预期收益排序）

### 差距 1：ext4_rs 没有任何元数据缓存——每次元数据访问都是同步 virtio 往返

**证据**：`KernelBlockDeviceAdapter::read_offset_into`（fs.rs:808）直通 `self.inner.read()`，无任何缓存层；ext4_rs 内部 `get_inode_ref`（每次重读 inode 块）、`find_extent`（每层树块 `Block::load` 重读）、balloc（位图重读）全部走它。Phase 5 的 `inode_meta_cache` 只盖住 fs.rs 的 `stat()`，**没盖住 ext4_rs 内部**。
**对照**：Linux 有 buffer cache（bh 层，元数据常驻内存）+ extent status tree；ext2-Asterinas inode 表常驻内存。
**后果**：写快路径的 `write_range_fully_mapped` 每次 write() 做一遍"inode 块读 + extent 树遍历"= 多次设备往返（旧 profile 98us/次 × 8.5M 次 = 843s，41%）；写回路径 `write_at` 的 `resolve_pblock` 闭包**无 extent 缓存，逐 4KB 块一次 find_extent**；balloc/转换/目录操作同罪。
**修法（P1）**：ext4_rs 层 pblock-keyed 有界 LRU 元数据块缓存（inode 块/extent 树块/位图，16–64MB），读穿透、写经由 `metadata_writer` 单 chokepoint 同步更新（写本来就全部过 journal handle 包装的 writer，天然单点）。
**风险**：与 JBD2 overlay 读（JournalIoBridge 的事务内读新）和 recovery 路径的一致性——cache 必须与 overlay 同层或在其下并在 replay 后失效；crash matrix 全量门控。
**预期**：吃掉 41% 桶的大部分 + 写回/分配/目录的设备读。1332 → **~500–600s**。工程 ~1 周。

### 差距 2：write() 快路径形态 vs ext2（每写多做 4 件事）

**证据**（`write_at_page_cache` fs.rs:4846）：① `vec![0u8; len]` 分配 + 双拷贝（ext2 直接 `pages().write(offset, reader)`）② `stat(ino)` ③ `write_range_fully_mapped` 全树映射检查 ④ 纯覆盖写也调 `invalidate_direct_read_cache(ino)`（映射没变，过度失效）。另：**`run_journaled_ext4` 每个 journaled op 清空全部 inode 的 meta cache**（fs.rs:4123-4124）——SQLite 的 rollback-journal 文件追加（journaled）与 DB 覆盖写（要 stat）交错 → stat 持续 miss 回设备。
**对照**：Linux write() = folio copy + es-tree 内存查询，零设备 I/O；ext2-Asterinas write() = upread 锁 + pages().write + 内存 size/mtime。
**修法（P2）**：① 权威的 per-inode 内存 extent 覆盖区间集（在 prepare/convert/truncate 的 chokepoint 增量维护，非 2a 的"查询结果缓存"；S6 左合并保证树紧凑、区间数小）→ 快路径 = 锁 + 区间查 + 直写 page cache；② meta cache 改 per-ino 失效（chokepoint 已知 `affected_ino`）；③ 纯覆盖不失效 read cache；④ 去 Vec 双拷贝。
**预期**：快路径 98us → <10us。叠加 P1：→ **~300–400s（13–17%）**。工程 3–5 天。

### 差距 3：fsync 写回扫描 O(file_size)，且逐块重解析映射

**证据**：fsync → `flush_all` → `evict_range(0..file_size)`（page_cache.rs:354）**逐页 peek 整个文件范围**（1GB DB = 26 万次 map 查找/次 fsync，持 pages 锁），写完再 `iter_mut` 全量标 clean；每个脏 run 的 `write_page_cache_data_at` 还要 map_blocks + `ext4_write_at` 内逐块 `get_pblock_idx`（差距 1 放大）。
**对照**：Linux 用 xarray dirty tag，writeback 只触达脏页；mpage 一次映射整 extent。
**修法（P3a）**：PageCache 维护脏页索引集合（BTreeSet/位图）或脏区间 [min,max]，fsync 只遍历脏集；run 映射走 P2 的内存区间集。
**预期**：fsync 固定开销从 ~数 ms 级降到 ~脏页数线性。与 P1/P2 叠加 → **~200–280s**。工程 2–3 天（动共享 page_cache.rs，ext2 行为必须不变，需 coherency 守底）。

### 差距 4：jbd2 commit 微观形态（3 处偏离 Linux）

**证据**（jbd2/mod.rs:144-217 + fs.rs:2156）：① descriptor + N 元数据块 + commit 块**逐块同步写**（每块一次 virtio 往返；journal 区物理连续，本可一个大 bio）② **每次 commit 重写 journal superblock**（mod.rs:203；Linux 只在 checkpoint/recovery 边界写 sb，replay 靠块头 sequence 扫描）③ flush 次数 = 2/fsync（jbd2 内 1 + inode.rs:523 尾部 1）——这与 Linux 等价，**不能再省**（virtio-blk 无 FUA）。
**修法（P3b）**：journal 块合并大 bio（绕 wrap 拆两段）；sb 更新延迟到 checkpoint（recovery.rs 需确认 replay 不依赖每-commit 的 sb.head——它有 sequence 链可走；改动必过 crash matrix + jbd_phase1）。
**预期**：每 commit 设备往返 (N+3)→~3。~6k commit 节省数十 s 级。工程 2–4 天（recovery 正确性关键）。

### 差距 5：fdatasync == fsync（保守实现），白付一半 jbd2 commit

**证据**：inode.rs:534-548 注释明示 fdatasync 走 fsync 同路径。SQLite 大量用 fdatasync；纯覆盖写事务（UPDATE 类）只改数据 + mtime，**Linux fdatasync 不为 mtime 提交 journal**——只写回数据 + flush，跳过 jbd2 commit。
**修法（P4）**：真 fdatasync：仅当 size/extent 等"取数所需元数据"变更时才 force-commit；纯覆盖 → 写回 + flush 即返回。SQLite 的 DB 文件 fsync 在 S6 预分配 + 覆盖写场景下大半可免 commit。
**预期**：commit 次数砍 30–50%。工程 2–3 天，**语义敏感**：必须过 host-crash fsync matrix + Phase 3 fsync/flush 套件（fdatasync 后 crash，size 可见性必须正确）。

### （核对过、不必做/不能做的）
- **flush 次数**：已是 Linux 形态（2/fsync），virtio 无 FUA，省不掉。
- **delalloc/后台 flusher**：仍被"途中写回损坏"平台墙挡死（Stage 1a），本路线不依赖它。
- **砍日志/改 SQLite 配置**：违反优秀档功能要求/诚实口径，不做。
- **页缓存写路径本体**（VMO write）：ext2 同路径达 95%，不是瓶颈。

## 4. 路线与里程碑（每步：профile 重跑 + 完整守底门控）

| 步 | 内容 | 依赖 | 预期 TOTAL（按 §2 实测校准）| 工程 | 风险 |
|----|------|------|-----------|------|------|
| P0 | ✅ S6 后 profile 已回填 §2：166GB 元数据读风暴 = 单一最大瓶颈 | — | — | 已完成 | — |
| P1 | ✅ **已落地（2026-06-11，实测 454.3s = 11.26%，−66%）**：adapter 层 `DeviceBlockCache`（write-through 设备镜像，O_DIRECT bio 显式失效；ext4_rs 仅 get_inode_ref 对齐读）。命中率 98.5%（38.58M hits），fast path 93→23us、slow prepare 552→216us。守底全绿（crash 18/18 / 八套件 / 并发两层 / host-crash 4/4 / fio write 79.08%）。P1 后新桶：fast 200.5s/41% + slow 161.5s/33% + commit/fsync ~100s/20%（489s profile 口径）| — | ~~350–500s~~ **454.3s ✓** | 1 天（设计将一致性局部化到 adapter，远低于原估 1 周）| 已过门控 |
| P2 | ✅ **已落地（2026-06-11，实测 243.9s = 20.88%，−46%）**：WrittenCoverage 区间集（coverage⊆truth，populate-on-miss + prepare 后插入 + Truncate/touch-cache 失效）+ VmReader 直写去双拷贝 + meta cache per-ino 失效。"接纳预分配追加走快路径"未做（移入 P3/P4 与 fdatasync 一并考虑）。**捎带修复 S6 潜伏 388 panic（尾段部分插入失败整体释放→双重释放，修为只释放未插入后缀 + ext_remove_leaf 防御钳制）**。守底全绿（fsync_flush 修复后 ×2、crash 18/18×2、fio write 75.02% 贴线待复跑）| — | ~~180–280s~~ **243.9s ✓** | 1 天 | 已过门控 |
| P3-1 | ❌ **size-only append 已试已回退（2026-06-11）**：三轮实测均净负（462.9/312.8/468.0 vs P2 243.9）。journal 侧省 23s 被 +69s 非 journal 开销和读侧 30% 回退吃掉，全局病灶未钉死。patch 留存 `benchmark/logs/p3_1_size_only_append_attempt_20260611.patch`；重启前置条件 = 细分 instrumentation（writeback 转换/populate/find_extent 计数）。P2 后真实桶：fast 4.9s + slow 153s（549K×278us，其中 journaled op avg 229us = 框架 40us + apply 187us）+ commit/fsync ~90s | — | 净负回退 | 1 天 | 教训已记 milestone |
| P3 | a) 脏页索引化 fsync b) jbd2 commit 大 bio + sb 延迟 + 核实 checkpoints=5131/commit 语义 | P1 | **~150–200s（25–34%）**（攻 ~90s commit/fsync 桶 + slow 中的框架开销）| 4–7 天 | a) 动 ext2 共享路径 b) recovery 正确性 |
| P4 | 真 fdatasync（数据型同步免 commit）| P3 | **~85–140s（37–60%）**| 2–3 天 | 持久化语义（host-crash + fsync/flush 套件）|
| P5 | 残余按 P4 后 profile 定（候选：目录 op 缓存化——SQLite journal 文件每事务 create/unlink；group commit；锁粒度）| P4 | 冲 **65–110s（47–80%+）**| 按需 | — |

可行性分档：**≥35%（M2）高置信**；**60% 中等**；**90% 边缘**——取决于 P1–P4 全部落满后日志税 + 残余 per-op 开销的实测，若 P4 后 profile 显示 commit 税 >15s，90% 即不可达，应按"诚实口径 + 三角归因"作答辩叙事（与日志税共存的最优解）。

## 5. 验收与守底（不变）

每步不回退：crash matrix 18/18、host-crash 4/4、phase6/pagecache/phase4/phase3 xfstests、并发两层、fsync_flush、fio O_DIRECT 75%+、SQLite `integrity_check` PASS。P3a 动共享 page_cache.rs 必须 ext2 行为不变 + coherency 套件；P3b/P4 动持久化语义，crash 类全量 + 新增"fdatasync 后 crash"用例。
