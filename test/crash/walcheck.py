#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0

"""Offline WAL write-order checker for recorded crash-matrix runs.

    walcheck.py <geometry.img> <log.img> [--survey] [--inject-violation]
                [--require-matched N]

<geometry.img> is the pristine (or final) ext4 image of the recorded
session; <log.img> is the dm-log-writes-compatible log produced by the
QEMU blklogwrites filter (tools/qemu_args.sh BLKLOG=on, same input as
sweep.sh).  Only the *static* filesystem geometry is taken from the
image (superblock copies, GDT + reserved GDT, block/inode bitmaps,
inode tables, resize-inode blocks, journal extent) and that geometry is
fixed at mke2fs time, so the final image is as good as the pristine one
for deriving it.

Invariant checked (the WAL write-order law, A2_claims "op-journaling WAL
write ordering"): on a journaled session, every write that lands a
metadata block on its *final* position must be preceded in the recorded
write stream by the commit block of a transaction whose after-image of
that block byte-equals what is being written.  The journal area itself
is the WAL and is exempt (it is instead parsed to learn the committed
after-images).  This is checked at the recording layer, below the
kernel, so the kernel cannot fake it -- the write_link-without-handle,
sync-direct-write and ITB-bare-read-RMW bugs that historically slipped
past ktest would all have violated it.

Implementation tier: BYTE-MATCHING (not just time-skeleton).  A judged
write passes only if its content equals one of the after-images of an
already-committed transaction containing that block.  Matching an
EARLIER committed image (not only the latest) is deliberate -- a lagging
checkpoint may legally land an older committed image after a newer
commit, because recovery replays the newer transaction on top (same as
jbd2) -- but only while that repair is still possible: once a logged
journal-superblock write advances s_sequence past the newest committed
transaction containing block B, recovery can never rewrite B again, so
every OLDER after-image of B is retired, and a later final-position
write matching only a retired image is flagged as stale-checkpoint /
rollback.  The newest image of each block stays acceptable forever
(re-landing the block's final committed content is idempotent).

Judged set (statically known geometry only):
  - primary + backup superblocks, GDT + reserved GDT blocks,
  - block bitmaps, inode bitmaps, inode tables (so the historical
    ITB-bare-read class is fully covered),
  - resize-inode blocks (DIND; the kernel must never touch them).

NOT judged (honest gaps, deliberately "miss rather than false-alarm"):
  - directory blocks, extent-tree index blocks, symlink blocks: they
    live in dynamically allocated blocks that cannot be known from
    static geometry, and a freed metadata block may be legally
    reallocated as *data* and rewritten before its allocating
    transaction commits (ordered-mode data is flushed BEFORE the
    commit block), so a "was once metadata" heuristic would false-red.
  - file data blocks: ordered-mode data legally precedes the commit.
  - a checkpoint landing a stale-but-committed image is accepted only
    while a newer committed transaction containing that block is still
    replayable (retirement above); after the observed tail advance it
    is flagged.  A late RE-write of the NEWEST committed image is still
    accepted even after the tail passed its transaction (idempotent
    content), so "tail advanced before the checkpoint actually landed"
    is NOT judged here -- catching that would need on-disk content
    tracking at tail-advance time.
  - flush/FUA epochs are NOT modeled: a checkpoint write that follows
    its commit in stream order but shares its flush epoch (no barrier
    in between) still passes.  Stream order is all that is judged, so
    a lost-barrier regression needs a different probe.
  - the two sb windows below constrain WHICH fields may change, not
    their values (counter drift inside the field set passes here; the
    e2fsck prefix judge backstops values).
  - sessions starting from a dirty journal: mount-time replay writes
    final-position metadata from *pre-session* transactions which this
    parser has no images for.  The tool requires the session to start
    with a clean journal (true for run_matrix.sh recordings).
  - if the geometry image is the FINAL image of a session whose sb got
    checkpointed mid-session, the head-window diff baseline is stale
    and may false-red (never false-green); prefer the pristine image.

Whitelist (structural, narrow, and content-checked):
  - writes to the PRIMARY superblock block are legal WITHOUT a
    journaled after-image only inside two windows, and only if the
    byte-diff against the previous accepted sb state stays inside the
    direct-write field set below:
      * pre-first-commit: the mount-time INCOMPAT_RECOVER stamp is
        written before the journal is published (it must be: it is
        what forces replay if we crash mid-session), so it cannot be
        journaled by construction.
      * post-log-empty: opens only after the log has been OBSERVED to
        become clean -- a journal-superblock write with s_start == 0
        after the last commit of the session.  `Ext4::drop` empties
        the journal (flush_on_unmount: final commit + checkpoint +
        clean jsb rewrite) strictly BEFORE its direct sb write, so a
        genuine clean unmount always logs that marker first.  A
        power-off recording never contains it, so on crash-matrix
        logs this window is CLOSED and tail sb corruption is flagged.
    Direct-write field set: mirrors the ONLY legal direct sb writer in
    the kernel, `Ext4::sync_metadata`'s RMW splice (fs.rs), which
    patches exactly s_free_blocks_count_lo/hi, s_free_inodes_count,
    s_feature_incompat and s_checksum and preserves every other byte
    losslessly; s_feature_incompat is further constrained to the
    RECOVER bit.  Any other changed byte inside a window = violation.
    The report still prints the byte-diff of every whitelisted write
    so reviewers can audit what the windows are used for.
  - the journal area itself (including the journal superblock) is
    exempt: it IS the WAL.
  Any OTHER direct superblock write (mid-session) and any direct write
  to bitmaps/GDT/ITB/backup-sb at ANY time must byte-match a committed
  after-image or it is flagged.

Exit status: 0 = no violations (and, with --inject-violation, BOTH
injected violations WERE flagged); 1 = violations found (or an injected
violation was NOT flagged); 2 = usage/parse errors; 3 = green but
vacuous under --require-matched N (fewer than N judged writes matched a
committed after-image, i.e. the byte-matching tier never ran for real).
"""

import argparse
import re
import struct
import subprocess
import sys

# ---- dm-log-writes / QEMU blklogwrites on-disk format ----------------------

WRITE_LOG_MAGIC = 0x6A736677736872  # QEMU block/blklogwrites.c
LOG_FLUSH_FLAG = 1 << 0
LOG_FUA_FLAG = 1 << 1
LOG_DISCARD_FLAG = 1 << 2
LOG_MARK_FLAG = 1 << 3

# ---- JBD2 on-disk format (all big-endian) ----------------------------------

JBD2_MAGIC = 0xC03B3998
JBD2_MAGIC_BYTES = b"\xc0\x3b\x39\x98"
BT_DESCRIPTOR = 1
BT_COMMIT = 2
BT_SB_V1 = 3
BT_SB_V2 = 4
BT_REVOKE = 5
BT_NAMES = {1: "descriptor", 2: "commit", 3: "sb_v1", 4: "sb_v2", 5: "revoke"}

TAG_FLAG_ESCAPE = 1
TAG_FLAG_SAME_UUID = 2
TAG_FLAG_DELETED = 4
TAG_FLAG_LAST_TAG = 8

JBD2_INCOMPAT_64BIT = 0x02
JBD2_INCOMPAT_CSUM_V2 = 0x08
JBD2_INCOMPAT_CSUM_V3 = 0x10

# ---- ext4 superblock direct-write field set ---------------------------------
#
# The ONLY legal direct (un-journaled) writer of the primary superblock in
# the kernel is `Ext4::sync_metadata` (kernel/src/fs/fs_impls/ext4/fs.rs),
# reachable only while no journal is published: the mount-time RECOVER
# stamp and the post-drain clean-unmount write.  It is an RMW splice that
# patches exactly the fields below and preserves every other on-disk byte
# losslessly, so a whitelisted window write must byte-diff ONLY inside
# them (offsets are sb-relative, i.e. relative to absolute byte 1024).
# Verified against all four archived real recordings: the mount stamp
# diffs exactly {s_feature_incompat: RECOVER set, s_checksum}.
EXT4_INCOMPAT_RECOVER = 0x0004
SB_WINDOW_FIELDS = (
    (0x00C, 4, "s_free_blocks_count_lo"),
    (0x010, 4, "s_free_inodes_count"),
    (0x060, 4, "s_feature_incompat"),  # RECOVER bit only (validated)
    (0x158, 4, "s_free_blocks_count_hi"),
    (0x3FC, 4, "s_checksum"),
)


class Entry:
    __slots__ = ("idx", "sector", "nr_sectors", "flags", "data_off", "payload")

    def __init__(self, idx, sector, nr_sectors, flags, data_off, payload=None):
        self.idx = idx
        self.sector = sector
        self.nr_sectors = nr_sectors
        self.flags = flags
        self.data_off = data_off
        self.payload = payload  # inline data for synthesized entries


def read_log(path):
    """Parse the log superblock and every entry header (data read lazily)."""
    f = open(path, "rb")
    sup = f.read(28)
    magic, version, nr_entries, sectorsize = struct.unpack("<QQQI", sup)
    if magic != WRITE_LOG_MAGIC:
        sys.exit("walcheck: %s: bad log magic %#x" % (path, magic))
    if version != 1:
        sys.exit("walcheck: %s: unsupported log version %d" % (path, version))
    entries = []
    off = sectorsize  # entry 0 starts right after the super sector
    for i in range(nr_entries):
        f.seek(off)
        hdr = f.read(32)
        if len(hdr) < 32:
            sys.exit("walcheck: truncated log at entry %d" % i)
        sector, nr_sectors, flags, data_len = struct.unpack("<QQQQ", hdr)
        data_off = off + sectorsize  # header is padded to one log sector
        if flags & LOG_MARK_FLAG:
            payload = (data_len + sectorsize - 1) // sectorsize * sectorsize
        elif flags & LOG_DISCARD_FLAG:
            payload = 0
        else:
            payload = nr_sectors * sectorsize
        entries.append(Entry(i, sector, nr_sectors, flags, data_off))
        off = data_off + payload
    return f, sectorsize, entries


# ---- static geometry from the ext4 image -----------------------------------


def run_tool(argv):
    return subprocess.run(
        argv, check=True, capture_output=True, text=True
    ).stdout


def fs_geometry(img):
    head = run_tool(["dumpe2fs", "-h", img])
    bs = int(re.search(r"^Block size:\s+(\d+)", head, re.M).group(1))
    groups = run_tool(["dumpe2fs", img])
    meta = {}  # fs block -> label

    def add_range(lo, hi, label):
        for b in range(lo, hi + 1):
            meta.setdefault(b, label)

    for m in re.finditer(
        r"(Primary|Backup) superblock at (\d+)"
        r"(?:, Group descriptors at (\d+)-(\d+))?",
        groups,
    ):
        kind = "sb-primary" if m.group(1) == "Primary" else "sb-backup"
        meta.setdefault(int(m.group(2)), kind)
        if m.group(3):
            add_range(int(m.group(3)), int(m.group(4)), "gdt")
    for m in re.finditer(r"Reserved GDT blocks at (\d+)-(\d+)", groups):
        add_range(int(m.group(1)), int(m.group(2)), "gdt-reserved")
    for m in re.finditer(r"Block bitmap at (\d+)", groups):
        meta.setdefault(int(m.group(1)), "block-bitmap")
    for m in re.finditer(r"Inode bitmap at (\d+)", groups):
        meta.setdefault(int(m.group(1)), "inode-bitmap")
    for m in re.finditer(r"Inode table at (\d+)-(\d+)", groups):
        add_range(int(m.group(1)), int(m.group(2)), "inode-table")

    # The resize inode's own blocks (its DIND); the reserved GDT blocks it
    # points at are already in the set.  Kernel must never write these.
    try:
        rout = run_tool(["debugfs", "-R", "blocks <7>", img])
        for tok in rout.split():
            if tok.isdigit() and int(tok) != 0:
                meta.setdefault(int(tok), "resize-inode")
    except subprocess.CalledProcessError:
        pass

    # Journal extent: the WAL itself, exempt from final-position judging.
    jout = run_tool(["debugfs", "-R", "blocks <8>", img])
    jblocks = [int(t) for t in jout.split() if t.isdigit()]
    if not jblocks:
        sys.exit("walcheck: %s: no journal blocks (inode 8 empty)?" % img)
    for b in jblocks:
        meta.pop(b, None)

    # Journal superblock as it sits in the image (clean-journal baseline)
    # and the primary-superblock block content (baseline for the window
    # content check: the first whitelisted sb write is diffed against it).
    sb_blk = 1024 // bs  # fs block holding the primary sb (byte 1024)
    with open(img, "rb") as f:
        f.seek(sb_blk * bs)
        sb0 = f.read(bs)
        f.seek(jblocks[0] * bs)
        jsb = parse_journal_sb(f.read(bs))
    if jsb is None:
        sys.exit("walcheck: %s: journal block 0 is not a journal sb" % img)
    if jsb["start"] != 0:
        # Expected when the FINAL image of a crash-recording session is
        # used for geometry (the guest powers off without a clean unmount,
        # that is the point of the harness).  The checker still requires
        # the SESSION to have started on a clean journal (true for
        # run_matrix.sh: fresh mke2fs); if it did not, the mount-time
        # replay's direct metadata writes are FLAGGED, not silently passed
        # -- the failure mode is a false red, never a false green.
        print(
            "walcheck: note: geometry image journal is dirty (s_start=%d); "
            "assuming the recorded session began on a clean journal"
            % jsb["start"]
        )
    return bs, meta, jblocks, jsb, sb0


def parse_journal_sb(data):
    magic, btype, _seq = struct.unpack_from(">III", data, 0)
    if magic != JBD2_MAGIC or btype not in (BT_SB_V1, BT_SB_V2):
        return None
    blocksize, maxlen, first, sequence, start = struct.unpack_from(
        ">IIIII", data, 12
    )
    incompat = struct.unpack_from(">I", data, 40)[0]
    return {
        "blocksize": blocksize,
        "maxlen": maxlen,
        "first": first,
        "sequence": sequence,
        "start": start,
        "incompat": incompat,
    }


# ---- JBD2 descriptor parsing ------------------------------------------------


def tag_size(incompat):
    """Per-tag stride, mirroring jbd2 journal_tag_bytes() exactly.

    csum_v2 (without csum_v3) is the quirky one: the on-disk stride is
    sizeof(journal_block_tag_s) + 2 = 14 with 64BIT, and 14 - 4 = 10
    without (the trailing t_blocknr_high is dropped) -- NOT the classic
    12/8.  Getting this wrong desyncs the descriptor walk from the
    second tag on (Linux-written v2 journals, which the kernel
    deliberately preserves instead of upgrading to v3).
    """
    if incompat & JBD2_INCOMPAT_CSUM_V3:
        return 16  # journal_block_tag3_s
    if incompat & JBD2_INCOMPAT_CSUM_V2:
        return 14 if incompat & JBD2_INCOMPAT_64BIT else 10
    return 12 if incompat & JBD2_INCOMPAT_64BIT else 8


def parse_descriptor_tags(data, incompat, bs):
    """Yield (fsblock, flags) per tag, jbd2-recovery style (do_one_pass)."""
    off = 12
    ts = tag_size(incompat)
    tags = []
    while off + ts <= bs:
        if incompat & JBD2_INCOMPAT_CSUM_V3:
            blocknr, flags, hi, _csum = struct.unpack_from(">IIII", data, off)
        else:
            blocknr, _csum, flags = struct.unpack_from(">IHH", data, off)
            hi = 0
            if incompat & JBD2_INCOMPAT_64BIT:
                hi = struct.unpack_from(">I", data, off + 8)[0]
        off += ts
        if not flags & TAG_FLAG_SAME_UUID:
            off += 16  # per-tag uuid follows unless SAME_UUID
        tags.append(((hi << 32) | blocknr, flags))
        if flags & TAG_FLAG_LAST_TAG:
            break
    return tags


# ---- the checker ------------------------------------------------------------


class Checker:
    def __init__(self, logf, sectorsize, bs, meta, jblocks, jsb, sb0):
        self.logf = logf
        self.ss = sectorsize
        self.bs = bs
        self.meta = meta
        self.jmap = {b: i for i, b in enumerate(jblocks)}
        self.maxlen = len(jblocks)
        self.jsb = jsb
        self.sb0 = sb0  # primary-sb block content from the geometry image

    def wrap(self, j):
        j += 1
        return self.jsb["first"] if j >= self.maxlen else j

    def read_data(self, e):
        if e.payload is not None:
            return e.payload
        self.logf.seek(e.data_off)
        return self.logf.read(e.nr_sectors * self.ss)

    def chunks(self, e):
        """Split a write entry into (fsblock, in-block offset, bytes)
        pieces.  Whole-block pieces are the common case; sub-block pieces
        happen for the 1 KiB ext4 superblock stamp and the 1 KiB journal
        superblock, which the kernel writes at their own granularity."""
        byte0 = e.sector * self.ss
        nbytes = e.nr_sectors * self.ss
        data = self.read_data(e)
        out = []
        pos = 0
        while pos < nbytes:
            blk, off = divmod(byte0 + pos, self.bs)
            take = min(self.bs - off, nbytes - pos)
            out.append((blk, off, data[pos : pos + take]))
            pos += take
        return out

    def prepass_commits(self, entries):
        """Stream POSITIONS (indices into `entries`) of commit-block writes.

        Positions, not `Entry.idx` values, so that injected / synthesized
        entry lists are judged by where a write sits in the stream -- the
        physical ordering is what the WAL law is about -- while `idx` stays
        a display name.  Header-cheap except for inline payloads.
        """
        commits = []
        for pos, e in enumerate(entries):
            if e.flags & (LOG_FLUSH_FLAG | LOG_MARK_FLAG) and e.nr_sectors == 0:
                continue
            if e.flags & LOG_DISCARD_FLAG or e.nr_sectors == 0:
                continue
            byte0 = e.sector * self.ss
            nbytes = e.nr_sectors * self.ss
            if byte0 % self.bs or nbytes % self.bs:
                continue
            base = byte0 // self.bs
            for k in range(nbytes // self.bs):
                j = self.jmap.get(base + k)
                if j is None or j == 0:
                    continue
                if e.payload is not None:
                    head = e.payload[k * self.bs : k * self.bs + 12]
                else:
                    self.logf.seek(e.data_off + k * self.bs)
                    head = self.logf.read(12)
                magic, btype, _ = struct.unpack(">III", head)
                if magic == JBD2_MAGIC and btype == BT_COMMIT:
                    commits.append(pos)
                    break
        return commits

    def check(self, entries, tag=""):
        bs = self.bs
        commits = self.prepass_commits(entries)  # stream positions
        first_commit = commits[0] if commits else None
        last_commit = commits[-1] if commits else None

        # Clean-journal marker: the first logged journal-superblock write
        # AFTER the last commit whose s_start == 0.  `Ext4::drop` empties
        # the log (flush_on_unmount: commit + checkpoint + clean jsb
        # rewrite) strictly BEFORE its direct sb write, so on a cleanly
        # unmounted recording this marker precedes the final sb write; a
        # power-off recording never logs it, and the post-log-empty sb
        # window below stays CLOSED (arbitrary tail sb corruption is then
        # judged like any mid-session write).
        clean_marker = None
        if last_commit is not None:
            for pos in range(last_commit + 1, len(entries)):
                e = entries[pos]
                if (
                    e.flags & (LOG_DISCARD_FLAG | LOG_MARK_FLAG)
                    or e.nr_sectors == 0
                ):
                    continue
                for blk, off, buf in self.chunks(e):
                    if (
                        self.jmap.get(blk) == 0
                        and off == 0
                        and len(buf) >= 44
                    ):
                        s = parse_journal_sb(buf)
                        if s is not None and s["start"] == 0:
                            clean_marker = pos
                            break
                if clean_marker is not None:
                    break

        # The primary superblock lives at absolute byte 1024; derive its
        # block and in-block offset instead of hardcoding +1024 (a 1 KiB
        # fs would put it at offset 0 of block 1).
        sb_blk = 1024 // bs
        sb_off = 1024 - sb_blk * bs

        jcontent = {}  # journal index -> latest 4K content this session
        committed = {}  # fsblock -> [(commit_pos, commit_idx, seq, image)]
        superseded = {}  # fsblock -> [(commit_idx, seq, image, at_idx, at_seq)]
        next_txn_start = self.jsb["first"]
        incompat = self.jsb["incompat"]
        prev_seq = None
        last_sb = self.sb0  # accepted primary-sb block state (image-seeded)
        violations = []
        records = []  # (entry idx, fsblock, label, status, note)
        stats = {
            "flush": 0,
            "fua": 0,
            "journal_blocks": 0,
            "unjudged_blocks": 0,
            "judged": 0,
            "matched": 0,
            "whitelisted": 0,
            "retired": 0,
            "txns": 0,
        }

        def viol(e_idx, blk, label, why):
            violations.append((e_idx, blk, label, why))
            records.append((e_idx, blk, label, "VIOLATION", why))

        def sb_diff(old, new):
            d = [o for o in range(len(new)) if old[o] != new[o]]
            if not d:
                return "content identical to previous sb state"
            spans = []
            lo = d[0]
            for a, b in zip(d, d[1:]):
                if b != a + 1:
                    spans.append((lo, a))
                    lo = b
            spans.append((lo, d[-1]))
            return "sb bytes changed at " + ", ".join(
                "%#x..%#x" % (a - sb_off, b - sb_off) for a, b in spans
            )

        def sb_window_audit(old, new):
            """Field-whitelists a window sb write.

            Returns None when the old->new byte diff stays inside the
            fields the kernel's only legal direct sb writer
            (`Ext4::sync_metadata`'s RMW splice, which preserves every
            unlisted byte losslessly) may patch, with s_feature_incompat
            further constrained to the RECOVER bit; otherwise a reason
            string naming the offending bytes (sb-relative; negative =
            before the sb inside its block, e.g. the boot area).
            """
            bad = []
            for o in range(len(new)):
                if old[o] == new[o]:
                    continue
                rel = o - sb_off
                if 0 <= rel < 1024 and any(
                    fo <= rel < fo + fl for fo, fl, _ in SB_WINDOW_FIELDS
                ):
                    continue
                bad.append(rel)
            if bad:
                spans = []
                lo = bad[0]
                for a, b in zip(bad, bad[1:]):
                    if b != a + 1:
                        spans.append((lo, a))
                        lo = b
                spans.append((lo, bad[-1]))
                return (
                    "bytes outside the direct-write field set changed at "
                    "sb-relative "
                    + ", ".join("%#x..%#x" % s for s in spans)
                )
            io = sb_off + 0x60
            old_inc = struct.unpack_from("<I", old, io)[0]
            new_inc = struct.unpack_from("<I", new, io)[0]
            if (old_inc ^ new_inc) & ~EXT4_INCOMPAT_RECOVER:
                return (
                    "s_feature_incompat changed beyond the RECOVER bit "
                    "(%#x -> %#x)" % (old_inc, new_inc)
                )
            return None

        def retire_superseded(at_idx, at_seq):
            # jbd2 tail semantics: recovery replays only transactions with
            # seq >= the on-disk journal superblock's s_sequence.  Once a
            # LOGGED jsb write advances s_sequence past the newest
            # committed transaction containing block B, no future replay
            # can rewrite B, so every OLDER after-image of B stops being a
            # legal checkpoint payload (landing one would be a permanent
            # rollback, not a lagging checkpoint).  Retire them; keep the
            # newest (re-landing the final content is idempotent).
            # Session-local sequence numbers are small and monotonic (the
            # commit-continuity check enforces the latter), so plain
            # integer comparison is enough here.
            for blk, lst in committed.items():
                newest = lst[-1][2]
                if newest >= at_seq or len(lst) == 1:
                    continue
                keep = [t for t in lst if t[2] == newest]
                drop = [t for t in lst if t[2] != newest]
                if not drop:
                    continue
                committed[blk] = keep
                sup = superseded.setdefault(blk, [])
                for _cpos, cidx, seq, img in drop:
                    sup.append((cidx, seq, img, at_idx, at_seq))
                stats["retired"] += len(drop)
                records.append(
                    (at_idx, blk, self.meta.get(blk, "journal"), "retired",
                     "%d stale after-image(s) older than txn %d retired "
                     "(jsb s_sequence -> %d: replay window closed)"
                     % (len(drop), newest, at_seq))
                )

        def process_commit(pos, e_idx, cj, seq):
            nonlocal next_txn_start, prev_seq
            stats["txns"] += 1
            if prev_seq is not None and seq != (prev_seq + 1) & 0xFFFFFFFF:
                viol(
                    e_idx,
                    None,
                    "journal",
                    "commit sequence %d does not follow %d" % (seq, prev_seq),
                )
            prev_seq = seq
            walk = next_txn_start
            images = []
            guard = 0
            while walk != cj:
                guard += 1
                if guard > self.maxlen * 2:
                    viol(e_idx, None, "journal", "runaway transaction walk")
                    break
                blk = jcontent.get(walk)
                if blk is None:
                    viol(
                        e_idx,
                        None,
                        "journal",
                        "txn %d: journal block %d never written this "
                        "session" % (seq, walk),
                    )
                    break
                magic, btype, bseq = struct.unpack_from(">III", blk, 0)
                if magic != JBD2_MAGIC or bseq != seq:
                    viol(
                        e_idx,
                        None,
                        "journal",
                        "txn %d: unexpected block at journal index %d "
                        "(magic %#x type %d seq %d)"
                        % (seq, walk, magic, btype, bseq),
                    )
                    break
                if btype == BT_DESCRIPTOR:
                    tags = parse_descriptor_tags(blk, incompat, bs)
                    walk = self.wrap(walk)
                    ok = True
                    for fsblock, flags in tags:
                        img = jcontent.get(walk)
                        if img is None:
                            viol(
                                e_idx,
                                None,
                                "journal",
                                "txn %d: missing after-image at journal "
                                "index %d for fs block %d"
                                % (seq, walk, fsblock),
                            )
                            ok = False
                            break
                        if flags & TAG_FLAG_ESCAPE:
                            img = JBD2_MAGIC_BYTES + img[4:]
                        images.append((fsblock, img))
                        walk = self.wrap(walk)
                    if not ok:
                        break
                elif btype == BT_REVOKE:
                    walk = self.wrap(walk)
                else:
                    viol(
                        e_idx,
                        None,
                        "journal",
                        "txn %d: unexpected %s block inside transaction "
                        "body at journal index %d"
                        % (seq, BT_NAMES.get(btype, btype), walk),
                    )
                    break
            next_txn_start = self.wrap(cj)
            for fsblock, img in images:
                if fsblock in self.meta:  # only judged blocks need images
                    committed.setdefault(fsblock, []).append(
                        (pos, e_idx, seq, img)
                    )

        def judge(pos, e_idx, blk, off, buf):
            nonlocal last_sb
            label = self.meta[blk]
            stats["judged"] += 1
            where = "" if off == 0 and len(buf) == bs else (
                " (sub-block %d+%d)" % (off, len(buf)))

            def accept_sb(content):
                nonlocal last_sb
                if label == "sb-primary":
                    base = bytearray(last_sb)
                    base[off : off + len(buf)] = buf
                    if content is not None:
                        base = bytearray(content)
                    last_sb = bytes(base)

            lst = committed.get(blk, ())
            for cpos, cidx, seq, img in reversed(lst):
                if img[off : off + len(buf)] == buf and cpos < pos:
                    stats["matched"] += 1
                    records.append(
                        (e_idx, blk, label, "matched",
                         "txn %d @ entry %d%s" % (seq, cidx, where))
                    )
                    accept_sb(img if off == 0 and len(buf) == bs else None)
                    return
            # structural whitelist: primary-sb writes in the two
            # direct-write windows, content-constrained to the
            # sync_metadata field set (module docstring has the argument)
            if label == "sb-primary":
                note = None
                if first_commit is None or pos < first_commit:
                    note = "pre-first-commit (mount RECOVER-stamp window)"
                elif clean_marker is not None and pos > clean_marker:
                    note = (
                        "post-log-empty (clean-unmount window; journal "
                        "emptied at entry #%d)" % entries[clean_marker].idx
                    )
                if note is not None:
                    cand = bytearray(last_sb)
                    cand[off : off + len(buf)] = buf
                    bad = sb_window_audit(last_sb, bytes(cand))
                    if bad is None:
                        stats["whitelisted"] += 1
                        old = last_sb
                        accept_sb(None)
                        records.append(
                            (e_idx, blk, label, "whitelisted",
                             note + where + "; " + sb_diff(old, last_sb))
                        )
                        return
                    viol(
                        e_idx, blk, label,
                        note + where + " REJECTED: " + bad,
                    )
                    return
            # stale-rollback tier: the content exists in the journal, but
            # only in an after-image whose replay window already closed
            for cidx, seq, img, at_idx, at_seq in superseded.get(blk, ()):
                if img[off : off + len(buf)] == buf:
                    viol(
                        e_idx, blk, label,
                        "content matches only a SUPERSEDED after-image "
                        "(txn %d @ entry %d) whose replay window closed "
                        "at the jsb write @ entry %d (s_sequence -> %d)%s "
                        "-- stale checkpoint / rollback"
                        % (seq, cidx, at_idx, at_seq, where),
                    )
                    return
            hint = ""
            if (
                label == "sb-primary"
                and last_commit is not None
                and pos > last_commit
                and clean_marker is None
            ):
                hint = (
                    " [post-last-commit sb write on a power-off log: the "
                    "clean-unmount window opens only after a logged "
                    "clean-journal marker (jsb s_start == 0), none seen]"
                )
            if lst:
                why = (
                    "content matches none of %d committed after-image(s) "
                    "of this block (latest txn %d committed at entry %d)%s "
                    "-- direct write or stale-RMW suspected%s"
                    % (len(lst), lst[-1][2], lst[-1][1], where, hint)
                )
            else:
                why = (
                    "write precedes any committed transaction containing "
                    "this block (%s)%s%s"
                    % (
                        "no commit seen yet"
                        if first_commit is None or pos < first_commit
                        else "block never journaled so far",
                        where,
                        hint,
                    )
                )
            viol(e_idx, blk, label, why)

        for pos, e in enumerate(entries):
            if e.flags & LOG_FLUSH_FLAG:
                stats["flush"] += 1
            if e.flags & LOG_FUA_FLAG:
                stats["fua"] += 1
            if e.flags & (LOG_DISCARD_FLAG | LOG_MARK_FLAG) or e.nr_sectors == 0:
                continue
            for blk, off, buf in self.chunks(e):
                whole = off == 0 and len(buf) == bs
                j = self.jmap.get(blk)
                if j is not None:
                    stats["journal_blocks"] += 1
                    if j == 0:
                        # journal superblock: 1 KiB structure, written at
                        # its own granularity.  Its feature words drive
                        # the tag layout of later descriptors, and its
                        # s_sequence drives stale-image retirement (a
                        # logged tail advance closes replay windows).
                        if off == 0 and len(buf) >= 44:
                            s = parse_journal_sb(buf)
                            if s:
                                incompat = s["incompat"]
                                retire_superseded(e.idx, s["sequence"])
                        continue
                    if not whole:
                        # log/descriptor/commit/after-image blocks are
                        # whole-block by construction; a partial write
                        # here would break WAL parseability
                        viol(e.idx, blk, "journal",
                             "partial (sub-block) write to journal block "
                             "(journal index %d)" % j)
                        continue
                    jcontent[j] = buf
                    magic, btype, seq = struct.unpack_from(">III", buf, 0)
                    if magic == JBD2_MAGIC and btype == BT_COMMIT:
                        process_commit(pos, e.idx, j, seq)
                elif blk in self.meta:
                    judge(pos, e.idx, blk, off, buf)
                else:
                    stats["unjudged_blocks"] += 1

        self.committed = committed  # kept for --inject-violation synthesis
        return violations, stats, records


# ---- driver -----------------------------------------------------------------


def report(name, violations, stats, records, survey):
    print(
        "walcheck[%s]: %d flush, %d fua, %d txn commits; "
        "%d journal-area block writes; %d judged metadata block writes "
        "(%d matched a committed after-image, %d whitelisted); "
        "%d stale after-image(s) retired by tail advance; "
        "%d unjudged block writes (data / dynamic metadata)"
        % (
            name,
            stats["flush"],
            stats["fua"],
            stats["txns"],
            stats["journal_blocks"],
            stats["judged"],
            stats["matched"],
            stats["whitelisted"],
            stats["retired"],
            stats["unjudged_blocks"],
        )
    )
    if stats["matched"] == 0 and not violations:
        print(
            "walcheck[%s]: note: no journal-backed final-position metadata "
            "write occurred -- the write-order law is (near-)vacuously "
            "satisfied for this log (lazy checkpoint + power-off without "
            "unmount legitimately produces this; a checkpoint-heavy "
            "recording, e.g. tiny-journal or remount, exercises it for "
            "real; a gate can demand a non-vacuous surface with "
            "--require-matched N, exit 3)"
            % name
        )
    if survey:
        for e_idx, blk, label, status, note in records:
            if status != "matched" or survey > 1:
                print(
                    "  entry #%-6s fs-block %-8s %-13s %-11s %s"
                    % (e_idx, blk, label, status, note)
                )
    for e_idx, blk, label, why in violations:
        print(
            "VIOLATION[%s]: log entry #%s, fs block %s (%s): %s"
            % (name, e_idx, blk, label, why)
        )
    return not violations


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("image", help="pristine or final ext4 image (geometry)")
    ap.add_argument("log", help="blklogwrites / dm-log-writes log")
    ap.add_argument(
        "--survey",
        action="count",
        default=0,
        help="print every non-matched judged write (twice: matched too)",
    )
    ap.add_argument(
        "--inject-violation",
        action="store_true",
        help="self-test both violation tiers: (A) reorder one "
        "legally-matched metadata write to before the first commit "
        "(write-order skeleton) and (B) corrupt one payload byte of a "
        "matched checkpoint write in place (byte-matching tier); require "
        "both to be flagged by name",
    )
    ap.add_argument(
        "--require-matched",
        type=int,
        default=0,
        metavar="N",
        help="exit 3 (instead of 0) when a green run byte-matched fewer "
        "than N judged writes -- lets a gate reject vacuous logs where "
        "the write-order law was never exercised for real",
    )
    args = ap.parse_args()

    logf, sectorsize, entries = read_log(args.log)
    bs, meta, jblocks, jsb, sb0 = fs_geometry(args.image)
    labels = {}
    for b, l in meta.items():
        labels[l] = labels.get(l, 0) + 1
    print(
        "walcheck: %d log entries (sector %d); fs block %d; judged set: %s; "
        "journal <8>: %d blocks (exempt); journal sb: first=%d seq=%d "
        "incompat=%#x (features re-read from logged jsb writes)"
        % (
            len(entries),
            sectorsize,
            bs,
            " ".join("%s=%d" % kv for kv in sorted(labels.items())),
            len(jblocks),
            jsb["first"],
            jsb["sequence"],
            jsb["incompat"],
        )
    )

    chk = Checker(logf, sectorsize, bs, meta, jblocks, jsb, sb0)
    violations, stats, records = chk.check(entries)
    green = report("real", violations, stats, records, args.survey)

    if not green:
        if args.inject_violation:
            print(
                "walcheck: real log already red; fix that before "
                "self-testing"
            )
        sys.exit(1)
    if stats["matched"] < args.require_matched:
        print(
            "walcheck: FAIL(vacuous): only %d judged write(s) matched a "
            "committed after-image, --require-matched %d -- this log never "
            "exercised the byte-matching tier"
            % (stats["matched"], args.require_matched)
        )
        sys.exit(3)
    if not args.inject_violation:
        sys.exit(0)

    commits = chk.prepass_commits(entries)  # stream positions
    if not commits:
        print("walcheck: log has no commits at all; cannot self-test")
        sys.exit(1)
    fc_pos = commits[0]
    fc_idx = entries[fc_pos].idx
    idx2pos = {e.idx: p for p, e in enumerate(entries)}

    # ---- mode A: write-order skeleton -------------------------------------
    # Preferred: pick the last judged write that legitimately matched a
    # commit and is not eligible for the sb whitelist, then move it in
    # front of the first commit-block write: the checker must turn red
    # and name it.
    target = None
    for e_idx, blk, label, status, note in reversed(records):
        if status == "matched" and label != "sb-primary":
            target = (e_idx, blk, label)
            break
    if target is not None:
        t_idx, t_blk, t_label = target
        t_pos = idx2pos[t_idx]
        moved = entries[t_pos]
        # A matched write always sits after its commit, hence after the
        # first commit: removing it does not shift `fc_pos`.
        rest = entries[:t_pos] + entries[t_pos + 1 :]
        injected = rest[:fc_pos] + [moved] + rest[fc_pos:]
        print(
            "walcheck: inject(A): moving entry #%d (fs block %d, %s) to "
            "before the first commit write (entry #%d)"
            % (t_idx, t_blk, t_label, fc_idx)
        )
    else:
        # Vacuous log (lazy checkpoint: no final-position write to move).
        # Synthesize a checkpoint write from a real committed after-image
        # of a judged block, prove it GREEN when appended at the end
        # (write-after-commit), then prove it RED when placed before the
        # first commit (write-before-commit).
        t_blk = t_img = None
        for blk, lst in chk.committed.items():
            if chk.meta.get(blk) != "sb-primary":
                t_blk, t_img = blk, lst[-1][3]
                if chk.meta[blk] in ("block-bitmap", "inode-table"):
                    break  # prefer an interesting label; any would do
        if t_blk is None:
            print(
                "walcheck: no committed after-image of a judged block; "
                "cannot self-test"
            )
            sys.exit(1)
        t_idx, t_label = len(entries), chk.meta[t_blk]  # one past the end
        bs = chk.bs
        synth = Entry(
            t_idx, t_blk * bs // sectorsize, bs // sectorsize, 0, 0, t_img
        )
        print(
            "walcheck: inject(A): log has no real final-position write to "
            "move; synthesizing a checkpoint write of fs block %d (%s) "
            "from txn after-image, as entry #%d" % (t_blk, t_label, t_idx)
        )
        v_ok, s_ok, r_ok = chk.check(entries + [synth])
        good = not v_ok and any(
            r[0] == t_idx and r[3] == "matched" for r in r_ok
        )
        print(
            "walcheck: inject(A): appended after all commits -> %s"
            % ("matched (green), as required" if good else "NOT matched")
        )
        if not good:
            print("walcheck: self-test FAIL: legal synthetic checkpoint "
                  "write did not judge green")
            sys.exit(1)
        injected = entries[:fc_pos] + [synth] + entries[fc_pos:]
        print(
            "walcheck: inject(A): now moving it to before the first commit "
            "write (entry #%d)" % fc_idx
        )
    v_a, s_a, r_a = chk.check(injected)
    hit = [v for v in v_a if v[0] == t_idx and v[1] == t_blk]
    report("inject-order", v_a, s_a, r_a, 0)
    if not hit:
        print(
            "walcheck: self-test FAIL: injected out-of-order write was "
            "NOT flagged"
        )
        sys.exit(1)
    print(
        "walcheck: self-test PASS(A): out-of-order write of entry #%s "
        "(fs block %d) was flagged" % (t_idx, t_blk)
    )

    # ---- mode B: byte-matching tier ----------------------------------------
    # Corrupt ONE payload byte of a checkpoint write that legitimately
    # matched, keeping its stream position: the checker must turn red in
    # the content-comparison branch ("matches none of ..."), proving the
    # byte-equality tier itself -- mode A only proves the time skeleton
    # (it is judged with an EMPTY candidate list, so a broken comparator
    # would keep mode A green while every direct-write/stale-RMW bug
    # sailed through).
    if target is not None:
        t2_idx, t2_blk, t2_label = target
        e0 = entries[idx2pos[t2_idx]]
        data = bytearray(chk.read_data(e0))
        p0 = None
        for cblk, coff, cbuf in chk.chunks(e0):
            if cblk == t2_blk:
                p0 = (cblk * chk.bs + coff) - e0.sector * sectorsize
                data[p0 + len(cbuf) // 2] ^= 0x01
                break
        if p0 is None:
            print("walcheck: self-test FAIL: cannot locate the matched "
                  "block inside its log entry")
            sys.exit(1)
        flipped = Entry(
            e0.idx, e0.sector, e0.nr_sectors, e0.flags, 0, bytes(data)
        )
        injected2 = [flipped if e is e0 else e for e in entries]
        print(
            "walcheck: inject(B): flipping one payload byte of matched "
            "entry #%d (fs block %d, %s) in place"
            % (t2_idx, t2_blk, t2_label)
        )
    else:
        # Vacuous log: append a checkpoint write whose payload is the
        # newest committed after-image with one byte flipped -- it lands
        # after its commit (legal position), so only the byte comparison
        # can reject it.
        chk.check(entries)  # repopulate committed{} from the real stream
        t2_blk = t2_img = None
        for blk, lst in chk.committed.items():
            if chk.meta.get(blk) != "sb-primary":
                t2_blk, t2_img = blk, lst[-1][3]
                if chk.meta[blk] in ("block-bitmap", "inode-table"):
                    break
        if t2_blk is None:
            print(
                "walcheck: no committed after-image of a judged block; "
                "cannot self-test the byte tier"
            )
            sys.exit(1)
        t2_idx, t2_label = len(entries) + 1, chk.meta[t2_blk]
        img2 = bytearray(t2_img)
        img2[len(img2) // 2] ^= 0x01
        flipped = Entry(
            t2_idx,
            t2_blk * chk.bs // sectorsize,
            chk.bs // sectorsize,
            0,
            0,
            bytes(img2),
        )
        injected2 = entries + [flipped]
        print(
            "walcheck: inject(B): no real matched write to corrupt; "
            "synthesizing a checkpoint write of fs block %d (%s) with one "
            "byte flipped, appended after all commits as entry #%d"
            % (t2_blk, t2_label, t2_idx)
        )
    v_b, s_b, r_b = chk.check(injected2)
    hit2 = [
        v
        for v in v_b
        if v[0] == t2_idx and v[1] == t2_blk and "matches none of" in v[3]
    ]
    report("inject-flip", v_b, s_b, r_b, 0)
    if not hit2:
        print(
            "walcheck: self-test FAIL: in-place byte corruption of a "
            "matched checkpoint write was NOT flagged by the "
            "byte-matching tier"
        )
        sys.exit(1)
    print(
        "walcheck: self-test PASS(B): corrupted checkpoint write of fs "
        "block %d (entry #%s) was flagged by byte-mismatch" % (t2_blk, t2_idx)
    )
    sys.exit(0)


if __name__ == "__main__":
    main()
