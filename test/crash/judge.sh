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
# The judgement pipeline is all-official (see test.md §5.3-5.4): first let
# e2fsck replay the journal — exactly what the kernel would do on the next
# mount — then demand a fully clean fsck. The replay MUST happen on a scratch
# copy: the caller keeps appending log entries to <crash.img> for the next
# crash point, and a repaired-in-place image would corrupt the simulation.

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

# Replay the journal (the kernel's mount-time recovery, done by e2fsck so the
# oracle is official). rc 0 = nothing to do, rc 1 = replayed/fixed; anything
# >= 4 means the journal itself is broken — a real crash-consistency failure.
e2fsck -E journal_only -p "$WORK" >/dev/null 2>&1
rc=$?
if [ "$rc" -ge 4 ]; then
    echo "CRASH-JUDGE: journal replay failed (e2fsck rc=$rc) on $IMG" >&2
    exit 1
fi

# After replay the filesystem must be spotless; -n never modifies.
if ! e2fsck -fn "$WORK" >/dev/null 2>&1; then
    echo "CRASH-JUDGE: post-replay fsck not clean on $IMG" >&2
    e2fsck -fn "$WORK" 2>&1 | head -20 >&2
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
