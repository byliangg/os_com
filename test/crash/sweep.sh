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
# Two sweep modes:
#
#   Default (no X4_LEDGER): replay-log aborts at the first red point and this
#   script exits non-zero there — the historical stop-on-first-fail gate.
#
#   Ledger mode (X4_LEDGER=<progress.tsv>): every judged point is recorded to
#   the TSV (point / entry / verdict / workload-hint / signature / evidence,
#   see judge_record.sh) and a RED DOES NOT STOP THE SWEEP — the remaining
#   FLUSH points are still judged and the run ends with a "N green / M red"
#   summary (exit 1 if any red). Red evidence (judge transcript + structdiff)
#   is kept under X4_EVIDENCE (default: <ledger dir>/evidence). The ledger
#   also makes the sweep RESUMABLE: X4_RESUME=1 with an existing ledger
#   fast-forwards the replay (no judging) past the last recorded point and
#   continues from there — an interrupted multi-hour sweep loses nothing.
#   X4_WL_LIST names the corpus workloads for the ledger's attribution hints.
#   A pre-existing ledger without X4_RESUME=1 is refused (fail-loud), so two
#   sweeps cannot silently interleave rows.
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
# non-zero = a failing point (named in the summary or by replay-log) or a
# coverage hole.

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
STATE=
trap 'rm -rf "$CRASH" "$SWEEP_LOG" "$STATE"' EXIT
cp --sparse=always "$PRISTINE" "$CRASH"

audit_coverage() {
    # The judge output is teed into $SWEEP_LOG so the oracle in-force peak
    # can be audited after the sweep. The peak is reached at the tail of the
    # log (a healthy run ends with every marker on disk), so a resumed sweep
    # still observes it even though earlier points' output is absent.
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
}

if [ -z "${X4_LEDGER:-}" ]; then
    # ---- default mode: stop at the first failing point ------------------
    "$REPLAY_LOG" --log "$LOG" --replay "$CRASH" \
        --check flush --fsck "$HERE/judge.sh $CRASH $*" 2>&1 | tee "$SWEEP_LOG"

    # The final state (all writes applied, clean end of run) must judge
    # clean too.
    "$HERE/judge.sh" "$CRASH" "$@" 2>&1 | tee -a "$SWEEP_LOG"

    audit_coverage
    echo "sweep: all $NR_FLUSHES flush points + final state judged consistent"
    exit 0
fi

# ---- ledger mode: record every point, survive reds, resumable -----------

LEDGER=$X4_LEDGER
EVIDENCE=${X4_EVIDENCE:-$(dirname "$LEDGER")/evidence}
STATE=$(mktemp -d "${TMPDIR:-/tmp}/crash-sweep-state-XXXXXX")

# The full-log FLUSH entry list: maps the k-th judged point to the log entry
# number a re-run can --start-entry from (and the attribution anchor the
# expected-red table keys on).
"$REPLAY_LOG" --log "$LOG" --replay /dev/null -v 2>/dev/null \
    | awk '/FLUSH/ { split($2, a, "@"); print a[1] }' > "$STATE/flush_entries"

START_ENTRY=0
POINT_BASE=0
if [ -f "$LEDGER" ]; then
    if [ "${X4_RESUME:-0}" != 1 ]; then
        echo "sweep: ledger $LEDGER already exists (set X4_RESUME=1 to continue it)" >&2
        exit 2
    fi
    if grep -q "OPERR" "$LEDGER"; then
        echo "sweep: ledger records a judge operational error — fix the" \
            "harness and start a fresh ledger instead of resuming" >&2
        exit 2
    fi
    if awk -F'\t' '$1 == "final"' "$LEDGER" | grep -q .; then
        echo "sweep: ledger already complete (final state judged); summarizing only"
    else
        done_points=$(awk -F'\t' '$1 ~ /^[0-9]+$/' "$LEDGER" | wc -l)
        if [ "$done_points" -gt 0 ]; then
            last_entry=$(awk -F'\t' '$1 ~ /^[0-9]+$/ { print $2 }' "$LEDGER" | sort -n | tail -1)
            START_ENTRY=$((last_entry + 1))
            POINT_BASE=$done_points
        fi
        echo "sweep: resuming — $done_points points already judged," \
            "fast-forwarding to entry $START_ENTRY"
    fi
else
    if [ "${X4_RESUME:-0}" = 1 ]; then
        echo "sweep: X4_RESUME=1 but no ledger at $LEDGER — starting fresh"
    fi
    mkdir -p "$(dirname "$LEDGER")"
    {
        echo "# crash-sweep progress ledger (see judge_record.sh)"
        echo "# log=$LOG pristine=$PRISTINE date=$(date -u +%FT%TZ)"
        printf '# point\tentry\tverdict\twl\tsig\tevidence\n'
    } > "$LEDGER"
fi

cat > "$STATE/config" <<EOF
LEDGER=$LEDGER
EVIDENCE=$EVIDENCE
POINT_BASE=$POINT_BASE
WL_LIST=${X4_WL_LIST:-}
EOF
echo 0 > "$STATE/counter"

if ! awk -F'\t' '$1 == "final"' "$LEDGER" | grep -q .; then
    if [ "$START_ENTRY" -gt 0 ]; then
        # Fast-forward: replay the already-judged prefix without judging.
        "$REPLAY_LOG" --log "$LOG" --replay "$CRASH" --limit "$START_ENTRY"
    fi
    "$REPLAY_LOG" --log "$LOG" --replay "$CRASH" --start-entry "$START_ENTRY" \
        --check flush \
        --fsck "$HERE/judge_record.sh $STATE $HERE/judge.sh $CRASH $*" \
        2>&1 | tee "$SWEEP_LOG"

    # The final state (all writes applied, clean end of run) is a ledger row
    # of its own.
    "$HERE/judge_record.sh" --final "$STATE" "$HERE/judge.sh" "$CRASH" "$@" \
        2>&1 | tee -a "$SWEEP_LOG"

    audit_coverage
fi

greens=$(awk -F'\t' '$3 == "GREEN"' "$LEDGER" | wc -l)
reds=$(awk -F'\t' '$3 == "RED"' "$LEDGER" | wc -l)
echo "sweep: ledger summary: $greens points green, $reds red ($LEDGER)"
if [ "$reds" -gt 0 ]; then
    echo "sweep: red points (point/entry/wl/sig):" >&2
    awk -F'\t' '$3 == "RED" { print "  " $1 "\t" $2 "\t" $4 "\t" $5 }' "$LEDGER" >&2
    exit 1
fi
