#!/bin/bash

# SPDX-License-Identifier: MPL-2.0
#
# B2 gate: block-side lazy-group (BLOCK_UNINIT) allocation.
#
# The crash matrix only ever exercises group 0 (its ACE workloads are tiny), so
# it never proves that allocation spilling into a lazily-initialized group is
# safe. This gate builds a small multi-group metadata_csum + flex_bg image —
# whose trailing groups carry BLOCK_UNINIT / INODE_UNINIT — then has the kernel
# write a file large enough to spill out of group 0 into those groups. A host
# e2fsck then confirms the reconstructed bitmaps handed out real free blocks
# (not the backup superblock / GDT the raw uninitialized bitmap would have
# offered as "free").
#
# Run from the repo root, inside the build container.

set -eu

HERE=$(dirname "$(readlink -f "$0")")
REPO=$(readlink -f "$HERE/../..")
BUILD=$REPO/test/initramfs/build
STAGE=$(mktemp -d "${TMPDIR:-/tmp}/b2-gate-XXXXXX")
trap 'rm -rf "$STAGE"' EXIT

# One workload: write ~150 MiB (past group 0's ~120 MiB of free space on a
# 512 MiB image) so allocation is forced into the BLOCK_UNINIT trailing group,
# then fsync + sync so the journal is committed and checkpointed to disk.
mkdir -p "$STAGE/root/.crash"
cat > "$STAGE/root/.crash/bigwrite.sh" <<'WL'
dd if=/dev/zero of=spill bs=1M count=150 conv=fsync 2>/dev/null
sync
WL

cd "$REPO"
rm -f "$BUILD/xfstests_test.img" "$BUILD/xfstests_scratch.img" qemu.log
mkdir -p "$BUILD"
truncate -s 512M "$BUILD/xfstests_test.img"
mke2fs -F -q -t ext4 -b 4096 -I 256 \
    -O has_journal,extent,filetype,metadata_csum,dir_index,64bit,flex_bg,^inline_data \
    -d "$STAGE/root" "$BUILD/xfstests_test.img"
# The guest also insists SCRATCH_DEV be a real block device.
truncate -s 512M "$BUILD/xfstests_scratch.img"
mke2fs -F -q -t ext4 -b 4096 -I 256 \
    -O has_journal,extent,filetype,metadata_csum,dir_index,64bit,flex_bg,^inline_data \
    "$BUILD/xfstests_scratch.img"

echo "b2-gate: trailing-group flags on the built image:"
dumpe2fs "$BUILD/xfstests_test.img" 2>/dev/null | grep -E "^Group [0-9]+:" | head

# Boot once (no BLKLOG: the guest writes straight through to the image), run the
# spill workload, exit.
make run_kernel AUTO_TEST=conformance RELEASE=1 \
    CONFORMANCE_TEST_SUITE=xfstests XFSTESTS_DISK_SIZE=512M \
    CRASH_WORKLOADS=all 2>&1 | tail -3 || true
if ! grep -q "crash workloads done" qemu.log; then
    echo "b2-gate: guest did not finish the workload (see qemu.log)" >&2
    exit 1
fi

# The guest leaves the journal needing recovery (it never unmounts). Let e2fsck
# replay it first (our synced journal is already checkpointed, so replay is a
# no-op), capturing the output: a *correct* fs shows only journal recovery, a
# broken one shows bitmap/count corruption here.
echo "b2-gate: === e2fsck pass 1 (journal replay + check) ==="
set +e
RECOVER_OUT=$(e2fsck -fy "$BUILD/xfstests_test.img" 2>&1)
RECOVER_RC=$?
set -e
echo "$RECOVER_OUT"
if echo "$RECOVER_OUT" | grep -qiE "bitmap differences|count wrong|unused inodes count|not in use|marked in use|overlaps|blocks? in use but|invalid"; then
    echo "b2-gate: FAIL — e2fsck reported corruption/repair after the cross-group spill" >&2
    exit 1
fi

echo "b2-gate: === e2fsck pass 2 (must be clean, rc 0) ==="
e2fsck -fn "$BUILD/xfstests_test.img"
echo "b2-gate: PASS — cross-group allocation is e2fsck-clean (pass-1 rc=$RECOVER_RC)"
