#!/usr/bin/busybox sh

# SPDX-License-Identifier: MPL-2.0

set -e

MNT=/ext4
mkdir -p "${MNT}"

TRACE_MNT=/trace
TRACE_FILE=""
mkdir -p "${TRACE_MNT}"
if mount -t ext2 /dev/vda "${TRACE_MNT}" 2>/dev/null; then
    TRACE_FILE="${TRACE_MNT}/ext4_smoke_trace.log"
fi

log() {
    echo "$1"
    if [ -n "${TRACE_FILE}" ]; then
        echo "$1" >> "${TRACE_FILE}"
    fi
}

log "ext4 smoke start"

EXT4_DEV=""
for dev in /dev/vdc /dev/vdb /dev/vda; do
    if [ ! -b "${dev}" ]; then
        log "skip ${dev}: not a block device"
        continue
    fi

    log "try mount ${dev} as ext4"
    if mount -t ext4 "${dev}" "${MNT}"; then
        EXT4_DEV="${dev}"
        break
    else
        log "mount failed on ${dev}"
    fi
done

if [ -z "${EXT4_DEV}" ]; then
    log "ext4 smoke failed: no mountable ext4 block device found"
    if [ -n "${TRACE_FILE}" ]; then
        sync
        umount "${TRACE_MNT}"
    fi
    poweroff -f
    exit 1
fi

log "ext4 smoke using ${EXT4_DEV}"

# lookup/readdir/read/create/write/unlink/mkdir/rmdir/truncate smoke
echo "hello-ext4" > "${MNT}/a.txt"
grep -q "hello-ext4" "${MNT}/a.txt"

mkdir "${MNT}/d1"
echo "world-ext4" > "${MNT}/d1/b.txt"
grep -q "world-ext4" "${MNT}/d1/b.txt"

ls "${MNT}" >/dev/null
ls "${MNT}/d1" >/dev/null

: > "${MNT}/d1/b.txt"
test ! -s "${MNT}/d1/b.txt"

rm "${MNT}/d1/b.txt"
rmdir "${MNT}/d1"

umount "${MNT}"
log "ext4 smoke passed."
if [ -n "${TRACE_FILE}" ]; then
    sync
    umount "${TRACE_MNT}"
fi
poweroff -f
