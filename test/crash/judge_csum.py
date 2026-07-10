#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Independent metadata_csum judge: recompute checksums straight from the
raw image bytes and compare with what is stored on disk.

Zero e2fsprogs involvement — the parser, the crc32c and every coverage
formula are implemented here from the Linux 6.6 definitions, so this judge
cannot share a blind spot with e2fsck (experience.md §12.3: a wrong
dir-tail coverage formula once survived a whole e2fsck-judged crash
matrix). crc32c = reflected poly 0x82F63B78, no inversion (__crc32c_le);
fs_seed = crc32c(~0, s_uuid); inode_seed = crc32c(crc32c(fs_seed, le32
ino), le32 generation).

Checked objects (per-object MATCH/MISMATCH on stdout):
  - superblock checksum (crc over sb[0..1020], seed ~0)
  - every group descriptor (crc(fs_seed, le32 group) over desc with the
    16-bit csum field zeroed)
  - block/inode bitmap checksums (full-bitmap crc(fs_seed, ...); lo 16
    bits, plus hi 16 when the 64-byte descriptor stores them; skipped
    for BLOCK_UNINIT/INODE_UNINIT groups whose bitmaps are synthetic)
  - a sample of in-use inodes (whole struct with both csum fields zeroed)
  - dirent tails of sampled directories (crc over block[..bs-12], i.e.
    the entire 12-byte tail excluded — the exact P6b bug shape)
  - extent-tree node tails of sampled inodes with depth >= 1 (crc over
    node[..12*(1+eh_max)])

Exit 0 = everything sampled matches (or the image has no metadata_csum —
reported as SKIP so the judge can be wired unconditionally); 1 = at least
one MISMATCH; 2 = unusable/unsupported image.

Usage: judge_csum.py [--inodes N] [--dirs N] [--ino N ...] [--quiet] <image>

The image is read as-is (never written). Run it on a journal-clean image:
either judge.sh's post-replay scratch copy (wire it as the oracle hook,
the image lands in the last positional argument) or a cleanly unmounted
filesystem. Boundaries: htree (INDEX_FL), inline-data and non-extent
inodes are reported as SKIP; bigalloc/meta_bg/csum_seed images are
refused; journal blocks are not checked (JBD2 csums are a different
oracle).
"""

import argparse
import struct
import sys

# ---------------------------------------------------------------- crc32c

_POLY = 0x82F63B78
_TBL = []
for _i in range(256):
    _c = _i
    for _ in range(8):
        _c = (_c >> 1) ^ _POLY if _c & 1 else _c >> 1
    _TBL.append(_c)


def crc32c(seed: int, data: bytes) -> int:
    c = seed & 0xFFFFFFFF
    for b in data:
        c = (c >> 8) ^ _TBL[(c ^ b) & 0xFF]
    return c


# ------------------------------------------------------------- constants

EXT4_MAGIC = 0xEF53
RO_GDT_CSUM = 0x10
RO_BIGALLOC = 0x200
RO_METADATA_CSUM = 0x400
INCOMPAT_META_BG = 0x10
INCOMPAT_64BIT = 0x80
INCOMPAT_CSUM_SEED = 0x2000
BG_INODE_UNINIT = 0x1
BG_BLOCK_UNINIT = 0x2
FL_INDEX = 0x00001000
FL_EXTENTS = 0x00080000
FL_INLINE_DATA = 0x10000000
EH_MAGIC = 0xF30A
DIR_TAIL_LEN = 12
S_IFMT = 0xF000
S_IFDIR = 0x4000


class Unsupported(Exception):
    pass


# --------------------------------------------------------------- parsing


class Fs:
    """Raw-parsed geometry of one ext4 image."""

    def __init__(self, path):
        self.f = open(path, "rb")
        sb = self.read_abs(1024, 1024)
        if struct.unpack_from("<H", sb, 0x38)[0] != EXT4_MAGIC:
            raise Unsupported("bad superblock magic")
        self.sb = sb
        self.inodes_count = struct.unpack_from("<I", sb, 0x00)[0]
        blocks_lo = struct.unpack_from("<I", sb, 0x04)[0]
        self.first_data_block = struct.unpack_from("<I", sb, 0x14)[0]
        self.block_size = 1024 << struct.unpack_from("<I", sb, 0x18)[0]
        self.blocks_per_group = struct.unpack_from("<I", sb, 0x20)[0]
        self.inodes_per_group = struct.unpack_from("<I", sb, 0x28)[0]
        self.first_ino = struct.unpack_from("<I", sb, 0x54)[0]
        self.inode_size = struct.unpack_from("<H", sb, 0x58)[0]
        self.compat = struct.unpack_from("<I", sb, 0x5C)[0]
        self.incompat = struct.unpack_from("<I", sb, 0x60)[0]
        self.ro_compat = struct.unpack_from("<I", sb, 0x64)[0]
        self.uuid = sb[0x68:0x78]
        self.desc_size = struct.unpack_from("<H", sb, 0xFE)[0]
        blocks_hi = struct.unpack_from("<I", sb, 0x150)[0]

        self.has_meta_csum = bool(self.ro_compat & RO_METADATA_CSUM)
        self.is_64bit = bool(self.incompat & INCOMPAT_64BIT)
        if not self.is_64bit:
            blocks_hi = 0
        if self.desc_size == 0 or not self.is_64bit:
            self.desc_size = 32
        self.blocks_count = blocks_lo | (blocks_hi << 32)
        self.groups = (
            self.blocks_count - self.first_data_block + self.blocks_per_group - 1
        ) // self.blocks_per_group

        if self.ro_compat & RO_BIGALLOC:
            raise Unsupported("bigalloc")
        if self.incompat & INCOMPAT_META_BG:
            raise Unsupported("meta_bg")
        if self.incompat & INCOMPAT_CSUM_SEED:
            raise Unsupported("csum_seed feature (seed != crc32c(~0, uuid))")

        self.fs_seed = crc32c(0xFFFFFFFF, self.uuid)
        self.descs = self._read_descs()

    def read_abs(self, off, ln):
        self.f.seek(off)
        d = self.f.read(ln)
        if len(d) != ln:
            raise Unsupported("short read at %d" % off)
        return d

    def read_block(self, blk):
        return self.read_abs(blk * self.block_size, self.block_size)

    def _read_descs(self):
        gdt_start = (self.first_data_block + 1) * self.block_size
        raw = self.read_abs(gdt_start, self.groups * self.desc_size)
        descs = []
        for g in range(self.groups):
            d = raw[g * self.desc_size : (g + 1) * self.desc_size]
            e = {
                "raw": d,
                "block_bitmap": struct.unpack_from("<I", d, 0x00)[0],
                "inode_bitmap": struct.unpack_from("<I", d, 0x04)[0],
                "inode_table": struct.unpack_from("<I", d, 0x08)[0],
                "free_blocks": struct.unpack_from("<H", d, 0x0C)[0],
                "free_inodes": struct.unpack_from("<H", d, 0x0E)[0],
                "flags": struct.unpack_from("<H", d, 0x12)[0],
                "block_bmp_csum": struct.unpack_from("<H", d, 0x18)[0],
                "inode_bmp_csum": struct.unpack_from("<H", d, 0x1A)[0],
                "itable_unused": struct.unpack_from("<H", d, 0x1C)[0],
                "checksum": struct.unpack_from("<H", d, 0x1E)[0],
            }
            if self.desc_size >= 64:
                e["block_bitmap"] |= struct.unpack_from("<I", d, 0x20)[0] << 32
                e["inode_bitmap"] |= struct.unpack_from("<I", d, 0x24)[0] << 32
                e["inode_table"] |= struct.unpack_from("<I", d, 0x28)[0] << 32
                e["free_blocks"] |= struct.unpack_from("<H", d, 0x2C)[0] << 16
                e["free_inodes"] |= struct.unpack_from("<H", d, 0x2E)[0] << 16
                e["itable_unused"] |= struct.unpack_from("<H", d, 0x32)[0] << 16
                e["block_bmp_csum"] |= struct.unpack_from("<H", d, 0x38)[0] << 16
                e["inode_bmp_csum"] |= struct.unpack_from("<H", d, 0x3A)[0] << 16
            descs.append(e)
        return descs

    # ---- per-object recomputation (Linux 6.6 formulas) ----

    def sb_csum(self):
        return crc32c(0xFFFFFFFF, self.sb[0:0x3FC])

    def sb_csum_stored(self):
        return struct.unpack_from("<I", self.sb, 0x3FC)[0]

    def gd_csum(self, group):
        d = self.descs[group]["raw"]
        c = crc32c(self.fs_seed, struct.pack("<I", group))
        c = crc32c(c, d[0:0x1E])
        c = crc32c(c, b"\x00\x00")
        if self.desc_size > 0x20:
            c = crc32c(c, d[0x20:])
        return c & 0xFFFF

    def bitmap_csum(self, blk, nbits, hi_stored):
        data = self.read_block(blk)[0 : (nbits + 7) // 8]
        c = crc32c(self.fs_seed, data)
        if not hi_stored:
            c &= 0xFFFF
        return c

    def raw_inode(self, ino):
        g = (ino - 1) // self.inodes_per_group
        idx = (ino - 1) % self.inodes_per_group
        off = self.descs[g]["inode_table"] * self.block_size + idx * self.inode_size
        return self.read_abs(off, self.inode_size)

    def inode_seed(self, ino, raw):
        gen = struct.unpack_from("<I", raw, 0x64)[0]
        c = crc32c(self.fs_seed, struct.pack("<I", ino))
        return crc32c(c, struct.pack("<I", gen))

    def inode_csum(self, ino, raw):
        seed = self.inode_seed(ino, raw)
        extra = struct.unpack_from("<H", raw, 0x80)[0] if self.inode_size > 128 else 0
        has_hi = self.inode_size > 128 and extra >= 4
        c = crc32c(seed, raw[0:0x7C])
        c = crc32c(c, b"\x00\x00")  # i_checksum_lo as zero
        c = crc32c(c, raw[0x7E:0x80])
        if self.inode_size > 128:
            c = crc32c(c, raw[0x80:0x82])
            if has_hi:
                c = crc32c(c, b"\x00\x00")  # i_checksum_hi as zero
                c = crc32c(c, raw[0x84 : self.inode_size])
            else:
                c = crc32c(c, raw[0x82 : self.inode_size])
        stored = struct.unpack_from("<H", raw, 0x7C)[0]
        if has_hi:
            stored |= struct.unpack_from("<H", raw, 0x82)[0] << 16
        else:
            c &= 0xFFFF
        return c, stored

    def used_inodes(self):
        """Inode numbers marked in-use by the on-disk inode bitmaps."""
        used = []
        for g, d in enumerate(self.descs):
            if d["flags"] & BG_INODE_UNINIT:
                continue
            bmp = self.read_block(d["inode_bitmap"])
            base = g * self.inodes_per_group
            for i in range(self.inodes_per_group):
                if bmp[i >> 3] & (1 << (i & 7)):
                    used.append(base + i + 1)
        return used

    def extent_blocks(self, ino, raw, notes):
        """Walk the extent tree: return (data blocks in file order capped
        later by the caller, on-disk node blocks). Non-extent trees are
        the caller's problem."""
        data = {}
        nodes = []

        def walk(node, depth, on_disk_block):
            magic, entries, _emax, ndepth = struct.unpack_from("<HHHH", node, 0)
            if magic != EH_MAGIC:
                notes.append("inode %d: bad extent magic in node" % ino)
                return
            if on_disk_block is not None:
                nodes.append(on_disk_block)
            if ndepth == 0:
                for i in range(entries):
                    off = 12 + i * 12
                    lblk, ln, hi, lo = struct.unpack_from("<IHHI", node, off)
                    ln &= 0x7FFF
                    phys = lo | (hi << 32)
                    for j in range(ln):
                        data[lblk + j] = phys + j
            else:
                for i in range(entries):
                    off = 12 + i * 12
                    lblk, lo, hi, _ = struct.unpack_from("<IIHH", node, off)
                    child = lo | (hi << 32)
                    walk(self.read_block(child), ndepth - 1, child)

        walk(raw[0x28 : 0x28 + 60], None, None)
        return data, nodes


# ------------------------------------------------------------------ main


def main():
    # The exit-code contract (docstring): 2 = unusable/unsupported image.
    # Geometry that parses but points outside the device (truncated image,
    # wild descriptor) raises Unsupported mid-scan; report it as unusable,
    # never as a corruption red — misattributing infra problems as fs bugs
    # burns triage time.
    try:
        return _run()
    except Unsupported as e:
        print("judge_csum: unusable image (mid-scan): %s" % e, file=sys.stderr)
        return 2


def _run():
    ap = argparse.ArgumentParser()
    ap.add_argument("--inodes", type=int, default=24, help="inode sample size")
    ap.add_argument("--dirs", type=int, default=12, help="directory sample size")
    ap.add_argument(
        "--ino",
        type=int,
        action="append",
        default=[],
        help="force this inode (and, if a directory, its blocks) into the sample",
    )
    ap.add_argument("--quiet", action="store_true", help="print only mismatches")
    ap.add_argument("image")
    args = ap.parse_args()

    try:
        fs = Fs(args.image)
    except Unsupported as e:
        print("judge_csum: unsupported image: %s" % e, file=sys.stderr)
        return 2
    if not fs.has_meta_csum:
        print("judge_csum: SKIP — no metadata_csum on this filesystem")
        return 0

    bad = 0
    notes = []

    def report(obj, computed, stored, width=8):
        nonlocal bad
        ok = computed == stored
        if not ok:
            bad += 1
        if not ok or not args.quiet:
            print(
                "%s: %s (stored=0x%0*x computed=0x%0*x)"
                % (obj, "MATCH" if ok else "MISMATCH", width, stored, width, computed)
            )

    report("sb", fs.sb_csum(), fs.sb_csum_stored())

    hi_blk = fs.desc_size >= 0x3A
    hi_ino = fs.desc_size >= 0x3C
    for g, d in enumerate(fs.descs):
        report("gd[%d]" % g, fs.gd_csum(g), d["checksum"], 4)
        if d["flags"] & BG_BLOCK_UNINIT:
            notes.append("gd[%d]: BLOCK_UNINIT, block bitmap csum skipped" % g)
        else:
            report(
                "bitmap-block[%d]" % g,
                fs.bitmap_csum(d["block_bitmap"], fs.blocks_per_group, hi_blk),
                d["block_bmp_csum"],
            )
        if d["flags"] & BG_INODE_UNINIT:
            notes.append("gd[%d]: INODE_UNINIT, inode bitmap csum skipped" % g)
        else:
            report(
                "bitmap-inode[%d]" % g,
                fs.bitmap_csum(d["inode_bitmap"], fs.inodes_per_group, hi_ino),
                d["inode_bmp_csum"],
            )

    # Sample in-use inodes: root + evenly spaced non-reserved ones + forced.
    used = [i for i in fs.used_inodes() if i == 2 or i >= fs.first_ino]
    sample = set(args.ino)
    sample.add(2)
    if used:
        step = max(1, len(used) // max(1, args.inodes))
        sample.update(used[::step][: args.inodes])

    dirs = []
    for ino in sorted(sample):
        try:
            raw = fs.raw_inode(ino)
        except Unsupported:
            notes.append("inode %d: unreadable, skipped" % ino)
            continue
        mode = struct.unpack_from("<H", raw, 0x00)[0]
        links = struct.unpack_from("<H", raw, 0x1A)[0]
        if links == 0 and mode == 0:
            notes.append("inode %d: unused-looking despite bitmap, skipped" % ino)
            continue
        computed, stored = fs.inode_csum(ino, raw)
        report("inode %d" % ino, computed, stored)
        if (mode & S_IFMT) == S_IFDIR:
            dirs.append((ino, raw))

    # Widen the directory pool beyond the inode sample if needed.
    if len(dirs) < args.dirs:
        for ino in used:
            if len(dirs) >= args.dirs:
                break
            if any(ino == d[0] for d in dirs):
                continue
            try:
                raw = fs.raw_inode(ino)
            except Unsupported:
                notes.append("inode %d: unreadable, skipped" % ino)
                continue
            if (struct.unpack_from("<H", raw, 0x00)[0] & S_IFMT) == S_IFDIR:
                dirs.append((ino, raw))
    dirs = dirs[: max(args.dirs, len(args.ino))]

    for ino, raw in dirs:
        flags = struct.unpack_from("<I", raw, 0x20)[0]
        if flags & FL_INDEX:
            notes.append("dir %d: htree (INDEX_FL), tail csum skipped" % ino)
            continue
        if flags & FL_INLINE_DATA:
            notes.append("dir %d: inline data, skipped" % ino)
            continue
        if not flags & FL_EXTENTS:
            notes.append("dir %d: not extent-mapped, skipped" % ino)
            continue
        seed = fs.inode_seed(ino, raw)
        data, nodes = fs.extent_blocks(ino, raw, notes)
        size = struct.unpack_from("<I", raw, 0x04)[0]
        nblocks = min(size // fs.block_size, 64)
        for lblk in range(nblocks):
            if lblk not in data:
                notes.append("dir %d: hole at block %d?!" % (ino, lblk))
                continue
            blk = fs.read_block(data[lblk])
            tail = blk[fs.block_size - DIR_TAIL_LEN :]
            t_ino, t_rec, t_nl, t_ft = struct.unpack_from("<IHBB", tail, 0)
            if not (t_ino == 0 and t_rec == DIR_TAIL_LEN and t_nl == 0 and t_ft == 0xDE):
                bad += 1
                print(
                    "dir %d block %d: MISMATCH (no valid dirent tail: %s)"
                    % (ino, lblk, tail[:8].hex())
                )
                continue
            computed = crc32c(seed, blk[0 : fs.block_size - DIR_TAIL_LEN])
            stored = struct.unpack_from("<I", tail, 8)[0]
            report("dir %d block %d" % (ino, lblk), computed, stored)
        for node_blk in nodes:
            _check_extent_node(fs, ino, seed, node_blk, report)

    # Extent node tails for sampled non-directories too.
    for ino in sorted(sample):
        if any(ino == d[0] for d in dirs):
            continue
        try:
            raw = fs.raw_inode(ino)
        except Unsupported:
            continue
        flags = struct.unpack_from("<I", raw, 0x20)[0]
        mode = struct.unpack_from("<H", raw, 0x00)[0]
        if mode == 0 or not flags & FL_EXTENTS:
            continue
        seed = fs.inode_seed(ino, raw)
        _, nodes = fs.extent_blocks(ino, raw, notes)
        for node_blk in nodes:
            _check_extent_node(fs, ino, seed, node_blk, report)

    for n in notes:
        print("note: %s" % n)
    if bad:
        print("judge_csum: %d MISMATCH object(s) on %s" % (bad, args.image))
        return 1
    print("judge_csum: all sampled objects MATCH on %s" % args.image)
    return 0


def _check_extent_node(fs, ino, seed, node_blk, report):
    node = fs.read_block(node_blk)
    emax = struct.unpack_from("<H", node, 4)[0]
    tail_off = 12 * (1 + emax)
    if tail_off + 4 > fs.block_size:
        return  # no room for a tail; nothing stored
    computed = crc32c(seed, node[0:tail_off])
    stored = struct.unpack_from("<I", node, tail_off)[0]
    report("extent-node ino %d block %d" % (ino, node_blk), computed, stored)


if __name__ == "__main__":
    sys.exit(main())
