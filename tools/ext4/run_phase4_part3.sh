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

LOG_DIR=${LOG_DIR:-"${ROOT_DIR}/benchmark/logs"}
mkdir -p "${LOG_DIR}" "${LOG_DIR}/lmbench" "${LOG_DIR}/crash"

INITRAMFS_IMG=${INITRAMFS_IMG:-"${ROOT_DIR}/benchmark/assets/initramfs/initramfs_phase4_part3.cpio.gz"}
BASE_INITRAMFS=${BASE_INITRAMFS:-"${ROOT_DIR}/benchmark/assets/initramfs/initramfs_phase3.cpio.gz"}
PHASE4_GOOD_THRESHOLD=${PHASE4_GOOD_THRESHOLD:-90}
CRASH_ROUNDS=${CRASH_ROUNDS:-2}
CRASH_PREPARE_WAIT_SEC=${CRASH_PREPARE_WAIT_SEC:-180}
XFSTESTS_SINGLE_TEST=${XFSTESTS_SINGLE_TEST:-}
XFSTESTS_CASE_TIMEOUT_SEC=${XFSTESTS_CASE_TIMEOUT_SEC:-600}
XFSTESTS_TRACE_RUN=${XFSTESTS_TRACE_RUN:-0}
XFSTESTS_CHILD_XTRACE=${XFSTESTS_CHILD_XTRACE:-0}
XFSTESTS_RUN_TIMEOUT_SEC=${XFSTESTS_RUN_TIMEOUT_SEC:-1800}
XFSTESTS_XFS_IO_DEBUG=${XFSTESTS_XFS_IO_DEBUG:-0}
XFSTESTS_SPARSE_PROBE_LOG=${XFSTESTS_SPARSE_PROBE_LOG:-0}
XFSTESTS_TEST_IMG_SIZE=${XFSTESTS_TEST_IMG_SIZE:-2G}
XFSTESTS_SCRATCH_IMG_SIZE=${XFSTESTS_SCRATCH_IMG_SIZE:-2G}
RUN_CRASH_SUITE=${RUN_CRASH_SUITE:-1}
RUN_PHASE4_GOOD=${RUN_PHASE4_GOOD:-1}
RUN_PHASE3_BASE=${RUN_PHASE3_BASE:-1}
RUN_PHASE6_GOOD=${RUN_PHASE6_GOOD:-0}
RUN_LMBENCH=${RUN_LMBENCH:-1}
KLOG_LEVEL=${KLOG_LEVEL:-error}
PHASE6_GOOD_THRESHOLD=${PHASE6_GOOD_THRESHOLD:-90}

if [ ! -f "${INITRAMFS_IMG}" ]; then
  "${ROOT_DIR}/tools/ext4/prepare_phase4_part3_initramfs.sh" "${BASE_INITRAMFS}" "${INITRAMFS_IMG}"
fi

run_crash_prepare_once() {
  local scenario="$1"
  local hold_op="$2"
  local round="$3"
  local log_file="$4"

  pkill -f qemu-system >/dev/null 2>&1 || true
  rm -f qemu.log kernel/qemu.log
  mkfs.ext4 -F -b 4096 test/initramfs/build/ext2.img >/tmp/mkfs_ext4_phase4_part3_crash.log 2>&1

  timeout 1200s bash -lc "cd '${ROOT_DIR}/kernel' && cargo osdk run \
    --kcmd-args='ostd.log_level=warn' \
    --kcmd-args='console=${CONSOLE}' \
    --kcmd-args='SYSCALL_TEST_SUITE=ext4_crash' \
    --kcmd-args='EXT4_CRASH_PHASE=prepare' \
    --kcmd-args='EXT4_CRASH_SCENARIO=${scenario}' \
    --kcmd-args='EXT4_CRASH_SKIP_MKFS=1' \
    --kcmd-args='ext4fs.replay_hold=1' \
    --kcmd-args='ext4fs.replay_hold_op=${hold_op}' \
    --init-args='/opt/syscall_test/run_syscall_test.sh' \
    --target-arch=x86_64 \
    --profile release-lto \
    --boot-method='${BOOT_METHOD}' \
    --grub-boot-protocol=multiboot2 \
    --initramfs='${INITRAMFS_IMG}'" >"${log_file}" 2>&1 &
  local run_pid=$!

  local marker="replay hold point reached for op=${hold_op}"
  local marker_seen=0
  local i=0
  while [ "${i}" -lt "${CRASH_PREPARE_WAIT_SEC}" ]; do
    if rg -q "${marker}" "${log_file}" 2>/dev/null; then
      marker_seen=1
      break
    fi
    if ! kill -0 "${run_pid}" >/dev/null 2>&1; then
      break
    fi
    sleep 1
    i=$((i + 1))
  done

  if [ "${marker_seen}" -ne 1 ]; then
    kill -TERM "${run_pid}" >/dev/null 2>&1 || true
    wait "${run_pid}" >/dev/null 2>&1 || true
    echo "[FAIL] crash prepare marker not observed: scenario=${scenario} round=${round}" >&2
    tail -n 120 "${log_file}" >&2 || true
    return 1
  fi

  # Simulate sudden power loss.
  pkill -f qemu-system >/dev/null 2>&1 || true
  sleep 1
  kill -TERM "${run_pid}" >/dev/null 2>&1 || true
  wait "${run_pid}" >/dev/null 2>&1 || true
  echo "[DONE] crash prepare killed: scenario=${scenario} round=${round} log=${log_file}"
}

run_crash_verify_once() {
  local scenario="$1"
  local round="$2"
  local log_file="$3"

  pkill -f qemu-system >/dev/null 2>&1 || true
  rm -f qemu.log kernel/qemu.log

  set +e
  timeout 900s bash -lc "cd '${ROOT_DIR}/kernel' && cargo osdk run \
    --kcmd-args='ostd.log_level=warn' \
    --kcmd-args='console=${CONSOLE}' \
    --kcmd-args='SYSCALL_TEST_SUITE=ext4_crash' \
    --kcmd-args='EXT4_CRASH_PHASE=verify' \
    --kcmd-args='EXT4_CRASH_SCENARIO=${scenario}' \
    --init-args='/opt/syscall_test/run_syscall_test.sh' \
    --target-arch=x86_64 \
    --profile release-lto \
    --boot-method='${BOOT_METHOD}' \
    --grub-boot-protocol=multiboot2 \
    --initramfs='${INITRAMFS_IMG}'" >"${log_file}" 2>&1
  local rc=$?
  set -e

  if [ ${rc} -ne 0 ]; then
    echo "[FAIL] crash verify returned rc=${rc}: scenario=${scenario} round=${round}" >&2
    tail -n 120 "${log_file}" >&2 || true
    return ${rc}
  fi

  if ! rg -q "EXT4_CRASH_VERIFY_PASS scenario=${scenario}" "${log_file}"; then
    echo "[FAIL] crash verify marker missing: scenario=${scenario} round=${round}" >&2
    tail -n 120 "${log_file}" >&2 || true
    return 1
  fi

  echo "[DONE] crash verify passed: scenario=${scenario} round=${round} log=${log_file}"
}

run_crash_suite() {
  local summary="$1"
  : > "${summary}"
  printf "round\tscenario\thold_op\tprepare_log\tverify_log\tresult\n" >> "${summary}"

  local round
  for round in $(seq 1 "${CRASH_ROUNDS}"); do
    for item in create_write:write rename:rename truncate_append:write; do
      local scenario="${item%%:*}"
      local hold_op="${item##*:}"
      local prepare_log="${LOG_DIR}/crash/${scenario}_prepare_r${round}.log"
      local verify_log="${LOG_DIR}/crash/${scenario}_verify_r${round}.log"

      run_crash_prepare_once "${scenario}" "${hold_op}" "${round}" "${prepare_log}"
      run_crash_verify_once "${scenario}" "${round}" "${verify_log}"
      printf "%s\t%s\t%s\t%s\t%s\tPASS\n" \
        "${round}" "${scenario}" "${hold_op}" "${prepare_log}" "${verify_log}" >> "${summary}"
    done
  done

  echo "===== Crash summary ====="
  cat "${summary}"
}

run_xfstests_mode() {
  local mode="$1"
  local threshold="$2"
  local log_file="$3"

  pkill -f qemu-system >/dev/null 2>&1 || true
  rm -f qemu.log kernel/qemu.log
  truncate -s "${XFSTESTS_TEST_IMG_SIZE}" test/initramfs/build/ext2.img
  truncate -s "${XFSTESTS_SCRATCH_IMG_SIZE}" test/initramfs/build/exfat.img
  mkfs.ext4 -F -b 4096 test/initramfs/build/ext2.img >/tmp/mkfs_ext4_phase4_part3_xfstests_test.log 2>&1
  mkfs.ext4 -F -b 4096 test/initramfs/build/exfat.img >/tmp/mkfs_ext4_phase4_part3_xfstests_scratch.log 2>&1

  set +e
  timeout "${XFSTESTS_RUN_TIMEOUT_SEC}s" bash -lc "cd '${ROOT_DIR}/kernel' && cargo osdk run \
    --kcmd-args='ostd.log_level=${KLOG_LEVEL}' \
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
    --kcmd-args='XFSTESTS_SKIP_MKFS=1' \
    --kcmd-args='XFSTESTS_SINGLE_TEST=${XFSTESTS_SINGLE_TEST}' \
    --kcmd-args='XFSTESTS_CASE_TIMEOUT_SEC=${XFSTESTS_CASE_TIMEOUT_SEC}' \
    --kcmd-args='XFSTESTS_TRACE_RUN=${XFSTESTS_TRACE_RUN}' \
    --kcmd-args='XFSTESTS_CHILD_XTRACE=${XFSTESTS_CHILD_XTRACE}' \
    --kcmd-args='XFSTESTS_XFS_IO_DEBUG=${XFSTESTS_XFS_IO_DEBUG}' \
    --kcmd-args='XFSTESTS_SPARSE_PROBE_LOG=${XFSTESTS_SPARSE_PROBE_LOG}' \
    --init-args='/opt/syscall_test/run_syscall_test.sh' \
    --target-arch=x86_64 \
    --profile release-lto \
    --boot-method='${BOOT_METHOD}' \
    --grub-boot-protocol=multiboot2 \
    --initramfs='${INITRAMFS_IMG}'" >"${log_file}" 2>&1
  local rc=$?
  set -e

  echo "[DONE] mode=${mode} rc=${rc} log=${log_file}"
  if command -v rg >/dev/null 2>&1; then
    rg -n "mode\\tpass\\tfail|${mode}\\t|xfstests ${mode} passed|xfstests ${mode} failed|All syscall tests passed|Error: xfstests failed|xfstests case done" "${log_file}" | tail -n 80 || true
  else
    grep -nE "mode[[:space:]]+pass[[:space:]]+fail|${mode}[[:space:]]|xfstests ${mode} passed|xfstests ${mode} failed|All syscall tests passed|Error: xfstests failed|xfstests case done" "${log_file}" | tail -n 80 || true
  fi
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
    if [[ "${bench}" == "lmbench/ext4_vfs_open_lat" ]]; then
      timeout_s=900
    fi
    if [[ "${bench}" == "lmbench/ext4_copy_files_bw" ]]; then
      timeout_s=700
    fi

    local ts
    ts=$(date +%Y%m%d_%H%M%S)
    local log_file="${LOG_DIR}/lmbench/${bench//\//_}_${ts}.log"

    pkill -f qemu-system >/dev/null 2>&1 || true
    rm -f qemu.log kernel/qemu.log

    mkfs.ext4 -F -b 4096 test/initramfs/build/ext2.img >/tmp/mkfs_ext4_phase4_part3.log 2>&1

    set +e
    timeout "${timeout_s}s" bash -lc "cd '${ROOT_DIR}/kernel' && cargo osdk run \
      --kcmd-args='ostd.log_level=${KLOG_LEVEL}' \
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
CRASH_SUMMARY="${LOG_DIR}/crash/phase4_part3_crash_summary_${TS}.tsv"
PHASE4_LOG="${LOG_DIR}/phase4_good_${TS}.log"
PHASE3_LOG="${LOG_DIR}/phase3_base_guard_${TS}.log"
PHASE6_LOG="${LOG_DIR}/phase6_good_${TS}.log"
LMB_SUMMARY="${LOG_DIR}/lmbench/phase4_part3_lmbench_summary_${TS}.tsv"

ANY_STAGE_RAN=0

if [ "${RUN_CRASH_SUITE}" = "1" ]; then
  run_crash_suite "${CRASH_SUMMARY}"
  ANY_STAGE_RAN=1
else
  echo "[SKIP] crash suite disabled (RUN_CRASH_SUITE=${RUN_CRASH_SUITE})"
fi

if [ "${RUN_PHASE4_GOOD}" = "1" ]; then
  run_xfstests_mode phase4_good "${PHASE4_GOOD_THRESHOLD}" "${PHASE4_LOG}"
  ANY_STAGE_RAN=1
else
  echo "[SKIP] phase4_good disabled (RUN_PHASE4_GOOD=${RUN_PHASE4_GOOD})"
fi

if [ "${RUN_PHASE3_BASE}" = "1" ]; then
  run_xfstests_mode phase3_base 90 "${PHASE3_LOG}"
  ANY_STAGE_RAN=1
else
  echo "[SKIP] phase3_base disabled (RUN_PHASE3_BASE=${RUN_PHASE3_BASE})"
fi

if [ "${RUN_PHASE6_GOOD}" = "1" ]; then
  run_xfstests_mode phase6_good "${PHASE6_GOOD_THRESHOLD}" "${PHASE6_LOG}"
  ANY_STAGE_RAN=1
else
  echo "[SKIP] phase6_good disabled (RUN_PHASE6_GOOD=${RUN_PHASE6_GOOD})"
fi

if [ "${RUN_LMBENCH}" = "1" ]; then
  run_lmbench_regression "${LMB_SUMMARY}"
  ANY_STAGE_RAN=1
else
  echo "[SKIP] lmbench disabled (RUN_LMBENCH=${RUN_LMBENCH})"
fi

if [ "${ANY_STAGE_RAN}" -ne 1 ]; then
  echo "Error: no stage selected. Enable at least one of RUN_CRASH_SUITE/RUN_PHASE4_GOOD/RUN_PHASE3_BASE/RUN_PHASE6_GOOD/RUN_LMBENCH." >&2
  exit 2
fi

echo "[DONE] phase4_part3 run finished"
if [ "${RUN_CRASH_SUITE}" = "1" ]; then
  echo "crash_summary=${CRASH_SUMMARY}"
else
  echo "crash_summary=<disabled>"
fi
if [ "${RUN_PHASE4_GOOD}" = "1" ]; then
  echo "phase4_good_log=${PHASE4_LOG}"
else
  echo "phase4_good_log=<disabled>"
fi
if [ "${RUN_PHASE3_BASE}" = "1" ]; then
  echo "phase3_base_log=${PHASE3_LOG}"
else
  echo "phase3_base_log=<disabled>"
fi
if [ "${RUN_PHASE6_GOOD}" = "1" ]; then
  echo "phase6_good_log=${PHASE6_LOG}"
else
  echo "phase6_good_log=<disabled>"
fi
if [ "${RUN_LMBENCH}" = "1" ]; then
  echo "lmbench_summary=${LMB_SUMMARY}"
else
  echo "lmbench_summary=<disabled>"
fi
