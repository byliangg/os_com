# Ext4 Phase2 Repro (LMbench)

## Prerequisites
- Linux host with `mkfs.ext4` (`e2fsprogs`) and QEMU runtime dependencies.
- Run all commands at repository root (`asterinas/`).

## Full 8-benchmark run
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

## Pass criterion
- `phase2_ext4_all8.tsv` shows all 8 items with `PASS` and `rc=0`.
- Key output lines should appear in logs:
  - `Simple open/close`
  - `Simple stat`
  - `Simple fstat`
  - `Simple read`
  - `Simple write`
  - `lat_fs` result lines for `0k` and `10k`
  - `lmdd result: ... MB/sec`
