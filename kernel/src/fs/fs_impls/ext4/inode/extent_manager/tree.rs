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
        EXTENT_MAGIC, Extent, ExtentHeader, ExtentIdx, ExtentKind, MAX_WRITTEN_LEN, RawExtent,
        RawExtentHeader, RawExtentIdx,
    },
};

/// Size of one extent-tree entry (header, index, or leaf), in bytes.
const ENTRY_SIZE: usize = 12;

/// Maximum extent-tree depth, mirroring `EXT4_MAX_EXTENT_DEPTH`.
const MAX_DEPTH: u32 = 5;

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
        ExtentHeader::try_from(&RawExtentHeader::from_bytes(
            &root.as_bytes()[0..ENTRY_SIZE],
        ))?;
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
    /// for a hole.
    ///
    /// External nodes are read through [`journal::read_metadata_block`]: on a
    /// journaled volume a node's newest bytes may still sit in a journal
    /// capture (WAL suppresses the direct write until checkpoint), so a bare
    /// device read here would walk a stale tree.
    pub(super) fn lookup(&self, fs: &Ext4, iblock: Iblock) -> Result<Option<Extent>> {
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
        let (extents, old_external) = self.flatten(fs)?;

        let range_start = iblock;
        let range_end = iblock as u64 + len as u64;

        // No unwritten extent overlaps the range → nothing to convert. Return
        // before `reserialize`, which would re-journal every external node of
        // the tree. `write_at` calls this on EVERY write, so without this gate
        // a plain overwrite of already-written blocks (no allocation, no size
        // change) would flatten-and-rebuild a large file's whole extent tree:
        // pure write amplification, and on a depth-2 file the rewrite can
        // capture more metadata blocks than one transaction's descriptor holds
        // and abort a legal write. (The old pre-write gate in `ensure_allocated`
        // did this check; it moved here with the conversion.)
        if !extents.iter().any(|e| {
            e.is_unwritten()
                && (e.block() as u64) < range_end
                && e.block() as u64 + e.len() as u64 > range_start as u64
        }) {
            return Ok(());
        }

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
    /// `i_blocks`.
    ///
    /// `data_policy` is the owning inode's revoke rule for its DATA blocks
    /// (Linux `get_default_free_blocks_flags`): directory blocks and
    /// slow-symlink targets are forgotten (revoked) before their free,
    /// regular-file data is not. The tree's own external nodes are always
    /// forgotten ([`free_meta_block`]), independent of the policy.
    pub(super) fn truncate_to_byte_len(
        &mut self,
        fs: &Ext4,
        new_size: usize,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
        data_policy: journal::DataForgetPolicy,
    ) -> Result<()> {
        // Lossless: callers bound `new_size` by `ensure_size_within_limit` /
        // `max_file_size` (≤ `u32::MAX` logical blocks — see `fs.rs`).
        let keep_blocks = new_size.div_ceil(BLOCK_SIZE) as Iblock;

        let (mut extents, old_external) = self.flatten(fs)?;
        extents.sort_by_key(|e| e.block());

        let mut kept: Vec<Extent> = Vec::new();
        let mut freed_data: u64 = 0;
        for e in &extents {
            let e_start = e.block();
            let e_end = e_start + e.len() as Iblock;
            if e_end <= keep_blocks {
                kept.push(*e);
                continue;
            }
            if e_start >= keep_blocks {
                // Entire extent is beyond the new size; free all its blocks
                // (the free discharges the forget when the inode's policy
                // says its data is revoke-covered — dir blocks, slow-symlink
                // targets). Per-extent authorization means an error on a
                // later extent leaves this one fully freed-and-revoked and
                // the later ones' journal state untouched.
                let auth = data_policy.authorize(e.start(), e.len() as u32);
                fs.free_blocks(auth, handle)?;
                freed_data += e.len() as u64;
                continue;
            }
            // The extent straddles `keep_blocks`: keep the head, free the tail.
            // Lossless: the head lies inside this extent, whose length is u16.
            let head_len = (keep_blocks - e_start) as u16;
            let tail_len = e.len() - head_len;
            let auth = data_policy.authorize(e.start() + head_len as Ext4Bid, tail_len as u32);
            fs.free_blocks(auth, handle)?;
            freed_data += tail_len as u64;
            kept.push(Extent::new(e.block(), head_len, e.start(), e.kind()));
        }

        let delta = self.reserialize(fs, &kept, &old_external, handle, csum_seed)?;
        // The external-leaf count changes by exactly the mutation's delta.
        let net_meta = delta.meta_allocated as i64 - delta.meta_freed as i64;
        let removed_sectors = (freed_data as i64 - net_meta) * SECTORS_PER_BLOCK as i64;
        // `i_blocks` must never drop below zero; `max(0)` saturates, the assert
        // catches a miscounted `sector_count` in debug builds.
        debug_assert!(self.sector_count as i64 >= removed_sectors);
        self.sector_count = (self.sector_count as i64 - removed_sectors).max(0) as u64;
        self.dirty = true;
        Ok(())
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
}

/// The outcome of searching a single extent-tree node for `iblock`.
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
fn search_node(bytes: &[u8], iblock: Iblock) -> Result<Step> {
    let header = ExtentHeader::try_from(&RawExtentHeader::from_bytes(&bytes[0..ENTRY_SIZE]))?;
    search_entries(&header, bytes, iblock)
}

/// Searches one node's entries for `iblock`, `header` already decoded.
///
/// Entries are sorted by logical block, so the covering entry is the last one
/// whose starting block is `<= iblock`. Phase 1 scans linearly (nodes hold at
/// most a few hundred entries); a binary search is a later optimization.
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
    let _create = journal::get_create_access(handle, bid)?;
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
fn stamp_extent_tail(block: &mut [u8], seed: InodeCsumSeed) {
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

/// Sorts `extents` by logical block and coalesces runs that are logically and
/// physically contiguous and share the same written/unwritten state.
fn merge_extents(extents: &mut Vec<Extent>) {
    extents.sort_by_key(|e| e.block());
    let mut merged: Vec<Extent> = Vec::with_capacity(extents.len());
    for e in extents.iter() {
        if let Some(last) = merged.last() {
            // Unwritten extents cap one below the written limit: the length is
            // bias-encoded as `len + MAX_WRITTEN_LEN`, so an unwritten run of
            // `MAX_WRITTEN_LEN` would overflow `ee_len` (Linux uses the distinct
            // `EXT_UNWRITTEN_MAX_LEN = 32767`).
            let max_len = if last.is_unwritten() {
                MAX_WRITTEN_LEN as u32 - 1
            } else {
                MAX_WRITTEN_LEN as u32
            };
            let contiguous = last.block() as u64 + last.len() as u64 == e.block() as u64
                && last.start() + last.len() as u64 == e.start()
                && last.is_unwritten() == e.is_unwritten()
                && last.len() as u32 + e.len() as u32 <= max_len;
            if contiguous {
                *merged.last_mut().unwrap() = Extent::new(
                    last.block(),
                    last.len() + e.len(),
                    last.start(),
                    last.kind(),
                );
                continue;
            }
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
}
