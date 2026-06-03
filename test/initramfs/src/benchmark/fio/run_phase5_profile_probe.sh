#!/usr/bin/env bash
# Phase 5 latency-attribution probe.
#
# Runs a focused set of O_DIRECT fio cases (Asterinas side only) with the ext4
# four-layer profiler enabled (`ext4fs.phase2_profile=1`) and `LOG_LEVEL=warn`
# so the `[ext4-direct-write]` / `[ext4-profile] direct-read` / `[ext4-phase2]`
# (warn!) and `[block-profile]` (unconditional) lines reach the captured log.
# Harvests those four line types into a per-case summary for the latency
# breakdown table. See feature_perf_phase5_plan.md §3/§Step 1.
#
# Usage:
#   ./run_phase5_profile_probe.sh            # default focused case set
#   CASES="ext4j-write-1M ext4j-write-4K" ./run_phase5_profile_probe.sh

set -euo pipefail

BENCHMARK_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ASTER_DIR="$(cd "${BENCHMARK_ROOT}/../../../.." && pwd)"
TS="$(date +%Y%m%d_%H%M%S)"
LOG_DIR="${LOG_DIR:-${ASTER_DIR}/benchmark/logs/phase5_profile_${TS}}"

if ! command -v docker >/dev/null 2>&1; then
    echo "Error: docker is not installed" >&2
    exit 1
fi
if [[ ! -e /dev/kvm ]]; then
    echo "Error: /dev/kvm is missing" >&2
    exit 1
fi

IMAGE="${IMAGE:-asterinas/asterinas:0.17.0-20260227}"
HTTP_PROXY_VALUE="${http_proxy:-http://127.0.0.1:7890}"
HTTPS_PROXY_VALUE="${https_proxy:-http://127.0.0.1:7890}"
ALL_PROXY_VALUE="${all_proxy:-socks5://127.0.0.1:7890}"
# Default focused set: verifies bottleneck (1) large-block single-job lives in
# block/virtio, and (2) small-block per-request overhead lives in ext4.
CASES_VALUE="${CASES:-ext4j-write-1M ext4j-write-4K ext4j-read-1M ext4j-read-4K raw-write-1M ext4n-write-1M}"

mkdir -p "${LOG_DIR}"
echo "phase5 profile probe logs: ${LOG_DIR}"
echo "cases: ${CASES_VALUE}"

docker run --pull=never --rm --privileged --network=host --device=/dev/kvm \
    -v /dev:/dev \
    -v "${ASTER_DIR}:/root/asterinas" \
    -v "${LOG_DIR}:/probe-logs" \
    -w /root/asterinas \
    -e http_proxy="${HTTP_PROXY_VALUE}" \
    -e https_proxy="${HTTPS_PROXY_VALUE}" \
    -e all_proxy="${ALL_PROXY_VALUE}" \
    -e BENCH_ENABLE_KVM=1 \
    -e BENCH_ASTER_NETDEV=tap \
    -e BENCH_ASTER_VHOST=on \
    -e BENCH_ASTER_SCHEME=null \
    -e BENCH_SKIP_RESULT_PARSE=1 \
    -e CARGO_TARGET_DIR=/root/asterinas/.target_bench \
    -e VDSO_LIBRARY_DIR=/root/asterinas/.local/linux_vdso \
    -e LINUX_DEPENDENCIES_DIR=/root/asterinas/.cache/linux_binary_cache \
    -e CASES_VALUE="${CASES_VALUE}" \
    -e EXT4_EXTENT_MAP_CACHE="${EXT4_EXTENT_MAP_CACHE:-1}" \
    "${IMAGE}" \
    bash -lc '
        set -uo pipefail

        rm -rf /root/asterinas/.target_bench/osdk \
               /root/asterinas/test/initramfs/build/initramfs \
               /root/asterinas/test/initramfs/build/initramfs.cpio.gz
        OSDK_LOCAL_DEV=1 cargo install --locked cargo-osdk --path /root/asterinas/osdk --force

        SUMMARY=/probe-logs/profile_summary.txt
        : > "${SUMMARY}"

        # case-name -> "fio-job bs numjobs"
        resolve() {
            case "$1" in
                ext4j-write-*)    echo "fio/ext4_seq_write_bw" ;;
                ext4j-randread-*) echo "fio/ext4_rand_read_bw" ;;
                ext4j-read-*)     echo "fio/ext4_seq_read_bw" ;;
                ext4n-write-*) echo "fio/ext4_nojournal_seq_write_bw" ;;
                ext4n-read-*)  echo "fio/ext4_nojournal_seq_read_bw" ;;
                raw-write-*)   echo "fio/raw_seq_write_bw" ;;
                raw-read-*)    echo "fio/raw_seq_read_bw" ;;
                *) echo "" ;;
            esac
        }
        bs_of() { echo "${1##*-}"; }

        run_case() {
            local name="$1"
            local job bs
            job="$(resolve "${name}")"
            bs="$(bs_of "${name}")"
            if [ -z "${job}" ]; then echo "skip unknown case ${name}"; return; fi
            local log="/probe-logs/${name}.log"
            echo ">>> [phase5-probe] ${name}: job=${job} bs=${bs} (asterinas-only, phase2_profile=1, LOG_LEVEL=warn)"
            EXT4_PHASE2_PROFILE=1 \
            LOG_LEVEL=warn \
            EXT4_PAGE_CACHE=0 \
            EXT4_DIRECT_READ_CACHE=0 \
            EXT4_EXTENT_MAP_CACHE="${EXT4_EXTENT_MAP_CACHE:-1}" \
            BENCH_RUN_ONLY=asterinas \
            BENCH_ASTER_SCHEME=null \
            BENCH_FIO_BS="${bs}" \
            BENCH_FIO_NUMJOBS=1 \
            bash test/initramfs/src/benchmark/bench_linux_and_aster.sh "${job}" x86_64 \
                > "${log}" 2>&1 || echo "  case ${name} returned rc=$?"

            {
                echo "===== ${name} (job=${job} bs=${bs}) ====="
                echo "--- [ext4-direct-write] (last) ---"
                grep -hE "\[ext4-direct-write\]" "${log}" | tail -1 || true
                echo "--- [ext4-profile] direct-read (last) ---"
                grep -hE "\[ext4-profile\] direct-read" "${log}" | tail -1 || true
                echo "--- [ext4-phase2] (last) ---"
                grep -hE "\[ext4-phase2\]" "${log}" | tail -1 || true
                echo "--- [block-profile] write-bio (last) ---"
                grep -hE "\[block-profile\] write-bio" "${log}" | tail -1 || true
                echo "--- [block-profile] read-bio (last) ---"
                grep -hE "\[block-profile\] read-bio" "${log}" | tail -1 || true
                echo ""
            } >> "${SUMMARY}"
        }

        for c in ${CASES_VALUE}; do
            run_case "${c}"
        done

        echo "================ PROFILE SUMMARY ================"
        cat "${SUMMARY}"
    '

echo "phase5 profile probe finished."
echo "Summary: ${LOG_DIR}/profile_summary.txt"
