#!/bin/bash

# SPDX-License-Identifier: MPL-2.0
#
# The basic crash matrix: converts ACE/CrashMonkey J-lang workloads to shell,
# bakes them into the (to-be-recorded) xfstests test disk, boots the kernel
# once to run them all, then reconstructs and judges every FLUSH-point crash
# state of the whole run.
#
#   run_matrix.sh <jlang-dir> [workload names...]
#
# With no names, every workload in <jlang-dir> whose ops fit the supported
# subset is taken (jlang2sh.py fails loudly on the rest; those are skipped
# and COUNTED — no silent truncation). Run from the repo root, inside the
# build container.

set -eu

if [ $# -lt 1 ]; then
    echo "usage: run_matrix.sh <jlang-dir> [names...]" >&2
    exit 2
fi

JLANG_DIR=$1
shift
HERE=$(dirname "$(readlink -f "$0")")
REPO=$(readlink -f "$HERE/../..")
BUILD=$REPO/test/initramfs/build
STAGE=$(mktemp -d "${TMPDIR:-/tmp}/crash-matrix-XXXXXX")
trap 'rm -rf "$STAGE"' EXIT

mkdir -p "$STAGE/root/.crash"
taken=0
skipped=0
if [ $# -ge 1 ]; then
    names=("$@")
else
    names=($(ls "$JLANG_DIR"))
fi
for n in "${names[@]}"; do
    if python3 "$HERE/jlang2sh.py" "$JLANG_DIR/$n" \
        > "$STAGE/root/.crash/$n.sh" 2>/dev/null; then
        taken=$((taken + 1))
    else
        rm -f "$STAGE/root/.crash/$n.sh"
        skipped=$((skipped + 1))
    fi
done
echo "matrix: $taken workloads baked, $skipped skipped (unsupported ops)"
if [ "$taken" -eq 0 ]; then
    echo "matrix: nothing to run" >&2
    exit 2
fi

# Fresh test disk, pre-populated with the workloads (mke2fs -d), and a fresh
# write log; the pristine snapshot is what every crash state replays onto.
cd "$REPO"
rm -f "$BUILD/xfstests_test.img" "$BUILD/xfstests_scratch.img" \
    "$BUILD/xfstests_test.logwrites.img" qemu.log
mkdir -p "$BUILD"
truncate -s 2G "$BUILD/xfstests_test.img"
mke2fs -F -q -t ext4 -b 4096 -I 256 \
    -O has_journal,extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,^resize_inode,^uninit_bg \
    -d "$STAGE/root" "$BUILD/xfstests_test.img"
PRISTINE=$STAGE/pristine.img
cp --sparse=always "$BUILD/xfstests_test.img" "$PRISTINE"

make run_kernel BLKLOG=on AUTO_TEST=conformance RELEASE=1 \
    CONFORMANCE_TEST_SUITE=xfstests MEM=12G XFSTESTS_DISK_SIZE=2G \
    CRASH_WORKLOADS=all 2>&1 | tail -3 || true
if ! grep -q "crash workloads done" qemu.log; then
    echo "matrix: guest did not finish the workloads (see qemu.log)" >&2
    exit 1
fi

"$HERE/sweep.sh" "$PRISTINE" "$BUILD/xfstests_test.logwrites.img"
echo "matrix: $taken workloads recorded and swept clean"
