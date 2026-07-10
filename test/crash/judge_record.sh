#!/bin/bash

# SPDX-License-Identifier: MPL-2.0
#
# Ledger-recording wrapper around a crash-point judge: lets a sweep record a
# red verdict and KEEP SWEEPING instead of dying at the first failing FLUSH
# point (replay-log aborts the whole replay when its --fsck command fails, so
# without this wrapper one red point hides every later one).
#
#   judge_record.sh [--final] <state-dir> <judge> <img> [judge args...]
#
# <state-dir> is prepared by sweep.sh and carries the sweep-scoped context a
# per-point invocation cannot reconstruct on its own:
#   config          shell-sourceable: LEDGER (progress TSV, appended here),
#                   EVIDENCE (directory for red-point forensics), POINT_BASE
#                   (points already in the ledger when resuming), WL_LIST
#                   (optional file of workload names, for attribution hints)
#   flush_entries   log entry number of every FLUSH point, one per line, for
#                   the whole log (so a point index maps to the replay-log
#                   entry a re-run can --start-entry from)
#   counter         invocations so far in THIS sweep process
#
# Every invocation appends one TSV row to the ledger:
#   point  entry  verdict  workload-hint  signature  evidence
# where `point` is the global judged-point ordinal ("final" for the
# end-of-log state), `entry` the replay-log entry number of the FLUSH,
# `verdict` GREEN, RED, or OPERR (judge operational error — sweep refuses to resume past it), `workload-hint` a best-effort attribution (the
# first corpus workload named in the judge output — the oracle names its
# victim; structural reds usually cannot be attributed and stay "-"),
# `signature` a digest of the judge output with volatile paths stripped
# (what the orchestrator matches against stages/P8_expected.tsv), and
# `evidence` the saved judge transcript (plus the judge's structdiff
# manifest) for red points.
#
# Exit code: 0 for both GREEN and RED — a recorded red must not stop the
# replay. Only an operational error (judge rc >= 2: broken harness, not a
# verdict) exits 2 and halts the sweep, fail-loud.

set -u

FINAL=0
if [ "${1:-}" = "--final" ]; then
    FINAL=1
    shift
fi
if [ $# -lt 3 ]; then
    echo "usage: judge_record.sh [--final] <state-dir> <judge> <img> [args...]" >&2
    exit 2
fi

STATE=$1
shift
. "$STATE/config" # LEDGER, EVIDENCE, POINT_BASE, WL_LIST

n=$(cat "$STATE/counter")
if [ "$FINAL" = 1 ]; then
    point=final
    entry=end
else
    point=$((POINT_BASE + n))
    # Line point+1 of the full-log FLUSH list (points are 0-based).
    entry=$(sed -n "$((point + 1))p" "$STATE/flush_entries")
    if [ -z "$entry" ]; then
        echo "judge_record: point $point beyond the flush list — log/state mismatch" >&2
        exit 2
    fi
    echo $((n + 1)) > "$STATE/counter"
fi

OUT=$(mktemp "${TMPDIR:-/tmp}/judge-record-XXXXXX.out")
trap 'rm -f "$OUT"' EXIT

"$@" > "$OUT" 2>&1
rc=$?
# The judge's own output must still reach the sweep transcript (the oracle
# in-force peak audit greps it there).
cat "$OUT"

IMG=$2
if [ "$rc" -ge 2 ]; then
    printf '%s\t%s\tOPERR\t-\t-\t-\n' "$point" "$entry" >> "$LEDGER"
    echo "judge_record: judge operational error (rc=$rc) at point $point (entry $entry)" >&2
    exit 2
fi

if [ "$rc" -eq 0 ]; then
    printf '%s\t%s\tGREEN\t-\t-\t-\n' "$point" "$entry" >> "$LEDGER"
    exit 0
fi

# RED: collect the full signature before letting the replay move on.
mkdir -p "$EVIDENCE"
ev="$EVIDENCE/p${point}-e${entry}.judge.txt"
cp "$OUT" "$ev"
# judge.sh dumps a structural manifest next to the (transient) swept image;
# rescue it under a point-stable name or it is gone with the stage dir.
if [ -f "$IMG.structdiff" ]; then
    mv "$IMG.structdiff" "$EVIDENCE/p${point}-e${entry}.structdiff"
fi
# Signature: volatile temp paths stripped so the same failure at the same
# point signs identically across runs.
sig=$(sed -e "s|${TMPDIR:-/tmp}[^ ]*||g" -e "s|$IMG||g" "$OUT" | md5sum)
sig=${sig%% *}
wl=-
if [ -n "${WL_LIST:-}" ] && [ -f "$WL_LIST" ]; then
    wl=$(grep -oFf "$WL_LIST" "$OUT" | head -1)
    wl=${wl:--}
fi
printf '%s\t%s\tRED\t%s\t%s\t%s\n' "$point" "$entry" "$wl" "${sig:0:12}" "$ev" >> "$LEDGER"
echo "judge_record: RED at point $point (entry $entry, wl $wl) — recorded, sweep continues" >&2
exit 0
