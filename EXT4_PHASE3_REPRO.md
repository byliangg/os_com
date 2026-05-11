# EXT4 Phase3 Repro Guide

## 1. 前置条件

1. 在仓库根目录执行：`/path/to/asterinas`。
2. 已配置 `VDSO_LIBRARY_DIR`。
3. 已准备 `XFSTESTS_PREBUILT_DIR`，目录需包含：
   - `xfstests-dev/`（内部有 `check`）
   - 可选 `tools/bin/`（xfstests 依赖命令）

示例：

```bash
cd /path/to/asterinas
export VDSO_LIBRARY_DIR=$(pwd)/.local/linux_vdso
export XFSTESTS_PREBUILT_DIR=/abs/path/to/xfstests-prebuilt
export PATH=$HOME/.local/bin:$PATH
```

## 2. 阶段3主验收（phase3_base）

执行两轮，要求都 `>=90%`。

```bash
cd /path/to/asterinas

# round-1
make run_kernel \
  AUTO_TEST=syscall \
  SYSCALL_TEST_SUITE=xfstests \
  SYSCALL_TEST_WORKDIR=/ext4 \
  XFSTESTS_MODE=phase3_base \
  XFSTESTS_THRESHOLD_PERCENT=90 \
  XFSTESTS_TEST_DEV=/dev/vda \
  XFSTESTS_SCRATCH_DEV=/dev/vdb \
  XFSTESTS_TEST_DIR=/ext4_test \
  XFSTESTS_SCRATCH_MNT=/ext4_scratch \
  RELEASE_LTO=1 ENABLE_KVM=0 NETDEV=user VHOST=off CONSOLE=ttyS0

# round-2
make run_kernel \
  AUTO_TEST=syscall \
  SYSCALL_TEST_SUITE=xfstests \
  SYSCALL_TEST_WORKDIR=/ext4 \
  XFSTESTS_MODE=phase3_base \
  XFSTESTS_THRESHOLD_PERCENT=90 \
  XFSTESTS_TEST_DEV=/dev/vda \
  XFSTESTS_SCRATCH_DEV=/dev/vdb \
  XFSTESTS_TEST_DIR=/ext4_test \
  XFSTESTS_SCRATCH_MNT=/ext4_scratch \
  RELEASE_LTO=1 ENABLE_KVM=0 NETDEV=user VHOST=off CONSOLE=ttyS0
```

主验收关键日志关键词：

1. `mode\tpass\tfail\tnotrun...`
2. `xfstests phase3_base passed: pass_rate=...`

## 3. 观测轨（generic_quick，非阻塞）

```bash
make run_kernel \
  AUTO_TEST=syscall \
  SYSCALL_TEST_SUITE=xfstests \
  SYSCALL_TEST_WORKDIR=/ext4 \
  XFSTESTS_MODE=generic_quick \
  RELEASE_LTO=1 ENABLE_KVM=0 NETDEV=user VHOST=off CONSOLE=ttyS0
```

## 4. LMbench 8项回归

```bash
cd /path/to/asterinas

benches=(
  lmbench/ext4_vfs_open_lat
  lmbench/ext4_vfs_stat_lat
  lmbench/ext4_vfs_fstat_lat
  lmbench/ext4_vfs_read_lat
  lmbench/ext4_vfs_write_lat
  lmbench/ext4_create_delete_files_0k_ops
  lmbench/ext4_create_delete_files_10k_ops
  lmbench/ext4_copy_files_bw
)

for bench in "${benches[@]}"; do
  mkfs.ext4 -F -b 4096 test/initramfs/build/ext2.img >/tmp/mkfs_ext4_phase3.log 2>&1

  timeout_s=420s
  if [[ "$bench" == "lmbench/ext4_copy_files_bw" ]]; then
    timeout_s=700s
  fi

  BENCHMARK="$bench" \
  RELEASE_LTO=1 ENABLE_KVM=0 NETDEV=user VHOST=off CONSOLE=ttyS0 \
  timeout "$timeout_s" make run_kernel

done
```
