#!/usr/bin/env bash
# Phase 5 correctness regression for the metadata-only extent mapping cache.
#
# Runs, in one container:
#   1. pagecache_phase4 (default: page_cache=1, direct_read_cache=1) — broad
#      non-regression for the mixed direct/buffered/truncate/mmap coherency mode
#      (confirms the read_direct_at refactor did not break anything).
#   2. generic/091 (fsx, O_DIRECT-heavy) in phase4_good mode (page_cache=0) at
#      direct_read_cache=1 (speculative baseline) vs =0 (the new extent map
#      cache). Same test + same mode isolates the cache: if both pass, the new
#      cache's invalidation is correct; if only =0 fails, it is the bug.
#   3. generic/130 (buffered/direct coherency, direct EOF zeroing) same A/B.
#
# The extent map cache only activates when page_cache=0 AND direct_read_cache=0,
# so the =0 leg is the only one that exercises it.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ASTER_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"
TS="$(date +%Y%m%d_%H%M%S)"
LOG_DIR="${LOG_DIR:-${ASTER_DIR}/benchmark/logs/phase5_regression_${TS}}"

if ! command -v docker >/dev/null 2>&1; then echo "Error: docker missing" >&2; exit 1; fi
if [[ ! -e /dev/kvm ]]; then echo "Error: /dev/kvm missing" >&2; exit 1; fi

IMAGE="${IMAGE:-asterinas/asterinas:0.17.0-20260227}"
HTTP_PROXY_VALUE="${http_proxy:-http://127.0.0.1:7890}"
HTTPS_PROXY_VALUE="${https_proxy:-http://127.0.0.1:7890}"
ALL_PROXY_VALUE="${all_proxy:-socks5://127.0.0.1:7890}"
# A/B test list (single tests, run once per direct_read_cache value).
AB_TESTS="${AB_TESTS:-generic/091 generic/130}"
RUN_PAGECACHE_NONREG="${RUN_PAGECACHE_NONREG:-1}"

mkdir -p "${LOG_DIR}"
echo "phase5 regression logs: ${LOG_DIR}"

docker run --pull=never --rm --privileged --network=host --device=/dev/kvm \
    -v /dev:/dev \
    -v "${ASTER_DIR}:/root/asterinas" \
    -v "${LOG_DIR}:/reg-logs" \
    -w /root/asterinas \
    -e http_proxy="${HTTP_PROXY_VALUE}" \
    -e https_proxy="${HTTPS_PROXY_VALUE}" \
    -e all_proxy="${ALL_PROXY_VALUE}" \
    -e AB_TESTS="${AB_TESTS}" \
    -e RUN_PAGECACHE_NONREG="${RUN_PAGECACHE_NONREG}" \
    -e FULL_GUARD="${FULL_GUARD:-0}" \
    -e FULL_SUITE="${FULL_SUITE:-0}" \
    "${IMAGE}" \
    bash -lc '
        set -uo pipefail
        rm -rf /root/asterinas/.target_bench/osdk
        OSDK_LOCAL_DEV=1 cargo install --locked cargo-osdk --path /root/asterinas/osdk --force

        tools/ext4/prepare_phase4_part3_initramfs.sh \
            /root/asterinas/benchmark/assets/initramfs/initramfs_phase3.cpio.gz \
            /root/asterinas/benchmark/assets/initramfs/initramfs_phase4_part3.cpio.gz

        common_env() {
            env \
                VDSO_LIBRARY_DIR=/root/asterinas/benchmark/assets/linux_vdso \
                CARGO_TARGET_DIR=/root/asterinas/.target_bench \
                BOOT_METHOD=qemu-direct OVMF=off RELEASE_LTO=1 \
                ENABLE_KVM=1 NETDEV=tap VHOST=on CONSOLE=ttyS0 KLOG_LEVEL=error \
                LOG_DIR=/reg-logs \
                INITRAMFS_IMG=/root/asterinas/benchmark/assets/initramfs/initramfs_phase4_part3.cpio.gz \
                BASE_INITRAMFS=/root/asterinas/benchmark/assets/initramfs/initramfs_phase3.cpio.gz \
                PHASE4_GOOD_THRESHOLD=0 PAGECACHE_PHASE4_THRESHOLD=100 \
                XFSTESTS_TEST_IMG_SIZE=2G XFSTESTS_SCRATCH_IMG_SIZE=2G \
                XFSTESTS_CASE_TIMEOUT_SEC=1200 XFSTESTS_RUN_TIMEOUT_SEC=5400 \
                XFSTESTS_IGNORE_STATIC_EXCLUDED_FOR_SINGLE=1 \
                RUN_CRASH_SUITE=0 RUN_PHASE3_BASE=0 RUN_PHASE6_GOOD=0 \
                RUN_JBD_PHASE1=0 RUN_PHASE2_CONCURRENCY=0 RUN_JBD_PHASE3=0 RUN_LMBENCH=0 \
                "$@"
        }

        RESULT=/reg-logs/regression_result.txt
        : > "${RESULT}"
        note() { echo "$1" | tee -a "${RESULT}"; }

        if [ "${FULL_SUITE:-0}" = "1" ]; then
            # Complete guard at drc=0 (extent + inode cache active): all xfstests
            # modes + crash matrix + Phase 2 concurrency + jbd3 fsync, then the
            # host-crash fsync matrix. Mirrors the sweep G group.
            note ">>> [full-suite drc=0] crash + phase4_good + pagecache_phase4 + phase3_base + phase6_good + jbd_phase1 + concurrency + jbd_phase3"
            common_env \
                RUN_CRASH_SUITE=1 RUN_PHASE4_GOOD=1 RUN_PAGECACHE_PHASE4=1 RUN_PHASE3_BASE=1 \
                RUN_PHASE6_GOOD=1 RUN_JBD_PHASE1=1 RUN_PHASE2_CONCURRENCY=1 RUN_JBD_PHASE3=1 \
                PHASE4_GOOD_THRESHOLD=90 PHASE6_GOOD_THRESHOLD=90 \
                CRASH_ROUNDS=2 CRASH_PREPARE_WAIT_SEC=180 CRASH_HOLD_STAGE=after_commit \
                CRASH_SCENARIOS="" CRASH_EXPECT=committed \
                EXT4_PHASE2_CASES="multi_file_write_verify,multi_file_read_write,create_unlink_churn,rename_churn,write_truncate_fsync,unlink_while_open,allocator_churn" \
                EXT4_PHASE2_WORKERS=4 EXT4_PHASE2_ROUNDS=8 EXT4_PHASE2_SEED=78 EXT4_PHASE2_TIMEOUT_SEC=900 \
                EXT4_DIRECT_READ_CACHE=0 EXT4_EXTENT_MAP_CACHE=1 \
                tools/ext4/run_phase4_part3.sh > /reg-logs/full_suite_main.log 2>&1 \
                && note "    full_suite_main rc=0" || note "    full_suite_main rc=$?"

            note ">>> [full-suite] host-crash fsync matrix"
            common_env \
                RUN_CRASH_SUITE=1 RUN_PHASE4_GOOD=0 RUN_PAGECACHE_PHASE4=0 RUN_PHASE3_BASE=0 \
                RUN_PHASE6_GOOD=0 RUN_JBD_PHASE1=0 RUN_PHASE2_CONCURRENCY=0 RUN_JBD_PHASE3=0 \
                CRASH_ROUNDS=1 CRASH_PREPARE_WAIT_SEC=180 CRASH_HOLD_STAGE=prepare_done \
                CRASH_SCENARIOS="host_crash_fsync_size_durability:prepare_done host_crash_fdatasync_metadata:prepare_done host_crash_rename_fsync_dst:prepare_done host_crash_concurrent_fsync:prepare_done" \
                CRASH_EXPECT=committed \
                EXT4_DIRECT_READ_CACHE=0 EXT4_EXTENT_MAP_CACHE=1 \
                tools/ext4/run_phase4_part3.sh > /reg-logs/full_suite_host_crash.log 2>&1 \
                && note "    full_suite_host_crash rc=0" || note "    full_suite_host_crash rc=$?"

            note "=== xfstests verdicts ==="
            grep -hoE "xfstests (phase4_good|pagecache_phase4|phase3_base|phase6_good|jbd_phase1|jbd_phase3_fsync_durability) (passed|failed): pass_rate=[0-9.]+%" \
                /reg-logs/full_suite_main.log 2>/dev/null | sed "s/^/    /" | tee -a "${RESULT}" || true
            note "=== concurrency / crash verdicts ==="
            grep -hiE "concurrency .*(PASS|FAIL)|[0-9]+/[0-9]+ PASS|crash summary|Error: xfstests failed" \
                /reg-logs/full_suite_main.log /reg-logs/full_suite_host_crash.log 2>/dev/null | tail -15 | sed "s/^/    /" | tee -a "${RESULT}" || true
            echo "================ REGRESSION RESULT ================"
            cat "${RESULT}"
            exit 0
        fi

        if [ "${FULL_GUARD:-0}" = "1" ]; then
            # Standard guard full lists with the new cache forced active
            # (page_cache=0, direct_read_cache=0, extent_map_cache=1). These
            # modes historically pass at drc=1; passing here is a clean
            # non-regression with the extent map cache on the real suite.
            note ">>> [full-guard drc=0] phase4_good + phase3_base + jbd_phase1 (extent_map_cache=1)"
            common_env RUN_PHASE4_GOOD=1 RUN_PHASE3_BASE=1 RUN_JBD_PHASE1=1 \
                RUN_PAGECACHE_PHASE4=0 \
                PHASE4_GOOD_THRESHOLD=90 \
                EXT4_DIRECT_READ_CACHE=0 EXT4_EXTENT_MAP_CACHE=1 \
                tools/ext4/run_phase4_part3.sh > /reg-logs/full_guard_drc0.log 2>&1 \
                && note "    full_guard_drc0 rc=0" || note "    full_guard_drc0 rc=$?"
            grep -hoE "xfstests (phase4_good|phase3_base|jbd_phase1) (passed|failed): pass_rate=[0-9.]+%" \
                /reg-logs/full_guard_drc0.log 2>/dev/null | sed "s/^/        /" | tee -a "${RESULT}" || true
            echo "================ REGRESSION RESULT ================"
            cat "${RESULT}"
            exit 0
        fi

        if [ "${RUN_PAGECACHE_NONREG}" = "1" ]; then
            note ">>> [non-reg] pagecache_phase4 (default page_cache=1, drc=1)"
            common_env RUN_PHASE4_GOOD=0 RUN_PAGECACHE_PHASE4=1 \
                tools/ext4/run_phase4_part3.sh > /reg-logs/nonreg_pagecache_phase4.log 2>&1 \
                && note "    pagecache_phase4 rc=0" || note "    pagecache_phase4 rc=$?"
        fi

        for t in ${AB_TESTS}; do
            safe=$(echo "$t" | tr "/" "_")
            for drc in 1 0; do
                lbl="ab_${safe}_drc${drc}"
                note ">>> [cache-AB] ${t} page_cache=0 direct_read_cache=${drc} extent_map_cache=1"
                common_env RUN_PHASE4_GOOD=1 RUN_PAGECACHE_PHASE4=0 \
                    XFSTESTS_SINGLE_TEST="${t}" \
                    EXT4_DIRECT_READ_CACHE="${drc}" EXT4_EXTENT_MAP_CACHE=1 \
                    tools/ext4/run_phase4_part3.sh > "/reg-logs/${lbl}.log" 2>&1 \
                    && note "    ${lbl} rc=0" || note "    ${lbl} rc=$?"
                # extract per-test PASS/FAIL/NOTRUN
                grep -hoE "(Passed|Failed|Not run|Ran):.*|${t} .*(PASS|FAIL|NOTRUN)|xfstests case done.*" "/reg-logs/${lbl}.log" 2>/dev/null | tail -3 | sed "s/^/        /" | tee -a "${RESULT}" || true
            done
        done

        echo "================ REGRESSION RESULT ================"
        cat "${RESULT}"
    '

echo "phase5 regression finished. Logs: ${LOG_DIR}"
