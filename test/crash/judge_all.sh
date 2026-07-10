#!/bin/bash

# SPDX-License-Identifier: MPL-2.0
#
# Composite per-crash-point data judge, wired as judge.sh's single oracle
# hook by run_matrix.sh (judge.sh appends the journal-replayed scratch image
# as the last argument):
#
#   judge_all.sh <oracle.table> <replayed.img>
#
# Runs the cheap independent judges first — accounting recount (~0.03s) and
# metadata_csum recompute (~0.04s) — so an accounting/csum red skips the
# ~2s oracle scan; the durability oracle runs last via exec, so its exit
# code (0 green / 1 red / 2 operational) is this script's. The cheap judges'
# rc 2 (unusable image) is folded into red: the image here has already been
# journal-replayed and greened by strict e2fsck, so "unusable" is itself an
# anomaly worth stopping on (fail-loud).

set -u

if [ $# -ne 2 ]; then
    echo "usage: judge_all.sh <oracle.table> <replayed.img>" >&2
    exit 2
fi

HERE=$(dirname "$(readlink -f "$0")")
TABLE=$1
IMG=$2

if ! ACC_OUT=$("$HERE/judge_accounting.sh" "$IMG" 2>&1); then
    echo "$ACC_OUT" | grep -v '^note:' | head -20 >&2
    echo "judge_all: accounting mismatch on $IMG" >&2
    exit 1
fi
if ! python3 "$HERE/judge_csum.py" --quiet "$IMG"; then
    echo "judge_all: metadata csum mismatch on $IMG" >&2
    exit 1
fi
exec python3 "$HERE/oracle.py" check "$TABLE" "$IMG"
