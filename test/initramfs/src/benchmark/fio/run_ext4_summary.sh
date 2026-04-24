#!/usr/bin/env bash

set -euo pipefail

BENCHMARK_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ASTER_DIR="$(cd "${BENCHMARK_ROOT}/../../../.." && pwd)"
LOG_DIR="$(mktemp -d "${TMPDIR:-/tmp}/ext4-fio-summary.XXXXXX")"

cleanup() {
    if [[ "${KEEP_LOGS:-0}" != "1" ]]; then
        rm -rf "${LOG_DIR}"
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

WRITE_LOG="${LOG_DIR}/ext4_seq_write_bw.log"
READ_LOG="${LOG_DIR}/ext4_seq_read_bw.log"

run_job() {
    local job="$1"
    local log_file="$2"

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

summarize_result() {
    local label="$1"
    local file_path="$2"

    if [[ ! -f "${file_path}" ]]; then
        echo "Error: missing benchmark result file: ${file_path}" >&2
        echo "Hint: rerun with KEEP_LOGS=1 to preserve container logs for debugging." >&2
        exit 1
    fi

    python3 - "$label" "$file_path" <<'PY'
import json
import pathlib
import sys

label = sys.argv[1]
path = pathlib.Path(sys.argv[2])
data = json.loads(path.read_text())
values = {item["extra"]: float(item["value"]) for item in data}
linux = values["linux_result"]
aster = values["aster_result"]
ratio = (aster / linux * 100.0) if linux else 0.0
print(f"{label}: Asterinas={aster:.0f} MB/s Linux={linux:.0f} MB/s ratio={ratio:.2f}%")
PY
}

if ! run_job "fio/ext4_seq_write_bw" "${WRITE_LOG}"; then
    echo "ext4 write benchmark failed. Log: ${WRITE_LOG}" >&2
    exit 1
fi

if ! run_job "fio/ext4_seq_read_bw" "${READ_LOG}"; then
    echo "ext4 read benchmark failed. Log: ${READ_LOG}" >&2
    exit 1
fi

summarize_result "ext4_seq_write_bw" "${ASTER_DIR}/result_fio-ext4_seq_write_bw.json"
summarize_result "ext4_seq_read_bw" "${ASTER_DIR}/result_fio-ext4_seq_read_bw.json"
