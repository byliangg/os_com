#!/bin/bash

# SPDX-License-Identifier: MPL-2.0
#
# The basic crash matrix: converts ACE/CrashMonkey J-lang workloads to shell,
# bakes them into the (to-be-recorded) xfstests test disk, boots the kernel
# once to run them all, then reconstructs and judges every FLUSH-point crash
# state of the whole run — with the full judge fleet at every point via
# judge_all.sh: strict e2fsck for structure (judge.sh), the independent
# accounting and metadata_csum judges (judge_accounting.sh / judge_csum.py),
# and the data oracle for durability (oracle.py; fsync-ed content and
# directory entry sets must survive, freed blocks must not resurface).
#
#   run_matrix.sh <jlang-dir> [workload names...]
#
# With no names, every workload in <jlang-dir> whose ops fit the supported
# subset is taken (jlang2sh.py fails loudly on the rest; those are skipped,
# COUNTED, and their stderr recorded in test/initramfs/build/matrix.skipped —
# and if a convertible-expectation list is checked in for the corpus, any
# shrink of the convertible set is a hard failure: a converter regression
# must not silently thin the matrix). The taken list is written to
# test/initramfs/build/matrix.workloads. Run from the repo root, inside the
# build container (needs loop-mount privileges for the 0x52 pre-dye).

set -eu

if [ $# -lt 1 ]; then
    echo "usage: run_matrix.sh <jlang-dir> [names...]" >&2
    exit 2
fi

JLANG_DIR=$1
shift
HERE=$(dirname "$(readlink -f "$0")")
REPO=$(readlink -f "$HERE/../..")
BUILD=$REPO/test/initramfs/build
mkdir -p "$BUILD"

# All scratch state lives in RAM (tmpfs): the 0x52 pre-dye below makes the
# 2G image fully allocated, and every one of the ~hundreds of crash points
# copies it once in sweep.sh and once per judge — on disk that would be
# hundreds of GB of write churn per matrix run.
SHM=${X4_SHM:-/dev/shm}
STAGE=$(mktemp -d "$SHM/crash-matrix-XXXXXX")
# An EXIT-only trap does not fire on Ctrl-C/kill, leaking a multi-GB tmpfs
# stage (pinned RAM) and possibly a loop mount; the signal trap converts
# HUP/INT/TERM into a normal exit so cleanup always runs. Forensic manifests
# judge.sh dumps next to the swept image (*.structdiff) are rescued into
# $BUILD first — the stage is gone by the time anyone can look at them.
trap 'mountpoint -q "$STAGE/mnt" 2>/dev/null && umount "$STAGE/mnt";
      cp "$STAGE"/*.structdiff "$BUILD"/ 2>/dev/null || true;
      rm -rf "$STAGE"' EXIT
trap 'exit 129' HUP INT TERM
export TMPDIR="$STAGE"

mkdir -p "$STAGE/root/.crash"
taken=0
skipped=0
taken_names=()
explicit=0
if [ $# -ge 1 ]; then
    names=("$@")
    explicit=1
else
    names=($(ls "$JLANG_DIR"))
fi
: > "$BUILD/matrix.skipped"
for n in "${names[@]}"; do
    if python3 "$HERE/jlang2sh.py" --oracle "$JLANG_DIR/$n" \
        > "$STAGE/root/.crash/$n.sh" 2> "$STAGE/convert.err"; then
        taken=$((taken + 1))
        taken_names+=("$n")
    else
        # A deliberate unsupported-op skip and an accidental converter crash
        # both land here; the recorded stderr is what tells them apart.
        rm -f "$STAGE/root/.crash/$n.sh"
        skipped=$((skipped + 1))
        printf '%s\t%s\n' "$n" "$(tr '\n' ' ' < "$STAGE/convert.err")" \
            >> "$BUILD/matrix.skipped"
    fi
done
echo "matrix: $taken workloads baked, $skipped skipped (reasons -> $BUILD/matrix.skipped)"
if [ "$taken" -eq 0 ]; then
    echo "matrix: nothing to run" >&2
    exit 2
fi
printf '%s\n' "${taken_names[@]}" > "$BUILD/matrix.workloads"

# Corpus-shrink gate: when converting a whole directory that has a checked-in
# convertible-expectation list, every expected name must still convert.
EXPECT_LIST=${X4_CONVERTIBLE_LIST:-$HERE/convertible_$(basename "$JLANG_DIR").list}
if [ "$explicit" -eq 0 ] && [ -f "$EXPECT_LIST" ]; then
    lost=$(comm -23 <(LC_ALL=C sort "$EXPECT_LIST") \
        <(printf '%s\n' "${taken_names[@]}" | LC_ALL=C sort))
    if [ -n "$lost" ]; then
        echo "matrix: converter regression — expected-convertible workloads now skipped:" >&2
        echo "$lost" | head -20 >&2
        echo "matrix: (per-workload stderr is in $BUILD/matrix.skipped)" >&2
        exit 1
    fi
    echo "matrix: all $(wc -l < "$EXPECT_LIST") expected-convertible workloads converted"
fi

# Fresh test disk, pre-populated with the workloads (mke2fs -d), and a fresh
# write log; the pristine snapshot is what every crash state replays onto.
cd "$REPO"
rm -f "$BUILD/xfstests_test.img" "$BUILD/xfstests_scratch.img" \
    "$BUILD/xfstests_test.logwrites.img" qemu.log
truncate -s 2G "$BUILD/xfstests_test.img"
mke2fs -F -q -t ext4 -b 4096 -I 256 \
    -O has_journal,extent,filetype,metadata_csum,dir_index,64bit,flex_bg,^inline_data \
    -d "$STAGE/root" "$BUILD/xfstests_test.img"

# Pre-dye the free-block pool with the 0x52 sentinel: fill the fs with one
# big 0x52 file, delete it, and verify the image is still fsck-clean. Every
# data block the kernel-under-test later allocates starts out as 0x52, so a
# lost/unordered data write shows up as recognizable stale bytes (the data
# oracle asserts declared files never read them back) instead of silently
# reading as legitimate zeros. 0x52 is distinct from the workloads' 0x22
# pwrite pattern and from zero-filled holes.
MNT=$STAGE/mnt
mkdir "$MNT"
mount -o loop "$BUILD/xfstests_test.img" "$MNT"
(tr '\0' '\122' < /dev/zero | dd of="$MNT/.x4dye" bs=1M conv=fsync status=none) || true
[ -s "$MNT/.x4dye" ] || { echo "matrix: 0x52 pre-dye wrote nothing" >&2; exit 1; }
# The fill must have run to ENOSPC: a dd killed mid-flight (OOM/admin kill)
# leaves the pool partially dyed and silently weakens the stale-block leg of
# the oracle over the undyed fraction.
avail_k=$(df -k --output=avail "$MNT" | tail -1 | tr -dc '0-9')
if [ "${avail_k:-99999999}" -ge 16384 ]; then
    echo "matrix: 0x52 pre-dye underfilled (${avail_k}K still free — dd killed mid-fill?)" >&2
    exit 1
fi
rm "$MNT/.x4dye"
umount "$MNT"
if ! e2fsck -fn "$BUILD/xfstests_test.img" > "$STAGE/dye-fsck.log" 2>&1; then
    echo "matrix: image not clean after the 0x52 pre-dye" >&2
    tail -5 "$STAGE/dye-fsck.log" >&2
    exit 1
fi

PRISTINE=$STAGE/pristine.img
cp --sparse=always "$BUILD/xfstests_test.img" "$PRISTINE"

make run_kernel BLKLOG=on AUTO_TEST=conformance RELEASE=1 \
    CONFORMANCE_TEST_SUITE=xfstests MEM=12G XFSTESTS_DISK_SIZE=2G \
    CRASH_WORKLOADS=all 2>&1 | tail -3 || true
if ! grep -q "crash workloads done" qemu.log; then
    echo "matrix: guest did not finish the workloads (see qemu.log)" >&2
    exit 1
fi

# Build the durability expectation table from the declaration lines the
# instrumented workloads printed to the console (checksummed against serial
# line loss; lost workloads are recorded as explicitly unjudged).
python3 "$HERE/oracle.py" collect qemu.log "$BUILD/oracle.table"

# Sweep with the whole judge fleet at every FLUSH point, plus the coverage
# gates (see sweep.sh): the log must contain at least 2 FLUSHes per workload
# and the oracle's in-force count must peak at exactly the taken count —
# otherwise a barrier-loss regression could green a matrix it never checked.
X4_EXPECT_WL=$taken X4_MIN_FLUSHES=$((2 * taken)) \
    "$HERE/sweep.sh" "$PRISTINE" "$BUILD/xfstests_test.logwrites.img" \
    "$HERE/judge_all.sh" "$BUILD/oracle.table"

# Vacuity guard: on the end-of-run image every checkpoint marker of every
# taken workload must be in force and green. Without this, an fsync that
# never persists anything would keep every per-prefix oracle check silently
# vacuous (no marker => nothing asserted => green).
FINAL_WORK=$STAGE/final-replayed.img
cp --sparse=always "$BUILD/xfstests_test.img" "$FINAL_WORK"
e2fsck -fy "$FINAL_WORK" > /dev/null 2>&1 || {
    rc=$?
    if [ "$rc" -ge 8 ]; then
        echo "matrix: e2fsck operational error on the final image" >&2
        exit 1
    fi
}
python3 "$HERE/oracle.py" final "$BUILD/oracle.table" "$taken" "$FINAL_WORK" || {
    rc=$?
    if [ "$rc" -eq 3 ]; then
        echo "matrix: RUN UNJUDGED — the oracle lost console lines; rerun the matrix" >&2
    fi
    exit "$rc"
}

# End-state second opinions + forensic baseline: accounting and csum are
# cheap and must green the cleanly-replayed final image; the structdiff
# manifest is kept as the attribution reference for post-hoc triage.
if ! ACC_OUT=$("$HERE/judge_accounting.sh" "$FINAL_WORK" 2>&1); then
    echo "matrix: final-state accounting mismatch:" >&2
    echo "$ACC_OUT" | head -20 >&2
    exit 1
fi
python3 "$HERE/judge_csum.py" --quiet "$FINAL_WORK"
"$HERE/judge_structdiff.sh" "$FINAL_WORK" > "$BUILD/matrix.final.manifest"

echo "matrix: $taken workloads recorded, swept clean, oracle green" \
    "(final manifest -> $BUILD/matrix.final.manifest)"
