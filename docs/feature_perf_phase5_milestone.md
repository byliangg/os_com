# ext4 性能优化 Phase 5 Milestone 记录（延迟归因驱动）

首次创建时间：2026-06-02（Asia/Shanghai）

配套计划：`feature_perf_phase5_plan.md`
基线证据：`fio_direct_parameter_sweep_report.md`、`fio_direct_senior_feedback_response.md`

## 当前状态（2026-06-03）

| Step | 内容 | 状态 |
|------|------|------|
| Step 0 | 基线固化 & profile 盘点 | ✅ 完成 |
| Step 1 | 收尾 dump + 收割阶段占比表 | ✅ 完成（四层 profile 落地，1M/4K/16K write+read 占比表已收割） |
| Step 2 | 定位 → 选优化点 | ✅ 选定「读 extent-mapping plan 缓存」（学长已批） |
| Step 3 | 实施优化 | ✅ 收口：4K/16K/64K read +50~60%、1M read +24%（同轮 A/B）；正确性 pagecache 100% + 091/130 零差异 + full-guard@drc=0 三模式 100% |
| Step 3b | 全文件覆盖结果缓存（学长追问的元数据缓存方向） | ✅ 收口：随机读 4K/16K ×1.30~1.32、1M seq hit→100%；plan 74µs→0；full-guard@drc=0 三模式 100% |
| Step 3c | atime 按秒节流 | ✅ 小块读 +12~13% |
| Step 3d | ⭐ ext4 inode 元数据缓存（ext2 对照催生） | ✅ **小块读 16–24%→84–95%（×2.6–5.2）**、1M write 63→76%；完整守底全绿 |
| Step 4 | 全量回归 + benchmark 收口 | ✅ 完整守底 @drc=0 全绿（crash 18/18、host-crash 4/4、concurrency 7/7、xfstests 全 100%）；读优化定版 |

**Step 1 一句话结论**：① 大块写 71% 在 virtio device_wait（ext4 之下，ext4 plan/prepare/touch=0）；② 小块瓶颈在**读**的 ext4 `plan`（extent 映射，4K read 占 75%、固定 ~60µs）——这是射程内最佳优化点；③ 锁竞争单 job 不可见，需 nj≥2。

## Phase 5 最终结果（read+write 完整表，2026-06-05）

口径：`direct=1, nj=1`，cache-off（`EXT4_PAGE_CACHE=0` `EXT4_DIRECT_READ_CACHE=0`）+ 本优化 `extent_map_cache=1` + inode 缓存（默认 on）+ **`BENCH_DROP_CACHES=1`（drop 公平基线）**，中位数。日志：read `benchmark/logs/phase5_inodecache_ratio/`、write `benchmark/logs/phase5_write_sweep/`。

| bs | read MB/s (Aster/Linux) | **read ratio** | write MB/s (Aster/Linux) | **write ratio** |
|----|------------------------:|---------------:|-------------------------:|----------------:|
| 4K | 165.5 / 191.5 | **86.38%** | 179.0 / 237.0 | **75.54%** |
| 16K | 647.0 / 766.5 | **84.42%** | 627.0 / 827.5 | **75.78%** |
| 64K | 1933.0 / 2227.5 | **86.89%** | 1756.0 / 2095.0 | **84.09%** |
| 256K | 4098.0 / 4333.5 | **94.81%** | 3363.5 / 2778.5 | **121.07%** |
| 1M | 3648.0 / 2978.0 | **122.94%** | 2659.0 / 3042.0 | **88.28%** |

（read 与 write 来自两轮不同会话，Linux 绝对基线略有方差；每行 ratio 为同会话 Asterinas/Linux，单独有效。1M write 单轮 81/95%，中位 88%。）

### 优化前 → 优化后（同口径净提升）

| bs | read 前→后 | write 前→后 |
|----|-----------|-----------|
| 4K | 16.67% → **86.38%** | 20.55% → **75.54%** |
| 16K | 18.31% → **84.42%** | — → **75.78%** |
| 64K | 24.46% → **86.89%** | — → **84.09%** |
| 256K | 36.20% → **94.81%** | — → **121.07%** |
| 1M | 118.69% → **122.94%** | 63.44% → **88.28%** |

### 一句话结论
四个 ext4 优化（extent 映射缓存 / 全文件覆盖 / atime 节流 / **inode 元数据缓存**）把读写双双从 16–63% 拉到 **75–123%**。**ext4 域内的 per-op 固定开销已榨干**（profile：write `prepare(mtime)=0`、`above≈3–20µs`、stat 已缓存），读写现在都顶在 **virtio 设备往返**这个平台地板上（写往返本就比读慢 → 小块写 ~75% 比读 ~86% 低约 10pt，属 virtio/平台层、跨 FS 通用，ext2 同此极限）。

### 剩余开放项（非"无懈可击"）
- **性能**：并发读 nj>1 退化（Step 1 见 nj2 68%，inode 缓存后未重测）；bio_copy 零拷贝（1M 写 17%，难）；其它 workload（buffered/lmbench/metadata）未在此优化口径下测；virtio 往返延迟（平台层，与 1M write blocker 同根，需与学长定位）。
- **功能**：xfstests 为精选子集（非上游全量）；已知缺口 symlink 读不支持、O_DIRECT 512B 对齐不支持；ext2 4K direct write hang（佐证平台小块 direct 路径有坑）。

## Phase 4 收口基线（Phase 5 守底，不能回退）

| 测试项 | Phase 4 收口结果 | Phase 5 要求 |
|--------|------------------|--------------|
| `phase3_base_guard` | PASS, 100% | 不回退 |
| `phase4_good` | PASS, 100% | 不回退 |
| `phase6_good` | `25/25 PASS`（sweep G 组 `generic/011` 偶发失败待复查） | 不回退 + 复查 011 |
| `jbd_phase1` | PASS, 100% | 不回退 |
| JBD2 crash matrix | `18/18 PASS` | 不回退 |
| Phase 2 concurrency | `7/7 PASS` | 不回退 |
| `jbd_phase3_fsync_flush` | 0 FAIL | 不回退 |
| Phase 3 host-crash fsync matrix | `4/4 PASS` | 不回退 |
| `pagecache_phase4` | `9 PASS / 0 FAIL / 4 NOTRUN`，有效样本 100% | 不回退 |

## 性能基线（cache-off 诚实口径，2026-05-18/19 sweep）

| 维度 | Asterinas | Linux | ratio |
|------|----------:|------:|------:|
| O_DIRECT write `bs=1M nj=1` | 1707 MB/s | 3308 MB/s | **51.60%**（主 blocker） |
| O_DIRECT read `bs=1M nj=1` | 2821 MB/s | 2768 MB/s | **101.91%**（已达标） |
| O_DIRECT write `bs=1M nj=4`（ext4j） | 3824 MB/s | 4025 MB/s | 95.01%（多 job 可达标） |
| O_DIRECT write `bs=4K nj=1`（ext4j） | 45.1 MB/s | 219 MB/s | 20.59%（小块 ext4 自身弱） |
| O_DIRECT read `bs=4K nj=1`（ext4j） | 22.3 MB/s | 196 MB/s | 11.38%（小块 ext4 自身弱） |

三瓶颈分解结论见 plan §2。

## 三方对齐结论（学长 + Claude + Codex）

1. 优化主线拉回 O_DIRECT write，**不是 PageCache**；
2. JBD2 与大块 ext4 路径数据上已洗清嫌疑，单 job 写天花板在 block/virtio；
3. instrumentation 已端到端建好（FS/virtio/锁/JBD2 四层），**缺的是 final dump + 收割**，不是采点；
4. 必须加入 4K/16K 档——小块才是 ext4 自身的优化故事；
5. `bio_wait_return_after_complete_ns` 重点看——若高则是 waiter 唤醒路径（好修）；
6. 1M 大块定位（virtio vs ext4）需与学长对齐答辩口径。

## Step 0：基线固化 & profile 盘点

**状态：** ✅ 完成（2026-06-02）

### 改动概要
- 新建 `feature_perf_phase5_plan.md` / `feature_perf_phase5_milestone.md`；
- 索引文档（根 `CLAUDE.md` / `AGENTS.md`、`asterinas/AGENTS.md` / `asterinas/CLAUDE.md`）由 phase4 切换指向 phase5；
- 完整 sweep 报告与学长反馈纳入 Phase 5 基线证据。

### 涉及文件
- `feature_perf_phase5_plan.md`（新建）
- `feature_perf_phase5_milestone.md`（新建）
- `CLAUDE.md`、`AGENTS.md`、`asterinas/AGENTS.md`、`asterinas/CLAUDE.md`（索引更新）

### 性能结果
- 未跑新 benchmark；引用 2026-05-18/19 sweep 作为基线。

### 功能回归
- 无代码改动，守底不受影响。

## Step 1：收尾 dump + 收割阶段占比表

**状态：** ✅ 完成（2026-06-03，代码 + 矩阵收割 + 归因结论）

### 改动概要
- `bio.rs`：新增 `pub fn dump_read_bio_profile()` / `dump_write_bio_profile()`（无条件强制打印），`maybe_log_read/write_bio_profile` 加 `force` 旁路，两个调用点传 `false`；
- `fs.rs`：`maybe_log_direct_read_profile` 移除硬编码 `DIRECT_READ_PROFILE_LOG_ENABLED=false`，改由 `phase2_profile + force` 控制（**之前读 profile 完全打不开**）；`maybe_log_direct_write_profile` 加 `force`；新增 `dump_perf_summary()` 一次性收割四层；挂到 `FileSystem::sync()` 末尾（非 shutdown）；
- `bench_linux_and_aster.sh`：转发 `EXT4_PHASE2_PROFILE` + `LOG_LEVEL`（默认不变）；
- 新增可复用 probe 脚本 `test/initramfs/src/benchmark/fio/run_phase5_profile_probe.sh`。

### 涉及文件
- `kernel/comps/block/src/bio.rs`
- `kernel/src/fs/ext4/fs.rs`
- `test/initramfs/src/benchmark/bench_linux_and_aster.sh`
- `test/initramfs/src/benchmark/fio/run_phase5_profile_probe.sh`（新建）

### 编译
- `cargo check -p aster-kernel --target x86_64-unknown-none`：**exit 0**，仅 2 个既有 warning，无新 warning。

### 口径
- Asterinas-only、cache-off（`EXT4_PAGE_CACHE=0` `EXT4_DIRECT_READ_CACHE=0`）、`ext4fs.phase2_profile=1`、`LOG_LEVEL=warn`、`numjobs=1`；
- 注意 `[ext4-phase2]`/`[ext4-direct-write]` 是 `warn!` 打印，必须 `LOG_LEVEL=warn`；`[block-profile]`/`[ext4-profile] direct-read` 无条件 print；
- 日志：`benchmark/logs/phase5_smoke/`、`benchmark/logs/phase5_matrix/`。

### 写路径阶段占比（`[ext4-direct-write]` 平均 µs/写；device_wait 来自 `[block-profile]`）

| case | total | bio_wait | bio_copy | plan | prepare | touch | wait_after_complete | block device_wait |
|------|------:|---------:|---------:|-----:|--------:|------:|--------------------:|------------------:|
| ext4j-write-1M | 268 | **191 (71%)** | 52 (19%) | 0 | 0 | 0 | 9 | 178 |
| ext4j-write-16K | 34 | 31 (91%) | 0 | 0 | 0 | 0 | 7 | 22 |
| ext4j-write-4K | 30 | 28 (93%) | 0 | 0 | 0 | 0 | 7 | 19 |
| ext4n-write-1M | 359 | 259 (72%) | 71 (20%) | 0 | 0 | 0 | 12 | 240 |

### 读路径阶段占比（`[ext4-profile] direct-read` 平均 µs/读）

| case | plan | wait | copy | plan 占比 |
|------|-----:|-----:|-----:|----------:|
| ext4j-read-1M | 63 | 117 | 49 | 27% |
| ext4j-read-4K | **60** | 20 | 0 | **75%** |

### 归因结论

1. **① 大块单 job 写 = virtio device_wait，已钉死在 ext4 之下。** ext4j-write-1M 总 268µs 中 `bio_wait=191µs(71%) ≈ block device_wait=178µs`；ext4 `plan/prepare/touch = 0µs`；`wait_after_complete=9µs`（waiter 唤醒**不是**瓶颈）。与 sweep "ext4≈raw(105%)" 互相印证。
2. **② 小块瓶颈在"读"，且是 ext4 自己的 `plan`（extent 映射）。** ext4j-read-4K 的 `plan=60µs` 占 **75%**，而 `plan` 在 1M/4K 上都固定 ~60–63µs——**每次读重走 extent 映射的固定开销**，小块下无法摊薄。这正是 sweep 最差格子（4K read 11%）的根因，也是**最具"我们优化了 ext4"故事、且射程内**的点。
3. **小块"写"反而不是 ext4 CPU 的锅**：4K write 的 `plan/prepare/touch` 同样 = 0，总时间 93% 在 `bio_wait`（per-request device 延迟）。即小块写差是 per-request 设备延迟无法摊薄，不是 ext4 元数据开销。
4. **③ 锁竞争在单 job 下不可见**：所有 case `avg_wait_us=0`（`max_hold_us` 的 33s 离群值是收尾 commit/checkpoint，非稳态）。读的并发退化（sweep nj2 100%→68%）需 `numjobs≥2` 才能在 profile 中显形——列入 Step 1b。
5. **JBD2 洗清（写）**：ext4j-write journaled_ops 整轮仅 165、overlay 命中 99.998%；nojournal 路径 journaled_ops=0 但 device_wait 反而更高，进一步说明写瓶颈在设备而非 JBD2。读路径有 atime 触发的 journaled 写（read-4K write_ops=262145），值得单独看是否可降。
6. **次要写优化点 `bio_copy`**：1M 写 52µs（19%）、nojournal 71µs，是用户 buffer→DMA 的 memcpy；profile 显示用户 buffer 为 256 个非连续物理页（`max_user_phys_run_pages=1`），零拷贝 SG 需 256 段——历史已记为"暂不主线"，但 19% 值得重估。

### 优化候选排序（Step 2 输入）

1. **读 extent-mapping plan 缓存**（砍 60µs 固定 `plan`）——射程内、故事性最好、直击 sweep 最差格子；与已退役的 DirectReadCache 的 mapping 缓存思路相关，但只缓存 metadata-only extent plan，不复活数据 cache。
2. **写 `bio_copy` 零拷贝**（1M 写 19%）——较难（256 非连续页）。
3. **1M 写 device_wait 请求 overlap / 队列深度**——在 ext4 之下，解释 numjobs=4 达标；答辩定位需与学长对齐。

### 遗留 / Step 1b
- `raw-write-1M` 未出 profile 行：block-profile 的 enable 绑定在 ext4 fs 初始化（`set_write_bio_profile_enabled`），raw 不挂 ext4 → 未开。ext4≈raw 已由 sweep ratio 确立，暂不补；如需 raw 直接对照可后续把 block-profile enable 独立于 ext4。
- `numjobs≥2` 锁竞争 profile（验证 ③ 读并发退化）。

### 守底回归
- 本 step 仅新增默认关闭的 profile dump 与 benchmark 工具，未改 correctness 路径；守底回归留待 Step 3 优化前统一复跑（Phase 4 收口基线全绿）。

## Step 2/3：metadata-only extent 映射缓存（读路径优化）

**状态：** ✅ 代码 + profile 验证完成；待全量 fio 复跑与守底回归（2026-06-03）

### 问题根因（profile 实测 + 代码核实）
- 读路径每次 `find_extent`（[extents.rs:220](asterinas/kernel/libs/ext4_rs/src/ext4_impls/extents.rs#L220)）都**从块设备直接读 extent tree 的 index/leaf 中间节点，无缓存**；
- 文件需多级 extent 树时，每次 plan 重读 2–3 个元数据块 ≈ 3 × ~20µs = **固定 ~60µs/读**，与块大小无关；
- 小块读被这固定开销主导：4K read `plan` 占 75%——即 sweep 最差格子 4K read 11% 的根因。

### 改动概要
- 新增 per-inode **metadata-only** extent 映射缓存 `ExtentMapCacheEntry { file_offset, len, mappings }`——只存 `SimpleBlockRange` 映射，**不存数据、不预读**；
- `plan_direct_read_extent_map_cached()`：命中切片缓存映射、跳过整个 `find_extent`（含磁盘读）；未命中用 8MiB 适度窗口解析一次（纯元数据）后缓存；
- `read_direct_at` 三级分流：投机数据 cache（opt-in，off）→ 本 metadata cache（默认 on）→ 裸 walk；
- 失效复用 `invalidate_direct_read_cache`（继承 write/truncate/fallocate/unlink/rename/shutdown 所有钩子）；
- 新 flag `ext4fs.extent_map_cache`（默认 on，与退役的投机数据 cache 独立，cache-off 守底口径下仍生效）；Makefile + bench 脚本已转发 `EXT4_EXTENT_MAP_CACHE`。

### 涉及文件
- `kernel/src/fs/ext4/fs.rs`、`Makefile`、`test/initramfs/src/benchmark/bench_linux_and_aster.sh`
- `test/initramfs/src/benchmark/fio/ext4_rand_read_bw/run.sh`（新建，随机读验证用）

### 安全性
1. 只缓存只读映射（几个整数），数据每次从盘新鲜读 → 不可能返回旧数据；
2. 读写同 inode 由 `inode_correctness_lock` 串行化 → 无并发变更撞缓存；
3. 所有改 extent 的操作都已调 `invalidate_direct_read_cache` → 映射不会比 extent 活得久；
4. 与退役 `DirectReadCache`（数据 + 128–512MiB 投机预读，即 127% 那个）清晰隔离。

### profile 验证（cache-off 口径，nj=1，`extent_map_cache=1`）
日志：`benchmark/logs/phase5_extmap_on/`。`plan` 为优化前→优化后（优化前数据见 Step 1）。

| case | plan 前→后 | cache_hit 率 | 每读总时 前→后 |
|------|-----------|------------:|---------------|
| 4K read | 60µs → **0µs** | 99.95% | ~80µs → **~25µs（~3.2×）** |
| 16K read | 60µs → **0µs** | 99.8% | 大幅下降 |
| 1M read | 63µs → **11µs** | 87.5% | 229µs → ~206µs（+11%）|

- 4K/16K 读从 plan 主导（75%）变为 wait 主导，**plan 固定开销被消除**；
- 1M read hit 87.5%（8MiB 窗口=8 个 1M 读后需重 plan），残留 11µs；1M read 本就达标，可后续调大窗口。

### 学长追问 → 元数据块缓存 headroom（Step 3b 评估）
- 学长指出根因是元数据路径"每次读盘不缓存"，建议缓存 extent tree 中间节点块；
- 本 A 缓存是**结果（映射）缓存**，对顺序读已消除 60µs；元数据**块**缓存是更通用的下一层（random read / 失效后首读 / 其他元数据 workload）；
- 用随机读 probe（`ext4j-randread-4K/16K`，日志 `benchmark/logs/phase5_randread/`）量化 A 之外的剩余 plan，作为元数据块缓存的收益上界；
- 风险：元数据块缓存的失效要对齐 JBD2 元数据回写 / 块释放复用（Phase 4 stale-metadata bug 前车之鉴），安全版应先只缓存 extent-tree 块 + 复用 per-inode 失效。

### A/B ratio 实测（Asterinas vs Linux，BENCH_RUN_ONLY=both，cache-off 其余口径）
日志：`benchmark/logs/phase5_ratio_ab/`。同轮 A/B：`EXT4_EXTENT_MAP_CACHE=0`（前）vs `=1`（后）。

完整同轮 A/B（c0/c1 同会话配对；4K/16K 来自 `phase5_ratio_ab`，64K/1M/write 来自 `phase5_ratio_ab_fill`）：

| bs | cache=0（前） | cache=1（后） | 提升 |
|----|------------:|------------:|------|
| **4K read** | 10.94% | **16.63%** | **+5.7pt / ×1.52** |
| **16K read** | 11.80% | **18.78%** | **+7.0pt / ×1.59** |
| **64K read** | 16.47% | **24.73%** | **+8.3pt / ×1.50** |
| **1M read** | 48.87% | **60.66%** | **+11.8pt / ×1.24** |
| 1M write | — | 99.54%※ | 不回退（Asterinas 2591 MB/s 稳定）|

**结论：metadata-only extent 映射缓存对所有块大小的 O_DIRECT 读都有效**，小块 ~1.5×、1M ~1.24×（1M plan 63µs→11µs），远超赛题"≥5%"。写路径未触碰、无回退。

诚实标注：
- **同轮 A/B 是可信口径**：c0/c1 对同会话同一 Linux 基线，直接体现优化净效果；
- **Linux 侧方差很大、跨会话不可比**：本轮 Linux 1M read 4942–5111 MB/s、1M write 2603，而 2 周前 sweep 分别是 2768、3308。所以 cache=0 的"% of Linux"绝对值（如 1M read 48.87% vs sweep 102%；1M write 99.54% vs sweep 51.60%）是 Linux 基线漂移所致，**不能跨会话对比，也不代表写性能提升**；
- 这条对答辩重要：**官方"% of Linux"对 Linux 侧状态敏感，应以同轮 A/B 报告本优化的净提升**，并把 Linux 基线方差现象记录在案（需与学长对齐报告口径）；
- 4K Asterinas 19.8→29.1 MB/s（×1.47）；ratio 净增 ×1.5 而非 profile 的 ×3.2，因小块端到端还有 ext4 之外 per-call 开销（syscall/锁/atime），plan 砍掉后成新大头。

### 守底回归（2026-06-03）
日志：`benchmark/logs/phase5_regression/`、`phase5_full_guard/`。runner 已新增 `ext4fs.direct_read_cache` / `ext4fs.extent_map_cache` kcmd-args（默认保持原行为）。

> 关键：本缓存只在 `page_cache=0 且 direct_read_cache=0` 时激活，普通守底默认 `direct_read_cache=1`（老投机缓存）、`pagecache_phase4` 强制 `page_cache=1`——都测不到本缓存。故需专门强制 `EXT4_DIRECT_READ_CACHE=0` 跑。

| 测试 | 配置 | 结果 |
|------|------|------|
| `pagecache_phase4` 全表 | 默认（page_cache=1, drc=1）| **PASS 100%**（read_direct_at 重构非回归）|
| `generic/091`（fsx）A/B | page_cache=0，drc=1 vs 0 | **drc=1 ≡ drc=0 字节一致** → 本缓存零行为差异 |
| `generic/130` A/B | page_cache=0，drc=1 vs 0 | drc=1 ≡ drc=0 一致（两者在 page_cache=0 下均非 PASS，是 pagecache-mode 用例不适配 page_cache=0，与缓存无关）|
| full-guard @ drc=0 | phase4_good + phase3_base + jbd_phase1 全表，**本缓存激活** | **全 100% PASS** ✅ |

判读：①pagecache_phase4 全绿证明 read_direct_at 重构安全；②091/130 的 drc 1-vs-0 字节一致，证明本缓存与已验证的投机缓存**零行为差异**（共用同一套失效钩子 + 切片逻辑 + per-inode 串行锁）；③full-guard@drc=0（phase4_good/phase3_base/jbd_phase1 全 100%）给出本缓存在真实守底套件上的干净 PASS 判决。

**→ A（extent 映射缓存）正确性收口。**

### Step 3 收口结论
- 性能：小块顺序读 4K/16K/64K +50~60%、1M read +24%（同轮 A/B），profile plan 60µs→0；
- 正确性：pagecache_phase4 100% + 091/130 零差异 + full-guard@drc=0 三模式 100%；
- 代码：默认 on、cache-off 口径生效、与投机数据 cache 清晰隔离。

## Step 3b：全文件覆盖结果缓存（随机读优化）

**状态：** ✅ 代码 + profile 验证；🔵 full-guard@drc=0 复核中（2026-06-03）

### 设计取舍（vs 学长字面提案）
- 学长字面提案是"缓存 extent tree 中间节点块"（block cache）——需改 ext4_rs `find_extent`、失效要对齐 JBD2 元数据回写（高风险）；
- 实际采用**更安全、对读路径更优**的做法：把 A 的结果缓存窗口从 8MiB 扩到 **1 GiB（覆盖整个典型文件）**，等价 Linux extent_status 缓存；
- 随机读时缓存基址随读到的最小 offset 单调下降，几次 miss 后即覆盖全文件 → 全部命中；
- **优于 block cache**：block cache 随机读仍需 CPU 树遍历（只省磁盘读），本结果缓存连遍历都省（一次二分查找）；复用已验证失效钩子，零新增 coherency 风险，不碰 ext4_rs；
- 加 `MAX_CACHED_EXTENTS=16384` 上限（~192 KiB/inode）防碎片文件 OOM，超限回退 per-read walk。

### profile 验证（cache-off 口径，nj=1，日志 `benchmark/logs/phase5_randread_3b/`）

| case | plan 前→后 | cache_hit 率 前→后 | miss |
|------|---:|---:|---:|
| **randread-4K** | 74µs → **0µs** | 1.0% → **99.997%** | 16 |
| **randread-16K** | 74µs → **0µs** | 1.3% → **99.998%** | 15 |
| 1M seq read | 11µs → **0µs** | 87.5% → **99.9998%** | 1 |

随机读 plan 彻底消除（基址几次 miss 即收敛全文件覆盖）；连 A 的 1M 顺序读 hit 也升到 ~100%。

### 涉及文件
- `kernel/src/fs/ext4/fs.rs`（`plan_direct_read_extent_map_cached` 窗口 + 上限）

### 守底回归（2026-06-03）
- full-guard@drc=0（**本缓存激活**）：phase4_good / phase3_base / jbd_phase1 **全 100% PASS**（日志 `benchmark/logs/phase5_full_guard_3b/`）→ 窗口扩大 + cap 不回退。

### 随机读 ratio A/B（both，extent_map_cache 0 vs 1，日志 `benchmark/logs/phase5_randread_ratio/`）

| bs | cache=0（前） | cache=1（后，3b） | 提升 |
|----|------------:|------------:|------|
| **randread-4K** | 7.23% | **9.54%** | +2.3pt / ×1.32 |
| **randread-16K** | 8.11% | **10.57%** | +2.5pt / ×1.30 |

随机读净提升 ~×1.3（4K 12.8→16.5 MB/s、16K 50.6→64.9 MB/s）。比顺序读小：profile 里 plan 74µs→0 确实消除，但随机 4K direct 端到端被 ext4 之外 per-IO 开销主导，realized 提升 ~×1.3。

**→ Step 3b 收口（性能 + 正确性 + 诚实 ratio 全齐）。**

## 待办（phase 收口前）
- [ ] 完整守底剩余项 @ drc=0（crash matrix / phase6_good / Phase 2 concurrency / fsync matrix）——本改动为只读路径、不碰 write/journal/recovery，风险低，留待 phase 整体收口统一复跑
- [ ] phase 收口后提交并 push 到 `jbd-phase-5-optimize`

## 基准方法学：Linux 基线波动归因 + drop_caches 基线（2026-06-03）

**问题**：Linux O_DIRECT 顺序读基线跨会话剧烈波动（1M read sweep 2768 ↔ 本会话 4942）。

**排查结论**：
- **KVM 排除**：`kvm_intel: VMX not supported` 是 guest 嵌套虚拟化噪音，host 正常加速。
- **根因**：QEMU `-drive` 默认 `cache=writeback`，宿主机缓存 backing image；guest `direct=1` 不绕过宿主机 page cache。fio 先写 1G 再读，读的是宿主机 RAM 热数据，吞吐随宿主机内存压力波动。
- **drop_caches 实测有效**（推翻"会被重新焐热"的预判）：每次 QEMU 前 drop 宿主机 cache 后，Linux 1M read 4942→**2818**（复现 sweep 2768，c0/c1 仅差 0.2%）。

**口径变更**：`BENCH_DROP_CACHES` 默认 `1`（`bench_linux_and_aster.sh` + `run_phase5_ratio_ab.sh`），每次 QEMU 启动前 drop 宿主机 cache。仅影响 perf 路径。详见 `benchmark.md` §6.5。

**重大修正：公平测量下 Asterinas 1M read 反超 Linux**

| 1M read | Asterinas | Linux | ratio |
|---------|----------:|------:|------:|
| 不带 drop（Linux 热 cache，不公平）| 2498–2998 | 4942–5111 | 49–61% |
| **带 drop（公平）** | 2947–3563 | 2818 | **104–126%** |

之前 milestone 记的"1M read ~52%/49%"是 Linux 热 cache 虚高所致；**公平口径下 Asterinas 1M O_DIRECT read 追平/反超 Linux**。读优化（extent map cache）净提升不受影响：带 drop 同轮 A/B 仍 ×1.21–1.83。

> 答辩口径建议：① perf 一律 `BENCH_DROP_CACHES=1`；② 报优化净提升用同轮 A/B（对 Linux 基线免疫）；③ 绝对"% of Linux"带 drop 后才可信、可复现。

## Step 3c：小块读全路径归因 + atime 按秒节流（2026-06-04）

**状态：** ✅ 收口（profile + 优化 + full-guard@drc=0 100%）

### 全路径 profile（新增 read_direct_at 墙钟 + atime 打点）
4K read 每读 ~120µs 拆解（日志 `benchmark/logs/phase5_fullpath/`）：

| 部分 | µs | 占比 | 层 | ext4 可修 |
|------|---:|---:|----|:--:|
| VFS / syscall / framekernel（read_direct_at 之上）| 66 | 55% | 平台 | ❌ |
| **atime（每读一次 `stat(ino)`）** | 31 | 26% | ext4 | ✅ |
| virtio 往返（wait）| 21 | 18% | 块层 | ⚠️ |

对照 Linux 整次 4K read = 18µs。

### 85–90% 可行性判断：不现实（平台层）
framekernel 每-syscall 开销 66µs 即 Linux 整次读的 3.7×；即便 atime/virtio 清零，单这 66µs 也把 4K 卡在 ~27%。**小块 direct read 的根本限制是 framekernel per-syscall 开销（平台层，超 ext4 射程）**，作为"定位到平台瓶颈"的研究结论。ext4 射程内能榨的（extent 查找 60µs + atime 31µs）已榨完。

### atime 优化
- **根因**：cache-off 口径下 atime 节流失效——`touch_atime` 的 relatime"无需更新"分支命中后不写 `inode_atime_cache` 就 return → 每读都 `stat(ino)`（≈31µs，读 inode 块）。
- **修法**（`touch_atime`）：relatime"无需更新"决定按秒写入 `inode_atime_cache`，同秒后续读跳过 stat；写操作仍 `remove` 该 cache（mtime/ctime 变化后正确重判）。
- **效果**（profile + Asterinas-only 吞吐）：atime 31µs→0、read_direct_at 54→26µs；4K read +12%（35→39.4 MB/s）、16K +13%（133→151）；估算 ratio 4K ~16.7%→~18.5%、16K ~18.3%→~20.6%（+~2pt）。全 bs 受益（1M read atime 39µs 同样消除）。
- **正确性**：full-guard@drc=0（phase4_good/phase3_base/jbd_phase1）三模式 **100% PASS**（日志 `benchmark/logs/phase5_full_guard_atime/`）。

### 涉及文件
- `kernel/src/fs/ext4/fs.rs`（`DirectReadProfileStats` 增 total/atime 打点；`read_direct_at` 墙钟；`touch_atime` relatime 决定按秒缓存）

## 公平基线整体画像（drop 口径 + extent_map_cache=1，3 轮中位数，2026-06-04）

日志：`benchmark/logs/phase5_guard_full/`。脚本：`run_phase5_guard_median.sh`。cache-off（`EXT4_PAGE_CACHE=0` `EXT4_DIRECT_READ_CACHE=0`）+ 本优化 `extent_map_cache=1` + `BENCH_DROP_CACHES=1`，nj=1。

| rw | bs | 中位 ratio | Asterinas MB/s | Linux MB/s |
|----|----|---------:|---------------:|-----------:|
| read | 4K | 16.67% | 35.7 | 213 |
| read | 16K | 18.31% | 140 | 759 |
| read | 64K | 24.46% | 524 | 2126 |
| read | 256K | 36.20% | 1685 | 4613 |
| read | **1M** | **118.69%** | 3626 | 3061 |
| write | 4K | 20.55% | 48.7 | 237 |
| write | **1M** | **63.44%** | 2075 | 3271 |

**修正后的官方守底站位（1M nj=1，公平口径）**：
- **read 118.69% —— 达标且反超 Linux**（旧"102%"是 Linux 热 cache 干扰；旧"127%"是投机数据 cache 不诚实数）；
- **write 63.44% —— 唯一 blocker**（旧"51%"也受 Linux 基线影响，公平口径下是 ~63%）。

**小块读 ratio 低的归因（瓶颈换层）**：本优化已消除 ext4 的 extent 查找开销（60µs→0）。小块 ratio 仍低是因为瓶颈转到 **per-request 固定开销**（syscall / VFS / per-inode 锁 / atime / virtio 单请求往返）——4K read 优化后 ext4 内部仅 ~25µs（理论 160 MB/s），实测 35.7 MB/s（≈109µs/读），差的 ~84µs 在 ext4 之外；Linux 整个 4K direct read 仅 ~18µs。与 1M write 卡 virtio device_wait 同源：块越小，固定 per-request 成本占比越大。读 ratio 随 bs 单调上升（4K 16.7% → 1M 118.7%）即此规律。

**drop 口径验证有效**：小块 read 3 轮中位极稳（4K 16.5/16.7/17.1，差 0.5pt）；唯 1M read 有一轮 Linux 偏快致 93.8（其余 ~119），中位 118.69 仍稳健。

## ext2 对照 → 推翻"平台天花板"结论，定位 ext4 缺 inode 缓存（2026-06-04）

用 Asterinas 成熟的 ext2（真 O_DIRECT，`read_blocks` 零拷贝直读）跑同口径对照（drop，日志 `benchmark/logs/phase5_ext2_guard2/`）：

| bs | ext4 read | **ext2 read** | 差距 |
|----|---:|---:|---:|
| 16K | 18.31% | **85.44%** | 4.7× |
| 64K | 24.46% | **83.83%** | 3.4× |
| 256K | 36.20% | **82.51%** | 2.3× |

**同平台、同 framekernel、同 virtio，ext2 小块读 82–85%，ext4 仅 18–36%。**

**结论修正**：之前判定"小块差在平台 framekernel per-syscall 开销（66µs），ext4 修不动"——**错**。那 66µs 不是平台共有，而是 **ext4 专属**。

**根因**：[ext4_rs `get_inode_ref`](kernel/libs/ext4_rs/src/ext4_impls/inode.rs#L219) 每次 `Block::load` **从块设备重读 inode 块（~25µs/次）**，无内存缓存；而 ext4 读路径每读 stat 多次（`read_at` 的 `type_()` Dir 检查 + atime relatime + VFS size/metadata）。ext2 把 inode 常驻内存（`InodeInner`），stat 全是内存访问 ~0。

**下一步优化（Step 3d）：ext4 inode 元数据缓存**
- 收益：小块读 18% → 可能 50–85%（向 ext2 看齐），ext2 已证明平台可行；
- 难点：失效面大——size/mtime 每次写都变，需在 write/truncate/setattr/create/unlink/rename/fallocate 失效；
- 这是 ext2 对照挖出的最大优化点，已提交存档后开做。

> 另记：ext2 **4K O_DIRECT write 会 hang**（fio layout 5.5h 零进度），ext2 1M read 本轮 layout 也 NA——成熟 ext2 的极小块/特定 direct 路径也有问题，佐证小块 direct 在此平台普遍棘手。脚本已加每-case 超时保护避免再卡。

## Step 3d：ext4 inode 元数据缓存（小块读最大优化）

**状态：** ✅ 代码 + profile + full-guard@drc=0 100%；🔵 两边 ratio 跑中（2026-06-04）

### 改动
- 新增 `inode_meta_cache: Mutex<BTreeMap<u32, SimpleInodeMeta>>` + 全局 `meta_cache_generation`；`SimpleInodeMeta` 是 `Copy`。
- `fs.stat`：先查缓存命中即返回；未命中读盘一次，**仅当读盘前后 generation 未变才插入**（防 read-vs-write TOCTOU）。
- **失效**：所有变更汇聚到单一入口 `run_journaled_ext4`（注释明确 "All create/mkdir/unlink/rmdir/rename/write/truncate paths flow through this helper"）——在其内 bump generation + 清空整个缓存，一处覆盖全部变更（含目录操作改父 inode）；唯一绕过点 nojournal setattr（`run_inode_metadata_update_with_op` else）也补了。
- 安全性：读写同 inode 由 correctness lock 串行；任何变更清空整缓存（保守但简单正确）；gen guard 关掉无锁 stat（VFS metadata/size）与写的竞争。缓存只影响**报给用户态的 stat/type/size**，不碰 ext4_rs 内部写逻辑（它仍读盘）。

### profile + 吞吐（Asterinas-only，对照之前 / ext2）

| bs | 之前 MB/s | **inode 缓存后** | 提升 | ext2 MB/s |
|----|---------:|---------------:|-----:|----------:|
| 4K | ~35 | **152** | **4.3×** | — |
| 16K | ~140 | **539** | **3.6×** | 640 |
| 64K | ~524 | **1526** | **2.9×** | 1760 |

- 每读时间 4K 117µs→27µs：那 ~90µs 的"每读多次 inode-block stat 读盘"被消除；read_direct_at 内只剩 ~25µs（主要 virtio wait 23µs，平台地板，ext2 同）。
- **ext4 小块读达 ext2 的 ~84%**（之前 ~22%）——FS 专属差距基本填平。

### 两边 ratio（drop 公平口径，中位数，日志 `benchmark/logs/phase5_inodecache_ratio/`）

| bs | 优化前 | **inode 缓存后** | 提升 |
|----|------:|---------------:|-----:|
| 4K read | 16.67% | **86.38%** | ×5.2 |
| 16K read | 18.31% | **84.42%** | ×4.6 |
| 64K read | 24.46% | **86.89%** | ×3.5 |
| 256K read | 36.20% | **94.81%** | ×2.6 |
| 1M read | 118.69% | **122.94%** | — |
| **1M write** | 63.44% | **76.31%** | +13pt |

**所有块大小读全部 84–95%——基本全线达标**；inode 缓存顺带把 write 路径（也每读 stat type/size）从 63% 拉到 76%。

### 正确性（完整守底 @drc=0，inode+extent 缓存激活，日志 `benchmark/logs/phase5_full_suite/`）
- crash matrix（2 轮 × 9 场景）：**18/18 PASS**
- host-crash fsync matrix：**4/4 PASS**
- Phase 2 concurrency（4 workers × 8 rounds，seed=78）：**7/7 PASS**（inode 缓存 gen guard 并发验证）
- phase4_good / pagecache_phase4 / phase3_base / phase6_good / jbd_phase1 / jbd_phase3_fsync_durability：**全 100%**
- → **inode 缓存正确性彻底锁死**：崩溃恢复、并发、fsync、全部 xfstests 无回退。

### 意义
ext2 对照（你提议测的）直接揭示根因（ext4 缺 inode 缓存）并催生本优化——**Phase 5 最大单项收益**。

## 变更日志

| 日期 | 改动 | 涉及文件 |
|------|------|----------|
| 2026-06-02 | 开 Phase 5 性能优化线，固化基线与 profile 盘点，索引切换 | plan/milestone 新建 + 4 处索引 |
| 2026-06-03 | Step 1：四层 profile final dump + force 旁路 + 启用读 profile；bench 转发 phase2_profile/LOG_LEVEL；probe 脚本；收割 1M/4K/16K write+read 占比表，定位读 plan 为首选优化点 | bio.rs, fs.rs, bench_linux_and_aster.sh, run_phase5_profile_probe.sh |
| 2026-06-03 | Step 2/3：实现 metadata-only extent 映射缓存（默认 on，cache-off 口径生效）；4K/16K read plan 60µs→0、命中 99.9%、~3.2×；新增随机读 job 量化元数据块缓存 headroom | fs.rs, Makefile, bench_linux_and_aster.sh, ext4_rand_read_bw/run.sh |
| 2026-06-03 | Step 3 收口：A/B ratio 4K/16K/64K read +50~60%、1M +24%；正确性 pagecache 100% + 091/130 零差异 + full-guard@drc=0 三模式 100%；runner 暴露 direct_read_cache/extent_map_cache flag | fs.rs, tools/ext4/run_phase4_part3.sh, run_phase5_regression.sh, run_phase5_ratio_ab.sh |
| 2026-06-03 | Step 3b 收口：缓存窗口 8MiB→1GiB 全文件覆盖 + extent 上限；随机读 4K/16K ×1.30~1.32、plan 74µs→0、命中 99.99%；1M seq hit→100%；full-guard@drc=0 三模式 100% | fs.rs |
| 2026-06-04 | drop_caches 设为基线（Linux 1M read 4942→2818 复现 sweep）；公平口径下 1M read 118.69% 反超、write 63.44% 为唯一 blocker；全 bs 中位数基线 | bench_linux_and_aster.sh, run_phase5_*.sh, benchmark.md |
| 2026-06-04 | Step 3c 收口：全路径 profile（atime 31µs/VFS 66µs/virtio 21µs）+ atime 按秒节流（4K/16K read +12~13%）；判定小块 85–90% 受 framekernel per-syscall 开销限制不可达 | fs.rs |
| 2026-06-04 | ext2 对照推翻"平台天花板"误判：ext2 小块读 82–85% vs ext4 18–36%，同平台 → ext4 缺 inode 内存缓存（get_inode_ref 每次读盘） | ext2 job, guard 脚本 |
| 2026-06-04 | Step 3d ⭐ ext4 inode 元数据缓存（gen 防 TOCTOU + run_journaled_ext4 单一失效点）：**小块读 16–24%→84–95%（×2.6–5.2）**，1M write 63→76%；full-guard@drc=0 三模式 100% | fs.rs |
| 2026-06-04 | 完整守底 FULL_SUITE@drc=0 全绿：crash 18/18、host-crash 4/4、concurrency 7/7、xfstests 全 100%；读优化（A/3b/3c/3d）定版 | run_phase5_regression.sh |
| 2026-06-05 | write 全 bs 重测（inode 缓存 build）：4K/16K/64K/256K/1M write = 75.5/75.8/84.1/121.1/88.3%（inode 缓存把写也拉上来）；write profile 确认 prepare(mtime)=0、above≈3–20µs，写已 virtio-bound，ext4 域内榨干 | guard_median/profile_probe |
