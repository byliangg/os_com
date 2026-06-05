#!/usr/bin/env bash
# SQLite real-application benchmark: sqlite-speedtest1 on ext4, Asterinas vs Linux.
#
# SQLite is a real application (single-file embedded DB) exercising buffered I/O,
# frequent fsync (transaction commits) and random small reads/writes -- a very
# different profile from the synthetic O_DIRECT fio guard. Runs the speedtest1
# total under both EXT4_PAGE_CACHE=1 (realistic buffered path, matches Linux page
# cache) and =0 (legacy buffered path) so we can see which the real workload
# prefers. Lower wall time is better; ratio = Linux_seconds / Asterinas_seconds.

set -euo pipefail

BENCHMARK_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ASTER_DIR="$(cd "${BENCHMARK_ROOT}/../../../.." && pwd)"
TS="$(date +%Y%m%d_%H%M%S)"
LOG_DIR="${LOG_DIR:-${ASTER_DIR}/benchmark/logs/sqlite_${TS}}"
PAGE_CACHE_LIST="${PAGE_CACHE_LIST:-1 0}"

if ! command -v docker >/dev/null 2>&1; then echo "Error: docker missing" >&2; exit 1; fi
if [[ ! -e /dev/kvm ]]; then echo "Error: /dev/kvm missing" >&2; exit 1; fi

IMAGE="${IMAGE:-asterinas/asterinas:0.17.0-20260227}"
HTTP_PROXY_VALUE="${http_proxy:-http://127.0.0.1:7890}"
HTTPS_PROXY_VALUE="${https_proxy:-http://127.0.0.1:7890}"
ALL_PROXY_VALUE="${all_proxy:-socks5://127.0.0.1:7890}"
LOG_LEVEL_VALUE="${LOG_LEVEL:-error}"

mkdir -p "${LOG_DIR}"
echo "sqlite benchmark logs: ${LOG_DIR} (page_cache configs: ${PAGE_CACHE_LIST})"

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
        printf "page_cache\taster_sec\tlinux_sec\tratio_pct\n" > "${SUMMARY}"

        for pc in ${PAGE_CACHE_LIST}; do
            log="/sqlite-logs/sqlite_pc${pc}.log"
            echo ">>> [sqlite] speedtest1 ext4 page_cache=${pc} (buffered, fsync-heavy real workload)"
            EXT4_PAGE_CACHE="${pc}" EXT4_DIRECT_READ_CACHE=0 \
            BENCH_ASTER_SCHEME=null \
            bash test/initramfs/src/benchmark/bench_linux_and_aster.sh sqlite/ext4_speedtest1 x86_64 \
                > "${log}" 2>&1 || echo "  page_cache=${pc} rc=$? (see ${log})"
            pkill -9 -f qemu-system 2>/dev/null || true
            sleep 2

            res=/root/asterinas/result_sqlite-ext4_speedtest1.json
            if [ -f "${res}" ]; then
                cp "${res}" "/sqlite-logs/result_pc${pc}.json"
                python3 - "${pc}" "${res}" >> "${SUMMARY}" <<'"'"'PY'"'"'
import json, sys, pathlib
pc = sys.argv[1]
data = json.loads(pathlib.Path(sys.argv[2]).read_text())
vals = {item["extra"]: float(item["value"]) for item in data}
a = vals.get("aster_result"); l = vals.get("linux_result")
ratio = (l / a * 100.0) if (a and l) else 0.0
print(f"{pc}\t{a}\t{l}\t{ratio:.2f}")
PY
            else
                echo "  page_cache=${pc}: result json missing" >&2
                printf "%s\tNA\tNA\tNA\n" "${pc}" >> "${SUMMARY}"
            fi
        done

        echo "================ SQLITE SUMMARY (lower sec better; ratio=Linux/Aster) ================"
        cat "${SUMMARY}"
    '

echo "sqlite benchmark finished. Summary: ${LOG_DIR}/sqlite_summary.tsv"
