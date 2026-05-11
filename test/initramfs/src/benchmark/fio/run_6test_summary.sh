#!/usr/bin/env bash
# Run 6 diagnostic benchmarks: raw / ext4-journaled / ext4-nojournal (read+write each)
# Usage: [KEEP_LOGS=1] [EXT4_DIRECT_READ_CACHE=0|1] ./run_6test_summary.sh
# Default EXT4_DIRECT_READ_CACHE=0 keeps the comprehensive test on the
# cache-off direct-I/O diagnostic baseline documented in benchmark.md.

set -euo pipefail

BENCHMARK_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ASTER_DIR="$(cd "${BENCHMARK_ROOT}/../../../.." && pwd)"
LOG_DIR="$(mktemp -d "${TMPDIR:-/tmp}/6test-fio-summary.XXXXXX")"

cleanup() {
    if [[ "${KEEP_LOGS:-0}" != "1" ]]; then
        rm -rf "${LOG_DIR}"
    else
        echo "Logs preserved at: ${LOG_DIR}"
    fi
}
trap cleanup EXIT

if ! command -v docker >/dev/null 2>&1; then
    echo "Error: docker is not installed" >&2; exit 1
fi
if ! command -v python3 >/dev/null 2>&1; then
    echo "Error: python3 is not installed" >&2; exit 1
fi
if [[ ! -e /dev/kvm ]]; then
    echo "Error: /dev/kvm is missing" >&2; exit 1
fi
if [[ ! -f "${ASTER_DIR}/Cargo.toml" ]]; then
    echo "Error: failed to locate Asterinas repo root: ${ASTER_DIR}" >&2; exit 1
fi

IMAGE="${IMAGE:-asterinas/asterinas:0.17.0-20260227}"
HTTP_PROXY_VALUE="${http_proxy:-http://127.0.0.1:7890}"
HTTPS_PROXY_VALUE="${https_proxy:-http://127.0.0.1:7890}"
ALL_PROXY_VALUE="${all_proxy:-socks5://127.0.0.1:7890}"
BENCH_RUN_ONLY_VALUE="${BENCH_RUN_ONLY:-both}"
LOG_LEVEL_VALUE="${LOG_LEVEL:-error}"
EXT4_DIRECT_READ_CACHE_VALUE="${EXT4_DIRECT_READ_CACHE:-0}"

run_job() {
    local job="$1"
    local log_file="$2"

    echo ">>> Running: ${job} ..."
    docker run --rm --privileged --network=host --device=/dev/kvm \
        -v /dev:/dev \
        -v "${ASTER_DIR}:/root/asterinas" \
        -w /root/asterinas \
        -e http_proxy="${HTTP_PROXY_VALUE}" \
        -e https_proxy="${HTTPS_PROXY_VALUE}" \
        -e all_proxy="${ALL_PROXY_VALUE}" \
        -e BENCH_RUN_ONLY="${BENCH_RUN_ONLY_VALUE}" \
        -e BENCH_ENABLE_KVM=1 \
        -e BENCH_ASTER_NETDEV=tap \
        -e BENCH_ASTER_VHOST=on \
        -e LOG_LEVEL="${LOG_LEVEL_VALUE}" \
        -e EXT4_DIRECT_READ_CACHE="${EXT4_DIRECT_READ_CACHE_VALUE}" \
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

extract_result() {
    local label="$1"
    local log_file="$2"
    local op="$3"

    if [[ ! -f "${log_file}" ]]; then
        echo "${label}: MISSING log file ${log_file}" >&2
        return 1
    fi

    python3 - "$label" "$log_file" "$op" <<'PY'
import pathlib, re, sys

label, log_file, op = sys.argv[1:4]
text = pathlib.Path(log_file).read_text(errors="replace")
text = re.sub(r"\x1b\[[0-9;]*m", "", text)

pattern = re.compile(rf"\b{op}: bw=[^\n]*?\(([\d.]+)([KMGT]?)B/s\)", re.IGNORECASE)
scale = {"": 1e-6, "K": 1e-3, "M": 1.0, "G": 1e3, "T": 1e6}
values = [float(value) * scale[unit.upper()] for value, unit in pattern.findall(text)]

if not values:
    raise SystemExit(f"{label}: failed to parse {op} bandwidth from {log_file}")

if len(values) == 1:
    aster = values[0]
    print(f"{label}: Asterinas={aster:.1f} MB/s  Linux=N/A  ratio=N/A")
    raise SystemExit(0)

aster, linux = values[0], values[1]
ratio = (aster / linux * 100.0) if linux else 0.0
print(f"{label}: Asterinas={aster:.1f} MB/s  Linux={linux:.1f} MB/s  ratio={ratio:.2f}%")
PY
}

JOBS=(
    "fio/raw_seq_read_bw"
    "fio/raw_seq_write_bw"
    "fio/ext4_seq_read_bw"
    "fio/ext4_seq_write_bw"
    "fio/ext4_nojournal_seq_read_bw"
    "fio/ext4_nojournal_seq_write_bw"
)

LABELS=(
    "raw_read"
    "raw_write"
    "ext4_journaled_read"
    "ext4_journaled_write"
    "ext4_nojournal_read"
    "ext4_nojournal_write"
)

OPS=(
    "READ"
    "WRITE"
    "READ"
    "WRITE"
    "READ"
    "WRITE"
)

LOG_FILES=()

for i in "${!JOBS[@]}"; do
    job="${JOBS[$i]}"
    log="${LOG_DIR}/${job//\//_}.log"
    LOG_FILES+=("${log}")
    if ! run_job "${job}" "${log}"; then
        echo "FAILED: ${job} — see log at ${log}" >&2
        KEEP_LOGS=1
        exit 1
    fi
done

echo ""
echo "===== 6-Test Summary ====="
for i in "${!JOBS[@]}"; do
    extract_result "${LABELS[$i]}" "${LOG_FILES[$i]}" "${OPS[$i]}"
done
