#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -e

echo "*** Running the FIO buffered sequential write test (Ext4) ***"

FIO_SIZE="${FIO_SIZE:-1G}"
FIO_BS="${FIO_BS:-1M}"
FIO_NUMJOBS="${FIO_NUMJOBS:-1}"
TEST_FILE="${FIO_TEST_FILE:-/ext4/fio-buffered-write-test}"

/benchmark/bin/fio -rw=write -filename="${TEST_FILE}" -name=buffered_seqwrite \
-size="${FIO_SIZE}" -bs="${FIO_BS}" \
-ioengine=sync -direct=0 -numjobs="${FIO_NUMJOBS}" -fsync_on_close=1
