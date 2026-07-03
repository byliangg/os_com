#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -e

echo "*** Running the FIO buffered sequential read test (ext4, cold+warm) ***"

mkdir -p /ext4
mount -t ext4 /dev/vdc /ext4

/benchmark/bin/fio -rw=write -filename=/ext4/fio-test -name=prep \
-size=1G -bs=1M -ioengine=sync -direct=0 -numjobs=1 -fsync_on_close=1 > /dev/null

umount /ext4
mount -t ext4 /dev/vdc /ext4

echo "=== COLD READ ==="
/benchmark/bin/fio -rw=read -filename=/ext4/fio-test -name=seqread-cold \
-size=1G -bs=1M -ioengine=sync -direct=0 -numjobs=1

echo "=== WARM READ ==="
/benchmark/bin/fio -rw=read -filename=/ext4/fio-test -name=seqread-warm \
-size=1G -bs=1M -ioengine=sync -direct=0 -numjobs=1
