#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd "${ROOT_DIR}"

SRC_IMG=${1:-"${ROOT_DIR}/benchmark/assets/initramfs/initramfs_phase3.cpio.gz"}
OUT_IMG=${2:-"${ROOT_DIR}/benchmark/assets/initramfs/initramfs_phase4_part1.cpio.gz"}

if [ ! -f "${SRC_IMG}" ]; then
  echo "Error: source initramfs not found: ${SRC_IMG}" >&2
  exit 1
fi

WORK_DIR=$(mktemp -d)
trap 'rm -rf "${WORK_DIR}"' EXIT
ROOTFS_DIR="${WORK_DIR}/rootfs"
mkdir -p "${ROOTFS_DIR}"

echo "[INFO] Extracting ${SRC_IMG} ..."
(
  cd "${ROOTFS_DIR}"
  gzip -dc "${SRC_IMG}" | cpio -idmu --quiet
)

echo "[INFO] Patching xfstests scripts for phase4_part1 ..."
install -D -m 0755 test/initramfs/src/syscall/xfstests/run_xfstests_test.sh \
  "${ROOTFS_DIR}/opt/xfstests/run_xfstests_test.sh"
install -D -m 0644 test/initramfs/src/syscall/xfstests/testcases/phase3_base.list \
  "${ROOTFS_DIR}/opt/xfstests/testcases/phase3_base.list"
install -D -m 0644 test/initramfs/src/syscall/xfstests/testcases/phase4_good.list \
  "${ROOTFS_DIR}/opt/xfstests/testcases/phase4_good.list"
install -D -m 0644 test/initramfs/src/syscall/xfstests/testcases/phase6_good.list \
  "${ROOTFS_DIR}/opt/xfstests/testcases/phase6_good.list"
install -D -m 0644 test/initramfs/src/syscall/xfstests/blocked/phase3_excluded.tsv \
  "${ROOTFS_DIR}/opt/xfstests/blocked/phase3_excluded.tsv"
install -D -m 0644 test/initramfs/src/syscall/xfstests/blocked/phase4_excluded.tsv \
  "${ROOTFS_DIR}/opt/xfstests/blocked/phase4_excluded.tsv"
install -D -m 0644 test/initramfs/src/syscall/xfstests/blocked/phase6_excluded.tsv \
  "${ROOTFS_DIR}/opt/xfstests/blocked/phase6_excluded.tsv"

mkdir -p "$(dirname "${OUT_IMG}")"
echo "[INFO] Repacking to ${OUT_IMG} ..."
(
  cd "${ROOTFS_DIR}"
  find . -print0 | cpio --null -ov --format=newc 2>/dev/null | gzip -n > "${OUT_IMG}"
)

echo "[DONE] Prepared phase4_part1 initramfs: ${OUT_IMG}"
