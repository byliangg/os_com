#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -e

echo "*** Running the FIO buffered sequential write test (ext2 reference) ***"

/benchmark/bin/fio -rw=write -filename=/ext2/fio-test -name=seqwrite \
-size=1G -bs=1M \
-ioengine=sync -direct=0 -numjobs=1 -fsync_on_close=1
