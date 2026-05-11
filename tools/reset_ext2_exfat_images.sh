#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
BUILD_DIR=${1:-"${ROOT_DIR}/test/initramfs/build"}

EXT2_IMG="${BUILD_DIR}/ext2.img"
EXFAT_IMG="${BUILD_DIR}/exfat.img"
EXT2_SIZE=${EXT2_SIZE:-2G}
EXFAT_SIZE=${EXFAT_SIZE:-64M}

docker_exec() {
  local cmd="$1"
  local tag
  tag=$(cat "${ROOT_DIR}/DOCKER_IMAGE_VERSION" 2>/dev/null || cat "${ROOT_DIR}/VERSION")
  local image="${ASTER_DOCKER_IMAGE:-asterinas/asterinas:${tag}}"
  docker run --rm \
    -v "${ROOT_DIR}:/root/asterinas" \
    -w /root/asterinas \
    "${image}" \
    bash -lc "${cmd}"
}

mkfs_ext2_image() {
  if command -v mke2fs >/dev/null 2>&1; then
    mke2fs -t ext2 -F "${EXT2_IMG}" >/dev/null
    return 0
  fi
  if ! command -v docker >/dev/null 2>&1; then
    echo "Error: neither mke2fs nor docker is available." >&2
    exit 1
  fi
  local rel
  rel=$(realpath --relative-to="${ROOT_DIR}" "${EXT2_IMG}")
  docker_exec "mke2fs -t ext2 -F '/root/asterinas/${rel}' >/dev/null"
}

mkfs_exfat_image() {
  if command -v mkfs.exfat >/dev/null 2>&1; then
    mkfs.exfat "${EXFAT_IMG}" >/dev/null
    return 0
  fi
  if ! command -v docker >/dev/null 2>&1; then
    echo "Error: neither mkfs.exfat nor docker is available." >&2
    exit 1
  fi
  local rel
  rel=$(realpath --relative-to="${ROOT_DIR}" "${EXFAT_IMG}")
  docker_exec "mkfs.exfat '/root/asterinas/${rel}' >/dev/null"
}

mkdir -p "${BUILD_DIR}"
rm -f "${EXT2_IMG}" "${EXFAT_IMG}"

echo "[INFO] Rebuilding ${EXT2_IMG} as ext2 (${EXT2_SIZE})"
fallocate -l "${EXT2_SIZE}" "${EXT2_IMG}"
mkfs_ext2_image

echo "[INFO] Rebuilding ${EXFAT_IMG} as exfat (${EXFAT_SIZE})"
fallocate -l "${EXFAT_SIZE}" "${EXFAT_IMG}"
mkfs_exfat_image

echo "[DONE] Rebuilt ext2/exfat images under ${BUILD_DIR}"
file -sL "${EXT2_IMG}" "${EXFAT_IMG}" || true
