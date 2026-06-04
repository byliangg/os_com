#!/usr/bin/env bash
# Run the ext4 fio parameter sweep in a single Docker container.

set -euo pipefail

BENCHMARK_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ASTER_DIR="$(cd "${BENCHMARK_ROOT}/../../../.." && pwd)"
WORKSPACE_DIR="$(cd "${ASTER_DIR}/.." && pwd)"
TS="$(date +%Y%m%d_%H%M%S)"
LOG_DIR="${LOG_DIR:-${ASTER_DIR}/benchmark/logs/fio_parameter_sweep_${TS}}"
SUMMARY_TSV="${SUMMARY_TSV:-${LOG_DIR}/fio_parameter_sweep_summary.tsv}"

if ! command -v docker >/dev/null 2>&1; then
    echo "Error: docker is not installed" >&2
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
BENCH_ASTER_SCHEME_VALUE="${BENCH_ASTER_SCHEME:-null}"

mkdir -p "${LOG_DIR}"

echo "fio parameter sweep logs: ${LOG_DIR}"
echo "summary: ${SUMMARY_TSV}"

docker run --pull=never --rm --privileged --network=host --device=/dev/kvm \
    -v /dev:/dev \
    -v "${ASTER_DIR}:/root/asterinas" \
    -v "${LOG_DIR}:/fio-sweep-logs" \
    -w /root/asterinas \
    -e http_proxy="${HTTP_PROXY_VALUE}" \
    -e https_proxy="${HTTPS_PROXY_VALUE}" \
    -e all_proxy="${ALL_PROXY_VALUE}" \
    -e BENCH_RUN_ONLY=both \
    -e BENCH_ENABLE_KVM=1 \
    -e BENCH_ASTER_NETDEV=tap \
    -e BENCH_ASTER_VHOST=on \
    -e BENCH_ASTER_SCHEME="${BENCH_ASTER_SCHEME_VALUE}" \
    -e BENCH_SKIP_RESULT_PARSE=1 \
    -e RUN_G_CORRECTNESS="${RUN_G_CORRECTNESS:-1}" \
    -e LOG_LEVEL="${LOG_LEVEL_VALUE}" \
    -e CARGO_TARGET_DIR=/root/asterinas/.target_bench \
    -e VDSO_LIBRARY_DIR=/root/asterinas/.local/linux_vdso \
    -e LINUX_DEPENDENCIES_DIR=/root/asterinas/.cache/linux_binary_cache \
    "${IMAGE}" \
    bash -lc '
        set -euo pipefail

        rm -rf /root/asterinas/.target_bench/osdk \
               /root/asterinas/test/initramfs/build/initramfs \
               /root/asterinas/test/initramfs/build/initramfs.cpio.gz
        OSDK_LOCAL_DEV=1 cargo install --locked cargo-osdk --path /root/asterinas/osdk --force

        SUMMARY=/fio-sweep-logs/fio_parameter_sweep_summary.tsv
        printf "group\tcase\ttarget\tjournal\trw\tdirect\tpage_cache\tdirect_read_cache\tbs\tnumjobs\tfsync\tasterinas_mb_s\tlinux_mb_s\tratio_pct\tlog\tnote\n" > "${SUMMARY}"

        sanitize() {
            printf "%s" "$1" | tr "/ :" "___"
        }

        append_row() {
            local group="$1" case_name="$2" target="$3" journal="$4" rw="$5"
            local direct="$6" page_cache="$7" direct_read_cache="$8" bs="$9"
            local numjobs="${10}" fsync="${11}" aster="${12}" linux="${13}"
            local ratio="${14}" log_file="${15}" note="${16}"
            printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" \
                "${group}" "${case_name}" "${target}" "${journal}" "${rw}" "${direct}" \
                "${page_cache}" "${direct_read_cache}" "${bs}" "${numjobs}" "${fsync}" \
                "${aster}" "${linux}" "${ratio}" "${log_file}" "${note}" >> "${SUMMARY}"
        }

        extract_direct_pair() {
            local log_file="$1" op="$2"
            python3 - "$log_file" "$op" <<'"'"'PY'"'"'
import pathlib
import re
import sys

path = pathlib.Path(sys.argv[1])
op = sys.argv[2]
text = path.read_text(errors="replace")
text = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", text)

scale = {
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

pattern = re.compile(
    rf"\b{op}: bw=([0-9.]+)([A-Za-z]+)/s(?:\s+\(([0-9.]+)([A-Za-z]+)/s\))?",
    re.IGNORECASE,
)
values = []
for first_value, first_unit, second_value, second_unit in pattern.findall(text):
    if second_value and second_unit in scale:
        values.append(float(second_value) * scale[second_unit])
    elif first_unit in scale:
        values.append(float(first_value) * scale[first_unit])
    else:
        raise SystemExit(f"unknown fio bandwidth unit in {path}: {first_unit!r}/{second_unit!r}")

if len(values) < 2:
    raise SystemExit(f"expected at least 2 {op} bandwidth lines in {path}, found {len(values)}")

aster, linux = values[0], values[1]
ratio = aster / linux * 100.0 if linux else 0.0
print(f"{aster:.1f}\t{linux:.1f}\t{ratio:.2f}")
PY
        }

        extract_buffered_read_rows() {
            local log_file="$1"
            python3 - "$log_file" <<'"'"'PY'"'"'
import pathlib
import re
import sys

path = pathlib.Path(sys.argv[1])
text = path.read_text(errors="replace")
text = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", text)
scale = {
    "B": 1 / 1_000_000,
    "kB": 1 / 1_000,
    "KB": 1 / 1_000,
    "KiB": 1024 / 1_000_000,
    "MB": 1.0,
    "MiB": 1024 ** 2 / 1_000_000,
    "GB": 1000.0,
    "GiB": 1024 ** 3 / 1_000_000,
}
pattern = re.compile(r"\bREAD: bw=([0-9.]+)([A-Za-z]+)/s(?:\s+\(([0-9.]+)([A-Za-z]+)/s\))?", re.I)
values = []
for first_value, first_unit, second_value, second_unit in pattern.findall(text):
    if second_value and second_unit in scale:
        values.append(float(second_value) * scale[second_unit])
    else:
        values.append(float(first_value) * scale[first_unit])
if len(values) < 4:
    raise SystemExit(f"expected at least 4 READ bandwidth lines in buffered read log {path}, found {len(values)}")
for aster, linux in ((values[0], values[2]), (values[1], values[3])):
    ratio = aster / linux * 100.0 if linux else 0.0
    print(f"{aster:.1f}\t{linux:.1f}\t{ratio:.2f}")
PY
        }

        run_case() {
            local group="$1" case_name="$2" target="$3" journal="$4" rw="$5" direct="$6"
            local page_cache="$7" direct_read_cache="$8" bs="$9" numjobs="${10}"
            local fsync="${11}" benchmark="${12}" op="${13}" parse_mode="${14}"
            local scheme="${15:-${BENCH_ASTER_SCHEME:-null}}"
            local safe_case
            safe_case=$(sanitize "${case_name}")
            local log_file="/fio-sweep-logs/${safe_case}.log"
            local fsync_env=()
            if [ "${fsync}" != "none" ]; then
                fsync_env=(BENCH_FIO_FSYNC="${fsync}")
            fi

            echo ">>> [${group}] ${case_name}: benchmark=${benchmark} bs=${bs} numjobs=${numjobs} fsync=${fsync} page_cache=${page_cache} direct_read_cache=${direct_read_cache} scheme=${scheme}"
            if ! env \
                BENCH_FIO_BS="${bs}" \
                BENCH_FIO_NUMJOBS="${numjobs}" \
                EXT4_PAGE_CACHE="${page_cache}" \
                EXT4_DIRECT_READ_CACHE="${direct_read_cache}" \
                BENCH_ASTER_SCHEME="${scheme}" \
                "${fsync_env[@]}" \
                bash test/initramfs/src/benchmark/bench_linux_and_aster.sh "${benchmark}" x86_64 >"${log_file}" 2>&1; then
                append_row "${group}" "${case_name}" "${target}" "${journal}" "${rw}" "${direct}" "${page_cache}" "${direct_read_cache}" "${bs}" "${numjobs}" "${fsync}" "FAIL" "FAIL" "FAIL" "${log_file}" "benchmark failed"
                echo "FAILED: ${case_name}; see ${log_file}" >&2
                exit 1
            fi

            if [ "${parse_mode}" = "buffered_read" ]; then
                mapfile -t rows < <(extract_buffered_read_rows "${log_file}")
                IFS=$'"'"'\t'"'"' read -r aster linux ratio <<<"${rows[0]}"
                append_row "${group}" "${case_name}-cold" "${target}" "${journal}" "read-cold" "${direct}" "${page_cache}" "${direct_read_cache}" "${bs}" "${numjobs}" "${fsync}" "${aster}" "${linux}" "${ratio}" "${log_file}" ""
                IFS=$'"'"'\t'"'"' read -r aster linux ratio <<<"${rows[1]}"
                append_row "${group}" "${case_name}-warm" "${target}" "${journal}" "read-warm" "${direct}" "${page_cache}" "${direct_read_cache}" "${bs}" "${numjobs}" "${fsync}" "${aster}" "${linux}" "${ratio}" "${log_file}" ""
            else
                local parsed aster linux ratio
                parsed=$(extract_direct_pair "${log_file}" "${op}")
                IFS=$'"'"'\t'"'"' read -r aster linux ratio <<<"${parsed}"
                append_row "${group}" "${case_name}" "${target}" "${journal}" "${rw}" "${direct}" "${page_cache}" "${direct_read_cache}" "${bs}" "${numjobs}" "${fsync}" "${aster}" "${linux}" "${ratio}" "${log_file}" ""
            fi
        }

        BS_VALUES=(4K 16K 64K 256K 1M 4M)
        NUMJOBS_VALUES=(1 2 4)
        FSYNC_VALUES=(none 4 16 64)

        # A. Official/cache-off direct guard.
        run_case A A1-ext4j-write ext4 journaled write 1 0 0 1M 1 none fio/ext4_seq_write_bw WRITE direct
        run_case A A2-ext4j-read ext4 journaled read 1 0 0 1M 1 none fio/ext4_seq_read_bw READ direct

        # B. 6-test layered diagnosis.
        run_case B B1-raw-write raw none write 1 0 0 1M 1 none fio/raw_seq_write_bw WRITE direct
        run_case B B2-raw-read raw none read 1 0 0 1M 1 none fio/raw_seq_read_bw READ direct
        run_case B B3-ext4j-write ext4 journaled write 1 0 0 1M 1 none fio/ext4_seq_write_bw WRITE direct
        run_case B B4-ext4j-read ext4 journaled read 1 0 0 1M 1 none fio/ext4_seq_read_bw READ direct
        run_case B B5-ext4n-write ext4 nojournal write 1 0 0 1M 1 none fio/ext4_nojournal_seq_write_bw WRITE direct
        run_case B B6-ext4n-read ext4 nojournal read 1 0 0 1M 1 none fio/ext4_nojournal_seq_read_bw READ direct

        # C. bs sweep.
        for bs in "${BS_VALUES[@]}"; do
            run_case C "C-W-raw-${bs}" raw none write 1 0 0 "${bs}" 1 none fio/raw_seq_write_bw WRITE direct
            run_case C "C-R-raw-${bs}" raw none read 1 0 0 "${bs}" 1 none fio/raw_seq_read_bw READ direct
            run_case C "C-W-ext4j-${bs}" ext4 journaled write 1 0 0 "${bs}" 1 none fio/ext4_seq_write_bw WRITE direct
            run_case C "C-R-ext4j-${bs}" ext4 journaled read 1 0 0 "${bs}" 1 none fio/ext4_seq_read_bw READ direct
            run_case C "C-W-ext4n-${bs}" ext4 nojournal write 1 0 0 "${bs}" 1 none fio/ext4_nojournal_seq_write_bw WRITE direct
            run_case C "C-R-ext4n-${bs}" ext4 nojournal read 1 0 0 "${bs}" 1 none fio/ext4_nojournal_seq_read_bw READ direct
        done

        # D. direct/cache comparison.
        run_case D D1-write ext4 journaled write 1 0 0 1M 1 none fio/ext4_seq_write_bw WRITE direct
        run_case D D1-read ext4 journaled read 1 0 0 1M 1 none fio/ext4_seq_read_bw READ direct
        run_case D D2-write ext4 journaled write 0 0 0 1M 1 none fio/ext4_buffered_seq_write_bw WRITE direct
        run_case D D2-read ext4 journaled read 0 0 0 1M 1 none fio/ext4_buffered_seq_read_bw READ buffered_read
        run_case D D3-write ext4 journaled write 0 1 0 1M 1 none fio/ext4_buffered_seq_write_bw WRITE direct
        run_case D D3-read ext4 journaled read 0 1 0 1M 1 none fio/ext4_buffered_seq_read_bw READ buffered_read
        run_case D D4-write ext4 journaled write 1 1 0 1M 1 none fio/ext4_seq_write_bw WRITE direct
        run_case D D4-read ext4 journaled read 1 1 0 1M 1 none fio/ext4_seq_read_bw READ direct

        # E. fsync sweep.
        for bs in 16K 1M; do
            for fsync in "${FSYNC_VALUES[@]}"; do
                run_case E "E-raw-${bs}-${fsync}" raw none write 1 0 0 "${bs}" 1 "${fsync}" fio/raw_seq_write_bw WRITE direct
                run_case E "E-ext4j-${bs}-${fsync}" ext4 journaled write 1 0 0 "${bs}" 1 "${fsync}" fio/ext4_seq_write_bw WRITE direct
                run_case E "E-ext4n-${bs}-${fsync}" ext4 nojournal write 1 0 0 "${bs}" 1 "${fsync}" fio/ext4_nojournal_seq_write_bw WRITE direct
            done
        done

        # F. numjobs sweep.
        for numjobs in "${NUMJOBS_VALUES[@]}"; do
            run_case F "F-raw-write-nj${numjobs}" raw none write 1 0 0 1M "${numjobs}" none fio/raw_seq_write_bw WRITE direct
            run_case F "F-ext4j-write-nj${numjobs}" ext4 journaled write 1 0 0 1M "${numjobs}" none fio/ext4_seq_write_bw WRITE direct
            run_case F "F-ext4n-write-nj${numjobs}" ext4 nojournal write 1 0 0 1M "${numjobs}" none fio/ext4_nojournal_seq_write_bw WRITE direct
            run_case F "F-raw-read-nj${numjobs}" raw none read 1 0 0 1M "${numjobs}" none fio/raw_seq_read_bw READ direct
            run_case F "F-ext4j-read-nj${numjobs}" ext4 journaled read 1 0 0 1M "${numjobs}" none fio/ext4_seq_read_bw READ direct
            run_case F "F-ext4n-read-nj${numjobs}" ext4 nojournal read 1 0 0 1M "${numjobs}" none fio/ext4_nojournal_seq_read_bw READ direct
        done

        if [ "${RUN_G_CORRECTNESS:-1}" = "1" ]; then
            G_LOG_DIR=/fio-sweep-logs/G_correctness
            mkdir -p "${G_LOG_DIR}"

            echo ">>> [G] Preparing Phase 4 part3 initramfs for correctness regression"
            tools/ext4/prepare_phase4_part3_initramfs.sh \
                /root/asterinas/benchmark/assets/initramfs/initramfs_phase3.cpio.gz \
                /root/asterinas/benchmark/assets/initramfs/initramfs_phase4_part3.cpio.gz

            echo ">>> [G] Running phase3/phase4/phase6/jbd/concurrency/crash/pagecache correctness regression"
            env \
                VDSO_LIBRARY_DIR=/root/asterinas/benchmark/assets/linux_vdso \
                CARGO_TARGET_DIR=/root/asterinas/.target_bench \
                BOOT_METHOD=qemu-direct OVMF=off RELEASE_LTO=1 \
                ENABLE_KVM=1 NETDEV=tap VHOST=on CONSOLE=ttyS0 KLOG_LEVEL=error \
                LOG_DIR="${G_LOG_DIR}" \
                INITRAMFS_IMG=/root/asterinas/benchmark/assets/initramfs/initramfs_phase4_part3.cpio.gz \
                BASE_INITRAMFS=/root/asterinas/benchmark/assets/initramfs/initramfs_phase3.cpio.gz \
                PHASE4_GOOD_THRESHOLD=90 PAGECACHE_PHASE4_THRESHOLD=100 PHASE6_GOOD_THRESHOLD=90 \
                CRASH_ROUNDS=2 CRASH_PREPARE_WAIT_SEC=180 CRASH_HOLD_STAGE=after_commit \
                CRASH_SCENARIOS="" CRASH_EXPECT=committed \
                XFSTESTS_SINGLE_TEST="" XFSTESTS_TEST_LIST_OVERRIDE="" \
                XFSTESTS_IGNORE_STATIC_EXCLUDED_FOR_SINGLE=0 XFSTESTS_CASE_TIMEOUT_SEC=1200 \
                XFSTESTS_TRACE_RUN=0 XFSTESTS_CHILD_XTRACE=0 XFSTESTS_RUN_TIMEOUT_SEC=5400 \
                XFSTESTS_XFS_IO_DEBUG=0 XFSTESTS_SPARSE_PROBE_LOG=0 \
                XFSTESTS_TEST_IMG_SIZE=2G XFSTESTS_SCRATCH_IMG_SIZE=2G \
                EXT4_PHASE2_CASES="multi_file_write_verify,multi_file_read_write,create_unlink_churn,rename_churn,write_truncate_fsync,unlink_while_open,allocator_churn" \
                EXT4_PHASE2_WORKERS=4 EXT4_PHASE2_ROUNDS=8 EXT4_PHASE2_SEED=78 EXT4_PHASE2_TIMEOUT_SEC=900 \
                RUN_CRASH_SUITE=1 RUN_PHASE4_GOOD=1 RUN_PAGECACHE_PHASE4=1 RUN_PHASE3_BASE=1 \
                RUN_PHASE6_GOOD=1 RUN_LMBENCH=0 RUN_JBD_PHASE1=1 RUN_PHASE2_CONCURRENCY=1 \
                RUN_JBD_PHASE3=1 \
                tools/ext4/run_phase4_part3.sh 2>&1 | tee "${G_LOG_DIR}/G_correctness_full.log"

            echo ">>> [G] Running Phase 3 host-crash fsync matrix"
            env \
                VDSO_LIBRARY_DIR=/root/asterinas/benchmark/assets/linux_vdso \
                CARGO_TARGET_DIR=/root/asterinas/.target_bench \
                BOOT_METHOD=qemu-direct OVMF=off RELEASE_LTO=1 \
                ENABLE_KVM=1 NETDEV=tap VHOST=on CONSOLE=ttyS0 KLOG_LEVEL=error \
                LOG_DIR="${G_LOG_DIR}" \
                INITRAMFS_IMG=/root/asterinas/benchmark/assets/initramfs/initramfs_phase4_part3.cpio.gz \
                BASE_INITRAMFS=/root/asterinas/benchmark/assets/initramfs/initramfs_phase3.cpio.gz \
                PHASE4_GOOD_THRESHOLD=90 PAGECACHE_PHASE4_THRESHOLD=100 PHASE6_GOOD_THRESHOLD=90 \
                CRASH_ROUNDS=1 CRASH_PREPARE_WAIT_SEC=180 CRASH_HOLD_STAGE=prepare_done \
                CRASH_SCENARIOS="host_crash_fsync_size_durability:prepare_done host_crash_fdatasync_metadata:prepare_done host_crash_rename_fsync_dst:prepare_done host_crash_concurrent_fsync:prepare_done" \
                CRASH_EXPECT=committed \
                XFSTESTS_SINGLE_TEST="" XFSTESTS_TEST_LIST_OVERRIDE="" \
                XFSTESTS_IGNORE_STATIC_EXCLUDED_FOR_SINGLE=0 XFSTESTS_CASE_TIMEOUT_SEC=1200 \
                XFSTESTS_TRACE_RUN=0 XFSTESTS_CHILD_XTRACE=0 XFSTESTS_RUN_TIMEOUT_SEC=5400 \
                XFSTESTS_XFS_IO_DEBUG=0 XFSTESTS_SPARSE_PROBE_LOG=0 \
                XFSTESTS_TEST_IMG_SIZE=2G XFSTESTS_SCRATCH_IMG_SIZE=2G \
                EXT4_PHASE2_CASES="multi_file_write_verify,multi_file_read_write,create_unlink_churn,rename_churn,write_truncate_fsync,unlink_while_open,allocator_churn" \
                EXT4_PHASE2_WORKERS=4 EXT4_PHASE2_ROUNDS=8 EXT4_PHASE2_SEED=78 EXT4_PHASE2_TIMEOUT_SEC=900 \
                RUN_CRASH_SUITE=1 RUN_PHASE4_GOOD=0 RUN_PAGECACHE_PHASE4=0 RUN_PHASE3_BASE=0 \
                RUN_PHASE6_GOOD=0 RUN_LMBENCH=0 RUN_JBD_PHASE1=0 RUN_PHASE2_CONCURRENCY=0 \
                RUN_JBD_PHASE3=0 \
                tools/ext4/run_phase4_part3.sh 2>&1 | tee "${G_LOG_DIR}/G_host_crash_fsync.log"
        else
            echo ">>> [G] correctness regression skipped because RUN_G_CORRECTNESS=${RUN_G_CORRECTNESS:-0}"
        fi
    '

echo "fio parameter sweep finished."
echo "Summary TSV: ${SUMMARY_TSV}"
