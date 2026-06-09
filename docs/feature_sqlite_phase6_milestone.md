# ext4 性能优化 Phase 6 Milestone 记录（SQLite 真实应用写优化主线）

首次创建时间：2026-06-09（Asia/Shanghai）

配套计划：`feature_sqlite_phase6_plan.md`
起点证据：`sqlite_benchmark_report.md`（speedtest1 报告）、`feature_perf_phase5_milestone.md`（Phase 5 收口）

## 当前状态（2026-06-09）

| Step | 内容 | 状态 |
|------|------|------|
| Step 0 | 起点固化 & SQLite profile 盘点 | ⏳ 待执行 |
| Step 1 | 定位 → 选优化点（delalloc / 批量化 / group commit）| ⏳ 待执行 |
| Step 2 | 实施优化（主线 delalloc）| ⏳ 待执行 |
| Step 3 | 全量回归 + SQLite 重测收口 | ⏳ 待执行 |

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

## 备注

- 方法论铁律（Phase 5 教训）：**先 profile 再优化**，每个优化点动手前必须有占比数支撑。
- 主线先验 = delalloc（延迟分配）；备选 = 慢路径 journaled prepare 批量化（更轻、可作中间收益/回退）。
- delalloc / group commit 触及持久化语义，必须过 crash matrix + `integrity_check`。
- HEAD（含 Phase 5 全部修复：A1 / B / 覆盖写快路径）为随时回退安全基线。
