# ext4 性能优化 Phase 6 Milestone 记录（SQLite 真实应用写优化主线）

首次创建时间：2026-06-09（Asia/Shanghai）

配套计划：`feature_sqlite_phase6_plan.md`
起点证据：`sqlite_benchmark_report.md`（speedtest1 报告）、`feature_perf_phase5_milestone.md`（Phase 5 收口）

## 当前状态（2026-06-11 收口）

**性能线最终战绩：SQLite speedtest1 2010.7s → 234.9s（ratio 2.97% → 21.92%，7.4×），integrity 全程 PASS，守底全绿，HEAD 干净（bc883375a）。** 路线、上限理论依据与 delalloc 解锁链见 plan §P 系列。

| 阶段 | 内容 | 状态 | SQLite |
|------|------|------|-------:|
| Step 0/0a | 起点固化 + 三 FS 三角 + 四层 profile 归因 | ✅ | 2010.7s（2.97%）|
| S 系列 | S3 fsync 保留 clean 页（−7%）→ S4 批量写回（−5%）→ S6 unwritten extent + 写时预分配（−25%）| ✅ | 1332.2s（3.86%）|
| P 系列 | P1 设备块缓存（−66%）→ P2 写快路径 ext2 化（−46%）→ P3b journal 合并写（中性）→ P5a lean prepare（−4%）；P3-1/P4 实测出局（负结果已记录）| ✅ | **234.9s（21.92%）** |
| 收敛评估 | 曲线趋平（−66→−46→−4%）；类别内现实可达 ~24-27%（P5b 脏页索引 ~12-15s 为最后机械项）；90% 需 delalloc 解锁链（赛期外，见 plan）| 已与理论对齐 | — |
| 并行（待执行）| revoke 正确性修复（F1，保答辩；S6/P2 块复用加大暴露面）；476/388 压力垃圾节点共同根因追踪（现有防御已保证降级 EIO 不 panic）| ⏳ | — |

### Step 2 分阶段（详见 `feature_sqlite_phase6_plan.md` §Step 2）

| Stage | 目标 | 攻哪个桶 | 门控 |
|-------|------|---------|------|
| 0 | 只读诊断确认 OOM 根因（脏页堆积 vs journal 内存）| — | 放行判定 |
| 1 | 写时块预留 + 延迟写（去掉每写 map 检查/prepare）+ 脏页节流 | 41% + 防 OOM + ENOSPC 正确 | SQLite 不 OOM 跑完 + integrity + 守底矩阵 |
| 2 | 写回合并连续脏页：一次 journaled 大 extent 分配 + 大 bio | 32% + 24% | crash matrix + integrity |
| 3 | 调参 + 硬化 + 诚实 TOTAL 终测 | — | 守底全绿 + O_DIRECT 不回退 + ≥5% 量化 |

**安全基线**：HEAD `8394f31a6`（Step 0 instrument + 诊断，含 Phase 5 全部修复），任一阶段回退即退到此。

### Stage 0 结果（OOM 根因，2026-06-09）

口径：临时复现 2b-1（去 size 内 map 检查）+ 只读 instrument（`[ext4-phase2]` 增 `checkpoint_depth` 与 `bufw_dirty_backlog_kb`），跑到 OOM（guest 537s）。日志 `benchmark/logs/sqlite_phase6_stage0_oom/`。

| 指标 | 趋势到 OOM | 结论 |
|------|-----------|------|
| `checkpoint_depth`（日志内存）| **有界**，0–58 抖动、多在 <15 | Phase 5 的 `JOURNAL_CHECKPOINT_MAX_DEPTH` 生效，**非 OOM 原因** |
| `bufw_dirty_backlog_kb`（页缓存脏数据）| **无界**，涨到 ~9.6 GB（VM 仅 8 GB）→ 堆耗尽 | **OOM 真因** |

**定论**：OOM 是**页缓存脏页超过内存**，不是日志内存、也不是磁盘满（free_blocks ~16k，充足；早先「磁盘满」是我误读截断行）。机理：去掉每写检查后写变快 → 写入(生产)速度超过写回(消费,仅 fsync 时 drain) → 脏页堆积。原检查的慢充当了隐式背压。

**对 Stage 1 的指导（已确认）**：① **脏页节流**（脏量超阈值即强制写回，相当于 Linux `balance_dirty_pages`）是 OOM 的**必需修复**；② **块预留**独立解决 ENOSPC。两者都在 plan Stage 1 内。诊断 instrument（`writeback_bytes` + phase2 两字段，只读、`phase2_profile` 门控）保留，用于 Stage 1 观察节流是否生效。
- 注：backlog 指标对「同页多次改写」会重复计数 → 实际脏内存 ≤ 9.6 GB；但「无界增长且超 RAM」+「日志有界」这一对比是定论依据。

### Stage 1a 结果（2026-06-10）：节流防住 OOM，但**「写途中刷回」损坏数据** —— delalloc 撞平台墙

实现：去掉每写映射检查（41% 提速）+ 脏页节流（脏量超 256MB 即强制写回当前 inode）。结果：

- ✅ **节流防住 OOM**：脏页内存压在 ~261MB（vs 9.6GB），不再 OOM。内存可控。
- ❌ **数据损坏**：~64s 时 SQLite 报 `database disk image is malformed`。

**根因调查（隔离实验，逐步缩小）：**
1. **2b-1**（只延迟、只 fsync 写回）→ 不损坏（跑到 OOM）；**Stage 1a**（延迟 + 途中节流写回）→ 损坏 → 罪在「途中写回」。
2. **隔离实验 A**（**基线写路径** + 每 4096 写强制 `evict_all`）→ **也损坏**（77s）→ 与 delalloc **无关**，是「途中写回」本身的问题。
3. 代码定位：`evict_all` = 写回 + **`decommit_vmo_range`（释放整文件页帧）**。途中调用会把 SQLite 正在用的页释放掉 → 重读从磁盘 → 损坏。fsync 时安全（静默边界）。
4. **隔离实验 B**（基线 + 途中写回，**改成只写回不 decommit**）→ **仍损坏**（182s）→ 不只是 decommit；**「途中调用写回路径」本身就损坏**（写回路径在非 fsync 点被驱动时不安全）。`run_journaled_ext4` 锁/operation scope 是顺序、干净的（非嵌套 bug）。

**定论（平台级阻塞）**：完整 delalloc 的脏页节流**必须**途中写回来控内存，但 **Asterinas ext4 的「途中写回」会损坏数据**（与 delalloc 无关、与 decommit 无关，是 fs 层在非 fsync 点驱动写回的深层 bug/限制）。Linux delalloc 靠**后台 flusher 线程**在安全点写回，Asterinas **没有这套基础设施**。→ 完整 delalloc 在 Phase 6 现有范围内被此平台限制阻塞。已全部回退，HEAD = `0d86d3ede`（干净）。**精确根因（exact line）未钉死**，需更侵入式调试（写回处 dump 页内容对比）。

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
| 2026-06-10 | S3 | fsync 保留 clean 页：新增 `flush_all`（`page_cache.evict_range` 写回+标 clean，**不 decommit**），`sync_page_cache_for_inode_locked` 改用之；`evict_all`（含 decommit）仍守 drop inode / truncate / O_DIRECT。已核实 O_DIRECT 自带 evict+discard（fs.rs:5047/5173）不依赖 fsync decommit，`page_cache.evict_range` 本身保留页（page_cache.rs:354 仅标 UpToDate）→ 一致性 by construction 安全 | `kernel/src/fs/ext4/fs.rs` | SQLite ext4 **2010.7→1868.2s（−7%）**，integrity PASS（Linux 同轮 60.8→51.1s 有 host 噪声，单跑）| **pagecache_phase4 coherency 9 PASS/0 FAIL/4 NOTRUN（含 dio-vs-buffered 247/263）；crash/fio 逻辑不受影响（盘上状态与 evict_all 逐字节相同，O_DIRECT 路径未碰）** | 已提交 730ddf30a |
| 2026-06-11 | P3b | **journal commit 块合并大写**：`jbd2/device.rs` 新增 `write_blocks_coalesced`（按"逻辑 journal 序 + 物理盘位双连续"分 run，每 run 一次大写=一个 bio；mkfs journal 通常全连续 → 整个 commit 的 descriptor+N 元数据块一次写完，替代 N+1 次同步 virtio 往返）；`mod.rs write_commit_plan_with_hook` 接入，**sync 先于 commit 块的顺序与 crash-inject hook 阶段语义不变**。SQLite **中性**（243.97 vs P2 243.9）——证实 commit 桶（~90s）大头不在 journal 块写（每 commit 平均块数少、5131 次合计仅数秒），而在 writeback/flush/串行等待；改动保留理由 = 减少每 commit virtio 往返（大事务/高并发受益）+ 无回退 | `kernel/libs/ext4_rs/src/ext4_impls/jbd2/{device,mod}.rs` | SQLite 243.97s（持平）| crash 18/18（重跑，首轮端口 flake）+ jbd_phase1 + fsync_flush + host-crash 4/4 全绿（replay 经合并写验证）| 待提交 |
| 2026-06-11 | P3-1（已回退）| **size-only append 实验：净负，整体回退，patch 留存**。设计：append 写时不转换不分配不 zero——A1（覆盖且扩 size）只 journal size+times、A2（覆盖且 size 内含 unwritten）纯页缓存写、unwritten→written 转换全推迟到 writeback（崩溃暴露 unwritten 读零，语义等价 Linux delalloc）、写回整块覆盖跳零填、coverage 升级 allocated 语义。**三轮实测均净负**：①首版 462.9s——慢路径 invalidate-on-slow 触发 GB 级文件全量 populate 走树风暴；②修为有界尾段补全后 312.8s（vs P2 243.9 仍 +28%）——profile 显示 journal 侧确实省 23s（558K ops avg 194us vs 229us），但 slow 调用非 journal 开销 +69s 且**纯读测试也回退 30%**（blkcache 读 +7M 次），全局性病灶未钉死；③二分禁 A1 反而 468.0s——证明 A1 的 append 改善真实（INSERT 类 −35%），病灶在 A2/延迟转换/coverage 公共部分。**正确性全程未破**（crash 18/18×2、fsync_flush 过）。按铁律回退（净负不留），patch 在 `benchmark/logs/p3_1_size_only_append_attempt_20260611.patch`。**教训**：①写时转换的成本被 run_journaled 框架（每 op 固定 ~40us + handle 簿记）部分掩盖，省 prepare 不等于省 op；②读回退 30% 是未解之谜（疑树/页状态全局变化），重启此线前必须先加细分 instrumentation（writeback 转换计时、populate/extend 计数、find_extent 计数）钉死再动手；③coverage invalidate-on-slow 在大文件上是 O(extents) 风暴，任何"失效后重建"设计必须有界 | （已回退，无代码变更）| SQLite 三轮：462.9 / 312.8 / 468.0s，全部 > P2 243.9s | crash 18/18×2、fsync_flush PASS（正确性未破）| 不提交 |
| 2026-06-11 | P2 | **write() 快路径 ext2 化（90% 路线第二步）+ 388 panic 修复**：①`WrittenCoverage` per-inode 内存 written-extent 区间集（合并保持极大/不相邻 → 覆盖判定 = 单次 BTreeMap 前驱查询）。**不变量 coverage ⊆ truth**：只由 `coverage_populate`（inode 锁下整文件权威 map_blocks 走树，>4096 区间标 TooFragmented 回退）创建、只用 prepare 成功返回的 mappings 扩展（buffered 慢路径 + O_DIRECT prepare 两处）；移除映射的路径全部失效——Truncate{ino} chokepoint + `clear_inode_touch_cache`（unlink/rmdir/rename-overwrite；已核实 ext4_rs unlink 不释放数据块，释放只走 truncate_inode）；mmap 写回等未跟踪新增仅使覆盖悲观、永不误判 ②快路径 `VmReader` 直写 page cache（去 Vec 双拷贝，ext2 同形；慢路径保留先拷贝语义）③meta cache per-ino 失效（Write/InodeMetadata/Truncate 单 ino remove，目录 op 保守 clear-all）——消掉 SQLite journal 文件写与 DB stat 交错的 miss 风暴。**捎带修复 S6 潜伏严重 bug（被 P2 提速暴露）**：`jbd_phase3_fsync_flush` 的 generic/388（fsstress+shutdown 循环、满盘 ENOSPC）触发内核 panic——S6 尾段插入失败处理把**整个 tail_part 释放**，但 `insert_unwritten_blocks_as_extents` 内部按段循环、部分插入后失败时已插入 extent 仍引用前缀块 → 被引用块回到分配器 → 重分配覆写 → 树叶变垃圾（`ex.pblock=12TB` 无限循环 warn 后 slice OOB）。修复 = `inserted_out` 跟踪进度、**只释放未插入后缀** + `ext_remove_leaf` 防御性 entries_count 钳制（损坏 fs 降级为错误路径而非 panic）| `kernel/src/fs/ext4/fs.rs`（+249）、`kernel/libs/ext4_rs/src/ext4_impls/{file,extents}.rs`（+80/−32 修复）| **SQLite ext4 454.3→243.9s（P2 单步 −46%；自起点 2010.7s 累计 −88%），ratio 11.26%→20.88%**，integrity PASS。fast path avg 23→约 4–6us（见 profile run 273s）| **全绿**：crash matrix 18/18×2 轮（实现后+修复后）；phase6_with_guard 四包 + jbd_phase1 + concurrency 10/10 + host-crash 4/4 PASS；**fsync_flush 修复后连跑 2 轮全过**（修复前 1 panic）；phase2 concurrency 7/7（重跑，上轮端口碰撞 flake）；fio O_DIRECT write 75.02%（贴线 ≥75%，建议 P3 前复跑确认非趋势）/read 91.79% | 待提交 |
| 2026-06-11 | P1 | **adapter 层设备块缓存（90% 路线第一步，见 `feature_sqlite_phase6_90pct_roadmap.md`）**：S6 后 profile 钉死单一最大瓶颈——写 33GB 的负载从设备读 166GB（39.2M 次 4KB 读 ≈705s 设备等待），根因 = ext4_rs 全程零元数据缓存（每次 `get_inode_ref`/`find_extent`/balloc 都是同步 virtio 往返）+ bridge 读"先无条件设备读再 overlay 打补丁"。实现 `DeviceBlockCache`：挂在 `KernelBlockDeviceAdapter`（最底层、JBD2 overlay 之下）做设备 home 内容的 **write-through 镜像**（8192 块 =32MB LRU；读=整块对齐命中免设备 I/O，overlay 照常打补丁；写=整块更新 `update_if_present`/部分块失效/失败失效；checkpoint home 写与 recovery 重放天然经 adapter 写穿透）→ 一致性推理局部化到 adapter，唯一旁路 O_DIRECT 数据 bio 在 `submit_direct_write_mappings` 完成后显式失效。ext4_rs 唯一改动：`get_inode_ref` 改对齐块读（同 `write_inode_image` 形状，含跨块防护回退），使其可被块缓存服务。`[ext4-blkcache]` 计数器入 `dump_perf_summary`。mount 每次新建 adapter（`FsType::create`→`Ext4Fs::open`），mkfs/remount 循环天然空缓存 | `kernel/src/fs/ext4/fs.rs`（DeviceBlockCache + 读写接入 + O_DIRECT 失效 + dump）、`kernel/libs/ext4_rs/src/ext4_impls/inode.rs`（get_inode_ref 对齐读）| **SQLite ext4 1332.2→454.3s（P1 单步 −66%；自起点 2010.7s 累计 −77%），ratio 3.86%→11.26%**，integrity PASS（speedtest1 内嵌 980 正常完成）。profile：**blkcache 命中率 98.5%**（hits=38.58M misses=596K resident=7418/8192）；fast path avg 93→23us（798.6→200.5s）、slow prepare 552→216us（362.6→161.5s）。新桶（489s profile 口径）：fast 200.5s/41% + slow 161.5s/33% + commit/fsync ~100s/20% | **全绿**：crash matrix 18/18（第三跑；前两次为宿主端口随机碰撞 flake，分别在 verify VM 未启动/prepare VM 未启动，非缓存问题）；phase6_with_guard 四包 + jbd_phase1 + phase2 concurrency 7/7 + concurrency 10/10（5400s 预算）+ host-crash 4/4 + fsync_flush 100% 全 PASS；fio O_DIRECT write 79.08%（≥75% 红线）/read 98.56%（O_DIRECT 读旁路缓存，仅写侧加失效）| 待提交 |
| 2026-06-10 | S6 | **unwritten extent 支持 + 写时预分配**：ext4_rs 实现 unwritten extent 语义（此前 file.rs:11 明确不支持）——①读语义：`get_pblock_idx_state` 暴露 unwritten 位；`read_at`/`collect_block_ranges`（→ map_blocks/plan_direct_read）把 unwritten 当洞→**读返回零**；内核层 fs.rs **零改动**（`write_range_fully_mapped`/O_DIRECT overwrite-cached 因覆盖检查失败自动落回 journaled 慢路径转换，by construction 安全）②写转换：`convert_unwritten_span`（extents.rs）单 extent 内转换，左邻 written 物理连续时走**合并快路径**（append 树不碎：200 次 append 仅 2 个 extent），否则原地改+插剩余片；原地编辑永不改 first_block（免祖先索引更新）；全部走 journaled metadata writer，崩溃原子性继承 JBD2 handle ③预分配：洞分配运行拆两段——写覆盖段 written + 越界尾段 **unwritten**，`SMALL_WRITE_PREALLOC_BLOCKS=1→32`；尾段插入失败即释放 ④zero-fill：write_at/prepare_write_at/allocate_range 把 unwritten 块纳入零填充列表（与洞同等崩溃窗口防御：元数据先于数据 commit 也只暴露零）。**顺手修 3 个真 bug**：truncate 保留 unwritten 头部丢标志（extents.rs:1194）、`get_last_extent` 直接用含标志位的 block_count（inode.rs:694）、defs `last_block`/`set_last_block` 同类。**顶级模型（opus）定点审计**：主路径崩溃安全判定 SOUND（zero-fill 直写在 journaled commit 前落盘，与既有洞分配同构）；2 个发现已修——can_merge unwritten 对合并到 32768 会静默丢标志暴露垃圾（major 潜伏，加 ≤32767 上限）、truncate 尾块对 unwritten 做 RMW 持久化垃圾（minor，跳过）| `kernel/libs/ext4_rs/src/ext4_defs/extents.rs`、`ext4_impls/{extents,file,inode}.rs`（+459/−42）、新 `src/bin/unwritten_probe.rs`（宿主机安全实验 harness）| 宿主机实验全过：unwritten_probe（200 append→[0,200) written+[200,224) Uninit 仅 2 extent；sparse 三路拆分；unwritten 读零）+ **e2fsck -fn 完全干净（exit 0）** + debugfs 验 Uninit 标志与 cat 读零；单测 29/29。**SQLite ext4 1772.4→1332.2s（S6 单步 −25%；自起点 2010.7s 累计 −34%），ratio 2.97%→3.86%，integrity PASS**；逐项：CREATE INDEX 131.8→39.2s（×3.4）、ordered INSERT 35.2→9.6s（×3.7）、INSERT w/3idx 341.4→143.3s（×2.4）、unordered INSERT 84.6→43.5s、VACUUM 193.8→130.6s、REPLACE TEXT PK 163.2→78.1s（均 vs Step 0a 2010s 基线）| **全绿**：crash matrix 18/18（含编译门）；phase6_good 25 / pagecache_phase4 13 / phase4_good 17 / phase3_base 16 全 rc=0；jbd_phase1 PASS；phase2 concurrency 7/7；concurrency xfstests 10/10（含 generic/269 近满盘 fsstress；注意整体预算需 `XFSTESTS_RUN_TIMEOUT_SEC=5400`，默认 1800s 不够——疑似 prealloc 在近满盘下增加 balloc 工作量，correctness 无回退，低剩余空间跳过 prealloc 列为后续调优）；host-crash 4/4；jbd_phase3_fsync_flush 100%；fio O_DIRECT cache-off write 82.32%/read 109.92%（Aster write 绝对值 2692≥基线 2659 MB/s，ratio 波动来自 Linux 侧，无回退）| 待提交 |
| 2026-06-10 | S4 | fsync 安全点批量写回：`PageCacheBackend` trait 加 `write_pages_async`（默认逐页 = **ext2 等逐字节不变**）；shared `evict_range` 按连续脏页 run 分组；ext4 override = 按 run 收数据（保 C4 clamp）→ 一次 `write_page_cache_data_at`（一次 map_blocks + 一次 revoke + **每 run 一个 JBD2 handle** + `write_at` 合并连续物理块成大 bio）。**稳健版保留 handle**（不做免-handle 崩溃安全风险点）| `kernel/src/fs/utils/page_cache.rs`、`kernel/src/fs/ext4/fs.rs` | SQLite ext4 **1868.2→1772.4s（−5%，累计 2010→1772 −12%）**，integrity PASS（Linux 51.1→50.6s 稳定，对比干净）| coherency 9/0/4（含 247/263，ext2 也走）+ **broad 守底全绿**（crash matrix 全 PASS / host-crash rc=0 / concurrency 7/7 / 全 xfstests 100%）| 已提交 334e97329 |

## 阶段性结论（2026-06-11，S3+S4+S6 后）

**累计**：S3（−7%）+ S4（−5%）+ **S6（−25%）** = **2010.7→1332.2s（累计 −34%），ratio 2.97%→3.86%，integrity 全程 PASS，完整守底全绿**。

**S6 是 Linux 对齐真功能**（unwritten extent 语义 + 写时预分配），不只是调参：ext4_rs 此前完全不支持 unwritten（file.rs:11），现在读返零/写转换/map 处理齐备，e2fsck/debugfs 互操作验证通过，crash matrix + host-crash 全过，opus 定点审计判定主路径崩溃安全 SOUND。新分配类写如归因预期拿到大头（CREATE INDEX ×3.4、ordered INSERT ×3.7、INSERT w/3idx ×2.4）。

**剩余两块**（S6 后 1332s 的構成：剩余大头是 41% 桶的每写 map_blocks 树块重读 + 24% 桶的 commit/fsync 串行）：
| 块 | 攻 | 为何是基建 | 估时 |
|----|----|-----------|------|
| **S5b 树块 buffer cache** | 41% 桶（每写 map_blocks 树块重读，现最大单桶；fast-path 覆盖写 + S6 转换路径都受益）| 新缓存层（按 pblock 缓存 extent 树块，防 2a 碎片悬崖）；失效复用 chokepoint | ~1 周 |
| **revoke（C1+C2）** | 不涨分，保答辩正确性 | JBD2 commit 写 revoke 块 + recovery 序列号过滤（两者必须一起）；**S6 块复用节奏加大暴露面，优先级上调** | 3–5 天 |

S6 调优备忘：①低剩余空间跳过 prealloc 尾段（generic/269 近满盘预算变紧的对症项）②SMALL_WRITE_PREALLOC_BLOCKS 32→64 需重新过 ENOSPC/守底③多块写（WRITE_PREALLOC_BLOCKS=1）尚未吃到预分配。

## 备注

- 方法论铁律（Phase 5 教训）：**先 profile 再优化**，每个优化点动手前必须有占比数支撑。
- **Step 0a 三 FS 诊断三角**（harness 已存在：`sqlite/{ext4_speedtest1,ext2_benchmarks,ramfs_benchmarks}`）：ext4 vs ext2 = 我们 journaling/每页分配净代价（可攻）；ext2 vs ramfs = 平台地板（改不动）；ramfs vs Linux = framekernel syscall 开销。**ext2 无日志，是"非日志写回天花板"+ 实现范例，非"ext4 必达目标"——不得为提速砍日志（优秀档功能要求）**。详见 plan §Step 0a。
- **主线（2026-06-10 重定档）= fsync 安全点 + 缓存层 + 写时预分配**（S1→S6），不引入任何"非 fsync 点的途中写回"（Stage 1a 撞死的墙）。delalloc 降 S7，被"途中写回损坏"平台限制阻塞，仅当未来修好该 bug 或加后台 flusher 才解锁。
- **铁律升级（Stage 1a 教训）**：动写回路径前先钉死正确性不变量（S1 断言）；所有写回只落在 fsync 安全点（静默边界，已证安全）或 write() 时（不延迟）。
- **S3 实测教训（2026-06-10）**：review/technical_report.md §6.2 预测 P1（删 decommit）是"最大单点、降到 600–1000s"，**实测只 −7%（2010→1868s）**。原因：profile 显示大头是 41% 每写 `ext4_map_blocks` 树块重读（S3 不碰），decommit→refill 只是小头。**再次印证：信实测 profile 胜过 reviewer 假设。** 真正大头在 S5（缓存 map_blocks，41%）/ S6（预分配，32%）。S3 仍留（correctness-safe、>5% 阈值、S4 前置、移除 refill 以免遮蔽后续增益），但其全部收益可能要 S5 去掉 41% 后才解蔽。
- **S5/S6 可行性调查（2026-06-10，只读）**：
  - **S5 继承 2a 碎片悬崖**：现成 `inode_extent_map_cache`（fs.rs:3345）是"1GB 窗口一条覆盖整文件"的对粒度缓存，但有 `MAX_CACHED_EXTENTS=16384` 上限；2a 正是栽在"碎片文件超上限→每 miss 重走整文件→灾难回退"。SQLite DB 经 INSERT/DELETE/VACUUM 会碎片化 → S5a（直接接 extent_map_cache）不安全。稳健解 = **S5b 按 pblock 缓存 extent 树块**（树块即使碎片也几百个、miss 只付单次树块读），但是新缓存层、工程量更大 → 放最后慎做。
  - **S6 不是改常量那么简单**：SQLite 多为 1 块写走 `SMALL_WRITE_PREALLOC_BLOCKS`；改大后 1 块写遇洞会 prealloc 32–64 块，这些"已分配未写"块若落 i_size 内（sparse 写涨 i_size 过它们）会**暴露磁盘垃圾**（ext4_rs 可能无 unwritten extent）。安全版 = **仅顺序追加、仅 prealloc 超 i_size** + e2fsck 实验确认无告警/无暴露。
  - **结论**：S5、S6 都需额外设计/实验 → 印证 report 的风险优先排序 P1→P2→P3→P4 正确。**先做安全的 fsync 安全点 S4，再啃 S5b/S6。**
- **S4 设计（fsync 安全点批量写回，下一步）**：给 `PageCacheBackend` trait 加 `write_pages_async`（默认逐页 = ext2 零影响），ext4 override = 连续脏页 run 一次 `ext4_map_blocks` + **已映射 run 免 JBD2 handle**（report P2：ordered 语义下纯数据写回不需 handle，现每页一 handle 是纯浪费）+ 一个大 bio；shared `page_cache.rs::evict_range` 按 run 分组。必保 C4 clamp 语义（`write_page_async` fs.rs:1158/1162）。⚠️ Phase 5 那次批量化只拿 3% = 只合并 bio 没干掉逐页 handle——本次必须干掉 handle。触碰 ext2 共享路径，默认实现必须保 ext2 行为不变。
- delalloc / group commit / S3 / S4 触及持久化语义或页生命周期，必须过 crash matrix + `integrity_check` + buffered/direct coherency + mmap 守底。
- HEAD（含 Phase 5 全部修复：A1 / B / 覆盖写快路径 + Step 0 instrument）为随时回退安全基线。

## P5a 收口补记（2026-06-11 晚）：476 判定完成，P5a 提交

generic/476 panic 判定：干净 HEAD concurrency ×2 + P5a 恢复后 ×2 = 4 轮全过（原始仅 1 次）→ **低频潜伏 bug（fsstress 随机序列依赖），与 P5a 无因果**。处置：给 `insert_extent` 入口加防御钳制（entries_count > 节点物理容量 → log error + EIO，替代 shift 循环越界 panic；与 388 的 ext_remove_leaf 钳制同族），宿主机全绿后随 P5a 提交。476/388 两处"压力下垃圾节点"的**共同根因仍未钉死**（疑 fsstress 满盘 ENOSPC 与 extent 操作部分失败的窗口），列为 hardening 跟踪项——现防御已保证不 panic、降级 EIO。

| 日期 | Step | 改动概要 | 涉及文件 | 性能结果 | 守底 | commit |
|------|------|----------|----------|----------|------|--------|
| 2026-06-11 | P5a | **lean append prepare + insert 防御钳制 + crash runner 端口修复**：①`prepare_write_at` 全已分配（written/unwritten）写的单遍探测捷径——记录 pblock+状态 → 按需 `convert_unwritten_span`（物理映射不变，记录仍有效）→ 零填转换块 → size 更新 → 探测记录直接构造 mappings；跳过 3 遍树查 + `initial_write_alloc_bgid`；洞回落通用路径，**零语义变化** ②`insert_extent` 损坏节点防御（476 panic 降级 EIO）③crash runner 场景间 sleep 2（osdk 按秒选随机端口、kill 后 1s 内重启撞垂死 VM 监听——当日 5 次 flake 根治，修复后 crash 一把过）| `kernel/libs/ext4_rs/src/ext4_impls/{file,extents}.rs`、`tools/ext4/run_phase4_part3.sh` | **SQLite 243.9→234.9s（−4%，ratio 21.92%；自起点累计 −88.3%）**。预期 50-70s 实得 9s 的偏差归因：P1 块缓存后被跳过的树查已是 us 级；slow 桶剩余 ~240us/op 为**结构性成本**（convert+零填设备写+handle 框架+overlay 记录）| **全绿**：crash 18/18、fsync_flush（388 过）、phase6_with_guard 四包、jbd_phase1、phase2 concurrency 7/7、concurrency 10/10×4 轮（476 判定）、host-crash 4/4、fio write 83.0%/read 109.0% | （见 git log）|

## 进行中状态快照（2026-06-11 晚，下一会话从此继续）

**P5a lean append prepare：实现完成、提交被 476 panic 阻塞。** 工作树未提交改动：①`kernel/libs/ext4_rs/src/ext4_impls/file.rs` prepare_write_at 单遍探测捷径（全已分配写：记录 pblock+unwritten → convert → 零填 → size → 直接构造 mappings；洞回落通用路径，零语义变化）②`tools/ext4/run_phase4_part3.sh` crash 场景间 sleep 2（根治 osdk 按秒选端口的背靠背碰撞 flake，今日 5 次）③fs.rs+inode.rs 的 [ext4-fsync] instrumentation 已提交（d17f01e5a）。

P5a 已绿：SQLite **243.9→234.9s（21.92%）**（−9s，低于预期 50-70s——P1 后被跳过的树查已是 us 级，slow 剩余 ~240us/op 是结构性：convert+零填设备写+handle 框架+overlay 记录）；crash 18/18（sleep 修复后一把过）；fsync_flush；phase6_with_guard 四包；jbd_phase1；phase2 concurrency 7/7；host-crash 4/4；fio write 83.0%/read 109.0%。

**阻塞项：concurrency 套件 generic/476（fsstress 长压测）内核 panic**：`insert_new_extent ← insert_extent ← ensure_write_range_mapped ← allocate_range ← ext4_zero_range ← fallocate`（panic_fmt，疑 root/leaf 插入处 index 越界，与 388 的"垃圾节点"同类）。日志 `benchmark/logs/concurrency_20260611_091517.log`（panic 栈在 124263 行附近，前文 generic/013/068/076/083 全 rc=0）。**与 P5a 大概率无直接因果**（P5a 不触 allocate_range/insert 路径）但需查清：①先在 P2 HEAD（42d8950c3 去掉工作树改动）重跑 concurrency ×2 判定是否预存潜伏 ②若预存：按 388 方法论查 insert_new_extent 的越界条件（root 满+grow+重插？unwritten 链路？fallocate zero_range 特有形状？）+ 加防御 ③修后 P5a 一并提交。

**P4 已出局**（fsync 桶实测仅 28.6s，instrumentation d17f01e5a），**P5b 脏页索引（~12-15s）为最后机械项**。曲线趋平：P1 −66% / P2 −46% / P5a −4%。诚实评估渐近线在 ~25-30% 附近（每 append 结构性 journaled 工作不可在"写时转换+无后台写回"约束下消除），与 90% 目标的差距应以"三角归因+守底全绿+方法论叙事"作答辩口径，建议与学长对齐。
