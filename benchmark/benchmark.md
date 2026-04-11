# Asterinas EXT4 Benchmark/Test 汇总（stage7）

更新时间：2026-04-10（Asia/Shanghai）

## 0. 最新复现（2026-04-10）

1. `fio/ext4_seq_write_bw`：完成 Linux 对比  
   结果：`Asterinas=513 MB/s`，`Linux=2902 MB/s`，`ratio=0.176775`  
   日志：`benchmark/logs/stage7_run_20260410_170858/perf_compare/ext4_seq_write_bw_20260410_090958.log`
2. `fio/ext4_seq_read_bw`：完成 Linux 对比  
   结果：`Asterinas=112 MB/s`，`Linux=1324 MB/s`，`ratio=0.084592`  
   日志：`benchmark/logs/stage7_run_20260410_170858/perf_compare/ext4_seq_read_bw_20260410_091849.log`
3. `phase6_perf_compare`：完成 Linux EXT4 对照性能单轮复现（8 项）  
   统计：`overall_avg_ratio=0.161503 < 0.80`  
   报告：`benchmark/logs/stage7_run_20260410_170858/perf_compare/20260410_093228/phase6_perf_compare_report.txt`  
   汇总：`benchmark/logs/stage7_run_20260410_170858/perf_compare/20260410_093228/phase6_perf_compare_aggregate.tsv`
4. `phase6_only`：PASS  
   统计：`pass=25 fail=0 notrun=0 static_blocked=26 denominator=25 pass_rate=100.00% threshold=90%`  
   日志：`benchmark/logs/stage7_run_20260410_170858/suite/phase6/phase6_good_20260410_093859.log`
5. `phase3_only`：PASS  
   统计：`pass=10 fail=0 notrun=6 static_blocked=24 denominator=10 pass_rate=100.00% threshold=90%`  
   日志：`benchmark/logs/stage7_run_20260410_170858/suite/phase3/phase3_base_guard_20260410_095418.log`
6. `phase4_good`：PASS  
   统计：`pass=12 fail=0 notrun=6 static_blocked=22 denominator=12 pass_rate=100.00% threshold=90%`  
   日志：`benchmark/logs/stage7_run_20260410_170858/suite/phase4/phase4_good_20260410_100230.log`
7. `crash_only`：PASS（`3 场景 x 2 轮 = 6/6`）  
   汇总：`benchmark/logs/stage7_run_20260410_170858/suite/crash/crash/phase4_part3_crash_summary_20260410_101135.tsv`
8. `others_general`：首次失败、重试通过  
   失败日志：`benchmark/logs/stage7_run_20260410_170858/others_general_20260410_181255.log`  
   通过日志：`benchmark/logs/stage7_run_20260410_170858/others_general_retry_20260410_181656.log`

说明：本节记录 `stage7` 当前复现结果；下方历史内容保留作参考。

## 1. 当前结论（Docker 内实跑）

1. `phase3_only`：PASS  
   统计：`pass=10 fail=0 notrun=6 static_blocked=24 denominator=10 pass_rate=100.00% threshold=90%`  
   日志：`benchmark/logs/phase3_base_guard_20260408_071539.log`
2. `phase4_good`：PASS  
   统计：`pass=12 fail=0 notrun=6 static_blocked=22 denominator=12 pass_rate=100.00% threshold=90%`  
   日志：`benchmark/logs/phase4_good_20260408_072542.log`
3. `phase6_only`：PASS（最新门禁）  
   统计：`pass=25 fail=0 notrun=0 static_blocked=26 denominator=25 pass_rate=100.00% threshold=90%`  
   日志：`benchmark/logs/phase6_good_20260408_094026.log`
4. `lmbench_only`：PASS（8/8）  
   汇总：`benchmark/logs/lmbench/phase4_part3_lmbench_summary_20260408_073643.tsv`
5. `crash_only`：PASS（基础崩溃恢复证据链）  
   统计：`3 场景 x 3 轮 = 9/9 PASS`  
   汇总：`benchmark/logs/crash/phase4_part3_crash_summary_20260408_114539.tsv`
6. `phase6_perf_compare`：FAIL（Linux EXT4 对照性能，8项x3轮）  
   统计：`overall_avg_ratio=0.166079 < 0.80`  
   目录：`benchmark/logs/perf_compare/20260408_142155/`  
   汇总：`benchmark/logs/perf_compare/20260408_142155/phase6_perf_compare_aggregate.tsv`

说明：当前统计口径分母为 `PASS + FAIL`，`NOTRUN/STATIC_BLOCKED` 不计入分母。

## 2. 关键状态更新（本轮）

1. P0 Step2 已完成：崩溃恢复证据链达到“固定场景 + 多轮复验 + 日志可复现”出口标准。
2. 证据核验口径：
   - `prepare` 日志命中 `replay hold point reached`
   - `verify` 日志命中 `EXT4_CRASH_VERIFY_PASS`
   - summary 中 `9` 组全部 `PASS`
3. phase6 功能门禁维持 `25/25` 全通过，不受本轮 crash 复验影响。
4. `generic/055` 已完成 3 轮审计复验（`CASE_TIMEOUT=1800`）：
   - `benchmark/logs/phase6_good_20260408_115358.log`
   - `benchmark/logs/phase6_good_20260408_121348.log`
   - `benchmark/logs/phase6_good_20260408_123334.log`
5. `generic/055` 审计结论：扩展预算下稳定 PASS；默认 phase6 门禁预算保持不变，继续按 stress profile 排除。

## 3. 一键命令

在宿主机 `/home/lby/os_com_codex/asterinas` 执行：

```bash
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

# Linux EXT4 对照性能（8项x3轮）
PERF_ROUNDS=3 \
BENCH_ENABLE_KVM=1 \
PERF_CASE_TIMEOUT_SEC=600 \
./tools/ext4/run_phase6_perf_compare_in_docker.sh
```

调试说明：

- P1 当前允许在容器内临时使用 `BENCH_RUN_ONLY=asterinas` 或 `BENCH_RUN_ONLY=linux`，让 `test/initramfs/src/benchmark/bench_linux_and_aster.sh` 只跑单边，缩短 fio 排障回路。
- 单边模式只用于定位性能回归或观察 Asterinas 自身多轮波动，不用于生成最终比例结果。
- 需要写入正式对比结论时，仍应保持默认 `BENCH_RUN_ONLY=both` 跑完整双边对照。
- 最终提交代码前，应恢复按默认双边流程使用 benchmark 脚本，不把单边模式当作最终工作流。

## 4. 目录说明

1. `benchmark/benchmark.md`：测试汇总
2. `benchmark/environment.md`：环境与复现口径
3. `benchmark/datasets/xfstests/`：list、blocked、样例脚本
4. `benchmark/logs/`：默认日志输出目录
5. `benchmark/datasets/results/`：日志副本归档目录
