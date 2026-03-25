#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -e

echo "*** Running the LMbench lmdd test on ext4 ***"

dd if=/dev/zero of=/ext4/zero_file bs=1M count=512
echo -n "lmdd result: " & /benchmark/bin/lmbench/lmdd if=/ext4/zero_file of=/ext4/test_file
