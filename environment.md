# Asterinas EXT4 Phase4 Environment (Clean)

更新时间：2026-04-07（Asia/Shanghai）

## 1. 目标与范围

这份文档只覆盖 `stage4` 的 **本机直跑环境**（非 Docker）。
目标是让别人拿到仓库后，按本文命令可直接复现当前阶段结论。

## 2. 当前结论（截至 2026-04-07）

1. 当前工作分支：`stage4`
2. 当前最好结果：
   - `generic/013` 单测可通过（`rc=0`）
   - 日志：`/home/lby/os_com_codex/asterinas/stage4_ext4_logs_restore_runp3_v10/phase4_good_single_generic013_afterbounds_20260407_114356.log`
3. 当前未打通项：
   - `phase4_good` 全量仍失败（`2 pass / 18 fail`）
   - 关键日志：`/home/lby/os_com_codex/asterinas/stage4_ext4_logs_restore_runp3_v10/phase4_good_full_afterbounds_20260407_115151.log`

补充：

1. 当前 ext4 fio 双项摘要复跑脚本是 `test/initramfs/src/benchmark/fio/run_ext4_summary.sh`。
2. 该脚本会顺序执行 `ext4_seq_write_bw` 与 `ext4_seq_read_bw`。
3. 默认不输出 benchmark 过程日志，只在最后打印 `Asterinas`、`Linux`、`ratio` 摘要。

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

保留并使用：

1. 构建目录：`/home/lby/os_com_codex/asterinas/target_lby`
2. 日志目录：`/home/lby/os_com_codex/asterinas/stage4_ext4_logs_restore_runp3_v10`
3. 基础 initramfs：`/home/lby/os_com_codex/asterinas/.local/initramfs_phase3.cpio.gz`
4. 当前推荐 initramfs：
   - 实体：`/home/lby/os_com_codex/asterinas/.local/initramfs_phase4_part3_stage4fix_v19.cpio.gz`
   - 入口（符号链接）：`/home/lby/os_com_codex/asterinas/.local/initramfs_phase4_part3.cpio.gz`
5. VDSO：`/home/lby/os_com_codex/asterinas/.local/linux_vdso`
6. xfstests 预构建目录：`/home/lby/os_com_codex/asterinas/.local/xfstests-prebuilt`
7. xfstests 源目录：`/home/lby/os_com_codex/asterinas/.local/xfstests-src`

说明：已经清理历史 `target_*` 和旧 `stage4/5/6` 日志，只保留上述最小集合。

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

## 8. 判定口径

1. 看 case 结果：日志出现 `xfstests case done: generic/013 rc=0`
2. 看总结果：日志出现 `All syscall tests passed.`
3. 全量 `phase4_good` 看统计行：`phase4_good\tpass\tfail...`

## 9. 已知问题与规避

1. 本环境当前不是 Docker 封装链路，按仓库脚本 + QEMU 直跑。
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

## 10. 一次性快速复现（最短路径）

```bash
cd /home/lby/os_com_codex/asterinas
export PATH=/home/lby/.local/bin:$PATH
export CARGO_TARGET_DIR=$(pwd)/target_lby
export VDSO_LIBRARY_DIR=$(pwd)/.local/linux_vdso

timeout 10800s tools/ext4/run_phase4_part3.sh
```

如果只做最小确认，先跑第 7.2 节单测 `generic/013`。
