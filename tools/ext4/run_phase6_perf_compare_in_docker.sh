#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd "${ROOT_DIR}"

DOCKER_TAG=$(cat "${ROOT_DIR}/DOCKER_IMAGE_VERSION" 2>/dev/null || cat "${ROOT_DIR}/VERSION")
IMAGE_NAME=${ASTER_DOCKER_IMAGE:-"asterinas/asterinas:${DOCKER_TAG}"}

CONTAINER_WORKDIR=${CONTAINER_WORKDIR:-/root/asterinas}
CONTAINER_LOG_ROOT=${CONTAINER_LOG_ROOT:-${CONTAINER_WORKDIR}/benchmark/logs/perf_compare}

PERF_ROUNDS=${PERF_ROUNDS:-3}
PERF_RATIO_THRESHOLD=${PERF_RATIO_THRESHOLD:-0.80}
PERF_CASE_TIMEOUT_SEC=${PERF_CASE_TIMEOUT_SEC:-600}
PERF_WARMUP_BENCH=${PERF_WARMUP_BENCH:-ext4_vfs_open_lat}
PERF_WARMUP_TIMEOUT_SEC=${PERF_WARMUP_TIMEOUT_SEC:-1200}
# Comma-separated bench names without "lmbench/" prefix.
# Example: PERF_BENCHES=ext4_vfs_open_lat,ext4_copy_files_bw
PERF_BENCHES=${PERF_BENCHES:-}
BENCH_ENABLE_KVM=${BENCH_ENABLE_KVM:-0}
BENCH_ASTER_NETDEV=${BENCH_ASTER_NETDEV:-user}
BENCH_ASTER_VHOST=${BENCH_ASTER_VHOST:-off}

if ! command -v docker >/dev/null 2>&1; then
  echo "Error: docker not found." >&2
  exit 1
fi

if [ "${USE_PROXY:-0}" = "1" ]; then
  export http_proxy=${http_proxy:-http://127.0.0.1:7890}
  export https_proxy=${https_proxy:-http://127.0.0.1:7890}
  export all_proxy=${all_proxy:-socks5://127.0.0.1:7890}
fi

DOCKER_ENV_ARGS=(
  -e CONTAINER_WORKDIR="${CONTAINER_WORKDIR}"
  -e CONTAINER_LOG_ROOT="${CONTAINER_LOG_ROOT}"
  -e PERF_ROUNDS="${PERF_ROUNDS}"
  -e PERF_RATIO_THRESHOLD="${PERF_RATIO_THRESHOLD}"
  -e PERF_CASE_TIMEOUT_SEC="${PERF_CASE_TIMEOUT_SEC}"
  -e PERF_WARMUP_BENCH="${PERF_WARMUP_BENCH}"
  -e PERF_WARMUP_TIMEOUT_SEC="${PERF_WARMUP_TIMEOUT_SEC}"
  -e PERF_BENCHES="${PERF_BENCHES}"
  -e BENCH_ENABLE_KVM="${BENCH_ENABLE_KVM}"
  -e BENCH_ASTER_NETDEV="${BENCH_ASTER_NETDEV}"
  -e BENCH_ASTER_VHOST="${BENCH_ASTER_VHOST}"
)

for key in http_proxy https_proxy all_proxy HTTP_PROXY HTTPS_PROXY ALL_PROXY; do
  if [ -n "${!key:-}" ]; then
    DOCKER_ENV_ARGS+=(-e "${key}=${!key}")
  fi
done

CONTAINER_SCRIPT=$(cat <<'EOF'
set -euo pipefail

cd "${CONTAINER_WORKDIR}"
mkdir -p "${CONTAINER_LOG_ROOT}"

export PATH="/root/.cargo/bin:${PATH}"
if ! cargo osdk --version >/dev/null 2>&1; then
  echo "[INFO] cargo-osdk missing in container, creating workspace wrapper..."
  mkdir -p /root/.cargo/bin
  cat >/root/.cargo/bin/cargo-osdk <<'EOS'
#!/usr/bin/env bash
set -euo pipefail
ROOT=${ASTERINAS_ROOT:-/root/asterinas}
BIN="${ROOT}/target_lby/debug/cargo-osdk"
STAMP="${ROOT}/target_lby/.cargo_osdk_local_dev"
if [ ! -x "${BIN}" ] || [ ! -f "${STAMP}" ]; then
  if [ -x "${BIN}" ] && [ ! -f "${STAMP}" ]; then
    cargo clean --manifest-path "${ROOT}/osdk/Cargo.toml" -p cargo-osdk || true
  fi
  OSDK_LOCAL_DEV=1 cargo build --manifest-path "${ROOT}/osdk/Cargo.toml" --bin cargo-osdk
  mkdir -p "$(dirname "${STAMP}")"
  touch "${STAMP}"
fi
exec "${BIN}" "$@"
EOS
  chmod +x /root/.cargo/bin/cargo-osdk
fi

if ! command -v yq >/dev/null 2>&1; then
  echo "Error: yq not found in container PATH." >&2
  exit 2
fi
if ! command -v jq >/dev/null 2>&1; then
  echo "Error: jq not found in container PATH." >&2
  exit 2
fi

if [ -n "${PERF_BENCHES}" ]; then
  IFS=',' read -r -a benches <<<"${PERF_BENCHES}"
else
  benches=(
    ext4_vfs_open_lat
    ext4_vfs_stat_lat
    ext4_vfs_fstat_lat
    ext4_vfs_read_lat
    ext4_vfs_write_lat
    ext4_create_delete_files_0k_ops
    ext4_create_delete_files_10k_ops
    ext4_copy_files_bw
  )
fi

TS=$(date +%Y%m%d_%H%M%S)
RUN_DIR="${CONTAINER_LOG_ROOT}/${TS}"
mkdir -p "${RUN_DIR}"

DETAIL_TSV="${RUN_DIR}/phase6_perf_compare_detail.tsv"
AGG_TSV="${RUN_DIR}/phase6_perf_compare_aggregate.tsv"
REPORT_TXT="${RUN_DIR}/phase6_perf_compare_report.txt"

printf "bench\tround\tlinux\tasterinas\tbigger_is_better\tratio\tstatus\tjson\n" >"${DETAIL_TSV}"

if [ -n "${PERF_WARMUP_BENCH}" ]; then
  warmup_job="lmbench/${PERF_WARMUP_BENCH}"
  warmup_log="${RUN_DIR}/warmup_${PERF_WARMUP_BENCH}.log"
  echo "[WARMUP] job=${warmup_job} timeout=${PERF_WARMUP_TIMEOUT_SEC}s"
  set +e
  BENCH_ENABLE_KVM="${BENCH_ENABLE_KVM}" \
  BENCH_ASTER_NETDEV="${BENCH_ASTER_NETDEV}" \
  BENCH_ASTER_VHOST="${BENCH_ASTER_VHOST}" \
  timeout "${PERF_WARMUP_TIMEOUT_SEC}" \
    bash test/initramfs/src/benchmark/bench_linux_and_aster.sh "${warmup_job}" x86_64 >"${warmup_log}" 2>&1
  warmup_rc=$?
  set -e
  if [ "${warmup_rc}" -ne 0 ]; then
    echo "[WARN] warmup failed (rc=${warmup_rc}), see ${warmup_log}" >&2
  fi
fi

for round in $(seq 1 "${PERF_ROUNDS}"); do
  for bench in "${benches[@]}"; do
    job="lmbench/${bench}"
    log_file="${RUN_DIR}/${bench}_round${round}.log"
    json_file="result_lmbench-${bench}.json"
    json_copy="${RUN_DIR}/result_lmbench-${bench}_round${round}.json"
    yaml_file="test/initramfs/src/benchmark/lmbench/${bench}/bench_result.yaml"

    echo "[RUN] round=${round} job=${job}"
    rm -f "${json_file}"

    set +e
    BENCH_ENABLE_KVM="${BENCH_ENABLE_KVM}" \
    BENCH_ASTER_NETDEV="${BENCH_ASTER_NETDEV}" \
    BENCH_ASTER_VHOST="${BENCH_ASTER_VHOST}" \
    timeout "${PERF_CASE_TIMEOUT_SEC}" \
      bash test/initramfs/src/benchmark/bench_linux_and_aster.sh "${job}" x86_64 >"${log_file}" 2>&1
    rc=$?
    set -e

    if [ "${rc}" -ne 0 ] || [ ! -f "${json_file}" ]; then
      printf "%s\t%s\t-\t-\t-\t-\tFAIL\t-\n" "${bench}" "${round}" >>"${DETAIL_TSV}"
      if [ "${rc}" -eq 124 ]; then
        echo "[WARN] round=${round} job=${job} timed out (${PERF_CASE_TIMEOUT_SEC}s), see ${log_file}" >&2
      else
        echo "[WARN] round=${round} job=${job} failed (rc=${rc}), see ${log_file}" >&2
      fi
      continue
    fi

    cp "${json_file}" "${json_copy}"
    bigger=$(yq -r '.alert.bigger_is_better' "${yaml_file}")
    linux_val=$(jq -r '.[] | select(.extra=="linux_result") | .value' "${json_file}")
    aster_val=$(jq -r '.[] | select(.extra=="aster_result") | .value' "${json_file}")

    ratio=$(awk -v l="${linux_val}" -v a="${aster_val}" -v b="${bigger}" '
      BEGIN {
        l += 0.0;
        a += 0.0;
        if (l <= 0.0 || a <= 0.0) {
          print "nan";
          exit 0;
        }
        if (b == "true") {
          printf "%.6f", a / l;
        } else {
          printf "%.6f", l / a;
        }
      }')

    status=$(awk -v r="${ratio}" -v t="${PERF_RATIO_THRESHOLD}" '
      BEGIN {
        if (r == "nan") {
          print "FAIL";
        } else if (r + 0.0 >= t + 0.0) {
          print "PASS";
        } else {
          print "FAIL";
        }
      }')

    printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" \
      "${bench}" "${round}" "${linux_val}" "${aster_val}" "${bigger}" "${ratio}" "${status}" "${json_copy}" \
      >>"${DETAIL_TSV}"
  done
done

awk -F '\t' '
  NR == 1 { next }
  {
    bench = $1
    if ($6 != "nan" && $6 != "-") {
      sum[bench] += $6
      cnt[bench] += 1
    }
  }
  END {
    print "bench\trounds\tratio_avg"
    for (b in sum) {
      printf "%s\t%d\t%.6f\n", b, cnt[b], sum[b] / cnt[b]
    }
  }
' "${DETAIL_TSV}" | sort >"${AGG_TSV}"

overall_ratio=$(awk -F '\t' '
  NR == 1 { next }
  {
    if ($6 != "nan" && $6 != "-") {
      s += $6
      n += 1
    }
  }
  END {
    if (n == 0) {
      print "nan"
    } else {
      printf "%.6f", s / n
    }
  }
' "${DETAIL_TSV}")

overall_status=$(awk -v r="${overall_ratio}" -v t="${PERF_RATIO_THRESHOLD}" '
  BEGIN {
    if (r == "nan") {
      print "FAIL";
    } else if (r + 0.0 >= t + 0.0) {
      print "PASS";
    } else {
      print "FAIL";
    }
  }
')

{
  echo "phase6_perf_compare_run_dir=${RUN_DIR}"
  echo "detail_tsv=${DETAIL_TSV}"
  echo "aggregate_tsv=${AGG_TSV}"
  echo "overall_avg_ratio=${overall_ratio}"
  echo "threshold=${PERF_RATIO_THRESHOLD}"
  echo "overall_status=${overall_status}"
} | tee "${REPORT_TXT}"
EOF
)

echo "[INFO] image=${IMAGE_NAME}"
echo "[INFO] rounds=${PERF_ROUNDS} ratio_threshold=${PERF_RATIO_THRESHOLD}"
echo "[INFO] case_timeout_sec=${PERF_CASE_TIMEOUT_SEC}"
echo "[INFO] warmup_bench=${PERF_WARMUP_BENCH:-<none>} warmup_timeout_sec=${PERF_WARMUP_TIMEOUT_SEC}"
echo "[INFO] aster_kvm=${BENCH_ENABLE_KVM} aster_netdev=${BENCH_ASTER_NETDEV} aster_vhost=${BENCH_ASTER_VHOST}"
if [ -n "${PERF_BENCHES}" ]; then
  echo "[INFO] benches=${PERF_BENCHES}"
else
  echo "[INFO] benches=default_ext4_lmbench_8"
fi
echo "[INFO] workdir=${ROOT_DIR} -> ${CONTAINER_WORKDIR}"

docker run --rm --privileged --network=host \
  -v /dev:/dev \
  -v "${ROOT_DIR}:${CONTAINER_WORKDIR}" \
  -w "${CONTAINER_WORKDIR}" \
  "${DOCKER_ENV_ARGS[@]}" \
  "${IMAGE_NAME}" \
  bash -lc "${CONTAINER_SCRIPT}"
