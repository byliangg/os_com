#!/bin/bash

# SPDX-License-Identifier: MPL-2.0
#
# Regression nail for judge.sh's extent-optimize whitelist (P8b T2).
#
# e2fsck's "Inode N extent tree (at level M) could be shorter.  Optimize?"
# is a COSMETIC minimization of a VALID tree, not a crash-consistency repair
# (see the block comment in judge.sh). The seq-2 corpus first exercised deep
# extent trees that get truncated/punched back down, at which point the strict
# `? yes` grep false-reds every such crash point. judge.sh now strips ONLY that
# exact anchored line before its corruption grep. This test pins that behaviour
# so a future edit cannot silently either (a) re-red the benign optimization or
# (b) over-strip and green a real repair that happens alongside it.
#
# Two layers:
#   * Synthetic — feeds crafted e2fsck-shaped lines through the SAME strip
#     pattern judge.sh uses (kept in sync below), covering the shorter/narrower
#     branches, the anchoring, and the co-occurrence non-masking guard without
#     needing a disk image.
#   * Fixtures — real images built with the HOST ext4 driver (which reproduces
#     the identical "could be shorter", proving the condition is not ours):
#       A   pure could-be-shorter (no csum + metadata_csum variant) -> GREEN
#       B   A + a real ref-count lie (debugfs links_count)          -> RED
#           (asserted to carry BOTH the optimize prompt and the Fix?)
#       C   pristine image                                          -> GREEN
#
#   judge_optimize_test.sh   (needs loop-mount privilege, like
#   b2_uninit_gate.sh; run from the repo root inside the build container)
#
# Exit 0 = whitelist calibrated correctly; 1 = a gate failed.

set -eu

HERE=$(dirname "$(readlink -f "$0")")
WORK=$(mktemp -d "${TMPDIR:-/tmp}/judge-opt-test-XXXXXX")
MNT="$WORK/mnt"
mkdir -p "$MNT"
trap 'umount "$MNT" 2>/dev/null || true; rm -rf "$WORK"' EXIT

fail() { echo "judge_optimize_test: FAIL — $1" >&2; exit 1; }

# ---- Synthetic layer: the strip pattern MUST stay identical to judge.sh -----
# (grep -vE this, then a surviving "? yes|FIXED|CLEARED|RECONNECT" == RED.)
STRIP_RE='^Inode [0-9]+ extent tree \(at level [0-9]+\) could be (shorter|narrower)\.[[:space:]]+Optimize\? yes$'
grep -qF "$STRIP_RE" "$HERE/judge.sh" \
    || fail "STRIP_RE drifted from judge.sh — keep them identical"

verdict() { # $1 = e2fsck-shaped text -> prints GREEN/RED like judge.sh does
    local scrubbed
    scrubbed=$(printf '%s\n' "$1" | grep -vE "$STRIP_RE")
    if printf '%s\n' "$scrubbed" | grep -qE "\? yes|FIXED|CLEARED|RECONNECT"; then
        echo RED
    else
        echo GREEN
    fi
}
syn() { # $1 label $2 expected $3 text
    local got; got=$(verdict "$3")
    [ "$got" = "$2" ] && echo "syn  $1 -> $2  ok" \
        || fail "synthetic '$1' expected $2 got $got"
}
syn "shorter-only"   GREEN "Inode 12 extent tree (at level 1) could be shorter.  Optimize? yes"
syn "narrower-only"  GREEN "Inode 12 extent tree (at level 2) could be narrower.  Optimize? yes"
syn "real-fix"       RED   "Inode 12 ref count is 9, should be 1.  Fix? yes"
syn "mixed-2-lines"  RED   "$(printf 'Inode 12 extent tree (at level 1) could be shorter.  Optimize? yes\nInode 12 ref count is 9, should be 1.  Fix? yes')"
# anchoring: a Fix? that shares a physical line with the optimize text must NOT
# be stripped (unreachable in real e2fsck output, but the anchor guards it).
syn "same-line-trap" RED   "Fix? yes  Inode 12 extent tree (at level 1) could be shorter.  Optimize? yes"
# the sibling over-depth message must stay red even with an Optimize? prompt.
syn "more-shallow"   RED   "Inode 12 extent tree (at level 3) could be more shallow.  Optimize? yes"

# ---- Fixture builder --------------------------------------------------------
# `frag` grows past 4 extents (depth-1 leaf) then is punched back to a handful,
# leaving a leaf that could be inlined -> "could be shorter". $2 = extra mke2fs
# -O features (e.g. metadata_csum) to match the production feature set.
build_could_be_shorter() {
    local img=$1 extra=${2:-}
    truncate -s 128M "$img"
    mke2fs -F -q -t ext4 -b 4096 -O "extent,^has_journal,^metadata_csum${extra}" "$img"
    mount -o loop "$img" "$MNT"
    local f="$MNT/frag" i
    fallocate -l $((400 * 4096)) "$f"
    for i in $(seq 1 2 399); do
        fallocate --punch-hole --offset $((i * 4096)) --length 4096 "$f"
    done
    for i in $(seq 6 2 399); do
        fallocate --punch-hole --offset $((i * 4096)) --length 4096 "$f" 2>/dev/null || true
    done
    sync
    umount "$MNT"
}

# ---- Fixture A: pure could-be-shorter must GREEN (no-csum + metadata_csum) --
for feat in "" ",metadata_csum"; do
    A="$WORK/a$feat.img"
    build_could_be_shorter "$A" "$feat"
    LC_ALL=C e2fsck -fn "$A" 2>&1 | grep -q "could be shorter.*Optimize" \
        || fail "fixture A${feat:+ ($feat)} did not produce the optimize prompt (host ext4 changed?)"
    if "$HERE/judge.sh" "$A" >/dev/null 2>&1; then
        echo "A  pure could-be-shorter${feat:+ +metadata_csum}   -> GREEN  ok"
    else
        fail "pure could-be-shorter${feat:+ ($feat)} judged RED (whitelist not applied)"
    fi
done

# ---- Fixture B: could-be-shorter + real ref-count lie must RED --------------
B="$WORK/b.img"
build_could_be_shorter "$B"
debugfs -w -R "sif frag links_count 9" "$B" >/dev/null 2>&1 \
    || fail "debugfs could not plant the ref-count lie"
# The whole point of B is co-occurrence: assert it carries BOTH prompts, else
# it degenerates into a trivial "a ref-count lie reds" test.
BOUT=$(LC_ALL=C e2fsck -fn "$B" 2>&1 || true)
echo "$BOUT" | grep -q "could be shorter.*Optimize" \
    || fail "fixture B lost its optimize prompt — no longer tests over-strip"
echo "$BOUT" | grep -qE "ref count is .*Fix" \
    || fail "fixture B lost its ref-count Fix prompt"
if "$HERE/judge.sh" "$B" >/dev/null 2>&1; then
    fail "could-be-shorter + wrong-links judged GREEN — whitelist MASKED a real repair!"
else
    echo "B  could-be-shorter + wrong-links       -> RED   ok"
fi

# ---- Fixture C: pristine must GREEN ----------------------------------------
C="$WORK/c.img"
truncate -s 64M "$C"
mke2fs -F -q -t ext4 -b 4096 -O extent,^has_journal,metadata_csum "$C"
if "$HERE/judge.sh" "$C" >/dev/null 2>&1; then
    echo "C  pristine (metadata_csum)             -> GREEN  ok"
else
    fail "pristine image judged RED"
fi

echo "judge_optimize_test: PASS"
