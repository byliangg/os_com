#!/usr/bin/env bash
# Phase 5 official single-job guard, fair baseline with repeats + median.
#
# bs=1M numjobs=1, read + write, current honest config (cache-off:
# EXT4_PAGE_CACHE=0, EXT4_DIRECT_READ_CACHE=0; extent_map_cache=1 default) with
# the drop-cache fair baseline (BENCH_DROP_CACHES=1 default). Runs REPEATS times
# and reports per-run ratios + median, so the Linux baseline noise is averaged.

set -euo pipefail

BENCHMARK_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ASTER_DIR="$(cd "${BENCHMARK_ROOT}/../../../.." && pwd)"
TS="$(date +%Y%m%d_%H%M%S)"
LOG_DIR="${LOG_DIR:-${ASTER_DIR}/benchmark/logs/phase5_guard_median_${TS}}"
REPEATS="${REPEATS:-3}"

if ! command -v docker >/dev/null 2>&1; then echo "Error: docker missing" >&2; exit 1; fi
if [[ ! -e /dev/kvm ]]; then echo "Error: /dev/kvm missing" >&2; exit 1; fi

IMAGE="${IMAGE:-asterinas/asterinas:0.17.0-20260227}"
HTTP_PROXY_VALUE="${http_proxy:-http://127.0.0.1:7890}"
HTTPS_PROXY_VALUE="${https_proxy:-http://127.0.0.1:7890}"
ALL_PROXY_VALUE="${all_proxy:-socks5://127.0.0.1:7890}"

mkdir -p "${LOG_DIR}"
echo "phase5 guard median logs: ${LOG_DIR} (REPEATS=${REPEATS})"

docker run --pull=never --rm --privileged --network=host --device=/dev/kvm \
    -v /dev:/dev \
    -v "${ASTER_DIR}:/root/asterinas" \
    -v "${LOG_DIR}:/guard-logs" \
    -w /root/asterinas \
    -e http_proxy="${HTTP_PROXY_VALUE}" -e https_proxy="${HTTPS_PROXY_VALUE}" -e all_proxy="${ALL_PROXY_VALUE}" \
    -e BENCH_ENABLE_KVM=1 -e BENCH_ASTER_NETDEV=tap -e BENCH_ASTER_VHOST=on -e BENCH_ASTER_SCHEME=null \
    -e BENCH_SKIP_RESULT_PARSE=1 -e CARGO_TARGET_DIR=/root/asterinas/.target_bench \
    -e VDSO_LIBRARY_DIR=/root/asterinas/.local/linux_vdso \
    -e LINUX_DEPENDENCIES_DIR=/root/asterinas/.cache/linux_binary_cache \
    -e REPEATS="${REPEATS}" \
    -e READ_BS_LIST="${READ_BS_LIST:-4K 16K 64K 256K 1M}" \
    -e WRITE_BS_LIST="${WRITE_BS_LIST:-4K 1M}" \
    -e READ_JOB="${READ_JOB:-fio/ext4_seq_read_bw}" \
    -e WRITE_JOB="${WRITE_JOB:-fio/ext4_seq_write_bw}" \
    "${IMAGE}" \
    bash -lc '
        set -uo pipefail
        # Network resilience: a transient proxy TLS blip during a per-case
        # initramfs rebuild must fall back to local building, not error out.
        export NIX_CONFIG="fallback = true
download-attempts = 10
connect-timeout = 20
stalled-download-timeout = 90"
        rm -rf /root/asterinas/.target_bench/osdk
        OSDK_LOCAL_DEV=1 cargo install --locked cargo-osdk --path /root/asterinas/osdk --force

        SUMMARY=/guard-logs/guard_summary.tsv
        printf "run\tbs\trw\taster_mb_s\tlinux_mb_s\tratio_pct\n" > "${SUMMARY}"

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
if len(vals)<2: print("NA\tNA\tNA"); raise SystemExit
a,l=vals[0],vals[1]
print(f"{a:.1f}\t{l:.1f}\t{(a/l*100 if l else 0):.2f}")
PY
        }

        run_one() {
            local run="$1" job="$2" rw="$3" op="$4" bs="$5"
            local log="/guard-logs/run${run}_${rw}_${bs}.log"
            echo ">>> [guard] run=${run} ${rw} bs=${bs} nj=1 (cache-off, extent_map_cache=1, drop=1)"
            EXT4_PAGE_CACHE=0 EXT4_DIRECT_READ_CACHE=0 EXT4_EXTENT_MAP_CACHE=1 \
            BENCH_DROP_CACHES=1 BENCH_ASTER_SCHEME=null BENCH_FIO_BS="${bs}" BENCH_FIO_NUMJOBS=1 \
            timeout --kill-after=30 "${CASE_TIMEOUT_SEC:-900}" \
            bash test/initramfs/src/benchmark/bench_linux_and_aster.sh "${job}" x86_64 \
                > "${log}" 2>&1 || echo "  run${run}_${rw}_${bs} rc=$? (timeout/err)"
            # Reap any QEMU left behind by a timed-out (hung) case so the next
            # case is not blocked.
            pkill -9 -f qemu-system 2>/dev/null || true
            sleep 2
            local parsed; parsed="$(parse_pair "${log}" "${op}")"
            printf "%s\t%s\t%s\t%s\n" "${run}" "${bs}" "${rw}" "${parsed}" >> "${SUMMARY}"
        }

        for r in $(seq 1 "${REPEATS}"); do
            for bs in ${READ_BS_LIST:-4K 16K 64K 256K 1M}; do
                run_one "${r}" "${READ_JOB:-fio/ext4_seq_read_bw}" read READ "${bs}"
            done
            for bs in ${WRITE_BS_LIST:-4K 1M}; do
                run_one "${r}" "${WRITE_JOB:-fio/ext4_seq_write_bw}" write WRITE "${bs}"
            done
        done

        echo "================ GUARD SUMMARY (per run) ================"
        cat "${SUMMARY}"
        echo "================ MEDIAN ================"
        python3 - "${SUMMARY}" <<'"'"'PY'"'"'
import sys, statistics, csv
rows=list(csv.DictReader(open(sys.argv[1]), delimiter="\t"))
def order(bs):
    u={"K":1,"M":1024}; return int(bs[:-1])*u.get(bs[-1],1)
keys=sorted({(r["bs"],r["rw"]) for r in rows}, key=lambda k:(0 if k[1]=="read" else 1, order(k[0])))
for bs,rw in keys:
    rs=[float(x["ratio_pct"]) for x in rows if x["bs"]==bs and x["rw"]==rw and x["ratio_pct"] not in ("NA","")]
    a=[float(x["aster_mb_s"]) for x in rows if x["bs"]==bs and x["rw"]==rw and x["aster_mb_s"] not in ("NA","")]
    l=[float(x["linux_mb_s"]) for x in rows if x["bs"]==bs and x["rw"]==rw and x["linux_mb_s"] not in ("NA","")]
    if rs:
        print(f"{rw:5s} {bs:4s}: median_ratio={statistics.median(rs):6.2f}%  aster={statistics.median(a):7.1f}  linux={statistics.median(l):7.1f}  runs={[round(x,1) for x in rs]}")
PY
    '

echo "phase5 guard median finished. Summary: ${LOG_DIR}/guard_summary.tsv"
