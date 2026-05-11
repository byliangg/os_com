#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -e

echo "*** Running the FIO sequential write test (raw device, bs=16K, fsync=4) ***"

/benchmark/bin/fio -rw=write -filename=/dev/vda -name=seqwrite \
-size=1G -bs=16K \
-ioengine=sync -direct=1 -numjobs=1 -fsync=4 -fsync_on_close=1 \
-time_based=1 -ramp_time=60 -runtime=100
