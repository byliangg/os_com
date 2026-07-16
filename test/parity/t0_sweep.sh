#!/bin/bash
# P10-T0: dual-side xfstests full-suite baseline sweep (ext4 vs ext2 parity).
#
# Runs INSIDE the dev container at /root/asterinas. One side per invocation:
#   bash test/parity/t0_sweep.sh ext4
#   bash test/parity/t0_sweep.sh ext2
#
# Design (P10_plan.md sec 2, T0):
#  - Enumerate every upstream generic/* test (plus ext4/* on the ext4 side),
#    shard into chunk runlists baked into the initramfs once, then boot one
#    QEMU per chunk parsing ./check verdicts out of qemu.log.
#  - A boot that times out or crashes only loses its chunk remainder: rounds
#    re-run unverdicted tests with shrinking chunk sizes (25 -> 5 -> 1); a
#    test with no verdict after a solo boot is recorded HANG.
#  - Images are recreated from scratch before every boot (experience 14.8),
#    formatted to match the side's local.config MKFS_OPTIONS.
#  - local.config is patched for the ext2 side and restored on exit.
set -u

SIDE=${1:?usage: t0_sweep.sh <ext4|ext2>}
REPO=/root/asterinas
XFSDIR=$REPO/test/initramfs/src/conformance/xfstests
BUILD=$REPO/test/initramfs/build
OUT=$REPO/test/parity/out/$SIDE
MEM=${MEM:-12G}
DISK_BYTES=${DISK_BYTES:-12884901888}   # 12G, must match XFSTESTS_DISK_SIZE
DISK_SIZE=${DISK_SIZE:-12G}
CHUNK0=${CHUNK0:-25}
TIMEOUT0=${TIMEOUT0:-3600}
TIMEOUT_SOLO=${TIMEOUT_SOLO:-5400}
ONLY_TESTS=${ONLY_TESTS:-}              # smoke override: space-separated names

mkdir -p "$OUT"
VERDICTS=$OUT/verdicts.tsv
touch "$VERDICTS"

XFS_PKG=$(ls -d /nix/store/*-xfstests-2023.05.14/lib/xfstests 2>/dev/null | head -1)
[ -n "$XFS_PKG" ] || { echo "t0: xfstests package not found in nix store" >&2; exit 1; }

restore_config() { cd "$REPO" && git checkout -q -- "$XFSDIR/local.config" 2>/dev/null || true; }
trap restore_config EXIT

case "$SIDE" in
ext4)
    restore_config   # pristine tree config (P5 feature set, keeps 96.3% comparability)
    MKFS="mke2fs -t ext4 -F -q -b 4096 -I 256 -O has_journal,extent,filetype,^metadata_csum,^dir_index,^64bit,^flex_bg,^inline_data,^resize_inode,^uninit_bg"
    ;;
ext2)
    restore_config
    sed -i -e 's/^export FSTYP=.*/export FSTYP=ext2/' \
           -e 's/^export MKFS_OPTIONS=.*/export MKFS_OPTIONS="-F -b 4096"/' \
           "$XFSDIR/local.config"
    MKFS="mke2fs -t ext2 -F -q -b 4096"
    ;;
*) echo "t0: side must be ext4 or ext2" >&2; exit 1 ;;
esac

# --- test inventory ---------------------------------------------------------
list_tests() {
    if [ -n "$ONLY_TESTS" ]; then
        for t in $ONLY_TESTS; do echo "$t"; done
        return
    fi
    ls "$XFS_PKG/tests/generic" | grep -E '^[0-9]+$' | sort -n | sed 's|^|generic/|'
    if [ "$SIDE" = ext4 ]; then
        ls "$XFS_PKG/tests/ext4" | grep -E '^[0-9]+$' | sort -n | sed 's|^|ext4/|'
    fi
}

pending_tests() {  # inventory minus anything already verdicted
    list_tests | while IFS= read -r t; do
        grep -q "^$t	" "$VERDICTS" || echo "$t"
    done
}

# --- qemu.log verdict parser ------------------------------------------------
parse_log() {  # <log> <chunkfile>  -> appends "test<TAB>verdict<TAB>detail"
    local log=$1 chunk=$2
    tr -d '\r' < "$log" | awk '
        /^(generic|ext4)\/[0-9]+([ \t]|$)/ {
            name=$1; rest=substr($0, length(name)+1);
            v=""; d="";
            if (rest ~ /\[not run\]/)        { v="NOTRUN"; sub(/^.*\[not run\][ \t]*/,"",rest); d=rest }
            else if (rest ~ /\[expunged\]/)  { v="EXPUNGED" }
            else if (rest ~ /output mismatch|\[failed|\[dumped core|_check_.*filesystem/) { v="FAIL"; d=rest }
            else if (rest ~ /^[ \t]*[0-9]+s[ \t]*$/) { v="PASS" }
            else if (rest ~ /^[ \t]*$/)      { next }  # bare name: started, no verdict yet
            else                             { v="FAIL"; d=rest }
            gsub(/\t/," ",d); if (length(d)>200) d=substr(d,1,200);
            print name "\t" v "\t" d
        }' | while IFS= read -r line; do
            t=${line%%	*}
            grep -q "^$t	" "$VERDICTS" || echo "$line" >> "$VERDICTS"
        done
}

# --- one boot over one chunk file -------------------------------------------
BOOT_N=0
boot_chunk() {  # <chunk-list-path-in-tree> <timeout>
    local chunk=$1 tmo=$2 guest_list rc
    guest_list=/opt/xfstests/$(basename "$chunk")
    BOOT_N=$((BOOT_N+1))
    cd "$REPO"
    rm -f "$BUILD/xfstests_test.img" "$BUILD/xfstests_scratch.img" qemu.log
    truncate -s "$DISK_BYTES" "$BUILD/xfstests_test.img" && $MKFS "$BUILD/xfstests_test.img"
    truncate -s "$DISK_BYTES" "$BUILD/xfstests_scratch.img" && $MKFS "$BUILD/xfstests_scratch.img"
    echo "t0[$SIDE] boot#$BOOT_N chunk=$(basename "$chunk") ($(grep -vc '^#' "$chunk") tests) tmo=${tmo}s $(date +%H:%M:%S)"
    timeout "$tmo" make run_kernel AUTO_TEST=conformance CONFORMANCE_TEST_SUITE=xfstests \
        RELEASE=1 MEM="$MEM" XFSTESTS_DISK_SIZE="$DISK_SIZE" \
        XFSTESTS_RUNLIST="$guest_list" >/dev/null 2>&1
    rc=$?
    [ -f qemu.log ] && parse_log qemu.log "$chunk"
    [ -f qemu.log ] && gzip -c qemu.log > "$OUT/boot_$(printf %03d "$BOOT_N").qemu.log.gz"
    return $rc
}

# --- rounds -----------------------------------------------------------------
# All chunk files of a round are written BEFORE the first boot so the
# initramfs (which bakes *.list) rebuilds once per round, not once per boot.
round() {  # <round-no> <chunk-size> <timeout>
    local rn=$1 sz=$2 tmo=$3 idx=0 i chunk
    local -a pend chunks=()
    mapfile -t pend < <(pending_tests)
    [ "${#pend[@]}" -gt 0 ] || return 0
    echo "t0[$SIDE] round$rn: ${#pend[@]} tests pending, chunk=$sz"
    for ((i=0; i<${#pend[@]}; i+=sz)); do
        chunk=$XFSDIR/t0_${SIDE}_r${rn}_$(printf %03d "$idx").list
        printf '%s\n' "${pend[@]:i:sz}" > "$chunk"
        chunks+=("$chunk"); idx=$((idx+1))
    done
    for chunk in "${chunks[@]}"; do
        boot_chunk "$chunk" "$tmo" || true
    done
    rm -f "$XFSDIR"/t0_${SIDE}_r${rn}_*.list
}

round 0 "$CHUNK0" "$TIMEOUT0"
round 1 5 "$TIMEOUT0"
round 2 1 "$TIMEOUT_SOLO"

# anything still unverdicted after a solo boot is a confirmed hang/crash
pending_tests | while IFS= read -r t; do
    echo "$t	HANG	no verdict after solo boot (vm hang or crash)" >> "$VERDICTS"
done

rm -f "$XFSDIR"/t0_${SIDE}_r*.list
restore_config

total=$(list_tests | wc -l)
echo "t0[$SIDE] done: $total tests"
cut -f2 "$VERDICTS" | sort | uniq -c | sed "s/^/t0[$SIDE]   /"
