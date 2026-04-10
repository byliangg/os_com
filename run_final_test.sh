#!/bin/bash
set -euo pipefail

cd /home/mafake/os_com

echo "===== 直接运行 fio 顺序读性能测试 ====="
echo ""

DOCKER_TAG=$(cat DOCKER_IMAGE_VERSION 2>/dev/null || cat VERSION)
IMAGE="asterinas/asterinas:${DOCKER_TAG}"
TOOLCHAIN="nightly-2025-12-06"
CACHE_STAMP="$HOME/.cache/os_com/fio_prewarm_${DOCKER_TAG}_${TOOLCHAIN}.ok"
RUNNER_NAME="oscom_fio_runner"

mkdir -p "$HOME/.rustup"
mkdir -p "$HOME/.cargo/registry" "$HOME/.cargo/git"
mkdir -p "$(dirname "$CACHE_STAMP")"

LOG="benchmark/logs/perf_compare/fio_ext4_seq_read_final_$(date +%Y%m%d_%H%M%S).log"
mkdir -p "$(dirname "$LOG")"
: > "$LOG"

echo "启动容器，运行测试..."
echo "预期耗时：3-5 分钟"
echo "日志文件：$LOG"
echo ""

printf '[%s] benchmark started\n' "$(date '+%F %T')" >> "$LOG"

run_with_retry() {
  local tries="$1"
  shift
  local i
  for i in $(seq 1 "$tries"); do
    if "$@"; then
      return 0
    fi
    echo "第 ${i}/${tries} 次失败，准备重试..." >&2
    if [ "$i" -lt "$tries" ]; then
      sleep 3
    fi
  done
  return 1
}

ensure_runner_container() {
  local existing_id existing_image
  existing_id=$(docker ps -aq -f "name=^/${RUNNER_NAME}$" || true)

  if [ -n "$existing_id" ]; then
    existing_image=$(docker inspect -f '{{.Config.Image}}' "$RUNNER_NAME" 2>/dev/null || true)
    if [ "$existing_image" != "$IMAGE" ]; then
      echo "检测到旧 runner 容器镜像版本不一致，重建..."
      docker rm -f "$RUNNER_NAME" >/dev/null 2>&1 || true
      existing_id=""
    fi
  fi

  if [ -z "$existing_id" ]; then
    echo "创建持久化 runner 容器（后续复用，避免每轮新建）..."
    docker run -d \
      --name "$RUNNER_NAME" \
      --privileged \
      --network=host \
      -v /dev:/dev \
      -v "$PWD":/root/asterinas \
      -v "$HOME/.rustup":/root/.rustup \
      -v "$HOME/.cargo/registry":/root/.cargo/registry \
      -v "$HOME/.cargo/git":/root/.cargo/git \
      -w /root/asterinas \
      -e RUSTUP_HOME=/root/.rustup \
      "$IMAGE" \
      bash -lc 'trap : TERM INT; while true; do sleep 3600; done' >/dev/null
  else
    if ! docker ps -q -f "name=^/${RUNNER_NAME}$" >/dev/null || [ -z "$(docker ps -q -f "name=^/${RUNNER_NAME}$")" ]; then
      echo "启动已存在的 runner 容器..."
      docker start "$RUNNER_NAME" >/dev/null
    fi
  fi
}

ensure_runner_container

if [ -f "$CACHE_STAMP" ]; then
  echo "检测到预热缓存标记，跳过预热阶段（加速本轮启动）..."
else
  echo "预热 Rust toolchain（避免 benchmark 中途下载超时）..."
  run_with_retry 3 docker exec \
    -e RUSTUP_DIST_SERVER \
    -e RUSTUP_UPDATE_ROOT \
    -e http_proxy \
    -e https_proxy \
    -e all_proxy \
    -e HTTP_PROXY \
    -e HTTPS_PROXY \
    -e ALL_PROXY \
    "$RUNNER_NAME" \
    bash -lc "set -euo pipefail
export RUSTUP_MAX_RETRIES=10
export CARGO_HTTP_TIMEOUT=180
export CARGO_NET_RETRY=10
export CARGO_NET_GIT_FETCH_WITH_CLI=true
export GIT_HTTP_LOW_SPEED_LIMIT=1024
export GIT_HTTP_LOW_SPEED_TIME=30
rustup set profile minimal
rustup toolchain install ${TOOLCHAIN} --no-self-update
rustup component add --toolchain ${TOOLCHAIN} rust-src rustc-dev llvm-tools
rustup target add --toolchain ${TOOLCHAIN} loongarch64-unknown-none-softfloat riscv64imac-unknown-none-elf x86_64-unknown-none

# Pre-fetch dependencies so benchmark phase does not appear stuck at git/index updates.
cargo +${TOOLCHAIN} fetch --manifest-path /root/asterinas/osdk/Cargo.toml
cargo +${TOOLCHAIN} fetch --manifest-path /root/asterinas/kernel/Cargo.toml
" || {
    echo "工具链预热失败，请检查代理或网络后重试。" >&2
    exit 2
  }
  : > "$CACHE_STAMP"
fi

run_with_retry 2 docker exec \
  -e RUSTUP_DIST_SERVER \
  -e RUSTUP_UPDATE_ROOT \
  -e http_proxy \
  -e https_proxy \
  -e all_proxy \
  -e HTTP_PROXY \
  -e HTTPS_PROXY \
  -e ALL_PROXY \
  -e BENCH_ENABLE_KVM=1 \
  -e BENCH_ASTER_NETDEV=tap \
  -e BENCH_ASTER_VHOST=on \
  -e BENCH_JOB="fio/ext4_seq_read_bw" \
  "$RUNNER_NAME" \
  bash -lc '
set -euo pipefail
export ASTERINAS_ROOT=/root/asterinas
export CARGO_TARGET_DIR=/root/asterinas/target_lby  
export VDSO_LIBRARY_DIR=/root/asterinas/benchmark/assets/linux_vdso
export RUSTUP_MAX_RETRIES=10
export CARGO_HTTP_TIMEOUT=180
export CARGO_NET_RETRY=10
export CARGO_NET_GIT_FETCH_WITH_CLI=true
export GIT_HTTP_LOW_SPEED_LIMIT=1024
export GIT_HTTP_LOW_SPEED_TIME=30

cd /root/asterinas

# Kill leftovers from previous interrupted runs in reused container.
# Do not match the current shell command line itself.
pkill -f "^qemu-system-x86_64" >/dev/null 2>&1 || true
pkill -f "^/root/asterinas/target_lby/debug/cargo-osdk osdk run" >/dev/null 2>&1 || true

mkdir -p /root/.cargo/bin
cat >/root/.cargo/bin/cargo-osdk << "EOS"
#!/usr/bin/env bash
set -euo pipefail
ROOT=${ASTERINAS_ROOT:-/root/asterinas}
BIN="${ROOT}/target_lby/debug/cargo-osdk"
STAMP="${ROOT}/target_lby/.cargo_osdk_local_dev"
if [ ! -x "${BIN}" ] || [ ! -f "${STAMP}" ]; then
  OSDK_LOCAL_DEV=1 cargo build --manifest-path "${ROOT}/osdk/Cargo.toml" --bin cargo-osdk
  mkdir -p "$(dirname "${STAMP}")"
  touch "${STAMP}"
fi
exec "${BIN}" "$@"
EOS
chmod +x /root/.cargo/bin/cargo-osdk

timeout 5400 bash test/initramfs/src/benchmark/bench_linux_and_aster.sh "${BENCH_JOB}" x86_64
' > "$LOG" 2>&1

echo ""
echo "测试完成！"
echo ""

if [ -f result_fio-ext4_seq_read_bw.json ]; then
  echo "===== 性能测试结果 ====="
  python3 << 'PYTHON_SCRIPT'
import json
try:
    with open('result_fio-ext4_seq_read_bw.json', 'r') as f:
        data = json.load(f)
    
    linux_bw = None
    aster_bw = None
    
    for entry in data:
        if 'Linux' in entry['name']:
            linux_bw = float(entry['value'])
        elif 'Asterinas' in entry['name']:
            aster_bw = float(entry['value'])
    
    if linux_bw and aster_bw:
        ratio = (aster_bw / linux_bw) * 100
        baseline_aster = 38.2
        improvement = ((aster_bw - baseline_aster) / baseline_aster) * 100
        
        print(f"Linux 带宽:          {linux_bw:.1f} MB/s")
        print(f"Asterinas 带宽:      {aster_bw:.1f} MB/s")
        print(f"性能比例:            {ratio:.2f}%")
        print(f"相比基线改进:        {improvement:+.1f}%")
        print("")
        
        if ratio >= 80:
            print("✓✓✓ 达到目标阈值 (>=80%)!")
        else:
            print(f"⚠ 未达到目标 (当前 {ratio:.2f}%, 目标 >=80%)")
            if improvement > 0:
                print(f"但是性能有明显改进，相比基线提升 {improvement:.1f}%")
except Exception as e:
    print(f"⚠ 无法解析结果: {e}")
PYTHON_SCRIPT
else
  echo "⚠ 结果文件未生成"
  echo "最后 100 行日志:"
  tail -100 "$LOG"
fi

echo ""
echo "完整日志: $LOG"
