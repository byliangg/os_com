#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd "${ROOT_DIR}"

SRC_IMG=${1:-"${ROOT_DIR}/benchmark/assets/initramfs/initramfs_phase3.cpio.gz"}
OUT_IMG=${2:-"${ROOT_DIR}/benchmark/assets/initramfs/initramfs_phase4_part3.cpio.gz"}
XFSTESTS_PREBUILT_DIR=${XFSTESTS_PREBUILT_DIR:-"${ROOT_DIR}/benchmark/assets/xfstests-prebuilt"}

case "${SRC_IMG}" in
  /*) ;;
  *) SRC_IMG="${ROOT_DIR}/${SRC_IMG}" ;;
esac
case "${OUT_IMG}" in
  /*) ;;
  *) OUT_IMG="${ROOT_DIR}/${OUT_IMG}" ;;
esac

if [ ! -f "${SRC_IMG}" ]; then
  echo "Error: source initramfs not found: ${SRC_IMG}" >&2
  exit 1
fi

WORK_DIR=$(mktemp -d)
trap 'chmod -R u+w "${WORK_DIR}" 2>/dev/null || true; rm -rf "${WORK_DIR}" 2>/dev/null || true' EXIT
ROOTFS_DIR="${WORK_DIR}/rootfs"
mkdir -p "${ROOTFS_DIR}"

echo "[INFO] Extracting ${SRC_IMG} ..."
(
  cd "${ROOTFS_DIR}"
  gzip -dc "${SRC_IMG}" | cpio -idmu --quiet
)

echo "[INFO] Patching syscall/xfstests/ext4_crash scripts for phase4_part3 ..."
install -D -m 0755 test/initramfs/src/syscall/run_syscall_test.sh \
  "${ROOTFS_DIR}/opt/syscall_test/run_syscall_test.sh"
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
install -D -m 0755 test/initramfs/src/syscall/ext4_crash/run_ext4_crash_test.sh \
  "${ROOTFS_DIR}/opt/ext4_crash/run_ext4_crash_test.sh"

if [ -d "${XFSTESTS_PREBUILT_DIR}/xfstests-dev" ]; then
  echo "[INFO] Injecting xfstests prebuilt from ${XFSTESTS_PREBUILT_DIR} ..."
  mkdir -p "${ROOTFS_DIR}/opt/xfstests"
  rm -rf "${ROOTFS_DIR}/opt/xfstests/xfstests-dev" "${ROOTFS_DIR}/opt/xfstests/tools"
  cp -a "${XFSTESTS_PREBUILT_DIR}/xfstests-dev" "${ROOTFS_DIR}/opt/xfstests/"
  if [ -d "${XFSTESTS_PREBUILT_DIR}/tools" ]; then
    cp -a "${XFSTESTS_PREBUILT_DIR}/tools" "${ROOTFS_DIR}/opt/xfstests/"
  fi
else
  echo "[WARN] xfstests prebuilt missing: ${XFSTESTS_PREBUILT_DIR}/xfstests-dev" >&2
fi

mkdir -p "$(dirname "${OUT_IMG}")"
echo "[INFO] Repacking to ${OUT_IMG} ..."
(
  cd "${ROOTFS_DIR}"
  find . -print0 | cpio --null -ov --format=newc 2>/dev/null | gzip -n > "${OUT_IMG}"
)

echo "[DONE] Prepared phase4_part3 initramfs: ${OUT_IMG}"
