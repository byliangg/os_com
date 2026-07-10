#!/bin/bash

# SPDX-License-Identifier: MPL-2.0
#
# Extended judge calibration: the original selfcheck.sh proves the strict
# e2fsck judge can red on ONE corruption shape (garbage in the inode
# table). P6 taught us that a judge with a green self-check can still have
# whole blind classes (experience.md §12.5: preen-repairable damage;
# §12.3: checksum-formula bugs). This script builds one sample per
# formerly-blind class and demands that, for each, the DESIGNATED new
# judge reds — and records which of the whole judge fleet
# (judge.sh strict e2fsck / judge_accounting.sh / judge_csum.py /
# judge_structdiff.sh) catches what, which is the point of judge
# diversification (P8_plan §2.5).
#
# Samples (all built on a mke2fs'ed metadata_csum image; group-descriptor
# and inode checksums are RE-STAMPED VALID after each lie, exactly like a
# buggy kernel that faithfully checksums whatever wrong value it writes):
#   S1 wrong per-group free-blocks count        -> judge_accounting must red
#   S2 corrupted group-descriptor checksum      -> judge_csum must red
#   S3 stale bg_itable_unused covering used inodes -> judge_accounting must red
#   S4 corrupted directory dirent-tail checksum -> judge_csum must red
#   S5 clobbered link count (csum restamped)    -> judge_structdiff must red
#
# Also demands every judge greens the pristine base image and a pristine
# full-feature (64bit+flex_bg+metadata_csum+dir_index) image.
#
#   selfcheck_extra.sh   (mke2fs/e2fsck/debugfs/python3 on PATH; no root)
#
# Exit 0 = fleet calibration OK; 1 = a gate failed.

set -eu

HERE=$(dirname "$(readlink -f "$0")")
WORK=$(mktemp -d "${TMPDIR:-/tmp}/crash-selfcheck-extra-XXXXXX")
trap 'rm -rf "$WORK"' EXIT

# ---------------------------------------------------------------- fixtures

SEED=$WORK/seed
mkdir -p "$SEED/sub"
dd if=/dev/urandom of="$SEED/f1" bs=4096 count=8 status=none
echo hello >"$SEED/f2"
for i in 1 2 3; do echo "g$i" >"$SEED/sub/g$i"; done
ln -s f1 "$SEED/ln1"

GOOD=$WORK/good.img
truncate -s 64M "$GOOD"
mke2fs -F -q -t ext4 -b 4096 -I 256 \
    -O has_journal,extent,filetype,metadata_csum,^dir_index,^64bit,^flex_bg,^resize_inode \
    -d "$SEED" "$GOOD"

FULL=$WORK/full.img
truncate -s 128M "$FULL"
mke2fs -F -q -t ext4 -b 4096 -I 256 \
    -O has_journal,extent,filetype,metadata_csum,dir_index,64bit,flex_bg \
    -d "$SEED" "$FULL"

REF_MANIFEST=$WORK/good.manifest
"$HERE/judge_structdiff.sh" "$GOOD" >"$REF_MANIFEST"

# patch <img> <op> [args] — lies with valid checksums, via the calibrated
# formulas of judge_csum.py (which double-checks them against e2fsprogs).
patch() {
    local img=$1
    shift
    JUDGE_DIR="$HERE" PYTHONDONTWRITEBYTECODE=1 python3 - "$img" "$@" <<'PYEOF'
import os, struct, sys

sys.path.insert(0, os.environ["JUDGE_DIR"])
from judge_csum import Fs, crc32c

img, op = sys.argv[1], sys.argv[2]
fs = Fs(img)
f = open(img, "r+b")
gdt_base = (fs.first_data_block + 1) * fs.block_size


def wr(off, data):
    f.seek(off)
    f.write(data)


def restamp_gd(group):
    # Re-read the descriptor from disk, recompute, store at bg_checksum.
    fs2 = Fs(img)
    wr(
        gdt_base + group * fs.desc_size + 0x1E,
        struct.pack("<H", fs2.gd_csum(group)),
    )


if op == "bump-free-blocks":
    off = gdt_base + 0x0C
    (v,) = struct.unpack("<H", fs.read_abs(off, 2))
    wr(off, struct.pack("<H", v + 7))
    f.flush()
    restamp_gd(0)
elif op == "clobber-bg-csum":
    off = gdt_base + 0x1E
    (v,) = struct.unpack("<H", fs.read_abs(off, 2))
    wr(off, struct.pack("<H", v ^ 0xBEEF))
elif op == "stale-itable-unused":
    wr(gdt_base + 0x1C, struct.pack("<H", fs.inodes_per_group - 4))
    f.flush()
    restamp_gd(0)
elif op == "break-dir-tail":
    blk = int(sys.argv[3])
    off = blk * fs.block_size + fs.block_size - 4
    (v,) = struct.unpack("<I", fs.read_abs(off, 4))
    wr(off, struct.pack("<I", v ^ 0xDEADBEEF))
elif op == "clobber-links":
    ino, links = int(sys.argv[3]), int(sys.argv[4])
    g = (ino - 1) // fs.inodes_per_group
    idx = (ino - 1) % fs.inodes_per_group
    ioff = fs.descs[g]["inode_table"] * fs.block_size + idx * fs.inode_size
    wr(ioff + 0x1A, struct.pack("<H", links))
    f.flush()
    fs2 = Fs(img)
    raw = fs2.raw_inode(ino)
    c, _ = fs2.inode_csum(ino, raw)
    wr(ioff + 0x7C, struct.pack("<H", c & 0xFFFF))
    extra = struct.unpack_from("<H", raw, 0x80)[0]
    if fs.inode_size > 128 and extra >= 4:
        wr(ioff + 0x82, struct.pack("<H", (c >> 16) & 0xFFFF))
else:
    sys.exit("unknown op " + op)
f.close()
PYEOF
}

SUB_BLK=$(debugfs -R "blocks /sub" "$GOOD" 2>/dev/null | tr -dc '0-9')
F1_INO=$(debugfs -R "stat /f1" "$GOOD" 2>/dev/null | sed -n 's/Inode: \([0-9]*\).*/\1/p')

make_sample() { # $1 = name, rest = patch args
    local name=$1
    shift
    cp --sparse=always "$GOOD" "$WORK/$name.img"
    patch "$WORK/$name.img" "$@"
}
make_sample S1-wrong-free-count bump-free-blocks
make_sample S2-bad-bg-csum clobber-bg-csum
make_sample S3-stale-itable-unused stale-itable-unused
make_sample S4-dir-tail-csum break-dir-tail "$SUB_BLK"
make_sample S5-links-clobber clobber-links "$F1_INO" 7

# ------------------------------------------------------------------ matrix

declare -A RESULT
run_fleet() { # $1 = row name, $2 = image
    local row=$1 img=$2 rc
    "$HERE/judge.sh" "$img" >/dev/null 2>&1 && rc=GREEN || rc=RED
    RESULT[$row,e2fsck]=$rc
    "$HERE/judge_accounting.sh" "$img" >/dev/null 2>&1 && rc=GREEN || rc=RED
    RESULT[$row,accounting]=$rc
    python3 "$HERE/judge_csum.py" --quiet "$img" >/dev/null 2>&1 && rc=GREEN || rc=RED
    RESULT[$row,csum]=$rc
    "$HERE/judge_structdiff.sh" "$img" "$REF_MANIFEST" >/dev/null 2>&1 && rc=GREEN || rc=RED
    RESULT[$row,structdiff]=$rc
}

ROWS="good-base S1-wrong-free-count S2-bad-bg-csum S3-stale-itable-unused S4-dir-tail-csum S5-links-clobber"
run_fleet good-base "$GOOD"
for s in $ROWS; do
    [ "$s" = good-base ] && continue
    run_fleet "$s" "$WORK/$s.img"
done

# Full-feature pristine image: structdiff has a different tree, judge the
# other three only.
"$HERE/judge.sh" "$FULL" >/dev/null 2>&1 && FULL_E2=GREEN || FULL_E2=RED
"$HERE/judge_accounting.sh" "$FULL" >/dev/null 2>&1 && FULL_ACC=GREEN || FULL_ACC=RED
python3 "$HERE/judge_csum.py" --quiet "$FULL" >/dev/null 2>&1 && FULL_CSUM=GREEN || FULL_CSUM=RED

printf '%-24s %-14s %-12s %-8s %s\n' sample e2fsck-strict accounting csum structdiff
printf '%-24s %-14s %-12s %-8s %s\n' good-fullfeat "$FULL_E2" "$FULL_ACC" "$FULL_CSUM" -
for s in $ROWS; do
    printf '%-24s %-14s %-12s %-8s %s\n' "$s" \
        "${RESULT[$s,e2fsck]}" "${RESULT[$s,accounting]}" \
        "${RESULT[$s,csum]}" "${RESULT[$s,structdiff]}"
done

# ------------------------------------------------------------------- gates

fail=0
must() { # $1 row, $2 judge, $3 want
    if [ "${RESULT[$1,$2]}" != "$3" ]; then
        echo "selfcheck_extra: FAIL — $1 / $2 is ${RESULT[$1,$2]}, want $3" >&2
        fail=1
    fi
}
for j in e2fsck accounting csum structdiff; do
    must good-base "$j" GREEN
done
for v in "$FULL_E2" "$FULL_ACC" "$FULL_CSUM"; do
    if [ "$v" != GREEN ]; then
        echo "selfcheck_extra: FAIL — full-feature pristine image judged RED" >&2
        fail=1
    fi
done
must S1-wrong-free-count accounting RED
must S2-bad-bg-csum csum RED
must S3-stale-itable-unused accounting RED
must S4-dir-tail-csum csum RED
must S5-links-clobber structdiff RED
for s in $ROWS; do
    [ "$s" = good-base ] && continue
    if ! [ "${RESULT[$s,e2fsck]}" = RED ] && ! [ "${RESULT[$s,accounting]}" = RED ] &&
        ! [ "${RESULT[$s,csum]}" = RED ] && ! [ "${RESULT[$s,structdiff]}" = RED ]; then
        echo "selfcheck_extra: FAIL — no judge in the fleet reds $s" >&2
        fail=1
    fi
done

e2red=0
for s in $ROWS; do
    [ "$s" = good-base ] && continue
    [ "${RESULT[$s,e2fsck]}" = RED ] && e2red=1
done
if [ $e2red -eq 0 ]; then
    echo "selfcheck_extra: WARNING — the strict e2fsck judge reddened NONE of" >&2
    echo "  the corruption samples. judge.sh pins LC_ALL=C internally, so a" >&2
    echo "  caller locale can no longer cause this (historical cause, fixed" >&2
    echo "  in the same commit that widened the repair-verb grep). If this" >&2
    echo '  fires, suspect the e2fsck build or the "? yes" verdict grep.' >&2
fi

if [ $fail -ne 0 ]; then
    exit 1
fi
echo "selfcheck_extra: judge fleet calibration OK"
