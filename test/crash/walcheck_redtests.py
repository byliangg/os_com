#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0

"""Offline red-flip self-tests for walcheck.py (no QEMU, no matrix).

    walcheck_redtests.py <pristine.img> <log.img>

Runs constructive violation experiments against a REAL recorded log and
asserts the checker flips red exactly where it must and stays green where
acceptance is deliberate.  Experiments (numbered after the P8a-H2 review
findings they lock in):

  1a  tail garbage sb write, NO clean-journal marker  -> RED
      (power-off logs have no clean-unmount window)
  1b  synthesized clean marker (jsb s_start==0 after the last commit)
      + a legal unmount sb write (RECOVER cleared, csum updated) -> GREEN
      (the window still exists, gated on the marker)
  1c  synthesized clean marker + garbage sb write     -> RED
      (the window is content-checked even when open)
  2   garbage sb write BEFORE the first commit        -> RED
      (the RECOVER-stamp window is content-checked; the real stamp in
      the same log stays whitelisted -- checked by the baseline run)
  3a  jsb s_sequence advanced past a block's newest txn, then an OLDER
      committed image of that block landed             -> RED (SUPERSEDED)
  3b  same tail advance, the NEWEST image landed       -> GREEN (idempotent)
  3c  no tail advance, an older image landed           -> GREEN
      (lagging checkpoint stays legal while replay can repair it)
  5   tag_size(): jbd2 journal_tag_bytes() parity incl. the csum_v2
      +2 quirk (14/10), and a multi-tag csum_v2 descriptor walks at the
      v2 stride

3a-3c need a judged block with images in >= 2 transactions in the log;
they SKIP (with a note) when the log has none.  Exit 0 = every runnable
experiment behaved; 1 = at least one did not.
"""

import os
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import walcheck as W  # noqa: E402


def spans_of(violations, blk):
    return [v for v in violations if v[1] == blk]


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__.split("\n")[2].strip())
    img, log = sys.argv[1], sys.argv[2]

    logf, ss, entries = W.read_log(log)
    bs, meta, jblocks, jsb, sb0 = W.fs_geometry(img)
    chk = W.Checker(logf, ss, bs, meta, jblocks, jsb, sb0)

    failures = []

    def expect(name, cond, detail):
        tag = "PASS" if cond else "FAIL"
        print("redtest %-3s %s  %s" % (name, tag, detail))
        if not cond:
            failures.append(name)

    # ---- baseline must be green (and gives us commits / images) ----------
    v0, s0, r0 = chk.check(entries)
    expect(
        "0",
        not v0,
        "baseline green (%d matched, %d whitelisted)"
        % (s0["matched"], s0["whitelisted"]),
    )
    if v0:
        sys.exit("redtests: baseline log is red; nothing to prove on top")
    committed = {b: list(lst) for b, lst in chk.committed.items()}
    commits = chk.prepass_commits(entries)
    last_seq = max(lst[-1][2] for lst in committed.values())

    sb_blk = 1024 // bs
    sb_sector = 1024 // ss
    sb_nsec = 1024 // ss

    def sb_entry(payload, idx):
        return W.Entry(idx, sb_sector, sb_nsec, 0, 0, bytes(payload))

    garbage = sb_entry(b"\x5a" * 1024, 91000)

    # The real mount-stamp payload (first judged sb write of the session):
    # the accepted head-window state, base for a legal unmount write.
    stamp = None
    for e in entries:
        if e.sector == sb_sector and e.nr_sectors == sb_nsec:
            f = chk.logf
            f.seek(e.data_off)
            stamp = bytearray(f.read(1024))
            break
    assert stamp is not None, "no 1 KiB primary-sb write in this log?"

    # A real logged jsb write, template for synthesized markers/advances.
    jsb_tpl = None
    jsb_entry = None
    for e in entries:
        for blk, off, buf in chk.chunks(e):
            if chk.jmap.get(blk) == 0 and off == 0 and len(buf) >= 44:
                jsb_tpl = bytearray(buf)
                jsb_entry = e
                break
        if jsb_tpl is not None:
            break
    assert jsb_tpl is not None, "no journal-superblock write in this log?"

    def jsb_write(sequence, start, idx):
        p = bytearray(jsb_tpl)
        struct.pack_into(">I", p, 24, sequence)  # s_sequence
        struct.pack_into(">I", p, 28, start)  # s_start
        return W.Entry(
            idx, jsb_entry.sector, jsb_entry.nr_sectors, 0, 0, bytes(p)
        )

    # ---- 1a: tail garbage, no marker -> RED -------------------------------
    v, s, r = chk.check(entries + [garbage])
    hits = spans_of(v, sb_blk)
    expect(
        "1a",
        bool(hits) and any("clean-journal marker" in h[3] for h in hits),
        "tail garbage sb write w/o clean marker flagged: %s"
        % (hits[0][3][:120] if hits else "NOT FLAGGED"),
    )

    # ---- 1b: marker + legal unmount sb write -> GREEN ----------------------
    marker = jsb_write(last_seq + 1, 0, 92000)
    legal = bytearray(stamp)
    legal[0x60] &= ~0x04  # clear INCOMPAT_RECOVER (the unmount write)
    for i in range(0x3FC, 0x400):
        legal[i] ^= 0xFF  # csum recomputed: any value is allowed
    v, s, r = chk.check(entries + [marker, sb_entry(legal, 92001)])
    wl = [x for x in r if x[0] == 92001 and x[3] == "whitelisted"]
    expect(
        "1b",
        not v and bool(wl),
        "legal unmount sb write after clean marker whitelisted: %s"
        % (wl[0][4][:110] if wl else "NOT WHITELISTED / red"),
    )

    # ---- 1c: marker + garbage -> RED (content check) -----------------------
    v, s, r = chk.check(entries + [marker, sb_entry(b"\x5a" * 1024, 92002)])
    hits = [x for x in v if x[0] == 92002 and "REJECTED" in x[3]]
    expect(
        "1c",
        bool(hits),
        "tail garbage sb write behind clean marker REJECTED: %s"
        % (hits[0][3][:120] if hits else "NOT FLAGGED"),
    )

    # ---- 2: head garbage -> RED (content check) ----------------------------
    # Insert right after the real mount stamp, still before the first
    # commit; the real stamp itself stayed whitelisted in the baseline.
    head_pos = min(commits) if commits else len(entries)
    injected = entries[:head_pos] + [garbage] + entries[head_pos:]
    v, s, r = chk.check(injected)
    hits = [
        x
        for x in v
        if x[0] == garbage.idx
        and "pre-first-commit" in x[3]
        and "REJECTED" in x[3]
    ]
    expect(
        "2",
        bool(hits),
        "head garbage sb write REJECTED by field whitelist: %s"
        % (hits[0][3][:120] if hits else "NOT FLAGGED"),
    )

    # ---- 3: stale-image retirement -----------------------------------------
    multi = None
    for blk, lst in sorted(committed.items()):
        seqs = sorted({t[2] for t in lst})
        if len(seqs) >= 2 and meta.get(blk) != "sb-primary":
            multi = (blk, lst)
            break
    if multi is None:
        for blk, lst in sorted(committed.items()):
            if len({t[2] for t in lst}) >= 2:
                multi = (blk, lst)
                break
    if multi is None:
        print(
            "redtest 3   SKIP  no judged block with images in >= 2 txns in "
            "this log (single-transaction recording)"
        )
    else:
        blk, lst = multi
        old_img = lst[0][3]
        new_img = lst[-1][3]
        newest = lst[-1][2]
        label = meta[blk]

        def ckpt(image, idx):
            return W.Entry(
                idx, blk * bs // ss, bs // ss, 0, 0, bytes(image)
            )

        # 3a: advance the tail past the newest txn of blk, land the OLD image
        adv = jsb_write(newest + 1, jsb["first"], 93000)
        v, s, r = chk.check(entries + [adv, ckpt(old_img, 93001)])
        hits = [x for x in v if x[0] == 93001 and "SUPERSEDED" in x[3]]
        expect(
            "3a",
            bool(hits) and s["retired"] > 0,
            "stale image of fs block %d (%s) after tail advance flagged: %s"
            % (blk, label, hits[0][3][:130] if hits else "NOT FLAGGED"),
        )
        # 3b: same advance, land the NEWEST image -> green (idempotent)
        v, s, r = chk.check(entries + [adv, ckpt(new_img, 93002)])
        ok = not v and any(
            x[0] == 93002 and x[3] == "matched" for x in r
        )
        expect(
            "3b",
            ok,
            "newest image of fs block %d after tail advance still matched"
            % blk,
        )
        # 3c: NO advance, land the old image -> green (lagging checkpoint)
        v, s, r = chk.check(entries + [ckpt(old_img, 93003)])
        ok = not v and any(
            x[0] == 93003 and x[3] == "matched" for x in r
        )
        expect(
            "3c",
            ok,
            "older image of fs block %d w/o tail advance still matched "
            "(lagging-checkpoint acceptance preserved)" % blk,
        )

    # ---- 5: tag stride parity with jbd2 journal_tag_bytes() ----------------
    ok = (
        W.tag_size(0) == 8
        and W.tag_size(W.JBD2_INCOMPAT_64BIT) == 12
        and W.tag_size(W.JBD2_INCOMPAT_CSUM_V2) == 10
        and W.tag_size(W.JBD2_INCOMPAT_CSUM_V2 | W.JBD2_INCOMPAT_64BIT) == 14
        and W.tag_size(W.JBD2_INCOMPAT_CSUM_V3) == 16
        and W.tag_size(W.JBD2_INCOMPAT_CSUM_V3 | W.JBD2_INCOMPAT_64BIT) == 16
    )
    expect("5", ok, "tag_size() == journal_tag_bytes() for all 6 layouts")
    # a two-tag csum_v2 descriptor must walk at the v2 stride (10/14);
    # the classic stride (8/12) would misread the second tag's blocknr
    for inc, stride, fmt64 in (
        (W.JBD2_INCOMPAT_CSUM_V2, 10, False),
        (W.JBD2_INCOMPAT_CSUM_V2 | W.JBD2_INCOMPAT_64BIT, 14, True),
    ):
        blkbuf = bytearray(bs)
        struct.pack_into(">III", blkbuf, 0, W.JBD2_MAGIC, W.BT_DESCRIPTOR, 7)
        off = 12
        for i, nr in enumerate((1111, 2222)):
            flags = W.TAG_FLAG_SAME_UUID | (
                W.TAG_FLAG_LAST_TAG if i == 1 else 0
            )
            struct.pack_into(">IHH", blkbuf, off, nr, 0, flags)
            if fmt64:
                struct.pack_into(">I", blkbuf, off + 8, 0)
            off += stride
        tags = W.parse_descriptor_tags(bytes(blkbuf), inc, bs)
        expect(
            "5s%d" % stride,
            [t[0] for t in tags] == [1111, 2222],
            "two-tag csum_v2 descriptor (stride %d) parsed as %s"
            % (stride, [t[0] for t in tags]),
        )

    logf.close()
    if failures:
        print("redtests: FAILED: %s" % ", ".join(failures))
        sys.exit(1)
    print("redtests: all runnable experiments behaved")
    sys.exit(0)


if __name__ == "__main__":
    main()
