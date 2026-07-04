#!/bin/bash

# SPDX-License-Identifier: MPL-2.0
#
# Judges one crash state: a disk image reconstructed by replaying the
# blklogwrites write-log prefix up to some FLUSH point (see sweep.sh).
#
#   judge.sh <crash.img> [oracle.sh [oracle args...]]
#
# Exit 0 = the state is crash-consistent; exit 1 = it is not (structural
# damage that journal replay cannot repair, or the data oracle failed).
#
# The judgement pipeline is all-official (see test.md §5.3-5.4): let e2fsck
# replay the journal — exactly what the kernel would do on the next mount —
# then demand a fully clean fsck. The replay MUST happen on a scratch copy: the
# caller keeps appending log entries to <crash.img> for the next crash point,
# and a repaired-in-place image would corrupt the simulation.
#
# Strictness note: the replay pass must NOT be a preen (`e2fsck -p`) run. Preen
# mode silently REPAIRS post-replay corruption (wrong bitmaps, stale
# itable_unused, bad group checksums) before the verification fsck ever sees it,
# masking exactly the crash-inconsistency this judge exists to catch — the
# crash matrix was blind to a real `bg_itable_unused` bug for this reason. So we
# run a single `-fy` pass and FAIL if it repaired ANYTHING beyond replaying the
# journal (every repair prints `Fix? yes` / `FIXED`; journal recovery does not).

set -u

if [ $# -lt 1 ]; then
    echo "usage: judge.sh <crash.img> [oracle.sh ...]" >&2
    exit 2
fi

IMG=$1
shift

WORK=$(mktemp "${TMPDIR:-/tmp}/crash-judge-XXXXXX.img")
trap 'rm -f "$WORK"' EXIT
cp --sparse=always "$IMG" "$WORK"

# Replay the journal and check, in one strict non-interactive pass. `-fy`
# auto-answers, so its exit code alone cannot distinguish journal replay from a
# corruption repair (both set the "fixed" bit); we judge on the output instead.
OUT=$(e2fsck -fy "$WORK" 2>&1)
rc=$?
if [ "$rc" -ge 8 ]; then
    echo "CRASH-JUDGE: e2fsck operational error (rc=$rc) on $IMG" >&2
    echo "$OUT" | head -20 >&2
    exit 1
fi

# The only repair allowed is journal recovery ("recovering journal", no
# `Fix?`). Any `Fix? yes` / `FIXED` means the post-replay filesystem was NOT
# crash-consistent — the state a real kernel would have mounted was corrupt.
if echo "$OUT" | grep -qE "Fix\? yes|FIXED|CLEARED|RECONNECT"; then
    echo "CRASH-JUDGE: post-replay fsck repaired corruption on $IMG" >&2
    echo "$OUT" | grep -iE "Fix\? yes|FIXED|CLEARED|RECONNECT|differences|wrong|invalid|overlaps|orphan|unused inodes" | head -20 >&2
    exit 1
fi

# Optional data oracle (fsync-durability / content assertions), run against
# the REPLAYED image via debugfs — no mount needed.
if [ $# -ge 1 ]; then
    if ! "$@" "$WORK"; then
        echo "CRASH-JUDGE: data oracle '$1' failed on $IMG" >&2
        exit 1
    fi
fi

exit 0
