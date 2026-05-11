#!/usr/bin/env bash
# Sweep raw block-device sequential read/write bandwidth across fio block sizes.

set -euo pipefail

BENCHMARK_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ASTER_DIR="$(cd "${BENCHMARK_ROOT}/../../../.." && pwd)"
WORKSPACE_DIR="$(cd "${ASTER_DIR}/.." && pwd)"
LOG_DIR="$(mktemp -d "${TMPDIR:-/tmp}/raw-bs-sweep.XXXXXX")"
OUTPUT_FILE="${OUTPUT_FILE:-${WORKSPACE_DIR}/core_results.md}"

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
LOG_LEVEL_VALUE="${LOG_LEVEL:-error}"

BS_VALUES=(1k 2k 4k 8k 16k 32k 64k 128k 256k 512k 1024k)
BS_LIST="${BS_VALUES[*]}"

TMP_TABLE="${LOG_DIR}/table.tsv"
: > "${TMP_TABLE}"

docker run --rm --privileged --network=host --device=/dev/kvm \
    -v /dev:/dev \
    -v "${ASTER_DIR}:/root/asterinas" \
    -v "${LOG_DIR}:/raw-bs-logs" \
    -w /root/asterinas \
    -e http_proxy="${HTTP_PROXY_VALUE}" \
    -e https_proxy="${HTTPS_PROXY_VALUE}" \
    -e all_proxy="${ALL_PROXY_VALUE}" \
    -e BS_LIST="${BS_LIST}" \
    -e BENCH_ENABLE_KVM=1 \
    -e BENCH_ASTER_NETDEV=tap \
    -e BENCH_ASTER_VHOST=on \
    -e LOG_LEVEL="${LOG_LEVEL_VALUE}" \
    -e CARGO_TARGET_DIR=/root/asterinas/.target_bench \
    -e VDSO_LIBRARY_DIR=/root/asterinas/.local/linux_vdso \
    -e LINUX_DEPENDENCIES_DIR=/root/asterinas/.cache/linux_binary_cache \
    "${IMAGE}" \
    bash -lc '
        set -euo pipefail

        rm -rf /root/asterinas/test/initramfs/build/initramfs \
               /root/asterinas/test/initramfs/build/initramfs.cpio.gz
        OSDK_LOCAL_DEV=1 cargo install --locked cargo-osdk --path /root/asterinas/osdk --force

        extract_pair() {
            local log_file="$1"
            python3 - "$log_file" <<'"'"'PY'"'"'
import pathlib
import re
import sys

unit_scale = {
    "B": 1 / 1_000_000,
    "kB": 1 / 1_000,
    "KB": 1 / 1_000,
    "KiB": 1024 / 1_000_000,
    "MB": 1.0,
    "MiB": 1024 ** 2 / 1_000_000,
    "GB": 1000.0,
    "GiB": 1024 ** 3 / 1_000_000,
    "TB": 1_000_000.0,
    "TiB": 1024 ** 4 / 1_000_000,
}

text = pathlib.Path(sys.argv[1]).read_text(errors="replace")
text = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", text)
matches = re.findall(r"\b(?:READ|WRITE): bw=([0-9.]+)([A-Za-z]+?)/s", text)
if len(matches) != 2:
    raise SystemExit(f"expected 2 fio summary bandwidth lines in {sys.argv[1]}, found {len(matches)}")

vals = []
for value, unit in matches:
    if unit not in unit_scale:
        raise SystemExit(f"unknown fio bandwidth unit {unit!r} in {sys.argv[1]}")
    vals.append(float(value) * unit_scale[unit])

aster, linux = vals
ratio = (aster / linux * 100.0) if linux else 0.0
print(f"{aster:.1f}|{linux:.1f}|{ratio:.2f}%")
PY
        }

        : > /raw-bs-logs/table.tsv
        for bs in ${BS_LIST}; do
            echo ">>> Running fio/raw_seq_read_bw with bs=${bs} ..."
            read_log="/raw-bs-logs/raw_seq_read_bw_${bs}.log"
            if ! BENCH_FIO_BS="${bs}" bash test/initramfs/src/benchmark/bench_linux_and_aster.sh fio/raw_seq_read_bw x86_64 >"${read_log}" 2>&1; then
                echo "FAILED: fio/raw_seq_read_bw bs=${bs}; see ${read_log}" >&2
                exit 1
            fi
            read_result=$(extract_pair "${read_log}")

            echo ">>> Running fio/raw_seq_write_bw with bs=${bs} ..."
            write_log="/raw-bs-logs/raw_seq_write_bw_${bs}.log"
            if ! BENCH_FIO_BS="${bs}" bash test/initramfs/src/benchmark/bench_linux_and_aster.sh fio/raw_seq_write_bw x86_64 >"${write_log}" 2>&1; then
                echo "FAILED: fio/raw_seq_write_bw bs=${bs}; see ${write_log}" >&2
                exit 1
            fi
            write_result=$(extract_pair "${write_log}")

            printf "%s|%s|%s\n" "${bs}" "${read_result}" "${write_result}" >> /raw-bs-logs/table.tsv
        done
    ' || {
        KEEP_LOGS=1
        exit 1
    }

python3 - "${TMP_TABLE}" "${OUTPUT_FILE}" <<'PY'
import datetime as dt
import pathlib
import sys

table = pathlib.Path(sys.argv[1])
out = pathlib.Path(sys.argv[2])
now = dt.datetime.now().strftime("%Y-%m-%d %H:%M:%S")

lines = [
    "# Raw FIO Block Size Sweep Results",
    "",
    f"更新时间：{now}（Asia/Shanghai）",
    "",
    "## 测试口径",
    "",
    "- 测试项：`fio/raw_seq_read_bw`、`fio/raw_seq_write_bw`",
    "- filename：`/dev/vda`",
    "- 变量：`bs=1k,2k,4k,8k,16k,32k,64k,128k,256k,512k,1024k`",
    "- 固定参数：`size=1G, ioengine=sync, direct=1, numjobs=1, fsync_on_close=1, ramp_time=60, runtime=100`",
    "- 环境：Docker `asterinas/asterinas:0.17.0-20260227`，`BENCH_ENABLE_KVM=1 BENCH_ASTER_NETDEV=tap BENCH_ASTER_VHOST=on`",
    f"- 结果解析：从 fio 最终 `READ/WRITE: bw=...` 行解析并统一换算为十进制 `MB/s`；原始日志保留在 `{table.parent}`。",
    "",
    "## 结果表",
    "",
    "| bs | raw read Asterinas (MB/s) | raw read Linux (MB/s) | raw read ratio | raw write Asterinas (MB/s) | raw write Linux (MB/s) | raw write ratio |",
    "|----|--------------------------:|----------------------:|---------------:|---------------------------:|-----------------------:|----------------:|",
]

for row in table.read_text().splitlines():
    bs, ra, rl, rr, wa, wl, wr = row.split("|")
    lines.append(f"| {bs} | {ra} | {rl} | {rr} | {wa} | {wl} | {wr} |")

out.write_text("\n".join(lines) + "\n")
print(f"Wrote {out}")
PY

echo ""
cat "${OUTPUT_FILE}"
