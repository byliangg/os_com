#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -e

echo "*** Running the LMbench open latency test on ext4 ***"

touch /ext4/testfile
/benchmark/bin/lmbench/lat_syscall -P 1 -W 1000 -N 1000 open /ext4/testfile
