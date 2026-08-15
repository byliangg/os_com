#!/usr/bin/env bash

# SPDX-License-Identifier: MPL-2.0
#
# Alternating A/B runner for the Ext4 NodeCache hotset benchmark. Results are
# intentionally kept under test/bench/nodecache_hotset/ and are not committed.

set -euo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
cd "$REPO"

OUT="${OUT:-test/bench/nodecache_hotset}"
BUILD=test/initramfs/build
ROUNDS="${ROUNDS:-3}"
RUNTIME="${NODECACHE_RUNTIME:-20}"
BLOCKS="${NODECACHE_BLOCKS:-128}"

mkdir -p "$OUT"
printf 'round\tmode\trc\tmetric\n' > "$OUT/summary.tsv"

prepare_image() {
    rm -f "$BUILD/xfstests_test.img"
    truncate -s 2G "$BUILD/xfstests_test.img"
    mke2fs -F -q -t ext4 -b 4096 -I 256 \
        -O has_journal,extent,filetype,metadata_csum,dir_index,64bit,flex_bg,^inline_data \
        "$BUILD/xfstests_test.img"
    sync
    echo 3 > /proc/sys/vm/drop_caches
}

write_guest_config() {
    local cfg
    cfg=$(mktemp)
    printf 'NODECACHE_BLOCKS=%s\nNODECACHE_RUNTIME=%s\n' "$BLOCKS" "$RUNTIME" > "$cfg"
    debugfs -w -R 'rm nodecachecfg' "$BUILD/ext2.img" >/dev/null 2>&1 || true
    debugfs -w -R "write $cfg nodecachecfg" "$BUILD/ext2.img" >/dev/null
    if ! debugfs -R 'cat nodecachecfg' "$BUILD/ext2.img" 2>/dev/null | diff -q - "$cfg" >/dev/null; then
        rm -f "$cfg"
        echo 'FATAL: nodecache guest config verification failed' >&2
        return 1
    fi
    rm -f "$cfg"
}

run_case() {
    local round="$1" mode="$2" enabled log fsck rc=0
    enabled=0
    [ "$mode" = on ] && enabled=1
    log="$OUT/r${round}_${mode}.log"
    fsck="$OUT/r${round}_${mode}.e2fsck.log"

    echo "=== START round=$round mode=$mode node_cache=$enabled $(date -Iseconds) ===" | tee -a "$OUT/state"
    prepare_image
    write_guest_config
    ATTACH_XFSTESTS_IMAGES=true \
        VDSO_LIBRARY_DIR=/root/asterinas/benchmark/assets/linux_vdso \
        EXT4_NODE_CACHE="$enabled" EXT4_NODE_CACHE_STATS=1 \
RELEASE=1 MEM=8G LOG_LEVEL=warn \
        timeout --foreground --kill-after=30 900 \
        make run_kernel BENCHMARK=fio/ext4_nodecache_hotset \
        BOOT_METHOD=qemu-direct OVMF=off CONSOLE=ttyS0 > "$log" 2>&1 < /dev/null || rc=$?

    if ! e2fsck -fn "$BUILD/xfstests_test.img" > "$fsck" 2>&1; then
        rc=1
    fi

    printf '%s\t%s\t%s\t%s\n' "$round" "$mode" "$rc" \
        "$(awk '
            /NODEBENCH_MEASURE_BEGIN/ { begin = $0; capture = 1 }
            capture && /write: IOPS=/ { fio = $0 }
            capture && /NODECACHE_PROGRESS/ { stats = $0 }
            capture && /NODEBENCH_DATA_CHECK/ { check = $0 }
            capture && /NODEBENCH_DONE/ { done = $0 }
            END { printf "%s;%s;%s;%s;%s;", begin, fio, stats, check, done }
        ' "$log" || true)" \
        >> "$OUT/summary.tsv"
    echo "=== END round=$round mode=$mode rc=$rc $(date -Iseconds) ===" | tee -a "$OUT/state"
    pkill -9 -f qemu-system-x86_64 2>/dev/null || true
    sleep 2
    return "$rc"
}

# Warm build only. The measured output begins after each fresh filesystem is
# mounted inside the guest, not during this initramfs build.
make initramfs BENCHMARK=fio/ext4_nodecache_hotset > "$OUT/initramfs-build.log" 2>&1

overall=0
for round in $(seq 1 "$ROUNDS"); do
    # Reverse the order on even rounds so host drift cannot favor one mode.
    if [ $((round % 2)) -eq 1 ]; then modes='off on'; else modes='on off'; fi
    for mode in $modes; do
        run_case "$round" "$mode" || overall=1
    done
done

printf '%s\n' "$overall" > "$OUT/rc"
echo "=== COMPLETE rc=$overall $(date -Iseconds) ===" | tee -a "$OUT/state"
exit "$overall"
