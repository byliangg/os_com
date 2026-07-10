#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Converts a CrashMonkey/ACE J-lang workload into a POSIX sh script.

The output runs inside the guest with CWD = a fresh directory on the
recorded test disk, using only tools present in the xfstests conformance
runtime (xfs_io, coreutils). Ops outside the supported subset (mmapwrite,
xattrs, dwrite, fzero-mode falloc) make the converter fail loudly — filter
the corpus first (run_matrix.sh does).

The ACE name table is parsed from the workload's own `# define` block:
ACE flattens paths in op arguments (`Afoo` means `A/foo`), so each declared
path maps from its slash-stripped form; the root `test` is the script's CWD.
For the standard seq-1 corpus this reproduces the historical hardcoded table
exactly (checked byte-for-byte against the pre-generalization converter).

Two ops beyond ACE's J-lang are accepted for the hand-written protocol
corpus (test/crash/protocol/):

  sleep N      emits a plain `sleep N` — dwells past the journal's 5s age
               trigger so age-driven commits get crash coverage. No oracle
               state changes (nothing is modified or persisted by sleeping).
  symlink T L  emits `ln -s` (the link may dangle; a symlink is a name plus
               a target blob, and the crash surface — dirent + inode — does
               not need the target to resolve). The symlink's own content is
               never DECLARED (fsync of a symlink path is refused loudly and
               `sync` skips them): its existence is still asserted through
               the parent directory's entry-set digest, which is what the
               known-bug sequence exercising symlinks (generic_348) fsyncs.

--oracle mode (test/crash/oracle.py, the crash-durability data oracle)
additionally translates `checkpoint N` instead of dropping it:

  1. An on-disk MARKER file `ckpt_N` (CrashMonkey CmCheckpoint's moral
     equivalent) whose single-block content is a unique magic record,
     written and fsynced at the checkpoint. Semantics: the marker bytes are
     first handed to the kernel only AFTER every previous persistence call
     (fsync/fdatasync/sync) has RETURNED, so any device-write of those bytes
     is submitted after the flushes those calls completed. Hence if a crash
     prefix of the write log contains the marker bytes, every persistence
     promise made before the checkpoint is binding in that prefix
     ("sufficient, not necessary": a prefix may satisfy the promises while
     lacking the marker — the oracle then simply skips it; conservative).
     The magic never appears contiguously in this generated script (it is
     emitted as adjacent-string shell concatenation), because the script
     itself is baked into the recorded disk and a literal would make every
     crash prefix grep-match the marker.

  2. Console DECLARATION lines (X4ORACLE|...) that record the expected
     state of every entity persisted so far. They are emitted immediately
     after each fsync/fdatasync/sync RETURNS — i.e. at the moment the
     durability promise is made — and tagged with the id of the NEXT
     checkpoint, whose marker's presence is what puts them in force.
     WHICH entities have been persisted is computed statically here
     (J-lang semantics are deterministic: `fsync X` persists X, `sync`
     persists everything alive); their content (size/md5) is captured at
     run time by the emitted shell. When an already-declared entity is
     modified again (write/falloc/truncate/unlink/rename/...), a
     revocation line (kind `x`) is emitted so the oracle stops asserting
     it — after an un-persisted modification the on-disk state may be old
     or new, so equality with the old snapshot must no longer be demanded.

     Directories are declared with a digest of their SORTED ENTRY SET
     (CrashMonkey's compare_entries_at_path: fsync(dir) promises exactly
     the directory's entries, the durability contract dirent-loss bugs
     break). The digest is md5 over the newline-terminated, bytewise-sorted
     entry names, excluding `.`/`..` and this instrumentation's own
     `ckpt_*` marker files (a marker created AFTER the declaration must
     not invalidate it; workload names starting with `ckpt_` are refused
     loudly below). Any un-persisted namespace op under a directory
     (create/mkdir/rmdir/unlink/rename/link) revokes the PARENT dir's
     standing declaration through the same revocation discipline — the
     entry set changed, so on-disk may be old or new. The nlink is still
     recorded for diagnostics but never compared.

     Every console line carries a per-workload sequence number and an
     md5 line checksum, and the script ends with an E(nd) line: serial
     consoles drop and garble lines (see experience.md §7), and a lost
     declaration must make the oracle explicitly mark the workload's
     oracle unavailable — never silently pass, never falsely fail.
"""

import hashlib
import os
import sys

# The marker magic, split so the generated script (which lives on the
# recorded disk) never contains the contiguous byte string.
MAGIC_PARTS = ("X4", "CKPT1!")
MAGIC = "".join(MAGIC_PARTS)


def marker_string(workload: str, ckpt: int) -> str:
    """The exact marker-file content for (workload, checkpoint).

    Deterministic so the host-side oracle can recompute it; the trailing
    md5 fragment guards against a partial/garbled on-disk match being
    taken for the real thing.
    """
    tail = f"{workload}|{ckpt}"
    h8 = hashlib.md5(tail.encode()).hexdigest()[:8]
    return f"{MAGIC}{tail}|{h8}"


def sh_marker_literal(workload: str, ckpt: int) -> str:
    """The marker as a shell word that never spells the magic contiguously."""
    tail = marker_string(workload, ckpt)[len(MAGIC):]
    parts = "".join(f"'{p}'" for p in MAGIC_PARTS)
    return f"{parts}'{tail}'"


def parent_dir(path: str) -> str:
    """The directory whose ENTRY SET a namespace op on `path` changes."""
    return path.rsplit("/", 1)[0] if "/" in path else "."


def parse_names(lines):
    """Builds the ACE name table from the `# define` block.

    Each declared path is keyed by its slash-stripped form (`A/foo` ->
    `Afoo`); the workload root `test` maps to `.` (the script's CWD).
    Path components starting with `ckpt_` are refused: directory entry-set
    digests exclude that prefix (reserved for the checkpoint marker files),
    so such a workload file would be invisible to the oracle.
    """
    names = {}
    in_define = False
    for raw in lines:
        line = raw.strip()
        if line.startswith("# define"):
            in_define = True
            continue
        if in_define:
            if line.startswith("#"):
                break
            if not line:
                continue
            if any(part.startswith("ckpt_") for part in line.split("/")):
                raise SystemExit(f"reserved ckpt_ prefix in ACE name: {line}")
            names[line.replace("/", "")] = "." if line == "test" else line
    if not names:
        raise SystemExit("no `# define` block: not a J-lang workload")
    return names


# The five ACE falloc modes, mapped exactly as crashmonkey's
# ace/xfstestAdapter.py maps them onto xfs_io. The fzero modes are
# fallocate(FALLOC_FL_ZERO_RANGE), which this kernel rejects with
# EOPNOTSUPP — converting them would make the guest run fail, so they
# stay a loud converter error (= counted skip in run_matrix.sh).
FALLOC_MODES = {
    "0": "falloc",
    "FALLOC_FL_KEEP_SIZE": "falloc -k",
    "FALLOC_FL_PUNCH_HOLE|FALLOC_FL_KEEP_SIZE": "fpunch",
}


def run_ops(lines):
    """Yields the (op, args) list of the `# run` section."""
    ops = []
    in_run = False
    for raw in lines:
        line = raw.strip()
        if line.startswith("# run"):
            in_run = True
            continue
        if not in_run or not line or line.startswith("#"):
            continue
        op, *args = line.split()
        ops.append((op, args))
    return ops


# Shell prolog for --oracle mode. WORKLOAD is substituted; X4S is the
# per-workload sequence number and every line's payload is checksummed so
# the collector can detect dropped/garbled console lines.
ORACLE_PROLOG = """\
X4S=0
x4emit() {{ # x4emit <payload-after-workload-field>
    X4S=$((X4S + 1))
    p="{w}|$X4S|$1"
    c=$(printf %s "$p" | md5sum)
    echo "X4ORACLE|$p|${{c%% *}}"
}}
x4d() {{ # x4d <ckpt> <path> — declare <path>'s just-persisted state
    if [ -d "$2" ]; then
        # Digest of the sorted entry set (see the converter docstring):
        # md5 over one name per line, bytewise order, our own ckpt_*
        # marker files excluded. Must mirror oracle.py's dir_digest().
        k=d; sz=$(stat -c %h "$2")
        m=$(ls -A "$2" | grep -v "^ckpt_" | LC_ALL=C sort | md5sum); m=${{m%% *}}
    elif [ -f "$2" ]; then k=f; sz=$(stat -c %s "$2"); m=$(md5sum < "$2"); m=${{m%% *}}
    else k=missing; sz=-; m=-; fi
    x4emit "D|$1|$k|$sz|$m|$2"
}}
x4x() {{ # x4x <ckpt> <path> — revoke: <path> modified after last declaration
    x4emit "D|$1|x|-|-|$2"
}}\
"""


class OracleState:
    """Static tracking of what is alive and what is assertably durable.

    `live` maps path -> kind for entities that exist right now (needed to
    expand `sync`, which persists everything). `durable` is the set of
    paths whose last persistence is at least as recent as their last
    modification — exactly the paths with a standing declaration that a
    later modification must revoke.
    """

    def __init__(self, workload, ckpt_ids):
        self.workload = workload
        # ckpt_ids in program order; decls between checkpoint i-1 and i are
        # tagged ckpt_ids[i]. Decls after the last checkpoint get a sentinel
        # the collector drops (no marker will ever put them in force).
        self.ckpt_ids = ckpt_ids
        self.next_ckpt_idx = 0
        self.live = {}
        self.durable = set()
        # Hard-link alias groups: path -> shared set of all names of the
        # same inode. A content modification through ANY name invalidates
        # the declared snapshot of EVERY name, so revocation fans out.
        self.aliases = {}
        self.out = []

    def next_ckpt(self):
        if self.next_ckpt_idx < len(self.ckpt_ids):
            return self.ckpt_ids[self.next_ckpt_idx]
        return (self.ckpt_ids[-1] + 1) if self.ckpt_ids else 1

    def ensure_file(self, path):
        if path not in self.live:
            self.live[path] = "f"
            self.aliases.setdefault(path, {path})
            # A new NAME appeared: the parent's declared entry set is stale.
            self.revoke_name(parent_dir(path))

    def mkdir(self, path):
        self.live[path] = "d"
        self.revoke_name(parent_dir(path))

    def symlink(self, path):
        # A new NAME in the parent; the symlink itself is tracked so that
        # persistence calls can refuse/skip it (see the docstring).
        self.live[path] = "l"
        self.revoke_name(parent_dir(path))

    def link(self, src, dst):
        group = self.aliases.setdefault(src, {src})
        group.add(dst)
        self.aliases[dst] = group
        self.live[dst] = "f"
        self.revoke_name(parent_dir(dst))

    def revoke_name(self, path):
        if path in self.durable:
            self.out.append(f'x4x {self.next_ckpt()} "{path}"')
            self.durable.discard(path)

    def modify(self, path):
        """A non-persisted CONTENT change: the inode's declared snapshot is
        stale under every hard-linked name, so revoke the whole alias group."""
        for p in sorted(self.aliases.get(path, {path})):
            self.revoke_name(p)

    def remove(self, path):
        # Only this NAME goes away; content seen through other hard links
        # is untouched (and file assertions never compare nlink). The
        # parent's entry set shrank, so its declaration falls too.
        self.revoke_name(path)
        self.revoke_name(parent_dir(path))
        self.live.pop(path, None)
        group = self.aliases.pop(path, None)
        if group is not None:
            group.discard(path)

    def rename(self, src, dst):
        # Neither the source name, the target name, nor anything beneath
        # them is assertable afterwards: the affected directory entries are
        # not persisted until the next fsync/sync. Content-preserving as the
        # rename is, every declared PATH under both prefixes must be revoked
        # (conservative; a later fsync/sync re-declares survivors). Both
        # parents' entry sets changed too.
        self.revoke_name(parent_dir(src))
        self.revoke_name(parent_dir(dst))
        for p in sorted(self.durable):
            if p == src or p == dst or p.startswith(src + "/") or p.startswith(dst + "/"):
                self.out.append(f'x4x {self.next_ckpt()} "{p}"')
                self.durable.discard(p)
        # An existing target is overwritten: that NAME's inode loses a link
        # (content under its other hard links, if any, is untouched).
        self.live.pop(dst, None)
        dst_group = self.aliases.pop(dst, None)
        if dst_group is not None:
            dst_group.discard(dst)
        moved = {}
        for p in list(self.live):
            if p == src or p.startswith(src + "/"):
                moved[dst + p[len(src):]] = self.live.pop(p)
        self.live.update(moved)
        for p in list(self.aliases):
            if p == src or p.startswith(src + "/"):
                new = dst + p[len(src):]
                group = self.aliases.pop(p)
                group.discard(p)
                group.add(new)
                self.aliases[new] = group

    def persist(self, path):
        """fsync/fdatasync returned for `path`: declare its runtime state."""
        if self.live.get(path) == "l":
            raise SystemExit(
                "fsync of a symlink is not instrumentable "
                f"(x4d cannot snapshot it): {path}"
            )
        self.out.append(f'x4d {self.next_ckpt()} "{path}"')
        self.durable.add(path)

    def persist_all(self):
        """sync returned: everything alive (and the root) is now durable.

        Symlinks are skipped (never declared): their target blob is not
        assertable through x4d, and their existence is already covered by
        the parent directory's entry-set digest.
        """
        for p in sorted(set(self.live) | {"."}):
            if self.live.get(p) == "l":
                continue
            self.persist(p)

    def checkpoint(self, ckpt):
        assert ckpt == self.ckpt_ids[self.next_ckpt_idx], "checkpoint order"
        self.out.append(f'x4emit "C|{ckpt}"')
        lit = sh_marker_literal(self.workload, ckpt)
        self.out.append(f"printf '%s\\n' {lit} > ckpt_{ckpt}")
        self.out.append(f'xfs_io -r -c "fsync" ckpt_{ckpt}')
        self.next_ckpt_idx += 1

    def end(self):
        self.out.append('x4emit "E"')


def convert(lines, workload=None, oracle=False):
    ops = run_ops(lines)
    names = parse_names(lines)

    def path(name):
        try:
            return names[name]
        except KeyError:
            raise SystemExit(f"unknown ACE name: {name}")

    ckpt_ids = []
    for op, args in ops:
        if op == "checkpoint":
            ckpt = int(args[0])
            if ckpt in ckpt_ids:
                raise SystemExit(f"duplicate checkpoint id: {ckpt}")
            ckpt_ids.append(ckpt)

    out = [
        "#!/bin/sh",
        "# Generated by jlang2sh.py — do not edit.",
        "set -eu",
    ]
    st = None
    if oracle:
        st = OracleState(workload, ckpt_ids)
        st.out = out
        out.append(ORACLE_PROLOG.format(w=workload))

    for op, args in ops:
        if op in ("open", "creat"):
            # Creation matters for the crash surface; fd bookkeeping does not
            # (persistence points are re-opened by name via xfs_io).
            out.append(f'[ -e "{path(args[0])}" ] || touch "{path(args[0])}"')
            if st is not None:
                st.ensure_file(path(args[0]))
        elif op == "opendir" or op == "close":
            pass
        elif op == "mkdir":
            out.append(f'mkdir -p "{path(args[0])}"')
            if st is not None:
                st.mkdir(path(args[0]))
        elif op == "rmdir":
            out.append(f'rmdir "{path(args[0])}"')
            if st is not None:
                st.remove(path(args[0]))
        elif op == "write":
            name, off, length = args[0], args[1], args[2]
            out.append(f'xfs_io -f -c "pwrite -S 0x22 {off} {length}" "{path(name)}"')
            if st is not None:
                st.ensure_file(path(name))
                st.modify(path(name))
        elif op == "falloc":
            name, mode, off, length = args[0], args[1], args[2], args[3]
            if mode not in FALLOC_MODES:
                raise SystemExit(f"unsupported falloc mode: {mode}")
            out.append(
                f'xfs_io -f -c "{FALLOC_MODES[mode]} {off} {length}" "{path(name)}"'
            )
            if st is not None:
                st.ensure_file(path(name))
                st.modify(path(name))
        elif op == "truncate":
            out.append(f'xfs_io -f -c "truncate {args[1]}" "{path(args[0])}"')
            if st is not None:
                st.ensure_file(path(args[0]))
                st.modify(path(args[0]))
        elif op == "link":
            out.append(f'ln "{path(args[0])}" "{path(args[1])}"')
            if st is not None:
                # A new name for an existing inode: the source's declared
                # size/content still stand; the target is not durable yet.
                # The two names now alias, so a later content modification
                # through either must revoke both.
                st.link(path(args[0]), path(args[1]))
        elif op in ("unlink", "remove"):
            out.append(f'rm -f "{path(args[0])}"')
            if st is not None:
                st.remove(path(args[0]))
        elif op == "rename":
            out.append(f'mv "{path(args[0])}" "{path(args[1])}"')
            if st is not None:
                st.rename(path(args[0]), path(args[1]))
        elif op == "fsync":
            out.append(f'xfs_io -r -c "fsync" "{path(args[0])}"')
            if st is not None:
                st.persist(path(args[0]))
        elif op == "fdatasync":
            out.append(f'xfs_io -r -c "fdatasync" "{path(args[0])}"')
            if st is not None:
                # fdatasync persists content and the size needed to read it;
                # on this journaled ext4 the creating transaction commits
                # with it, so existence is promised too (Linux behaves the
                # same; CrashMonkey's auto-checker asserts it likewise).
                st.persist(path(args[0]))
        elif op == "sync":
            out.append("sync")
            if st is not None:
                st.persist_all()
        elif op == "sleep":
            out.append(f"sleep {int(args[0])}")
        elif op == "symlink":
            out.append(f'ln -s "{path(args[0])}" "{path(args[1])}"')
            if st is not None:
                st.symlink(path(args[1]))
        elif op == "checkpoint":
            if st is not None:
                st.checkpoint(int(args[0]))
            else:
                out.append(f"# checkpoint {args[0]}")
        else:
            raise SystemExit(f"unsupported op: {op}")

    if st is not None:
        st.end()
    return "\n".join(out) + "\n"


if __name__ == "__main__":
    argv = sys.argv[1:]
    oracle = False
    if argv and argv[0] == "--oracle":
        oracle = True
        argv = argv[1:]
    if len(argv) != 1:
        raise SystemExit("usage: jlang2sh.py [--oracle] <j-lang-file>")
    workload = os.path.basename(argv[0])
    with open(argv[0]) as f:
        sys.stdout.write(convert(f.readlines(), workload=workload, oracle=oracle))
