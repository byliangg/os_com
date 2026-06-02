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

> 日志提示：sweep 默认 `LOG_LEVEL=error`，不要用 verbose/trace 跑（会产生数百 MB 的单文件日志，超 GitHub 100MB 上限且无分析价值）。
