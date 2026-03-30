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

LOG_DIR=${LOG_DIR:-"${ROOT_DIR}/stage6_ext4_logs_part1"}
mkdir -p "${LOG_DIR}" "${LOG_DIR}/lmbench"

INITRAMFS_IMG=${INITRAMFS_IMG:-"${ROOT_DIR}/.local/initramfs_phase6_part1.cpio.gz"}
BASE_INITRAMFS=${BASE_INITRAMFS:-"${ROOT_DIR}/.local/initramfs_phase3.cpio.gz"}
PHASE4_GOOD_THRESHOLD=${PHASE4_GOOD_THRESHOLD:-90}
JOURNAL_ITERS=${JOURNAL_ITERS:-100}
XFSTESTS_SINGLE_TEST=${XFSTESTS_SINGLE_TEST:-}
XFSTESTS_TRACE_RUN=${XFSTESTS_TRACE_RUN:-0}
XFSTESTS_CASE_TIMEOUT_SEC=${XFSTESTS_CASE_TIMEOUT_SEC:-0}
PHASE6_STAGES=${PHASE6_STAGES:-journal,phase4,phase3,lmbench}

stage_enabled() {
  local stage="$1"
  case ",${PHASE6_STAGES}," in
    *,"${stage}",*) return 0 ;;
    *) return 1 ;;
  esac
}

has_pattern() {
  local pattern="$1"
  local file="$2"
  if command -v rg >/dev/null 2>&1; then
    rg -q "${pattern}" "${file}" 2>/dev/null
  else
    grep -qE "${pattern}" "${file}" 2>/dev/null
  fi
}

print_matches() {
  local pattern="$1"
  local file="$2"
  local lines="$3"
  if command -v rg >/dev/null 2>&1; then
    rg -n "${pattern}" "${file}" | tail -n "${lines}" || true
  else
    grep -nE "${pattern}" "${file}" | tail -n "${lines}" || true
  fi
}

if [ ! -f "${INITRAMFS_IMG}" ]; then
  "${ROOT_DIR}/tools/ext4/prepare_phase6_part1_initramfs.sh" "${BASE_INITRAMFS}" "${INITRAMFS_IMG}"
fi

run_ext4_journal_suite() {
  local iters="$1"
  local log_file="$2"

  pkill -f qemu-system >/dev/null 2>&1 || true
  rm -f qemu.log kernel/qemu.log
  mkfs.ext4 -F -b 4096 test/initramfs/build/ext2.img >/tmp/mkfs_ext4_phase6_part1_journal.log 2>&1

  set +e
  timeout 1800s bash -lc "cd '${ROOT_DIR}/kernel' && cargo osdk run \
    --kcmd-args='ostd.log_level=error' \
    --kcmd-args='console=${CONSOLE}' \
    --kcmd-args='SYSCALL_TEST_SUITE=ext4_journal' \
    --kcmd-args='EXT4_JOURNAL_ITERS=${iters}' \
    --kcmd-args='EXT4_JOURNAL_TEST_DEV=/dev/vda' \
    --kcmd-args='EXT4_JOURNAL_MNT=/ext4_journal_test' \
    --kcmd-args='EXT4_JOURNAL_SKIP_MKFS=1' \
    --init-args='/opt/syscall_test/run_syscall_test.sh' \
    --target-arch=x86_64 \
    --profile release-lto \
    --boot-method='${BOOT_METHOD}' \
    --grub-boot-protocol=multiboot2 \
    --initramfs='${INITRAMFS_IMG}'" >"${log_file}" 2>&1
  local rc=$?
  set -e

  echo "[DONE] ext4_journal iters=${iters} rc=${rc} log=${log_file}"
  print_matches "EXT4_JOURNAL_PASS|EXT4_JOURNAL_FAIL|All syscall tests passed" "${log_file}" 40
  if [ ${rc} -ne 0 ]; then
    return ${rc}
  fi
  if ! has_pattern "EXT4_JOURNAL_PASS iters=${iters}" "${log_file}"; then
    echo "[FAIL] ext4_journal pass marker missing (iters=${iters})" >&2
    tail -n 120 "${log_file}" >&2 || true
    return 1
  fi
  return 0
}

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
    --kcmd-args='XFSTESTS_SINGLE_TEST=${XFSTESTS_SINGLE_TEST}' \
    --kcmd-args='XFSTESTS_TRACE_RUN=${XFSTESTS_TRACE_RUN}' \
    --kcmd-args='XFSTESTS_CASE_TIMEOUT_SEC=${XFSTESTS_CASE_TIMEOUT_SEC}' \
    --init-args='/opt/syscall_test/run_syscall_test.sh' \
    --target-arch=x86_64 \
    --profile release-lto \
    --boot-method='${BOOT_METHOD}' \
    --grub-boot-protocol=multiboot2 \
    --initramfs='${INITRAMFS_IMG}'" >"${log_file}" 2>&1
  local rc=$?
  set -e

  echo "[DONE] mode=${mode} rc=${rc} log=${log_file}"
  print_matches "mode\\tpass\\tfail|${mode}\\t|xfstests ${mode} passed|xfstests ${mode} failed|All syscall tests passed|Error: xfstests failed|xfstests case done" "${log_file}" 80
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

    mkfs.ext4 -F -b 4096 test/initramfs/build/ext2.img >/tmp/mkfs_ext4_phase6_part1.log 2>&1

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
JOURNAL_LOG="${LOG_DIR}/journal_tx_${TS}.log"
PHASE4_LOG="${LOG_DIR}/phase4_good_${TS}.log"
PHASE3_LOG="${LOG_DIR}/phase3_base_guard_${TS}.log"
LMB_SUMMARY="${LOG_DIR}/lmbench/phase6_part1_lmbench_summary_${TS}.tsv"

if stage_enabled journal; then
  run_ext4_journal_suite "${JOURNAL_ITERS}" "${JOURNAL_LOG}"
else
  JOURNAL_LOG=""
  echo "[SKIP] ext4_journal"
fi

if stage_enabled phase4; then
  run_xfstests_mode phase4_good "${PHASE4_GOOD_THRESHOLD}" "${PHASE4_LOG}"
else
  PHASE4_LOG=""
  echo "[SKIP] xfstests phase4_good"
fi

if stage_enabled phase3; then
  run_xfstests_mode phase3_base 90 "${PHASE3_LOG}"
else
  PHASE3_LOG=""
  echo "[SKIP] xfstests phase3_base"
fi

if stage_enabled lmbench; then
  run_lmbench_regression "${LMB_SUMMARY}"
else
  LMB_SUMMARY=""
  echo "[SKIP] lmbench"
fi

echo "[DONE] phase6_part1 run finished"
if [ -n "${JOURNAL_LOG}" ]; then
  echo "journal_log=${JOURNAL_LOG}"
fi
if [ -n "${PHASE4_LOG}" ]; then
  echo "phase4_good_log=${PHASE4_LOG}"
fi
if [ -n "${PHASE3_LOG}" ]; then
  echo "phase3_base_log=${PHASE3_LOG}"
fi
if [ -n "${LMB_SUMMARY}" ]; then
  echo "lmbench_summary=${LMB_SUMMARY}"
fi
