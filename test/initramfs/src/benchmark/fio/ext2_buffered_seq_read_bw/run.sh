#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -e

echo "*** Running the FIO buffered sequential read test (ext2 reference, cold+warm) ***"

/benchmark/bin/fio -rw=write -filename=/ext2/fio-test -name=prep \
-size=1G -bs=1M -ioengine=sync -direct=0 -numjobs=1 -fsync_on_close=1 > /dev/null

umount /ext2
mount -t ext2 /dev/vda /ext2

echo "=== COLD READ ==="
/benchmark/bin/fio -rw=read -filename=/ext2/fio-test -name=seqread-cold \
-size=1G -bs=1M -ioengine=sync -direct=0 -numjobs=1

echo "=== WARM READ ==="
/benchmark/bin/fio -rw=read -filename=/ext2/fio-test -name=seqread-warm \
-size=1G -bs=1M -ioengine=sync -direct=0 -numjobs=1
