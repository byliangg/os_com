# Asterinas EXT4 赛题版本（Stage4）

## 1. 项目概述

本仓库是基于 Asterinas 内核进行 EXT4 适配与验证的赛题工程版本，目标是：

1. 在 Asterinas 上实现并验证 EXT4 的核心文件系统能力。
2. 通过 xfstests 与 lmbench 建立可重复、可追踪的功能与性能验证流程。
3. 将测试输入、输出、运行资产统一收敛到仓库内，降低环境漂移风险，支持跨环境直接复现。

本版本重点覆盖 `stage4` 阶段工作，测试默认在 Docker 环境中执行。

## 2. 当前完成情况

### 2.1 功能实现与修复

1. 已完成一组可运行的 EXT4 核心功能链路，覆盖基础文件读写与目录操作相关场景。
2. 已修复历史关键问题：针对 legacy block map 路径的兼容处理（含 direct/single/double/triple 逻辑分支），解决了此前 `generic/013`、`generic/084` 等场景中的目录查找异常。
3. 已形成稳定的阶段测试入口脚本与日志归档流程。
4. 已完成 `ext4_rs` 工程目录迁移：`third_party/ext4_rs -> kernel/libs/ext4_rs`，并保持 workspace 依赖方式不变。

### 2.2 测试状态（最新一轮）

1. `phase4_good`（基于 `phase4_good.list` 共 40 个候选）：
   `PASS=6`，`FAIL=0`，`NOTRUN=14`，`STATIC_BLOCKED=20`，脚本口径 `pass_rate=100%`。
   当前通过样例：`generic/001, 006, 013, 028, 035, 084`。
2. `phase3_base`（基于 `phase3_base.list` 共 40 个候选）：
   `PASS=4`，`FAIL=0`，`NOTRUN=14`，`STATIC_BLOCKED=22`，脚本口径 `pass_rate=100%`。
   当前通过样例：`generic/006, 013, 028, 084`。
3. `lmbench`：`8/8` 通过，覆盖 VFS 延迟与小文件/拷贝吞吐测试。

最新日志见：

1. `benchmark/logs/phase4_good_20260407_120958.log`
2. `benchmark/logs/phase3_base_guard_20260407_122320.log`
3. `benchmark/logs/lmbench/phase4_part3_lmbench_summary_20260407_121811.tsv`

## 3. 测试体系说明

### 3.1 xfstests 模式

当前主要使用以下模式：

1. `phase4_good`
2. `phase3_base`

对应用例集合与静态排除列表在：

1. `test/initramfs/src/syscall/xfstests/testcases/`
2. `test/initramfs/src/syscall/xfstests/blocked/`

### 3.2 结果口径

当前脚本中 xfstests 通过率口径为：

1. 分母 = `PASS + FAIL`
2. `NOTRUN` 与 `STATIC_BLOCKED` 不计入分母

因此阅读测试结果时，需要同时结合详细结果表看覆盖边界。

### 3.3 当前样例覆盖范围（概览）

1. 已稳定通过的 xfstests 样例主要覆盖：基础文件创建/读写/截断、目录查找与路径遍历等核心链路（如 `generic/001/006/013/028/035/084`）。
2. 当前 `NOTRUN` 样例主要受能力或工具依赖限制：例如 hardlink、`shutdown/freezing`、`attr` 工具缺失、`O_DIRECT`、scratch 设备容量限制。
3. `STATIC_BLOCKED` 样例主要是阶段性未纳入范围的语义：如 hardlink/symlink、`O_TMPFILE/flink`、`renameat2`、`fallocate/collapse-range/fiemap`、xattr/chacl 等。
4. lmbench 当前覆盖 8 项：`open/stat/fstat/read/write` 延迟、`create+delete(0k/10k)`、`copy_files_bw`。

## 4. 一键测试（推荐）

在仓库根目录执行：

```bash
cd /home/lby/os_com/asterinas

# 1) phase4
PHASE4_DOCKER_MODE=phase4_good \
ENABLE_KVM=1 \
XFSTESTS_CASE_TIMEOUT_SEC=900 \
KLOG_LEVEL=error \
./tools/ext4/run_phase4_in_docker.sh

# 2) phase3
PHASE4_DOCKER_MODE=phase3_only \
ENABLE_KVM=1 \
XFSTESTS_CASE_TIMEOUT_SEC=900 \
KLOG_LEVEL=error \
./tools/ext4/run_phase4_in_docker.sh

# 3) lmbench
PHASE4_DOCKER_MODE=lmbench_only \
ENABLE_KVM=1 \
KLOG_LEVEL=error \
./tools/ext4/run_phase4_in_docker.sh
```

## 5. 目录与文档索引

| 文档/目录 | 作用 |
| --- | --- |
| `benchmark/README.md` | benchmark 子系统总览 |
| `benchmark/benchmark.md` | 最新测试结论与结果摘要 |
| `benchmark/environment.md` | 复现环境、依赖与命令说明 |
| `benchmark/assets/README.md` | 测试运行资产说明（initramfs、xfstests、vDSO） |
| `benchmark/datasets/xfstests/README.md` | xfstests 样例数据集说明 |
| `benchmark/logs/` | 测试脚本默认日志输出目录 |
| `benchmark/datasets/results/` | 归档后的稳定结果快照（便于 git 追踪） |
| `tools/ext4/run_phase4_in_docker.sh` | 主测试入口（Docker） |
| `tools/ext4/run_phase4_part3.sh` | phase3/phase4/lmbench 组合执行逻辑 |

历史阶段文档（保留）示例：

1. `EXT4_PHASE2_REPRO.md`
2. `EXT4_PHASE3_REPRO.md`
3. `EXT4_PHASE3_PART1_SUMMARY.md`
4. `asterinas_ext4_phase2_manual_test_commands.md`

## 6. 复现所需运行资产

为降低对宿主 `.local` 的依赖，当前脚本默认读取仓库内资产：

1. `benchmark/assets/initramfs/initramfs_phase3.cpio.gz`
2. `benchmark/assets/xfstests-prebuilt/xfstests-dev`
3. `benchmark/assets/linux_vdso/`

对应默认路径已在脚本中切换完成，核心涉及：

1. `tools/ext4/run_phase4_in_docker.sh`
2. `tools/ext4/run_phase4_part1.sh`
3. `tools/ext4/run_phase4_part2.sh`
4. `tools/ext4/run_phase4_part3.sh`
5. `tools/ext4/prepare_phase4_part*_initramfs.sh`
6. `tools/ext4/prepare_xfstests_prebuilt.sh`

## 7. 最小复现检查

在跑测试前建议先检查：

```bash
cd /home/lby/os_com/asterinas

test -f benchmark/assets/initramfs/initramfs_phase3.cpio.gz
test -d benchmark/assets/xfstests-prebuilt/xfstests-dev
test -f benchmark/assets/linux_vdso/vdso_x86_64.so
```

环境前提：

1. 宿主机已安装 Docker。
2. 宿主机可使用 `--privileged` 与 `/dev` 挂载能力。
3. 建议支持 KVM（`ENABLE_KVM=1`），否则部分测试耗时会明显增加。

## 8. 当前边界与后续方向

当前结果体现的是 `stage4` 阶段可运行能力与门禁通过状态。后续可继续推进：

1. 扩大 xfstests 可运行集合，减少 `NOTRUN/STATIC_BLOCKED`。
2. 完善更高阶段的功能覆盖（例如更多 POSIX 语义、崩溃一致性、并发场景）。
3. 进一步沉淀自动化验收规范与提交前自检流程。

## 9. 致谢与来源说明

本工程基于 Asterinas 社区项目进行赛题方向开发，保留原工程结构与许可证信息。
