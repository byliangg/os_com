#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -e

echo "*** Running the FIO sequential read test (Ext4, no journal) ***"

FIO_BS="${FIO_BS:-1M}"
FIO_NUMJOBS="${FIO_NUMJOBS:-1}"

/benchmark/bin/fio -rw=read -filename=/ext4/fio-test -name=seqread \
-size=1G -bs="${FIO_BS}" \
-ioengine=sync -direct=1 -numjobs="${FIO_NUMJOBS}" -fsync_on_close=1 \
-time_based=1 -ramp_time=60 -runtime=100
