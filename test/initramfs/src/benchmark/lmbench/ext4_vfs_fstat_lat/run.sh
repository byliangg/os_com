#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -e

echo "*** Running the LMbench fstat latency test on ext4 ***"

touch /ext4/test_file
/benchmark/bin/lmbench/lat_syscall -P 1 fstat /ext4/test_file
