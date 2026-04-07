# Asterinas EXT4 Benchmark/Test 汇总（stage4）

更新时间：2026-04-07 20:30（Asia/Shanghai）

## 1. 目录调整（已独立到仓库内）

`benchmark` 相关资产已统一放在仓库目录：

1. `benchmark/benchmark.md`
2. `benchmark/environment.md`
3. `benchmark/datasets/xfstests/`（list、blocked、可读样例脚本）
4. `benchmark/logs/`（测试脚本默认输出）
5. `benchmark/datasets/results/`（归档日志副本）

这意味着“查看测试样例”不再依赖 `.local`。

## 2. 当前实跑结论（Docker 内）

1. `phase4_good`：PASS  
   统计：`pass=6 fail=0 notrun=14 static_blocked=20 denominator=6 pass_rate=100.00% threshold=90%`  
   其中 `generic/013`、`generic/084` 均为 PASS。
2. `phase3_base`：PASS  
   统计：`pass=4 fail=0 notrun=14 static_blocked=22 denominator=4 pass_rate=100.00% threshold=90%`。
3. `lmbench`：PASS（8/8）。

说明：以上通过率是当前脚本定义口径（分母 = PASS + FAIL），`NOTRUN/STATIC_BLOCKED` 不计入分母。

## 3. 关键实跑命令

在宿主机 `/home/lby/os_com/asterinas` 执行：

```bash
# Phase4
PHASE4_DOCKER_MODE=phase4_good \
ENABLE_KVM=1 \
XFSTESTS_CASE_TIMEOUT_SEC=900 \
KLOG_LEVEL=error \
./tools/ext4/run_phase4_in_docker.sh

# Phase3
PHASE4_DOCKER_MODE=phase3_only \
ENABLE_KVM=1 \
XFSTESTS_CASE_TIMEOUT_SEC=900 \
KLOG_LEVEL=error \
./tools/ext4/run_phase4_in_docker.sh

# LMbench
PHASE4_DOCKER_MODE=lmbench_only \
ENABLE_KVM=1 \
KLOG_LEVEL=error \
./tools/ext4/run_phase4_in_docker.sh
```

说明：若 `phase3` 遇到 QEMU `hostfwd` 端口冲突，可固定端口后重跑，例如：

```bash
PHASE4_DOCKER_MODE=phase3_only \
ENABLE_KVM=1 \
XFSTESTS_CASE_TIMEOUT_SEC=900 \
KLOG_LEVEL=error \
SSH_PORT=42222 NGINX_PORT=48080 REDIS_PORT=46379 IPERF_PORT=45201 \
LMBENCH_TCP_LAT_PORT=41234 LMBENCH_TCP_BW_PORT=41236 MEMCACHED_PORT=41121 \
./tools/ext4/run_phase4_in_docker.sh
```

## 4. 可复核数据位置

1. 原始日志：
   - `benchmark/logs/phase4_good_20260407_120958.log`
   - `benchmark/logs/phase3_base_guard_20260407_122320.log`
   - `benchmark/logs/lmbench/phase4_part3_lmbench_summary_20260407_121811.tsv`
2. 仓库内副本：
   - `benchmark/datasets/results/phase4_good_20260407_120958.log`
   - `benchmark/datasets/results/phase3_base_guard_20260407_122320.log`
   - `benchmark/datasets/results/phase4_part3_lmbench_summary_20260407_121811.tsv`
3. 用例定义与样例：
   - `benchmark/datasets/xfstests/lists/*.list`
   - `benchmark/datasets/xfstests/blocked/*.tsv`
   - `benchmark/datasets/xfstests/samples/generic/*`
