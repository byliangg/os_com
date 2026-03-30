#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd "${ROOT_DIR}"

: "${VDSO_LIBRARY_DIR:?VDSO_LIBRARY_DIR is required}"

export PATH="${HOME}/.local/bin:${PATH}"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-"${ROOT_DIR}/target_lby"}
export BOOT_METHOD=${BOOT_METHOD:-qemu-direct}
export OVMF=${OVMF:-off}
export RELEASE_LTO=${RELEASE_LTO:-1}
export ENABLE_KVM=${ENABLE_KVM:-0}
export NETDEV=${NETDEV:-user}
export VHOST=${VHOST:-off}
export CONSOLE=${CONSOLE:-ttyS0}

LOG_DIR=${LOG_DIR:-"${ROOT_DIR}/stage4_ext4_logs_part2"}
mkdir -p "${LOG_DIR}" "${LOG_DIR}/lmbench"

INITRAMFS_IMG=${INITRAMFS_IMG:-"${ROOT_DIR}/.local/initramfs_phase4_part2.cpio.gz"}
BASE_INITRAMFS=${BASE_INITRAMFS:-"${ROOT_DIR}/.local/initramfs_phase3.cpio.gz"}
PHASE4_GOOD_THRESHOLD=${PHASE4_GOOD_THRESHOLD:-90}

if [ ! -f "${INITRAMFS_IMG}" ]; then
  "${ROOT_DIR}/tools/ext4/prepare_phase4_part2_initramfs.sh" "${BASE_INITRAMFS}" "${INITRAMFS_IMG}"
fi

run_xfstests_mode() {
  local mode="$1"
  local threshold="$2"
  local log_file="$3"

  pkill -f qemu-system >/dev/null 2>&1 || true
  rm -f qemu.log kernel/qemu.log

  set +e
  timeout 1800s bash -lc "cd '${ROOT_DIR}/kernel' && cargo osdk run \
    --kcmd-args='ostd.log_level=error' \
    --kcmd-args='console=${CONSOLE}' \
    --kcmd-args='SYSCALL_TEST_SUITE=xfstests' \
    --kcmd-args='SYSCALL_TEST_WORKDIR=/ext4' \
    --kcmd-args='EXTRA_BLOCKLISTS_DIRS=' \
    --kcmd-args='XFSTESTS_MODE=${mode}' \
    --kcmd-args='XFSTESTS_THRESHOLD_PERCENT=${threshold}' \
    --kcmd-args='XFSTESTS_RESULTS_DIR=' \
    --kcmd-args='XFSTESTS_TEST_DEV=/dev/vda' \
    --kcmd-args='XFSTESTS_SCRATCH_DEV=/dev/vdb' \
    --kcmd-args='XFSTESTS_TEST_DIR=/ext4_test' \
    --kcmd-args='XFSTESTS_SCRATCH_MNT=/ext4_scratch' \
    --kcmd-args='XFSTESTS_SINGLE_TEST=' \
    --kcmd-args='XFSTESTS_CASE_TIMEOUT_SEC=' \
    --init-args='/opt/syscall_test/run_syscall_test.sh' \
    --target-arch=x86_64 \
    --profile release-lto \
    --boot-method='${BOOT_METHOD}' \
    --grub-boot-protocol=multiboot2 \
    --initramfs='${INITRAMFS_IMG}'" >"${log_file}" 2>&1
  local rc=$?
  set -e

  echo "[DONE] mode=${mode} rc=${rc} log=${log_file}"
  rg -n "mode\\tpass\\tfail|${mode}\\t|xfstests ${mode} passed|xfstests ${mode} failed|All syscall tests passed|Error: xfstests failed|xfstests case done" "${log_file}" | tail -n 60 || true
  if [ ${rc} -ne 0 ]; then
    return ${rc}
  fi
  return 0
}

run_lmbench_regression() {
  local summary="$1"
  : > "${summary}"

  local benches=(
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
    local timeout_s=420
    if [[ "${bench}" == "lmbench/ext4_copy_files_bw" ]]; then
      timeout_s=700
    fi

    local ts
    ts=$(date +%Y%m%d_%H%M%S)
    local log_file="${LOG_DIR}/lmbench/${bench//\//_}_${ts}.log"

    pkill -f qemu-system >/dev/null 2>&1 || true
    rm -f qemu.log kernel/qemu.log

    mkfs.ext4 -F -b 4096 test/initramfs/build/ext2.img >/tmp/mkfs_ext4_phase4_part2.log 2>&1

    set +e
    timeout "${timeout_s}s" bash -lc "cd '${ROOT_DIR}/kernel' && cargo osdk run \
      --kcmd-args='ostd.log_level=error' \
      --kcmd-args='console=${CONSOLE}' \
      --init-args='/benchmark/common/bench_runner.sh ${bench} asterinas' \
      --target-arch=x86_64 \
      --profile release-lto \
      --boot-method='${BOOT_METHOD}' \
      --grub-boot-protocol=multiboot2 \
      --initramfs='${INITRAMFS_IMG}'" >"${log_file}" 2>&1
    local rc=$?
    set -e

    local key=""
    case "${bench}" in
      lmbench/ext4_vfs_open_lat)  key=$(grep -E "Simple open/close" "${log_file}" | tail -n 1 || true) ;;
      lmbench/ext4_vfs_stat_lat)  key=$(grep -E "Simple stat" "${log_file}" | tail -n 1 || true) ;;
      lmbench/ext4_vfs_fstat_lat) key=$(grep -E "Simple fstat" "${log_file}" | tail -n 1 || true) ;;
      lmbench/ext4_vfs_read_lat)  key=$(grep -E "Simple read" "${log_file}" | tail -n 1 || true) ;;
      lmbench/ext4_vfs_write_lat) key=$(grep -E "Simple write" "${log_file}" | tail -n 1 || true) ;;
      lmbench/ext4_create_delete_files_0k_ops|lmbench/ext4_create_delete_files_10k_ops)
        key=$(grep -E "^[0-9]+k[[:space:]]+[0-9]+[[:space:]]+[0-9]+[[:space:]]+[0-9]+" "${log_file}" | tail -n 1 || true)
        ;;
      lmbench/ext4_copy_files_bw)
        key=$(grep -E "lmdd result: .* MB/sec" "${log_file}" | tail -n 1 || true)
        ;;
    esac

    local status="FAIL"
    if [ ${rc} -eq 0 ] && [ -n "${key}" ]; then
      status="PASS"
    fi

    printf "%s\t%s\trc=%s\t%s\tlog=%s\n" "${bench}" "${status}" "${rc}" "${key}" "${log_file}" >> "${summary}"
    echo "[DONE] ${bench} status=${status} rc=${rc}"
  done

  echo "===== LMbench summary ====="
  cat "${summary}"
}

TS=$(date +%Y%m%d_%H%M%S)
PHASE4_LOG="${LOG_DIR}/phase4_good_${TS}.log"
PHASE3_LOG="${LOG_DIR}/phase3_base_guard_${TS}.log"
LMB_SUMMARY="${LOG_DIR}/lmbench/phase4_part2_lmbench_summary_${TS}.tsv"

run_xfstests_mode phase4_good "${PHASE4_GOOD_THRESHOLD}" "${PHASE4_LOG}"
run_xfstests_mode phase3_base 90 "${PHASE3_LOG}"
run_lmbench_regression "${LMB_SUMMARY}"

echo "[DONE] phase4_part2 run finished"
echo "phase4_good_log=${PHASE4_LOG}"
echo "phase3_base_log=${PHASE3_LOG}"
echo "lmbench_summary=${LMB_SUMMARY}"
