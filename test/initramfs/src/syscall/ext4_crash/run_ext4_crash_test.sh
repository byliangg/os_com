#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -eu

PHASE=${EXT4_CRASH_PHASE:-verify}
SCENARIO=${EXT4_CRASH_SCENARIO:-create_write}
TEST_DEV=${EXT4_CRASH_TEST_DEV:-/dev/vda}
MNT_DIR=${EXT4_CRASH_MNT:-/ext4_crash_test}
CASE_DIR_NAME=${EXT4_CRASH_CASE_DIR:-phase4_crash}
CASE_DIR=${MNT_DIR}/${CASE_DIR_NAME}

fail() {
    echo "EXT4_CRASH_FAIL scenario=${SCENARIO} phase=${PHASE} reason=$1" >&2
    exit 1
}

require_file_content() {
    file="$1"
    expected="$2"
    [ -f "${file}" ] || fail "missing file ${file}"
    actual=$(cat "${file}" 2>/dev/null || true)
    [ "${actual}" = "${expected}" ] || fail "file content mismatch for ${file}"
}

ensure_mountpoint_unmounted() {
    if awk -v t="${MNT_DIR}" '$2==t { found=1; exit 0 } END { exit(found ? 0 : 1) }' /proc/mounts; then
        umount "${MNT_DIR}" >/dev/null 2>&1 || true
    fi
}

mount_test_dev() {
    [ -b "${TEST_DEV}" ] || fail "device not found: ${TEST_DEV}"
    mkdir -p "${MNT_DIR}"
    ensure_mountpoint_unmounted
    mount -t ext4 "${TEST_DEV}" "${MNT_DIR}" || fail "mount failed"
}

mkfs_ext4_if_needed() {
    dev="$1"
    mkfs_log="/tmp/ext4_crash_mkfs_$(basename "${dev}" | tr -c '[:alnum:]' '_').log"
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

prepare_create_write() {
    mkdir -p "${CASE_DIR}"
    printf "phase4-create-write" > "${CASE_DIR}/create_write.txt"
}

verify_create_write() {
    require_file_content "${CASE_DIR}/create_write.txt" "phase4-create-write"
}

prepare_rename() {
    mkdir -p "${CASE_DIR}"
    dd if=/dev/zero of="${CASE_DIR}/rename_src.bin" bs=512 count=1 >/dev/null 2>&1
    mv "${CASE_DIR}/rename_src.bin" "${CASE_DIR}/rename_dst.bin"
}

verify_rename() {
    [ ! -e "${CASE_DIR}/rename_src.bin" ] || fail "rename_src.bin still exists"
    [ -f "${CASE_DIR}/rename_dst.bin" ] || fail "rename_dst.bin missing"
}

prepare_truncate_append() {
    mkdir -p "${CASE_DIR}"
    dd if=/dev/zero of="${CASE_DIR}/truncate_append.txt" bs=512 count=1 >/dev/null 2>&1
    : > "${CASE_DIR}/truncate_append.txt"
    printf "after-truncate-append" >> "${CASE_DIR}/truncate_append.txt"
}

verify_truncate_append() {
    require_file_content "${CASE_DIR}/truncate_append.txt" "after-truncate-append"
}

case "${PHASE}" in
    prepare)
        mkfs_ext4_if_needed "${TEST_DEV}" || fail "mkfs.ext4 failed"
        mount_test_dev

        case "${SCENARIO}" in
            create_write)
                prepare_create_write
                ;;
            rename)
                prepare_rename
                ;;
            truncate_append)
                prepare_truncate_append
                ;;
            *)
                fail "unknown scenario ${SCENARIO}"
                ;;
        esac

        echo "EXT4_CRASH_PREPARE_DONE scenario=${SCENARIO}"
        # If kernel replay-hold injection is disabled, keep VM alive for host-side kill.
        sleep 600
        ;;
    verify)
        mount_test_dev
        case "${SCENARIO}" in
            create_write)
                verify_create_write
                ;;
            rename)
                verify_rename
                ;;
            truncate_append)
                verify_truncate_append
                ;;
            *)
                fail "unknown scenario ${SCENARIO}"
                ;;
        esac
        sync || true
        umount "${MNT_DIR}" >/dev/null 2>&1 || true
        echo "EXT4_CRASH_VERIFY_PASS scenario=${SCENARIO}"
        ;;
    *)
        fail "unknown phase ${PHASE}"
        ;;
esac

exit 0
