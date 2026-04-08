# Asterinas EXT4 Environment（Current, stage4 + stage6）

更新时间：2026-04-08 22:55（Asia/Shanghai）

## 1. 当前分支与执行口径

1. 分支：`stage-6`
2. 执行方式：统一走 Docker 入口 `tools/ext4/run_phase4_in_docker.sh`
3. 日志目录：`benchmark/logs/`
4. 测试资产目录：`benchmark/assets/`（不依赖 `.local`）

## 2. 最新稳定结果

1. `phase3_only`：PASS（`pass_rate=100.00%`）
2. `phase4_good`：PASS（`pass_rate=100.00%`）
3. `phase6_only`：PASS（`pass_rate=100.00%`，`25/25`）
4. `lmbench_only`：PASS（8/8）
5. `crash_only`：PASS（`3 场景 x 3 轮 = 9/9`）
6. `generic/055` 审计复验（`CASE_TIMEOUT=1800`）：3/3 PASS
7. `phase6_perf_compare`：`8项 x 3轮` 已执行，`overall_avg_ratio=0.166079`（阈值 `0.80`，未达标）

关键日志：

1. `benchmark/logs/phase3_base_guard_20260408_071539.log`
2. `benchmark/logs/phase4_good_20260408_072542.log`
3. `benchmark/logs/phase6_good_20260408_094026.log`
4. `benchmark/logs/lmbench/phase4_part3_lmbench_summary_20260408_073643.tsv`
5. `benchmark/logs/crash/phase4_part3_crash_summary_20260408_114539.tsv`
6. `benchmark/logs/phase6_good_20260408_115358.log`
7. `benchmark/logs/phase6_good_20260408_121348.log`
8. `benchmark/logs/phase6_good_20260408_123334.log`
9. `benchmark/logs/perf_compare/20260408_142155/phase6_perf_compare_report.txt`
10. `benchmark/logs/perf_compare/20260408_142155/phase6_perf_compare_aggregate.tsv`
11. `benchmark/logs/others_general_20260408_224447.log`

## 3. 本轮关键环境要点

1. `phase6_*` 模式默认超时：
   - 单例超时：`XFSTESTS_CASE_TIMEOUT_SEC=1200`
   - 整体超时：`XFSTESTS_RUN_TIMEOUT_SEC=5400`
2. `run_phase4_part3.sh` 在 xfstests 前自动标准化镜像容量：
   - `XFSTESTS_TEST_IMG_SIZE` 默认 `2G`
   - `XFSTESTS_SCRATCH_IMG_SIZE` 默认 `2G`
3. `crash_only` 可直接产出崩溃恢复证据链 summary。
4. `ENABLE_KVM=1` 的 qemu 参数已采用 `-enable-kvm`（避免 `-accel` 与 `-machine accel=` 冲突）。
5. `run_phase6_perf_compare_in_docker.sh` 支持 `PERF_CASE_TIMEOUT_SEC`，默认 `600s`，防止单项性能用例卡死。

## 4. 复现命令

```bash
cd /home/lby/os_com/asterinas

# phase3 guard
PHASE4_DOCKER_MODE=phase3_only \
ENABLE_KVM=1 \
XFSTESTS_CASE_TIMEOUT_SEC=900 \
KLOG_LEVEL=error \
./tools/ext4/run_phase4_in_docker.sh

# phase4 good
PHASE4_DOCKER_MODE=phase4_good \
ENABLE_KVM=1 \
XFSTESTS_CASE_TIMEOUT_SEC=900 \
KLOG_LEVEL=error \
./tools/ext4/run_phase4_in_docker.sh

# phase6 good
PHASE4_DOCKER_MODE=phase6_only \
ENABLE_KVM=1 \
KLOG_LEVEL=error \
./tools/ext4/run_phase4_in_docker.sh

# lmbench
PHASE4_DOCKER_MODE=lmbench_only \
ENABLE_KVM=1 \
KLOG_LEVEL=error \
./tools/ext4/run_phase4_in_docker.sh

# crash evidence (3 scenes x 3 rounds)
PHASE4_DOCKER_MODE=crash_only \
ENABLE_KVM=1 \
CRASH_ROUNDS=3 \
KLOG_LEVEL=warn \
./tools/ext4/run_phase4_in_docker.sh

# generic/055 audit (ignore static exclusion, 1800s case timeout)
PHASE4_DOCKER_MODE=phase6_only \
ENABLE_KVM=1 \
XFSTESTS_SINGLE_TEST=generic/055 \
XFSTESTS_IGNORE_STATIC_EXCLUDED_FOR_SINGLE=1 \
XFSTESTS_CASE_TIMEOUT_SEC=1800 \
XFSTESTS_RUN_TIMEOUT_SEC=4000 \
KLOG_LEVEL=error \
./tools/ext4/run_phase4_in_docker.sh

# Linux EXT4 对照性能（8项x3轮）
PERF_ROUNDS=3 \
BENCH_ENABLE_KVM=1 \
PERF_CASE_TIMEOUT_SEC=600 \
./tools/ext4/run_phase6_perf_compare_in_docker.sh
```

可选参数（覆盖默认）：

1. `XFSTESTS_CASE_TIMEOUT_SEC`
2. `XFSTESTS_RUN_TIMEOUT_SEC`
3. `XFSTESTS_TEST_IMG_SIZE`
4. `XFSTESTS_SCRATCH_IMG_SIZE`
