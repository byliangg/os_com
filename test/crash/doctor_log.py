#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Doctors a recorded blklogwrites log to inject a mid-sweep failure.

    doctor_log.py list-markers <log.img>
    doctor_log.py corrupt-marker <log.img> <out.img> <workload> <ckpt>
    doctor_log.py corrupt-entry  <log.img> <out.img> <entry> [byte-count]

Calibration tool for the sweep ledger (sweep.sh X4_LEDGER / judge_record.sh):
a sweep over a doctored log MUST record the injected red at the right
points, keep sweeping, and summarize correctly — a continue-on-failure
mechanism that has never seen a failure is untested plumbing.

`corrupt-marker` targets the durability oracle deterministically: it finds
the log entry that carries checkpoint <ckpt>'s on-disk marker bytes for
<workload> (the fsynced ckpt_N file oracle.py scans the raw image for) and
overwrites the marker magic in the OUTPUT COPY of the log. From the point
that entry is replayed onward, marker N's bytes are absent while any later
checkpoint's marker still lands, so oracle.py's marker-contiguity law
("the in-force set must be a contiguous prefix") flips RED at every
subsequent FLUSH point that has a later marker in force — including the
final state, which the vacuity guard (oracle.py final) also refuses.
The original log is never touched.

`corrupt-entry` is the blunt variant: XORs the first byte-count (default
64) payload bytes of the given entry, whatever it is. Corrupting a data
write turns the oracle red at points where that file's content is asserted;
corrupting a journal write typically breaks the transaction checksum and
shifts what e2fsck replays. Deterministic only in that the bytes differ —
what the judges make of it depends on the entry. Prefer corrupt-marker for
calibration.

Exit 0 on success (the requested corruption was applied / listing done),
2 on usage or "target not found".
"""

import os
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import walcheck as W  # noqa: E402  (log parser shared with the WAL checker)

MAGIC = b"X4CKPT1!"


def entry_payload(logf, sectorsize, e):
    if e.flags != 0:
        return b""
    logf.seek(e.data_off)
    return logf.read(e.nr_sectors * sectorsize)


def find_marker_entries(log_path):
    """Yields (entry, offset-in-payload, marker-line) for marker writes."""
    logf, sectorsize, entries = W.read_log(log_path)
    for e in entries:
        data = entry_payload(logf, sectorsize, e)
        i = data.find(MAGIC)
        if i >= 0:
            end = data.find(b"\n", i)
            line = data[i:end if end > 0 else i + 64]
            yield e, i, line.decode("ascii", errors="replace")
    logf.close()


def main(argv):
    if len(argv) >= 2 and argv[0] == "list-markers":
        for e, off, line in find_marker_entries(argv[1]):
            print(f"entry {e.idx}\tsector {e.sector}\t+{off}\t{line}")
        return 0

    if len(argv) == 5 and argv[0] == "corrupt-marker":
        log, out, workload, ckpt = argv[1:]
        want = f"{MAGIC.decode()}{workload}|{ckpt}|"
        target = None
        for e, off, line in find_marker_entries(log):
            if line.startswith(want):
                target = (e, off, line)
                break
        if target is None:
            print(f"doctor_log: no marker write for {workload} ckpt {ckpt} "
                  f"in {log} (list-markers to see candidates)", file=sys.stderr)
            return 2
        e, off, line = target
        # Logs are created 4G-sparse; a naive byte copy would materialize
        # the holes (4G of pinned RAM when the copy lands on tmpfs).
        subprocess.run(["cp", "--sparse=always", log, out], check=True)
        with open(out, "r+b") as f:
            f.seek(e.data_off + off)
            f.write(b"XXGONE!!")  # same length as the magic, never matches
        print(f"doctor_log: entry {e.idx} (sector {e.sector}): marker "
              f"'{line}' magic overwritten in {out}")
        return 0

    if len(argv) in (4, 5) and argv[0] == "corrupt-entry":
        log, out, idx = argv[1], argv[2], int(argv[3])
        count = int(argv[4]) if len(argv) == 5 else 64
        logf, sectorsize, entries = W.read_log(log)
        logf.close()
        matches = [e for e in entries if e.idx == idx]
        if not matches or matches[0].flags != 0:
            print(f"doctor_log: entry {idx} not found or has no payload",
                  file=sys.stderr)
            return 2
        e = matches[0]
        count = min(count, e.nr_sectors * sectorsize)
        # Logs are created 4G-sparse; a naive byte copy would materialize
        # the holes (4G of pinned RAM when the copy lands on tmpfs).
        subprocess.run(["cp", "--sparse=always", log, out], check=True)
        with open(out, "r+b") as f:
            f.seek(e.data_off)
            data = bytearray(f.read(count))
            f.seek(e.data_off)
            f.write(bytes(b ^ 0xFF for b in data))
        print(f"doctor_log: entry {idx} (sector {e.sector}): first {count} "
              f"payload bytes XORed in {out}")
        return 0

    print(__doc__.strip().split("\n\n")[0], file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
