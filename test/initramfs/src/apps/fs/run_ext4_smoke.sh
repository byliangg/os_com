#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -e

mkdir -p /ext4
mount -t ext4 /dev/vdc /ext4

# lookup/readdir/read/create/write/unlink/mkdir/rmdir smoke
echo "hello-ext4" > /ext4/a.txt
cat /ext4/a.txt | grep -q "hello-ext4"

mkdir /ext4/d1
echo "world-ext4" > /ext4/d1/b.txt
cat /ext4/d1/b.txt | grep -q "world-ext4"
ls /ext4 >/dev/null
ls /ext4/d1 >/dev/null

rm /ext4/d1/b.txt
rmdir /ext4/d1

umount /ext4
echo "ext4 smoke passed."
poweroff -f
