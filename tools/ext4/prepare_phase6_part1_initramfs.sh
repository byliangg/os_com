#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd "${ROOT_DIR}"

SRC_IMG=${1:-"${ROOT_DIR}/.local/initramfs_phase3.cpio.gz"}
OUT_IMG=${2:-"${ROOT_DIR}/.local/initramfs_phase6_part1.cpio.gz"}
XFSTESTS_PREBUILT_DIR=${XFSTESTS_PREBUILT_DIR:-"${ROOT_DIR}/.local/xfstests-prebuilt"}

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
trap 'chmod -R u+w "${WORK_DIR}" >/dev/null 2>&1 || true; rm -rf "${WORK_DIR}"' EXIT
ROOTFS_DIR="${WORK_DIR}/rootfs"
mkdir -p "${ROOTFS_DIR}"

echo "[INFO] Extracting ${SRC_IMG} ..."
(
  cd "${ROOTFS_DIR}"
  gzip -dc "${SRC_IMG}" | cpio -idmu --quiet
)

# Some extracted system directories can be read-only for the current user.
# Relax write bits for this staging tree so we can inject helper binaries.
chmod -R u+w "${ROOTFS_DIR}/usr" "${ROOTFS_DIR}/lib" "${ROOTFS_DIR}/lib64" >/dev/null 2>&1 || true

echo "[INFO] Patching syscall suites for phase6_part1 ..."
install -D -m 0755 test/initramfs/src/syscall/run_syscall_test.sh \
  "${ROOTFS_DIR}/opt/syscall_test/run_syscall_test.sh"

install -D -m 0755 test/initramfs/src/syscall/xfstests/run_xfstests_test.sh \
  "${ROOTFS_DIR}/opt/xfstests/run_xfstests_test.sh"
install -D -m 0644 test/initramfs/src/syscall/xfstests/testcases/phase3_base.list \
  "${ROOTFS_DIR}/opt/xfstests/testcases/phase3_base.list"
install -D -m 0644 test/initramfs/src/syscall/xfstests/testcases/phase4_good.list \
  "${ROOTFS_DIR}/opt/xfstests/testcases/phase4_good.list"
install -D -m 0644 test/initramfs/src/syscall/xfstests/blocked/phase3_excluded.tsv \
  "${ROOTFS_DIR}/opt/xfstests/blocked/phase3_excluded.tsv"
install -D -m 0644 test/initramfs/src/syscall/xfstests/blocked/phase4_excluded.tsv \
  "${ROOTFS_DIR}/opt/xfstests/blocked/phase4_excluded.tsv"

if [ -d "${XFSTESTS_PREBUILT_DIR}/xfstests-dev" ]; then
  echo "[INFO] Injecting xfstests payload from ${XFSTESTS_PREBUILT_DIR} ..."
  rm -rf "${ROOTFS_DIR}/opt/xfstests/xfstests-dev"
  cp -a "${XFSTESTS_PREBUILT_DIR}/xfstests-dev" "${ROOTFS_DIR}/opt/xfstests/xfstests-dev"
  if [ -d "${XFSTESTS_PREBUILT_DIR}/tools" ]; then
    rm -rf "${ROOTFS_DIR}/opt/xfstests/tools"
    cp -a "${XFSTESTS_PREBUILT_DIR}/tools" "${ROOTFS_DIR}/opt/xfstests/tools"
  fi
else
  echo "[WARN] xfstests payload missing at ${XFSTESTS_PREBUILT_DIR}; xfstests run may fail." >&2
fi

ensure_xfstests_group_lists() {
  local xfstests_dev_dir="$1"
  local tests_dir="${xfstests_dev_dir}/tests"
  local mkgroupfile="${xfstests_dev_dir}/tools/mkgroupfile"
  local missing_count=0
  local generated_count=0

  [ -d "${tests_dir}" ] || return 0
  if [ ! -x "${mkgroupfile}" ]; then
    echo "[WARN] xfstests mkgroupfile missing at ${mkgroupfile}; skip group.list generation." >&2
    return 0
  fi

  for test_dir in "${tests_dir}"/*; do
    [ -d "${test_dir}" ] || continue
    if [ ! -s "${test_dir}/group.list" ]; then
      missing_count=$((missing_count + 1))
      if (cd "${test_dir}" && ../../tools/mkgroupfile group.list >/dev/null 2>&1); then
        generated_count=$((generated_count + 1))
      else
        echo "[WARN] failed to generate ${test_dir}/group.list" >&2
      fi
    fi
  done

  if [ "${missing_count}" -gt 0 ]; then
    echo "[INFO] xfstests group.list generated ${generated_count}/${missing_count}"
  fi
}

ensure_xfstests_group_lists "${ROOTFS_DIR}/opt/xfstests/xfstests-dev"

# xfstests prebuilt tools/bin uses absolute symlinks to /usr/sbin/* e2fsprogs
# commands. The minimal initramfs may not contain those binaries. Inject host
# e2fsprogs + runtime shared libs so mkfs/fsck helpers are executable in guest.
copy_host_binary_and_libs() {
  local bin="$1"
  [ -n "${bin}" ] || return 0
  [ -x "${bin}" ] || return 0

  local dst="${ROOTFS_DIR}${bin}"
  mkdir -p "$(dirname "${dst}")"
  cp -aL "${bin}" "${dst}"

  while IFS= read -r dep; do
    [ -n "${dep}" ] || continue
    [ -e "${dep}" ] || continue
    local dep_dst="${ROOTFS_DIR}${dep}"
    mkdir -p "$(dirname "${dep_dst}")"
    cp -aL "${dep}" "${dep_dst}"
  done < <(
    ldd "${bin}" 2>/dev/null | awk '
      $1 ~ /^\// { print $1; next }
      $2 == "=>" && $3 ~ /^\// { print $3; next }
    ' | sort -u
  )
}

inject_host_tool_by_name() {
  local name="$1"
  local path
  path=$(command -v "${name}" 2>/dev/null || true)
  if [ -n "${path}" ]; then
    copy_host_binary_and_libs "${path}"
  fi
}

inject_host_tool_by_name mkfs.ext4
inject_host_tool_by_name mke2fs
inject_host_tool_by_name e2fsck
inject_host_tool_by_name dumpe2fs
inject_host_tool_by_name blkid

install -D -m 0755 test/initramfs/src/syscall/ext4_crash/run_ext4_crash_test.sh \
  "${ROOTFS_DIR}/opt/ext4_crash/run_ext4_crash_test.sh"
install -D -m 0755 test/initramfs/src/syscall/ext4_journal/run_ext4_journal_test.sh \
  "${ROOTFS_DIR}/opt/ext4_journal/run_ext4_journal_test.sh"

# Ensure `bash` is discoverable in PATH for xfstests check scripts.
# The base initramfs already carries nix-store bash, but `/bin/bash` is absent.
if [ -d "${ROOTFS_DIR}/opt/xfstests/tools/bin" ]; then
  chmod -R u+w "${ROOTFS_DIR}/opt/xfstests/tools/bin" >/dev/null 2>&1 || true
  rm -f "${ROOTFS_DIR}/opt/xfstests/tools/bin/bash"
  cat > "${ROOTFS_DIR}/opt/xfstests/tools/bin/bash" <<'EOF'
#!/bin/sh
set -eu
for cand in /nix/store/*-bash-*/bin/bash; do
  if [ -x "${cand}" ]; then
    exec "${cand}" "$@"
  fi
done
echo "bash wrapper: no nix-store bash found" >&2
exit 127
EOF
  chmod 0755 "${ROOTFS_DIR}/opt/xfstests/tools/bin/bash"
fi

mkdir -p "$(dirname "${OUT_IMG}")"
echo "[INFO] Repacking to ${OUT_IMG} ..."
(
  cd "${ROOTFS_DIR}"
  find . -print0 | cpio --null -ov --format=newc 2>/dev/null | gzip -n > "${OUT_IMG}"
)

echo "[DONE] Prepared phase6_part1 initramfs: ${OUT_IMG}"
