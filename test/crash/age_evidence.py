#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Proves age-triggered commits from a recorded blklogwrites log.

    age_evidence.py <log.img> [--assert <workload>:<commits> ...]

The journal's 5s age trigger has no syscall to answer for — the only
externally visible artifact of an age commit is a journal commit nobody
asked for. This tool makes that countable, directly at the JBD2 level: it
scans the log for commit blocks (jbd2 magic, type 2, with the sequence
number) and for the checkpoint marker writes (the X4CKPT1!<workload>|
records jlang2sh.py --oracle emits and fsyncs, totally ordered within the
session), and attributes each commit to a workload span. A span ends at
the marker's own COMMIT — the first commit block after the marker's data
write (the marker is ordered file data, so its bytes land before the
transaction that publishes it) — and the next span begins there.

Reading the numbers (see protocol/age1-pending-create et al.): every age
workload issues exactly two persistence calls (a leading data fsync and
the trailing checkpoint), so its span must hold exactly 2 requested
commits; a third commit inside the dwell was requested by nobody — with
batch sizes far below the quarter-journal trigger, that is the age
trigger by elimination. The no-sleep control twins must show exactly 2,
proving spans accumulate no stray commits. FLUSH counts are printed for
context only (this kernel emits 2-3 barriers per commit: ordered-data
flush, commit, journal-superblock trail), which is why the law is stated
in commit blocks, not barriers.

--assert workload:count gates the reading: exits 1 unless the named
workload's span holds exactly <count> commit blocks.

Exit: 0 (all asserts hold / none given), 1 assert failed, 2 usage.
"""

import os
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import walcheck as W  # noqa: E402

MAGIC = b"X4CKPT1!"
JBD2_MAGIC = W.JBD2_MAGIC_BYTES


def main(argv):
    if not argv or argv[0].startswith("-"):
        print(__doc__.split("\n\n")[0], file=sys.stderr)
        return 2
    log = argv[0]
    asserts = {}
    for a in argv[1:]:
        if a == "--assert":
            continue
        wl, _, n = a.partition(":")
        asserts[wl] = int(n)

    logf, ss, entries = W.read_log(log)
    markers = []  # (entry idx, workload)
    commits = []  # (entry idx, sequence)
    nflush = 0
    for e in entries:
        if e.flags & W.LOG_FLUSH_FLAG:
            nflush += 1
            continue
        if e.flags != 0:
            continue
        logf.seek(e.data_off)
        data = logf.read(min(4096, e.nr_sectors * ss))
        i = data.find(MAGIC)
        if i >= 0:
            name = data[i + len(MAGIC):i + len(MAGIC) + 96].split(b"|")[0]
            markers.append((e.idx, name.decode("ascii", errors="replace")))
            continue
        if data[:4] == JBD2_MAGIC and len(data) >= 12:
            btype = struct.unpack(">I", data[4:8])[0]
            if btype == W.BT_COMMIT:
                commits.append((e.idx, struct.unpack(">I", data[8:12])[0]))

    print(f"age_evidence: {len(entries)} entries, {nflush} flushes, "
          f"{len(commits)} commit blocks, {len(markers)} markers")
    failed = []
    prev = -1  # entry idx of the previous span's closing commit
    span_commits = {}
    for midx, wl in markers:
        close = next(((i, s) for i, s in commits if i > midx), None)
        if close is None:
            print(f"  {wl}: marker entry {midx} has no closing commit "
                  f"(truncated log?)")
            continue
        span = [(i, s) for i, s in commits if prev < i <= close[0]]
        span_commits[wl] = span_commits.get(wl, 0) + len(span)
        print(f"  {wl}: marker entry {midx}, span commits {len(span)} "
              f"{['seq%d@%d' % (s, i) for i, s in span]}")
        prev = close[0]
    leftover = [(i, s) for i, s in commits if i > prev]
    if leftover:
        print(f"  (unattributed tail commits: "
              f"{['seq%d@%d' % (s, i) for i, s in leftover]})")
    for wl, want in asserts.items():
        got = span_commits.get(wl)
        ok = got == want
        print(f"  assert {wl}: want {want} commits, got {got} "
              f"-> {'OK' if ok else 'FAIL'}")
        if not ok:
            failed.append(wl)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
