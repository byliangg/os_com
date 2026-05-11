#!/usr/bin/env bash
# Run 3 diagnostic write benchmarks with bs=16K and fsync=4:
# raw / ext4-journaled / ext4-nojournal
# Usage: [KEEP_LOGS=1] ./run_write_16k_fsync4_summary.sh

set -euo pipefail

BENCHMARK_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ASTER_DIR="$(cd "${BENCHMARK_ROOT}/../../../.." && pwd)"
LOG_DIR="$(mktemp -d "${TMPDIR:-/tmp}/write-16k-fsync4.XXXXXX")"

cleanup() {
    if [[ "${KEEP_LOGS:-0}" != "1" ]]; then
        rm -rf "${LOG_DIR}"
    else
        echo "Logs preserved at: ${LOG_DIR}"
    fi
}
trap cleanup EXIT

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
if [[ ! -f "${ASTER_DIR}/Cargo.toml" ]]; then
    echo "Error: failed to locate Asterinas repo root: ${ASTER_DIR}" >&2
    exit 1
fi

IMAGE="${IMAGE:-asterinas/asterinas:0.17.0-20260227}"
HTTP_PROXY_VALUE="${http_proxy:-http://127.0.0.1:7890}"
HTTPS_PROXY_VALUE="${https_proxy:-http://127.0.0.1:7890}"
ALL_PROXY_VALUE="${all_proxy:-socks5://127.0.0.1:7890}"
BENCH_RUN_ONLY_VALUE="${BENCH_RUN_ONLY:-both}"
LOG_LEVEL_VALUE="${LOG_LEVEL:-error}"

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
    local result_file="$2"
    local log_file="$3"

    if [[ ! -f "${log_file}" ]]; then
        echo "${label}: MISSING log file ${log_file}" >&2
        return 1
    fi

    python3 - "$label" "$result_file" "$log_file" <<'PY'
import json
import pathlib
import re
import sys

label = sys.argv[1]
result_path = pathlib.Path(sys.argv[2])
log_path = pathlib.Path(sys.argv[3])
text = log_path.read_text(errors="replace")

matches = re.findall(r"WRITE:\s+bw=([0-9.]+)([KMGT]?i?B)/s", text)
if len(matches) < 2:
    raise SystemExit(f"{label}: failed to parse final fio WRITE bandwidth from {log_path}")

def to_mb_per_sec(value, unit):
    value = float(value)
    multipliers = {
        "B": 1 / 1_000_000,
        "KiB": 1024 / 1_000_000,
        "MiB": 1024 * 1024 / 1_000_000,
        "GiB": 1024 * 1024 * 1024 / 1_000_000,
        "TiB": 1024 * 1024 * 1024 * 1024 / 1_000_000,
        "KB": 1 / 1000,
        "MB": 1,
        "GB": 1000,
        "TB": 1_000_000,
    }
    return value * multipliers[unit]

# bench_linux_and_aster.sh runs Asterinas first and Linux second.
aster = to_mb_per_sec(*matches[0])
linux = to_mb_per_sec(*matches[1])
ratio = (aster / linux * 100.0) if linux else 0.0

if result_path.exists():
    data = json.loads(result_path.read_text())
    for item in data:
        if item.get("extra") == "aster_result":
            item["value"] = f"{aster:.3f}"
            item["unit"] = "MB/s"
        elif item.get("extra") == "linux_result":
            item["value"] = f"{linux:.3f}"
            item["unit"] = "MB/s"
    result_path.write_text(json.dumps(data, indent=2) + "\n")

print(f"{label}: Asterinas={aster:.1f} MB/s  Linux={linux:.1f} MB/s  ratio={ratio:.2f}%")
PY
}

JOBS=(
    "fio/raw_seq_write_bw_16k_fsync4"
    "fio/ext4_seq_write_bw_16k_fsync4"
    "fio/ext4_nojournal_seq_write_bw_16k_fsync4"
)

RESULT_FILES=(
    "${ASTER_DIR}/result_fio-raw_seq_write_bw_16k_fsync4.json"
    "${ASTER_DIR}/result_fio-ext4_seq_write_bw_16k_fsync4.json"
    "${ASTER_DIR}/result_fio-ext4_nojournal_seq_write_bw_16k_fsync4.json"
)

LABELS=(
    "raw_write_16k_fsync4"
    "ext4_journaled_write_16k_fsync4"
    "ext4_nojournal_write_16k_fsync4"
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
echo "===== Write 16K fsync=4 Summary ====="
for i in "${!JOBS[@]}"; do
    extract_result "${LABELS[$i]}" "${RESULT_FILES[$i]}" "${LOG_FILES[$i]}"
done
