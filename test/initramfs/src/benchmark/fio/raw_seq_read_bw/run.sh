#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -e

echo "*** Running the FIO sequential read test (raw device) ***"

FIO_BS="${FIO_BS:-1M}"

/benchmark/bin/fio -rw=read -filename=/dev/vda -name=seqread \
-size=1G -bs="${FIO_BS}" \
-ioengine=sync -direct=1 -numjobs=1 -fsync_on_close=1 \
-time_based=1 -ramp_time=60 -runtime=100
