# Asterinas EXT4 赛题版本（Stage7）

## 1. 项目概述

本仓库是基于 Asterinas 内核进行 EXT4 适配与验证的赛题工程版本，目标是：

1. 在 Asterinas 上实现并验证 EXT4 的核心文件系统能力。
2. 通过 xfstests 与 lmbench 建立可重复、可追踪的功能与性能验证流程。
3. 将测试输入、输出、运行资产统一收敛到仓库内，降低环境漂移风险，支持跨环境直接复现。

本版本重点覆盖 `stage7` 阶段工作，测试默认在 Docker 环境中执行。

## 2. 当前完成情况

### 2.1 功能实现与修复

1. 已完成一组可运行的 EXT4 核心功能链路，覆盖基础文件读写与目录操作相关场景。
2. 已修复历史关键问题：针对 legacy block map 路径的兼容处理（含 direct/single/double/triple 逻辑分支），解决了此前 `generic/013`、`generic/084` 等场景中的目录查找异常。
3. 已形成稳定的阶段测试入口脚本与日志归档流程。
4. 已完成 `ext4_rs` 工程目录迁移：`third_party/ext4_rs -> kernel/libs/ext4_rs`，并保持 workspace 依赖方式不变。
5. 已新增 EXT4 fio 单项对照作业：`fio/ext4_seq_write_bw`、`fio/ext4_seq_read_bw`，并接入 `bench_linux_and_aster.sh` 统一对比口径。
6. 已完成 Stage7 写路径优化：
   1. `ext4_rs::write_at` 改为“整段预映射 + 连续物理块批量写”，避免原先按 4KiB 块逐次读改写导致的 I/O 放大。
   2. 对“块对齐的追加写”增加快速预分配分支，减少 `ensure_write_range_mapped` 逐块探测开销。
7. 已完成 Stage7 读路径优化：
   1. `ext4_rs::read_at` 增加 extent 命中缓存，减少顺序读中重复 extent tree 查询。
   2. 引入复用缓冲读取路径，减少每块 `Vec` 分配/拷贝。
8. 已完成内核 EXT4 设备适配层优化：
   1. 增加 `read_offset_into` 接口，用于向已有缓冲区读取。
   2. 对齐读写命中 fast path 时直接走设备读写，减少中间对齐缓冲。
9. 已修正 fio 对比流程公平性：Asterinas/Linux 均基于重新 prepare 后的镜像执行，避免镜像状态不对称。

### 2.2 测试状态（最新一轮）

1. `phase3_only`：PASS  
   `pass=10 fail=0 notrun=6 static_blocked=24 denominator=10 pass_rate=100.00%`
2. `phase4_good`：PASS  
   `pass=12 fail=0 notrun=6 static_blocked=22 denominator=12 pass_rate=100.00%`
3. `phase6_only`：PASS  
   `pass=25 fail=0 notrun=0 static_blocked=26 denominator=25 pass_rate=100.00%`
4. `lmbench_only`：PASS（8/8）
5. `crash_only`：PASS（`3 场景 x 3 轮 = 9/9`）
6. `phase6_perf_compare`：FAIL（Linux EXT4 对照性能，`8项 x 3轮`）  
   `overall_avg_ratio=0.166079`（目标阈值 `0.80`）
7. `fio_ext4_compare`：FAIL（目标阈值 `0.80`，单线程口径 `numjobs=1`）
   1. 基线（Stage6 公平对照）：
      1. 顺序写：Asterinas `7780KiB/s` vs Linux `1197MiB/s`（约 `0.635%`）
      2. 顺序读：Asterinas `12.1MiB/s` vs Linux `5101MiB/s`（约 `0.237%`）
   2. Stage7 最新（2026-04-09）：
      1. 顺序写：Asterinas `609MiB/s` vs Linux `1134MiB/s`（约 `53.704%`，较基线约 `80.16x`）
      2. 顺序读：Asterinas `36.3MiB/s` vs Linux `5153MiB/s`（约 `0.704%`，较基线约 `3.00x`）

最新日志见：

1. `benchmark/logs/phase3_base_guard_20260408_071539.log`
2. `benchmark/logs/phase4_good_20260408_072542.log`
3. `benchmark/logs/phase6_good_20260408_094026.log`
4. `benchmark/logs/lmbench/phase4_part3_lmbench_summary_20260408_073643.tsv`
5. `benchmark/logs/crash/phase4_part3_crash_summary_20260408_114539.tsv`
6. `benchmark/logs/perf_compare/20260408_142155/phase6_perf_compare_report.txt`
7. `benchmark/logs/perf_compare/20260408_142155/phase6_perf_compare_aggregate.tsv`
8. `benchmark/logs/perf_compare/fio_ext4_seq_write_fair_20260409_174226.log`
9. `benchmark/logs/perf_compare/fio_ext4_seq_read_fair_20260409_174908.log`
10. `benchmark/logs/perf_compare/fio_ext4_seq_write_opt3b_afterpatch_20260409_193147.log`
11. `benchmark/logs/perf_compare/fio_ext4_seq_read_opt3b_afterpatch_20260409_193920.log`

### 2.3 与“良好”指标的差异

按当前赛题“良好”口径，核心差异如下：

1. `xfstests` 阶段集通过率（`>=90%`）：已满足（`phase6_only` 为 `25/25`，`pass_rate=100%`）。
2. 基础崩溃恢复证据链：已满足（固定 3 场景、每场景 3 轮、日志可复现）。
3. Linux EXT4 对照性能（目标 `>=80%`）：未满足，当前 `overall_avg_ratio=0.166079`。
4. fio EXT4 对照性能（目标 `>=80%`）：未满足。
   1. 顺序写已从极低基线提升到 `53.704%`。
   2. 顺序读仍仅 `0.704%`，是 Stage7 之后的主要性能瓶颈。

结论：当前“良好”指标仍主要卡在 Linux EXT4 对照性能，其中读路径是下一阶段核心攻关项。

### 2.4 Stage7 今日增量（2026-04-09）

1. 新增并跑通 EXT4 fio 对照链路（Asterinas vs Linux）。
2. 修正了 fio 对比流程公平性与结果口径（以原始 fio `Run status` 为准）。
3. 完成了 EXT4 写路径批量化、追加写快速预分配、设备适配层 fast path 优化。
4. 完成了 EXT4 读路径 extent 缓存与缓冲复用优化。
5. 完成多轮容器内复测并归档日志，形成 Stage7 可协作追踪证据。

## 3. 测试体系说明

### 3.1 xfstests 模式

当前主要使用以下模式：

1. `phase4_good`
2. `phase3_only`
3. `phase6_only`
4. `lmbench_only`
5. `crash_only`

对应用例集合与静态排除列表在：

1. `test/initramfs/src/syscall/xfstests/testcases/`
2. `test/initramfs/src/syscall/xfstests/blocked/`

### 3.2 结果口径

当前脚本中 xfstests 通过率口径为：

1. 分母 = `PASS + FAIL`
2. `NOTRUN` 与 `STATIC_BLOCKED` 不计入分母

因此阅读测试结果时，需要同时结合详细结果表看覆盖边界。

### 3.3 当前样例覆盖范围（概览）

1. 当前 `phase6` 候选池为 `51`，静态排除 `26`，理论可运行集合 `25`。
2. 当前门禁运行集合 `25/25` 均通过（`phase6_only`）。
3. `STATIC_BLOCKED` 主要是阶段性能力外语义：如 AIO、hardlink/symlink、`O_TMPFILE/flink`、`renameat2`、`fallocate/collapse-range/fiemap`、xattr/chacl 等。
4. lmbench 覆盖 8 项：`open/stat/fstat/read/write` 延迟、`create+delete(0k/10k)`、`copy_files_bw`。

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

# 4) phase6 功能门禁
PHASE4_DOCKER_MODE=phase6_only \
ENABLE_KVM=1 \
KLOG_LEVEL=error \
./tools/ext4/run_phase4_in_docker.sh

# 5) Linux EXT4 对照性能（8项x3轮）
PERF_ROUNDS=3 \
BENCH_ENABLE_KVM=1 \
PERF_CASE_TIMEOUT_SEC=600 \
./tools/ext4/run_phase6_perf_compare_in_docker.sh

# 6) EXT4 fio 单项对照（一键，顺序写）
LOG=benchmark/logs/perf_compare/fio_ext4_seq_write_$(date +%Y%m%d_%H%M%S).log
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
bash test/initramfs/src/benchmark/bench_linux_and_aster.sh fio/ext4_seq_write_bw x86_64 >"$LOG" 2>&1
echo "fio 顺序写日志：$LOG"

# 7) EXT4 fio 单项对照（一键，顺序读）
LOG=benchmark/logs/perf_compare/fio_ext4_seq_read_$(date +%Y%m%d_%H%M%S).log
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
bash test/initramfs/src/benchmark/bench_linux_and_aster.sh fio/ext4_seq_read_bw x86_64 >"$LOG" 2>&1
echo "fio 顺序读日志：$LOG"

# 8) EXT4 fio 双项串行对照（一键，先写后读）
for JOB in fio/ext4_seq_write_bw fio/ext4_seq_read_bw; do
   LOG=benchmark/logs/perf_compare/${JOB##*/}_$(date +%Y%m%d_%H%M%S).log
   BENCH_ENABLE_KVM=1 \
   BENCH_ASTER_NETDEV=tap \
   BENCH_ASTER_VHOST=on \
   bash test/initramfs/src/benchmark/bench_linux_and_aster.sh "$JOB" x86_64 >"$LOG" 2>&1
   echo "$JOB 日志：$LOG"
done

# 9) 通用回归（非 ext4），用于检查 kernel 改动是否波及其他子系统
# 说明：
# - 先重建 ext2/exfat 镜像，避免被 xfstests 流程污染
# - 该回归建议 ENABLE_KVM=0，规避部分环境下 qemu accel 参数冲突
./tools/reset_ext2_exfat_images.sh

PROXY_HTTP=http://127.0.0.1:7890
PROXY_SOCKS=socks5://127.0.0.1:7890
DOCKER_TAG=$(cat DOCKER_IMAGE_VERSION 2>/dev/null || cat VERSION)
LOG=benchmark/logs/others_general_$(date +%Y%m%d_%H%M%S).log

docker run --rm --privileged --network=host \
  -v /dev:/dev \
  -v "$PWD":/root/asterinas \
  -w /root/asterinas \
  -e http_proxy="$PROXY_HTTP" \
  -e https_proxy="$PROXY_HTTP" \
  -e all_proxy="$PROXY_SOCKS" \
  -e HTTP_PROXY="$PROXY_HTTP" \
  -e HTTPS_PROXY="$PROXY_HTTP" \
  -e ALL_PROXY="$PROXY_SOCKS" \
  "asterinas/asterinas:${DOCKER_TAG}" \
  bash -lc '
set -euo pipefail
mkdir -p /root/.cargo/bin
cat >/root/.cargo/bin/cargo-osdk << "EOS"
#!/usr/bin/env bash
set -euo pipefail
ROOT=${ASTERINAS_ROOT:-/root/asterinas}
BIN="${ROOT}/target_lby/debug/cargo-osdk"
STAMP="${ROOT}/target_lby/.cargo_osdk_local_dev"
if [ ! -x "${BIN}" ] || [ ! -f "${STAMP}" ]; then
  if [ -x "${BIN}" ] && [ ! -f "${STAMP}" ]; then
    cargo clean --manifest-path "${ROOT}/osdk/Cargo.toml" -p cargo-osdk || true
  fi
  OSDK_LOCAL_DEV=1 cargo build --manifest-path "${ROOT}/osdk/Cargo.toml" --bin cargo-osdk
  mkdir -p "$(dirname "${STAMP}")"
  touch "${STAMP}"
fi
exec "${BIN}" "$@"
EOS
chmod +x /root/.cargo/bin/cargo-osdk
export VDSO_LIBRARY_DIR=/root/asterinas/benchmark/assets/linux_vdso
export CARGO_TARGET_DIR=/root/asterinas/target_lby
timeout 5400s make AUTO_TEST=test ENABLE_KVM=0 LOG_LEVEL=error CONSOLE=ttyS0 BOOT_METHOD=qemu-direct OVMF=off RELEASE_LTO=1 run_kernel
' | tee "$LOG"

echo "通用回归日志：$LOG"
grep -E "mount: mounting /dev/vda|mount: mounting /dev/vdb|All test in /test/fs passed|All general tests passed" "$LOG" | tail -n 20
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

当前结果体现的是 `stage7` 阶段在写路径上的显著改进与读路径瓶颈定位。后续优先方向：

1. 持续优化 EXT4 顺序读路径（重点：批量读/预读、进一步减少映射查询与同步等待），目标优先把 fio 读对照占比提升到两位数以上。
2. 在保持当前写路径收益不回退的前提下，继续冲刺 Linux EXT4 对照 `>=80%`。
3. 在不回退现有通过率的前提下，继续扩大 xfstests 可运行集合，逐步减少 `STATIC_BLOCKED`。
4. 继续沉淀自动化验收规范与提交前自检流程。

## 9. 致谢与来源说明

本工程基于 Asterinas 社区项目进行赛题方向开发，保留原工程结构与许可证信息。
