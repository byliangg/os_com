#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

set -e

echo "*** Running the SQLite speedtest1 (Ext4) ***"

# Real-application workload: SQLite stores the whole database as one file on the
# filesystem and exercises buffered I/O + frequent fsync (transaction commits) +
# random small reads/writes. `--size` scales the number of rows/operations.
SQLITE_SIZE="${SQLITE_SIZE:-1000}"

rm -f /ext4/test.db /ext4/test.db-journal /ext4/test.db-wal 2>/dev/null || true
/benchmark/bin/sqlite-speedtest1 --size "${SQLITE_SIZE}" /ext4/test.db
