#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""The crash-durability data oracle (B3 auto-checker, CrashMonkey-style).

Judges what e2fsck cannot: that every entity the workload *persisted*
(fsync/fdatasync/sync, as instrumented by `jlang2sh.py --oracle`) survives
the crash with exactly the promised content, and that no freed/stale bytes
(the 0x52 pre-dye laid down by run_matrix.sh) resurface through any
asserted file.

Three subcommands, wired up by run_matrix.sh:

  oracle.py collect <qemu.log> <out.table>
      Harvests the X4ORACLE| declaration lines the instrumented workloads
      printed to the console during the recorded boot and writes the
      expectation table. Serial consoles drop and garble lines
      (experience.md §7), so every line carries a sequence number and an
      md5 checksum: any workload whose line stream fails validation is
      recorded as UNAVAIL — its assertions are skipped EXPLICITLY (listed
      in every verdict), never silently passed, never falsely reddened.

  oracle.py check <table> <img>
      The per-crash-state judge, invoked by judge.sh (via sweep.sh's
      oracle tail-arguments) on the journal-replayed scratch image.
      In-force detection: a checkpoint's promises are binding iff the
      checkpoint's on-disk marker bytes (single-block magic record written
      by the instrumented script strictly after the promises' calls
      returned) appear anywhere in the raw image. The scan is a raw byte
      search — deliberately independent of the filesystem's own structures,
      so a damaged fs cannot hide a marker it already persisted. Because
      marker files are themselves fsynced in program order, the in-force
      set must be a contiguous prefix 1..N; a gap is itself a durability
      violation (an fsynced marker's bytes vanished) and turns the state
      red. For the in-force checkpoint N, each declared entity's latest
      declaration at or before N is asserted via debugfs (dump/stat/ls on
      the replayed image, no mount needed) — files by size+content-md5,
      directories by their sorted-entry-set digest (fsync(dir) promises
      the entries, CrashMonkey's compare_entries_at_path) — unless the
      entity was modified again in the N..N+1 window (its revocation/
      redeclaration is tagged N+1), where the on-disk state may
      legitimately be old or new: those are skipped. Modifications tagged
      N+2 or later cannot appear in a prefix that lacks marker N+1 (their
      syscalls ran only after marker N+1's fsync returned), so assertions
      stand for them. Revocations tagged past the LAST checkpoint (the
      converter's sentinel id) are kept for exactly that skip window;
      non-revocation declarations past it can never be in force and are
      dropped at collect time.

  oracle.py final <table> <expected-workloads> <img>
      Vacuity guard, run once on the (journal-replayed) end-of-run image:
      every converted workload must be present in the table and EVERY
      checkpoint marker must be in force with all assertions green. This
      is what keeps the per-prefix checks honest — if markers never
      reached the disk (e.g. an fsync that silently does nothing), `check`
      would skip everything and stay green; `final` turns that into a hard
      failure. Exit 3 (not red) when the oracle itself is unavailable
      (console loss): the run must be treated as unjudged, not as passing.

Exit codes: 0 green, 1 red (a durability/consistency violation), 2 usage
or malformed input, 3 oracle unavailable (final mode only).
"""

import hashlib
import os
import re
import subprocess
import sys
import tempfile

MAGIC = b"X4CKPT1!"
BLOCK = 4096
STALE_BLOCK = b"\x52" * BLOCK


def marker_string(workload: str, ckpt: int) -> bytes:
    """Must mirror jlang2sh.py's marker_string exactly."""
    tail = f"{workload}|{ckpt}"
    h8 = hashlib.md5(tail.encode()).hexdigest()[:8]
    return MAGIC + f"{tail}|{h8}".encode()


def md5_hex(data: bytes) -> str:
    return hashlib.md5(data).hexdigest()


# ---------------------------------------------------------------- collect

def parse_console_lines(log_path):
    """Yields (valid, workload_or_None, fields) per X4ORACLE console line."""
    with open(log_path, "rb") as f:
        data = f.read()
    for raw in data.split(b"\n"):
        i = raw.find(b"X4ORACLE|")
        if i < 0:
            continue
        line = raw[i:].rstrip(b"\r \t").decode("utf-8", errors="replace")
        body = line[len("X4ORACLE|"):]
        cut = body.rfind("|")
        payload, cksum = body[:cut], body[cut + 1:]
        if cut < 0 or md5_hex(payload.encode()) != cksum:
            # Garbled: attribution is unreliable; the per-workload sequence
            # gap it leaves behind is what marks the victim UNAVAIL.
            yield False, None, None
            continue
        yield True, payload.split("|")[0], payload.split("|")


def collect(log_path, table_path):
    per_wl = {}
    garbled = 0
    for valid, wl, fields in parse_console_lines(log_path):
        if not valid:
            garbled += 1
            continue
        per_wl.setdefault(wl, []).append(fields)

    avail, unavail = [], []
    for wl, rows in sorted(per_wl.items()):
        reason = None
        seqs = []
        for f in rows:
            try:
                seqs.append(int(f[1]))
            except (IndexError, ValueError):
                reason = "malformed row"
        if reason is None:
            if sorted(seqs) != list(range(1, len(seqs) + 1)):
                reason = f"sequence gap/dup (got {len(seqs)} rows, max {max(seqs, default=0)})"
            elif rows[-1][2] != "E" or sum(1 for f in rows if f[2] == "E") != 1:
                reason = "missing/misplaced E(nd) line"
            elif not any(f[2] == "C" for f in rows):
                reason = "no checkpoint line"
            elif any(f[2] == "D" and f[4] == "missing" for f in rows):
                reason = "declared entity missing at persist time (converter bug?)"
        if reason:
            unavail.append((wl, reason))
        else:
            avail.append((wl, rows))

    with open(table_path, "w") as out:
        out.write(f"STATS\t{len(avail)}\t{len(unavail)}\t{garbled}\n")
        for wl, reason in unavail:
            out.write(f"UNAVAIL\t{wl}\t{reason}\n")
        for wl, rows in avail:
            rows.sort(key=lambda f: int(f[1]))
            ckpt_ids = [int(f[3]) for f in rows if f[2] == "C"]
            for ord_, cid in enumerate(ckpt_ids, 1):
                marker = marker_string(wl, cid).decode()
                out.write(f"CKPT\t{wl}\t{ord_}\t{cid}\t{marker}\n")
            for f in rows:
                if f[2] != "D":
                    continue
                _, _, _, ckpt, kind, size, md5, path = f
                if int(ckpt) not in ckpt_ids and kind != "x":
                    # d/f declared past the last checkpoint: never in force.
                    # Revocations (kind x) with the sentinel id ARE kept:
                    # they open the old-or-new skip window for the last
                    # checkpoint's assertions (a post-checkpoint write must
                    # not be judged against the pre-write snapshot).
                    continue
                out.write(f"DECL\t{wl}\t{ckpt}\t{kind}\t{size}\t{md5}\t{path}\n")

    print(
        f"oracle collect: {len(avail)} workloads available, "
        f"{len(unavail)} unavailable, {garbled} garbled lines -> {table_path}"
    )
    for wl, reason in unavail:
        print(f"oracle collect: UNAVAIL {wl}: {reason}")
    return 0


# ------------------------------------------------------------ check/final

class Table:
    def __init__(self, path):
        self.unavail = []          # [(workload, reason)]
        self.ckpts = {}            # workload -> [(ord, id, marker_bytes)]
        self.events = {}           # workload -> {path: [(ord, kind, size, md5)]}
        with open(path) as f:
            for line in f:
                t = line.rstrip("\n").split("\t")
                if t[0] == "UNAVAIL":
                    self.unavail.append((t[1], t[2]))
                elif t[0] == "CKPT":
                    self.ckpts.setdefault(t[1], []).append(
                        (int(t[2]), int(t[3]), t[4].encode())
                    )
                elif t[0] == "DECL":
                    wl, ckpt, kind, size, md5, p = t[1], int(t[2]), t[3], t[4], t[5], t[6]
                    lst = self.ckpts.get(wl, [])
                    ords = [o for o, cid, _ in lst if cid == ckpt]
                    if ords:
                        ord_ = ords[0]
                    elif kind == "x":
                        # Sentinel-tagged revocation (emitted after the last
                        # checkpoint): order it right after the last real
                        # checkpoint so judge()'s top+1 skip window fires.
                        ord_ = (max(o for o, _, _ in lst) + 1) if lst else 1
                    else:
                        continue  # never in force (defensive; collect drops these)
                    self.events.setdefault(wl, {}).setdefault(p, []).append(
                        (ord_, kind, size, md5)
                    )

    def all_markers(self):
        for wl, lst in self.ckpts.items():
            for ord_, _cid, marker in lst:
                yield wl, ord_, marker


def scan_markers(img_path, table):
    """One raw pass over the image; returns {workload: set(in-force ords)}.

    Byte search is done by hand (chunked bytes.find on the magic) instead of
    grep: the 0x52 pre-dye leaves gigabytes without a single newline and
    line-oriented tools would buffer it all.
    """
    candidates = {}
    maxlen = 0
    for wl, ord_, marker in table.all_markers():
        candidates[marker] = (wl, ord_)
        maxlen = max(maxlen, len(marker))
    found = {}
    if not candidates:
        return found
    chunk_size = 8 << 20
    overlap = maxlen + len(MAGIC)
    with open(img_path, "rb") as f:
        tail = b""
        while True:
            chunk = f.read(chunk_size)
            if not chunk:
                break
            buf = tail + chunk
            pos = 0
            while True:
                pos = buf.find(MAGIC, pos)
                if pos < 0:
                    break
                window = buf[pos:pos + maxlen]
                for marker, (wl, ord_) in candidates.items():
                    if window.startswith(marker):
                        found.setdefault(wl, set()).add(ord_)
                        break
                pos += 1
            tail = buf[-overlap:]
    return found


def wd_path(workload, path):
    root = f"/wd_{workload}"
    return root if path == "." else f"{root}/{path}"


def run_debugfs(img_path, commands):
    """Runs one batched debugfs session; returns {command: stdout-segment}.

    Only stdout is segmented (the `debugfs: <cmd>` prompt echoes and the
    stat payloads both go there in order); stderr is unbuffered and may
    interleave arbitrarily, so it is captured separately and ignored —
    a failed stat simply yields a segment without `Inode:`.
    """
    with tempfile.NamedTemporaryFile("w", suffix=".dbgfs", delete=False) as tf:
        tf.write("\n".join(commands) + "\n")
        cmdfile = tf.name
    try:
        proc = subprocess.run(
            ["debugfs", "-f", cmdfile, img_path],
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            env={**os.environ, "LC_ALL": "C"},  # English "Inode:"/"Type:" tokens
        )
        segments = {}
        current = None
        for line in proc.stdout.decode("utf-8", errors="replace").splitlines():
            if line.startswith("debugfs:"):
                current = line[len("debugfs:"):].strip()
                segments.setdefault(current, [])
            elif current is not None:
                segments[current].append(line)
        return {cmd: "\n".join(lines) for cmd, lines in segments.items()}
    finally:
        os.unlink(cmdfile)


def dir_digest(ls_segment):
    """Entry-set digest from a debugfs `ls -p` output segment.

    Must mirror jlang2sh.py's guest-side computation exactly:
    md5 over the newline-terminated, bytewise-sorted entry names, with
    `.`/`..` and the instrumentation's own ckpt_* marker files excluded
    (an empty directory digests the empty string). `ls -p` lines look
    like `/ino/mode/uid/gid/name/size/`; ino 0 is an empty dirent slot.
    """
    names = []
    for line in ls_segment.splitlines():
        if not line.startswith("/"):
            continue
        parts = line.split("/")
        if len(parts) < 7:
            continue
        ino_s, name = parts[1], parts[5]
        if not ino_s.isdigit() or int(ino_s) == 0:
            continue
        if name in (".", "..") or name.startswith("ckpt_"):
            continue
        names.append(name)
    blob = "".join(n + "\n" for n in sorted(names, key=str.encode))
    return md5_hex(blob.encode())


def stale_dye_blocks(content):
    """Block-aligned runs of pure 0x52 = a freed (pre-dyed) block resurfaced.

    Legitimate workload data is the 0x22 pwrite pattern and holes read back
    zero, so a full block of 0x52 inside an asserted file can only be stale
    disk content leaking through (lost write / bad extent / block revival).
    """
    hits = []
    for off in range(0, len(content) - BLOCK + 1, BLOCK):
        if content[off:off + BLOCK] == STALE_BLOCK:
            hits.append(off)
    if not hits and content and len(content) < BLOCK and set(content) == {0x52}:
        hits.append(0)
    return hits


def judge(table_path, img_path, final=False, expected_workloads=None):
    table = Table(table_path)
    found = scan_markers(img_path, table)

    failures = []
    assertions = []  # (workload, ckpt_ord, path, kind, size, md5)
    inforce_workloads = 0

    for wl, ords in sorted(found.items()):
        top = max(ords)
        inforce_workloads += 1
        want = set(range(1, top + 1))
        if ords != want:
            missing = sorted(want - ords)
            failures.append(
                f"{wl}: marker contiguity broken: checkpoint {top} is in force "
                f"but fsynced marker(s) {missing} vanished from the disk"
            )
            continue
        for path, evs in table.events.get(wl, {}).items():
            e = None
            nxt = None
            for ev in evs:  # events are in declaration order
                if ev[0] <= top:
                    e = ev
                elif nxt is None:
                    nxt = ev
            if e is None or e[1] == "x":
                continue
            if nxt is not None and nxt[0] == top + 1:
                continue  # modified in the unknown N..N+1 window: old-or-new
            assertions.append((wl, e[0], path, e[1], e[2], e[3]))

    if final:
        seen = {wl for wl, _ in table.unavail} | set(table.ckpts)
        if table.unavail or (
            expected_workloads is not None and len(seen) != expected_workloads
        ):
            print(
                f"ORACLE-UNAVAILABLE: {len(table.unavail)} workload(s) lost console "
                f"lines {[w for w, _ in table.unavail]}; saw {len(seen)}/"
                f"{expected_workloads} workloads. The run is UNJUDGED — rerun it."
            )
            return 3
        for wl, lst in sorted(table.ckpts.items()):
            missing = {o for o, _, _ in lst} - found.get(wl, set())
            if missing:
                failures.append(
                    f"{wl}: final image lacks marker(s) {sorted(missing)} — "
                    f"persisted data never reached the disk (vacuous oracle)"
                )

    # Verify all assertions in one debugfs session.
    dumps = {}
    cmds = []
    dumpdir = tempfile.mkdtemp(prefix="x4oracle-")
    try:
        for i, (wl, _ord, path, kind, size, md5) in enumerate(assertions):
            abspath = wd_path(wl, path)
            if kind == "f":
                out = os.path.join(dumpdir, str(i))
                cmds.append(f"dump {abspath} {out}")
                dumps[i] = out
            else:
                cmds.append(f"stat {abspath}")
                if kind == "d" and md5 != "-":
                    cmds.append(f"ls -p {abspath}")
        segments = run_debugfs(img_path, cmds) if cmds else {}

        for i, (wl, ord_, path, kind, size, md5) in enumerate(assertions):
            abspath = wd_path(wl, path)
            where = f"{wl} ckpt#{ord_} {abspath}"
            if kind == "f":
                out = dumps[i]
                if not os.path.exists(out):
                    failures.append(f"{where}: fsynced file MISSING (expected {size}B md5={md5})")
                    continue
                with open(out, "rb") as f:
                    content = f.read()
                if str(len(content)) != size:
                    failures.append(f"{where}: size {len(content)} != expected {size}")
                    continue
                got = md5_hex(content)
                if got != md5 and md5 != "-":
                    failures.append(f"{where}: content md5 {got} != expected {md5}")
                for off in stale_dye_blocks(content):
                    failures.append(
                        f"{where}: STALE 0x52 dye block at offset {off} — "
                        f"freed/never-written block resurfaced through this file"
                    )
            else:  # d (and any future existence-only kinds)
                seg = segments.get(f"stat {abspath}", "")
                m = re.search(r"Type:\s+(\w+)", seg)
                if "Inode:" not in seg or not m:
                    failures.append(f"{where}: fsynced directory MISSING")
                elif kind == "d" and m.group(1) != "directory":
                    failures.append(f"{where}: expected directory, found {m.group(1)}")
                elif kind == "d" and md5 != "-":
                    got = dir_digest(segments.get(f"ls -p {abspath}", ""))
                    if got != md5:
                        failures.append(
                            f"{where}: fsynced directory ENTRY SET changed "
                            f"(digest {got} != declared {md5}) — a promised "
                            f"dirent was lost or a phantom entry appeared"
                        )
    finally:
        for out in dumps.values():
            if os.path.exists(out):
                os.unlink(out)
        os.rmdir(dumpdir)

    tag = "final" if final else "check"
    if failures:
        print(f"ORACLE-RED ({tag}) on {img_path}:")
        for f in failures:
            print(f"  {f}")
        return 1
    print(
        f"oracle {tag}: {inforce_workloads} workloads in force, "
        f"{len(assertions)} assertions ok"
        + (f", {len(table.unavail)} UNAVAIL (skipped: {[w for w, _ in table.unavail]})"
           if table.unavail else "")
    )
    return 0


def main(argv):
    if len(argv) >= 4 and argv[1] == "collect":
        return collect(argv[2], argv[3])
    if len(argv) >= 4 and argv[1] == "check":
        return judge(argv[2], argv[3])
    if len(argv) >= 5 and argv[1] == "final":
        return judge(argv[2], argv[4], final=True, expected_workloads=int(argv[3]))
    # judge.sh appends the scratch image path to the oracle argument vector,
    # so `oracle.py check <table>` from sweep.sh arrives here as check+2 args.
    print(
        "usage: oracle.py collect <qemu.log> <out.table>\n"
        "       oracle.py check <table> <img>\n"
        "       oracle.py final <table> <expected-workloads> <img>",
        file=sys.stderr,
    )
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
