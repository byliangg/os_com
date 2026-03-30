#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ROOT_DIR=$(cd -- "${SCRIPT_DIR}/../.." && pwd)

IMAGE="asterinas/asterinas:0.17.0-20260227"
TIMEOUT_S="14400s"
USE_PROXY=0
PROXY_HTTP="http://127.0.0.1:7890"
PROXY_HTTPS="http://127.0.0.1:7890"
PROXY_ALL="socks5://127.0.0.1:7890"

usage() {
  cat <<USAGE
Usage: $(basename "$0") [options]

Options:
  --image <name>        Docker image (default: ${IMAGE})
  --timeout <dur>       Timeout passed to phase6 runner (default: ${TIMEOUT_S})
  --proxy               Enable Clash proxy env with 127.0.0.1:7890
  --proxy-http <url>    Override http_proxy value
  --proxy-https <url>   Override https_proxy value
  --proxy-all <url>     Override all_proxy value
  -h, --help            Show help
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --image)
      IMAGE="$2"
      shift 2
      ;;
    --timeout)
      TIMEOUT_S="$2"
      shift 2
      ;;
    --proxy)
      USE_PROXY=1
      shift
      ;;
    --proxy-http)
      PROXY_HTTP="$2"
      shift 2
      ;;
    --proxy-https)
      PROXY_HTTPS="$2"
      shift 2
      ;;
    --proxy-all)
      PROXY_ALL="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if ! command -v docker >/dev/null 2>&1; then
  echo "docker not found in PATH" >&2
  exit 127
fi

DOCKER_ARGS=(
  run --rm
  --privileged
  --network=host
  -e "HOST_UID=$(id -u)"
  -e "HOST_GID=$(id -g)"
  -v /dev:/dev
  -v "${ROOT_DIR}:/root/asterinas"
  -w /root/asterinas
)

for passthrough_var in \
  JOURNAL_ITERS \
  PHASE4_GOOD_THRESHOLD \
  XFSTESTS_SINGLE_TEST \
  XFSTESTS_TRACE_RUN \
  XFSTESTS_CASE_TIMEOUT_SEC \
  PHASE6_STAGES \
  INITRAMFS_IMG \
  BASE_INITRAMFS \
  LOG_DIR; do
  if [ -n "${!passthrough_var:-}" ]; then
    DOCKER_ARGS+=( -e "${passthrough_var}=${!passthrough_var}" )
  fi
done

if [[ -t 0 && -t 1 ]]; then
  DOCKER_ARGS+=( -it )
fi

if [[ ${USE_PROXY} -eq 1 ]]; then
  DOCKER_ARGS+=(
    -e "http_proxy=${PROXY_HTTP}"
    -e "https_proxy=${PROXY_HTTPS}"
    -e "all_proxy=${PROXY_ALL}"
  )
fi

RUN_CMD=$(cat <<'CMD'
set -euo pipefail
cd /root/asterinas
if ! command -v cargo-osdk >/dev/null 2>&1; then
  OSDK_LOCAL_DEV=1 cargo install --path osdk --locked --force
fi
export PATH="$HOME/.local/bin:$PATH"
export CARGO_TARGET_DIR=$(pwd)/target_lby
export VDSO_LIBRARY_DIR=$(pwd)/.local/linux_vdso
export BOOT_METHOD=qemu-direct
export ENABLE_KVM=0
export RELEASE_LTO=1
export OVMF=off
export NETDEV=user
export VHOST=off
export CONSOLE=ttyS0
TIMEOUT_PLACEHOLDER
if command -v chown >/dev/null 2>&1; then
  chown -R "${HOST_UID}:${HOST_GID}" stage6_ext4_logs_part1 >/dev/null 2>&1 || true
fi
CMD
)
RUN_CMD=${RUN_CMD/TIMEOUT_PLACEHOLDER/timeout ${TIMEOUT_S} tools/ext4/run_phase6_part1.sh}

echo "[INFO] repo=${ROOT_DIR}"
echo "[INFO] image=${IMAGE}"
echo "[INFO] timeout=${TIMEOUT_S}"
if [[ ${USE_PROXY} -eq 1 ]]; then
  echo "[INFO] proxy enabled (${PROXY_HTTP})"
fi

docker "${DOCKER_ARGS[@]}" "${IMAGE}" bash -lc "${RUN_CMD}"
