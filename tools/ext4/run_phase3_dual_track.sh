#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd "${ROOT_DIR}"

: "${VDSO_LIBRARY_DIR:?VDSO_LIBRARY_DIR is required}"
: "${XFSTESTS_PREBUILT_DIR:?XFSTESTS_PREBUILT_DIR is required}"

export PATH="$HOME/.local/bin:${PATH}"
export RELEASE_LTO=${RELEASE_LTO:-1}
export ENABLE_KVM=${ENABLE_KVM:-0}
export NETDEV=${NETDEV:-user}
export VHOST=${VHOST:-off}
export CONSOLE=${CONSOLE:-ttyS0}

LOG_DIR=${LOG_DIR:-"${ROOT_DIR}/stage3_ext4_logs"}
mkdir -p "${LOG_DIR}"

run_phase3_base_round() {
  local round="$1"
  local log_file="${LOG_DIR}/xfstests_phase3_base_round${round}.log"
  echo "[RUN] phase3_base round-${round}"
  set +e
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
    >"${log_file}" 2>&1
  local rc=$?
  set -e

  local rate_line
  rate_line=$(grep -E "phase3_base\t[0-9]+\t[0-9]+\t[0-9]+\t[0-9]+\t[0-9]+\t[0-9.]+\t[0-9]+" "${log_file}" | tail -n 1 || true)
  echo "[DONE] phase3_base round-${round} rc=${rc} rate_line='${rate_line}'"
  return ${rc}
}

run_generic_quick() {
  local log_file="${LOG_DIR}/xfstests_generic_quick.log"
  echo "[RUN] generic_quick"
  set +e
  make run_kernel \
    AUTO_TEST=syscall \
    SYSCALL_TEST_SUITE=xfstests \
    SYSCALL_TEST_WORKDIR=/ext4 \
    XFSTESTS_MODE=generic_quick \
    >"${log_file}" 2>&1
  local rc=$?
  set -e
  local obs
  obs=$(grep -E "generic quick done|generic_quick\t" "${log_file}" | tail -n 2 || true)
  echo "[DONE] generic_quick rc=${rc}"
  echo "${obs}"
  return 0
}

run_lmbench_regression() {
  local summary="${LOG_DIR}/lmbench_phase3_regression.tsv"
  : >"${summary}"

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
    local log_file="${LOG_DIR}/${bench//\//_}.log"
    local timeout_s=420s
    if [[ "${bench}" == "lmbench/ext4_copy_files_bw" ]]; then
      timeout_s=700s
    fi

    mkfs.ext4 -F -b 4096 test/initramfs/build/ext2.img >/tmp/mkfs_ext4_phase3.log 2>&1

    set +e
    BENCHMARK="${bench}" timeout "${timeout_s}" make run_kernel >"${log_file}" 2>&1
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

    local status=FAIL
    if [[ -n "${key}" && "${rc}" -eq 0 ]]; then
      status=PASS
    fi

    printf "%s\t%s\trc=%s\t%s\n" "${bench}" "${status}" "${rc}" "${key}" >>"${summary}"
    echo "[DONE] ${bench} status=${status} rc=${rc}"
  done

  echo "===== LMbench summary ====="
  cat "${summary}"
}

run_phase3_base_round 1
run_phase3_base_round 2
run_generic_quick
run_lmbench_regression

echo "All phase3 dual-track steps finished. Logs: ${LOG_DIR}"
