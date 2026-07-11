#!/bin/sh

# SPDX-License-Identifier: MPL-2.0
#
# SQLite speedtest1 on ext4 —— P9 计分板的真实负载 case（test.md §6.2）。
# 性能与数据无损绑定：跑完 speedtest1 后对产出库跑 PRAGMA integrity_check，
# 只有 "ok" 才算这个 case 成立。P9CASE/P9DONE 标记供 host 侧 p9_parse.py 取
# 时间与判定；surgery 落地前 speedtest1 预期崩于 test 150（reserialize
# mega-buffer），host 把它如实记成 CRASH 行——那是 before 台账的一部分。

set -e

mkdir -p /ext4
mount -t ext4 /dev/vdc /ext4

echo "P9CASE sqlite-speedtest1"
time /benchmark/bin/sqlite-speedtest1 --size 1000 /ext4/test.db
echo "P9DONE sqlite-speedtest1"

echo "P9CASE sqlite-integrity"
/benchmark/bin/sqlite3 /ext4/test.db "PRAGMA integrity_check;"
echo "P9DONE sqlite-integrity"

echo "P9ALLDONE"
