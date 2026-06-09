#!/usr/bin/env bash
# SQLite real-application benchmark: sqlite-speedtest1, Asterinas vs Linux.
#
# SQLite is a real application (single-file embedded DB) exercising buffered I/O,
# frequent fsync (transaction commits) and random small reads/writes -- a very
# different profile from the synthetic O_DIRECT fio guard. Lower wall time is
# better; ratio = Linux_seconds / Asterinas_seconds.
#
# Phase 6 Step 0a "3-FS diagnostic triangle": run the same speedtest1 on our
# ext4 (journaled), the reference ext2 (PageCache buffered, NO journaling) and
# ramfs (pure in-memory, no block device), so the totals separate:
#   ext4 - ext2  = our journaling + per-page journaled-allocation net cost (attackable)
#   ext2 - ramfs = virtio round-trip + PageCache writeback (platform floor, common to all FS)
#   ramfs - Linux= framekernel per-syscall overhead
# NOTE: ext2 has NO journaling -- it is the "non-journaled writeback ceiling" and
# an implementation reference, NOT a target ext4 must match (we keep the journal
# for the crash-consistency feature requirement).
#
# Env:
#   FS_LIST           filesystems to run (default "ext4 ext2 ramfs")
#   PAGE_CACHE_LIST   ext4 page_cache configs (default "1"; only affects ext4)
#   EXT4_PHASE2_PROFILE  1 = emit the 4-layer ext4 profile (FS/JBD2/lock/bio) +
#                        the Phase 6 [ext4-bufw] buffered-write split (default 0)
#   LOG_LEVEL         guest log level (default error; set "warn" for profiling)
#   SQLITE_SIZE       speedtest1 --size (default 1000; does NOT scale row count)

set -euo pipefail

BENCHMARK_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ASTER_DIR="$(cd "${BENCHMARK_ROOT}/../../../.." && pwd)"
TS="$(date +%Y%m%d_%H%M%S)"
LOG_DIR="${LOG_DIR:-${ASTER_DIR}/benchmark/logs/sqlite_${TS}}"
PAGE_CACHE_LIST="${PAGE_CACHE_LIST:-1}"
FS_LIST="${FS_LIST:-ext4 ext2 ramfs}"
EXT4_PHASE2_PROFILE_VALUE="${EXT4_PHASE2_PROFILE:-0}"

if ! command -v docker >/dev/null 2>&1; then echo "Error: docker missing" >&2; exit 1; fi
if [[ ! -e /dev/kvm ]]; then echo "Error: /dev/kvm missing" >&2; exit 1; fi

IMAGE="${IMAGE:-asterinas/asterinas:0.17.0-20260227}"
HTTP_PROXY_VALUE="${http_proxy:-http://127.0.0.1:7890}"
HTTPS_PROXY_VALUE="${https_proxy:-http://127.0.0.1:7890}"
ALL_PROXY_VALUE="${all_proxy:-socks5://127.0.0.1:7890}"
LOG_LEVEL_VALUE="${LOG_LEVEL:-error}"

mkdir -p "${LOG_DIR}"
echo "sqlite benchmark logs: ${LOG_DIR}"
echo "  FS_LIST=${FS_LIST} PAGE_CACHE_LIST=${PAGE_CACHE_LIST} EXT4_PHASE2_PROFILE=${EXT4_PHASE2_PROFILE_VALUE} LOG_LEVEL=${LOG_LEVEL_VALUE}"

docker run --pull=never --rm --privileged --network=host --device=/dev/kvm \
    -v /dev:/dev \
    -v "${ASTER_DIR}:/root/asterinas" \
    -v "${LOG_DIR}:/sqlite-logs" \
    -w /root/asterinas \
    -e http_proxy="${HTTP_PROXY_VALUE}" -e https_proxy="${HTTPS_PROXY_VALUE}" -e all_proxy="${ALL_PROXY_VALUE}" \
    -e BENCH_ENABLE_KVM=1 -e BENCH_ASTER_NETDEV=tap -e BENCH_ASTER_VHOST=on -e BENCH_ASTER_SCHEME=null \
    -e LOG_LEVEL="${LOG_LEVEL_VALUE}" \
    -e CARGO_TARGET_DIR=/root/asterinas/.target_bench \
    -e VDSO_LIBRARY_DIR=/root/asterinas/.local/linux_vdso \
    -e LINUX_DEPENDENCIES_DIR=/root/asterinas/.cache/linux_binary_cache \
    -e PAGE_CACHE_LIST="${PAGE_CACHE_LIST}" \
    -e FS_LIST="${FS_LIST}" \
    -e EXT4_PHASE2_PROFILE="${EXT4_PHASE2_PROFILE_VALUE}" \
    -e SQLITE_SIZE="${SQLITE_SIZE:-1000}" \
    "${IMAGE}" \
    bash -lc '
        set -uo pipefail
        # Network resilience: fall back to local nix build on transient proxy TLS.
        export NIX_CONFIG="fallback = true
download-attempts = 10
connect-timeout = 20
stalled-download-timeout = 90"

        rm -rf /root/asterinas/.target_bench/osdk \
               /root/asterinas/test/initramfs/build/initramfs \
               /root/asterinas/test/initramfs/build/initramfs.cpio.gz
        OSDK_LOCAL_DEV=1 cargo install --locked cargo-osdk --path /root/asterinas/osdk --force

        SUMMARY=/sqlite-logs/sqlite_summary.tsv
        printf "fs\tpage_cache\taster_sec\tlinux_sec\tratio_pct\n" > "${SUMMARY}"

        # Map an FS name to its benchmark path and the result JSON that holds the
        # speedtest1 TOTAL wall time (ext4 uses a single-result case; ext2/ramfs
        # use the multi-subtest *_benchmarks case whose TOTAL lands in the
        # bench_results/total.json).
        fs_bench() {
            case "$1" in
                ext4)  echo "sqlite/ext4_speedtest1";;
                ext2)  echo "sqlite/ext2_benchmarks";;
                ramfs) echo "sqlite/ramfs_benchmarks";;
                *) echo "" ;;
            esac
        }
        fs_resjson() {
            case "$1" in
                ext4)  echo "result_sqlite-ext4_speedtest1.json";;
                ext2)  echo "result_sqlite-ext2_benchmarks-bench_results-total.json";;
                ramfs) echo "result_sqlite-ramfs_benchmarks-bench_results-total.json";;
                *) echo "" ;;
            esac
        }

        for fs in ${FS_LIST}; do
            bench="$(fs_bench ${fs})"
            resjson="$(fs_resjson ${fs})"
            if [ -z "${bench}" ]; then echo "skip unknown fs=${fs}" >&2; continue; fi

            # page_cache only affects ext4; for ext2/ramfs run a single iteration.
            if [ "${fs}" = "ext4" ]; then pc_list="${PAGE_CACHE_LIST}"; else pc_list="na"; fi

            for pc in ${pc_list}; do
                log="/sqlite-logs/sqlite_${fs}_pc${pc}.log"
                echo ">>> [sqlite] ${fs} ${bench} page_cache=${pc} profile=${EXT4_PHASE2_PROFILE}"
                pc_env=0; [ "${pc}" != "na" ] && pc_env="${pc}"
                EXT4_PAGE_CACHE="${pc_env}" EXT4_DIRECT_READ_CACHE=0 \
                EXT4_PHASE2_PROFILE="${EXT4_PHASE2_PROFILE}" \
                BENCH_ASTER_SCHEME=null \
                bash test/initramfs/src/benchmark/bench_linux_and_aster.sh "${bench}" x86_64 \
                    > "${log}" 2>&1 || echo "  ${fs} pc=${pc} rc=$? (see ${log})"
                pkill -9 -f qemu-system 2>/dev/null || true
                sleep 2

                res="/root/asterinas/${resjson}"
                if [ -f "${res}" ]; then
                    cp "${res}" "/sqlite-logs/result_${fs}_pc${pc}.json"
                    python3 - "${fs}" "${pc}" "${res}" >> "${SUMMARY}" <<'"'"'PY'"'"'
import json, sys, pathlib
fs, pc = sys.argv[1], sys.argv[2]
data = json.loads(pathlib.Path(sys.argv[3]).read_text())
vals = {item["extra"]: float(item["value"]) for item in data}
a = vals.get("aster_result"); l = vals.get("linux_result")
ratio = (l / a * 100.0) if (a and l) else 0.0
print(f"{fs}\t{pc}\t{a}\t{l}\t{ratio:.2f}")
PY
                else
                    echo "  ${fs} pc=${pc}: result json missing (${resjson})" >&2
                    printf "%s\t%s\tNA\tNA\tNA\n" "${fs}" "${pc}" >> "${SUMMARY}"
                fi
            done
        done

        echo "================ SQLITE SUMMARY (lower sec better; ratio=Linux/Aster) ================"
        cat "${SUMMARY}"
    '

echo "sqlite benchmark finished. Summary: ${LOG_DIR}/sqlite_summary.tsv"
