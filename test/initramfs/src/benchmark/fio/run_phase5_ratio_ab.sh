#!/usr/bin/env bash
# Phase 5 A/B ratio run for the metadata-only extent mapping cache.
#
# For each (bs, cache) runs the fio job with BENCH_RUN_ONLY=both (Asterinas
# serially first, then Linux) and parses the two bandwidth lines into an
# Asterinas/Linux ratio. Compares EXT4_EXTENT_MAP_CACHE=0 (before) vs =1 (after)
# under the cache-off guard (EXT4_PAGE_CACHE=0, EXT4_DIRECT_READ_CACHE=0).
# Baseline reference: fio_direct_parameter_sweep_report.md C group.
#
# Usage:  ./run_phase5_ratio_ab.sh

set -euo pipefail

BENCHMARK_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ASTER_DIR="$(cd "${BENCHMARK_ROOT}/../../../.." && pwd)"
TS="$(date +%Y%m%d_%H%M%S)"
LOG_DIR="${LOG_DIR:-${ASTER_DIR}/benchmark/logs/phase5_ratio_ab_${TS}}"

if ! command -v docker >/dev/null 2>&1; then echo "Error: docker missing" >&2; exit 1; fi
if [[ ! -e /dev/kvm ]]; then echo "Error: /dev/kvm missing" >&2; exit 1; fi

IMAGE="${IMAGE:-asterinas/asterinas:0.17.0-20260227}"
HTTP_PROXY_VALUE="${http_proxy:-http://127.0.0.1:7890}"
HTTPS_PROXY_VALUE="${https_proxy:-http://127.0.0.1:7890}"
ALL_PROXY_VALUE="${all_proxy:-socks5://127.0.0.1:7890}"

mkdir -p "${LOG_DIR}"
echo "phase5 ratio A/B logs: ${LOG_DIR}"

docker run --pull=never --rm --privileged --network=host --device=/dev/kvm \
    -v /dev:/dev \
    -v "${ASTER_DIR}:/root/asterinas" \
    -v "${LOG_DIR}:/ratio-logs" \
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
    -e RATIO_BS_LIST="${RATIO_BS_LIST:-4K 16K 64K 1M}" \
    -e RATIO_RUN_WRITE="${RATIO_RUN_WRITE:-1}" \
    -e RATIO_READ_JOB="${RATIO_READ_JOB:-fio/ext4_seq_read_bw}" \
    -e BENCH_DROP_CACHES="${BENCH_DROP_CACHES:-1}" \
    "${IMAGE}" \
    bash -lc '
        set -uo pipefail
        rm -rf /root/asterinas/.target_bench/osdk \
               /root/asterinas/test/initramfs/build/initramfs \
               /root/asterinas/test/initramfs/build/initramfs.cpio.gz
        OSDK_LOCAL_DEV=1 cargo install --locked cargo-osdk --path /root/asterinas/osdk --force

        SUMMARY=/ratio-logs/ratio_summary.tsv
        printf "label\trw\tbs\tcache\taster_mb_s\tlinux_mb_s\tratio_pct\n" > "${SUMMARY}"

        parse_pair() {
            python3 - "$1" "$2" <<'"'"'PY'"'"'
import re, sys, pathlib
text = pathlib.Path(sys.argv[1]).read_text(errors="replace")
text = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", text)
op = sys.argv[2]
scale = {"B":1/1e6,"kB":1/1e3,"KB":1/1e3,"KiB":1024/1e6,"MB":1.0,"MiB":1024**2/1e6,
         "GB":1e3,"GiB":1024**3/1e6,"TB":1e6,"TiB":1024**4/1e6}
pat = re.compile(rf"\b{op}: bw=([0-9.]+)([A-Za-z]+)/s(?:\s+\(([0-9.]+)([A-Za-z]+)/s\))?", re.I)
vals=[]
for v1,u1,v2,u2 in pat.findall(text):
    if v2 and u2 in scale: vals.append(float(v2)*scale[u2])
    elif u1 in scale: vals.append(float(v1)*scale[u1])
if len(vals)<2:
    print("NA\tNA\tNA"); raise SystemExit
a,l=vals[0],vals[1]
print(f"{a:.1f}\t{l:.1f}\t{(a/l*100 if l else 0):.2f}")
PY
        }

        run_ratio() {
            local label="$1" job="$2" bs="$3" cache="$4" rw="$5" op="$6"
            local log="/ratio-logs/${label}.log"
            echo ">>> [ratio] ${label}: job=${job} bs=${bs} extent_map_cache=${cache} (both)"
            EXT4_PAGE_CACHE=0 EXT4_DIRECT_READ_CACHE=0 EXT4_EXTENT_MAP_CACHE="${cache}" \
            BENCH_DROP_CACHES="${BENCH_DROP_CACHES:-1}" \
            BENCH_ASTER_SCHEME=null BENCH_FIO_BS="${bs}" BENCH_FIO_NUMJOBS=1 \
            bash test/initramfs/src/benchmark/bench_linux_and_aster.sh "${job}" x86_64 \
                > "${log}" 2>&1 || echo "  ${label} rc=$?"
            local parsed; parsed="$(parse_pair "${log}" "${op}")"
            printf "%s\t%s\t%s\t%s\t%s\n" "${label}" "${rw}" "${bs}" "${cache}" "${parsed}" >> "${SUMMARY}"
        }

        READ_JOB="${RATIO_READ_JOB:-fio/ext4_seq_read_bw}"
        for cache in 0 1; do
            for bs in ${RATIO_BS_LIST:-4K 16K 64K 1M}; do
                run_ratio "read-${bs}-c${cache}" "${READ_JOB}" "${bs}" "${cache}" read READ
            done
        done
        # no-regression: 1M write unaffected by the read cache
        if [ "${RATIO_RUN_WRITE:-1}" = "1" ]; then
            run_ratio "write-1M-c1" fio/ext4_seq_write_bw 1M 1 write WRITE
        fi

        echo "================ RATIO SUMMARY ================"
        cat "${SUMMARY}"
    '

echo "phase5 ratio A/B finished."
echo "Summary: ${LOG_DIR}/ratio_summary.tsv"
