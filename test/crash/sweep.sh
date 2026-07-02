#!/bin/bash

# SPDX-License-Identifier: MPL-2.0
#
# Sweeps every persistence point of a recorded run: reconstructs the disk as
# it would look if power failed at each FLUSH barrier, and judges each state.
#
#   sweep.sh <pristine.img> <log.img> [oracle.sh ...]
#
# <pristine.img> is a snapshot of the device taken BEFORE the recorded boot
# (blklogwrites logs every write since then, so pristine + full log replay ==
# final device). <log.img> is the dm-log-writes-compatible log the QEMU
# blklogwrites filter produced (tools/qemu_args.sh BLKLOG=on).
#
# The write-prefix-replay method is B3's (CrashMonkey, OSDI'18): a barrier
# (VIRTIO_BLK_T_FLUSH) is the only point where the guest may assume
# persistence, so crashing "between" barriers is modeled by replaying the log
# only up to one. replay-log is xfstests' official dm-log-writes replayer;
# it invokes the judge at every FLUSH entry via --check flush.
#
# Exit 0 = every crash point judged consistent; non-zero = replay-log stopped
# at the first failing point (its output names the entry).

set -eu

if [ $# -lt 2 ]; then
    echo "usage: sweep.sh <pristine.img> <log.img> [oracle.sh ...]" >&2
    exit 2
fi

PRISTINE=$1
LOG=$2
shift 2

HERE=$(dirname "$(readlink -f "$0")")

# xfstests' replay-log binary from the nix store (built for the conformance
# suite; runs in the build container).
REPLAY_LOG=${REPLAY_LOG:-$(ls -d /nix/store/*-xfstests-*/lib/xfstests/src/log-writes/replay-log 2>/dev/null | head -1)}
if [ -z "$REPLAY_LOG" ] || [ ! -x "$REPLAY_LOG" ]; then
    echo "sweep.sh: replay-log not found (build the xfstests conformance package first)" >&2
    exit 2
fi

NR_ENTRIES=$("$REPLAY_LOG" --log "$LOG" --num-entries)
NR_FLUSHES=$("$REPLAY_LOG" --log "$LOG" --replay /dev/null -v 2>/dev/null | grep -c "FLUSH" || true)
echo "sweep: $NR_ENTRIES log entries, $NR_FLUSHES flush points"

CRASH=$(mktemp "${TMPDIR:-/tmp}/crash-sweep-XXXXXX.img")
trap 'rm -f "$CRASH"' EXIT
cp --sparse=always "$PRISTINE" "$CRASH"

# Replay entry by entry; at every FLUSH, run the judge on the current state.
# The judge copies the image before touching it, so the incremental replay
# stays faithful.
"$REPLAY_LOG" --log "$LOG" --replay "$CRASH" \
    --check flush --fsck "$HERE/judge.sh $CRASH $*"

# The final state (all writes applied, clean end of run) must judge clean too.
"$HERE/judge.sh" "$CRASH" "$@"
echo "sweep: all $NR_FLUSHES flush points + final state judged consistent"
