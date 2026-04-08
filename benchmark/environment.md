# Asterinas EXT4 Environment（Current, stage4 + stage6）

更新时间：2026-04-08 06:17（Asia/Shanghai）

## 1. 当前分支与执行口径

1. 分支：`stage-1234-sumup`
2. 执行方式：统一走 Docker 入口 `tools/ext4/run_phase4_in_docker.sh`
3. 日志目录：`benchmark/logs/`
4. 测试资产目录：`benchmark/assets/`（不依赖 `.local`）

## 2. 最新稳定结果

1. `phase6_only`：PASS（`pass_rate=100.00%`）
2. `phase6_with_guard`：PASS（`phase6_good + phase4_good + phase3_base` 全绿）
3. `lmbench_only`：PASS（8/8）

关键日志：

1. `benchmark/logs/phase6_good_20260407_204241.log`
2. `benchmark/logs/phase6_good_20260407_211358.log`
3. `benchmark/logs/phase4_good_20260407_211358.log`
4. `benchmark/logs/phase3_base_guard_20260407_211358.log`
5. `benchmark/logs/lmbench/phase4_part3_lmbench_summary_20260407_221413.tsv`

## 3. 本轮关键环境修复

1. 修复了 `exfat.img` 容量漂移问题（历史可能残留为 `16M`）：
   - 现在 `run_phase4_part3.sh` 在 xfstests 前会自动执行：
     - `truncate -s ${XFSTESTS_TEST_IMG_SIZE:-2G} test/initramfs/build/ext2.img`
     - `truncate -s ${XFSTESTS_SCRATCH_IMG_SIZE:-2G} test/initramfs/build/exfat.img`
2. `phase6_*` 模式默认超时：
   - 单例超时：`XFSTESTS_CASE_TIMEOUT_SEC=1200`
   - 整体超时：`XFSTESTS_RUN_TIMEOUT_SEC=5400`
3. 内核侧增加 `ext4_rs` 运行时块大小同步与串行保护，降低多文件系统场景串扰风险。

## 4. 复现命令

```bash
cd /home/lby/os_com/asterinas

# phase6 全量
PHASE4_DOCKER_MODE=phase6_only ENABLE_KVM=1 KLOG_LEVEL=error ./tools/ext4/run_phase4_in_docker.sh

# phase6 + guard
PHASE4_DOCKER_MODE=phase6_with_guard ENABLE_KVM=1 KLOG_LEVEL=error ./tools/ext4/run_phase4_in_docker.sh

# lmbench
PHASE4_DOCKER_MODE=lmbench_only ENABLE_KVM=1 KLOG_LEVEL=error ./tools/ext4/run_phase4_in_docker.sh
```

可选参数（覆盖默认）：

1. `XFSTESTS_CASE_TIMEOUT_SEC`
2. `XFSTESTS_RUN_TIMEOUT_SEC`
3. `XFSTESTS_TEST_IMG_SIZE`（默认 `2G`）
4. `XFSTESTS_SCRATCH_IMG_SIZE`（默认 `2G`）
