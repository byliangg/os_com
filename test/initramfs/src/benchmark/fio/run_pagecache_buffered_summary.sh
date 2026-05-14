#!/usr/bin/env bash
# Run official fio buffered-I/O ext4 microbenchmarks for Phase 4 PageCache A/B.
# This script keeps the existing fio tool and benchmark harness; it only uses
# direct=0 jobs so the workload exercises buffered I/O and PageCache.

set -euo pipefail

BENCHMARK_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ASTER_DIR="$(cd "${BENCHMARK_ROOT}/../../../.." && pwd)"
TS="$(date +%Y%m%d_%H%M%S)"
LOG_DIR="${ASTER_DIR}/benchmark/logs/pagecache_buffered_fio"
SUMMARY="${LOG_DIR}/pagecache_buffered_fio_summary_${TS}.tsv"
mkdir -p "${LOG_DIR}"

if ! command -v docker >/dev/null 2>&1; then
    echo "Error: docker is not installed" >&2
    exit 1
fi

if ! command -v python3 >/dev/null 2>&1; then
    echo "Error: python3 is not installed" >&2
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
LOG_LEVEL_VALUE="${LOG_LEVEL:-error}"
EXT4_DIRECT_READ_CACHE_VALUE="${EXT4_DIRECT_READ_CACHE:-0}"
BENCH_FIO_SIZE_VALUE="${BENCH_FIO_SIZE:-1G}"

run_job() {
    local job="$1"
    local system="$2"
    local page_cache="$3"
    local log_file="$4"

    echo ">>> Running: job=${job} system=${system} page_cache=${page_cache} log=${log_file}"
    docker run --rm --privileged --network=host --device=/dev/kvm \
        -v /dev:/dev \
        -v "${ASTER_DIR}:/root/asterinas" \
        -w /root/asterinas \
        -e http_proxy="${HTTP_PROXY_VALUE}" \
        -e https_proxy="${HTTPS_PROXY_VALUE}" \
        -e all_proxy="${ALL_PROXY_VALUE}" \
        -e BENCH_RUN_ONLY="${system}" \
        -e BENCH_ENABLE_KVM=1 \
        -e BENCH_ASTER_NETDEV=tap \
        -e BENCH_ASTER_VHOST=on \
        -e LOG_LEVEL="${LOG_LEVEL_VALUE}" \
        -e EXT4_DIRECT_READ_CACHE="${EXT4_DIRECT_READ_CACHE_VALUE}" \
        -e EXT4_PAGE_CACHE="${page_cache}" \
        -e BENCH_FIO_SIZE="${BENCH_FIO_SIZE_VALUE}" \
        -e CARGO_TARGET_DIR=/root/asterinas/.target_bench \
        -e VDSO_LIBRARY_DIR=/root/asterinas/.local/linux_vdso \
        -e LINUX_DEPENDENCIES_DIR=/root/asterinas/.cache/linux_binary_cache \
        "${IMAGE}" \
        bash -lc "
            set -euo pipefail
            rm -rf /root/asterinas/.target_bench/osdk \
                   /root/asterinas/test/initramfs/build/initramfs \
                   /root/asterinas/test/initramfs/build/initramfs.cpio.gz
            OSDK_LOCAL_DEV=1 cargo install --locked cargo-osdk --path /root/asterinas/osdk --force
            bash test/initramfs/src/benchmark/bench_linux_and_aster.sh ${job} x86_64
        " >"${log_file}" 2>&1
}

extract_read_result() {
    local label="$1"
    local log_file="$2"
    python3 - "$label" "$log_file" <<'PY'
import pathlib
import re
import sys

label, log_file = sys.argv[1:3]
text = pathlib.Path(log_file).read_text(errors="replace")
text = re.sub(r"\x1b\[[0-9;]*m", "", text)
pattern = re.compile(r"\bREAD: bw=[^\n]*?\(([\d.]+)([KMGT]?)B/s\)", re.IGNORECASE)
scale = {"": 1e-6, "K": 1e-3, "M": 1.0, "G": 1e3, "T": 1e6}
values = [float(value) * scale[unit.upper()] for value, unit in pattern.findall(text)]
if len(values) < 2:
    raise SystemExit(f"{label}: failed to parse cold/warm READ bandwidth from {log_file}")
cold, warm = values[0], values[1]
gain = (warm / cold * 100.0) if cold else 0.0
print(f"{label}\tread\t{cold:.1f}\t{warm:.1f}\t{gain:.2f}\t{log_file}")
PY
}

extract_write_result() {
    local label="$1"
    local log_file="$2"
    python3 - "$label" "$log_file" <<'PY'
import pathlib
import re
import sys

label, log_file = sys.argv[1:3]
text = pathlib.Path(log_file).read_text(errors="replace")
text = re.sub(r"\x1b\[[0-9;]*m", "", text)
pattern = re.compile(r"\bWRITE: bw=[^\n]*?\(([\d.]+)([KMGT]?)B/s\)", re.IGNORECASE)
scale = {"": 1e-6, "K": 1e-3, "M": 1.0, "G": 1e3, "T": 1e6}
values = [float(value) * scale[unit.upper()] for value, unit in pattern.findall(text)]
if not values:
    raise SystemExit(f"{label}: failed to parse WRITE bandwidth from {log_file}")
print(f"{label}\twrite\t{values[-1]:.1f}\tN/A\tN/A\t{log_file}")
PY
}

READ_JOB="fio/ext4_buffered_seq_read_bw"
WRITE_JOB="fio/ext4_buffered_seq_write_bw"

LINUX_READ_LOG="${LOG_DIR}/linux_buffered_read_${TS}.log"
ASTER_PC0_READ_LOG="${LOG_DIR}/aster_pagecache0_buffered_read_${TS}.log"
ASTER_PC1_READ_LOG="${LOG_DIR}/aster_pagecache1_buffered_read_${TS}.log"
LINUX_WRITE_LOG="${LOG_DIR}/linux_buffered_write_${TS}.log"
ASTER_PC0_WRITE_LOG="${LOG_DIR}/aster_pagecache0_buffered_write_${TS}.log"
ASTER_PC1_WRITE_LOG="${LOG_DIR}/aster_pagecache1_buffered_write_${TS}.log"

run_job "${READ_JOB}" linux 0 "${LINUX_READ_LOG}"
run_job "${READ_JOB}" asterinas 0 "${ASTER_PC0_READ_LOG}"
run_job "${READ_JOB}" asterinas 1 "${ASTER_PC1_READ_LOG}"
run_job "${WRITE_JOB}" linux 0 "${LINUX_WRITE_LOG}"
run_job "${WRITE_JOB}" asterinas 0 "${ASTER_PC0_WRITE_LOG}"
run_job "${WRITE_JOB}" asterinas 1 "${ASTER_PC1_WRITE_LOG}"

{
    printf "label\tkind\tcold_or_write_MBps\twarm_MBps\twarm_vs_cold_percent\tlog\n"
    extract_read_result "linux" "${LINUX_READ_LOG}"
    extract_read_result "asterinas_page_cache_0" "${ASTER_PC0_READ_LOG}"
    extract_read_result "asterinas_page_cache_1" "${ASTER_PC1_READ_LOG}"
    extract_write_result "linux" "${LINUX_WRITE_LOG}"
    extract_write_result "asterinas_page_cache_0" "${ASTER_PC0_WRITE_LOG}"
    extract_write_result "asterinas_page_cache_1" "${ASTER_PC1_WRITE_LOG}"
} | tee "${SUMMARY}"

echo "[DONE] PageCache buffered fio summary: ${SUMMARY}"
