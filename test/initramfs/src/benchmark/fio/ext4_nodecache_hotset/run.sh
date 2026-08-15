#!/bin/sh

# SPDX-License-Identifier: MPL-2.0
#
# NodeCache-focused workload. The setup alternates one 4 KiB block of the
# target with one block of a spacer file, producing a target with many small
# extents. After a clean remount, fio repeatedly overwrites only those mapped
# blocks through O_DIRECT. This makes extent-node lookup, rather than data
# allocation, the hot path under test.

set -eu

FIO=/benchmark/bin/fio
MNT=/ext4
TARGET=$MNT/nodecache-hotset
SPACER=$MNT/nodecache-spacer
REFERENCE=$MNT/nodecache-reference
CONFIG=/ext2/nodecachecfg

# The host writes this tiny config into the ext2 helper disk before each boot.
# It keeps the workload parameters identical across the node-cache A/B legs
# without rebuilding the initramfs.
if [ -f "$CONFIG" ]; then
    . "$CONFIG"
fi
BLOCKS="${NODECACHE_BLOCKS:-128}"
RUNTIME="${NODECACHE_RUNTIME:-20}"

mkdir -p "$MNT"
mount -t ext4 /dev/vdc "$MNT"

echo "NODEBENCH_SETUP blocks=$BLOCKS block_size=4K"
: > "$TARGET"
: > "$SPACER"
i=0
while [ "$i" -lt "$BLOCKS" ]; do
    # Separate files force the allocator to interleave their physical blocks;
    # logical blocks in TARGET remain consecutive, so every target block is a
    # valid 4 KiB direct-I/O overwrite location during the measured phase.
    dd if=/dev/zero of="$TARGET" bs=4K count=1 seek="$i" conv=notrunc 2>/dev/null
    dd if=/dev/zero of="$SPACER" bs=4K count=1 seek="$i" conv=notrunc 2>/dev/null
    i=$((i + 1))
done
dd if=/dev/zero of="$REFERENCE" bs=4K count="$BLOCKS" 2>/dev/null
sync

# Do not let setup populate the measured inode's NodeCache. A clean remount
# gives both A/B legs the same cold metadata-cache starting point.
umount "$MNT"
mount -t ext4 /dev/vdc "$MNT"

echo "NODEBENCH_MEASURE_BEGIN direct=1 rw=randwrite bs=4K size=$((BLOCKS * 4))K runtime=${RUNTIME}s"
"$FIO" --name=nodecache-hotset --filename="$TARGET" --rw=randwrite \
    --size="$((BLOCKS * 4))K" --bs=4K --ioengine=sync --direct=1 --numjobs=1 \
    --time_based=1 --ramp_time=3 --runtime="$RUNTIME" --randrepeat=1 \
    --buffer_pattern=0x00
echo "NODEBENCH_MEASURE_FIO_DONE"

sync
# Ext4 and the target inode are destroyed here. With ext4.node_cache_stats=1,
# the kernel emits the per-inode NODECACHE counters before this command returns.
umount "$MNT"

# The benchmark writes zeros over an all-zero file. Reopen and compare all
# target blocks with a separately allocated zero reference, then cleanly
# unmount. This validates the data result independently of cache state.
mount -t ext4 /dev/vdc "$MNT"
if cmp -s "$TARGET" "$REFERENCE"; then
    echo "NODEBENCH_DATA_CHECK PASS"
else
    echo "NODEBENCH_DATA_CHECK FAIL"
    exit 1
fi
umount "$MNT"
echo "NODEBENCH_DONE"
