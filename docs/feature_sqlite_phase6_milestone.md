# ext4 性能优化 Phase 6 Milestone 记录（SQLite 真实应用写优化主线）

首次创建时间：2026-06-09（Asia/Shanghai）

配套计划：`feature_sqlite_phase6_plan.md`
起点证据：`sqlite_benchmark_report.md`（speedtest1 报告）、`feature_perf_phase5_milestone.md`（Phase 5 收口）

## 当前状态（2026-06-09）

| Step | 内容 | 状态 |
|------|------|------|
| Step 0 | 起点固化 & SQLite profile 盘点（含 Step 0a 三 FS 诊断三角 ext4/ext2/ramfs）| ✅ 已完成：三角 + 四层 profile 归因表出齐（见下）|
| Step 1 | 定位 → 选优化点 | ✅ 定档：完整 delalloc（写时预留 + 写回批量分配 + 脏页节流）。记录 2a（缓存检查→死路 0.005% 命中）/ 2b-1（去检查→540s OOM，证明预留必须）两次失败教训 |
| Step 2 | 实施 delalloc，分阶段 | 🔄 进行中：Stage 0 OOM 根因诊断 → Stage 1 写时预留+延迟写（攻 41%）→ Stage 2 写回批量分配+大 bio（攻 32%/24%）→ Stage 3 调参收口。详见 plan |
| Step 3 | 全量回归 + SQLite 重测收口 | ⏳ 待执行 |

### Step 2 分阶段（详见 `feature_sqlite_phase6_plan.md` §Step 2）

| Stage | 目标 | 攻哪个桶 | 门控 |
|-------|------|---------|------|
| 0 | 只读诊断确认 OOM 根因（脏页堆积 vs journal 内存）| — | 放行判定 |
| 1 | 写时块预留 + 延迟写（去掉每写 map 检查/prepare）+ 脏页节流 | 41% + 防 OOM + ENOSPC 正确 | SQLite 不 OOM 跑完 + integrity + 守底矩阵 |
| 2 | 写回合并连续脏页：一次 journaled 大 extent 分配 + 大 bio | 32% + 24% | crash matrix + integrity |
| 3 | 调参 + 硬化 + 诚实 TOTAL 终测 | — | 守底全绿 + O_DIRECT 不回退 + ≥5% 量化 |

**安全基线**：HEAD `8394f31a6`（Step 0 instrument + 诊断，含 Phase 5 全部修复），任一阶段回退即退到此。

## Phase 6 起点基线（继承 Phase 5 收口，2026-06-07）

口径：`page_cache=1`，`sqlite-speedtest1 --size 1000 /ext4/test.db`，drop_caches 公平基线，Linux 同口径。

| 项 | 值 |
|----|----|
| SQLite TOTAL（Asterinas）| **2022s** |
| SQLite TOTAL（Linux）| 60.2s |
| ratio | **2.97%** |
| `PRAGMA integrity_check` | PASS（数据无损）|
| 读类（SELECT）| 1–4× 慢（已追平，非 Phase 6 目标）|
| 写类·追加/新分配（INSERT 新块 / CREATE INDEX / VACUUM）| 数十~244× 慢（**Phase 6 主线**）|

慢路径根因（Phase 5 已定位）：追加/新分配写每个 4KB 页跑一遍完整 journaled 分配（JBD2 handle + runtime 锁 + `ext4_map_blocks` + meta cache clear）+ 每事务 fsync + 逐 4KB bio；覆盖写已被 Phase 5 快路径绕过。

## Step 0a 三 FS 诊断三角结果（2026-06-09）

口径：`sqlite-speedtest1 --size 1000`，`page_cache=1`（ext4），**`LOG_LEVEL=error`（无 warn 日志噪声，可与 2022s 基线诚实对比）**，drop_caches 公平基线，Asterinas 与 Linux 同口径同轮。日志：`benchmark/logs/sqlite_phase6_step0a_v2/triangle/`。

| FS | Aster TOTAL | Linux TOTAL | ratio | integrity_check | 说明 |
|----|------:|------:|------:|:---:|------|
| **ext4**（journaled，现状）| **2010.7s** | 60.8s | **3.02%** | PASS | 复现 2.97% 起点（误差内）|
| **ext2**（Asterinas 原生，PageCache buffered，**无 journaling**）| **62.5s** | 59.3s | **94.91%** | PASS | 同平台几乎追平 Linux |
| **ramfs**（纯内存，无块设备）| **55.6s** | 53.3s | **95.87%** | PASS | 平台地板 |

**三角判定（决定性）：**
- **ext4 vs ext2 = 我们 ext4 journaled 写路径的净代价**：2010.7 → 62.5s，**ext4 比 ext2 慢 ~32×**，差值 ~1948s **全部**在我们 ext4 写路径里 → **delalloc 头部空间巨大、值得重仓**。
- **ext2 vs ramfs = 平台地板（virtio 往返 + PageCache 写回）**：62.5 vs 55.6s，仅 ~12%，**地板很薄**。
- **ramfs vs Linux = framekernel per-syscall**：55.6 vs 53.3s，仅 ~4%，可忽略。
- **结论**：2.97% 里几乎全部（96%+ 的损失）是「我们 ext4 每页 journaled 分配 + 每事务 commit/fsync」造成，**不是平台墙**。ext2 在**同一 Asterinas 平台**（同 virtio、同 PageCache）达 95% 证明 buffered 写回 + 批量分配（delalloc 的形状）在本平台可达 Linux 水平。
- **诚实边界**：ext2 **无 journaling**，它的 62s 含「省掉日志」的便宜，**不是 ext4 必达目标**——ext4 为崩溃一致性保留日志（优秀档功能要求），不得为提速砍日志；ext2 仅作「非日志写回天花板」+ 实现范例。

**逐写类 sub-test 三 FS 对比**（秒，越低越好；ext4=Aster journaled / ext2=Aster 无日志 / Linux）：

| # | 操作 | ext4 | ext2 | Linux | ext4 vs ext2 | 类型 |
|---|------|----:|----:|----:|----:|------|
| 180 | 500000 INSERT w/3 索引 | 341.4 | 2.50 | 2.7 | **137×** | 写·新分配 |
| 200 | VACUUM | 193.8 | 2.27 | 3.0 | **85×** | 写·重写 |
| 500 | 700000 REPLACE on TEXT PK | 163.2 | 1.90 | 2.0 | **86×** | 写 |
| 270 | 100000 DELETE indexed | 144.9 | 3.02 | 3.0 | **48×** | 写 |
| 280 | 500000 DELETE individual | 136.5 | 2.71 | 2.6 | **50×** | 写 |
| 190 | DELETE and REFILL | 133.0 | 2.53 | 2.6 | **53×** | 写 |
| 150 | CREATE INDEX ×5 | 131.8 | — | 1.0 | 大 | 写·元数据 |
| 400 | 700000 REPLACE on IPK | 127.5 | — | 1.6 | 大 | 写 |
| 120 | 500000 unordered INSERT w/PK | 84.6 | — | 1.0 | 大 | 写·新分配 |
| 240 | 500000 UPDATE individual | 56.8 | 1.64 | 1.6 | **35×** | 写·覆盖 |
| 110 | 500000 ordered INSERT w/PK | 35.2 | — | 0.4 | 大 | 写·新分配 |
| 230 | 100000 UPDATE indexed | 28.5 | 1.85 | 1.8 | **15×** | 写·覆盖 |
| 100 | 500000 INSERT no index | 8.4 | — | 0.3 | 大 | 写·新分配 |
| — | 各类 SELECT（130-170/410/510/520）合计 | ~26 | ~20 | ~20 | ~1× | 读（已追平）|

写类合计 ≈ **1586s / 2011s ≈ 79%**（读 ~26s 已追平 Linux）。**关键观察**：连 in-place UPDATE（230/240，走 Phase 5 覆盖快路径、不分配）都比 ext2 慢 15–35× → 说明慢的不只是「慢路径每页 journaled 分配」，**每事务 commit/journal-fsync 往返也是大税**（覆盖写仍每 COMMIT 一次 jbd2 commit + flush + virtio 往返）。slow-path 分配 vs commit/fsync 各占多少由 Phase B `[ext4-bufw]` / `[ext4-phase2]` 定量。

**writeback bio 形状**（Phase A `[block-profile]` 早期采样）：`avg_bytes=4095`（逐 4KB bio，无合并）、`avg_device_wait_us≈27–90`（每 4KB 一次 virtio 往返）→ 证实 plan 的「逐 4KB bio」假设，delalloc 的「批量大 extent + 大 bio」可直接消除。

### Step 0 四层 profile → SQLite TOTAL 时间归因表（Phase B，2026-06-09）

口径：`page_cache=1`，`LOG_LEVEL=warn` + `ext4fs.phase2_profile=1`，profile run TOTAL=**2057s**（vs Phase A 诚实 error 口径 2011s，差 +2.3% = warn 日志 + profile 开销，占比代表性成立）。日志：`benchmark/logs/sqlite_phase6_step0a_v2/profile/sqlite_ext4_pc1.log`，末尾 `[ext4-bufw]` / `[ext4-phase2]` / `[block-profile]` 累计快照。

**末尾累计读数：**
- `[ext4-bufw] calls=9,074,952 fast_calls=8,518,909 (93.9%) avg_fast_us=98 total_fast_ms=843,086 | slow_calls=556,043 (6.1%) slow_blocks=710,688 avg_slow_prepare_us=1193 total_slow_ms=727,016 total_slow_prepare_ms=663,705 | max_slow_prepare_us=53,772`
- `[ext4-phase2] runtime_lock_acquires=2,173,012 avg_hold_us=449 (→ 锁持有合计 ~975s) journaled_write_ops=1,010,500 avg_apply_us=886 commits_finished=5931`
- `[block-profile] write-bio avg_bytes=4095 avg_device_wait_us=28 avg_irq_delivery_us=27`（逐 4KB bio，无合并；virtio 往返 28us/bio）

**归因表（按可优化桶，时间用 2057s profile run 拆，占比对总）：**

| 桶 | 证据 | 时间 | 占比 | 主优化手段 |
|----|------|----:|----:|------------|
| **快路径覆盖**（无分配，每写仍 `ext4_map_blocks`+runtime 锁+stat）| 8.52M calls × 98us | **843s** | **41%** | **delalloc**：write() 变纯 page-cache copy，彻底去掉每写 map_blocks + 全局锁 |
| **慢路径 journaled 分配**（追加/新块 prepare）| 556K calls，710K blocks，avg 1193us | **664s** | **32%** | **delalloc**：writeback 对连续脏页一次批量分配大 extent |
| 慢路径其余（page_cache.write 等）| total_slow − prepare | 63s | 3% | （随 delalloc 一并）|
| **COMMIT / journal-commit + fsync + writeback bio** | 5931 commits；812K×4KB bio | **~487s** | **~24%** | group commit / fsync 合并；delalloc 顺带出大 bio |
| 读（SELECT 全部）| 三角实测 | ~26s | 1% | 已追平，非目标 |

**交叉校验：** write_at_page_cache 内合计 843+727=**1570s（76%）**；全局 `EXT4_RS_RUNTIME_LOCK` 持有 **~975s（47%）**（单线程 avg_wait=0，是串行工作非争用）；virtio 设备往返仅 ~数十 s（device_wait 28us × bio）→ 平台地板薄，与三角 ext2=95% 一致。

**Step 0 决定性结论（回答验收问「逐页分配 + 逐 4KB bio 占多少 / ext4 可优化 vs 平台地板」）：**
1. **~96% 的损失是 ext4 域内可优化**（每写 map_blocks+全局锁 41% + 每页 journaled 分配 32% + 每 4KB bio + commit 串行 24%），**平台地板（virtio 往返）仅数十 s**——三角 + bio profile 双证。
2. **重要修正（推翻 plan 先验权重）**：最大单桶是**快路径覆盖（41%）**，不是新分配慢路径。Phase 5 覆盖快路径虽跳过分配，但**仍每写做一次 `ext4_map_blocks` 全 extent 树遍历 + 持全局锁**（8.52M 次 × 98us = 843s）。这正是「连 in-place UPDATE 都比 ext2 慢 15–35×」的根因（ext2 buffered write = 纯 page-cache copy，无每写 map+锁）。
3. **delalloc 是正解且覆盖面比预想更大**：真正的 delalloc 让 write() 只做「拷进 page cache + 标记 delayed」**不碰 ext4_map_blocks / 不取全局锁 / 不分配**，分配与映射全推迟到 writeback 批量做（大 extent + 大 bio）。它**同时**吃掉快路径 41% + 慢路径 32% ≈ **73%**，外加大 bio 改善 24% 桶的一部分。
4. **group commit / fsync 合并** 处理剩余 ~24% 的 COMMIT 串行（5931 commits）——单线程 speedtest1 每 COMMIT 必 fsync，delalloc 不减次数只减每次工作量，fsync 合并是独立的次级杠杆（触持久化语义，需 crash matrix 严格门控）。

→ **Step 1 定档：主线 delalloc（write 路径纯页缓存化 + writeback 批量分配/大 bio），次线 commit/fsync 批量化。** 与学长对齐项：delalloc 改写时机 / ENOSPC 预留语义、group commit 改 fsync 时序——动手前过一遍。

## 守底基线（Phase 6 不能回退）

| 测试项 | Phase 5 收口结果 | Phase 6 要求 |
|--------|------------------|--------------|
| `phase3_base_guard` | PASS | 不回退 |
| `phase4_good` | PASS | 不回退 |
| `phase6_good` | `25/25 PASS` | 不回退 |
| `jbd_phase1` | 有效样本 100% | 不回退 |
| JBD2 crash matrix | `18/18 PASS` | 不回退 |
| Phase 2 concurrency（自研 `phase2_concurrency.c`）| `7/7 PASS` | 不回退 |
| `concurrency` xfstests 套件 | `10/10 PASS` | 不回退 |
| `jbd_phase3_fsync_flush` | 0 FAIL | 不回退 |
| Phase 3 host-crash fsync matrix | `4/4 PASS` | 不回退 |
| `pagecache_phase4` | `FAIL=0` | 不回退 |
| fio O_DIRECT 守底（cache-off, nj=1）| read 86–123% / write 76–121% | 不回退 |
| SQLite `integrity_check` | PASS | 不回退 |

## 变更日志

| 日期 | Step | 改动概要 | 涉及文件 | 性能结果 | 守底 | commit |
|------|------|----------|----------|----------|------|--------|
| 2026-06-09 | — | Phase 6 立项：plan + milestone + AGENTS/CLAUDE/索引同步，建分支 | `feature_sqlite_phase6_*.md`、`docs/*`、`AGENTS.md`、`CLAUDE.md` | — | — | （建分支提交）|
| 2026-06-09 | Step 0 | 加只读 buffered-write profile（`[ext4-bufw]` fast/slow 分流，门控 `phase2_profile`，默认关）；泛化 `run_sqlite_summary.sh`（`FS_LIST` + `EXT4_PHASE2_PROFILE` 透传）；跑 Step 0a 三 FS 三角 + 四层 profile，产出 TOTAL 归因表 | `kernel/src/fs/ext4/fs.rs`、`test/initramfs/src/benchmark/sqlite/run_sqlite_summary.sh` | ext4 3.02% / ext2 94.91% / ramfs 95.87%；归因：快路径覆盖 41% + 慢路径分配 32% + commit/fsync 24%，平台地板仅数十 s | 未跑（纯诊断 + 只读 instrument，无行为改动；守底待 Step 2 改动后跑）| 待提交 |

## 备注

- 方法论铁律（Phase 5 教训）：**先 profile 再优化**，每个优化点动手前必须有占比数支撑。
- **Step 0a 三 FS 诊断三角**（harness 已存在：`sqlite/{ext4_speedtest1,ext2_benchmarks,ramfs_benchmarks}`）：ext4 vs ext2 = 我们 journaling/每页分配净代价（可攻）；ext2 vs ramfs = 平台地板（改不动）；ramfs vs Linux = framekernel syscall 开销。**ext2 无日志，是"非日志写回天花板"+ 实现范例，非"ext4 必达目标"——不得为提速砍日志（优秀档功能要求）**。详见 plan §Step 0a。
- 主线先验 = delalloc（延迟分配）；备选 = 慢路径 journaled prepare 批量化（更轻、可作中间收益/回退）。
- delalloc / group commit 触及持久化语义，必须过 crash matrix + `integrity_check`。
- HEAD（含 Phase 5 全部修复：A1 / B / 覆盖写快路径）为随时回退安全基线。
