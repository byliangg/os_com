#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -e

echo "*** Running the FIO sequential write test (Ext4, no journal) ***"

FIO_BS="${FIO_BS:-1M}"
FIO_FSYNC="${FIO_FSYNC:-}"
FIO_NUMJOBS="${FIO_NUMJOBS:-1}"
FIO_FSYNC_ARGS=""
if [ -n "${FIO_FSYNC}" ]; then
    FIO_FSYNC_ARGS="-fsync=${FIO_FSYNC}"
fi

/benchmark/bin/fio -rw=write -filename=/ext4/fio-test -name=seqwrite \
-size=1G -bs="${FIO_BS}" \
-ioengine=sync -direct=1 -numjobs="${FIO_NUMJOBS}" ${FIO_FSYNC_ARGS} -fsync_on_close=1 \
-time_based=1 -ramp_time=60 -runtime=100
