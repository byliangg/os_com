#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -eu

TEST_DEV=${EXT4_JOURNAL_TEST_DEV:-/dev/vda}
MNT_DIR=${EXT4_JOURNAL_MNT:-/ext4_journal_test}
ITERS=${EXT4_JOURNAL_ITERS:-100}
SKIP_MKFS=${EXT4_JOURNAL_SKIP_MKFS:-1}
CASE_DIR_NAME=${EXT4_JOURNAL_CASE_DIR:-phase6_part1_journal}
CASE_DIR=${MNT_DIR}/${CASE_DIR_NAME}

fail() {
    echo "EXT4_JOURNAL_FAIL reason=$1" >&2
    exit 1
}

dump_diag() {
    echo "EXT4_JOURNAL_DIAG mounts:" >&2
    grep -E " ${MNT_DIR} " /proc/mounts >&2 || true
    echo "EXT4_JOURNAL_DIAG dirs:" >&2
    ls -ld "${MNT_DIR}" "${CASE_DIR}" >&2 || true
    ls -la "${CASE_DIR}" >&2 || true
}

ensure_mountpoint_unmounted() {
    if awk -v t="${MNT_DIR}" '$2==t { found=1; exit 0 } END { exit(found ? 0 : 1) }' /proc/mounts; then
        umount "${MNT_DIR}" >/dev/null 2>&1 || true
    fi
}

unmount_dev_if_needed() {
    dev="$1"
    if [ -z "${dev}" ]; then
        return 0
    fi
    awk -v d="${dev}" '$1==d { print $2 }' /proc/mounts | while IFS= read -r mnt; do
        [ -n "${mnt}" ] || continue
        [ "${mnt}" = "/" ] && continue
        umount "${mnt}" >/dev/null 2>&1 || true
    done
}

mount_test_dev() {
    [ -b "${TEST_DEV}" ] || fail "device not found: ${TEST_DEV}"
    mkdir -p "${MNT_DIR}"
    ensure_mountpoint_unmounted
    unmount_dev_if_needed "${TEST_DEV}"
    mount -t ext4 "${TEST_DEV}" "${MNT_DIR}" || fail "mount failed"
}

mkfs_ext4_if_needed() {
    dev="$1"
    mkfs_log="/tmp/ext4_journal_mkfs_$(basename "${dev}" | tr -c '[:alnum:]' '_').log"
    : >"${mkfs_log}"

    if [ -x /usr/sbin/mkfs.ext4 ] && /usr/sbin/mkfs.ext4 -F "${dev}" >"${mkfs_log}" 2>&1; then
        return 0
    fi
    if [ -x /usr/bin/mkfs.ext4 ] && /usr/bin/mkfs.ext4 -F "${dev}" >"${mkfs_log}" 2>&1; then
        return 0
    fi
    if command -v mkfs.ext4 >/dev/null 2>&1 && mkfs.ext4 -F "${dev}" >"${mkfs_log}" 2>&1; then
        return 0
    fi
    if [ -x /usr/sbin/mke2fs ] && /usr/sbin/mke2fs -F "${dev}" >"${mkfs_log}" 2>&1; then
        return 0
    fi
    if [ -x /usr/bin/mke2fs ] && /usr/bin/mke2fs -F "${dev}" >"${mkfs_log}" 2>&1; then
        return 0
    fi
    if command -v mke2fs >/dev/null 2>&1 && mke2fs -F "${dev}" >"${mkfs_log}" 2>&1; then
        return 0
    fi
    if [ -s "${mkfs_log}" ]; then
        sed -n '1,80p' "${mkfs_log}" >&2 || true
    fi
    return 1
}

if [ "${SKIP_MKFS}" != "1" ]; then
    unmount_dev_if_needed "${TEST_DEV}"
    mkfs_ext4_if_needed "${TEST_DEV}" || fail "mkfs.ext4 failed"
fi
unmount_dev_if_needed "${TEST_DEV}"
mount_test_dev

mkdir -p "${CASE_DIR}"
i=1
while [ "${i}" -le "${ITERS}" ]; do
    tmp="${CASE_DIR}/txn_${i}.tmp"
    file="${CASE_DIR}/txn_${i}.dat"
    moved="${CASE_DIR}/txn_${i}.done"
    trunc_file="${CASE_DIR}/txn_${i}.trunc"
    dir="${CASE_DIR}/dir_${i}"
    payload="txn-${i}-payload"

    if ! (: > "${tmp}") 2>/tmp/ext4_journal_create.err; then
        [ -s /tmp/ext4_journal_create.err ] && sed -n '1,20p' /tmp/ext4_journal_create.err >&2 || true
        dump_diag
        fail "create tmp failed at iter=${i}"
    fi
    if ! (printf "%s" "${payload}" >> "${tmp}") 2>/tmp/ext4_journal_write.err; then
        [ -s /tmp/ext4_journal_write.err ] && sed -n '1,20p' /tmp/ext4_journal_write.err >&2 || true
        dump_diag
        fail "write tmp failed at iter=${i}"
    fi
    mv "${tmp}" "${file}" || fail "rename tmp->file failed at iter=${i}"
    printf "-append" >> "${file}" || fail "append failed at iter=${i}"
    mv "${file}" "${moved}" || fail "rename file->moved failed at iter=${i}"

    actual=$(cat "${moved}" 2>/dev/null || true)
    [ "${actual}" = "${payload}-append" ] || fail "content mismatch at iter=${i}"

    : > "${trunc_file}" || fail "truncate init failed at iter=${i}"
    printf "truncate-%s" "${i}" >> "${trunc_file}" || fail "truncate append failed at iter=${i}"
    rm -f "${trunc_file}" || fail "remove trunc file failed at iter=${i}"

    mkdir "${dir}" || fail "mkdir failed at iter=${i}"
    rmdir "${dir}" || fail "rmdir failed at iter=${i}"
    rm -f "${moved}" || fail "cleanup moved file failed at iter=${i}"

    if [ $((i % 10)) -eq 0 ]; then
        sync || true
    fi
    i=$((i + 1))
done

printf "%s" "${ITERS}" > "${CASE_DIR}/journal_iters.ok"
sync || true
umount "${MNT_DIR}" >/dev/null 2>&1 || true

mount_test_dev
recorded=$(cat "${CASE_DIR}/journal_iters.ok" 2>/dev/null || true)
[ "${recorded}" = "${ITERS}" ] || fail "iteration marker mismatch after remount"
sync || true
umount "${MNT_DIR}" >/dev/null 2>&1 || true

echo "EXT4_JOURNAL_PASS iters=${ITERS}"
exit 0
