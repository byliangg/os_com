#!/bin/bash

# SPDX-License-Identifier: MPL-2.0
#
# Independent accounting judge: recount the per-group and filesystem-wide
# allocation bookkeeping straight from the on-disk bitmaps and compare it
# with what the group descriptors and superblock declare.
#
#   judge_accounting.sh <image>
#
# Checks (all raw-parsed in python, no e2fsprogs library involved — this
# is a second opinion with an independent implementation, not a wrapper
# around dumpe2fs):
#   1. per group: declared free-block count  == zeros in the block bitmap
#      over the group's real span (last group may be partial);
#   2. per group: declared free-inode count  == zeros in the inode bitmap;
#   3. per group: bg_itable_unused invariant — no inode may be marked
#      in-use inside the "never used" tail it claims (the exact shape of
#      the bg_itable_unused bug the preen-mode judge was blind to, see
#      experience.md §12.5). Note: unused SMALLER than the free tail is
#      legal (a conservative high-water mark), so only the invariant is
#      enforced, not equality;
#   4. totals: superblock free blocks/inodes == sum over group descriptors.
#
# Exit 0 = accounting consistent; 1 = at least one mismatch; 2 = unusable
# image. UNINIT groups have synthetic bitmaps, so their bitmap recount is
# skipped; the residual checks there are INODE_UNINIT's free_i == ipg and
# BLOCK_UNINIT's free_b <= group span only — a consistently-lying free-block
# count inside a BLOCK_UNINIT group is otherwise invisible to this judge.
# e2fsck's pass-5 deep recount (from the actual inode/extent scan) remains
# the stronger oracle for that and for consistently-wrong desc+bitmap pairs.
#
# Run it on a journal-clean image (judge.sh's post-replay scratch copy —
# wire it as the oracle hook — or a cleanly unmounted fs). Read-only.

set -u

if [ $# -ne 1 ]; then
    echo "usage: judge_accounting.sh <image>" >&2
    exit 2
fi

exec python3 - "$1" <<'PYEOF'
import struct, sys

BG_INODE_UNINIT = 0x1
BG_BLOCK_UNINIT = 0x2

f = open(sys.argv[1], "rb")


def rd(off, ln):
    f.seek(off)
    d = f.read(ln)
    if len(d) != ln:
        print("judge_accounting: short read at %d" % off)
        sys.exit(2)
    return d


sb = rd(1024, 1024)
if struct.unpack_from("<H", sb, 0x38)[0] != 0xEF53:
    print("judge_accounting: bad superblock magic")
    sys.exit(2)

block_size = 1024 << struct.unpack_from("<I", sb, 0x18)[0]
first_block = struct.unpack_from("<I", sb, 0x14)[0]
bpg = struct.unpack_from("<I", sb, 0x20)[0]
ipg = struct.unpack_from("<I", sb, 0x28)[0]
incompat = struct.unpack_from("<I", sb, 0x60)[0]
ro_compat = struct.unpack_from("<I", sb, 0x64)[0]
if ro_compat & 0x200:
    print("judge_accounting: bigalloc unsupported")
    sys.exit(2)
if incompat & 0x10:
    print("judge_accounting: meta_bg unsupported")
    sys.exit(2)
is64 = bool(incompat & 0x80)
desc_size = struct.unpack_from("<H", sb, 0xFE)[0] if is64 else 32
if desc_size == 0:
    desc_size = 32
blocks = struct.unpack_from("<I", sb, 0x04)[0]
sb_free_blocks = struct.unpack_from("<I", sb, 0x0C)[0]
if is64:
    blocks |= struct.unpack_from("<I", sb, 0x150)[0] << 32
    sb_free_blocks |= struct.unpack_from("<I", sb, 0x158)[0] << 32
sb_free_inodes = struct.unpack_from("<I", sb, 0x10)[0]
groups = (blocks - first_block + bpg - 1) // bpg

bad = 0
notes = []


def flag(msg):
    global bad
    bad += 1
    print("MISMATCH: %s" % msg)


def count_free(bitmap, nbits):
    free = 0
    for i in range(nbits):
        if not bitmap[i >> 3] & (1 << (i & 7)):
            free += 1
    return free


tot_free_blocks = 0
tot_free_inodes = 0
gdt = rd((first_block + 1) * block_size, groups * desc_size)
for g in range(groups):
    d = gdt[g * desc_size : (g + 1) * desc_size]
    blk_bmp = struct.unpack_from("<I", d, 0x00)[0]
    ino_bmp = struct.unpack_from("<I", d, 0x04)[0]
    free_b = struct.unpack_from("<H", d, 0x0C)[0]
    free_i = struct.unpack_from("<H", d, 0x0E)[0]
    flags = struct.unpack_from("<H", d, 0x12)[0]
    unused = struct.unpack_from("<H", d, 0x1C)[0]
    if desc_size >= 64:
        blk_bmp |= struct.unpack_from("<I", d, 0x20)[0] << 32
        ino_bmp |= struct.unpack_from("<I", d, 0x24)[0] << 32
        free_b |= struct.unpack_from("<H", d, 0x2C)[0] << 16
        free_i |= struct.unpack_from("<H", d, 0x2E)[0] << 16
        unused |= struct.unpack_from("<H", d, 0x32)[0] << 16
    tot_free_blocks += free_b
    tot_free_inodes += free_i

    span = min(bpg, blocks - first_block - g * bpg)
    if flags & BG_BLOCK_UNINIT:
        # Synthetic bitmap: no recount possible; the span bound is the
        # only invariant left to hold the declared count to.
        if free_b > span:
            flag(
                "group %d: BLOCK_UNINIT declares %d free blocks > group span %d"
                % (g, free_b, span)
            )
        notes.append("group %d: BLOCK_UNINIT, block recount skipped (free<=span only)" % g)
    else:
        counted = count_free(rd(blk_bmp * block_size, block_size), span)
        if counted != free_b:
            flag(
                "group %d free blocks: declared %d, bitmap says %d"
                % (g, free_b, counted)
            )

    if flags & BG_INODE_UNINIT:
        if free_i != ipg:
            flag(
                "group %d: INODE_UNINIT but declares %d free inodes (want %d)"
                % (g, free_i, ipg)
            )
        notes.append("group %d: INODE_UNINIT, inode recount skipped" % g)
        continue
    ibmp = rd(ino_bmp * block_size, block_size)
    counted = count_free(ibmp, ipg)
    if counted != free_i:
        flag("group %d free inodes: declared %d, bitmap says %d" % (g, free_i, counted))
    # bg_itable_unused invariant: the last `unused` inodes of the group
    # claim to have never been used; none may be marked allocated.
    if unused > ipg:
        flag("group %d itable_unused %d exceeds inodes per group %d" % (g, unused, ipg))
    else:
        for i in range(ipg - unused, ipg):
            if ibmp[i >> 3] & (1 << (i & 7)):
                flag(
                    "group %d itable_unused %d covers allocated inode %d"
                    % (g, unused, g * ipg + i + 1)
                )
                break

if tot_free_blocks != sb_free_blocks:
    flag(
        "sb free blocks %d != sum of group descriptors %d"
        % (sb_free_blocks, tot_free_blocks)
    )
if tot_free_inodes != sb_free_inodes:
    flag(
        "sb free inodes %d != sum of group descriptors %d"
        % (sb_free_inodes, tot_free_inodes)
    )

for n in notes:
    print("note: %s" % n)
if bad:
    print("judge_accounting: %d mismatch(es) on %s" % (bad, sys.argv[1]))
    sys.exit(1)
print("judge_accounting: accounting consistent on %s" % sys.argv[1])
PYEOF
