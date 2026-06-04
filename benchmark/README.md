# benchmark 目录说明

该目录用于集中存放可复现的 benchmark 资产，便于团队共享与仓库追踪。

## 目录内容

- `benchmark.md`：当前测试结果汇总。
- `environment.md`：当前环境记录。
- `assets/`：运行测试所需依赖资产（initramfs、xfstests、vDSO 等）。
- `datasets/xfstests/`：用例清单、静态排除原因、样例脚本副本。
- `logs/`：ext4 测试脚本默认日志输出目录。
- `datasets/results/`：用于 git 归档的稳定结果快照。

如需刷新数据集快照，请执行 `benchmark/sync_dataset.sh`。

## 常用测试入口

| 脚本 | 用途 |
|------|------|
| `test/initramfs/src/benchmark/fio/run_6test_summary.sh` | 6-test 综合诊断（raw / ext4 journaled / ext4 nojournal × read/write），见 `benchmark.md` §5 |
| `test/initramfs/src/benchmark/fio/run_ext4_summary.sh` | ext4 官方 O_DIRECT 守底 read/write 摘要 |
| `test/initramfs/src/benchmark/fio/run_parameter_sweep_summary.sh` | **fio 参数 sweep（A–G 全量画像）**：单容器跑遍 `bs`/`numjobs`/`fsync`/`direct`/`page_cache` 并与 Linux 对照，输出 `logs/fio_parameter_sweep_<TS>/fio_parameter_sweep_summary.tsv`。用法与分组见 `benchmark.md` §6；`RUN_G_CORRECTNESS=0` 可只跑 A–F 性能 |
| `test/initramfs/src/benchmark/fio/run_phase5_guard_median.sh` | **Phase 5 守底 / bs 扫描，多轮中位数**（默认 `BENCH_DROP_CACHES=1` 公平基线）。`READ_JOB`/`WRITE_JOB` 可指向 `fio/ext2_seq_*` 做 ext2 对照。见 `benchmark.md` §6.6 |
| `test/initramfs/src/benchmark/fio/run_phase5_ratio_ab.sh` | Phase 5 优化 A/B（`extent_map_cache` 0 vs 1，两边同轮）|
| `test/initramfs/src/benchmark/fio/run_phase5_profile_probe.sh` | Phase 5 四层延迟 profile（FS/virtio/锁/JBD2，门控 `ext4fs.phase2_profile=1`）|
| `tools/ext4/run_phase5_regression.sh` | Phase 5 守底回归（`FULL_SUITE=1` 完整套 / `FULL_GUARD=1` 三模式，drc=0 激活 extent+inode 缓存）|

## Phase 5 读写优化结果（O_DIRECT，nj=1，drop 公平口径，中位数）

四个 ext4 优化（extent 映射缓存 / 全文件覆盖 / atime 节流 / **inode 元数据缓存**）把读写从 16–63% 拉到 75–123%：

| bs | read | write |
|----|-----:|------:|
| 4K | 86.38% | 75.54% |
| 16K | 84.42% | 75.78% |
| 64K | 86.89% | 84.09% |
| 256K | 94.81% | 121.07% |
| 1M | 122.94% | 88.28% |

ext4 域内固定开销已榨干，剩余瓶颈在 Asterinas virtio 设备往返（跨 FS 通用）。完整守底全绿（crash 18/18、concurrency 7/7、xfstests 全 100%）。详见 `feature_perf_phase5_milestone.md`。

> 日志提示：profile 默认 `LOG_LEVEL=error`（profile probe 显式用 `warn`）；不要用 verbose/trace 跑（会产生数百 MB 的单文件日志，超 GitHub 100MB 上限且无分析价值）。
