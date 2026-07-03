#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -e

mkdir -p /ext4
mount -t ext4 /dev/vdc /ext4

time /benchmark/bin/sqlite-speedtest1 --size 1000 /ext4/test.db
