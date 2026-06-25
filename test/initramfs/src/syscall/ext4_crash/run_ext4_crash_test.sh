#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -eu

PHASE=${EXT4_CRASH_PHASE:-verify}
SCENARIO=${EXT4_CRASH_SCENARIO:-create_write}
EXPECT=${EXT4_CRASH_EXPECT:-committed}
TEST_DEV=${EXT4_CRASH_TEST_DEV:-/dev/vda}
MNT_DIR=${EXT4_CRASH_MNT:-/ext4_crash_test}
CASE_DIR_NAME=${EXT4_CRASH_CASE_DIR:-phase4_crash}
CASE_DIR=${MNT_DIR}/${CASE_DIR_NAME}
FSYNC_HELPER=${EXT4_CRASH_FSYNC_HELPER:-/opt/xfstests/fsync_file}

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

require_file_size() {
    file="$1"
    expected="$2"
    [ -f "${file}" ] || fail "missing file ${file}"
    actual=$(wc -c < "${file}" 2>/dev/null | tr -d '[:space:]')
    [ "${actual}" = "${expected}" ] || fail "file size mismatch for ${file}: got ${actual}, want ${expected}"
}

require_file_contains() {
    file="$1"
    needle="$2"
    [ -f "${file}" ] || fail "missing file ${file}"
    grep -F -q "${needle}" "${file}" 2>/dev/null || fail "file ${file} does not contain ${needle}"
}

require_file_not_contains() {
    file="$1"
    needle="$2"
    [ -f "${file}" ] || fail "missing file ${file}"
    if grep -F -q "${needle}" "${file}" 2>/dev/null; then
        fail "file ${file} unexpectedly contains ${needle} (stale dir-block leak)"
    fi
}

run_e2fsck_clean() {
    dev="$1"
    fsck_bin=""
    for cand in /usr/sbin/e2fsck /sbin/e2fsck /usr/bin/e2fsck; do
        if [ -x "${cand}" ]; then
            fsck_bin="${cand}"
            break
        fi
    done
    if [ -z "${fsck_bin}" ]; then
        # No fsck binary injected: skip rather than fail (interop check is best-effort).
        echo "EXT4_CRASH_E2FSCK_SKIP dev=${dev} reason=no_e2fsck_binary" >&2
        return 0
    fi
    ensure_device_unmounted "${dev}"
    # -f force full check, -n read-only (never modify), -y not needed because -n.
    # e2fsck exit codes: 0 = clean, 1 = errors corrected (impossible with -n),
    # >=4 = errors left uncorrected -> the filesystem is inconsistent.
    fsck_log="/tmp/ext4_crash_e2fsck_$(basename "${dev}" | tr -c '[:alnum:]' '_').log"
    # Guard against `set -e`: a non-zero e2fsck exit (e.g. rc=4) must not abort
    # the script here; we inspect rc explicitly below.
    rc=0
    "${fsck_bin}" -f -n "${dev}" >"${fsck_log}" 2>&1 || rc=$?
    if [ "${rc}" -ge 4 ]; then
        echo "EXT4_CRASH_E2FSCK_FAIL dev=${dev} rc=${rc}" >&2
        sed -n '1,80p' "${fsck_log}" >&2 || true
        fail "e2fsck reported uncorrected inconsistency on ${dev} (rc=${rc})"
    fi
    echo "EXT4_CRASH_E2FSCK_CLEAN dev=${dev} rc=${rc}"
}

require_file_filled_with_byte() {
    file="$1"
    byte="$2"
    char=$(printf "\\$(printf '%03o' "${byte}")")
    [ -f "${file}" ] || fail "missing file ${file}"
    remaining=$(LC_ALL=C tr -d "${char}" < "${file}" | wc -c | tr -d '[:space:]')
    [ "${remaining}" = "0" ] || fail "file ${file} contains ${remaining} bytes other than ${byte}"
}

fsync_helper() {
    [ -x "${FSYNC_HELPER}" ] || fail "missing fsync helper ${FSYNC_HELPER}"
    "${FSYNC_HELPER}" "$@" || fail "fsync helper failed: $*"
}

ensure_mountpoint_unmounted() {
    if awk -v t="${MNT_DIR}" '$2==t { found=1; exit 0 } END { exit(found ? 0 : 1) }' /proc/mounts; then
        umount "${MNT_DIR}" >/dev/null 2>&1 || true
    fi
}

ensure_device_unmounted() {
    dev="$1"
    # Unmount every mountpoint using this block device.
    while :; do
        mp=$(awk -v d="${dev}" '$1==d { print $2; exit 0 }' /proc/mounts)
        if [ -z "${mp}" ]; then
            break
        fi
        umount "${mp}" >/dev/null 2>&1 || true
        # If unmount failed, avoid infinite loop.
        if awk -v d="${dev}" '$1==d { found=1; exit 0 } END { exit(found ? 0 : 1) }' /proc/mounts; then
            break
        fi
    done
}

mount_test_dev() {
    [ -b "${TEST_DEV}" ] || fail "device not found: ${TEST_DEV}"
    mkdir -p "${MNT_DIR}"
    ensure_device_unmounted "${TEST_DEV}"
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
    if [ -x /usr/sbin/mke2fs ] && mke2fs_try_ext4 /usr/sbin/mke2fs "${dev}" "${mkfs_log}"; then
        return 0
    fi
    if [ -x /usr/bin/mke2fs ] && mke2fs_try_ext4 /usr/bin/mke2fs "${dev}" "${mkfs_log}"; then
        return 0
    fi
    if command -v mke2fs >/dev/null 2>&1 && mke2fs_try_ext4 mke2fs "${dev}" "${mkfs_log}"; then
        return 0
    fi

    if [ -s "${mkfs_log}" ]; then
        sed -n '1,80p' "${mkfs_log}" >&2 || true
    fi
    return 1
}

mke2fs_try_ext4() {
    cmd="$1"
    dev="$2"
    log="$3"

    if "${cmd}" -t ext4 -F "${dev}" >"${log}" 2>&1; then
        return 0
    fi
    if "${cmd}" -F "${dev}" >"${log}" 2>&1; then
        return 0
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
    truncate -s 512 "${CASE_DIR}/truncate_append.txt"
    : > "${CASE_DIR}/truncate_append.txt"
    printf "after-truncate-append" >> "${CASE_DIR}/truncate_append.txt"
}

verify_truncate_append() {
    require_file_content "${CASE_DIR}/truncate_append.txt" "after-truncate-append"
}

prepare_large_write() {
    mkdir -p "${CASE_DIR}"
    dd if=/dev/zero of="${CASE_DIR}/large_write.tmp" bs=4096 count=1024 >/dev/null 2>&1
    mv "${CASE_DIR}/large_write.tmp" "${CASE_DIR}/large_write.bin"
}

verify_large_write() {
    require_file_size "${CASE_DIR}/large_write.bin" 4194304
}

prepare_fsync_durability() {
    mkdir -p "${CASE_DIR}"
    printf "fsync-durable-payload" > "${CASE_DIR}/fsync_durability.tmp"
    sync
    mv "${CASE_DIR}/fsync_durability.tmp" "${CASE_DIR}/fsync_durability.txt"
}

verify_fsync_durability() {
    require_file_content "${CASE_DIR}/fsync_durability.txt" "fsync-durable-payload"
}

prepare_multi_file_create() {
    mkdir -p "${CASE_DIR}/multi_tmp"
    for i in 1 2 3 4 5 6 7 8; do
        printf "multi-file-%s" "${i}" > "${CASE_DIR}/multi_tmp/file_${i}.txt"
    done
    mv "${CASE_DIR}/multi_tmp" "${CASE_DIR}/multi"
}

verify_multi_file_create() {
    for i in 1 2 3 4 5 6 7 8; do
        require_file_content "${CASE_DIR}/multi/file_${i}.txt" "multi-file-${i}"
    done
}

prepare_dir_tree_churn() {
    mkdir -p "${CASE_DIR}/tree/a/b/c" "${CASE_DIR}/tree/remove_me"
    printf "tree-marker" > "${CASE_DIR}/tree/a/b/c/marker.txt"
    rmdir "${CASE_DIR}/tree/remove_me"
}

verify_dir_tree_churn() {
    require_file_content "${CASE_DIR}/tree/a/b/c/marker.txt" "tree-marker"
    [ ! -e "${CASE_DIR}/tree/remove_me" ] || fail "removed directory still exists"
}

prepare_rename_across_dir() {
    mkdir -p "${CASE_DIR}/src" "${CASE_DIR}/dst"
    printf "rename-across-dir" > "${CASE_DIR}/src/item.txt"
    mv "${CASE_DIR}/src/item.txt" "${CASE_DIR}/dst/item.txt"
}

verify_rename_across_dir() {
    [ ! -e "${CASE_DIR}/src/item.txt" ] || fail "source still exists after cross-dir rename"
    require_file_content "${CASE_DIR}/dst/item.txt" "rename-across-dir"
}

prepare_truncate_shrink() {
    mkdir -p "${CASE_DIR}"
    dd if=/dev/zero of="${CASE_DIR}/truncate_shrink.bin" bs=4096 count=4 >/dev/null 2>&1
    : > "${CASE_DIR}/truncate_shrink.bin"
}

verify_truncate_shrink() {
    require_file_size "${CASE_DIR}/truncate_shrink.bin" 0
}

# Validates the JBD2 REVOKE fix (Phase 7 Task 8 / BUG-5) end-to-end against a
# real crash + real disk. Workload (see technical_report.md 附录 A-1):
#   1. Grow a directory across MULTIPLE journaled dir-blocks, then sync so those
#      blocks are committed to the journal and checkpointed to their home spots.
#   2. Delete every entry and rmdir the directory. This FREES the directory's
#      metadata blocks; with the fix the freeing transaction records REVOKE
#      records for each freed dir-block.
#   3. Immediately create a fresh regular file and write a LARGE distinctive
#      data pattern (a single repeating byte 'R' = 0x52, which is NOT a valid
#      ext4 dir-block: no leaf magic, no rec_len/name_len dirent structure, no
#      dirent filenames), sized to reclaim roughly the just-freed blocks. fsync
#      commits the reused-block data AND the pending revoke.
# The host harness holds the kernel at `write:after_commit` (the file-write
# commit, after the commit block + SB are on disk, BEFORE the freed dir-blocks
# are checkpointed to their home location), then kills the VM. Recovery then
# replays the journal with BOTH the stale dir-block records AND the revoke
# present. If revoke works, recovery skips the freed dir-blocks and the file
# keeps its 'R' payload. If revoke is broken (BUG-5), recovery replays stale
# dir-block content (dirent bytes / leaf magic) over the reused data blocks and
# the file is corrupted -> verify FAILS (filled-byte mismatch and/or a leaked
# dirent filename), and e2fsck may also flag inconsistency.
DIR_DELETE_REUSE_ENTRY_PREFIX="reuse_entry_long_name_marker_"
DIR_DELETE_REUSE_PAYLOAD_BYTE=82   # ASCII 'R' for "REUSE"
prepare_dir_delete_reuse() {
    mkdir -p "${CASE_DIR}"
    rd="${CASE_DIR}/reuse_dir"
    rm -rf "${rd}"
    mkdir -p "${rd}"

    # Long entry names force the directory to span several 4 KiB dir-blocks
    # well before the count gets large; a few hundred entries grows it to
    # multiple journaled metadata blocks.
    i=1
    while [ "${i}" -le 400 ]; do
        : > "${rd}/${DIR_DELETE_REUSE_ENTRY_PREFIX}${i}"
        i=$((i + 1))
    done
    # Commit + checkpoint the directory metadata blocks to their home location.
    sync

    # Free every dir entry and the directory itself: this frees the directory's
    # metadata blocks and (with the fix) records REVOKE records for them.
    i=1
    while [ "${i}" -le 400 ]; do
        rm -f "${rd}/${DIR_DELETE_REUSE_ENTRY_PREFIX}${i}"
        i=$((i + 1))
    done
    rmdir "${rd}"

    # Immediately reclaim the freed blocks with distinctive file data. Build the
    # payload by filling with the byte 'R' (0x52). 8 MiB comfortably exceeds the
    # freed directory's block footprint so the realloc lands on freed space.
    char=$(printf "\\$(printf '%03o' "${DIR_DELETE_REUSE_PAYLOAD_BYTE}")")
    target="${CASE_DIR}/reuse_data.bin"
    rm -f "${target}"
    # tr emits the fill byte for every input byte; /dev/zero feeds the length.
    dd if=/dev/zero bs=4096 count=2048 2>/dev/null | tr '\000' "${char}" > "${target}"

    # fsync the file: commits the reused-block file data AND the pending revoke
    # from the rmdir, so recovery after the crash sees the revoke.
    if [ -x "${FSYNC_HELPER}" ]; then
        fsync_helper fsync "${target}"
    else
        sync
    fi
}

verify_dir_delete_reuse() {
    target="${CASE_DIR}/reuse_data.bin"
    # The reused-data file must survive recovery byte-for-byte: filled entirely
    # with 'R' (0x52). Any other byte means stale dir-block metadata (leaf
    # magic, rec_len/inode fields, checksum) leaked over the reused data blocks
    # -> revoke is broken.
    require_file_size "${target}" 8388608
    require_file_filled_with_byte "${target}" "${DIR_DELETE_REUSE_PAYLOAD_BYTE}"
    # Directly assert no stale dir entry name leaked into the file data.
    require_file_not_contains "${target}" "${DIR_DELETE_REUSE_ENTRY_PREFIX}"
    # The freed directory must stay gone after recovery.
    [ ! -e "${CASE_DIR}/reuse_dir" ] || fail "reuse_dir reappeared after recovery"
    # Interop check: a broken revoke can also leave e2fsck-detectable damage
    # (e.g. blocks claimed by both the file and the stale dir inode). Unmount
    # first, then run a read-only forced fsck.
    sync || true
    umount "${MNT_DIR}" >/dev/null 2>&1 || true
    run_e2fsck_clean "${TEST_DEV}"
    # Remount so the common verify-phase teardown (sync + umount) is a no-op.
    mount_test_dev
}

prepare_append_concurrent() {
    mkdir -p "${CASE_DIR}"
    : > "${CASE_DIR}/append_concurrent.tmp"
    for i in 1 2 3 4; do
        (
            printf "worker-%s\n" "${i}" >> "${CASE_DIR}/append_concurrent.tmp"
        ) &
    done
    wait
    printf "append-final\n" >> "${CASE_DIR}/append_concurrent.tmp"
    mv "${CASE_DIR}/append_concurrent.tmp" "${CASE_DIR}/append_concurrent.txt"
}

verify_append_concurrent() {
    require_file_contains "${CASE_DIR}/append_concurrent.txt" "append-final"
}

prepare_host_crash_fsync_size_durability() {
    mkdir -p "${CASE_DIR}/fsync_size"
    fsync_helper pwrite "${CASE_DIR}/fsync_size/data.bin" 0 65536 65
    fsync_helper fsync "${CASE_DIR}/fsync_size/data.bin"
    fsync_helper fsync "${CASE_DIR}/fsync_size"
}

verify_host_crash_fsync_size_durability() {
    require_file_size "${CASE_DIR}/fsync_size/data.bin" 65536
    require_file_filled_with_byte "${CASE_DIR}/fsync_size/data.bin" 65
}

prepare_host_crash_fdatasync_metadata() {
    mkdir -p "${CASE_DIR}/fdatasync_meta"
    fsync_helper pwrite "${CASE_DIR}/fdatasync_meta/data.bin" 0 32768 66
    fsync_helper fdatasync "${CASE_DIR}/fdatasync_meta/data.bin"
    fsync_helper pwrite "${CASE_DIR}/fdatasync_meta/fsync_data.bin" 0 4096 67
    fsync_helper fsync "${CASE_DIR}/fdatasync_meta/fsync_data.bin"
    fsync_helper fsync "${CASE_DIR}/fdatasync_meta"
}

verify_host_crash_fdatasync_metadata() {
    require_file_size "${CASE_DIR}/fdatasync_meta/data.bin" 32768
    require_file_filled_with_byte "${CASE_DIR}/fdatasync_meta/data.bin" 66
    require_file_size "${CASE_DIR}/fdatasync_meta/fsync_data.bin" 4096
    require_file_filled_with_byte "${CASE_DIR}/fdatasync_meta/fsync_data.bin" 67
}

prepare_host_crash_rename_fsync_dst() {
    mkdir -p "${CASE_DIR}/rename_fsync/src" "${CASE_DIR}/rename_fsync/dst"
    printf "rename-fsync-dst-payload" > "${CASE_DIR}/rename_fsync/src/item.tmp"
    fsync_helper fsync "${CASE_DIR}/rename_fsync/src/item.tmp"
    mv "${CASE_DIR}/rename_fsync/src/item.tmp" "${CASE_DIR}/rename_fsync/dst/item.txt"
    fsync_helper fsync "${CASE_DIR}/rename_fsync/dst"
}

verify_host_crash_rename_fsync_dst() {
    [ ! -e "${CASE_DIR}/rename_fsync/src/item.tmp" ] || fail "rename source still exists"
    require_file_content "${CASE_DIR}/rename_fsync/dst/item.txt" "rename-fsync-dst-payload"
}

prepare_host_crash_concurrent_fsync() {
    mkdir -p "${CASE_DIR}/concurrent_fsync"
    for i in 1 2 3 4; do
        (
            byte=$((70 + i))
            fsync_helper pwrite "${CASE_DIR}/concurrent_fsync/worker_${i}.bin" 0 8192 "${byte}"
            fsync_helper fsync "${CASE_DIR}/concurrent_fsync/worker_${i}.bin"
        ) &
    done
    wait
    fsync_helper fsync "${CASE_DIR}/concurrent_fsync"
}

verify_host_crash_concurrent_fsync() {
    for i in 1 2 3 4; do
        byte=$((70 + i))
        require_file_size "${CASE_DIR}/concurrent_fsync/worker_${i}.bin" 8192
        require_file_filled_with_byte "${CASE_DIR}/concurrent_fsync/worker_${i}.bin" "${byte}"
    done
}

prepare_scenario() {
    case "$1" in
        create_write) prepare_create_write ;;
        rename) prepare_rename ;;
        truncate_append) prepare_truncate_append ;;
        large_write) prepare_large_write ;;
        fsync_durability) prepare_fsync_durability ;;
        multi_file_create) prepare_multi_file_create ;;
        dir_tree_churn) prepare_dir_tree_churn ;;
        rename_across_dir) prepare_rename_across_dir ;;
        truncate_shrink) prepare_truncate_shrink ;;
        dir_delete_reuse) prepare_dir_delete_reuse ;;
        append_concurrent) prepare_append_concurrent ;;
        host_crash_fsync_size_durability) prepare_host_crash_fsync_size_durability ;;
        host_crash_fdatasync_metadata) prepare_host_crash_fdatasync_metadata ;;
        host_crash_rename_fsync_dst) prepare_host_crash_rename_fsync_dst ;;
        host_crash_concurrent_fsync) prepare_host_crash_concurrent_fsync ;;
        *) fail "unknown scenario $1" ;;
    esac
}

verify_scenario() {
    case "$1" in
        create_write) verify_create_write ;;
        rename) verify_rename ;;
        truncate_append) verify_truncate_append ;;
        large_write) verify_large_write ;;
        fsync_durability) verify_fsync_durability ;;
        multi_file_create) verify_multi_file_create ;;
        dir_tree_churn) verify_dir_tree_churn ;;
        rename_across_dir) verify_rename_across_dir ;;
        truncate_shrink) verify_truncate_shrink ;;
        dir_delete_reuse) verify_dir_delete_reuse ;;
        append_concurrent) verify_append_concurrent ;;
        host_crash_fsync_size_durability) verify_host_crash_fsync_size_durability ;;
        host_crash_fdatasync_metadata) verify_host_crash_fdatasync_metadata ;;
        host_crash_rename_fsync_dst) verify_host_crash_rename_fsync_dst ;;
        host_crash_concurrent_fsync) verify_host_crash_concurrent_fsync ;;
        *) fail "unknown scenario $1" ;;
    esac
}

verify_scenario_uncommitted() {
    case "$1" in
        create_write)
            [ ! -e "${CASE_DIR}/create_write.txt" ] || fail "uncommitted create_write file exists"
            ;;
        *)
            fail "uncommitted expectation is not implemented for scenario $1"
            ;;
    esac
}

case "${PHASE}" in
    prepare)
        ensure_device_unmounted "${TEST_DEV}"
        if [ "${EXT4_CRASH_SKIP_MKFS:-0}" != "1" ]; then
            mkfs_ext4_if_needed "${TEST_DEV}" || fail "mkfs.ext4 failed"
        fi
        mount_test_dev

        prepare_scenario "${SCENARIO}"

        echo "EXT4_CRASH_PREPARE_DONE scenario=${SCENARIO}"
        # If kernel replay-hold injection is disabled, keep VM alive for host-side kill.
        sleep 600
        ;;
    verify)
        mount_test_dev
        case "${EXPECT}" in
            committed)
                verify_scenario "${SCENARIO}"
                ;;
            uncommitted)
                verify_scenario_uncommitted "${SCENARIO}"
                ;;
            *)
                fail "unknown expectation ${EXPECT}"
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
