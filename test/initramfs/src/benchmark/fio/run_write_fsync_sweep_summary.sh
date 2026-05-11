#!/usr/bin/env bash
# Sweep fio write fsync=N for raw / ext4-journaled / ext4-nojournal.
# Usage: [KEEP_LOGS=1] [FSYNC_VALUES="1 2 3 4 5 6"] ./run_write_fsync_sweep_summary.sh

set -euo pipefail

BENCHMARK_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ASTER_DIR="$(cd "${BENCHMARK_ROOT}/../../../.." && pwd)"
WORKTREE_ROOT="$(dirname "${ASTER_DIR}")"
LOG_DIR="$(mktemp -d "${TMPDIR:-/tmp}/write-fsync-sweep.XXXXXX")"

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
EXT4_DIRECT_READ_CACHE_VALUE="${EXT4_DIRECT_READ_CACHE:-0}"
FIO_BS_VALUE="${BENCH_FIO_BS:-1M}"
FSYNC_VALUES_VALUE="${FSYNC_VALUES:-1 2 3 4 5 6}"
CORE_RESULTS_FILE="${CORE_RESULTS_FILE:-${WORKTREE_ROOT}/core_results.md}"

run_job() {
    local fsync_value="$1"
    local job="$2"
    local log_file="$3"

    echo ">>> Running: fsync=${fsync_value} ${job} ..."
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
        -e BENCH_FIO_BS="${FIO_BS_VALUE}" \
        -e BENCH_FIO_FSYNC="${fsync_value}" \
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

JOBS=(
    "fio/raw_seq_write_bw"
    "fio/ext4_seq_write_bw"
    "fio/ext4_nojournal_seq_write_bw"
)

LABELS=(
    "raw write"
    "ext4 journaled write"
    "ext4 nojournal write"
)

for fsync_value in ${FSYNC_VALUES_VALUE}; do
    for i in "${!JOBS[@]}"; do
        job="${JOBS[$i]}"
        log="${LOG_DIR}/fsync${fsync_value}_${job//\//_}.log"
        if ! run_job "${fsync_value}" "${job}" "${log}"; then
            echo "FAILED: fsync=${fsync_value} ${job} -- see log at ${log}" >&2
            KEEP_LOGS=1
            exit 1
        fi
    done
done

python3 - "${LOG_DIR}" "${CORE_RESULTS_FILE}" "${FSYNC_VALUES_VALUE}" "${FIO_BS_VALUE}" "${EXT4_DIRECT_READ_CACHE_VALUE}" "${BENCH_RUN_ONLY_VALUE}" "${LABELS[@]}" <<'PY'
import datetime as dt
import pathlib
import re
import sys
from zoneinfo import ZoneInfo

log_dir = pathlib.Path(sys.argv[1])
core_results = pathlib.Path(sys.argv[2])
fsync_values = sys.argv[3].split()
fio_bs = sys.argv[4]
cache_value = sys.argv[5]
bench_run_only = sys.argv[6]
labels = sys.argv[7:]
jobs = [
    "fio_raw_seq_write_bw",
    "fio_ext4_seq_write_bw",
    "fio_ext4_nojournal_seq_write_bw",
]

scale = {"": 1e-6, "K": 1e-3, "M": 1.0, "G": 1e3, "T": 1e6}

def parse_write(log_path: pathlib.Path):
    text = log_path.read_text(errors="replace")
    text = re.sub(r"\x1b\[[0-9;]*m", "", text)
    values = [
        float(value) * scale[unit.upper()]
        for value, unit in re.findall(r"\bWRITE:\s+bw=[^\n]*?\(([\d.]+)([KMGT]?)B/s\)", text, re.I)
    ]
    if not values:
        raise SystemExit(f"failed to parse WRITE bandwidth from {log_path}")
    aster = values[0]
    linux = values[1] if len(values) > 1 else None
    ratio = (aster / linux * 100.0) if linux else None
    return aster, linux, ratio

rows = []
for fsync_value in fsync_values:
    row = [fsync_value]
    for job in jobs:
        aster, linux, ratio = parse_write(log_dir / f"fsync{fsync_value}_{job}.log")
        row.extend([aster, linux, ratio])
    rows.append(row)

timestamp = dt.datetime.now(ZoneInfo("Asia/Shanghai")).strftime("%Y-%m-%d %H:%M:%S")

def fmt(value):
    return "N/A" if value is None else f"{value:.1f}"

def pct(value):
    return "N/A" if value is None else f"{value:.2f}%"

lines = [
    "",
    "# Write Fsync Sweep Results",
    "",
    f"更新时间：{timestamp}（Asia/Shanghai）",
    "",
    "## 测试口径",
    "",
    "- 测试项：`raw_seq_write_bw`、`ext4_seq_write_bw`、`ext4_nojournal_seq_write_bw`。",
    f"- 变量：`fsync=1,2,3,4,5,6`；实际本轮为 `{','.join(fsync_values)}`。",
    f"- 固定 fio 参数：`size=1G, bs={fio_bs}, ioengine=sync, direct=1, numjobs=1, fsync_on_close=1, ramp_time=60, runtime=100`，写测试额外加入 `fsync=N`。",
    f"- cache 开关：Asterinas 侧 `EXT4_DIRECT_READ_CACHE={cache_value}`；写路径 overwrite mapping reuse 保持默认。",
    "- 环境：Docker `asterinas/asterinas:0.17.0-20260227`，`BENCH_ENABLE_KVM=1 BENCH_ASTER_NETDEV=tap BENCH_ASTER_VHOST=on`。",
    f"- 运行范围：`BENCH_RUN_ONLY={bench_run_only}`。",
    f"- 结果解析：从 fio 最终 `WRITE: bw=...` 行解析括号中的十进制 `MB/s`；原始日志保留在 `{log_dir}`。",
    "",
    "## 结果表",
    "",
    "| fsync | raw Asterinas | raw Linux | raw ratio | ext4 journaled Asterinas | ext4 journaled Linux | ext4 journaled ratio | ext4 nojournal Asterinas | ext4 nojournal Linux | ext4 nojournal ratio |",
    "|------:|---------------:|----------:|----------:|-------------------------:|---------------------:|---------------------:|--------------------------:|----------------------:|----------------------:|",
]

for row in rows:
    fsync_value = row[0]
    raw_a, raw_l, raw_r = row[1:4]
    j_a, j_l, j_r = row[4:7]
    n_a, n_l, n_r = row[7:10]
    lines.append(
        f"| {fsync_value} | {fmt(raw_a)} | {fmt(raw_l)} | {pct(raw_r)} | "
        f"{fmt(j_a)} | {fmt(j_l)} | {pct(j_r)} | "
        f"{fmt(n_a)} | {fmt(n_l)} | {pct(n_r)} |"
    )

section = "\n".join(lines) + "\n"
print(section)
with core_results.open("a", encoding="utf-8") as f:
    f.write(section)
PY
