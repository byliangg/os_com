#!/bin/bash

# SPDX-License-Identifier: MPL-2.0

set -e
set -o pipefail

# Set BENCHMARK_ROOT to the parent directory of the current directory if it is not set
BENCHMARK_ROOT="${BENCHMARK_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." &>/dev/null && pwd)}"
PROJECT_ROOT="$(cd "${BENCHMARK_ROOT}/../../../.." &>/dev/null && pwd)"
# Set the log file
LINUX_OUTPUT="${BENCHMARK_ROOT}/linux_output.txt"
ASTER_OUTPUT="${BENCHMARK_ROOT}/aster_output.txt"
# Dependencies for Linux
# Prefer a writable project-local cache dir by default; allow override.
LINUX_DEPENDENCIES_DIR="${LINUX_DEPENDENCIES_DIR:-${BENCHMARK_ROOT}/../../build/linux_binary_cache}"
LINUX_KERNEL="${LINUX_DEPENDENCIES_DIR}/vmlinuz"
LINUX_KERNEL_VERSION="6.16.0"
LINUX_MODULES_DIR="${BENCHMARK_ROOT}/../build/initramfs/lib/modules/${LINUX_KERNEL_VERSION}/kernel"
WGET_SCRIPT="${PROJECT_ROOT}/tools/atomic_wget.sh"
INITRAMFS_IMAGE="${BENCHMARK_ROOT}/../../build/initramfs.cpio.gz"
PREBUILT_INITRAMFS_IMAGE="${PROJECT_ROOT}/benchmark/assets/initramfs/initramfs_phase3.cpio.gz"

# Prepare Linux kernel and modules
prepare_libs() {
    mkdir -p "${LINUX_DEPENDENCIES_DIR}"

    # Array of files to download and their URLs
    declare -A files=(
        ["${LINUX_KERNEL}"]="https://raw.githubusercontent.com/asterinas/linux_binary_cache/24db4ff/vmlinuz-${LINUX_KERNEL_VERSION}"
    )

    # Download files if they don't exist
    for file in "${!files[@]}"; do
        if [ ! -f "$file" ]; then
            echo "Downloading ${file##*/}..."
            if [ -x "${WGET_SCRIPT}" ]; then
                "${WGET_SCRIPT}" "$file" "${files[$file]}" || {
                    echo "Failed to download ${file##*/}."
                    exit 1
                }
            elif command -v wget >/dev/null 2>&1; then
                wget -O "$file" "${files[$file]}" || {
                    echo "Failed to download ${file##*/}."
                    exit 1
                }
            elif command -v curl >/dev/null 2>&1; then
                curl -fL "${files[$file]}" -o "$file" || {
                    echo "Failed to download ${file##*/}."
                    exit 1
                }
            else
                echo "Failed to download ${file##*/}: no downloader found." >&2
                exit 1
            fi
        fi
    done
}

# Prepare fs for Linux
prepare_fs() {
    if [[ "${benchmark:-}" == */ext4_* || "${benchmark:-}" == ext4_* ]]; then
        # Ext4 benchmark: keep Linux side media as ext4.
        mkfs.ext4 -F "${BENCHMARK_ROOT}/../../build/ext2.img"
    else
        # Ext2/non-ext4 benchmark: keep historical ext2 compatibility profile.
        mke2fs -F -O ^ext_attr -O ^resize_inode -O ^dir_index "${BENCHMARK_ROOT}/../../build/ext2.img"
    fi

    if command -v nix-build >/dev/null 2>&1; then
        # nix-build --out-link needs to create this path as a symlink.
        # Remove stale regular files/symlinks from previous fallback runs.
        rm -f "${INITRAMFS_IMAGE}"
        make initramfs BENCHMARK=${benchmark}
        return
    fi

    if [ -f "${INITRAMFS_IMAGE}" ]; then
        echo "nix-build not found, reusing existing initramfs: ${INITRAMFS_IMAGE}"
        return
    fi

    if [ -f "${PREBUILT_INITRAMFS_IMAGE}" ]; then
        echo "nix-build not found, using prebuilt initramfs: ${PREBUILT_INITRAMFS_IMAGE}"
        mkdir -p "$(dirname "${INITRAMFS_IMAGE}")"
        # Remove dangling symlink target created by previous nix-build runs.
        if [ -L "${INITRAMFS_IMAGE}" ]; then
            rm -f "${INITRAMFS_IMAGE}"
        fi
        cp "${PREBUILT_INITRAMFS_IMAGE}" "${INITRAMFS_IMAGE}"
        return
    fi

    echo "Error: nix-build is not installed and no usable initramfs was found." >&2
    echo "Install Nix, or run the Docker benchmark scripts instead." >&2
    exit 1
}
