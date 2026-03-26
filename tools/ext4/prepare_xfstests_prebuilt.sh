#!/usr/bin/env bash

set -euo pipefail

if ! command -v git >/dev/null 2>&1; then
  echo "git is required" >&2
  exit 1
fi

if ! command -v make >/dev/null 2>&1; then
  echo "make is required" >&2
  exit 1
fi

ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
OUT_DIR=${1:-"${ROOT_DIR}/.local/xfstests-prebuilt"}
SRC_DIR=${2:-"${ROOT_DIR}/.local/xfstests-src"}
JOBS=${JOBS:-$(nproc)}

mkdir -p "${OUT_DIR}" "${SRC_DIR}"

if [ ! -d "${SRC_DIR}/.git" ]; then
  rm -rf "${SRC_DIR}"
  git clone https://git.kernel.org/pub/scm/fs/xfs/xfstests-dev.git "${SRC_DIR}"
fi

pushd "${SRC_DIR}" >/dev/null
git pull --ff-only || true

if [ -f ./configure ]; then
  ./configure --prefix="${OUT_DIR}/xfstests-dev"
fi

make -j"${JOBS}" || true
make install prefix="${OUT_DIR}/xfstests-dev" || true
popd >/dev/null

mkdir -p "${OUT_DIR}/tools/bin"
for cmd in bash awk sed grep find xargs mount umount mkfs.ext4 mke2fs dumpe2fs blkid stat chmod chown chgrp ln rm mkdir rmdir cp mv sync sleep; do
  if command -v "${cmd}" >/dev/null 2>&1; then
    ln -sf "$(command -v "${cmd}")" "${OUT_DIR}/tools/bin/${cmd}"
  fi
done

if [ -x "${SRC_DIR}/check" ]; then
  rm -rf "${OUT_DIR}/xfstests-dev"
  mkdir -p "${OUT_DIR}/xfstests-dev"
  cp -a "${SRC_DIR}"/* "${OUT_DIR}/xfstests-dev/"
fi

echo "Prepared xfstests prebuilt at: ${OUT_DIR}"
echo "Export with:"
echo "  export XFSTESTS_PREBUILT_DIR=${OUT_DIR}"
