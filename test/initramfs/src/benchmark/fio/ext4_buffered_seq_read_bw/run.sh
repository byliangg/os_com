#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -e

echo "*** Running the FIO buffered sequential read test (Ext4) ***"

FIO_SIZE="${FIO_SIZE:-1G}"
FIO_BS="${FIO_BS:-1M}"
TEST_FILE="${FIO_TEST_FILE:-/ext4/fio-buffered-read-test}"

echo "*** Preparing the read file with direct I/O to avoid pre-warming buffered cache ***"
/benchmark/bin/fio -rw=write -filename="${TEST_FILE}" -name=prepare_direct_write \
-size="${FIO_SIZE}" -bs="${FIO_BS}" \
-ioengine=sync -direct=1 -numjobs=1 -fsync_on_close=1

echo "*** Running the cold buffered read pass (direct=0) ***"
/benchmark/bin/fio -rw=read -filename="${TEST_FILE}" -name=buffered_cold_seqread \
-size="${FIO_SIZE}" -bs="${FIO_BS}" \
-ioengine=sync -direct=0 -numjobs=1

echo "*** Running the warm buffered read pass (direct=0) ***"
/benchmark/bin/fio -rw=read -filename="${TEST_FILE}" -name=buffered_warm_seqread \
-size="${FIO_SIZE}" -bs="${FIO_BS}" \
-ioengine=sync -direct=0 -numjobs=1
