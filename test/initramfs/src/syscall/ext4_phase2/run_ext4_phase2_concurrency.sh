#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -eu

TEST_DEV=${EXT4_PHASE2_TEST_DEV:-/dev/vda}
MNT_DIR=${EXT4_PHASE2_MNT:-/ext4_phase2}
ROOT_DIR=${EXT4_PHASE2_ROOT:-${MNT_DIR}/phase2}
RESULTS_DIR=${EXT4_PHASE2_RESULTS_DIR:-/tmp/ext4_phase2_results}
HELPER=${EXT4_PHASE2_HELPER:-/opt/ext4_phase2/phase2_concurrency}
CASES_RAW=${EXT4_PHASE2_CASES:-"multi_file_write_verify,multi_file_read_write,create_unlink_churn,rename_churn,write_truncate_fsync"}
CASES=$(echo "${CASES_RAW}" | tr ',' ' ')
WORKERS=${EXT4_PHASE2_WORKERS:-4}
ROUNDS=${EXT4_PHASE2_ROUNDS:-8}
SEED=${EXT4_PHASE2_SEED:-1}
SKIP_MKFS=${EXT4_PHASE2_SKIP_MKFS:-0}

SUMMARY_FILE=${RESULTS_DIR}/jbd_phase2_concurrency_summary.tsv
RESULTS_FILE=${RESULTS_DIR}/jbd_phase2_concurrency_results.tsv

fail() {
    echo "EXT4_PHASE2_RUNNER_FAIL reason=$1" >&2
    exit 1
}

ensure_device_unmounted() {
    dev="$1"
    while :; do
        mp=$(awk -v d="${dev}" '$1==d { print $2; exit 0 }' /proc/mounts)
        [ -n "${mp}" ] || break
        umount "${mp}" >/dev/null 2>&1 || true
        if awk -v d="${dev}" '$1==d { found=1; exit 0 } END { exit(found ? 0 : 1) }' /proc/mounts; then
            break
        fi
    done
}

ensure_mountpoint_unmounted() {
    if awk -v t="${MNT_DIR}" '$2==t { found=1; exit 0 } END { exit(found ? 0 : 1) }' /proc/mounts; then
        umount "${MNT_DIR}" >/dev/null 2>&1 || true
    fi
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

mkfs_ext4_if_needed() {
    dev="$1"
    mkfs_log="/tmp/ext4_phase2_mkfs_$(basename "${dev}" | tr -c '[:alnum:]' '_').log"
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

    sed -n '1,80p' "${mkfs_log}" >&2 || true
    return 1
}

mount_test_dev() {
    [ -b "${TEST_DEV}" ] || fail "device not found: ${TEST_DEV}"
    mkdir -p "${MNT_DIR}" "${RESULTS_DIR}"
    ensure_device_unmounted "${TEST_DEV}"
    ensure_mountpoint_unmounted
    if [ "${SKIP_MKFS}" != "1" ]; then
        mkfs_ext4_if_needed "${TEST_DEV}" || fail "mkfs failed"
    fi
    mount -t ext4 "${TEST_DEV}" "${MNT_DIR}" || fail "mount failed"
    mkdir -p "${ROOT_DIR}"
}

main() {
    [ -x "${HELPER}" ] || fail "helper missing: ${HELPER}"

    mount_test_dev
    : >"${RESULTS_FILE}"
    printf "case\tstatus\trc\tseed\tworkers\trounds\tlog\n" >"${RESULTS_FILE}"

    pass_count=0
    fail_count=0

    for case_name in ${CASES}; do
        case_log="${RESULTS_DIR}/${case_name}.log"
        echo "EXT4_PHASE2_CASE_START case=${case_name} seed=${SEED} workers=${WORKERS} rounds=${ROUNDS}"
        set +e
        "${HELPER}" \
            --case "${case_name}" \
            --root "${ROOT_DIR}" \
            --workers "${WORKERS}" \
            --rounds "${ROUNDS}" \
            --seed "${SEED}" >"${case_log}" 2>&1
        rc=$?
        set -e
        cat "${case_log}"
        if [ "${rc}" -eq 0 ] && grep -q "EXT4_PHASE2_CASE_PASS case=${case_name}" "${case_log}"; then
            printf "%s\tPASS\t0\t%s\t%s\t%s\t%s\n" \
                "${case_name}" "${SEED}" "${WORKERS}" "${ROUNDS}" "${case_log}" >>"${RESULTS_FILE}"
            pass_count=$((pass_count + 1))
        else
            printf "%s\tFAIL\t%s\t%s\t%s\t%s\t%s\n" \
                "${case_name}" "${rc}" "${SEED}" "${WORKERS}" "${ROUNDS}" "${case_log}" >>"${RESULTS_FILE}"
            fail_count=$((fail_count + 1))
        fi
    done

    sync || true
    umount "${MNT_DIR}" >/dev/null 2>&1 || true

    {
        echo "mode\tpass\tfail\tseed\tworkers\trounds"
        echo "jbd_phase2_concurrency\t${pass_count}\t${fail_count}\t${SEED}\t${WORKERS}\t${ROUNDS}"
    } >"${SUMMARY_FILE}"
    cat "${SUMMARY_FILE}"
    echo "===== jbd_phase2_concurrency detailed results ====="
    cat "${RESULTS_FILE}"

    if [ "${fail_count}" -ne 0 ]; then
        fail "one or more phase2 concurrency cases failed"
    fi

    echo "EXT4_PHASE2_CONCURRENCY_PASS pass=${pass_count} fail=${fail_count} seed=${SEED} workers=${WORKERS} rounds=${ROUNDS}"
}

main "$@"
