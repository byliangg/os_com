#!/bin/bash

# SPDX-License-Identifier: MPL-2.0
#
# Calibrates the crash judge: a judge that cannot turn red is worthless, and
# one that reddens a pristine filesystem is noise. Run this before trusting
# any sweep result (and in CI before the crash matrix).
#
#   selfcheck.sh    (needs mke2fs/e2fsck/debugfs on PATH; no root)
#
# Exit 0 = the judge both greens a known-good image and reds a known-bad one.

set -eu

HERE=$(dirname "$(readlink -f "$0")")
WORK=$(mktemp -d "${TMPDIR:-/tmp}/crash-selfcheck-XXXXXX")
trap 'rm -rf "$WORK"' EXIT

GOOD=$WORK/good.img
truncate -s 64M "$GOOD"
mke2fs -F -q -t ext4 -b 4096 -I 256 \
    -O has_journal,extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,^resize_inode,^uninit_bg \
    "$GOOD"

if ! "$HERE/judge.sh" "$GOOD"; then
    echo "selfcheck: FAIL — the judge reddened a freshly mkfs'ed image" >&2
    exit 1
fi
echo "selfcheck: good image judged clean (as it must)"

# Break the filesystem the way a lost barrier would: overwrite an inode-table
# block with garbage while the superblock still claims everything is fine.
BAD=$WORK/bad.img
cp --sparse=always "$GOOD" "$BAD"
# Block 68 sits in the first group's inode table for this geometry; garbage
# there guarantees fsck errors without touching the superblock e2fsck needs.
dd if=/dev/urandom of="$BAD" bs=4096 seek=68 count=4 conv=notrunc status=none

if "$HERE/judge.sh" "$BAD" 2>/dev/null; then
    echo "selfcheck: FAIL — the judge greened a corrupted image" >&2
    exit 1
fi
echo "selfcheck: corrupted image judged damaged (as it must)"

echo "selfcheck: judge calibration OK"
