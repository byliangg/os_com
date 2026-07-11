// SPDX-License-Identifier: MPL-2.0

//! [`ExtentTree`] — the validated, mutable extent tree of one inode.
//!
//! The tree root lives inline in the inode's 60-byte `i_block`; interior nodes
//! hold index entries pointing to child blocks read from the device, and leaf
//! nodes hold the extents mapping logical blocks to physical runs. The type
//! owns the root (validated once at construction — the parse-once boundary),
//! the inode's `i_blocks` sector accounting, and the dirty flag; every tree
//! operation is a method, so only a constructed (i.e. proven well-formed) tree
//! can be searched or mutated. Mirrors ext2's `BlockPtrTree`.

use core::ops::ControlFlow;

use super::{
    super::{
        super::{
            checksum::{self, InodeCsumSeed},
            fs::Ext4,
            journal,
            prelude::*,
        },
        RAW_BLOCK_PTRS_LEN,
    },
    node::{
        ENTRY_SIZE, EXTENT_MAGIC, Extent, ExtentHeader, ExtentIdx, ExtentKind, MAX_DEPTH,
        MAX_WRITTEN_LEN, RawExtent, RawExtentHeader, RawExtentIdx,
    },
    path::{self, ExtentPath, NodeBuf, PathLevel, Search},
};

/// Maximum extents in the inline (depth-0) root: the 60-byte `i_block` holds a
/// 12-byte header plus four 12-byte entries.
const INLINE_MAX: usize = 4;

/// Maximum extents in one full-block external leaf node.
const LEAF_MAX: usize = (BLOCK_SIZE - ENTRY_SIZE) / ENTRY_SIZE;

/// Maximum index entries in one full-block external interior node — the same
/// geometry as a leaf, since an index entry is also 12 bytes. A depth-2 tree
/// therefore holds up to `INLINE_MAX × INTERIOR_MAX × LEAF_MAX` extents.
const INTERIOR_MAX: usize = (BLOCK_SIZE - ENTRY_SIZE) / ENTRY_SIZE;

/// 512-byte sectors per filesystem block; the unit `i_blocks` is counted in.
const SECTORS_PER_BLOCK: u64 = (BLOCK_SIZE / SECTOR_SIZE) as u64;

/// The validated, mutable extent tree of one inode, plus the `i_blocks`
/// accounting that every tree mutation must keep in step.
///
/// `root` is the inode's 60-byte `i_block` in its on-disk layout, **validated
/// at construction** ([`try_new`](Self::try_new)) and thereafter only rewritten
/// by this type's own mutators — so methods trust it without re-validating
/// (rule: parse once at the boundary). `sector_count` mirrors the inode's
/// `i_blocks` (data + extent-tree metadata, in 512-byte sectors); `dirty`
/// records whether either has changed since the last writeback.
///
/// This struct is the "ExtentTree" lock content at position ③ in the global
/// lock order (report §5.1); [`ExtentManager`](super::ExtentManager) wraps it
/// in the `RwMutex` and delegates.
pub(in crate::fs::fs_impls::ext4::inode) struct ExtentTree {
    root: [u32; RAW_BLOCK_PTRS_LEN],
    sector_count: u64,
    dirty: bool,
}

impl ExtentTree {
    /// Validates `root`'s extent header once and takes ownership of the tree.
    ///
    /// This is the parse boundary: a bad magic / entry count / depth is
    /// rejected here, and every later method call trusts the root.
    pub(super) fn try_new(root: [u32; RAW_BLOCK_PTRS_LEN], sector_count: u64) -> Result<Self> {
        let header = ExtentHeader::try_from(&RawExtentHeader::from_bytes(
            &root.as_bytes()[0..ENTRY_SIZE],
        ))?;
        // The inline root holds a 12-byte header plus at most `INLINE_MAX`
        // 12-byte entries in the 60-byte `i_block`. `ExtentHeader::try_from`
        // only checks `entries <= max`; bound both here so a crafted image
        // with `entries`/`max` above the inline capacity is rejected at this
        // parse boundary rather than overrunning `i_block` in a later scan
        // (the root scanners read `root.as_bytes()[12*(1+i)..]` with no
        // container length of their own).
        if header.entries() as usize > INLINE_MAX || header.max() as usize > INLINE_MAX {
            return_errno_with_message!(
                Errno::EUCLEAN,
                "inline extent root exceeds i_block capacity"
            );
        }
        Ok(Self {
            root,
            sector_count,
            dirty: false,
        })
    }

    /// A valid empty tree: a depth-0 header (magic, 0 entries, max 4) followed
    /// by zeros — what a freshly created regular file or directory carries, so
    /// the extent reader sees a well-formed (empty) tree from the first byte.
    pub(in crate::fs::fs_impls::ext4::inode) const fn empty() -> Self {
        let mut root = [0u32; RAW_BLOCK_PTRS_LEN];
        // Each `i_block` word packs two 16-bit fields, little-endian: word 0 is
        // `eh_magic | eh_entries(=0)`, word 1 is `eh_max(=4) | eh_depth(=0)`.
        root[0] = EXTENT_MAGIC as u32;
        root[1] = INLINE_MAX as u32;
        Self {
            root,
            sector_count: 0,
            dirty: false,
        }
    }

    /// Returns the root in its on-disk 60-byte layout — the serialization
    /// boundary for the inode writeback (`i_block`).
    pub(in crate::fs::fs_impls::ext4::inode) const fn root_bytes(
        &self,
    ) -> &[u32; RAW_BLOCK_PTRS_LEN] {
        &self.root
    }

    /// Returns the inode's `i_blocks` (512-byte sectors) accounting.
    pub(super) const fn sector_count(&self) -> u64 {
        self.sector_count
    }

    /// Returns whether the tree or `i_blocks` has changed since the last
    /// writeback.
    pub(super) const fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Clears the dirty flag after a successful inode writeback.
    pub(super) fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    /// Returns the tree depth (0 = inline leaf, 1 = one level of index blocks,
    /// 2 = two levels). Infallible: the root was validated at construction. Used
    /// to assert tree shape in tests and to size the per-chunk journal credit
    /// estimate for write/truncate/reclaim ([`ExtentManager::root_depth`]).
    pub(super) fn depth(&self) -> u16 {
        self.header().depth()
    }

    /// Returns the root's header, decoded from the trusted
    /// (construction-validated) bytes.
    fn header(&self) -> ExtentHeader {
        ExtentHeader::from_trusted(&RawExtentHeader::from_bytes(
            &self.root.as_bytes()[0..ENTRY_SIZE],
        ))
    }

    /// Walks the tree to find the extent covering `iblock`, returning `None`
    /// for a hole. A thin wrapper over [`find`](Self::find) that drops the
    /// path (the read-only callers need just the verdict).
    pub(super) fn lookup(&self, fs: &Ext4, iblock: Iblock) -> Result<Option<Extent>> {
        match self.find(fs, iblock)? {
            Search::Covered { extent, .. } => Ok(Some(extent)),
            Search::Gap { .. } => Ok(None),
        }
    }

    /// Walks the tree to the leaf landing position for `iblock`: the covering
    /// extent, or the hole's insertion point with its in-leaf predecessor
    /// (see [`Search`]) — the path-recording walker the in-place surgery
    /// (P9a) edits through.
    ///
    /// External nodes are read through the journal funnel ([`NodeBuf::read`]):
    /// on a journaled volume a node's newest bytes may still sit in a journal
    /// capture (WAL suppresses the direct write until checkpoint), so a bare
    /// device read here would walk a stale tree. Each child's depth must step
    /// down by exactly one from its parent's, so a corrupt (loopy or grafted)
    /// tree fails loud instead of walking forever. When `iblock` precedes a
    /// node's first entry the walk descends into child 0 (Linux-style); on a
    /// well-formed tree no leaf under child 0 maps anything below its first
    /// key, so the landing is the same hole answer the old short-circuit gave.
    pub(super) fn find(&self, fs: &Ext4, iblock: Iblock) -> Result<Search> {
        let header = self.header();
        let root_bytes = self.root.as_bytes();
        let nr = header.entries() as usize;

        if header.is_leaf() {
            let chosen = path::last_key_le(nr, |i| root_extent_at(root_bytes, i).block(), iblock);
            let (pos, landing) = leaf_landing(chosen, |i| root_extent_at(root_bytes, i), iblock);
            let path = ExtentPath {
                root_pos: pos,
                levels: Vec::new(),
            };
            return Ok(landing.into_search(path));
        }

        let root_pos =
            path::last_key_le(nr, |i| root_index_at(root_bytes, i).block(), iblock).unwrap_or(0);
        let mut next_bid = root_index_at(root_bytes, root_pos).leaf();
        let mut levels: Vec<PathLevel> = Vec::with_capacity(header.depth() as usize);

        for expected_depth in (0..header.depth()).rev() {
            let node = NodeBuf::read(fs, next_bid)?;
            if node.depth() != expected_depth {
                return_errno_with_message!(
                    Errno::EUCLEAN,
                    "extent child depth does not step down by one"
                );
            }
            if node.is_leaf() {
                let chosen = node.leaf_pos(iblock);
                let (pos, landing) = leaf_landing(chosen, |i| node.extent_at(i), iblock);
                levels.push(PathLevel { node, pos });
                let path = ExtentPath { root_pos, levels };
                return Ok(landing.into_search(path));
            }
            let pos = node.index_pos(iblock).unwrap_or(0);
            next_bid = node.index_at(pos).leaf();
            levels.push(PathLevel { node, pos });
        }
        // The countdown ends at depth 0, whose node is a leaf and returned above.
        return_errno_with_message!(Errno::EUCLEAN, "extent walk fell through its own depth");
    }

    /// Calls `visit_fn` on each extent overlapping `[range.start, range.end)`
    /// in ascending logical order, descending only the subtrees the range
    /// touches — the bounded replacement for whole-tree flattens on the read
    /// paths. The range is `u64` because a length-derived end (`iblock + len`)
    /// can exceed the 32-bit logical space by up to one extent.
    pub(super) fn walk_range(
        &self,
        fs: &Ext4,
        range: Range<u64>,
        visit_fn: &mut impl FnMut(&Extent) -> ControlFlow<()>,
    ) -> Result<()> {
        if range.start >= range.end {
            return Ok(());
        }
        // Nothing maps at or above 2^32 logical blocks: an out-of-space start
        // has nothing to visit.
        let Ok(start_key) = Iblock::try_from(range.start) else {
            return Ok(());
        };
        let header = self.header();
        let root_bytes = self.root.as_bytes();
        let nr = header.entries() as usize;

        if header.is_leaf() {
            for i in 0..nr {
                let e = root_extent_at(root_bytes, i);
                if e.block() as u64 >= range.end {
                    break;
                }
                if e.block() as u64 + e.len() as u64 > range.start && visit_fn(&e).is_break() {
                    return Ok(());
                }
            }
            return Ok(());
        }

        let first =
            path::last_key_le(nr, |i| root_index_at(root_bytes, i).block(), start_key).unwrap_or(0);
        for i in first..nr {
            let child = root_index_at(root_bytes, i);
            if child.block() as u64 >= range.end {
                break;
            }
            if walk_child(fs, child.leaf(), header.depth() - 1, &range, visit_fn)?.is_break() {
                return Ok(());
            }
        }
        Ok(())
    }

    /// The pre-surgery linear walker, kept verbatim as the ktest
    /// cross-verification reference for [`find`](Self::find): both must give
    /// the same covered-extent / hole verdict on every probe of any tree.
    #[cfg(ktest)]
    pub(super) fn lookup_linear(&self, fs: &Ext4, iblock: Iblock) -> Result<Option<Extent>> {
        let root_bytes = self.root.as_bytes();
        let mut next_bid = match search_entries(&self.header(), root_bytes, iblock)? {
            Step::Found(extent) => return Ok(Some(extent)),
            Step::Hole => return Ok(None),
            Step::Descend(bid) => bid,
        };

        let journal = fs.journal();
        let device = fs.block_device().as_ref();
        for _ in 0..MAX_DEPTH {
            let block = journal::read_metadata_block(journal.as_deref(), device, next_bid)?;
            match search_node(&block, iblock)? {
                Step::Found(extent) => return Ok(Some(extent)),
                Step::Hole => return Ok(None),
                Step::Descend(bid) => next_bid = bid,
            }
        }
        return_errno_with_message!(Errno::EUCLEAN, "extent tree deeper than maximum depth");
    }

    /// Walks the right spine of the tree, descending into the LAST index entry
    /// at each level, and returns the path down to the rightmost external
    /// leaf. `None` on a depth-0 tree (the inline root is itself the leaf) or
    /// an empty tree. Each child's depth must step down by exactly one (loop /
    /// graft guard, as in [`find`](Self::find)).
    ///
    /// The tail-truncate spine reads the rightmost leaf, frees its doomed
    /// entries, and (when it empties) prunes upward — re-walking here after
    /// each prune, which reads the just-written-back parent through the
    /// journal funnel.
    fn rightmost_path(&self, fs: &Ext4) -> Result<Option<ExtentPath>> {
        let header = self.header();
        if header.is_leaf() {
            return Ok(None);
        }
        let root_bytes = self.root.as_bytes();
        let nr = header.entries() as usize;
        // A well-formed non-leaf root has ≥ 1 child (an empty index root is
        // reset to a depth-0 leaf by the pruning below). A crafted empty index
        // root is corruption — fail loud rather than underflow `nr - 1`
        // (matching `find`'s `unwrap_or(0)` robustness on the same input).
        if nr == 0 {
            return_errno_with_message!(Errno::EUCLEAN, "non-leaf extent root has no children");
        }
        let root_pos = nr - 1;
        let mut next_bid = root_index_at(root_bytes, root_pos).leaf();
        let mut levels: Vec<PathLevel> = Vec::with_capacity(header.depth() as usize);
        for expected_depth in (0..header.depth()).rev() {
            let node = NodeBuf::read(fs, next_bid)?;
            if node.depth() != expected_depth {
                return_errno_with_message!(
                    Errno::EUCLEAN,
                    "extent child depth does not step down by one"
                );
            }
            // `NodeBuf::read` rejects an empty interior; a committed tree never
            // keeps an empty external leaf either (a truncate prunes it), so an
            // empty node here is corruption — fail loud, don't underflow `- 1`.
            let Some(pos) = node.entries().checked_sub(1) else {
                return_errno_with_message!(Errno::EUCLEAN, "rightmost extent node is empty");
            };
            if node.is_leaf() {
                levels.push(PathLevel { node, pos });
                return Ok(Some(ExtentPath { root_pos, levels }));
            }
            next_bid = node.index_at(pos).leaf();
            levels.push(PathLevel { node, pos });
        }
        return_errno_with_message!(Errno::EUCLEAN, "extent walk fell through its own depth");
    }

    /// Parses the whole tree into a list of leaf extents sorted by logical
    /// block. Used by the write path to plan hole runs from a tree snapshot.
    pub(super) fn extents(&self, fs: &Ext4) -> Result<Vec<Extent>> {
        let (mut extents, _external) = self.flatten(fs)?;
        extents.sort_by_key(|e| e.block());
        Ok(extents)
    }

    /// Inserts the extent mapping `[iblock, iblock+len)` → `[pblock,
    /// pblock+len)`, rebuilding the on-disk layout and growing `i_blocks` by
    /// the `len` data blocks plus the net metadata-block delta.
    ///
    /// The rebuild takes the simple, correct route: the tree is flattened to a
    /// sorted extent list, the new run is merged in, and the list is
    /// re-serialized as an inline (≤ [`INLINE_MAX`] extents), depth-1, or
    /// depth-2 tree (see [`reserialize`](Self::reserialize)).
    /// In-place B-tree surgery is a later (Phase 9) optimization. The caller
    /// must guarantee `[iblock, iblock+len)` is currently a hole (the write
    /// path only inserts for unmapped blocks).
    ///
    /// External leaf blocks are reused in place across mutations. Under a
    /// journal handle their reads and writes go through the journal funnels
    /// ([`journal::read_metadata_block`], capture + patch), so WAL order
    /// holds; without one they are read and written directly (Phases 1–3
    /// semantics).
    #[expect(clippy::too_many_arguments)]
    pub(super) fn insert(
        &mut self,
        fs: &Ext4,
        iblock: Iblock,
        pblock: Ext4Bid,
        len: u16,
        kind: ExtentKind,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
    ) -> Result<()> {
        // Surgery route (T2 fast path + T3 room-making): edit exactly the
        // landing leaf, splitting full nodes or growing the tree a level when
        // the path has no room. Every `make_room_for` strictly adds capacity
        // on the path (a split gives the landing leaf room; a grow adds a
        // level the next round splits), so the retry bound is unreachable
        // except on a corrupt tree — fail loud rather than spin.
        if self.header().depth() > 0 {
            for _ in 0..(MAX_DEPTH as usize + 2) {
                if self.try_insert_in_place(fs, iblock, pblock, len, kind, handle, csum_seed)? {
                    // Data blocks only: the leaf was edited in place; any
                    // metadata the room-making allocated was accounted there.
                    self.sector_count = (self.sector_count as i64
                        + len as i64 * SECTORS_PER_BLOCK as i64)
                        .max(0) as u64;
                    self.dirty = true;
                    return Ok(());
                }
                self.make_room_for(fs, iblock, handle, csum_seed)?;
            }
            return_errno_with_message!(Errno::EUCLEAN, "extent insert cannot make room");
        }

        // Depth-0: the inline rebuild — memory-only up to INLINE_MAX extents,
        // and the exact, cheap builder of the first external leaf beyond.
        let (mut extents, old_external) = self.flatten(fs)?;
        extents.push(Extent::new(iblock, len, pblock, kind));
        merge_extents(&mut extents);
        let delta = self.reserialize(fs, &extents, &old_external, handle, csum_seed)?;

        let net_meta = delta.meta_allocated as i64 - delta.meta_freed as i64;
        let added_blocks = len as i64 + net_meta;
        // `.max(0)`: `i_blocks` must never wrap negative-to-huge on a
        // miscounted delta.
        self.sector_count =
            (self.sector_count as i64 + added_blocks * SECTORS_PER_BLOCK as i64).max(0) as u64;
        self.dirty = true;
        Ok(())
    }

    /// Attempts the in-place leaf insert of `[iblock, iblock+len) → pblock`:
    /// merge onto the in-leaf predecessor (absorbing a bridged successor), or
    /// shift-insert into free space, correcting ancestor index keys when the
    /// leaf's first key drops. Returns `false` — tree untouched — when the
    /// leaf is full, and the caller falls back to the whole-tree rebuild.
    ///
    /// The caller guarantees the whole range is a hole (the write path plans
    /// from a snapshot under this same ③ lock), so the landing must be a
    /// [`Search::Gap`]; a covered landing means the tree contradicts the plan
    /// — fail loud rather than corrupt.
    #[expect(clippy::too_many_arguments)]
    fn try_insert_in_place(
        &mut self,
        fs: &Ext4,
        iblock: Iblock,
        pblock: Ext4Bid,
        len: u16,
        kind: ExtentKind,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
    ) -> Result<bool> {
        let e = Extent::new(iblock, len, pblock, kind);
        let Search::Gap { mut path, prev } = self.find(fs, iblock)? else {
            return_errno_with_message!(Errno::EUCLEAN, "extent insert target is already mapped");
        };
        let device = fs.block_device();
        // The caller gated on depth ≥ 1, so the path ends in an external leaf.
        let leaf_level = path.levels.len() - 1;
        let insert_pos = path.levels[leaf_level].pos;

        // Merge onto the predecessor: entry count unchanged, first key
        // unchanged (a predecessor exists, so the position is ≥ 1) — no index
        // correction, one leaf write.
        if let Some(p) = prev
            && can_merge(&p, &e)
        {
            let leaf = &mut path.levels[leaf_level].node;
            let mut merged = merged_pair(&p, &e);
            // The grown run may now bridge flush against the old successor
            // (still named by `insert_pos`: nothing shifted).
            if insert_pos < leaf.entries() {
                let next = leaf.extent_at(insert_pos);
                if can_merge(&merged, &next) {
                    merged = merged_pair(&merged, &next);
                    leaf.remove_extent_at(insert_pos);
                }
            }
            leaf.replace_extent_at(insert_pos - 1, &merged);
            leaf.write_back(device.as_ref(), handle, csum_seed)?;
            return Ok(true);
        }

        // Shift-insert into free space; a full leaf is the rebuild fallback.
        // The capacity check (and the in-buffer edit) precedes every durable
        // write, so a `false` return leaves the tree untouched.
        {
            let leaf = &mut path.levels[leaf_level].node;
            if leaf.insert_extent_at(insert_pos, &e).is_err() {
                return Ok(false);
            }
            // The new run may bridge flush against its successor.
            if insert_pos + 1 < leaf.entries() {
                let next = leaf.extent_at(insert_pos + 1);
                if can_merge(&e, &next) {
                    leaf.replace_extent_at(insert_pos, &merged_pair(&e, &next));
                    leaf.remove_extent_at(insert_pos + 1);
                }
            }
        }

        // Inserted at the leaf's position 0: the leaf's first key dropped, so
        // the ancestor index keys naming this subtree drop with it (Linux
        // `ext4_ext_correct_indexes`, extents.c:1705 — propagate upward while
        // each level sits at position 0). The ancestors are written BEFORE the
        // leaf: a key alone (old or new) is a valid lower bound either way, so
        // every intermediate error state is a well-formed tree that does NOT
        // yet reference the new data blocks — if any write fails here or at
        // the leaf, the caller's on-error free of those blocks cannot strand a
        // mapped-but-freed extent.
        if insert_pos == 0 {
            self.correct_ancestor_keys(fs, &mut path, leaf_level, iblock, handle, csum_seed)?;
        }

        // The leaf lands last (see above).
        path.levels[leaf_level]
            .node
            .write_back(device.as_ref(), handle, csum_seed)?;
        Ok(true)
    }

    /// Rewrites the ancestor index keys naming the subtree under
    /// `path.levels[child_level]` to `new_key`, propagating upward while each
    /// level sits at position 0 (Linux `ext4_ext_correct_indexes`,
    /// extents.c:1705). Needed whenever a node's FIRST key changes — an insert
    /// at position 0 lowers it, a punch that removes or re-keys position 0
    /// raises it — because Linux's read-side validation demands the parent key
    /// EQUAL the child's first key exactly, not merely lower-bound it.
    fn correct_ancestor_keys(
        &mut self,
        fs: &Ext4,
        path: &mut ExtentPath,
        child_level: usize,
        new_key: Iblock,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
    ) -> Result<()> {
        let device = fs.block_device();
        let mut level = child_level;
        loop {
            if level == 0 {
                self.set_root_index_key(path.root_pos, new_key);
                self.dirty = true;
                break;
            }
            let parent = &mut path.levels[level - 1];
            parent.node.set_index_key_at(parent.pos, new_key);
            parent.node.write_back(device.as_ref(), handle, csum_seed)?;
            if parent.pos != 0 {
                break;
            }
            level -= 1;
        }
        Ok(())
    }

    /// Rewrites root index entry `i`'s key, keeping its child pointer. The
    /// root lives in the in-memory `i_block` and reaches disk with the inode
    /// writeback (no block capture of its own), like every other root rewrite.
    fn set_root_index_key(&mut self, i: usize, key: Iblock) {
        let off = ENTRY_SIZE * (1 + i);
        let bytes = self.root.as_mut_bytes();
        let mut raw = RawExtentIdx::from_bytes(&bytes[off..off + ENTRY_SIZE]);
        raw.block = key;
        bytes[off..off + ENTRY_SIZE].copy_from_slice(raw.as_bytes());
    }

    /// Inserts index entry `idx` at root position `i`, shifting later entries
    /// right — in-memory, like every root rewrite. Caller checked the root has
    /// room ([`INLINE_MAX`]).
    fn insert_root_index_at(&mut self, i: usize, idx: &RawExtentIdx) {
        let header = self.header();
        let n = header.entries() as usize;
        debug_assert!(n < INLINE_MAX && i <= n);
        let bytes = self.root.as_mut_bytes();
        let start = ENTRY_SIZE * (1 + i);
        let end = ENTRY_SIZE * (1 + n);
        bytes.copy_within(start..end, start + ENTRY_SIZE);
        bytes[start..start + ENTRY_SIZE].copy_from_slice(idx.as_bytes());
        let mut raw = RawExtentHeader::from_bytes(&bytes[0..ENTRY_SIZE]);
        raw.entries = (n + 1) as u16;
        bytes[0..ENTRY_SIZE].copy_from_slice(raw.as_bytes());
    }

    /// Grows the tree one level deeper (Linux `ext4_ext_grow_indepth`,
    /// extents.c:1311): the root's entire content moves into a freshly
    /// allocated full-block node, and the root becomes a one-entry index
    /// pointing at it. The new node is written (journaled) BEFORE the
    /// in-memory root flips, so an error leaves the old tree fully intact and
    /// only the (rolled-back) allocation touched.
    fn grow_root(
        &mut self,
        fs: &Ext4,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
    ) -> Result<()> {
        let header = self.header();
        // Cap growth at depth 2 while the flatten-based consumers
        // (`convert_unwritten`, `truncate_chunk`, `punch_chunk`, `extents`)
        // still reject depth > 2: a deeper tree would read and insert fine but
        // become un-truncatable and un-convertible (EUCLEAN), so a file could
        // not be deleted or have its unwritten regions written. The cap lifts
        // to `MAX_DEPTH` once those consumers go path-based (T5) and `flatten`
        // is retired (T6); until then it matches the pre-surgery `reserialize`
        // depth-2 ceiling — an honest ENOSPC, not a silently read-only tree.
        const GROW_DEPTH_CAP: u16 = 2;
        const { assert!(GROW_DEPTH_CAP <= MAX_DEPTH) };
        if header.depth() >= GROW_DEPTH_CAP {
            return_errno_with_message!(Errno::ENOSPC, "extent tree would exceed depth 2");
        }
        let n = header.entries() as usize;
        debug_assert!(n > 0, "only a full root grows, and full is non-empty");
        let root_bytes = self.root.as_bytes();
        let first_key = if header.is_leaf() {
            root_extent_at(root_bytes, 0).block()
        } else {
            root_index_at(root_bytes, 0).block()
        };
        // Goal: near the first child (interior root) or first data run (leaf
        // root) for locality.
        let goal = if header.is_leaf() {
            root_extent_at(root_bytes, 0).start()
        } else {
            root_index_at(root_bytes, 0).leaf()
        };

        let bid = alloc_meta_block(fs, goal, handle)?;
        let mut node = NodeBuf::fresh(bid, header.depth());
        // The root's entries are a prefix-compatible layout (same 12-byte
        // slabs); copy them verbatim under the full-block header.
        node.adopt_entries(&root_bytes[ENTRY_SIZE..ENTRY_SIZE * (1 + n)], n);
        if let Err(err) = node.write_back(fs.block_device().as_ref(), handle, csum_seed) {
            rollback_meta_blocks(fs, &[bid], handle);
            return Err(err);
        }

        // Publish: the root becomes a one-entry index one level up (in-memory,
        // infallible; it rides the inode writeback).
        self.write_index_root(&[make_index_entry(first_key, bid)], header.depth() + 1);
        self.sector_count += SECTORS_PER_BLOCK;
        self.dirty = true;
        Ok(())
    }

    /// Makes room on the path to `iblock` so the next in-place insert attempt
    /// succeeds: splits the full nodes along the path (Linux
    /// `ext4_ext_create_new_leaf`/`ext4_ext_split`, extents.c:1398/1052), or
    /// grows the tree a level when the whole path up to the root is full.
    /// Only reorganizes EXISTING entries — the new extent lands afterwards via
    /// the ordinary in-place insert, so no intermediate state here references
    /// the caller's new data blocks.
    ///
    /// Write ordering (the always-valid discipline): fresh nodes first (still
    /// unreferenced), then the shrunk old nodes, then the one landing write
    /// (or the in-memory root edit) that publishes the new subtree. Between
    /// the shrink and the publish the moved entries live only in the
    /// unpublished new nodes; if the single publish write fails, a best-effort
    /// restore re-grows the old nodes. The alternative order (publish before
    /// shrink) would expose a both-sides-referenced state whose escape on an
    /// error return double-frees on the next truncate — strictly worse than
    /// this order's worst case (a leak e2fsck reclaims).
    fn make_room_for(
        &mut self,
        fs: &Ext4,
        iblock: Iblock,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
    ) -> Result<()> {
        let Search::Gap { mut path, .. } = self.find(fs, iblock)? else {
            return_errno_with_message!(Errno::EUCLEAN, "extent insert target is already mapped");
        };
        let leaf_level = path.levels.len() - 1;
        if !path.levels[leaf_level].node.is_full() {
            // Spurious call (already room): nothing to do.
            return Ok(());
        }

        // The contiguous run of full nodes from the leaf upward; `land` is the
        // first level with room above them (`None` = the root is the landing).
        let mut top = leaf_level;
        while top > 0 && path.levels[top - 1].node.is_full() {
            top -= 1;
        }
        if top == 0 && self.header().entries() as usize >= INLINE_MAX {
            // Full all the way through the root: grow a level and let the
            // caller's loop retry (the copied-down root usually still needs a
            // split, handled by the next round).
            return self.grow_root(fs, handle, csum_seed);
        }

        let device = fs.block_device();
        // Allocate one fresh node per full level, leaf-first (goal: next to
        // the old leaf). On any failure the fresh blocks roll back; nothing
        // was referenced yet.
        let goal = path.levels[leaf_level].node.bid() + 1;
        let mut fresh: Vec<NodeBuf> = Vec::with_capacity(leaf_level - top + 1);
        let mut fresh_bids: Vec<Ext4Bid> = Vec::with_capacity(fresh.capacity());
        for level in (top..=leaf_level).rev() {
            let bid = match alloc_meta_block(fs, goal, handle) {
                Ok(bid) => bid,
                Err(err) => {
                    rollback_meta_blocks(fs, &fresh_bids, handle);
                    return Err(err);
                }
            };
            fresh_bids.push(bid);
            fresh.push(NodeBuf::fresh(bid, path.levels[level].node.depth()));
        }

        // Stage the reorganization in memory, leaf upward. `carry` is the
        // index entry publishing each level's fresh node into the level above.
        // The staging is pure memory: nothing durable happens until the write
        // phase below.
        let mut carry: Option<RawExtentIdx> = None;
        for (i, level) in (top..=leaf_level).rev().enumerate() {
            let at = if level == leaf_level {
                // The leaf splits at the insertion point, but never at 0: a
                // before-first insert (`pos == 0`) keeps the leaf's original
                // first entry in the old node (Linux's `ext4_ext_split` moves
                // from `p_ext + 1`). Splitting at 0 would empty the old leaf
                // and key the fresh node at the same block as the old leaf's
                // unchanged parent index entry — an out-of-order (duplicate-
                // key) index a crash in the retry window could persist, which
                // Linux's `ext4_valid_extent_entries` rejects on read.
                path.levels[level].pos.max(1)
            } else {
                // An interior level keeps its descent child (position `pos`)
                // on the old side.
                path.levels[level].pos + 1
            };
            path.levels[level].node.move_upper_into(at, &mut fresh[i]);
            if let Some(c) = carry.take() {
                // The child carry keys between the old node's kept tail and
                // the fresh node's first moved entry: append to old when it
                // has room, else prepend to a (necessarily empty) fresh node.
                // Both targets are proven to have room (old just passed the
                // `!is_full` check; a full old means it kept every entry, so
                // `at` moved none and fresh is empty) — a full-node `ENOSPC`
                // is unreachable, so the failure would only mean a logic bug,
                // not a runtime condition to leak `fresh_bids` on.
                let old = &mut path.levels[level].node;
                if !old.is_full() {
                    let pos = old.entries();
                    old.insert_index_at(pos, &c)
                        .expect("carry into a non-full interior cannot overflow");
                } else {
                    fresh[i]
                        .insert_index_at(0, &c)
                        .expect("carry into an empty fresh interior cannot overflow");
                }
            }
            // A split that keeps `>= 1` entry in the old node (the `at.max(1)`
            // leaf case and every interior split) gives the fresh node its
            // real first key; a pure-append leaf split (`pos == entries`)
            // leaves the fresh node empty and keys it at the insertion target
            // — a valid lower bound for the extent about to land there, above
            // everything kept below it (Linux keys an append's new leaf the
            // same way).
            let key = if fresh[i].entries() > 0 {
                fresh[i].first_key()
            } else {
                iblock
            };
            carry = Some(make_index_entry(key, fresh[i].bid()));
        }
        let final_carry = carry
            .take()
            .expect("the leaf split always produces a carry");

        // WRITES. Phase 1 — fresh nodes: still unreferenced, so any failure
        // rolls back to a fully intact old tree (the shrinks above live only
        // in this function's buffers).
        for node in fresh.iter_mut() {
            if let Err(err) = node.write_back(device.as_ref(), handle, csum_seed) {
                rollback_meta_blocks(fs, &fresh_bids, handle);
                return Err(err);
            }
        }

        // Phase 2 — the shrunk old nodes, then the landing publish. Until the
        // FIRST shrink write lands the disk is still the fully intact old tree
        // (the fresh nodes are written but referenced by nothing), so that one
        // failure rolls back clean by freeing the fresh blocks. Once a shrunk
        // node has landed the moved entries live only in the still-unpublished
        // fresh nodes: the state is irreversible inside this transaction, and
        // any later failure escalates through the P7e error funnel — the
        // journal aborts, the captures never commit, and crash atomicity
        // discards the half-state (Linux `ext4_std_error` posture). A
        // non-journaled volume has no abort; the failure leaves the same
        // torn-metadata exposure every multi-block mutation has there (P1–3).
        let mut shrink_landed = false;
        let publish = (|| -> Result<()> {
            for level in top..=leaf_level {
                path.levels[level]
                    .node
                    .write_back(device.as_ref(), handle, csum_seed)?;
                shrink_landed = true;
            }
            if top == 0 {
                // The root landing is an in-memory edit: infallible, closing
                // the window with no fallible step after the shrink writes.
                self.insert_root_index_at(path.root_pos + 1, &final_carry);
            } else {
                let landing = &mut path.levels[top - 1];
                let pos = landing.pos + 1;
                landing.node.insert_index_at(pos, &final_carry)?;
                landing
                    .node
                    .write_back(device.as_ref(), handle, csum_seed)?;
            }
            Ok(())
        })();
        if let Err(err) = publish {
            if shrink_landed {
                if let Some(h) = handle {
                    h.abort_journal_on_fs_error();
                }
            } else {
                rollback_meta_blocks(fs, &fresh_bids, handle);
            }
            return Err(err);
        }

        self.sector_count += fresh_bids.len() as u64 * SECTORS_PER_BLOCK;
        self.dirty = true;
        Ok(())
    }

    /// Converts the unwritten parts of the logical range `[iblock, iblock +
    /// len)` to written, splitting any overlapping unwritten extent so the
    /// written sub-range keeps the same physical mapping. Used by the write
    /// path so data written into preallocated (unwritten) extents becomes
    /// readable.
    ///
    /// Each overlapping unwritten extent splits into up to three runs — an
    /// unwritten head `[e.block, ov_start)`, a written middle `[ov_start,
    /// ov_end)` at the same physical offset, and an unwritten tail `[ov_end,
    /// e.end)` — dropping empty parts. Written and non-overlapping extents are
    /// untouched.
    ///
    /// No data blocks are allocated or freed: the physical mapping is
    /// preserved, so `i_blocks` changes only by the net metadata-block delta a
    /// split may cause.
    pub(super) fn convert_unwritten(
        &mut self,
        fs: &Ext4,
        iblock: Iblock,
        len: u32,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
    ) -> Result<()> {
        let range_start = iblock;
        let range_end = iblock as u64 + len as u64;

        // No unwritten extent overlaps the range → nothing to convert. Return
        // before the flatten AND the `reserialize` (which would re-journal
        // every external node of the tree). `write_at` calls this on EVERY
        // write, so this gate must be cheap: a bounded [`walk_range`] probe
        // over just the leaves the range touches (P9a-T1) — the previous
        // whole-tree flatten gate made even a plain overwrite of written
        // blocks O(tree) node reads plus a tree-sized `Vec`, the direct cause
        // of the SQLite 110/120 pathology. Without the gate itself, a depth-2
        // file's rewrite can also capture more metadata blocks than one
        // transaction holds and abort a legal write.
        let mut any_unwritten = false;
        self.walk_range(fs, range_start as u64..range_end, &mut |e| {
            if e.is_unwritten() {
                any_unwritten = true;
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        })?;
        if !any_unwritten {
            return Ok(());
        }

        // The conversion itself is still flatten-and-rebuild until P9a-T5.
        let (extents, old_external) = self.flatten(fs)?;

        // The `as u16` narrowings on the three split lengths below are
        // lossless: each split lies inside one extent, whose length is a u16
        // (`ee_len` on disk, biased below `MAX_WRITTEN_LEN`).

        let mut converted: Vec<Extent> = Vec::with_capacity(extents.len() + 2);
        for e in &extents {
            let e_start = e.block();
            let e_end = e_start as u64 + e.len() as u64;
            // Leave written extents and any extent fully outside the range as-is.
            if !e.is_unwritten() || e_end <= range_start as u64 || e_start as u64 >= range_end {
                converted.push(*e);
                continue;
            }

            let ov_start = e_start.max(range_start);
            let ov_end = (e_end).min(range_end) as Iblock;

            // Unwritten head before the overlap.
            if ov_start > e_start {
                converted.push(Extent::new(
                    e_start,
                    (ov_start - e_start) as u16,
                    e.start(),
                    ExtentKind::Unwritten,
                ));
            }
            // Written middle: same physical mapping, shifted by the head length.
            let mid_start = e.start() + (ov_start - e_start) as Ext4Bid;
            converted.push(Extent::new(
                ov_start,
                (ov_end - ov_start) as u16,
                mid_start,
                ExtentKind::Written,
            ));
            // Unwritten tail after the overlap.
            if (ov_end as u64) < e_end {
                let tail_start = e.start() + (ov_end - e_start) as Ext4Bid;
                converted.push(Extent::new(
                    ov_end,
                    (e_end - ov_end as u64) as u16,
                    tail_start,
                    ExtentKind::Unwritten,
                ));
            }
        }

        merge_extents(&mut converted);
        let delta = self.reserialize(fs, &converted, &old_external, handle, csum_seed)?;

        let net_meta = delta.meta_allocated as i64 - delta.meta_freed as i64;
        self.sector_count =
            (self.sector_count as i64 + net_meta * SECTORS_PER_BLOCK as i64).max(0) as u64;
        self.dirty = true;
        Ok(())
    }

    /// Frees every data block and extent-tree metadata block mapping a logical
    /// region at or beyond `new_size` bytes, rewriting the tree and updating
    /// `i_blocks`, in ONE transaction (the single-transaction truncate path).
    ///
    /// A thin whole-tree wrapper over [`truncate_chunk`](Self::truncate_chunk)
    /// with no credit bound: it frees every doomed extent and reserializes once.
    /// Used by `rollback_write` and the `Inode::resize` fast path, whose gate
    /// already proved the whole truncate fits one transaction.
    pub(super) fn truncate_to_byte_len(
        &mut self,
        fs: &Ext4,
        new_size: usize,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
        data_policy: journal::DataForgetPolicy,
    ) -> Result<()> {
        self.truncate_chunk(fs, new_size, handle, csum_seed, data_policy, None)?;
        Ok(())
    }

    /// One credit-bounded step of shrinking the tree to `new_size` bytes: walks
    /// the right spine, frees the doomed tail extents IN PLACE (P9a-T4a — the
    /// covering leaf entry is trimmed or removed, an emptied leaf and its
    /// emptied ancestors are pruned), and returns the frontier `reached` the
    /// survivor still references.
    ///
    /// `max_credits` is `None` for the whole-tree (single-transaction) path — it
    /// frees every doomed extent in one transaction and returns `keep_blocks`.
    /// `Some(max)` is the chunked, orphan-protected spine: each free is preceded
    /// by a wait-free credit probe reserving one free plus the O(depth) node
    /// write-backs it can trigger, and on [`ExtendOutcome::NeedsRestart`] the
    /// walk STOPS with the progress made so far. The stop returns a `reached`
    /// above `keep_blocks`; the OUTER spine `journal_restart`s (never under this
    /// ③ lock — iron law 1) and calls again, re-walking the right spine of the
    /// smaller tree.
    ///
    /// Per-chunk (not per-truncate) crash red-line: the frees and the in-place
    /// tree edits that drop exactly those extents ride ONE transaction, so a
    /// committed chunk never leaves the tree pointing at a freed (reallocatable)
    /// block (double-alloc) or a freed block the tree still names (leak). Any
    /// failure after the first free aborts the journal (the frees are captured
    /// and irreversible); the freed run's pins (`free_blocks`) release at commit,
    /// after the edit dropped it — no freed-and-reallocated window.
    ///
    /// `data_policy` is the owning inode's revoke rule for its DATA blocks
    /// (Linux `get_default_free_blocks_flags`): directory blocks and
    /// slow-symlink targets are forgotten (revoked) before their free,
    /// regular-file data is not. The tree's own external nodes are always
    /// forgotten ([`free_meta_block`]), independent of the policy.
    pub(super) fn truncate_chunk(
        &mut self,
        fs: &Ext4,
        new_size: usize,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
        data_policy: journal::DataForgetPolicy,
        max_credits: Option<usize>,
    ) -> Result<super::TruncateChunk> {
        // Lossless: callers bound `new_size` by `ensure_size_within_limit` /
        // `max_file_size` (≤ `u32::MAX` logical blocks — see `fs.rs`).
        let keep_blocks = new_size.div_ceil(BLOCK_SIZE) as Iblock;
        let depth = self.header().depth();
        let free_cost = fs.extent_free_credits();
        // The follow-up credits ONE probed data free may consume, sized to the
        // tree DEPTH (not its size): freeing the extent can empty the rightmost
        // leaf and cascade a prune up to the root, freeing ≤ `depth` metadata
        // blocks (the leaf plus up to `depth - 1` interior parents, each a
        // `free_cost` bitmap/GDT/revoke), then writing back one surviving
        // parent (1 capture) — or, on the terminal step, the boundary leaf (1
        // capture) — and the inode descriptor the chunk transaction carries (1).
        // `free_cost * depth + 2` bounds all of it. This O(depth) headroom is
        // what retires the whole-tree `truncate-EFBIG-floor` debt: a real
        // journal (thousands of credits) always clears `free_cost + this`.
        let node_headroom = free_cost * depth as usize + 2;

        // Depth-0 (inline root as a leaf): at most `INLINE_MAX` extents, so the
        // whole truncate fits any transaction — free the doomed tail and
        // rewrite the inline root in one step, no chunking, no pruning.
        if depth == 0 {
            let mut kept: Vec<Extent> = Vec::with_capacity(INLINE_MAX);
            let mut freed_data: u64 = 0;
            let root_bytes = self.root.as_bytes();
            let n = self.header().entries() as usize;
            for i in 0..n {
                let e = root_extent_at(root_bytes, i);
                let e_end = e.block() as u64 + e.len() as u64;
                if e_end <= keep_blocks as u64 {
                    kept.push(e);
                } else if e.block() < keep_blocks {
                    let head_len = (keep_blocks - e.block()) as u16;
                    let auth = data_policy.authorize(
                        e.start() + head_len as Ext4Bid,
                        e.len() as u32 - head_len as u32,
                    );
                    fs.free_blocks(auth, handle)?;
                    freed_data += (e.len() - head_len) as u64;
                    kept.push(Extent::new(e.block(), head_len, e.start(), e.kind()));
                } else {
                    let auth = data_policy.authorize(e.start(), e.len() as u32);
                    fs.free_blocks(auth, handle)?;
                    freed_data += e.len() as u64;
                }
            }
            self.write_inline_leaf_root(&kept);
            let removed = freed_data as i64 * SECTORS_PER_BLOCK as i64;
            debug_assert!(self.sector_count as i64 >= removed);
            self.sector_count = (self.sector_count as i64 - removed).max(0) as u64;
            self.dirty = true;
            let reached = kept
                .iter()
                .map(|e| e.block() + e.len() as Iblock)
                .max()
                .unwrap_or(0);
            return Ok(super::TruncateChunk {
                reached,
                next_bound: free_cost + node_headroom,
            });
        }

        // The honest EFBIG floor: one free plus the pruning/write-back it can
        // trigger must fit a whole transaction, or no restart ever can. O(depth)
        // now — a real journal always clears it (this is the debt's retirement).
        // Only a real free obligation (a rightmost extent above `keep_blocks`)
        // trips it.
        let device = fs.block_device();
        let rightmost_end = {
            let p = self
                .rightmost_path(fs)?
                .expect("depth ≥ 1 has an external leaf");
            let leaf = &p.leaf().expect("rightmost path ends in a leaf").node;
            let n = leaf.entries();
            // A well-formed non-root leaf is non-empty; guard anyway.
            if n == 0 {
                0
            } else {
                let e = leaf.extent_at(n - 1);
                e.block() as u64 + e.len() as u64
            }
        };
        let has_work = rightmost_end > keep_blocks as u64;
        if let Some(max) = max_credits
            && has_work
            && free_cost + node_headroom > max
        {
            return_errno_with_message!(
                Errno::EFBIG,
                "one truncate chunk's free plus node writes exceeds a journal transaction"
            );
        }

        let mut freed_data: u64 = 0;
        let mut freed_meta: u64 = 0;

        // Walk the right spine, freeing doomed tail extents in place. When the
        // rightmost leaf empties, prune it and re-walk to the previous sibling.
        // The loop is bounded: every iteration either frees ≥ 1 extent, prunes
        // ≥ 1 node, hits the survivor boundary, or credit-stops.
        //
        // The whole spine runs inside an abort guard: once the first free
        // captures into this transaction the chunk is irreversible, so any
        // later failure — a node `write_back` (device EIO / OOM), a prune, or
        // an ancestor-key correction — must abort the journal, or the freed
        // data would commit while the tree still maps it (double allocation).
        // The frees themselves self-abort; this catches the metadata writes
        // that follow them. (A non-journaled volume has no abort; the failure
        // leaves the same torn exposure every multi-block mutation has there.)
        let spine = (|| -> Result<Iblock> {
            let mut reached = keep_blocks;
            'chunk: loop {
                let mut path = self
                    .rightmost_path(fs)?
                    .expect("depth ≥ 1 keeps an external leaf until the root resets");
                let leaf_level = path.levels.len() - 1;
                let mut leaf_edited = false;

                // Free doomed entries from this leaf's tail. `Break::Emptied`
                // falls to the prune below; `Break::Done` ends the chunk (the
                // boundary was reached or the credit probe stopped).
                enum Break {
                    Emptied,
                    Done,
                }
                let outcome = loop {
                    let leaf = &path.levels[leaf_level].node;
                    let n = leaf.entries();
                    if n == 0 {
                        break Break::Emptied;
                    }
                    let last = leaf.extent_at(n - 1);
                    let last_end = last.block() as u64 + last.len() as u64;
                    if last_end <= keep_blocks as u64 {
                        // Survivor boundary: this leaf's tail is entirely kept.
                        reached = reached.max(last_end as Iblock);
                        break Break::Done;
                    }
                    // A free is due; probe first (chunked mode).
                    if let Some(h) = handle
                        && max_credits.is_some()
                        && stop_before_free(h, free_cost + node_headroom)?
                    {
                        reached = reached.max(last_end as Iblock);
                        break Break::Done;
                    }
                    if last.block() < keep_blocks {
                        // Straddler: keep the head, free the tail, done.
                        let head_len = (keep_blocks - last.block()) as u16;
                        let tail_len = last.len() - head_len;
                        let auth = data_policy
                            .authorize(last.start() + head_len as Ext4Bid, tail_len as u32);
                        fs.free_blocks(auth, handle)?;
                        freed_data += tail_len as u64;
                        let head = Extent::new(last.block(), head_len, last.start(), last.kind());
                        path.levels[leaf_level].node.replace_extent_at(n - 1, &head);
                        leaf_edited = true;
                        reached = reached.max(keep_blocks);
                        break Break::Done;
                    }
                    // Fully doomed: free its data and drop it from the leaf.
                    let auth = data_policy.authorize(last.start(), last.len() as u32);
                    fs.free_blocks(auth, handle)?;
                    freed_data += last.len() as u64;
                    path.levels[leaf_level].node.remove_extent_at(n - 1);
                    leaf_edited = true;
                };

                match outcome {
                    Break::Done => {
                        if leaf_edited {
                            path.levels[leaf_level].node.write_back(
                                device.as_ref(),
                                handle,
                                csum_seed,
                            )?;
                        }
                        break 'chunk Ok(reached);
                    }
                    Break::Emptied => {
                        // Prune the emptied leaf and every ancestor it empties,
                        // writing each shrunk parent back BEFORE the next
                        // re-walk reads it. A truncate to zero resets the root.
                        freed_meta += self.prune_emptied_leaf(fs, &mut path, handle, csum_seed)?;
                        if self.header().is_leaf() {
                            break 'chunk Ok(0);
                        }
                        // Re-walk to the new rightmost leaf and continue.
                    }
                }
            }
        })();
        let reached = match spine {
            Ok(reached) => reached,
            Err(err) => {
                if let Some(h) = handle {
                    h.abort_journal_on_fs_error();
                }
                return Err(err);
            }
        };

        let net_removed = freed_data + freed_meta;
        let removed_sectors = net_removed as i64 * SECTORS_PER_BLOCK as i64;
        debug_assert!(self.sector_count as i64 >= removed_sectors);
        self.sector_count = (self.sector_count as i64 - removed_sectors).max(0) as u64;
        self.dirty = true;

        Ok(super::TruncateChunk {
            reached,
            next_bound: free_cost + node_headroom,
        })
    }

    /// Frees the (emptied) leaf `path` ends in and prunes every ancestor its
    /// removal empties, returning the count of metadata blocks freed. Each
    /// level removes ITS path position (the truncate spine's rightmost walk
    /// puts every position at the last entry; a punch can empty any child).
    /// Removing a parent's position 0 raises that parent's first key, so the
    /// grandparents' keys are corrected to keep Linux's exact-first-key
    /// invariant. Each shrunk parent is written back through the journal
    /// funnel BEFORE returning, so a re-walk reads the pruned tree. When the
    /// inline root's last index entry goes, the root resets to an empty
    /// depth-0 leaf (in-memory, riding the inode writeback). Mirrors Linux
    /// `ext4_ext_rm_idx` cascading to `ext4_ext_remove_space`'s root collapse.
    fn prune_emptied_leaf(
        &mut self,
        fs: &Ext4,
        path: &mut ExtentPath,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
    ) -> Result<u64> {
        let device = fs.block_device();
        let mut freed_meta = 0u64;

        // Free the emptied leaf, then walk up removing each child's index entry;
        // stop at the first parent that stays non-empty.
        let leaf_bid = path.levels[path.levels.len() - 1].node.bid();
        free_meta_block(fs, leaf_bid, handle)?;
        freed_meta += 1;

        let mut child_level = path.levels.len() - 1;
        loop {
            if child_level == 0 {
                // The removed node's parent is the inline root.
                let root_entries = self.header().entries() as usize;
                if root_entries <= 1 {
                    // Last child gone: the tree is now empty.
                    self.write_inline_leaf_root(&[]);
                } else {
                    self.remove_root_index_at(path.root_pos);
                }
                self.dirty = true;
                break;
            }
            let parent_level = child_level - 1;
            let remove_at = path.levels[parent_level].pos;
            let pn = path.levels[parent_level].node.entries();
            if pn <= 1 {
                // Parent empties too: free it and cascade to ITS parent.
                let parent_bid = path.levels[parent_level].node.bid();
                free_meta_block(fs, parent_bid, handle)?;
                freed_meta += 1;
                child_level = parent_level;
                continue;
            }
            path.levels[parent_level].node.remove_index_at(remove_at);
            path.levels[parent_level]
                .node
                .write_back(device.as_ref(), handle, csum_seed)?;
            // Removing the parent's FIRST child raised its first key; the
            // ancestors' keys must follow exactly (Linux read-side equality).
            if remove_at == 0 {
                let new_key = path.levels[parent_level].node.first_key();
                self.correct_ancestor_keys(fs, path, parent_level, new_key, handle, csum_seed)?;
            }
            break;
        }
        Ok(freed_meta)
    }

    /// Removes root index entry `i`, shifting later entries left (in-memory,
    /// like every root rewrite; it rides the inode writeback).
    fn remove_root_index_at(&mut self, i: usize) {
        let header = self.header();
        let n = header.entries() as usize;
        debug_assert!(!header.is_leaf() && i < n);
        let bytes = self.root.as_mut_bytes();
        let start = ENTRY_SIZE * (1 + i);
        let end = ENTRY_SIZE * (1 + n);
        bytes.copy_within(start + ENTRY_SIZE..end, start);
        let mut raw = RawExtentHeader::from_bytes(&bytes[0..ENTRY_SIZE]);
        raw.entries = (n - 1) as u16;
        bytes[0..ENTRY_SIZE].copy_from_slice(raw.as_bytes());
    }

    /// One credit-bounded step of freeing the mapped blocks in the MIDDLE logical
    /// range `[start_block, end_block)` (a punch-hole), editing the covering
    /// leaves IN PLACE (P9a-T4b). Returns whether doomed blocks remain (the outer
    /// spine restarts and calls again) and the reservation the next chunk's fresh
    /// transaction should start from.
    ///
    /// Unlike [`truncate_chunk`](Self::truncate_chunk) the file size is UNCHANGED
    /// and there is no orphan protection: a crash between chunks leaves some of
    /// the range freed and some still mapped — a partial hole, which is a valid
    /// file state (reads return zero where freed, the original data where not),
    /// so no orphan link is needed to make recovery re-converge. The freed
    /// data/metadata blocks go through the SAME forget/revoke + pin funnels as
    /// truncate (`data_policy` for data, [`free_meta_block`] for tree nodes), so
    /// a replay never resurrects stale data into a reused block and a freed block
    /// is not reallocated before its freeing transaction commits.
    ///
    /// The scan runs low→high; each overlapping extent is edited in place: an
    /// edge-straddling extent is trimmed to its kept head or tail (a raised
    /// first key corrects the ancestor index keys, leaf-write-first for
    /// lower-bound safety), a fully-covered extent is removed (an emptied leaf
    /// and its emptied ancestors are pruned), and an extent spanning BOTH edges
    /// splits into head + tail — same-leaf shift-insert when there is room, else
    /// a trim plus an insert through the split machinery. `max_credits` is `None`
    /// for the whole-range path and `Some(max)` for the chunked spine — the same
    /// O(depth) per-step credit probe and EFBIG floor as truncate.
    ///
    /// Per-chunk (not per-punch) crash red-line, identical to truncate: the frees
    /// and the in-place edits that drop exactly those extents ride one
    /// transaction, and any failure after the first free aborts the journal, so
    /// a committed chunk never leaves the tree naming a freed (reallocatable)
    /// block (double-alloc) or a freed block the tree still names (leak).
    pub(super) fn punch_chunk(
        &mut self,
        fs: &Ext4,
        range: Range<Iblock>,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
        data_policy: journal::DataForgetPolicy,
        max_credits: Option<usize>,
    ) -> Result<super::PunchChunk> {
        let Range {
            start: start_block,
            end: end_block,
        } = range;
        if start_block >= end_block {
            return Ok(super::PunchChunk {
                more: false,
                next_bound: fs.extent_free_credits(),
            });
        }

        let depth = self.header().depth();
        let free_cost = fs.extent_free_credits();
        // The same O(depth) follow-up bound as the truncate spine (a free may
        // empty the leaf and prune a cascade; one boundary write; the inode
        // descriptor)…
        let node_headroom = free_cost * depth as usize + 2;
        // …plus, for the one case that INSERTS (an extent spanning the whole
        // punch range splits into head + tail through the insert machinery),
        // the insert's own per-op worst case.
        let split_headroom = node_headroom + fs.write_credits(depth);

        // Depth-0: at most INLINE_MAX entries — punch them in one pass over
        // the inline root (any transaction fits it; a spans-both split may
        // overflow the root and grow the tree through the insert path).
        if depth == 0 {
            return self.punch_inline_root(
                fs,
                start_block,
                end_block,
                handle,
                csum_seed,
                data_policy,
            );
        }

        let mut freed_data: u64 = 0;
        let mut freed_meta: u64 = 0;
        let mut more = false;
        // The reservation the outer spine restarts with: the common per-step
        // bound, raised to a stopped step's own need (a full-leaf spans-both
        // split) so the restarted transaction can run it.
        let mut restart_bound = free_cost + node_headroom;

        // The whole scan runs inside an abort guard, like the truncate spine:
        // once the first free captures into this transaction, any later
        // failure — a node `write_back`, an ancestor-key correction, a prune,
        // or the spans-both tail re-insert — must abort the journal, or the
        // freed data would commit while the tree still maps it.
        let device = fs.block_device();
        let scan = (|| -> Result<()> {
            // Scan low→high: find the first extent overlapping the remaining
            // range, classify, edit in place, continue. (The old spine freed
            // high→low, but every chunk re-scans the whole range, so the
            // direction is immaterial to the contract — `more` just says "call
            // again" — and a partial hole is a valid crash/restart state.)
            let mut cursor = start_block;
            'scan: while (cursor as u64) < end_block as u64 {
                let mut hit: Option<Extent> = None;
                self.walk_range(fs, cursor as u64..end_block as u64, &mut |e| {
                    hit = Some(*e);
                    ControlFlow::Break(())
                })?;
                let Some(e) = hit else {
                    break 'scan; // no mapped extent left in the range
                };
                let e_start = e.block();
                let e_end = e_start as u64 + e.len() as u64;

                let spans_both = e_start < start_block && e_end > end_block as u64;
                // A spans-both split usually fits the SAME leaf (head shrinks
                // in place, the tail shift-inserts beside it): probe the small
                // bound. Only a FULL leaf needs the insert split budget.
                let mut need = free_cost + node_headroom;
                let mut spans_slow = false;
                if spans_both {
                    let Search::Covered {
                        path: probe_path, ..
                    } = self.find(fs, e_start)?
                    else {
                        return_errno_with_message!(
                            Errno::EUCLEAN,
                            "walked extent vanished under the extent lock"
                        );
                    };
                    let leaf = &probe_path.levels[probe_path.levels.len() - 1].node;
                    if leaf.is_full() {
                        spans_slow = true;
                        need = free_cost + split_headroom;
                    }
                }
                // The honest EFBIG floor — O(depth), a real journal always
                // clears it (the whole-tree floor retired with the reserialize).
                if let Some(max) = max_credits
                    && need > max
                {
                    return_errno_with_message!(
                        Errno::EFBIG,
                        "one punch step's free plus node writes exceeds a journal transaction"
                    );
                }
                // Credit-aware early stop (chunked mode): never restart under
                // this ③ lock — report `more` and let the outer spine restart.
                if let Some(h) = handle
                    && max_credits.is_some()
                    && stop_before_free(h, need)?
                {
                    more = true;
                    restart_bound = restart_bound.max(need);
                    break 'scan;
                }

                // The covered middle to free.
                let mid_start = e_start.max(start_block);
                let mid_end = e_end.min(end_block as u64) as Iblock;
                let mid_len = (mid_end - mid_start) as u16; // inside one extent
                let mid_phys = e.start() + (mid_start - e_start) as Ext4Bid;

                // Locate the covering leaf entry for the in-place edit.
                let Search::Covered { mut path, .. } = self.find(fs, e_start)? else {
                    return_errno_with_message!(
                        Errno::EUCLEAN,
                        "walked extent vanished under the extent lock"
                    );
                };
                let leaf_level = path.levels.len() - 1;
                let pos = path.levels[leaf_level].pos;

                if spans_both {
                    let head_len = (start_block - e_start) as u16;
                    let tail_len = (e_end - end_block as u64) as u16;
                    let tail_phys = e.start() + (end_block - e_start) as Ext4Bid;
                    let head = Extent::new(e_start, head_len, e.start(), e.kind());
                    let tail = Extent::new(end_block, tail_len, tail_phys, e.kind());
                    if !spans_slow {
                        // Same-leaf fast path: shrink to the head and
                        // shift-insert the tail beside it — one write.
                        let leaf = &mut path.levels[leaf_level].node;
                        leaf.replace_extent_at(pos, &head);
                        leaf.insert_extent_at(pos + 1, &tail)
                            .expect("probed leaf had room for the split tail");
                        leaf.write_back(device.as_ref(), handle, csum_seed)?;
                    } else {
                        // Full leaf: trim to the head in place; the tail range
                        // is then a hole and re-enters through the ordinary
                        // insert (the split machinery). All in ONE transaction;
                        // the abort guard covers a re-insert failure.
                        path.levels[leaf_level].node.replace_extent_at(pos, &head);
                        path.levels[leaf_level].node.write_back(
                            device.as_ref(),
                            handle,
                            csum_seed,
                        )?;
                        self.insert(
                            fs,
                            end_block,
                            tail_phys,
                            tail_len,
                            e.kind(),
                            handle,
                            csum_seed,
                        )?;
                        // `insert` accounted the tail as NEW data; its blocks
                        // were already counted before the split.
                        self.sector_count = self
                            .sector_count
                            .saturating_sub(tail_len as u64 * SECTORS_PER_BLOCK);
                    }
                    let auth = data_policy.authorize(mid_phys, mid_len as u32);
                    fs.free_blocks(auth, handle)?;
                    freed_data += mid_len as u64;
                    cursor = end_block;
                    continue 'scan;
                }

                // Single-sided straddles and fully-covered extents: one
                // in-place edit, no insert. Free first (probed above), then
                // edit.
                let auth = data_policy.authorize(mid_phys, mid_len as u32);
                fs.free_blocks(auth, handle)?;
                freed_data += mid_len as u64;
                if e_start < start_block {
                    // Straddles the start: keep the head (first key unchanged).
                    let head_len = (start_block - e_start) as u16;
                    let head = Extent::new(e_start, head_len, e.start(), e.kind());
                    path.levels[leaf_level].node.replace_extent_at(pos, &head);
                    path.levels[leaf_level]
                        .node
                        .write_back(device.as_ref(), handle, csum_seed)?;
                } else if e_end > end_block as u64 {
                    // Straddles the end: keep the tail, re-keyed to `end_block`.
                    // A position-0 edit raises the leaf's first key; write the
                    // LEAF first, THEN correct the ancestors — the risen key
                    // goes up only after the leaf actually holds it, so every
                    // intermediate state has an ancestor key that is a valid
                    // lower bound (never above the leaf's real first block).
                    let tail_len = (e_end - end_block as u64) as u16;
                    let tail_phys = e.start() + (end_block - e_start) as Ext4Bid;
                    let tail = Extent::new(end_block, tail_len, tail_phys, e.kind());
                    path.levels[leaf_level].node.replace_extent_at(pos, &tail);
                    path.levels[leaf_level]
                        .node
                        .write_back(device.as_ref(), handle, csum_seed)?;
                    if pos == 0 {
                        self.correct_ancestor_keys(
                            fs, &mut path, leaf_level, end_block, handle, csum_seed,
                        )?;
                    }
                } else {
                    // Fully covered: drop the entry; prune if the leaf empties,
                    // else write the leaf and (on a risen first key) correct
                    // the ancestors AFTER the leaf write (lower-bound-safe
                    // order, as above).
                    path.levels[leaf_level].node.remove_extent_at(pos);
                    if path.levels[leaf_level].node.entries() == 0 {
                        freed_meta += self.prune_emptied_leaf(fs, &mut path, handle, csum_seed)?;
                    } else {
                        let rose = pos == 0;
                        let new_key = path.levels[leaf_level].node.first_key();
                        path.levels[leaf_level].node.write_back(
                            device.as_ref(),
                            handle,
                            csum_seed,
                        )?;
                        if rose {
                            self.correct_ancestor_keys(
                                fs, &mut path, leaf_level, new_key, handle, csum_seed,
                            )?;
                        }
                    }
                }
                cursor = mid_end;
            }
            Ok(())
        })();
        if let Err(err) = scan {
            if let Some(h) = handle {
                h.abort_journal_on_fs_error();
            }
            return Err(err);
        }

        // `i_blocks` drops by the freed data plus the pruned tree nodes.
        let removed_sectors = (freed_data + freed_meta) as i64 * SECTORS_PER_BLOCK as i64;
        debug_assert!(self.sector_count as i64 >= removed_sectors);
        self.sector_count = (self.sector_count as i64 - removed_sectors).max(0) as u64;
        self.dirty = true;

        Ok(super::PunchChunk {
            more,
            next_bound: restart_bound,
        })
    }

    /// The depth-0 punch: at most [`INLINE_MAX`] inline entries, decomposed in
    /// one pass (any transaction holds it). A spans-both split can push the
    /// survivor count past the inline capacity; the tail then re-enters via
    /// the ordinary insert, which grows the tree as needed.
    fn punch_inline_root(
        &mut self,
        fs: &Ext4,
        start_block: Iblock,
        end_block: Iblock,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
        data_policy: journal::DataForgetPolicy,
    ) -> Result<super::PunchChunk> {
        let free_cost = fs.extent_free_credits();
        let root_bytes = self.root.as_bytes();
        let n = self.header().entries() as usize;

        // Decompose in logical order. A single extent spanning BOTH edges
        // yields head + tail (+1 survivor), so the survivor list can reach
        // `INLINE_MAX + 1`; every other case nets zero or fewer.
        let mut survivors: Vec<Extent> = Vec::with_capacity(INLINE_MAX + 1);
        let mut doomed: Vec<Extent> = Vec::new(); // freed after the root rewrite
        for i in 0..n {
            let e = root_extent_at(root_bytes, i);
            let e_start = e.block();
            let e_end = e_start as u64 + e.len() as u64;
            if e_end <= start_block as u64 || e_start as u64 >= end_block as u64 {
                survivors.push(e);
                continue;
            }
            let mid_start = e_start.max(start_block);
            let mid_end = e_end.min(end_block as u64) as Iblock;
            doomed.push(Extent::new(
                mid_start,
                (mid_end - mid_start) as u16,
                e.start() + (mid_start - e_start) as Ext4Bid,
                e.kind(),
            ));
            if e_start < start_block {
                survivors.push(Extent::new(
                    e_start,
                    (start_block - e_start) as u16,
                    e.start(),
                    e.kind(),
                ));
            }
            if e_end > end_block as u64 {
                survivors.push(Extent::new(
                    end_block,
                    (e_end - end_block as u64) as u16,
                    e.start() + (end_block - e_start) as Ext4Bid,
                    e.kind(),
                ));
            }
        }

        // At most one extent overflows the inline root; peel the last survivor
        // (they stay sorted) and re-insert it after the rewrite, growing the
        // tree to depth 1. `survivors` never exceeds `INLINE_MAX + 1`.
        debug_assert!(survivors.len() <= INLINE_MAX + 1);
        let overflow_tail = if survivors.len() > INLINE_MAX {
            survivors.pop()
        } else {
            None
        };

        self.write_inline_leaf_root(&survivors);
        self.dirty = true;
        if let Some(tail) = overflow_tail {
            // The rewrite dropped the tail from the root; a failure before the
            // insert lands would strand it unmapped-but-allocated — escalate.
            let reattach = self.insert(
                fs,
                tail.block(),
                tail.start(),
                tail.len(),
                tail.kind(),
                handle,
                csum_seed,
            );
            if let Err(err) = reattach {
                if let Some(h) = handle {
                    h.abort_journal_on_fs_error();
                }
                return Err(err);
            }
            // `insert` counted the tail as new data; it was already accounted.
            self.sector_count = self
                .sector_count
                .saturating_sub(tail.len() as u64 * SECTORS_PER_BLOCK);
        }

        let mut freed_data: u64 = 0;
        for d in &doomed {
            let auth = data_policy.authorize(d.start(), d.len() as u32);
            fs.free_blocks(auth, handle)?;
            freed_data += d.len() as u64;
        }
        let removed_sectors = freed_data as i64 * SECTORS_PER_BLOCK as i64;
        debug_assert!(self.sector_count as i64 >= removed_sectors);
        self.sector_count = (self.sector_count as i64 - removed_sectors).max(0) as u64;

        Ok(super::PunchChunk {
            more: false,
            next_bound: free_cost + 2,
        })
    }

    /// Parses the whole tree into a list of leaf extents, also returning the
    /// physical blocks of **every** external node — leaf blocks at depth 1, and
    /// both interior and leaf blocks at depth 2. The returned block list is the
    /// reuse/free pool [`reserialize`](Self::reserialize) draws from, so it must
    /// name all metadata blocks the current tree references.
    ///
    /// Phase 6 builds depth-0 (inline), depth-1, or depth-2 trees, so a depth of
    /// 3 or more is rejected rather than walked. External nodes are read through
    /// [`journal::read_metadata_block`] — see [`lookup`](Self::lookup).
    fn flatten(&self, fs: &Ext4) -> Result<(Vec<Extent>, Vec<Ext4Bid>)> {
        let root_bytes = self.root.as_bytes();
        let header = self.header();
        let nr = header.entries() as usize;

        if header.is_leaf() {
            let mut extents = Vec::with_capacity(nr);
            for i in 0..nr {
                let off = ENTRY_SIZE * (1 + i);
                extents.push(Extent::from(&RawExtent::from_bytes(
                    &root_bytes[off..off + ENTRY_SIZE],
                )));
            }
            return Ok((extents, Vec::new()));
        }

        let depth = header.depth();
        if depth != 1 && depth != 2 {
            return_errno_with_message!(Errno::EUCLEAN, "extent tree deeper than phase 6 supports");
        }

        // The root's index entries name the immediate children: leaf blocks at
        // depth 1, interior blocks at depth 2.
        let mut child_bids = Vec::with_capacity(nr);
        for i in 0..nr {
            let off = ENTRY_SIZE * (1 + i);
            let idx = ExtentIdx::from(&RawExtentIdx::from_bytes(
                &root_bytes[off..off + ENTRY_SIZE],
            ));
            child_bids.push(idx.leaf());
        }

        let journal = fs.journal();
        let device = fs.block_device().as_ref();

        if depth == 1 {
            let mut extents = Vec::new();
            for &leaf_bid in &child_bids {
                let block = journal::read_metadata_block(journal.as_deref(), device, leaf_bid)?;
                extents.extend(parse_leaf_node(&block)?);
            }
            return Ok((extents, child_bids));
        }

        // depth == 2: root → interior nodes → leaf nodes. Collect every external
        // block (interior and leaf) into the reuse pool.
        let mut extents = Vec::new();
        let mut external = Vec::new();
        for &interior_bid in &child_bids {
            external.push(interior_bid);
            let interior = journal::read_metadata_block(journal.as_deref(), device, interior_bid)?;
            for leaf_bid in parse_interior_node(&interior)? {
                let leaf = journal::read_metadata_block(journal.as_deref(), device, leaf_bid)?;
                extents.extend(parse_leaf_node(&leaf)?);
                external.push(leaf_bid);
            }
        }
        Ok((extents, external))
    }

    /// Re-serializes `extents` into the on-disk tree, reusing the existing
    /// external metadata blocks where possible and allocating/freeing the
    /// difference.
    ///
    /// The shape follows the extent count: an inline (depth-0) root for up to
    /// [`INLINE_MAX`] extents, a depth-1 index for up to `INLINE_MAX × LEAF_MAX`,
    /// and a depth-2 index (root → interior nodes → leaf nodes) for up to
    /// `INLINE_MAX × INTERIOR_MAX × LEAF_MAX`. Beyond that a depth-3 tree would be
    /// needed, which the flatten-and-rebuild strategy does not build — the
    /// honest [`Errno::ENOSPC`]. In-place B-tree surgery is a later optimization.
    fn reserialize(
        &mut self,
        fs: &Ext4,
        extents: &[Extent],
        old_external: &[Ext4Bid],
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
    ) -> Result<TreeDelta> {
        let device = fs.block_device().as_ref();

        if extents.len() <= INLINE_MAX {
            self.write_inline_leaf_root(extents);
            // The root no longer references any external block; free them all.
            let mut meta_freed = 0;
            for &bid in old_external {
                free_meta_block(fs, bid, handle)?;
                meta_freed += 1;
            }
            return Ok(TreeDelta {
                meta_allocated: 0,
                meta_freed,
            });
        }

        let nr_leaves = extents.len().div_ceil(LEAF_MAX);
        let goal = extents.first().map(|e| e.start()).unwrap_or(0);

        if nr_leaves <= INLINE_MAX {
            // Depth-1: the inline root indexes `nr_leaves` external leaf blocks.
            let reuse = nr_leaves.min(old_external.len());
            let (leaf_bids, newly_allocated) =
                acquire_meta_blocks(fs, old_external, nr_leaves, goal, handle)?;

            // Write each leaf node. On failure, roll back the freshly allocated
            // blocks (the in-memory root is not yet updated, so the old tree
            // stays referenced).
            for (chunk, &leaf_bid) in extents.chunks(LEAF_MAX).zip(leaf_bids.iter()) {
                if let Err(err) = write_leaf_node(device, leaf_bid, chunk, handle, csum_seed) {
                    rollback_meta_blocks(fs, &newly_allocated, handle);
                    return Err(err);
                }
            }

            // Commit: rewrite the depth-1 index root (in memory, infallible).
            let index_entries: Vec<RawExtentIdx> = extents
                .chunks(LEAF_MAX)
                .zip(leaf_bids.iter())
                .map(|(chunk, &leaf_bid)| make_index_entry(chunk[0].block(), leaf_bid))
                .collect();
            self.write_index_root(&index_entries, 1);

            // Free surplus old external blocks the root no longer references.
            let mut meta_freed = 0;
            for &bid in &old_external[reuse..] {
                free_meta_block(fs, bid, handle)?;
                meta_freed += 1;
            }

            return Ok(TreeDelta {
                meta_allocated: newly_allocated.len() as u32,
                meta_freed,
            });
        }

        let nr_interior = nr_leaves.div_ceil(INTERIOR_MAX);
        if nr_interior > INLINE_MAX {
            // Would need a depth-3 tree; the rebuild strategy caps at depth 2.
            return_errno_with_message!(Errno::ENOSPC, "extent tree would exceed depth 2");
        }

        // Depth-2: the inline root indexes `nr_interior` interior nodes, each of
        // which indexes up to `INTERIOR_MAX` leaf blocks. Interior and leaf blocks are
        // interchangeable metadata blocks (each is fully overwritten), so one
        // reuse pool serves both; the first `nr_leaves` become leaves, the rest
        // interiors.
        let total = nr_leaves + nr_interior;
        let reuse = total.min(old_external.len());
        let (blocks, newly_allocated) = acquire_meta_blocks(fs, old_external, total, goal, handle)?;
        let (leaf_bids, interior_bids) = blocks.split_at(nr_leaves);

        // Write leaves, then interiors. On any failure, roll back the freshly
        // allocated blocks (the in-memory root is not yet updated).
        for (chunk, &leaf_bid) in extents.chunks(LEAF_MAX).zip(leaf_bids.iter()) {
            if let Err(err) = write_leaf_node(device, leaf_bid, chunk, handle, csum_seed) {
                rollback_meta_blocks(fs, &newly_allocated, handle);
                return Err(err);
            }
        }

        // One leaf-index entry per leaf, keyed by that leaf's first logical block
        // (the extents are sorted, so the chunk's first entry is its minimum).
        let leaf_index: Vec<RawExtentIdx> = extents
            .chunks(LEAF_MAX)
            .zip(leaf_bids.iter())
            .map(|(chunk, &leaf_bid)| make_index_entry(chunk[0].block(), leaf_bid))
            .collect();

        for (chunk, &interior_bid) in leaf_index.chunks(INTERIOR_MAX).zip(interior_bids.iter()) {
            if let Err(err) = write_interior_node(device, interior_bid, chunk, handle, csum_seed) {
                rollback_meta_blocks(fs, &newly_allocated, handle);
                return Err(err);
            }
        }

        // Commit: rewrite the depth-2 index root, one entry per interior node
        // keyed by that interior's first logical block (in memory, infallible).
        let root_index: Vec<RawExtentIdx> = leaf_index
            .chunks(INTERIOR_MAX)
            .zip(interior_bids.iter())
            .map(|(chunk, &interior_bid)| make_index_entry(chunk[0].block, interior_bid))
            .collect();
        self.write_index_root(&root_index, 2);

        // Free surplus old external blocks the tree no longer references.
        let mut meta_freed = 0;
        for &bid in &old_external[reuse..] {
            free_meta_block(fs, bid, handle)?;
            meta_freed += 1;
        }

        Ok(TreeDelta {
            meta_allocated: newly_allocated.len() as u32,
            meta_freed,
        })
    }

    /// Rewrites the root as a depth-0 inline leaf (header + up to
    /// [`INLINE_MAX`] extents).
    fn write_inline_leaf_root(&mut self, extents: &[Extent]) {
        let bytes = self.root.as_mut_bytes();
        bytes.fill(0);
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: extents.len() as u16,
            max: INLINE_MAX as u16,
            depth: 0,
            generation: 0,
        };
        bytes[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        for (i, ext) in extents.iter().enumerate() {
            let off = ENTRY_SIZE * (1 + i);
            bytes[off..off + ENTRY_SIZE].copy_from_slice(RawExtent::from(ext).as_bytes());
        }
    }

    /// Rewrites the root as an index node at `depth` (1 or 2): a header plus one
    /// index entry per child (an external leaf at depth 1, an interior node at
    /// depth 2). The inline root's capacity is [`INLINE_MAX`] at either depth.
    fn write_index_root(&mut self, entries: &[RawExtentIdx], depth: u16) {
        let bytes = self.root.as_mut_bytes();
        bytes.fill(0);
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: entries.len() as u16,
            max: INLINE_MAX as u16,
            depth,
            generation: 0,
        };
        bytes[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        for (i, idx) in entries.iter().enumerate() {
            let off = ENTRY_SIZE * (1 + i);
            bytes[off..off + ENTRY_SIZE].copy_from_slice(idx.as_bytes());
        }
    }

    /// Returns the external (leaf + interior) node count of the on-disk tree
    /// holding `extents` extents — the number of full-block nodes
    /// [`reserialize`](Self::reserialize) writes for that count, following the
    /// same inline / depth-1 / depth-2 shape.
    ///
    /// An inline (depth-0) root has no external nodes; a depth-1 tree has
    /// `ceil(extents / LEAF_MAX)` leaves under the inline root; a depth-2 tree
    /// adds `ceil(nr_leaves / INTERIOR_MAX)` interior nodes. Used to size the
    /// write spine's per-chunk credit bound before an insert (an upper bound: a
    /// merge on insert can only lower the true count).
    pub(super) fn external_node_count(extents: usize) -> usize {
        if extents <= INLINE_MAX {
            return 0;
        }
        let nr_leaves = extents.div_ceil(LEAF_MAX);
        let nr_interior = if nr_leaves <= INLINE_MAX {
            0
        } else {
            nr_leaves.div_ceil(INTERIOR_MAX)
        };
        nr_leaves + nr_interior
    }

    /// Returns a safe upper bound on the metadata blocks the next
    /// [`insert`](Self::insert) plus the per-chunk work that follows it will
    /// capture when the tree ends up holding `projected_extents` extents — the
    /// whole-tree reserialize's external-node writes plus the filesystem's
    /// bitmap/GDT/superblock charge, AND the inode-descriptor writeback /
    /// convert-to-written that ride the same chunk transaction
    /// ([`Ext4::chunk_insert_credits`]).
    ///
    /// The write spine's [`ensure_allocated_chunk`](super::ExtentManager::ensure_allocated_chunk)
    /// early stop compares this against the handle's transaction headroom: when
    /// the next insert will not fit even after growing in place, it stops with
    /// the progress made so far and reports this bound so the OUTER spine
    /// restarts onto a fresh transaction reserving exactly it (the restart
    /// cannot run under the ExtentTree lock). Reporting the same bound the
    /// reservation was checked against — not the smaller per-chunk
    /// `write_credits` estimate — is what keeps the restarted transaction able
    /// to hold the insert that did not fit.
    pub(super) fn next_insert_credit_bound(fs: &Ext4, projected_extents: usize) -> usize {
        fs.chunk_insert_credits(Self::external_node_count(projected_extents))
    }
}

/// Decodes leaf entry `i` of the trusted inline root.
fn root_extent_at(root_bytes: &[u8], i: usize) -> Extent {
    let off = ENTRY_SIZE * (1 + i);
    Extent::from(&RawExtent::from_bytes(&root_bytes[off..off + ENTRY_SIZE]))
}

/// Decodes index entry `i` of the trusted inline root.
fn root_index_at(root_bytes: &[u8], i: usize) -> ExtentIdx {
    let off = ENTRY_SIZE * (1 + i);
    ExtentIdx::from(&RawExtentIdx::from_bytes(
        &root_bytes[off..off + ENTRY_SIZE],
    ))
}

/// Where a leaf scan landed, before the path is attached.
enum LeafLanding {
    Covered(Extent),
    Gap(Option<Extent>),
}

impl LeafLanding {
    fn into_search(self, path: ExtentPath) -> Search {
        match self {
            LeafLanding::Covered(extent) => Search::Covered { path, extent },
            LeafLanding::Gap(prev) => Search::Gap { path, prev },
        }
    }
}

/// Resolves a leaf scan into its landing position and verdict: `chosen` is the
/// last entry with first block `<= iblock` (`None` = before the first entry).
/// The returned position is the covering entry on a hit, or the insertion
/// point a new entry keyed at `iblock` would take on a miss.
fn leaf_landing(
    chosen: Option<usize>,
    extent_at_fn: impl Fn(usize) -> Extent,
    iblock: Iblock,
) -> (usize, LeafLanding) {
    match chosen {
        None => (0, LeafLanding::Gap(None)),
        Some(i) => {
            let e = extent_at_fn(i);
            if e.covers(iblock) {
                (i, LeafLanding::Covered(e))
            } else {
                (i + 1, LeafLanding::Gap(Some(e)))
            }
        }
    }
}

/// The recursive child step of [`ExtentTree::walk_range`]: visits the extents
/// of the subtree rooted at `bid` that overlap `range`, in ascending order.
/// `expected_depth` enforces the one-step-down invariant ([`ExtentTree::find`]),
/// which also bounds the recursion at [`MAX_DEPTH`](MAX_DEPTH).
fn walk_child(
    fs: &Ext4,
    bid: Ext4Bid,
    expected_depth: u16,
    range: &Range<u64>,
    visit_fn: &mut impl FnMut(&Extent) -> ControlFlow<()>,
) -> Result<ControlFlow<()>> {
    let node = NodeBuf::read(fs, bid)?;
    if node.depth() != expected_depth {
        return_errno_with_message!(
            Errno::EUCLEAN,
            "extent child depth does not step down by one"
        );
    }
    if node.is_leaf() {
        for i in 0..node.entries() {
            let e = node.extent_at(i);
            if e.block() as u64 >= range.end {
                break;
            }
            if e.block() as u64 + e.len() as u64 > range.start && visit_fn(&e).is_break() {
                return Ok(ControlFlow::Break(()));
            }
        }
        return Ok(ControlFlow::Continue(()));
    }
    // `range.start` fits an `Iblock` (walk_range early-returns otherwise) and
    // only shrinks along the recursion.
    let start_key = range.start as Iblock;
    let first = node.index_pos(start_key).unwrap_or(0);
    for i in first..node.entries() {
        let child = node.index_at(i);
        if child.block() as u64 >= range.end {
            break;
        }
        if walk_child(fs, child.leaf(), expected_depth - 1, range, visit_fn)?.is_break() {
            return Ok(ControlFlow::Break(()));
        }
    }
    Ok(ControlFlow::Continue(()))
}

/// The outcome of searching a single extent-tree node for `iblock` — retained
/// (with the linear walker below) as the ktest cross-verification reference
/// for the path-based [`ExtentTree::find`].
#[cfg(ktest)]
enum Step {
    /// A leaf extent that covers `iblock`.
    Found(Extent),
    /// No extent covers `iblock`: a hole.
    Hole,
    /// An interior node points to a child at this physical block.
    Descend(Ext4Bid),
}

/// Parses and searches one freshly read (untrusted) node — the parse boundary
/// for device bytes.
#[cfg(ktest)]
fn search_node(bytes: &[u8], iblock: Iblock) -> Result<Step> {
    let header = ExtentHeader::try_from(&RawExtentHeader::from_bytes(&bytes[0..ENTRY_SIZE]))?;
    search_entries(&header, bytes, iblock)
}

/// Searches one node's entries for `iblock`, `header` already decoded.
///
/// Entries are sorted by logical block, so the covering entry is the last one
/// whose starting block is `<= iblock`.
#[cfg(ktest)]
fn search_entries(header: &ExtentHeader, bytes: &[u8], iblock: Iblock) -> Result<Step> {
    let nr_entries = header.entries() as usize;

    let entries_end = ENTRY_SIZE * (1 + nr_entries);
    if entries_end > bytes.len() {
        return_errno_with_message!(Errno::EUCLEAN, "extent node entries overrun node");
    }

    if header.is_leaf() {
        let mut covering: Option<Extent> = None;
        for i in 0..nr_entries {
            let off = ENTRY_SIZE * (1 + i);
            let extent = Extent::from(&RawExtent::from_bytes(&bytes[off..off + ENTRY_SIZE]));
            if extent.block() <= iblock {
                covering = Some(extent);
            } else {
                break;
            }
        }
        match covering {
            Some(extent) if extent.covers(iblock) => Ok(Step::Found(extent)),
            _ => Ok(Step::Hole),
        }
    } else {
        let mut chosen: Option<ExtentIdx> = None;
        for i in 0..nr_entries {
            let off = ENTRY_SIZE * (1 + i);
            let idx = ExtentIdx::from(&RawExtentIdx::from_bytes(&bytes[off..off + ENTRY_SIZE]));
            if idx.block() <= iblock {
                chosen = Some(idx);
            } else {
                break;
            }
        }
        match chosen {
            Some(idx) => Ok(Step::Descend(idx.leaf())),
            // `iblock` lies before the first index entry. Linux descends into
            // the first child anyway and then finds no covering extent; we
            // short-circuit to the same answer. On a well-formed tree the two
            // are equivalent (no leaf under index 0 maps anything below its
            // `first_block`); on a corrupt tree the short-circuit is the safer
            // degradation — a hole read instead of chasing a bogus subtree
            // (P1 review item, judged & documented at P5).
            None => Ok(Step::Hole),
        }
    }
}

/// Parses one freshly read (untrusted) external leaf node into its extents —
/// the parse boundary for a depth-0 node's device bytes. Rejects a non-leaf
/// header or an entry count that overruns the block.
fn parse_leaf_node(block: &[u8]) -> Result<Vec<Extent>> {
    let header = ExtentHeader::try_from(&RawExtentHeader::from_bytes(&block[0..ENTRY_SIZE]))?;
    if !header.is_leaf() {
        return_errno_with_message!(Errno::EUCLEAN, "external extent leaf is not a leaf node");
    }
    let nr = header.entries() as usize;
    if ENTRY_SIZE * (1 + nr) > block.len() {
        return_errno_with_message!(Errno::EUCLEAN, "extent leaf entries overrun node");
    }
    let mut extents = Vec::with_capacity(nr);
    for i in 0..nr {
        let off = ENTRY_SIZE * (1 + i);
        extents.push(Extent::from(&RawExtent::from_bytes(
            &block[off..off + ENTRY_SIZE],
        )));
    }
    Ok(extents)
}

/// Parses one freshly read (untrusted) external interior node (a depth-2 tree's
/// middle level) into its child leaf-block ids — the parse boundary for a
/// depth-1 node's device bytes. Rejects a node that is not a depth-1 interior or
/// an entry count that overruns the block.
fn parse_interior_node(block: &[u8]) -> Result<Vec<Ext4Bid>> {
    let header = ExtentHeader::try_from(&RawExtentHeader::from_bytes(&block[0..ENTRY_SIZE]))?;
    if header.is_leaf() || header.depth() != 1 {
        return_errno_with_message!(
            Errno::EUCLEAN,
            "depth-2 child is not a depth-1 interior node"
        );
    }
    let nr = header.entries() as usize;
    if ENTRY_SIZE * (1 + nr) > block.len() {
        return_errno_with_message!(Errno::EUCLEAN, "extent index entries overrun node");
    }
    let mut leaf_bids = Vec::with_capacity(nr);
    for i in 0..nr {
        let off = ENTRY_SIZE * (1 + i);
        leaf_bids
            .push(ExtentIdx::from(&RawExtentIdx::from_bytes(&block[off..off + ENTRY_SIZE])).leaf());
    }
    Ok(leaf_bids)
}

/// The metadata (index/leaf) blocks a tree mutation allocated and freed;
/// consumed internally by the mutators' `i_blocks` accounting.
struct TreeDelta {
    meta_allocated: u32,
    meta_freed: u32,
}

/// Acquires `count` metadata blocks for an external-node rebuild: reuses the
/// front of `pool` (surviving blocks the mutation will overwrite in place) and
/// allocates the shortfall. On an allocation error the freshly allocated blocks
/// are freed before returning, so no metadata leaks.
///
/// Returns the full block list (`reuse` reused blocks followed by the fresh
/// ones) and, separately, just the freshly allocated blocks — the caller frees
/// those if a later node write fails, since the in-memory root has not yet been
/// pointed at the new layout.
/// Whether the truncate chunk must stop before the next free: `true` when the
/// handle cannot reserve `need` more credits (one free plus the survivor
/// reserialize) in its current transaction even after growing in place. A
/// wait-free probe under the ExtentTree lock ③ (never restarts here — the
/// restart's re-admission may wait, illegal under this lock); the OUTER spine
/// restarts with ③ released.
fn stop_before_free(handle: &journal::Handle, need: usize) -> Result<bool> {
    Ok(journal::try_reserve_next(handle, need)? == journal::ExtendOutcome::NeedsRestart)
}

fn acquire_meta_blocks(
    fs: &Ext4,
    pool: &[Ext4Bid],
    count: usize,
    goal: Ext4Bid,
    handle: Option<&journal::Handle>,
) -> Result<(Vec<Ext4Bid>, Vec<Ext4Bid>)> {
    let reuse = count.min(pool.len());
    let mut blocks: Vec<Ext4Bid> = pool[..reuse].to_vec();
    let mut newly_allocated: Vec<Ext4Bid> = Vec::new();
    for _ in reuse..count {
        match alloc_meta_block(fs, goal, handle) {
            Ok(bid) => newly_allocated.push(bid),
            Err(err) => {
                rollback_meta_blocks(fs, &newly_allocated, handle);
                return Err(err);
            }
        }
    }
    blocks.extend_from_slice(&newly_allocated);
    Ok((blocks, newly_allocated))
}

/// Frees blocks allocated during a rebuild that then failed, on a best-effort
/// basis (the mutation is already returning an error).
fn rollback_meta_blocks(fs: &Ext4, blocks: &[Ext4Bid], handle: Option<&journal::Handle>) {
    for &bid in blocks {
        let _ = free_meta_block(fs, bid, handle);
    }
}

/// Builds an index entry keyed by first logical block `block`, pointing at the
/// child node at physical block `child_bid`.
fn make_index_entry(block: Iblock, child_bid: Ext4Bid) -> RawExtentIdx {
    // 48-bit on-disk cap (see `RawExtent::from`); lossless while the no-64bit
    // mount invariant bounds bids below 2^32.
    debug_assert!(child_bid < 1 << 48);
    RawExtentIdx {
        block,
        leaf_lo: child_bid as u32,
        leaf_hi: (child_bid >> 32) as u16,
        unused: 0,
    }
}

/// Allocates one metadata block for an external extent-tree node.
fn alloc_meta_block(fs: &Ext4, goal: Ext4Bid, handle: Option<&journal::Handle>) -> Result<Ext4Bid> {
    let range = fs.alloc_blocks(1, goal, handle)?;
    let bid = range.start;
    // Zero-seed the fresh block's capture now; the capture lives in the
    // running transaction (the credential is proof, not owner), and
    // `write_leaf_node` re-mints its own when it fills the block.
    //
    // If seeding the capture fails (e.g. the transaction is at its credit
    // ceiling), free the block before returning: it is allocated (its bitmap
    // bit set and captured) but referenced by nothing, and the callers' own
    // rollbacks only cover the bids they received — an unfreed one leaks until
    // the next e2fsck. The free rides the same handle (its bitmap after-image
    // is already captured, so it costs no new credit).
    if let Err(err) = journal::get_create_access(handle, bid) {
        let _ = fs.free_blocks(journal::BlockFreeAuth::without_revoke_duty(bid, 1), handle);
        return Err(err);
    }
    Ok(bid)
}

/// Frees one external extent-tree metadata block — the textbook revoke case
/// (Linux `ext4_ext_rm_idx` frees tree nodes with `METADATA | FORGET`,
/// fs/ext4/extents.c:2332). The mint is pure; `free_blocks` discharges the
/// forget effects immediately before the bitmap clear, under the same
/// handle, so the revoke record and the clear commit together — and a
/// rebuild that errors before reaching this block's free leaves its journal
/// state untouched (no revoke for a still-referenced node).
fn free_meta_block(fs: &Ext4, bid: Ext4Bid, handle: Option<&journal::Handle>) -> Result<()> {
    fs.free_blocks(journal::forget(bid, 1), handle)
}

/// Byte offset of `et_checksum` in a full-block external extent node: a 4-byte
/// tail after the header and `LEAF_MAX`/`INTERIOR_MAX` (== 340) entries. Both
/// node kinds write `eh_max = 340`, so Linux's `EXT4_EXTENT_TAIL_OFFSET`
/// (`12 * (1 + eh_max)`) is fixed at this offset.
const EXTENT_TAIL_OFFSET: usize = ENTRY_SIZE * (1 + LEAF_MAX);

/// Stamps the `metadata_csum` extent-block tail (Linux `ext4_extent_block_csum`):
/// crc32c of the node up to the tail, seeded with the owning inode's seed. Only
/// external (full-block) leaf/interior nodes carry this tail; the inline root is
/// covered by the inode checksum instead.
pub(super) fn stamp_extent_tail(block: &mut [u8], seed: InodeCsumSeed) {
    let csum = checksum::crc32c(seed.get(), &block[..EXTENT_TAIL_OFFSET]);
    block[EXTENT_TAIL_OFFSET..EXTENT_TAIL_OFFSET + size_of::<u32>()]
        .copy_from_slice(&csum.to_le_bytes());
}

/// Serializes `extents` into a full-block external leaf node at `bid`.
fn write_leaf_node(
    device: &dyn BlockDevice,
    bid: Ext4Bid,
    extents: &[Extent],
    handle: Option<&journal::Handle>,
    csum_seed: Option<InodeCsumSeed>,
) -> Result<()> {
    let mut block = [0u8; BLOCK_SIZE];
    let header = RawExtentHeader {
        magic: EXTENT_MAGIC,
        entries: extents.len() as u16,
        max: LEAF_MAX as u16,
        depth: 0,
        generation: 0,
    };
    block[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
    for (i, ext) in extents.iter().enumerate() {
        let off = ENTRY_SIZE * (1 + i);
        block[off..off + ENTRY_SIZE].copy_from_slice(RawExtent::from(ext).as_bytes());
    }
    // With `metadata_csum`, stamp the extent-block tail over the finished node
    // (header + entries) before it is captured/written; a `None` seed (feature
    // off) leaves the block byte-for-byte identical.
    if let Some(seed) = csum_seed {
        stamp_extent_tail(&mut block, seed);
    }
    // The leaf may be REUSED from the previous tree layout (`reserialize`
    // re-fills surviving external leaves in place); only freshly allocated
    // leaves were captured by `alloc_meta_block`. Capture idempotently so the
    // reuse path is journaled too — a fresh leaf's zero-seeded capture is left
    // untouched, a reused leaf gets one here.
    let access = journal::get_write_access(handle, bid)?;
    access.patch(|buf| buf.copy_from_slice(&block))?;
    // Under a live capture the extent block reaches its final location via
    // checkpoint after the transaction commits; suppress the direct write so
    // metadata never precedes its commit (WAL). See
    // `fs::Ext4::write_back_inode_desc`.
    if !access.is_live() {
        device.write_val(Bid::new(bid).to_offset(), &block)?;
    }
    Ok(())
}

/// Serializes `idx_entries` into a full-block external interior (depth-1) node
/// at `bid` — the middle level of a depth-2 tree, whose entries point at leaf
/// blocks. Mirrors [`write_leaf_node`], including its journal-capture idempotency
/// (a block reused as an interior may have been a leaf, and vice versa; the whole
/// block is overwritten either way).
fn write_interior_node(
    device: &dyn BlockDevice,
    bid: Ext4Bid,
    idx_entries: &[RawExtentIdx],
    handle: Option<&journal::Handle>,
    csum_seed: Option<InodeCsumSeed>,
) -> Result<()> {
    let mut block = [0u8; BLOCK_SIZE];
    let header = RawExtentHeader {
        magic: EXTENT_MAGIC,
        entries: idx_entries.len() as u16,
        max: INTERIOR_MAX as u16,
        depth: 1,
        generation: 0,
    };
    block[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
    for (i, idx) in idx_entries.iter().enumerate() {
        let off = ENTRY_SIZE * (1 + i);
        block[off..off + ENTRY_SIZE].copy_from_slice(idx.as_bytes());
    }
    // Stamp the extent-block tail with `metadata_csum` on (see `write_leaf_node`);
    // a `None` seed leaves the block unchanged.
    if let Some(seed) = csum_seed {
        stamp_extent_tail(&mut block, seed);
    }
    // Capture idempotently: a reused block gets its capture here, a freshly
    // allocated one keeps the zero-seeded capture from `alloc_meta_block`.
    let access = journal::get_write_access(handle, bid)?;
    access.patch(|buf| buf.copy_from_slice(&block))?;
    // Under a live capture the block reaches its final location via checkpoint
    // after commit; suppress the direct write so metadata never precedes its
    // commit (WAL). See `write_leaf_node`.
    if !access.is_live() {
        device.write_val(Bid::new(bid).to_offset(), &block)?;
    }
    Ok(())
}

/// Returns whether `right` can coalesce onto the end of `left`: logically and
/// physically contiguous, the same written/unwritten state, and the combined
/// length still encodable. Unwritten extents cap one below the written limit:
/// the length is bias-encoded as `len + MAX_WRITTEN_LEN`, so an unwritten run
/// of `MAX_WRITTEN_LEN` would overflow `ee_len` (Linux uses the distinct
/// `EXT_UNWRITTEN_MAX_LEN = 32767`).
fn can_merge(left: &Extent, right: &Extent) -> bool {
    let max_len = if left.is_unwritten() {
        MAX_WRITTEN_LEN as u32 - 1
    } else {
        MAX_WRITTEN_LEN as u32
    };
    left.block() as u64 + left.len() as u64 == right.block() as u64
        && left.start() + left.len() as u64 == right.start()
        && left.is_unwritten() == right.is_unwritten()
        && left.len() as u32 + right.len() as u32 <= max_len
}

/// Coalesces `left` and `right`; caller proved [`can_merge`].
fn merged_pair(left: &Extent, right: &Extent) -> Extent {
    Extent::new(
        left.block(),
        left.len() + right.len(),
        left.start(),
        left.kind(),
    )
}

/// Sorts `extents` by logical block and coalesces runs that are logically and
/// physically contiguous and share the same written/unwritten state.
fn merge_extents(extents: &mut Vec<Extent>) {
    extents.sort_by_key(|e| e.block());
    let mut merged: Vec<Extent> = Vec::with_capacity(extents.len());
    for e in extents.iter() {
        if let Some(last) = merged.last().copied()
            && can_merge(&last, e)
        {
            *merged.last_mut().unwrap() = merged_pair(&last, e);
            continue;
        }
        merged.push(*e);
    }
    *extents = merged;
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;
    use crate::fs::fs_impls::ext4::test_utils::Ext4FixtureBuilder;

    /// Builds a validated tree over a depth-0 root (header + extents).
    fn inline_tree(extents: &[RawExtent]) -> ExtentTree {
        let mut block = [0u32; RAW_BLOCK_PTRS_LEN];
        let bytes = block.as_mut_bytes();
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: extents.len() as u16,
            max: 4,
            depth: 0,
            generation: 0,
        };
        bytes[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        for (i, extent) in extents.iter().enumerate() {
            let off = ENTRY_SIZE * (1 + i);
            bytes[off..off + ENTRY_SIZE].copy_from_slice(extent.as_bytes());
        }
        ExtentTree::try_new(block, 0).unwrap()
    }

    #[ktest]
    fn inline_single_extent_lookup() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        // One extent mapping logical 0..4 to physical 100..104.
        let tree = inline_tree(&[RawExtent {
            block: 0,
            len: 4,
            start_hi: 0,
            start_lo: 100,
        }]);

        let mapped = tree.lookup(&f.ext4, 2).unwrap().unwrap();
        assert_eq!(mapped.start(), 100);
        assert_eq!(mapped.block(), 0);

        // Block 4 is beyond the extent: a hole.
        assert!(tree.lookup(&f.ext4, 4).unwrap().is_none());
    }

    #[ktest]
    fn inline_multiple_extents_lookup() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let tree = inline_tree(&[
            RawExtent {
                block: 0,
                len: 2,
                start_hi: 0,
                start_lo: 200,
            },
            RawExtent {
                block: 5,
                len: 3,
                start_hi: 0,
                start_lo: 300,
            },
        ]);

        // Logical 6 → second extent, physical 300 + (6 - 5) = 301.
        let mapped = tree.lookup(&f.ext4, 6).unwrap().unwrap();
        assert_eq!(mapped.start() + (6 - mapped.block()) as u64, 301);

        // Logical 3 falls in the gap between the two extents: a hole.
        assert!(tree.lookup(&f.ext4, 3).unwrap().is_none());
    }

    #[ktest]
    fn empty_root_is_all_holes() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let tree = ExtentTree::empty();
        assert!(tree.lookup(&f.ext4, 0).unwrap().is_none());
    }

    #[ktest]
    fn rejects_bad_root_magic() {
        let root = [0u32; RAW_BLOCK_PTRS_LEN];
        assert!(ExtentTree::try_new(root, 0).is_err());
    }

    /// Builds a validated tree over a depth-1 index root pointing at a single
    /// external leaf node at physical block `leaf_block`.
    fn index_tree(leaf_block: u32) -> ExtentTree {
        let mut block = [0u32; RAW_BLOCK_PTRS_LEN];
        let bytes = block.as_mut_bytes();
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: 1,
            max: 4,
            depth: 1,
            generation: 0,
        };
        bytes[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        let idx = RawExtentIdx {
            block: 0,
            leaf_lo: leaf_block,
            leaf_hi: 0,
            unused: 0,
        };
        bytes[ENTRY_SIZE..2 * ENTRY_SIZE].copy_from_slice(idx.as_bytes());
        ExtentTree::try_new(block, 0).unwrap()
    }

    /// Builds a full-block external leaf node (depth 0) from `extents`.
    fn leaf_node(extents: &[RawExtent]) -> [u8; BLOCK_SIZE] {
        let mut block = [0u8; BLOCK_SIZE];
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: extents.len() as u16,
            max: ((BLOCK_SIZE / ENTRY_SIZE) - 1) as u16,
            depth: 0,
            generation: 0,
        };
        block[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        for (i, extent) in extents.iter().enumerate() {
            let off = ENTRY_SIZE * (1 + i);
            block[off..off + ENTRY_SIZE].copy_from_slice(extent.as_bytes());
        }
        block
    }

    /// A depth-1 tree (index root → external leaf read from the device) must be
    /// descended into. This exercises the interior-node read path that inline
    /// (depth-0) roots never reach — the real-image counterpart is a fragmented
    /// file whose extents overflow the inline root.
    #[ktest]
    fn descends_into_external_leaf() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();

        let leaf_block = 200u32;
        let leaf = leaf_node(&[
            RawExtent {
                block: 0,
                len: 2,
                start_hi: 0,
                start_lo: 300,
            },
            RawExtent {
                block: 5,
                len: 3,
                start_hi: 0,
                start_lo: 400,
            },
        ]);
        f.write_data_block(leaf_block, &leaf);
        let tree = index_tree(leaf_block);

        // Logical 1 → descend to the leaf → first extent (0..2) → physical 301.
        let m0 = tree.lookup(&f.ext4, 1).unwrap().unwrap();
        assert_eq!(m0.start() + (1 - m0.block()) as u64, 301);
        // Logical 6 → second extent (5..8) → physical 401.
        let m1 = tree.lookup(&f.ext4, 6).unwrap().unwrap();
        assert_eq!(m1.start() + (6 - m1.block()) as u64, 401);
        // Logical 3 → gap between the leaf's extents → hole.
        assert!(tree.lookup(&f.ext4, 3).unwrap().is_none());
        // Logical 100 → beyond all extents → hole.
        assert!(tree.lookup(&f.ext4, 100).unwrap().is_none());
    }

    /// Returns the entry count of the root header.
    fn root_entries(tree: &ExtentTree) -> u16 {
        ExtentHeader::try_from(&RawExtentHeader::from_bytes(
            &tree.root_bytes().as_bytes()[0..ENTRY_SIZE],
        ))
        .unwrap()
        .entries()
    }

    #[ktest]
    fn insert_into_inline_merges_contiguous() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();

        // [0,2) -> 100, then contiguous [2,2) -> 102 must coalesce into [0,4).
        tree.insert(&f.ext4, 0, 100, 2, ExtentKind::Written, None, None)
            .unwrap();
        // Pure data growth: no metadata block was needed.
        assert_eq!(tree.sector_count(), 2 * SECTORS_PER_BLOCK);
        tree.insert(&f.ext4, 2, 102, 2, ExtentKind::Written, None, None)
            .unwrap();

        // Still inline depth-0 with a single merged extent.
        assert_eq!((tree.depth(), root_entries(&tree)), (0, 1));
        assert_eq!(tree.sector_count(), 4 * SECTORS_PER_BLOCK);
        assert!(tree.is_dirty());
        let m = tree.lookup(&f.ext4, 3).unwrap().unwrap();
        assert_eq!(m.start() + (3 - m.block()) as u64, 103);
    }

    #[ktest]
    fn insert_non_contiguous_stays_separate() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();

        tree.insert(&f.ext4, 0, 100, 1, ExtentKind::Written, None, None)
            .unwrap();
        tree.insert(&f.ext4, 5, 200, 1, ExtentKind::Written, None, None)
            .unwrap();

        assert_eq!((tree.depth(), root_entries(&tree)), (0, 2));
        assert_eq!(tree.lookup(&f.ext4, 0).unwrap().unwrap().start(), 100);
        assert_eq!(tree.lookup(&f.ext4, 5).unwrap().unwrap().start(), 200);
        assert!(tree.lookup(&f.ext4, 3).unwrap().is_none());
    }

    #[ktest]
    fn inline_overflow_grows_to_depth1() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();

        // Five non-contiguous extents overflow the 4-entry inline root.
        for k in 0..5u32 {
            tree.insert(
                &f.ext4,
                k * 2,
                100 + k as u64 * 10,
                1,
                ExtentKind::Written,
                None,
                None,
            )
            .unwrap();
        }

        // The root is now a depth-1 index with one external leaf, and
        // `i_blocks` counts the 5 data blocks plus exactly 1 leaf block.
        assert_eq!((tree.depth(), root_entries(&tree)), (1, 1));
        assert_eq!(tree.sector_count(), (5 + 1) * SECTORS_PER_BLOCK);

        // All five mappings are still reachable through the external leaf.
        for k in 0..5u32 {
            let m = tree.lookup(&f.ext4, k * 2).unwrap().unwrap();
            assert_eq!(m.start(), 100 + k as u64 * 10);
        }

        // The allocated leaf block is marked in the block bitmap (e2fsck-clean).
        let leaf_bid = ExtentIdx::from(&RawExtentIdx::from_bytes(
            &tree.root_bytes().as_bytes()[ENTRY_SIZE..2 * ENTRY_SIZE],
        ))
        .leaf();
        let group = f.ext4.block_group(0);
        let metadata = group.metadata();
        assert!(
            metadata
                .block_bitmap
                .is_allocated((leaf_bid - group.first_block()) as u16)
        );
    }

    #[ktest]
    fn insert_into_depth1_reuses_leaf() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        for k in 0..5u32 {
            tree.insert(
                &f.ext4,
                k * 2,
                100 + k as u64 * 10,
                1,
                ExtentKind::Written,
                None,
                None,
            )
            .unwrap();
        }
        assert_eq!(tree.depth(), 1);
        let sectors_before = tree.sector_count();

        // A sixth extent fits the existing leaf: no new metadata block, so
        // `i_blocks` grows by the data block only.
        tree.insert(&f.ext4, 20, 500, 1, ExtentKind::Written, None, None)
            .unwrap();
        assert_eq!(tree.sector_count(), sectors_before + SECTORS_PER_BLOCK);

        assert_eq!(tree.lookup(&f.ext4, 20).unwrap().unwrap().start(), 500);
    }

    /// A file fragmented past the depth-1 capacity (`INLINE_MAX × LEAF_MAX` =
    /// 4 × 340 = 1360 extents) must grow a *depth-2* tree (root → interior node →
    /// leaf nodes), and every mapping must stay reachable through both index
    /// levels. Stride-2 single-block extents never coalesce, so N inserts yield N
    /// distinct extents. 1400 extents need 5 leaves (> `INLINE_MAX`), overflowing
    /// the inline root into one interior node holding 5 leaf-index entries.
    #[ktest]
    fn overflow_past_depth1_grows_to_depth2() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();

        const N: u32 = 1400;
        const DATA_BASE: Ext4Bid = 100_000;
        for k in 0..N {
            tree.insert(
                &f.ext4,
                k * 2,
                DATA_BASE + k as Ext4Bid,
                1,
                ExtentKind::Written,
                None,
                None,
            )
            .unwrap();
        }

        // The inline root is now a depth-2 index.
        assert_eq!(tree.depth(), 2);

        // Every mapping is reachable, spot-checked across all five leaf blocks
        // (each leaf holds 340 extents: 0..340, 340..680, 680..1020, 1020..1360,
        // 1360..1400). The stride-1 gap after each extent stays a hole.
        for k in [0u32, 339, 340, 680, 1020, 1360, 1399] {
            let m = tree.lookup(&f.ext4, k * 2).unwrap().unwrap();
            assert_eq!(m.block(), k * 2);
            assert_eq!(m.start(), DATA_BASE + k as Ext4Bid);
            assert!(tree.lookup(&f.ext4, k * 2 + 1).unwrap().is_none());
        }

        // `i_blocks` counts the N data blocks plus the tree metadata: 5 leaves +
        // 1 interior = 6 blocks. The pure-growth sequence never frees, so the
        // total is exact — and trivially non-negative and above the data count.
        const NR_META: u64 = 6;
        assert_eq!(
            tree.sector_count(),
            (N as u64 + NR_META) * SECTORS_PER_BLOCK
        );
    }

    /// Regression: two contiguous *unwritten* extents whose lengths sum to
    /// `MAX_WRITTEN_LEN` (32768) must NOT coalesce — an unwritten `ee_len` of
    /// 32768 overflows the bias encoding (`len + MAX_WRITTEN_LEN`), so an
    /// unwritten run caps at `EXT_UNWRITTEN_MAX_LEN = 32767`. Written runs of the
    /// same shape may still merge to 32768.
    #[ktest]
    fn merge_caps_unwritten_below_max_len() {
        let half = MAX_WRITTEN_LEN / 2; // 16384

        let mut unwritten = vec![
            Extent::new(0, half, 100, ExtentKind::Unwritten),
            Extent::new(
                half as Iblock,
                half,
                100 + half as Ext4Bid,
                ExtentKind::Unwritten,
            ),
        ];
        merge_extents(&mut unwritten);
        for e in &unwritten {
            assert!(!e.is_unwritten() || e.len() < MAX_WRITTEN_LEN);
        }

        let mut written = vec![
            Extent::new(0, half, 200, ExtentKind::Written),
            Extent::new(
                half as Iblock,
                half,
                200 + half as Ext4Bid,
                ExtentKind::Written,
            ),
        ];
        merge_extents(&mut written);
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].len(), MAX_WRITTEN_LEN);
    }

    /// `stamp_extent_tail` writes crc32c of the node's first `EXTENT_TAIL_OFFSET`
    /// bytes into the 4-byte tail, and any change to the covered region changes
    /// the stamp — the `metadata_csum` extent-block invariant (Linux
    /// `ext4_extent_block_csum`).
    #[ktest]
    fn stamp_extent_tail_matches_crc32c() {
        let seed = checksum::FsCsumSeed::new(0x1357_9bdf).derive_inode(1, 0);
        let mut block = [0u8; BLOCK_SIZE];
        for (i, b) in block.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        // The tail is excluded from its own cover; zero it before stamping.
        block[EXTENT_TAIL_OFFSET..EXTENT_TAIL_OFFSET + 4].fill(0);

        stamp_extent_tail(&mut block, seed);
        let stored = u32::from_le_bytes(
            block[EXTENT_TAIL_OFFSET..EXTENT_TAIL_OFFSET + 4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(
            stored,
            checksum::crc32c(seed.get(), &block[..EXTENT_TAIL_OFFSET])
        );

        // A single-byte change to the covered node re-stamps to a new value.
        block[0] ^= 0xFF;
        stamp_extent_tail(&mut block, seed);
        let restamped = u32::from_le_bytes(
            block[EXTENT_TAIL_OFFSET..EXTENT_TAIL_OFFSET + 4]
                .try_into()
                .unwrap(),
        );
        assert_ne!(stored, restamped);
    }

    /// End-to-end: a seeded depth-1 rebuild writes its external leaf to the
    /// device with a correct extent-block tail checksum; a `None` seed leaves the
    /// tail zeroed. Exercises the `write_leaf_node` compute-on-write hook.
    #[ktest]
    fn seeded_reserialize_stamps_leaf_tail() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();

        let seed = checksum::FsCsumSeed::new(0x0bad_c0de).derive_inode(1, 0);
        let mut tree = ExtentTree::empty();
        // Five disjoint single-block extents overflow the inline root into one
        // external leaf (depth 1).
        for k in 0..5u32 {
            tree.insert(
                &f.ext4,
                k * 2,
                100 + k as Ext4Bid,
                1,
                ExtentKind::Written,
                None,
                Some(seed),
            )
            .unwrap();
        }
        assert_eq!(tree.depth(), 1);

        // The leaf block id lives in the root's single index entry.
        let leaf_bid = ExtentIdx::from(&RawExtentIdx::from_bytes(
            &tree.root_bytes().as_bytes()[ENTRY_SIZE..2 * ENTRY_SIZE],
        ))
        .leaf();

        // No journal handle was used, so the leaf was written straight to disk.
        let mut block = [0u8; BLOCK_SIZE];
        f.disk
            .segment()
            .read_bytes(leaf_bid as usize * BLOCK_SIZE, &mut block)
            .unwrap();
        let stored = u32::from_le_bytes(
            block[EXTENT_TAIL_OFFSET..EXTENT_TAIL_OFFSET + 4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(
            stored,
            checksum::crc32c(seed.get(), &block[..EXTENT_TAIL_OFFSET])
        );
    }

    // ---- P9a-T1: path 手术读侧（find / walk_range）互证与语义钉 ----

    /// `find` must agree with the pre-surgery linear walker on every probe,
    /// and `walk_range` over the whole space must reproduce the flatten list,
    /// at every tree shape from inline through depth-2 — the path-based read
    /// side is only trusted through this equivalence.
    #[ktest]
    fn find_and_walk_match_linear_reference_across_shapes() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();

        // 1450 single-block extents with one-block gaps (unmergeable) force
        // growth through inline → depth-1 → depth-2 (5 leaves + 1 interior).
        // Checkpoints along the way exercise each shape; unwritten kind every
        // third extent exercises kind fidelity.
        let checkpoints = [3usize, 4, 300, 1400, 1450];
        let mut inserted = 0usize;
        for &target in &checkpoints {
            while inserted < target {
                let i = inserted as u32;
                let kind = if i.is_multiple_of(3) {
                    ExtentKind::Unwritten
                } else {
                    ExtentKind::Written
                };
                tree.insert(
                    &f.ext4,
                    i * 2,
                    100_000 + i as Ext4Bid * 2,
                    1,
                    kind,
                    None,
                    None,
                )
                .unwrap();
                inserted += 1;
            }

            // Probe every logical block up to past the last extent.
            for ib in 0..(inserted as u32 * 2 + 4) {
                let linear = tree.lookup_linear(&f.ext4, ib).unwrap();
                match (linear, tree.find(&f.ext4, ib).unwrap()) {
                    (Some(l), Search::Covered { extent: e, .. }) => {
                        assert_eq!(
                            (l.block(), l.len(), l.start(), l.kind()),
                            (e.block(), e.len(), e.start(), e.kind())
                        );
                    }
                    (None, Search::Gap { .. }) => {}
                    (l, _) => panic!("find/linear disagree at block {ib} (linear: {l:?})"),
                }
            }

            // walk_range over everything == flatten, order and fields.
            let (mut flat, _) = tree.flatten(&f.ext4).unwrap();
            flat.sort_by_key(|e| e.block());
            let mut walked = Vec::new();
            tree.walk_range(&f.ext4, 0..u64::MAX, &mut |e| {
                walked.push(*e);
                ControlFlow::Continue(())
            })
            .unwrap();
            assert_eq!(flat.len(), walked.len());
            for (a, b) in flat.iter().zip(walked.iter()) {
                assert_eq!(
                    (a.block(), a.len(), a.start(), a.kind()),
                    (b.block(), b.len(), b.start(), b.kind())
                );
            }
        }
        assert_eq!(tree.depth(), 2);

        // Bounded walk: exactly the extents overlapping [101, 140) — blocks
        // are even, so extents 51..=69 qualify (block 102..=138).
        let mut seen = Vec::new();
        tree.walk_range(&f.ext4, 101..140, &mut |e| {
            seen.push(e.block());
            ControlFlow::Continue(())
        })
        .unwrap();
        let want: Vec<Iblock> = (51..70).map(|i| i * 2).collect();
        assert_eq!(seen, want);

        // Early break stops the walk mid-tree.
        let mut count = 0;
        tree.walk_range(&f.ext4, 0..u64::MAX, &mut |_| {
            count += 1;
            if count == 3 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .unwrap();
        assert_eq!(count, 3);
    }

    /// A gap's landing carries the insertion point and the in-leaf
    /// predecessor (the allocation-goal donor) — the rule-6 payload inserts
    /// consume without a second walk.
    #[ktest]
    fn find_gap_reports_insertion_point_and_prev() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();

        // Inline root: extents [0,2)→100 and [5,8)→400.
        let tree = inline_tree(&[
            RawExtent {
                block: 0,
                len: 2,
                start_hi: 0,
                start_lo: 100,
            },
            RawExtent {
                block: 5,
                len: 3,
                start_hi: 0,
                start_lo: 400,
            },
        ]);
        match tree.find(&f.ext4, 3).unwrap() {
            Search::Gap { path, prev } => {
                assert!(path.levels.is_empty());
                assert_eq!(path.root_pos, 1); // between the two entries
                assert_eq!(prev.unwrap().block(), 0);
            }
            _ => panic!("block 3 must be a gap"),
        }
        match tree.find(&f.ext4, 6).unwrap() {
            Search::Covered { path, extent } => {
                assert_eq!(path.root_pos, 1);
                assert_eq!(extent.block(), 5);
            }
            _ => panic!("block 6 must be covered"),
        }

        // External leaf via a depth-1 root: same landings, one path level.
        let leaf_block = 200u32;
        let leaf = leaf_node(&[
            RawExtent {
                block: 5,
                len: 3,
                start_hi: 0,
                start_lo: 400,
            },
            RawExtent {
                block: 20,
                len: 1,
                start_hi: 0,
                start_lo: 500,
            },
        ]);
        f.write_data_block(leaf_block, &leaf);
        let tree = index_tree(leaf_block);

        // Before the leaf's first entry: insertion point 0, no predecessor.
        match tree.find(&f.ext4, 2).unwrap() {
            Search::Gap { path, prev } => {
                assert_eq!(path.levels.len(), 1);
                assert_eq!(path.leaf().unwrap().pos, 0);
                assert!(prev.is_none());
            }
            _ => panic!("block 2 must be a gap"),
        }
        // Past the last entry: insertion point = entry count, prev = last.
        match tree.find(&f.ext4, 100).unwrap() {
            Search::Gap { path, prev } => {
                assert_eq!(path.leaf().unwrap().pos, 2);
                assert_eq!(prev.unwrap().block(), 20);
            }
            _ => panic!("block 100 must be a gap"),
        }
    }

    /// The path walker rejects structurally corrupt children loud: a child
    /// whose depth does not step down by one, and a node whose entry count
    /// overruns the block.
    #[ktest]
    fn find_rejects_corrupt_children() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();

        // A depth-1 root must point at leaves; hand it an interior node.
        let bogus_block = 210u32;
        let mut interior = [0u8; BLOCK_SIZE];
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: 0,
            max: INTERIOR_MAX as u16,
            depth: 1,
            generation: 0,
        };
        interior[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        f.write_data_block(bogus_block, &interior);
        let tree = index_tree(bogus_block);
        assert!(tree.find(&f.ext4, 0).is_err());
        let mut visited = 0;
        assert!(
            tree.walk_range(&f.ext4, 0..u64::MAX, &mut |_| {
                visited += 1;
                ControlFlow::Continue(())
            })
            .is_err()
        );
        assert_eq!(visited, 0);

        // An entry count that overruns the 4K node (forged max admits it past
        // the header check) must be rejected at the NodeBuf parse boundary.
        let overrun_block = 211u32;
        let mut overrun = [0u8; BLOCK_SIZE];
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: 400,
            max: 500,
            depth: 0,
            generation: 0,
        };
        overrun[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        f.write_data_block(overrun_block, &overrun);
        let tree = index_tree(overrun_block);
        assert!(tree.find(&f.ext4, 0).is_err());
    }

    /// The every-write convert gate must stay semantics-identical after the
    /// bounded-probe rewrite: a written-only range returns untouched (no
    /// dirtying, no rebuild), an unwritten overlap still converts.
    #[ktest]
    fn convert_gate_bounded_probe_keeps_semantics() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        tree.insert(&f.ext4, 0, 100, 4, ExtentKind::Written, None, None)
            .unwrap();
        tree.insert(&f.ext4, 10, 200, 2, ExtentKind::Unwritten, None, None)
            .unwrap();
        tree.clear_dirty();

        // Written-only range: the gate returns before any rebuild.
        tree.convert_unwritten(&f.ext4, 0, 4, None, None).unwrap();
        assert!(!tree.is_dirty());

        // Unwritten overlap: conversion still runs and splits at the range end.
        tree.convert_unwritten(&f.ext4, 10, 1, None, None).unwrap();
        assert!(tree.is_dirty());
        assert!(!tree.lookup(&f.ext4, 10).unwrap().unwrap().is_unwritten());
        assert!(tree.lookup(&f.ext4, 11).unwrap().unwrap().is_unwritten());
    }

    // ---- P9a-T2: insert 快路（原位叶编辑）----

    /// Decodes root index entry `i`'s key (test-side check of correct_indexes).
    fn root_index_key(tree: &ExtentTree, i: usize) -> Iblock {
        let bytes = tree.root_bytes().as_bytes();
        let off = ENTRY_SIZE * (1 + i);
        ExtentIdx::from(&RawExtentIdx::from_bytes(&bytes[off..off + ENTRY_SIZE])).block()
    }

    /// Probes every block in `0..limit` against the linear reference.
    fn assert_matches_linear(
        f: &crate::fs::fs_impls::ext4::test_utils::Ext4Fixture,
        tree: &ExtentTree,
        limit: Iblock,
    ) {
        for ib in 0..limit {
            let linear = tree.lookup_linear(&f.ext4, ib).unwrap();
            let new = tree.lookup(&f.ext4, ib).unwrap();
            match (linear, new) {
                (Some(l), Some(n)) => assert_eq!(
                    (l.block(), l.len(), l.start(), l.kind()),
                    (n.block(), n.len(), n.start(), n.kind()),
                    "mismatch at block {ib}"
                ),
                (None, None) => {}
                (l, n) => panic!("verdicts differ at block {ib}: linear {l:?} vs path {n:?}"),
            }
        }
    }

    /// The in-place insert fast path on an external leaf: mid-leaf shift
    /// insert, left merge, bridging absorb, and a position-0 insert that
    /// corrects the root index key — each leaving `i_blocks` with a pure-data
    /// delta (no metadata block churn, the fast-path signature) and the whole
    /// tree agreeing with the linear reference.
    #[ktest]
    fn insert_fast_path_edits_leaf_in_place() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();

        // Six unmergeable extents overflow the inline root → depth 1, one leaf.
        for (b, p) in [
            (10, 1000),
            (20, 2000),
            (30, 3000),
            (40, 4000),
            (50, 5000),
            (60, 6000),
        ] {
            tree.insert(&f.ext4, b, p, 2, ExtentKind::Written, None, None)
                .unwrap();
        }
        assert_eq!(tree.depth(), 1);

        // Mid-leaf shift insert (no merge partner): pure-data i_blocks delta.
        let sc = tree.sector_count();
        tree.insert(&f.ext4, 25, 9000, 2, ExtentKind::Written, None, None)
            .unwrap();
        assert_eq!(tree.sector_count(), sc + 2 * SECTORS_PER_BLOCK);
        let m = tree.lookup(&f.ext4, 26).unwrap().unwrap();
        assert_eq!((m.block(), m.len(), m.start()), (25, 2, 9000));

        // Left merge: physically contiguous extension of [10,12)→1000.
        let sc = tree.sector_count();
        tree.insert(&f.ext4, 12, 1002, 2, ExtentKind::Written, None, None)
            .unwrap();
        assert_eq!(tree.sector_count(), sc + 2 * SECTORS_PER_BLOCK);
        let m = tree.lookup(&f.ext4, 13).unwrap().unwrap();
        assert_eq!((m.block(), m.len(), m.start()), (10, 4, 1000));

        // Bridging absorb: two physically consecutive runs with a hole flush
        // between them; filling it fuses all three into one extent.
        tree.insert(&f.ext4, 70, 8000, 2, ExtentKind::Written, None, None)
            .unwrap();
        tree.insert(&f.ext4, 75, 8005, 3, ExtentKind::Written, None, None)
            .unwrap();
        let sc = tree.sector_count();
        tree.insert(&f.ext4, 72, 8002, 3, ExtentKind::Written, None, None)
            .unwrap();
        assert_eq!(tree.sector_count(), sc + 3 * SECTORS_PER_BLOCK);
        let m = tree.lookup(&f.ext4, 77).unwrap().unwrap();
        assert_eq!((m.block(), m.len(), m.start()), (70, 8, 8000));

        // Position-0 insert: the leaf's first key drops from 10 to 4 and the
        // root index key follows (correct_indexes at the root).
        assert_eq!(root_index_key(&tree, 0), 10);
        tree.insert(&f.ext4, 4, 7000, 2, ExtentKind::Written, None, None)
            .unwrap();
        assert_eq!(root_index_key(&tree, 0), 4);
        assert_eq!(tree.lookup(&f.ext4, 4).unwrap().unwrap().start(), 7000);
        // Blocks below the new first key stay holes.
        assert!(tree.lookup(&f.ext4, 3).unwrap().is_none());

        assert_matches_linear(&f, &tree, 90);
    }

    /// The sequential-append pattern (fio seq write, SQLite growth): every
    /// extension left-merges onto the tail extent in place — extent count and
    /// metadata footprint stay constant while only data blocks accrue.
    #[ktest]
    fn insert_fast_path_appends_without_rebuild() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        for (b, p) in [(0, 500), (10, 600), (20, 700), (30, 800), (40, 900)] {
            tree.insert(&f.ext4, b, p, 2, ExtentKind::Written, None, None)
                .unwrap();
        }
        assert_eq!(tree.depth(), 1);

        // 60 contiguous appends onto the tail run [40,42)→900.
        let sc = tree.sector_count();
        for i in 0..60u32 {
            tree.insert(
                &f.ext4,
                42 + i,
                902 + i as Ext4Bid,
                1,
                ExtentKind::Written,
                None,
                None,
            )
            .unwrap();
        }
        // Pure data growth (no metadata churn), one merged tail extent.
        assert_eq!(tree.sector_count(), sc + 60 * SECTORS_PER_BLOCK);
        let m = tree.lookup(&f.ext4, 101).unwrap().unwrap();
        assert_eq!((m.block(), m.len(), m.start()), (40, 62, 900));
        let (extents, _) = tree.flatten(&f.ext4).unwrap();
        assert_eq!(extents.len(), 5);

        assert_matches_linear(&f, &tree, 110);
    }

    // ---- P9a-T3: 分裂与加深 ----

    /// Builds a tree of `n` unmergeable single-block extents at blocks
    /// `0,2,4,…` (odd physical parity kills merging) through the ordinary
    /// insert path — ascending appends, so every split is a pure-boundary
    /// (dense) one.
    fn ascending_tree(
        f: &crate::fs::fs_impls::ext4::test_utils::Ext4Fixture,
        n: u32,
    ) -> ExtentTree {
        let mut tree = ExtentTree::empty();
        for i in 0..n {
            tree.insert(
                &f.ext4,
                i * 2,
                200_000 + i as Ext4Bid * 2,
                1,
                ExtentKind::Written,
                None,
                None,
            )
            .unwrap();
        }
        tree
    }

    /// A full leaf split on the append boundary keeps the old leaf dense
    /// (Linux's split-at-insert-point policy): the new leaf starts with just
    /// the appended extent, and the root gains exactly one index entry.
    #[ktest]
    fn split_on_append_keeps_leaves_dense() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let tree_full = ascending_tree(&f, LEAF_MAX as u32);
        assert_eq!((tree_full.depth(), root_entries(&tree_full)), (1, 1));

        let mut tree = tree_full;
        let sc = tree.sector_count();
        // The 341st ascending extent: leaf full → split; new leaf holds it alone.
        tree.insert(
            &f.ext4,
            LEAF_MAX as u32 * 2,
            300_000,
            1,
            ExtentKind::Written,
            None,
            None,
        )
        .unwrap();
        assert_eq!((tree.depth(), root_entries(&tree)), (1, 2));
        // One fresh metadata block plus one data block.
        assert_eq!(tree.sector_count(), sc + 2 * SECTORS_PER_BLOCK);
        match tree.find(&f.ext4, LEAF_MAX as u32 * 2).unwrap() {
            Search::Covered { path, .. } => {
                assert_eq!(path.leaf().unwrap().node.entries(), 1);
            }
            _ => panic!("appended extent must be covered"),
        }
        match tree.find(&f.ext4, 0).unwrap() {
            Search::Covered { path, .. } => {
                assert_eq!(path.leaf().unwrap().node.entries(), LEAF_MAX);
            }
            _ => panic!("old extents must survive the split"),
        }
        assert_matches_linear(&f, &tree, LEAF_MAX as u32 * 2 + 4);
    }

    /// A mid-leaf split moves the tail entries to the new leaf and lands the
    /// extent in the old one; a below-first-key insert into a full leaf moves
    /// everything, lands in the emptied old leaf, and corrects the root key.
    #[ktest]
    fn split_mid_leaf_and_position_zero() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();

        // Mid split: insert into the hole at block 401 (odd = unmapped).
        let mut tree = ascending_tree(&f, LEAF_MAX as u32);
        tree.insert(&f.ext4, 401, 400_000, 1, ExtentKind::Written, None, None)
            .unwrap();
        assert_eq!((tree.depth(), root_entries(&tree)), (1, 2));
        let m = tree.lookup(&f.ext4, 401).unwrap().unwrap();
        assert_eq!(m.start(), 400_000);
        assert_matches_linear(&f, &tree, LEAF_MAX as u32 * 2 + 4);

        // Position-zero split: rebuild dense, then insert below every key.
        // (Blocks start at 2 here so 0..2 is a hole below the first key.)
        let mut tree = ExtentTree::empty();
        for i in 0..LEAF_MAX as u32 {
            tree.insert(
                &f.ext4,
                2 + i * 2,
                500_000 + i as Ext4Bid * 2,
                1,
                ExtentKind::Written,
                None,
                None,
            )
            .unwrap();
        }
        assert_eq!(root_index_key(&tree, 0), 2);
        tree.insert(&f.ext4, 0, 600_000, 1, ExtentKind::Written, None, None)
            .unwrap();
        // The leaf's (and root's) first key dropped to 0.
        assert_eq!(root_index_key(&tree, 0), 0);
        assert_eq!(tree.lookup(&f.ext4, 0).unwrap().unwrap().start(), 600_000);
        assert_matches_linear(&f, &tree, LEAF_MAX as u32 * 2 + 8);
    }

    /// Filling four dense leaves fills the inline root; the next append grows
    /// the tree to depth 2 (the old root's content moves into a fresh interior
    /// node) and the split cascade lands in it.
    #[ktest]
    fn grow_indepth_on_full_root() {
        let f = Ext4FixtureBuilder::new(16384, 256, 16384)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let full = INLINE_MAX as u32 * LEAF_MAX as u32; // 4 × 340 = 1360
        let mut tree = ascending_tree(&f, full);
        assert_eq!((tree.depth(), root_entries(&tree)), (1, INLINE_MAX as u16));

        let sc = tree.sector_count();
        tree.insert(
            &f.ext4,
            full * 2,
            700_000,
            1,
            ExtentKind::Written,
            None,
            None,
        )
        .unwrap();
        // Depth grew; the root now holds the single index entry to the copied
        // old root; the appended extent landed in a fresh dense leaf.
        assert_eq!((tree.depth(), root_entries(&tree)), (2, 1));
        // Two fresh metadata blocks (the copied-down root + the new leaf) plus
        // one data block.
        assert_eq!(tree.sector_count(), sc + 3 * SECTORS_PER_BLOCK);
        let m = tree.lookup(&f.ext4, full * 2).unwrap().unwrap();
        assert_eq!(m.start(), 700_000);

        // Spot probes across the whole space agree with the linear reference
        // (a full probe over 2700+ blocks would dominate the suite runtime).
        for ib in (0..full * 2 + 4).step_by(7) {
            let linear = tree.lookup_linear(&f.ext4, ib).unwrap().map(|e| e.block());
            let path = tree.lookup(&f.ext4, ib).unwrap().map(|e| e.block());
            assert_eq!(linear, path, "mismatch at block {ib}");
        }

        // And the tree keeps absorbing appends after the growth.
        for i in 1..8u32 {
            tree.insert(
                &f.ext4,
                (full + i) * 2,
                700_000 + i as Ext4Bid * 2,
                1,
                ExtentKind::Written,
                None,
                None,
            )
            .unwrap();
        }
        assert_eq!(tree.depth(), 2);
        let m = tree.lookup(&f.ext4, (full + 7) * 2).unwrap().unwrap();
        assert_eq!(m.start(), 700_014);
    }

    /// Splits under a live journal: every touched node (fresh and old) rides
    /// the transaction's captures, and post-flush lookups read the split tree
    /// back consistently through the journal funnel. The commit thread stays
    /// alive — 345 ops outgrow any fixture journal without its space reclaim,
    /// and reads must stay correct across whatever running / committing /
    /// checkpointed mix results.
    #[ktest]
    fn journaled_split_reads_back_consistently() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .with_journal_inode(256)
            .build()
            .unwrap();
        let journal = f.ext4.journal().unwrap();

        let mut tree = ExtentTree::empty();
        let n = LEAF_MAX as u32 + 5; // forces one journaled split near the end
        for i in 0..n {
            let op = f.ext4.begin_op(8).unwrap();
            tree.insert(
                &f.ext4,
                i * 2,
                200_000 + i as Ext4Bid * 2,
                1,
                ExtentKind::Written,
                op.get(),
                None,
            )
            .unwrap();
        }
        assert_eq!((tree.depth(), root_entries(&tree)), (1, 2));

        // Reads through the journal stations see the split tree…
        assert_matches_linear(&f, &tree, n * 2 + 4);
        // …and after commit + checkpoint the device is authoritative and
        // still agrees.
        journal.flush_on_unmount().unwrap();
        assert_matches_linear(&f, &tree, n * 2 + 4);
    }

    // ---- P9a-T3 收口审查 findings 回归钉 ----

    /// `try_new` rejects an inline root whose `entries`/`max` exceed the
    /// `i_block` capacity (INLINE_MAX). Without the bound the root scanners
    /// would index past the 60-byte `i_block` on a crafted image — a release
    /// slice panic, not the fail-loud EUCLEAN a corrupt mount owes.
    #[ktest]
    fn try_new_rejects_oversized_inline_root() {
        let mut root = [0u32; RAW_BLOCK_PTRS_LEN];
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: 5, // > INLINE_MAX (4): would overrun i_block at entry 4
            max: 5,
            depth: 0,
            generation: 0,
        };
        root.as_mut_bytes()[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        assert!(ExtentTree::try_new(root, 0).is_err());
    }

    /// A depth-1 root pointing at an entries==0 interior node is rejected at
    /// the `NodeBuf::read` parse boundary (a childless interior is corruption);
    /// the walker never reads its phantom entry-0 bytes.
    #[ktest]
    fn find_rejects_empty_interior_child() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        // A depth-2 root would descend through an interior node; craft that
        // interior as empty (entries=0, depth=1).
        let empty_interior = 220u32;
        let mut block = [0u8; BLOCK_SIZE];
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: 0,
            max: INTERIOR_MAX as u16,
            depth: 1,
            generation: 0,
        };
        block[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        f.write_data_block(empty_interior, &block);

        // A depth-2 index root whose single child is that empty interior.
        let mut rootb = [0u32; RAW_BLOCK_PTRS_LEN];
        let rh = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: 1,
            max: 4,
            depth: 2,
            generation: 0,
        };
        rootb.as_mut_bytes()[0..ENTRY_SIZE].copy_from_slice(rh.as_bytes());
        let idx = RawExtentIdx {
            block: 0,
            leaf_lo: empty_interior,
            leaf_hi: 0,
            unused: 0,
        };
        rootb.as_mut_bytes()[ENTRY_SIZE..2 * ENTRY_SIZE].copy_from_slice(idx.as_bytes());
        let tree = ExtentTree::try_new(rootb, 0).unwrap();
        assert!(tree.find(&f.ext4, 0).is_err());
    }

    /// A before-first insert into a full leaf splits keeping the old leaf's
    /// original first entry (Linux `at.max(1)`): the old leaf never empties
    /// and the fresh node's parent key is strictly greater, so no duplicate
    /// index key is ever produced. The new extent lands in the old leaf and
    /// corrects the root key downward.
    #[ktest]
    fn before_first_split_keeps_old_leaf_nonempty() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        // A full leaf whose first block is 10 (so 0..10 is a before-first hole).
        let mut tree = ExtentTree::empty();
        for i in 0..LEAF_MAX as u32 {
            tree.insert(
                &f.ext4,
                10 + i * 2,
                800_000 + i as Ext4Bid * 2,
                1,
                ExtentKind::Written,
                None,
                None,
            )
            .unwrap();
        }
        assert_eq!((tree.depth(), root_entries(&tree)), (1, 1));
        assert_eq!(root_index_key(&tree, 0), 10);

        tree.insert(&f.ext4, 0, 900_000, 1, ExtentKind::Written, None, None)
            .unwrap();
        // Split happened; the two root index keys are strictly ordered (no
        // duplicate), the first dropped to the new extent's block.
        assert_eq!((tree.depth(), root_entries(&tree)), (1, 2));
        assert_eq!(root_index_key(&tree, 0), 0);
        assert!(root_index_key(&tree, 0) < root_index_key(&tree, 1));
        // Both leaves are non-empty (the old leaf kept its first entry).
        for probe in [0u32, 10] {
            match tree.find(&f.ext4, probe).unwrap() {
                Search::Covered { path, .. } => {
                    assert!(path.leaf().unwrap().node.entries() >= 1);
                }
                _ => panic!("block {probe} must be covered"),
            }
        }
        assert_eq!(tree.lookup(&f.ext4, 0).unwrap().unwrap().start(), 900_000);
        assert_matches_linear(&f, &tree, LEAF_MAX as u32 * 2 + 12);
    }

    // ---- P9a-T4a: in-place tail truncate ----

    /// Like [`ascending_tree`], but the data blocks are REALLY allocated from
    /// the fixture's bitmap — a truncate test frees them, and freeing a
    /// fictional block panics the group lookup. Returns the per-extent
    /// physical blocks for mapping assertions.
    fn ascending_tree_allocated(
        f: &crate::fs::fs_impls::ext4::test_utils::Ext4Fixture,
        n: u32,
    ) -> (ExtentTree, Vec<Ext4Bid>) {
        let mut tree = ExtentTree::empty();
        let mut pblocks = Vec::with_capacity(n as usize);
        for i in 0..n {
            let range = f.ext4.alloc_blocks(1, 0, None).unwrap();
            let pb = range.start;
            tree.insert(&f.ext4, i * 2, pb, 1, ExtentKind::Written, None, None)
                .unwrap();
            pblocks.push(pb);
        }
        (tree, pblocks)
    }

    /// Truncating a depth-2 tree frees the doomed tail extents in place and
    /// prunes the emptied leaves and interior nodes (Linux rm_leaf/rm_idx
    /// cascade), leaving the survivor prefix mapped and the tree still valid.
    #[ktest]
    fn truncate_prunes_depth2_cascade() {
        let f = Ext4FixtureBuilder::new(16384, 256, 16384)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        // Grow to depth 2: > INLINE_MAX × LEAF_MAX = 1360 unmergeable extents.
        let n = INLINE_MAX as u32 * LEAF_MAX as u32 + 200; // 1560
        let (mut tree, pblocks) = ascending_tree_allocated(&f, n);
        assert_eq!(tree.depth(), 2);
        let sc_full = tree.sector_count();

        // Truncate to keep only the first 100 blocks (50 extents at even
        // blocks). The doomed tail spans many leaves and whole interiors.
        let keep = 100usize;
        tree.truncate_to_byte_len(
            &f.ext4,
            keep * BLOCK_SIZE,
            None,
            None,
            journal::DataForgetPolicy::PlainData,
        )
        .unwrap();

        // Survivor prefix reads correctly; the freed tail is holes.
        for b in (0..100u32).step_by(2) {
            let m = tree.lookup(&f.ext4, b).unwrap().unwrap();
            assert_eq!(m.start(), pblocks[(b / 2) as usize]);
        }
        assert!(tree.lookup(&f.ext4, 100).unwrap().is_none());
        assert!(tree.lookup(&f.ext4, n * 2 - 2).unwrap().is_none());
        assert_matches_linear(&f, &tree, 130);
        // i_blocks dropped: 50 survivor data blocks + a handful of surviving
        // metadata nodes, far below the full tree.
        assert!(tree.sector_count() < sc_full);
        assert!(tree.sector_count() >= 50 * SECTORS_PER_BLOCK);
        // The tree stays valid and re-truncatable to zero.
        tree.truncate_to_byte_len(&f.ext4, 0, None, None, journal::DataForgetPolicy::PlainData)
            .unwrap();
        assert_eq!((tree.depth(), root_entries(&tree)), (0, 0));
        assert_eq!(tree.sector_count(), 0);
        assert!(tree.lookup(&f.ext4, 0).unwrap().is_none());
    }

    /// Truncating a depth-1 tree to zero frees every leaf and resets the root
    /// to an empty inline depth-0 node.
    #[ktest]
    fn truncate_to_zero_resets_root() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let (mut tree, _pblocks) = ascending_tree_allocated(&f, LEAF_MAX as u32 + 50); // depth 1
        assert_eq!(tree.depth(), 1);
        tree.truncate_to_byte_len(&f.ext4, 0, None, None, journal::DataForgetPolicy::PlainData)
            .unwrap();
        assert_eq!((tree.depth(), root_entries(&tree)), (0, 0));
        assert_eq!(tree.sector_count(), 0);
        // Reusable: inserts after a full truncate rebuild normally.
        tree.insert(&f.ext4, 0, 5000, 3, ExtentKind::Written, None, None)
            .unwrap();
        let m = tree.lookup(&f.ext4, 1).unwrap().unwrap();
        assert_eq!((m.block(), m.len(), m.start()), (0, 3, 5000));
    }

    /// A chunked (credit-bounded) truncate frees a bounded batch per call and
    /// reports a frontier the outer spine drives down; the survivor after each
    /// chunk stays consistent with the linear reference.
    #[ktest]
    fn truncate_chunk_frontier_and_consistency() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let n = LEAF_MAX as u32 + 100; // depth 1, 2 leaves
        let (mut tree, _pblocks) = ascending_tree_allocated(&f, n);

        // Drive chunks with a tiny per-chunk budget until the frontier reaches
        // keep_blocks, mimicking the outer restart spine (sans journal).
        let keep = 10usize;
        let keep_blocks = keep as Iblock;
        let mut guard = 0;
        loop {
            let chunk = tree
                .truncate_chunk(
                    &f.ext4,
                    keep * BLOCK_SIZE,
                    None,
                    None,
                    journal::DataForgetPolicy::PlainData,
                    Some(64),
                )
                .unwrap();
            // After each chunk the tree is a valid survivor.
            assert_matches_linear(&f, &tree, n * 2 + 4);
            if chunk.reached <= keep_blocks {
                break;
            }
            guard += 1;
            assert!(guard < 10_000, "truncate frontier must converge");
        }
        // Kept prefix survives, tail is holes.
        for b in (0..keep as u32).step_by(2) {
            assert!(tree.lookup(&f.ext4, b).unwrap().is_some());
        }
        assert!(tree.lookup(&f.ext4, keep as u32).unwrap().is_none());
    }

    // ---- P9a-T4b: in-place punch ----

    /// Punching the middle of an inline (depth-0) extent splits it into head +
    /// tail in the root and frees exactly the covered run.
    #[ktest]
    fn punch_inline_middle_splits() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        let range = f.ext4.alloc_blocks(20, 0, None).unwrap();
        let p = range.start;
        let got = (range.end - range.start) as u16;
        assert_eq!(got, 20, "fixture must satisfy a 20-block run");
        tree.insert(&f.ext4, 0, p, 20, ExtentKind::Written, None, None)
            .unwrap();
        let sc = tree.sector_count();

        tree.punch_chunk(
            &f.ext4,
            5..9,
            None,
            None,
            journal::DataForgetPolicy::PlainData,
            None,
        )
        .unwrap();
        let head = tree.lookup(&f.ext4, 4).unwrap().unwrap();
        assert_eq!((head.block(), head.len(), head.start()), (0, 5, p));
        assert!(tree.lookup(&f.ext4, 5).unwrap().is_none());
        assert!(tree.lookup(&f.ext4, 8).unwrap().is_none());
        let tail = tree.lookup(&f.ext4, 9).unwrap().unwrap();
        assert_eq!((tail.block(), tail.len(), tail.start()), (9, 11, p + 9));
        assert_eq!(tree.sector_count(), sc - 4 * SECTORS_PER_BLOCK);
    }

    /// A depth-0 root already holding [`INLINE_MAX`] extents overflows when a
    /// punch splits one of them; the tail re-enters through insert and the
    /// tree grows to depth 1 with every mapping intact.
    #[ktest]
    fn punch_inline_overflow_grows_tree() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        let mut runs = Vec::new();
        for i in 0..INLINE_MAX as u32 {
            let r = f.ext4.alloc_blocks(6, 0, None).unwrap();
            assert_eq!((r.end - r.start) as u16, 6);
            tree.insert(&f.ext4, i * 10, r.start, 6, ExtentKind::Written, None, None)
                .unwrap();
            runs.push(r.start);
        }
        assert_eq!(tree.depth(), 0);

        // Punch the middle of extent #1: head+tail push the root past
        // INLINE_MAX → the tail re-insert grows the tree.
        tree.punch_chunk(
            &f.ext4,
            12..14,
            None,
            None,
            journal::DataForgetPolicy::PlainData,
            None,
        )
        .unwrap();
        assert_eq!(tree.depth(), 1);
        let head = tree.lookup(&f.ext4, 11).unwrap().unwrap();
        assert_eq!((head.block(), head.len()), (10, 2));
        assert!(tree.lookup(&f.ext4, 12).unwrap().is_none());
        let tail = tree.lookup(&f.ext4, 14).unwrap().unwrap();
        assert_eq!(
            (tail.block(), tail.len(), tail.start()),
            (14, 2, runs[1] + 4)
        );
        // Untouched neighbours survive.
        for i in [0u32, 2, 3] {
            let m = tree.lookup(&f.ext4, i * 10 + 1).unwrap().unwrap();
            assert_eq!(m.start(), runs[i as usize]);
        }
        assert_matches_linear(&f, &tree, 40);
    }

    /// Punching the head of a non-first leaf removes its first entries; the
    /// leaf's risen first key must propagate into the parent index (Linux's
    /// exact-first-key invariant), and lookups stay correct.
    #[ktest]
    fn punch_leaf_head_corrects_parent_key() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let n = LEAF_MAX as u32 + 30;
        let (mut tree, _p) = ascending_tree_allocated(&f, n);
        assert_eq!((tree.depth(), root_entries(&tree)), (1, 2));
        let leaf2_first = LEAF_MAX as u32 * 2; // block 680
        assert_eq!(root_index_key(&tree, 1), leaf2_first);

        // Punch the second leaf's first 5 extents: [680, 690).
        tree.punch_chunk(
            &f.ext4,
            leaf2_first..leaf2_first + 10,
            None,
            None,
            journal::DataForgetPolicy::PlainData,
            None,
        )
        .unwrap();
        // The parent key follows the leaf's new first entry exactly.
        assert_eq!(root_index_key(&tree, 1), leaf2_first + 10);
        assert!(tree.lookup(&f.ext4, leaf2_first).unwrap().is_none());
        assert!(tree.lookup(&f.ext4, leaf2_first + 10).unwrap().is_some());
        assert_matches_linear(&f, &tree, n * 2 + 4);
    }

    /// A punch that covers a whole leaf's range empties and prunes it from the
    /// middle of the tree; the sibling leaves survive untouched.
    #[ktest]
    fn punch_empties_and_prunes_middle_leaf() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let n = LEAF_MAX as u32 + 30;
        let (mut tree, _p) = ascending_tree_allocated(&f, n);
        assert_eq!((tree.depth(), root_entries(&tree)), (1, 2));
        let sc = tree.sector_count();

        // Punch the ENTIRE first leaf's range [0, 680).
        tree.punch_chunk(
            &f.ext4,
            0..LEAF_MAX as u32 * 2,
            None,
            None,
            journal::DataForgetPolicy::PlainData,
            None,
        )
        .unwrap();
        assert_eq!((tree.depth(), root_entries(&tree)), (1, 1));
        assert!(tree.lookup(&f.ext4, 0).unwrap().is_none());
        assert!(tree.lookup(&f.ext4, 678).unwrap().is_none());
        assert!(tree.lookup(&f.ext4, LEAF_MAX as u32 * 2).unwrap().is_some());
        // 340 data blocks + the pruned leaf node left i_blocks.
        assert_eq!(
            tree.sector_count(),
            sc - (LEAF_MAX as u64 + 1) * SECTORS_PER_BLOCK
        );
        assert_matches_linear(&f, &tree, n * 2 + 4);
    }

    /// Punching inside one extent of a FULL leaf takes the slow spans-both
    /// path: trim to the head, re-insert the tail through the split machinery
    /// (the leaf splits), every mapping intact.
    #[ktest]
    fn punch_full_leaf_spans_both_splits() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        // 339 singles + one 5-block run = exactly LEAF_MAX entries, depth 1.
        let (mut tree, _p) = ascending_tree_allocated(&f, LEAF_MAX as u32 - 1);
        let big = f.ext4.alloc_blocks(5, 0, None).unwrap();
        assert_eq!((big.end - big.start) as u16, 5);
        let big_block = (LEAF_MAX as u32 - 1) * 2; // 678, covers 678..683
        tree.insert(
            &f.ext4,
            big_block,
            big.start,
            5,
            ExtentKind::Written,
            None,
            None,
        )
        .unwrap();
        assert_eq!((tree.depth(), root_entries(&tree)), (1, 1));
        match tree.find(&f.ext4, big_block).unwrap() {
            Search::Covered { path, .. } => {
                assert_eq!(path.leaf().unwrap().node.entries(), LEAF_MAX)
            }
            _ => panic!("big extent must be mapped"),
        }

        // Punch [679, 681): head 678..679, tail 681..683, leaf FULL → split.
        tree.punch_chunk(
            &f.ext4,
            big_block + 1..big_block + 3,
            None,
            None,
            journal::DataForgetPolicy::PlainData,
            None,
        )
        .unwrap();
        assert_eq!((tree.depth(), root_entries(&tree)), (1, 2));
        let head = tree.lookup(&f.ext4, big_block).unwrap().unwrap();
        assert_eq!(
            (head.block(), head.len(), head.start()),
            (big_block, 1, big.start)
        );
        assert!(tree.lookup(&f.ext4, big_block + 1).unwrap().is_none());
        assert!(tree.lookup(&f.ext4, big_block + 2).unwrap().is_none());
        let tail = tree.lookup(&f.ext4, big_block + 3).unwrap().unwrap();
        assert_eq!(
            (tail.block(), tail.len(), tail.start()),
            (big_block + 3, 2, big.start + 3)
        );
        assert_matches_linear(&f, &tree, big_block + 8);
    }
}
