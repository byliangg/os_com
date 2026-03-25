# Asterinas EXT4 阶段2测试 README

## 1. 测试范围

本文用于复现 Asterinas 上 EXT4 集成的阶段2功能基准验证。

覆盖的 8 项 benchmark：

1. `lmbench/ext4_vfs_open_lat`
2. `lmbench/ext4_vfs_stat_lat`
3. `lmbench/ext4_vfs_fstat_lat`
4. `lmbench/ext4_vfs_read_lat`
5. `lmbench/ext4_vfs_write_lat`
6. `lmbench/ext4_create_delete_files_0k_ops`
7. `lmbench/ext4_create_delete_files_10k_ops`
8. `lmbench/ext4_copy_files_bw`

## 2. 仓库状态说明

- `ext4_rs` 已内嵌到 Asterinas 工作区：
  - `third_party/ext4_rs`
- 不再依赖外部同级目录 `../ext4_rs`。

## 3. 前置条件

- Linux 主机
- 已安装 `mkfs.ext4`（`e2fsprogs`）
- 已具备 QEMU 运行依赖
- 所有命令在仓库根目录执行：

```bash
cd /path/to/asterinas
```

如遇下载或网络慢，可选代理：

```bash
export http_proxy=http://127.0.0.1:7890
export https_proxy=http://127.0.0.1:7890
export all_proxy=socks5://127.0.0.1:7890
```

## 4. 单项冒烟测试

```bash
cd /path/to/asterinas

mkfs.ext4 -F -b 4096 test/initramfs/build/ext2.img

env PATH=$HOME/.local/bin:$PATH \
    CARGO_TARGET_DIR=$(pwd)/target_lby \
    VDSO_LIBRARY_DIR=$(pwd)/.local/linux_vdso \
    BOOT_METHOD=qemu-direct \
    BENCHMARK=lmbench/ext4_vfs_open_lat \
    RELEASE_LTO=1 ENABLE_KVM=0 NETDEV=user VHOST=off CONSOLE=ttyS0 \
    timeout 420s make run_kernel
```

## 5. 全量 8 项测试

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

mkdir -p stage2_ext4_logs
summary=stage2_ext4_logs/phase2_ext4_all8.tsv
: > "$summary"

for bench in "${benches[@]}"; do
  log="stage2_ext4_logs/${bench//\//_}.log"
  if [[ "$bench" == "lmbench/ext4_copy_files_bw" ]]; then
    t=700s
  else
    t=420s
  fi

  mkfs.ext4 -F -b 4096 test/initramfs/build/ext2.img >/tmp/mkfs_ext4_phase2.log 2>&1

  set +e
  env PATH=$HOME/.local/bin:$PATH \
      CARGO_TARGET_DIR=$(pwd)/target_lby \
      VDSO_LIBRARY_DIR=$(pwd)/.local/linux_vdso \
      BOOT_METHOD=qemu-direct \
      BENCHMARK="$bench" \
      RELEASE_LTO=1 ENABLE_KVM=0 NETDEV=user VHOST=off CONSOLE=ttyS0 \
      timeout "$t" make run_kernel >"$log" 2>&1
  rc=$?
  set -e

  if [ $rc -eq 0 ]; then
    status=PASS
  else
    status=FAIL
  fi

  printf "%s\t%s\trc=%s\n" "$bench" "$status" "$rc" >> "$summary"
  echo "[DONE] $bench status=$status rc=$rc"
done

cat "$summary"
```

## 6. 通过标准

`stage2_ext4_logs/phase2_ext4_all8.tsv` 中 8 项都满足：

- `status=PASS`
- `rc=0`

日志中应出现以下关键字：

1. `Simple open/close`
2. `Simple stat`
3. `Simple fstat`
4. `Simple read`
5. `Simple write`
6. `lat_fs` 的 `0k` / `10k` 结果行
7. `lmdd result: ... MB/sec`

## 7. 说明与排障

1. `ext4_copy_files_bw` 最慢；在 TCG（无 KVM）模式下，`700s` 超时是正常配置。
2. 若主机有 `/dev/kvm` 权限，可改为 `ENABLE_KVM=1` 以加速。
3. 每项前重新 `mkfs.ext4` 可以避免镜像脏状态影响结果。
4. `target_*`、`.local/`、`stage2_ext4_logs/` 属于本地产物，不应作为源码变更提交。
