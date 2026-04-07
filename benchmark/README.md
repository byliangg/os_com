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
