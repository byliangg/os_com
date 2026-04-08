# Asterinas EXT4 Benchmark/Test 汇总（stage4 + stage6）

更新时间：2026-04-08 06:17（Asia/Shanghai）

## 1. 当前结论（Docker 内实跑）

1. `phase6_only`：PASS  
   统计：`pass=17 fail=0 notrun=10 static_blocked=24 denominator=17 pass_rate=100.00% threshold=90%`  
   日志：`benchmark/logs/phase6_good_20260407_204241.log`
2. `phase6_with_guard`：PASS（phase6 + phase4 + phase3 全通过）  
   - `phase6_good`：`benchmark/logs/phase6_good_20260407_211358.log`  
   - `phase4_good`：`benchmark/logs/phase4_good_20260407_211358.log`  
   - `phase3_base`：`benchmark/logs/phase3_base_guard_20260407_211358.log`
3. `lmbench_only`：PASS（8/8）  
   汇总：`benchmark/logs/lmbench/phase4_part3_lmbench_summary_20260407_221413.tsv`

说明：当前统计口径分母为 `PASS + FAIL`，`NOTRUN/STATIC_BLOCKED` 不计入分母。

## 2. 关键修复点（本轮）

1. 修复 `generic/124` 崩溃/ENOSPC链路：
   - `ext4_rs` 运行时块大小在每次调用前同步；
   - 增加全局串行锁，避免多文件系统块大小串扰；
   - 写入预分配逻辑收敛（避免极端路径误判）。
2. 修复环境根因：`exfat.img` 历史上可能残留为小容量（如 `16M`）。
   - 脚本现在会在 xfstests 前自动扩容镜像（默认 `2G`），避免 `scratch too small`。
3. `phase6_*` 默认单例超时已提升到 `1200s`，降低 `generic/013` 误超时风险。

## 3. 一键命令

在宿主机 `/home/lby/os_com/asterinas` 执行：

```bash
# phase6 全量
PHASE4_DOCKER_MODE=phase6_only \
ENABLE_KVM=1 \
KLOG_LEVEL=error \
./tools/ext4/run_phase4_in_docker.sh

# phase6 + guard（phase4 + phase3）
PHASE4_DOCKER_MODE=phase6_with_guard \
ENABLE_KVM=1 \
KLOG_LEVEL=error \
./tools/ext4/run_phase4_in_docker.sh

# lmbench
PHASE4_DOCKER_MODE=lmbench_only \
ENABLE_KVM=1 \
KLOG_LEVEL=error \
./tools/ext4/run_phase4_in_docker.sh
```

## 4. 目录说明

1. `benchmark/benchmark.md`：测试汇总
2. `benchmark/environment.md`：环境与复现口径
3. `benchmark/datasets/xfstests/`：list、blocked、样例脚本
4. `benchmark/logs/`：默认日志输出目录
5. `benchmark/datasets/results/`：日志副本归档目录
