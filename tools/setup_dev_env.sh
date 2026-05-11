#!/usr/bin/env bash

# SPDX-License-Identifier: MPL-2.0

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VDSO_REV="7489835"
TOOLCHAIN_FILE="${ROOT_DIR}/rust-toolchain.toml"

usage() {
    cat <<'USAGE'
Usage: tools/setup_dev_env.sh [--no-vdso]

Installs the Rust components and targets expected by this Asterinas checkout,
then prepares the local VDSO directory used by kernel checks.

Options:
  --no-vdso    Skip cloning/checking .local/linux_vdso.
USAGE
}

prepare_vdso=true
while [ "$#" -gt 0 ]; do
    case "$1" in
        --no-vdso)
            prepare_vdso=false
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
    shift
done

cd "$ROOT_DIR"

toolchain="$(
    sed -n 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$TOOLCHAIN_FILE" |
        head -n 1
)"
if [ -z "$toolchain" ]; then
    echo "failed to read Rust channel from ${TOOLCHAIN_FILE}" >&2
    exit 1
fi

echo "[setup] Rust toolchain from rust-toolchain.toml: ${toolchain}"
rustup toolchain install "$toolchain"

echo "[setup] installing Rust components"
rustup component add rust-src rustfmt rustc-dev llvm-tools-preview --toolchain "$toolchain"

echo "[setup] installing Rust targets"
rustup target add \
    x86_64-unknown-none \
    riscv64imac-unknown-none-elf \
    loongarch64-unknown-none-softfloat \
    --toolchain "$toolchain"

mkdir -p .cache/linux_binary_cache .target_bench .local

if [ "$prepare_vdso" = true ]; then
    if [ ! -d .local/linux_vdso/.git ]; then
        echo "[setup] cloning linux_vdso into .local/linux_vdso"
        rm -rf .local/linux_vdso
        git clone https://github.com/asterinas/linux_vdso.git .local/linux_vdso
    fi
    echo "[setup] checking out linux_vdso ${VDSO_REV}"
    git -C .local/linux_vdso fetch --quiet origin || true
    git -C .local/linux_vdso checkout --quiet "$VDSO_REV"
fi

cat <<EOF
[setup] done

Useful environment for kernel checks:
  export VDSO_LIBRARY_DIR=${ROOT_DIR}/.local/linux_vdso
  export CARGO_TARGET_DIR=/tmp/os_com_codex_kernel_target

Example:
  VDSO_LIBRARY_DIR=${ROOT_DIR}/.local/linux_vdso \\
  CARGO_TARGET_DIR=/tmp/os_com_codex_kernel_target \\
  cargo check -p aster-kernel --target x86_64-unknown-none
EOF
