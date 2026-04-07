# Asterinas EXT4 Environment（Current, stage4）

更新时间：2026-04-07 19:45（Asia/Shanghai）

## 1. 当前状态

1. 分支：`stage4`
2. 测试执行位置：Docker 容器内（宿主机只负责启动）
3. 结果口径：
   - `phase4_good`：`pass=6 fail=0 notrun=14 static_blocked=20 denominator=6`
   - `phase3_base`：`pass=4 fail=0 notrun=14 static_blocked=22 denominator=4`
   - `lmbench`：8/8 PASS

## 2. Benchmark 资产目录（仓库内）

1. 汇总：`benchmark/benchmark.md`
2. 环境：`benchmark/environment.md`
3. xfstests 样例数据集：`benchmark/datasets/xfstests/`
4. 默认日志输出：`benchmark/logs/`
5. 日志副本：`benchmark/datasets/results/`

说明：阅读测试样例（list/blocked/generic 脚本）不再依赖 `.local`。

## 3. 运行测试仍需的环境前提

1. 仓库路径：`/home/lby/os_com/asterinas`
2. Docker 镜像：`asterinas/asterinas:0.17.0-20260227`
3. 建议启用：`ENABLE_KVM=1`
4. 运行时依赖的本地资产：
   - `benchmark/assets/initramfs/initramfs_phase3.cpio.gz`
   - `benchmark/assets/xfstests-prebuilt/xfstests-dev`
   - `benchmark/assets/linux_vdso/`
5. 目标目录：`target_lby`（可自动生成）

## 4. 复现命令

```bash
cd /home/lby/os_com/asterinas

PHASE4_DOCKER_MODE=phase4_good ENABLE_KVM=1 XFSTESTS_CASE_TIMEOUT_SEC=900 KLOG_LEVEL=error ./tools/ext4/run_phase4_in_docker.sh
PHASE4_DOCKER_MODE=phase3_only ENABLE_KVM=1 XFSTESTS_CASE_TIMEOUT_SEC=900 KLOG_LEVEL=error ./tools/ext4/run_phase4_in_docker.sh
PHASE4_DOCKER_MODE=lmbench_only ENABLE_KVM=1 KLOG_LEVEL=error ./tools/ext4/run_phase4_in_docker.sh
```

## 5. 关键日志路径

1. `benchmark/logs/phase4_good_20260407_112525.log`
2. `benchmark/logs/phase3_base_guard_20260407_113342.log`
3. `benchmark/logs/lmbench/phase4_part3_lmbench_summary_20260407_114043.tsv`
