#!/bin/bash

# SPDX-License-Identifier: MPL-2.0
#
# The crash matrix driver: converts ACE/CrashMonkey J-lang workloads to
# shell, bakes them into the (to-be-recorded) xfstests test disk, boots the
# kernel to run them, then reconstructs and judges every FLUSH-point crash
# state with the full judge fleet (judge_all.sh: strict e2fsck structure +
# accounting recount + metadata_csum recompute + the durability oracle).
#
#   run_matrix.sh [options] <jlang-dir> [workload names...]
#
# Options:
#   --shards N        split the corpus into N shards (max 2, plan §5
#                     decision 3), each with its own bake + recorded boot +
#                     sweep. Boots are SERIALIZED — the kernel image, the
#                     qemu.log path and the test-disk path are process-wide
#                     singletons, and the container runs one QEMU at a time
#                     — while the sweeps (the expensive half, ~2.5s/point)
#                     run in PARALLEL, each against its own renamed
#                     artifacts. Default 1.
#   --mem SIZE        guest RAM for the recorded boot (default 12G)
#   --journal-size MB bake with `mke2fs -J size=MB` (4 = the 1024-block
#                     minimum: forces wrap/checkpoint traffic that the
#                     default-journal lazy-checkpoint matrix never emits)
#   --walcheck N      after each shard's recording, run the WAL write-order
#                     checker (walcheck.py, geometry = the shard pristine)
#                     and require >= N byte-matched judged writes; a red or
#                     a vacuous log fails the run (recorded per shard in
#                     matrix.sK.walcheck.log). Meaningful with
#                     --journal-size 4 — see the a4 finding: default-
#                     journal power-cut logs have an EMPTY judged surface.
#                     Not supported with --dirty-start (pre-session journal
#                     transactions are outside walcheck's parse; a4 gap).
#   --dirty-start     two-stage recording (single shard only): stage 1 runs
#                     a journal-dirtying prologue (dirtier.sh, installed as
#                     00-dirty-dirtier.sh so it sorts first) plus the
#                     corpus, and is HARD CUT — no clean unmount — at the
#                     first FLUSH where the prologue's self-cleanup is
#                     durable, the journal still needs recovery, and the
#                     strict judge greens the cut state. Stage 2 boots FROM
#                     that dirty disk: mount-time journal replay itself is
#                     recorded, so every replay write enters the
#                     crash-prefix enumeration; the corpus then reruns and
#                     the sweep judges the whole session against the cut
#                     image as pristine.
#   --keep            keep per-shard images/logs even when everything is
#                     green (default: green shards are cleaned down to
#                     ledgers/evidence/small logs — the host disk is the
#                     25G-bound resource; a RED shard automatically keeps
#                     its write log AND saves its pristine image, which
#                     together re-derive every crash state).
#
# Environment: X4_SHM (staging root, default /dev/shm),
#   X4_CONVERTIBLE_LIST (corpus-shrink expectation list, default
#   convertible_<corpus-dir>.list next to this script).
#
# Continue-on-red: sweeps run in ledger mode (sweep.sh X4_LEDGER) — every
# judged point lands in $BUILD/matrix.sK.progress.tsv, a red point is
# recorded (with evidence under $BUILD/matrix-evidence/sK/) and the sweep
# CONTINUES; the run ends with per-shard green/red summaries and a nonzero
# exit if anything was red. Matching red signatures against
# stages/P8_expected.tsv is the orchestrator's step, not this script's.
# An interrupted sweep can be continued against the kept artifacts with
# sweep.sh X4_RESUME=1 (a fresh run_matrix invocation starts over and
# clears the previous run's ledgers/evidence — harvest first).
#
# Corpus entries named *.sh are RAW workloads taken verbatim (protocol
# points the J-lang subset cannot express, test/crash/protocol/): no
# oracle instrumentation, excluded from the oracle vacuity expectations,
# but every structural judge still runs on their FLUSH points. Other
# entries are converted by jlang2sh.py --oracle; unconvertible ones are
# skipped, COUNTED, and their stderr recorded in $BUILD/matrix.skipped —
# and if a convertible-expectation list is checked in for the corpus
# (convertible_$(basename dir).list), any shrink of the convertible set is
# a hard failure. Run from the repo root, inside the build container
# (needs loop-mount privileges for the 0x52 pre-dye).
#
# Resource accounting: a background sampler tracks the /dev/shm high-water
# mark, the $BUILD high-water mark and the minimum root-disk headroom, and
# leaves them in $BUILD/matrix.peaks (printed in the summary).

set -eu

usage() {
    awk 'NR<3 {next} /^#/ {sub(/^# ?/, ""); print; next} {exit}' "$0" >&2
    exit 2
}

SHARDS=1
MEM=12G
JSIZE=
WALCHECK=0
DIRTY=0
KEEP=0
while [ $# -gt 0 ]; do
    case "$1" in
    --shards) SHARDS=$2; shift 2 ;;
    --mem) MEM=$2; shift 2 ;;
    --journal-size) JSIZE=$2; shift 2 ;;
    --walcheck) WALCHECK=$2; shift 2 ;;
    --dirty-start) DIRTY=1; shift ;;
    --keep) KEEP=1; shift ;;
    -h|--help) usage ;;
    --*) echo "run_matrix: unknown option $1" >&2; usage ;;
    *) break ;;
    esac
done
if [ $# -lt 1 ]; then
    usage
fi
if [ "$SHARDS" -lt 1 ] || [ "$SHARDS" -gt 2 ]; then
    echo "run_matrix: --shards must be 1 or 2 (plan §5 decision 3)" >&2
    exit 2
fi
if [ "$DIRTY" = 1 ] && [ "$SHARDS" -ne 1 ]; then
    echo "run_matrix: --dirty-start is single-shard only" >&2
    exit 2
fi
if [ "$DIRTY" = 1 ] && [ "$WALCHECK" -gt 0 ]; then
    echo "run_matrix: --walcheck cannot judge a dirty-start log (pre-session" \
        "transactions are outside walcheck's parse — a4 known gap)" >&2
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
SWEEP_PIDS=()
SAMPLER_PID=
# An EXIT-only trap does not fire on Ctrl-C/kill, leaking a multi-GB tmpfs
# stage (pinned RAM), background sweeps and possibly a loop mount; the
# signal trap converts HUP/INT/TERM into a normal exit so cleanup always
# runs. Forensic manifests judge.sh dumps next to swept images are rescued
# into $BUILD first — the stage is gone by the time anyone can look.
# cleanup() must drop set -e: under it a benign probe failure (mountpoint
# on a never-mounted dir, kill of a reaped pid) aborts the trap mid-way —
# leaking the pinned-RAM stage — and clobbers the script's exit status
# (bash only restores the pre-trap $? when the trap runs to completion).
cleanup() {
    set +e
    for m in "$STAGE"/s*/mnt; do
        mountpoint -q "$m" 2>/dev/null && umount "$m"
    done
    [ -n "$SAMPLER_PID" ] && kill "$SAMPLER_PID" 2>/dev/null
    for p in ${SWEEP_PIDS[@]+"${SWEEP_PIDS[@]}"}; do
        [ -n "$p" ] && kill "$p" 2>/dev/null
    done
    cp "$STAGE"/s*/*.structdiff "$BUILD"/ 2>/dev/null
    rm -rf "$STAGE"
    return 0
}
trap cleanup EXIT
trap 'exit 129' HUP INT TERM
export TMPDIR="$STAGE"

# ---- resource high-water sampler -----------------------------------------
PEAKS=$BUILD/matrix.peaks
sampler() {
    local shm_max=0 build_max=0 root_min=999999999 s b r
    while :; do
        s=$(df --output=used -k "$SHM" 2>/dev/null | tail -1 | tr -dc '0-9')
        b=$(du -sk "$BUILD" 2>/dev/null | cut -f1)
        r=$(df --output=avail -k / 2>/dev/null | tail -1 | tr -dc '0-9')
        [ -n "$s" ] && [ "$s" -gt "$shm_max" ] && shm_max=$s
        [ -n "$b" ] && [ "$b" -gt "$build_max" ] && build_max=$b
        [ -n "$r" ] && [ "$r" -lt "$root_min" ] && root_min=$r
        printf 'shm_peak_kb=%s\nbuild_peak_kb=%s\nroot_free_min_kb=%s\n' \
            "$shm_max" "$build_max" "$root_min" > "$PEAKS"
        sleep 2
    done
}
sampler &
SAMPLER_PID=$!

# ---- conversion (once, before slicing) ------------------------------------
mkdir -p "$STAGE/convert"
taken=0
skipped=0
taken_names=()  # oracle-instrumented (J-lang) workloads
raw_names=()    # verbatim *.sh workloads (no oracle)
explicit=0
if [ $# -ge 1 ]; then
    names=("$@")
    explicit=1
else
    names=($(ls "$JLANG_DIR"))
fi
: > "$BUILD/matrix.skipped"
for n in "${names[@]}"; do
    case "$n" in
    README*|*.list) continue ;;
    *.sh)
        cp "$JLANG_DIR/$n" "$STAGE/convert/$n"
        raw_names+=("$n")
        continue
        ;;
    esac
    if python3 "$HERE/jlang2sh.py" --oracle "$JLANG_DIR/$n" \
        > "$STAGE/convert/$n.sh" 2> "$STAGE/convert.err"; then
        taken=$((taken + 1))
        taken_names+=("$n")
    else
        # A deliberate unsupported-op skip and an accidental converter crash
        # both land here; the recorded stderr is what tells them apart.
        rm -f "$STAGE/convert/$n.sh"
        skipped=$((skipped + 1))
        printf '%s\t%s\n' "$n" "$(tr '\n' ' ' < "$STAGE/convert.err")" \
            >> "$BUILD/matrix.skipped"
    fi
done
total=$((taken + ${#raw_names[@]}))
echo "matrix: $taken workloads converted, ${#raw_names[@]} raw, $skipped" \
    "skipped (reasons -> $BUILD/matrix.skipped)"
if [ "$total" -eq 0 ]; then
    echo "matrix: nothing to run" >&2
    exit 2
fi
printf '%s\n' ${taken_names[@]+"${taken_names[@]}"} | grep -v '^$' \
    > "$BUILD/matrix.workloads" || true
printf '%s\n' ${raw_names[@]+"${raw_names[@]}"} | grep -v '^$' \
    > "$BUILD/matrix.workloads.raw" || true

# Corpus-shrink gate: when converting a whole directory that has a checked-in
# convertible-expectation list, every expected name must still convert.
EXPECT_LIST=${X4_CONVERTIBLE_LIST:-$HERE/convertible_$(basename "$JLANG_DIR").list}
if [ "$explicit" -eq 0 ] && [ -f "$EXPECT_LIST" ]; then
    lost=$(comm -23 <(grep -v '^#' "$EXPECT_LIST" | LC_ALL=C sort) \
        <(printf '%s\n' ${taken_names[@]+"${taken_names[@]}"} \
                        ${raw_names[@]+"${raw_names[@]}"} | LC_ALL=C sort))
    if [ -n "$lost" ]; then
        echo "matrix: converter regression — expected-convertible workloads now skipped:" >&2
        echo "$lost" | head -20 >&2
        echo "matrix: (per-workload stderr is in $BUILD/matrix.skipped)" >&2
        exit 1
    fi
    echo "matrix: all $(wc -l < "$EXPECT_LIST") expected-convertible workloads converted"
fi

if [ "$SHARDS" -gt "$total" ]; then
    SHARDS=1
fi

# ---- shard slicing (round-robin over the combined corpus) -----------------
for ((i = 0; i < SHARDS; i++)); do
    mkdir -p "$STAGE/s$i/root/.crash" "$STAGE/s$i/mnt"
    : > "$STAGE/s$i/wl.list"
done
idx=0
declare -a SH_ORACLE_N
for ((i = 0; i < SHARDS; i++)); do SH_ORACLE_N[$i]=0; done
for n in ${taken_names[@]+"${taken_names[@]}"}; do
    i=$((idx % SHARDS)); idx=$((idx + 1))
    cp "$STAGE/convert/$n.sh" "$STAGE/s$i/root/.crash/$n.sh"
    echo "$n" >> "$STAGE/s$i/wl.list"
    SH_ORACLE_N[$i]=$((SH_ORACLE_N[$i] + 1))
done
for n in ${raw_names[@]+"${raw_names[@]}"}; do
    i=$((idx % SHARDS)); idx=$((idx + 1))
    cp "$STAGE/convert/$n" "$STAGE/s$i/root/.crash/$n"
    echo "${n%.sh}" >> "$STAGE/s$i/wl.list"
done
for ((i = 0; i < SHARDS; i++)); do
    cp "$STAGE/s$i/wl.list" "$BUILD/matrix.s$i.workloads"
done

# ---- per-shard bake / boot / record ---------------------------------------
JOPT=()
if [ -n "$JSIZE" ]; then
    JOPT=(-J "size=$JSIZE")
fi

bake_shard() { # <i> — fresh dyed test disk holding shard i's workloads
    local i=$1 SDIR=$STAGE/s$i
    cd "$REPO"
    rm -f "$BUILD/xfstests_test.img"
    truncate -s 2G "$BUILD/xfstests_test.img"
    mke2fs -F -q -t ext4 -b 4096 -I 256 ${JOPT[@]+"${JOPT[@]}"} \
        -O has_journal,extent,filetype,metadata_csum,dir_index,64bit,flex_bg,^inline_data \
        -d "$SDIR/root" "$BUILD/xfstests_test.img"

    # Pre-dye the free-block pool with the 0x52 sentinel: fill the fs with
    # one big 0x52 file, delete it, verify the image is still fsck-clean.
    # Every data block the kernel-under-test later allocates starts out as
    # 0x52, so a lost/unordered data write shows up as recognizable stale
    # bytes (the oracle asserts declared files never read them back). 0x52
    # is distinct from the workloads' 0x22 pwrite pattern and from
    # zero-filled holes.
    local MNT=$SDIR/mnt
    mount -o loop "$BUILD/xfstests_test.img" "$MNT"
    (tr '\0' '\122' < /dev/zero | dd of="$MNT/.x4dye" bs=1M conv=fsync status=none) || true
    [ -s "$MNT/.x4dye" ] || { echo "matrix: 0x52 pre-dye wrote nothing" >&2; exit 1; }
    # The fill must have run to ENOSPC: a dd killed mid-flight leaves the
    # pool partially dyed and silently weakens the stale-block oracle leg.
    local avail_k
    avail_k=$(df -k --output=avail "$MNT" | tail -1 | tr -dc '0-9')
    if [ "${avail_k:-99999999}" -ge 16384 ]; then
        echo "matrix: 0x52 pre-dye underfilled (${avail_k}K free — dd killed mid-fill?)" >&2
        exit 1
    fi
    rm "$MNT/.x4dye"
    umount "$MNT"
    if ! e2fsck -fn "$BUILD/xfstests_test.img" > "$SDIR/dye-fsck.log" 2>&1; then
        echo "matrix: image not clean after the 0x52 pre-dye" >&2
        tail -5 "$SDIR/dye-fsck.log" >&2
        exit 1
    fi
    cp --sparse=always "$BUILD/xfstests_test.img" "$SDIR/pristine.img"
}

boot_record() { # <tag> — one recorded boot; artifacts renamed to matrix.<tag>.*
    local tag=$1
    cd "$REPO"
    rm -f "$BUILD/xfstests_scratch.img" "$BUILD/xfstests_test.logwrites.img" qemu.log
    make run_kernel BLKLOG=on AUTO_TEST=conformance RELEASE=1 \
        CONFORMANCE_TEST_SUITE=xfstests MEM="$MEM" XFSTESTS_DISK_SIZE=2G \
        CRASH_WORKLOADS=all 2>&1 | tail -3 || true
    if ! grep -q "crash workloads done" qemu.log; then
        mv qemu.log "$BUILD/matrix.$tag.qemu.log" 2>/dev/null || true
        echo "matrix: guest did not finish the workloads (see $BUILD/matrix.$tag.qemu.log)" >&2
        exit 1
    fi
    mv qemu.log "$BUILD/matrix.$tag.qemu.log"
    mv "$BUILD/xfstests_test.logwrites.img" "$BUILD/matrix.$tag.logwrites.img"
    mv "$BUILD/xfstests_test.img" "$BUILD/matrix.$tag.final.img"
}

REPLAY_LOG=${REPLAY_LOG:-$(ls -d /nix/store/*-xfstests-*/lib/xfstests/src/log-writes/replay-log 2>/dev/null | head -1)}

find_cut() { # dirty-start: hard-cut stage 1 at the first usable FLUSH
    # Usable = post-replay state has the dirtier's sentinel and NO leftover
    # workload dirs (the stage-2 rerun re-creates them), the RAW cut image
    # still needs journal recovery (the whole point: stage 2's mount must
    # replay on camera), and the strict judge greens the state (a red here
    # is a stage-1 crash-consistency bug — report it, do not build on it).
    local S1LOG=$BUILD/matrix.dirty-stage1.logwrites.img
    local CUT=$STAGE/s0/cut.img SCRATCH=$STAGE/s0/cut-scratch.img
    local INFO=$BUILD/matrix.dirty.cutinfo
    local pos=0 f ok= erc eout
    if [ -z "$REPLAY_LOG" ] || [ ! -x "$REPLAY_LOG" ]; then
        echo "matrix: replay-log not found (needed for the dirty-start cut)" >&2
        exit 2
    fi
    cp --sparse=always "$STAGE/s0/pristine.img" "$CUT"
    local flushes
    flushes=$("$REPLAY_LOG" --log "$S1LOG" --replay /dev/null -v 2>/dev/null \
        | awk '/FLUSH/ { split($2, a, "@"); print a[1] }' | head -60)
    for f in $flushes; do
        "$REPLAY_LOG" --log "$S1LOG" --replay "$CUT" \
            --start-entry "$pos" --limit $((f - pos + 1))
        pos=$((f + 1))
        cp --sparse=always "$CUT" "$SCRATCH"
        erc=0
        eout=$(LC_ALL=C e2fsck -fy "$SCRATCH" 2>&1) || erc=$?
        if [ "$erc" -ge 8 ]; then continue; fi
        # Dirty proof: this cut really makes the next mount replay.
        if ! echo "$eout" | grep -qi "recovering journal"; then continue; fi
        if ! debugfs -R "cat /.dirty_cut_sentinel" "$SCRATCH" 2>/dev/null \
            | grep -q "DIRTY-START-CUT"; then continue; fi
        if debugfs -R "ls /" "$SCRATCH" 2>/dev/null | grep -q "wd_"; then continue; fi
        if ! dumpe2fs -h "$CUT" 2>/dev/null | grep -qi "needs_recovery"; then continue; fi
        if ! "$HERE/judge.sh" "$CUT" > "$STAGE/s0/cut-judge.log" 2>&1; then
            echo "matrix: dirty-start cut candidate at entry $f judged RED —" \
                "stage-1 crash bug, not a harness state (see $INFO)" >&2
            cat "$STAGE/s0/cut-judge.log" >> "$INFO"
            exit 1
        fi
        ok=$f
        break
    done
    if [ -z "$ok" ]; then
        echo "matrix: no usable dirty-start cut point in the first 60 flushes" >&2
        exit 1
    fi
    {
        echo "cut_entry=$ok"
        echo "stage1_log=$S1LOG"
        echo "sentinel=DIRTY-START-CUT (post-replay), needs_recovery=set (raw)"
        dumpe2fs -h "$CUT" 2>/dev/null | grep -i "features"
    } > "$INFO"
    rm -f "$SCRATCH"
    # The cut image IS stage 2's pristine.
    mv "$CUT" "$STAGE/s0/pristine.img"
    echo "matrix: dirty-start cut at entry $ok (details -> $INFO)"
}

# ---- record + sweep pipeline ----------------------------------------------
FAILURES=

launch_sweep() { # <i> — background sweep with ledger + full judge fleet
    local i=$1 SDIR=$STAGE/s$i
    rm -f "$BUILD/matrix.s$i.progress.tsv"
    rm -rf "$BUILD/matrix-evidence/s$i"
    (
        export TMPDIR="$SDIR"
        X4_LEDGER=$BUILD/matrix.s$i.progress.tsv \
        X4_EVIDENCE=$BUILD/matrix-evidence/s$i \
        X4_WL_LIST=$SDIR/wl.list \
        X4_EXPECT_WL=${SH_ORACLE_N[$i]} \
        X4_MIN_FLUSHES=$((2 * SH_ORACLE_N[$i])) \
            "$HERE/sweep.sh" "$SDIR/pristine.img" \
            "$BUILD/matrix.s$i.logwrites.img" \
            "$HERE/judge_all.sh" "$BUILD/matrix.s$i.oracle.table"
    ) > "$BUILD/matrix.s$i.sweep.log" 2>&1 &
    SWEEP_PIDS[$i]=$!
}

for ((i = 0; i < SHARDS; i++)); do
    if [ "$DIRTY" = 1 ]; then
        # Installed before baking so it is on the recorded disk; the name
        # sorts first, so it runs before every corpus workload.
        cp "$HERE/dirtier.sh" "$STAGE/s$i/root/.crash/00-dirty-dirtier.sh"
    fi
    bake_shard "$i"
    if [ "$DIRTY" = 1 ]; then
        boot_record dirty-stage1
        find_cut
        # Stage 2 boots FROM the dirty cut image; fresh write log.
        cp --sparse=always "$STAGE/s$i/pristine.img" "$BUILD/xfstests_test.img"
        boot_record "s$i"
    else
        boot_record "s$i"
    fi

    # Durability expectation table from the recorded console declarations
    # (checksummed against serial line loss; lost workloads are recorded as
    # explicitly unjudged).
    python3 "$HERE/oracle.py" collect "$BUILD/matrix.s$i.qemu.log" \
        "$BUILD/matrix.s$i.oracle.table"

    if [ "$WALCHECK" -gt 0 ]; then
        # WAL write-order machine check over the shard's log; the shard
        # pristine provides the static geometry. rc 3 = vacuous (judged
        # surface smaller than required) — a failure by definition here.
        wrc=0
        python3 "$HERE/walcheck.py" "$STAGE/s$i/pristine.img" \
            "$BUILD/matrix.s$i.logwrites.img" --require-matched "$WALCHECK" \
            > "$BUILD/matrix.s$i.walcheck.log" 2>&1 || wrc=$?
        if [ "$wrc" -eq 0 ]; then
            echo "matrix: s$i walcheck green:" \
                "$(tail -1 "$BUILD/matrix.s$i.walcheck.log")"
        else
            FAILURES="$FAILURES s$i:walcheck(rc=$wrc)"
            echo "matrix: s$i WALCHECK FAILED rc=$wrc (rc 3 = vacuous surface;" \
                "see $BUILD/matrix.s$i.walcheck.log)" >&2
        fi
    fi

    launch_sweep "$i"
done

# ---- wait for sweeps, per-shard finals, cleanup ---------------------------
for ((i = 0; i < SHARDS; i++)); do
    rc=0
    wait "${SWEEP_PIDS[$i]}" || rc=$?
    SWEEP_PIDS[$i]=  # reaped; keep the EXIT trap from killing a dead pid
    tail -3 "$BUILD/matrix.s$i.sweep.log" | sed "s/^/matrix s$i: /"
    if [ "$rc" -ne 0 ]; then
        FAILURES="$FAILURES s$i:sweep(rc=$rc)"
    fi

    # End-state gates: the vacuity guard (every checkpoint marker of every
    # oracle workload in force and green on the final image — without it an
    # fsync that persists nothing keeps every per-prefix check vacuously
    # green) + accounting/csum second opinions + the forensic baseline.
    FINAL_WORK=$STAGE/s$i/final-replayed.img
    cp --sparse=always "$BUILD/matrix.s$i.final.img" "$FINAL_WORK"
    e2fsck -fy "$FINAL_WORK" > /dev/null 2>&1 || {
        frc=$?
        if [ "$frc" -ge 8 ]; then
            echo "matrix: e2fsck operational error on s$i final image" >&2
            FAILURES="$FAILURES s$i:final-e2fsck"
        fi
    }
    frc=0
    python3 "$HERE/oracle.py" final "$BUILD/matrix.s$i.oracle.table" \
        "${SH_ORACLE_N[$i]}" "$FINAL_WORK" || frc=$?
    if [ "$frc" -ne 0 ]; then
        if [ "$frc" -eq 3 ]; then
            echo "matrix: s$i RUN UNJUDGED — console loss; rerun" >&2
        fi
        FAILURES="$FAILURES s$i:oracle-final(rc=$frc)"
    fi
    if ! ACC_OUT=$("$HERE/judge_accounting.sh" "$FINAL_WORK" 2>&1); then
        echo "$ACC_OUT" | head -20 >&2
        FAILURES="$FAILURES s$i:final-accounting"
    fi
    if ! python3 "$HERE/judge_csum.py" --quiet "$FINAL_WORK"; then
        FAILURES="$FAILURES s$i:final-csum"
    fi
    "$HERE/judge_structdiff.sh" "$FINAL_WORK" > "$BUILD/matrix.s$i.final.manifest"
    rm -f "$FINAL_WORK"

    # Disk discipline: green shards are cleaned down to ledger + evidence +
    # small logs; red shards keep the reproducer (write log + pristine).
    case "$FAILURES" in
    *" s$i:"*|*"s$i:"*)
        cp --sparse=always "$STAGE/s$i/pristine.img" "$BUILD/matrix.s$i.pristine.img"
        echo "matrix: s$i artifacts KEPT for reproduction" \
            "(pristine -> $BUILD/matrix.s$i.pristine.img)"
        ;;
    *)
        if [ "$KEEP" = 0 ]; then
            rm -f "$BUILD/matrix.s$i.final.img" "$BUILD/matrix.s$i.logwrites.img" \
                "$BUILD/matrix.dirty-stage1.logwrites.img" \
                "$BUILD/matrix.dirty-stage1.final.img"
        fi
        ;;
    esac
done

# ---- summary ---------------------------------------------------------------
kill "$SAMPLER_PID" 2>/dev/null || true
SAMPLER_PID=
echo "matrix: ---- summary ----"
for ((i = 0; i < SHARDS; i++)); do
    g=$(awk -F'\t' '$3 == "GREEN"' "$BUILD/matrix.s$i.progress.tsv" 2>/dev/null | wc -l)
    r=$(awk -F'\t' '$3 == "RED"' "$BUILD/matrix.s$i.progress.tsv" 2>/dev/null | wc -l)
    echo "matrix: s$i: $g green / $r red (ledger $BUILD/matrix.s$i.progress.tsv)"
done
[ -f "$PEAKS" ] && sed 's/^/matrix: peak /' "$PEAKS"
if [ -n "$FAILURES" ]; then
    echo "matrix: FAILURES:$FAILURES" >&2
    exit 1
fi
echo "matrix: $total workloads recorded, swept clean, oracle green"
