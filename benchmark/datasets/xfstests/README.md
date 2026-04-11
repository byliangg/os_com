# xfstests 样例数据集（仓库内）

该数据集已提交到仓库，读取测试样例时不再依赖 `.local` 目录。

## 内容说明

- `lists/`：阶段候选用例清单。
- `blocked/`：静态排除用例及原因。
- `samples/generic/`：从上游 `xfstests` 拷贝的 `tests/generic/*` 脚本与期望输出（基于 `phase3+phase4+phase6` 用例并集）。
- `licenses/`：上游许可与参考文件。

## 上游来源

- 同步来源目录：/home/lby/os_com_codex/asterinas/benchmark/assets/xfstests-src
- 上游版本：a7b2080d1e8676a8a6635816ac13e4011ba87688

## 同步方式

当 list 或 excluded 变化后，执行 `benchmark/sync_dataset.sh` 重新同步。
