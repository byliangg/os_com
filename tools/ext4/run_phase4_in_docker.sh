#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd "${ROOT_DIR}"

DOCKER_TAG=$(cat "${ROOT_DIR}/DOCKER_IMAGE_VERSION" 2>/dev/null || cat "${ROOT_DIR}/VERSION")
IMAGE_NAME=${ASTER_DOCKER_IMAGE:-"asterinas/asterinas:${DOCKER_TAG}"}
MODE=${PHASE4_DOCKER_MODE:-phase4_good}

CONTAINER_WORKDIR=${CONTAINER_WORKDIR:-/root/asterinas}
CONTAINER_TARGET_DIR=${CONTAINER_TARGET_DIR:-${CONTAINER_WORKDIR}/target_lby}
CONTAINER_VDSO_DIR=${CONTAINER_VDSO_DIR:-${CONTAINER_WORKDIR}/benchmark/assets/linux_vdso}
CONTAINER_BASE_INITRAMFS=${CONTAINER_BASE_INITRAMFS:-${CONTAINER_WORKDIR}/benchmark/assets/initramfs/initramfs_phase3.cpio.gz}
CONTAINER_INITRAMFS=${CONTAINER_INITRAMFS:-${CONTAINER_WORKDIR}/benchmark/assets/initramfs/initramfs_phase4_part3.cpio.gz}
CONTAINER_XFSTESTS_PREBUILT_DIR=${CONTAINER_XFSTESTS_PREBUILT_DIR:-${CONTAINER_WORKDIR}/benchmark/assets/xfstests-prebuilt}
CONTAINER_LOG_DIR=${CONTAINER_LOG_DIR:-${CONTAINER_WORKDIR}/benchmark/logs}

AUTO_PREPARE_XFSTESTS=${AUTO_PREPARE_XFSTESTS:-1}
PHASE4_GOOD_THRESHOLD=${PHASE4_GOOD_THRESHOLD:-90}
PHASE6_GOOD_THRESHOLD=${PHASE6_GOOD_THRESHOLD:-90}
CRASH_ROUNDS=${CRASH_ROUNDS:-2}
CRASH_PREPARE_WAIT_SEC=${CRASH_PREPARE_WAIT_SEC:-180}
XFSTESTS_SINGLE_TEST=${XFSTESTS_SINGLE_TEST:-}
XFSTESTS_IGNORE_STATIC_EXCLUDED_FOR_SINGLE=${XFSTESTS_IGNORE_STATIC_EXCLUDED_FOR_SINGLE:-0}
if [ -n "${XFSTESTS_CASE_TIMEOUT_SEC+x}" ]; then
  XFSTESTS_CASE_TIMEOUT_SEC="${XFSTESTS_CASE_TIMEOUT_SEC}"
else
  case "${MODE}" in
    phase6_only|phase6_with_guard)
      XFSTESTS_CASE_TIMEOUT_SEC=1200
      ;;
    *)
      XFSTESTS_CASE_TIMEOUT_SEC=600
      ;;
  esac
fi
XFSTESTS_TRACE_RUN=${XFSTESTS_TRACE_RUN:-0}
XFSTESTS_CHILD_XTRACE=${XFSTESTS_CHILD_XTRACE:-0}
XFSTESTS_XFS_IO_DEBUG=${XFSTESTS_XFS_IO_DEBUG:-0}
XFSTESTS_SPARSE_PROBE_LOG=${XFSTESTS_SPARSE_PROBE_LOG:-0}

if [ -n "${XFSTESTS_RUN_TIMEOUT_SEC+x}" ]; then
  XFSTESTS_RUN_TIMEOUT_SEC="${XFSTESTS_RUN_TIMEOUT_SEC}"
else
  case "${MODE}" in
    phase6_only|phase6_with_guard)
      XFSTESTS_RUN_TIMEOUT_SEC=5400
      ;;
    *)
      XFSTESTS_RUN_TIMEOUT_SEC=1800
      ;;
  esac
fi

BOOT_METHOD=${BOOT_METHOD:-qemu-direct}
OVMF=${OVMF:-off}
RELEASE_LTO=${RELEASE_LTO:-1}
ENABLE_KVM=${ENABLE_KVM:-0}
NETDEV=${NETDEV:-user}
VHOST=${VHOST:-off}
CONSOLE=${CONSOLE:-ttyS0}
KLOG_LEVEL=${KLOG_LEVEL:-error}

if ! command -v docker >/dev/null 2>&1; then
  echo "Error: docker not found." >&2
  exit 1
fi

if [ "${USE_PROXY:-0}" = "1" ]; then
  export http_proxy=${http_proxy:-http://127.0.0.1:7890}
  export https_proxy=${https_proxy:-http://127.0.0.1:7890}
  export all_proxy=${all_proxy:-socks5://127.0.0.1:7890}
fi

DOCKER_ENV_ARGS=(
  -e PHASE4_DOCKER_MODE="${MODE}"
  -e CONTAINER_WORKDIR="${CONTAINER_WORKDIR}"
  -e CONTAINER_TARGET_DIR="${CONTAINER_TARGET_DIR}"
  -e CONTAINER_VDSO_DIR="${CONTAINER_VDSO_DIR}"
  -e CONTAINER_BASE_INITRAMFS="${CONTAINER_BASE_INITRAMFS}"
  -e CONTAINER_INITRAMFS="${CONTAINER_INITRAMFS}"
  -e CONTAINER_XFSTESTS_PREBUILT_DIR="${CONTAINER_XFSTESTS_PREBUILT_DIR}"
  -e CONTAINER_LOG_DIR="${CONTAINER_LOG_DIR}"
  -e AUTO_PREPARE_XFSTESTS="${AUTO_PREPARE_XFSTESTS}"
  -e PHASE4_GOOD_THRESHOLD="${PHASE4_GOOD_THRESHOLD}"
  -e PHASE6_GOOD_THRESHOLD="${PHASE6_GOOD_THRESHOLD}"
  -e CRASH_ROUNDS="${CRASH_ROUNDS}"
  -e CRASH_PREPARE_WAIT_SEC="${CRASH_PREPARE_WAIT_SEC}"
  -e XFSTESTS_SINGLE_TEST="${XFSTESTS_SINGLE_TEST}"
  -e XFSTESTS_IGNORE_STATIC_EXCLUDED_FOR_SINGLE="${XFSTESTS_IGNORE_STATIC_EXCLUDED_FOR_SINGLE}"
  -e XFSTESTS_CASE_TIMEOUT_SEC="${XFSTESTS_CASE_TIMEOUT_SEC}"
  -e XFSTESTS_TRACE_RUN="${XFSTESTS_TRACE_RUN}"
  -e XFSTESTS_CHILD_XTRACE="${XFSTESTS_CHILD_XTRACE}"
  -e XFSTESTS_RUN_TIMEOUT_SEC="${XFSTESTS_RUN_TIMEOUT_SEC}"
  -e XFSTESTS_XFS_IO_DEBUG="${XFSTESTS_XFS_IO_DEBUG}"
  -e XFSTESTS_SPARSE_PROBE_LOG="${XFSTESTS_SPARSE_PROBE_LOG}"
  -e BOOT_METHOD="${BOOT_METHOD}"
  -e OVMF="${OVMF}"
  -e RELEASE_LTO="${RELEASE_LTO}"
  -e ENABLE_KVM="${ENABLE_KVM}"
  -e NETDEV="${NETDEV}"
  -e VHOST="${VHOST}"
  -e CONSOLE="${CONSOLE}"
  -e KLOG_LEVEL="${KLOG_LEVEL}"
)

for key in http_proxy https_proxy all_proxy HTTP_PROXY HTTPS_PROXY ALL_PROXY; do
  if [ -n "${!key:-}" ]; then
    DOCKER_ENV_ARGS+=(-e "${key}=${!key}")
  fi
done

CONTAINER_SCRIPT=$(cat <<'EOF'
set -euo pipefail

cd "${CONTAINER_WORKDIR}"
mkdir -p "${CONTAINER_LOG_DIR}"

if [ ! -f "${CONTAINER_BASE_INITRAMFS}" ]; then
  echo "Error: base initramfs missing: ${CONTAINER_BASE_INITRAMFS}" >&2
  exit 2
fi

if [ "${AUTO_PREPARE_XFSTESTS}" = "1" ] && [ ! -d "${CONTAINER_XFSTESTS_PREBUILT_DIR}/xfstests-dev" ]; then
  echo "[INFO] xfstests prebuilt missing, preparing in container..."
  tools/ext4/prepare_xfstests_prebuilt.sh "${CONTAINER_XFSTESTS_PREBUILT_DIR}" "${CONTAINER_WORKDIR}/benchmark/assets/xfstests-src"
fi

export PATH="/root/.cargo/bin:${PATH}"
if ! cargo osdk --version >/dev/null 2>&1; then
  echo "[INFO] cargo-osdk missing in container, creating workspace wrapper..."
  mkdir -p /root/.cargo/bin
  cat >/root/.cargo/bin/cargo-osdk <<'EOS'
#!/usr/bin/env bash
set -euo pipefail
ROOT=${ASTERINAS_ROOT:-/root/asterinas}
# Build cargo-osdk from workspace source once, then exec the binary directly.
# Using `cargo run` here can alter invocation context and cause build issues.
BIN="${ROOT}/target_lby/debug/cargo-osdk"
STAMP="${ROOT}/target_lby/.cargo_osdk_local_dev"
if [ ! -x "${BIN}" ] || [ ! -f "${STAMP}" ]; then
  # Ensure base-crate dependencies use local path crates (no crates.io duplicate ostd).
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
fi

tools/ext4/prepare_phase4_part3_initramfs.sh \
  "${CONTAINER_BASE_INITRAMFS}" \
  "${CONTAINER_INITRAMFS}"

run_part3_with_flags() {
  env \
    VDSO_LIBRARY_DIR="${CONTAINER_VDSO_DIR}" \
    CARGO_TARGET_DIR="${CONTAINER_TARGET_DIR}" \
    BOOT_METHOD="${BOOT_METHOD}" OVMF="${OVMF}" RELEASE_LTO="${RELEASE_LTO}" \
    ENABLE_KVM="${ENABLE_KVM}" NETDEV="${NETDEV}" VHOST="${VHOST}" CONSOLE="${CONSOLE}" \
    KLOG_LEVEL="${KLOG_LEVEL}" \
    LOG_DIR="${CONTAINER_LOG_DIR}" INITRAMFS_IMG="${CONTAINER_INITRAMFS}" \
    BASE_INITRAMFS="${CONTAINER_BASE_INITRAMFS}" PHASE4_GOOD_THRESHOLD="${PHASE4_GOOD_THRESHOLD}" \
    PHASE6_GOOD_THRESHOLD="${PHASE6_GOOD_THRESHOLD}" \
    XFSTESTS_SINGLE_TEST="${XFSTESTS_SINGLE_TEST}" \
    XFSTESTS_IGNORE_STATIC_EXCLUDED_FOR_SINGLE="${XFSTESTS_IGNORE_STATIC_EXCLUDED_FOR_SINGLE}" \
    XFSTESTS_CASE_TIMEOUT_SEC="${XFSTESTS_CASE_TIMEOUT_SEC}" \
    XFSTESTS_TRACE_RUN="${XFSTESTS_TRACE_RUN}" XFSTESTS_CHILD_XTRACE="${XFSTESTS_CHILD_XTRACE}" \
    XFSTESTS_RUN_TIMEOUT_SEC="${XFSTESTS_RUN_TIMEOUT_SEC}" XFSTESTS_XFS_IO_DEBUG="${XFSTESTS_XFS_IO_DEBUG}" \
    XFSTESTS_SPARSE_PROBE_LOG="${XFSTESTS_SPARSE_PROBE_LOG}" \
    RUN_CRASH_SUITE="${RUN_CRASH_SUITE}" RUN_PHASE4_GOOD="${RUN_PHASE4_GOOD}" \
    RUN_PHASE3_BASE="${RUN_PHASE3_BASE}" RUN_PHASE6_GOOD="${RUN_PHASE6_GOOD}" RUN_LMBENCH="${RUN_LMBENCH}" \
    tools/ext4/run_phase4_part3.sh
}

case "${PHASE4_DOCKER_MODE}" in
  phase4_good)
    echo "[INFO] mode=phase4_good (only xfstests phase4_good)"
    RUN_CRASH_SUITE=0 RUN_PHASE4_GOOD=1 RUN_PHASE3_BASE=0 RUN_PHASE6_GOOD=0 RUN_LMBENCH=0 run_part3_with_flags
    ;;
  phase3_only)
    echo "[INFO] mode=phase3_only (only xfstests phase3_base)"
    RUN_CRASH_SUITE=0 RUN_PHASE4_GOOD=0 RUN_PHASE3_BASE=1 RUN_PHASE6_GOOD=0 RUN_LMBENCH=0 run_part3_with_flags
    ;;
  lmbench_only)
    echo "[INFO] mode=lmbench_only (only lmbench regression)"
    RUN_CRASH_SUITE=0 RUN_PHASE4_GOOD=0 RUN_PHASE3_BASE=0 RUN_PHASE6_GOOD=0 RUN_LMBENCH=1 run_part3_with_flags
    ;;
  phase4_with_guard)
    echo "[INFO] mode=phase4_with_guard (phase4_good + phase3_base)"
    RUN_CRASH_SUITE=0 RUN_PHASE4_GOOD=1 RUN_PHASE3_BASE=1 RUN_PHASE6_GOOD=0 RUN_LMBENCH=0 run_part3_with_flags
    ;;
  phase6_only)
    echo "[INFO] mode=phase6_only (only xfstests phase6_good)"
    RUN_CRASH_SUITE=0 RUN_PHASE4_GOOD=0 RUN_PHASE3_BASE=0 RUN_PHASE6_GOOD=1 RUN_LMBENCH=0 run_part3_with_flags
    ;;
  phase6_with_guard)
    echo "[INFO] mode=phase6_with_guard (phase6_good + phase4_good + phase3_base)"
    RUN_CRASH_SUITE=0 RUN_PHASE4_GOOD=1 RUN_PHASE3_BASE=1 RUN_PHASE6_GOOD=1 RUN_LMBENCH=0 run_part3_with_flags
    ;;
  crash_only)
    echo "[INFO] mode=crash_only (only ext4 crash suite)"
    RUN_CRASH_SUITE=1 RUN_PHASE4_GOOD=0 RUN_PHASE3_BASE=0 RUN_PHASE6_GOOD=0 RUN_LMBENCH=0 run_part3_with_flags
    ;;
  part3_full)
    echo "[INFO] mode=part3_full (crash + phase4_good + phase3_base + lmbench)"
    RUN_CRASH_SUITE=1 RUN_PHASE4_GOOD=1 RUN_PHASE3_BASE=1 RUN_PHASE6_GOOD=0 RUN_LMBENCH=1 run_part3_with_flags
    ;;
  *)
    echo "Error: unsupported PHASE4_DOCKER_MODE=${PHASE4_DOCKER_MODE}" >&2
    echo "Supported: phase4_good | phase3_only | phase6_only | lmbench_only | phase4_with_guard | phase6_with_guard | crash_only | part3_full" >&2
    exit 3
    ;;
esac
EOF
)

echo "[INFO] image=${IMAGE_NAME}"
echo "[INFO] mode=${MODE}"
echo "[INFO] workdir=${ROOT_DIR} -> ${CONTAINER_WORKDIR}"

docker run --rm --privileged --network=host \
  -v /dev:/dev \
  -v "${ROOT_DIR}:${CONTAINER_WORKDIR}" \
  -w "${CONTAINER_WORKDIR}" \
  "${DOCKER_ENV_ARGS[@]}" \
  "${IMAGE_NAME}" \
  bash -lc "${CONTAINER_SCRIPT}"
