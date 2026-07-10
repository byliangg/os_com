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
# Coverage gates (the other half of the vacuity guard; run_matrix.sh sets
# both): a kernel that stops emitting FLUSH barriers (fsync without a device
# flush — the head-line bug class this harness exists for) would silently
# shrink the checked-point set to nothing while the final image still looks
# complete. So the sweep itself must prove it exercised the corpus:
#   X4_MIN_FLUSHES  minimum number of FLUSH entries the log must contain
#                   (run_matrix uses 2x the workload count: every workload
#                   fsyncs at least one data file and one marker);
#   X4_EXPECT_WL    the per-point oracle's in-force workload count must
#                   REACH this peak across the swept points (a healthy run
#                   ends with every workload's markers on disk).
#
# Exit 0 = every crash point judged consistent and the coverage gates hold;
# non-zero = a failing point (replay-log names the entry) or a coverage hole.

set -eu
set -o pipefail

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

if [ -n "${X4_MIN_FLUSHES:-}" ] && [ "$NR_FLUSHES" -lt "$X4_MIN_FLUSHES" ]; then
    echo "sweep: FLUSH density collapsed: $NR_FLUSHES flush points < required" \
        "$X4_MIN_FLUSHES — fsync/commit stopped emitting barriers?" >&2
    exit 1
fi

CRASH=$(mktemp "${TMPDIR:-/tmp}/crash-sweep-XXXXXX.img")
SWEEP_LOG=$(mktemp "${TMPDIR:-/tmp}/crash-sweep-XXXXXX.out")
trap 'rm -f "$CRASH" "$SWEEP_LOG"' EXIT
cp --sparse=always "$PRISTINE" "$CRASH"

# Replay entry by entry; at every FLUSH, run the judge on the current state.
# The judge copies the image before touching it, so the incremental replay
# stays faithful. The judge output is teed so the in-force peak can be
# audited after the sweep.
"$REPLAY_LOG" --log "$LOG" --replay "$CRASH" \
    --check flush --fsck "$HERE/judge.sh $CRASH $*" 2>&1 | tee "$SWEEP_LOG"

# The final state (all writes applied, clean end of run) must judge clean too.
"$HERE/judge.sh" "$CRASH" "$@" 2>&1 | tee -a "$SWEEP_LOG"

if [ -n "${X4_EXPECT_WL:-}" ]; then
    peak=$(sed -n 's/^oracle check: \([0-9]\{1,\}\) workloads in force.*/\1/p' \
        "$SWEEP_LOG" | sort -n | tail -1)
    peak=${peak:-0}
    if [ "$peak" -ne "$X4_EXPECT_WL" ]; then
        echo "sweep: in-force coverage hole: oracle peak $peak workloads in" \
            "force != expected $X4_EXPECT_WL — markers never persisted or" \
            "the oracle never saw them (vacuous sweep)" >&2
        exit 1
    fi
    echo "sweep: oracle in-force peak $peak == expected $X4_EXPECT_WL"
fi
echo "sweep: all $NR_FLUSHES flush points + final state judged consistent"
