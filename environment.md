# Asterinas EXT4 Environment（Current, Phase 4 PageCache）

更新时间：2026-05-14（Asia/Shanghai）

## 1. 目标与范围

这份文档记录当前 ext4 Phase 4 PageCache 集成与 benchmark 阶段的推荐环境。
当前优先使用 Docker runner 复现 Phase 2/3 守底回归、Phase 3 fsync/flush 持久化测试、Phase 4 PageCache xfstests 与 PageCache benchmark；宿主机直跑只作为排障辅助。

当前结论：

1. Phase 2 correctness baseline 已收口。
2. Phase 2 concurrency final baseline：7/7 PASS，`EXT4_PHASE2_WORKERS=4 EXT4_PHASE2_ROUNDS=8 EXT4_PHASE2_SEED=78`。
3. 最新 baseline 日志：`benchmark/logs/jbd_phase2_concurrency_20260514_034441.log`。
4. Phase 2/JBD2 守底 fio 为 read `93.49%`、write `87.01%`；Phase 3 Step 6 普通 fio 复跑为 read `127.06%`、write `39.18%`。
5. Phase 3 fsync/flush 语义主线已收口；普通 fio write 当前低于 `75%` hardening 红线，作为后续性能 hardening blocker 单独推进。
6. Phase 4 PageCache correctness 已恢复到 `pagecache_phase4` full list `9 PASS / 0 FAIL / 4 NOTRUN`。
7. Phase 4 PageCache benchmark A-E 已落地：`lmbench_only`、buffered fio cold/warm read、buffered fio write、O_DIRECT cache-off 守底。O_DIRECT 指标与 PageCache 指标必须分开统计。

## 2. 当前结论（截至 2026-05-14）

1. 当前有效工作树：`/home/lby/os_com_codex/asterinas`
2. 当前功能 baseline：
   - `phase3_base_guard`：10 PASS / 0 FAIL / 6 NOTRUN / 24 STATIC_BLOCKED
   - `phase4_good`：12 PASS / 0 FAIL / 6 NOTRUN / 22 STATIC_BLOCKED
   - `phase6_good`：25/25 PASS
   - `jbd_phase1`：6 PASS / 0 FAIL / 6 NOTRUN
   - JBD2 crash matrix：18/18 PASS，summary `benchmark/logs/crash/phase4_part3_crash_summary_20260514_043248.tsv`
   - lmbench regression：8/8 PASS，summary `benchmark/logs/lmbench/phase4_part3_lmbench_summary_20260514_051539.tsv`
   - Phase 2 concurrency：7/7 PASS，日志 `benchmark/logs/jbd_phase2_concurrency_20260514_034441.log`
   - `jbd_phase3_fsync_flush`：11 PASS / 0 FAIL / 1 NOTRUN，日志 `benchmark/logs/jbd_phase3_fsync_durability_20260514_034641.log`
   - Phase 3 host-crash fsync matrix：4/4 PASS，summary `benchmark/logs/crash/phase4_part3_crash_summary_20260514_043536.tsv`
   - `pagecache_phase4` upstream xfstests：9 PASS / 0 FAIL / 4 NOTRUN，日志 `benchmark/logs/pagecache_phase4_20260513_091938.log`
3. 当前遗留项：
   - Phase 3 Step 6 普通 fio write 最新复跑值为 `39.18%`（1189 MB/s vs Linux 3035 MB/s），低于 `75%` hardening 红线，继续作为性能 blocker。
   - Phase 3 Step 6 普通 fio read 最新复跑值为 `127.06%`（5179 MB/s vs Linux 4076 MB/s），已通过。
   - Phase 4 PageCache benchmark：warm read 已体现收益；cold read、buffered write / dirty writeback 仍是 Step 7 hardening 点。
   - `8 workers / 64 rounds` 高压混合并发探针曾观察到偶发短读/extent mapping 风险，不作为当前功能验收基线。

补充：

1. 当前 ext4 fio 双项摘要复跑脚本是 `test/initramfs/src/benchmark/fio/run_ext4_summary.sh`。
2. 该脚本会顺序执行 `ext4_seq_write_bw` 与 `ext4_seq_read_bw`。
3. PageCache buffered fio 摘要脚本是 `test/initramfs/src/benchmark/fio/run_pagecache_buffered_summary.sh`。
4. 默认不输出 benchmark 过程日志，只在最后打印 `Asterinas`、`Linux`、`ratio` 摘要。

## 3. 机器与工具版本（实际检测值）

1. OS：Ubuntu 24.04.1 LTS
2. Kernel：`6.8.0-41-generic`
3. Rust：`rustc 1.94.0-nightly (2025-12-05)`
4. Cargo：`cargo 1.94.0-nightly`
5. cargo-osdk：`0.17.0`
6. QEMU：`8.2.2`
7. e2fsprogs（mke2fs/mkfs.ext4）：`1.47.0`
8. bash：`5.2.21`
9. ripgrep：`15.1.0`

## 4. 干净环境目录（当前保留）

仓库根：`/home/lby/os_com_codex/asterinas`

仓库内应提交并随 GitHub 同步的是源码、脚本、文档、xfstests case list / blocked list / dataset samples，以及少量明确保留的 benchmark 证据日志。以下目录是本机生成物或缓存，不要求推送，队友克隆后可重建：

1. 构建目录：`/home/lby/os_com_codex/asterinas/target_lby`
2. 日志目录：`/home/lby/os_com_codex/asterinas/benchmark/logs`
3. VDSO：`/home/lby/os_com_codex/asterinas/.local/linux_vdso`
4. xfstests 预构建目录：`/home/lby/os_com_codex/asterinas/.local/xfstests-prebuilt`
5. xfstests 源目录：`/home/lby/os_com_codex/asterinas/.local/xfstests-src`
6. 临时 benchmark target/cache：`.target_bench/`、`.cache/`

`.cache` 当前实际内容是 `linux_binary_cache/vmlinuz`（约 24 MiB），用于 fio benchmark 的 Linux 对照侧启动内核。各 fio summary 脚本会把容器内 `LINUX_DEPENDENCIES_DIR` 指到 `/root/asterinas/.cache/linux_binary_cache`；如果该文件不存在，`test/initramfs/src/benchmark/common/prepare_host.sh` 会从 `https://raw.githubusercontent.com/asterinas/linux_binary_cache/24db4ff/vmlinuz-6.16.0` 自动下载。也就是说 `.cache` 不需要进 Git，但队友机器需要能访问该 URL，或者手工预置同名文件。

当前仓库已跟踪的可复用 initramfs：

1. 基础 initramfs：`benchmark/assets/initramfs/initramfs_phase3.cpio.gz`
2. 当前推荐 initramfs：`benchmark/assets/initramfs/initramfs_phase4_part3.cpio.gz`

说明：Docker runner 会按需重打包 phase4_part3 initramfs；如只做功能回归，优先使用 `tools/ext4/run_phase4_in_docker.sh`。

## 5. 环境变量（统一口径）

```bash
cd /home/lby/os_com_codex/asterinas

export PATH=/home/lby/.local/bin:$PATH
export CARGO_TARGET_DIR=$(pwd)/target_lby
export VDSO_LIBRARY_DIR=$(pwd)/.local/linux_vdso
export BOOT_METHOD=qemu-direct
export OVMF=off
export RELEASE_LTO=1
export ENABLE_KVM=0
export NETDEV=user
export VHOST=off
export CONSOLE=ttyS0
```

Docker/KVM 复跑推荐变量：

```bash
export ENABLE_KVM=1
export BENCH_ENABLE_KVM=1
export BENCH_ASTER_NETDEV=tap
export BENCH_ASTER_VHOST=on
```

首次进入工作树或 toolchain component 缺失时，可先跑仓库自带的一键准备脚本：

```bash
cd /home/lby/os_com_codex/asterinas
./tools/setup_dev_env.sh
```

该脚本会安装 `rust-src`、`rustfmt`、`rustc-dev`、`llvm-tools-preview` 与内核 target，并准备 `.local/linux_vdso`。如果只想补 Rust 组件、不碰 VDSO，可使用：

```bash
./tools/setup_dev_env.sh --no-vdso
```

可选代理（仅下载超时时打开，Clash 7890）：

```bash
export http_proxy=http://127.0.0.1:7890
export https_proxy=http://127.0.0.1:7890
export all_proxy=socks5://127.0.0.1:7890
```

## 6. 依赖准备与校验

### 6.1 校验基础命令

```bash
command -v qemu-system-x86_64
command -v mkfs.ext4
command -v cargo
command -v rg
```

### 6.2 需要重建 xfstests 预构建时

```bash
cd /home/lby/os_com_codex/asterinas
tools/ext4/prepare_xfstests_prebuilt.sh \
  /home/lby/os_com_codex/asterinas/.local/xfstests-prebuilt \
  /home/lby/os_com_codex/asterinas/.local/xfstests-src
```

### 6.3 需要重建 part3 initramfs 时

```bash
cd /home/lby/os_com_codex/asterinas
tools/ext4/prepare_phase4_part3_initramfs.sh \
  /home/lby/os_com_codex/asterinas/.local/initramfs_phase3.cpio.gz \
  /home/lby/os_com_codex/asterinas/.local/initramfs_phase4_part3.cpio.gz
```

## 7. 运行命令（phase4）

### 7.1 全流程（part3 脚本）

```bash
cd /home/lby/os_com_codex/asterinas

env VDSO_LIBRARY_DIR=$(pwd)/.local/linux_vdso PATH=/home/lby/.local/bin:$PATH \
    CARGO_TARGET_DIR=$(pwd)/target_lby BOOT_METHOD=qemu-direct OVMF=off \
    RELEASE_LTO=1 ENABLE_KVM=0 NETDEV=user VHOST=off CONSOLE=ttyS0 \
    timeout 10800s tools/ext4/run_phase4_part3.sh
```

### 7.2 只跑 `generic/013`（定位）

```bash
cd /home/lby/os_com_codex/asterinas/kernel

timeout 1800s cargo osdk run \
  --kcmd-args='ostd.log_level=error' \
  --kcmd-args='console=ttyS0' \
  --kcmd-args='SYSCALL_TEST_SUITE=xfstests' \
  --kcmd-args='SYSCALL_TEST_WORKDIR=/ext4' \
  --kcmd-args='EXTRA_BLOCKLISTS_DIRS=' \
  --kcmd-args='XFSTESTS_MODE=phase4_good' \
  --kcmd-args='XFSTESTS_THRESHOLD_PERCENT=90' \
  --kcmd-args='XFSTESTS_RESULTS_DIR=' \
  --kcmd-args='XFSTESTS_TEST_DEV=/dev/vda' \
  --kcmd-args='XFSTESTS_SCRATCH_DEV=/dev/vdb' \
  --kcmd-args='XFSTESTS_TEST_DIR=/ext4_test' \
  --kcmd-args='XFSTESTS_SCRATCH_MNT=/ext4_scratch' \
  --kcmd-args='XFSTESTS_SKIP_MKFS=1' \
  --kcmd-args='XFSTESTS_SINGLE_TEST=generic/013' \
  --kcmd-args='XFSTESTS_CASE_TIMEOUT_SEC=600' \
  --init-args='/opt/syscall_test/run_syscall_test.sh' \
  --target-arch=x86_64 \
  --profile release-lto \
  --boot-method='qemu-direct' \
  --grub-boot-protocol=multiboot2 \
  --initramfs='../.local/initramfs_phase4_part3.cpio.gz'
```

## 8. Phase 3 fsync/flush 回归入口

### 8.1 Phase 3 专项（jbd_phase3_fsync_flush Docker mode）

```bash
cd /home/lby/os_com_codex/asterinas

PHASE4_DOCKER_MODE=jbd_phase3_fsync_flush \
ENABLE_KVM=1 \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
XFSTESTS_CASE_TIMEOUT_SEC=1200 \
XFSTESTS_RUN_TIMEOUT_SEC=5400 \
bash tools/ext4/run_phase4_in_docker.sh
```

说明：
- Tier 1 shutdown 用例（generic/043-049/052/054/055/388/392）现在用于验证真实 `EXT4_IOC_SHUTDOWN` + fsync/flush durability。
- Phase 3 收口口径为默认 2G scratch `11 PASS / 1 NOTRUN / 0 FAIL`；`generic/048` 需要 12G scratch 单点复跑。

### 8.2 Phase 3 host-crash 替代验证（jbd_phase3_host_crash Docker mode）

```bash
cd /home/lby/os_com_codex/asterinas

PHASE4_DOCKER_MODE=jbd_phase3_host_crash \
ENABLE_KVM=1 \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
KLOG_LEVEL=warn \
bash tools/ext4/run_phase4_in_docker.sh
```

说明：
- 该 mode 跑 4 个自研场景：`host_crash_fsync_size_durability`、`host_crash_fdatasync_metadata`、`host_crash_rename_fsync_dst`、`host_crash_concurrent_fsync`。
- runner 在 guest 内完成真实 `fsync` / `fdatasync` / 目录 `fsync` 后，等待 `EXT4_CRASH_PREPARE_DONE`，再杀 QEMU 并重挂载校验。
- 该证据覆盖 guest powercut + journal replay；host page cache 丢失 / dm-log-writes 级别证据需单独记录，不能混算。

### 8.3 fsync-heavy fio 预研（独立于 xfstests）

```bash
cd /home/lby/os_com_codex
KEEP_LOGS=1 bash ./asterinas/test/initramfs/src/benchmark/fio/run_write_16k_fsync4_summary.sh
```

用于暴露持久化语义，不作为普通吞吐宣传。

### 8.4 普通 ext4 fio 复跑

```bash
cd /home/lby/os_com_codex
KEEP_LOGS=1 bash ./asterinas/test/initramfs/src/benchmark/fio/run_ext4_summary.sh
```

2026-05-08 Step 6 结果：read `5179/4076=127.06%`，write `1189/3035=39.18%`。read 通过；write 低于 75% hardening 红线，已从 Phase 3 退场条件中移出，后续按独立性能 hardening 推进。

## 9. Phase 4 PageCache 入口

Phase 4 计划与 milestone：

- `docs/feature_pagecache_phase4_plan.md`
- `docs/feature_pagecache_phase4_milestone.md`

### 9.1 PageCache correctness

```bash
cd /home/lby/os_com_codex/asterinas
PHASE4_DOCKER_MODE=pagecache_phase4 \
ENABLE_KVM=1 \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
XFSTESTS_CASE_TIMEOUT_SEC=1200 \
bash tools/ext4/run_phase4_in_docker.sh
```

最新结果：`9 PASS / 0 FAIL / 4 NOTRUN`，日志 `benchmark/logs/pagecache_phase4_20260513_091938.log`。

### 9.2 PageCache benchmark A-E

A. lmbench_only：

```bash
cd /home/lby/os_com_codex/asterinas
KLOG_LEVEL=error \
PHASE4_DOCKER_MODE=lmbench_only \
ENABLE_KVM=1 \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
PERF_ROUNDS=1 \
PERF_CASE_TIMEOUT_SEC=600 \
bash tools/ext4/run_phase4_in_docker.sh
```

B/C/D. buffered fio cold/warm read 与 buffered write：

```bash
cd /home/lby/os_com_codex/asterinas
EXT4_DIRECT_READ_CACHE=0 \
BENCH_FIO_SIZE=1G \
LOG_LEVEL=error \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
bash test/initramfs/src/benchmark/fio/run_pagecache_buffered_summary.sh
```

E. O_DIRECT cache-off 守底：

```bash
cd /home/lby/os_com_codex/asterinas
EXT4_DIRECT_READ_CACHE=0 \
EXT4_PAGE_CACHE=0 \
KEEP_LOGS=1 \
LOG_LEVEL=error \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
bash test/initramfs/src/benchmark/fio/run_ext4_summary.sh
```

当前结果见 `benchmark/benchmark.md` 的 “PageCache Phase 4 benchmark A-E”。

PageCache 验收覆盖：

1. buffered read/write；
2. mmap read/write；
3. buffered write + O_DIRECT read；
4. O_DIRECT write + buffered read；
5. truncate shrink/extend + cached read；
6. dirty PageCache + fsync/fdatasync + remount/crash。

## 10. 判定口径

1. 看 case 结果：日志出现 `xfstests case done: generic/013 rc=0`
2. 看总结果：日志出现 `All syscall tests passed.`
3. 全量 `phase4_good` 看统计行：`phase4_good\tpass\tfail...`

## 11. 已知问题与规避

1. 当前推荐 Docker 封装链路；宿主机直跑按仓库脚本 + QEMU 作为排障辅助。
2. 曾出现“历史 root 权限污染目录”问题，已从仓库移出：
   - `/home/lby/os_com_codex/garbage/asterinas_target_root_polluted_20260407`
   - `/home/lby/os_com_codex/garbage/asterinas_osdk_target_root_polluted_20260407`
   - `/home/lby/os_com_codex/garbage/asterinas_target_lby_root_backup_20260407`
3. 如需彻底删掉上述垃圾隔离目录，需要 root 权限：

```bash
sudo rm -rf /home/lby/os_com_codex/garbage/asterinas_target_root_polluted_20260407 \
            /home/lby/os_com_codex/garbage/asterinas_osdk_target_root_polluted_20260407 \
            /home/lby/os_com_codex/garbage/asterinas_target_lby_root_backup_20260407
```

## 12. GitHub 克隆复跑清单

队友从 GitHub 克隆后，理论上不需要复制本机 `.local/`、`target_lby/`、`.target_bench/`、`.cache/`。这些都是生成物或缓存；其中 `.cache/linux_binary_cache/vmlinuz` 是 fio Linux 对照侧 kernel 缓存，可自动下载，也可离线手工预置。需要满足：

1. 宿主机安装 Docker，并能运行 privileged container；
2. 宿主机有 `/dev/kvm`，当前 benchmark 推荐 KVM；
3. 拉取 Docker 镜像 `asterinas/asterinas:0.17.0-20260227`；
4. clone 后在仓库根目录运行 `tools/setup_dev_env.sh`，或直接使用 Docker runner；
5. 如 xfstests 预构建缺失，runner 会通过仓库内脚本与 dataset 重新准备。
6. 如网络无法访问 GitHub raw，需提前准备 `.cache/linux_binary_cache/vmlinuz`，来源为 `asterinas/linux_binary_cache` 的 `vmlinuz-6.16.0`。

Docker 入口示例：

```bash
cd /home/lby/os_com_codex/asterinas
docker pull asterinas/asterinas:0.17.0-20260227

PHASE4_DOCKER_MODE=pagecache_phase4 \
ENABLE_KVM=1 \
BENCH_ENABLE_KVM=1 \
BENCH_ASTER_NETDEV=tap \
BENCH_ASTER_VHOST=on \
XFSTESTS_CASE_TIMEOUT_SEC=1200 \
bash tools/ext4/run_phase4_in_docker.sh
```

推送前必须确认这些 Phase 4 环境/测试资产已 `git add`，否则队友 clone 后缺入口：

```bash
git add environment.md docs/environment.md README.md \
  benchmark/benchmark.md docs/benchmark.md \
  docs/feature_pagecache_phase4_plan.md docs/feature_pagecache_phase4_milestone.md \
  Makefile \
  test/initramfs/src/benchmark/bench_linux_and_aster.sh \
  test/initramfs/src/benchmark/common/bench_runner.sh \
  test/initramfs/src/benchmark/fio/run_ext4_summary.sh \
  test/initramfs/src/benchmark/fio/run_pagecache_buffered_summary.sh \
  test/initramfs/src/benchmark/fio/ext4_buffered_seq_read_bw \
  test/initramfs/src/benchmark/fio/ext4_buffered_seq_write_bw \
  test/initramfs/src/syscall/xfstests/testcases/pagecache_phase4.list \
  test/initramfs/src/syscall/xfstests/blocked/pagecache_phase4_excluded.tsv \
  benchmark/datasets/xfstests/lists/pagecache_phase4.list \
  benchmark/datasets/xfstests/blocked/pagecache_phase4_excluded.tsv \
  benchmark/datasets/xfstests/samples/generic
```

不要提交：

```bash
.local/
target_lby/
.target_bench/
.cache/
result_*.json
```

benchmark 结果日志是否提交按团队协作需要决定；至少应保留 milestone / benchmark 文档中引用的关键 summary 路径，或者在提交说明里注明日志只在本机存在。

## 13. 一次性快速复现（最短路径）

```bash
cd /home/lby/os_com_codex/asterinas
export PATH=/home/lby/.local/bin:$PATH
export CARGO_TARGET_DIR=$(pwd)/target_lby
export VDSO_LIBRARY_DIR=$(pwd)/.local/linux_vdso

timeout 10800s tools/ext4/run_phase4_part3.sh
```

如果只做最小确认，先跑第 7.2 节单测 `generic/013`。
