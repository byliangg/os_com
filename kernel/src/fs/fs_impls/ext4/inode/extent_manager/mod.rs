// SPDX-License-Identifier: MPL-2.0

//! Ext4 extent block-mapping engine — the inode's logical→physical block
//! translation for reads, writes, allocation, and truncation.
//!
//! This replaces ext2's indirect-block tree. The authoritative state is one
//! [`ExtentTree`] per inode (the validated tree root + `i_blocks` accounting,
//! defined in [`tree`]); [`ExtentManager`] wraps it in the position-③ lock
//! (report §5.1), delegates every operation, and doubles as the `PageCache`
//! backend, mirroring ext2's `InodeBlockManager` over `BlockPtrTree`.
//!
//! Interior tree nodes are read per lookup (through the journal's read
//! funnel); a frame cache (an allocation-free fast path for repeated lookups)
//! is a P9 optimization.

use core::sync::atomic::{AtomicUsize, Ordering};

use super::{
    super::{journal, prelude::*},
    RAW_BLOCK_PTRS_LEN,
};

mod node;
mod tree;

pub(super) use self::tree::ExtentTree;

/// State of a mapped logical block — the three-way view tests assert against
/// (production code pattern-matches [`Mapping`] directly).
#[cfg(ktest)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MapState {
    /// Backed by written data on disk.
    Written,
    /// Allocated but never written; reads as zeros.
    Unwritten,
    /// Not allocated; reads as zeros.
    Hole,
}

/// The result of mapping a logical block. A hole carries no physical block at
/// all — there is no in-band "pblock 0" to misread as block 0.
#[derive(Clone, Copy, Debug)]
pub(super) enum Mapping {
    /// A contiguous mapped physical run.
    Mapped {
        pblock: Ext4Bid,
        /// Contiguous logical blocks from the queried one to the run's end.
        len: u32,
        /// `false` = preallocated-unwritten: allocated, but reads as zeros.
        written: bool,
    },
    /// Not allocated; reads as zeros.
    Hole {
        /// Logical blocks known to be unmapped (currently always 1). Only the
        /// test view reads it today; production hole consumers allocate or
        /// zero-fill one block at a time.
        #[cfg_attr(not(ktest), expect(dead_code))]
        len: u32,
    },
}

impl Mapping {
    /// Returns the three-way state view (see [`MapState`]).
    #[cfg(ktest)]
    pub(super) const fn state(&self) -> MapState {
        match self {
            Mapping::Mapped { written: true, .. } => MapState::Written,
            Mapping::Mapped { written: false, .. } => MapState::Unwritten,
            Mapping::Hole { .. } => MapState::Hole,
        }
    }

    /// Returns the number of contiguous logical blocks this mapping describes.
    #[cfg(ktest)]
    pub(super) const fn len(&self) -> u32 {
        match self {
            Mapping::Mapped { len, .. } | Mapping::Hole { len } => *len,
        }
    }

    /// Returns the physical block backing the run, `None` for a hole.
    pub(super) const fn mapped_pblock(&self) -> Option<Ext4Bid> {
        match self {
            Mapping::Mapped { pblock, .. } => Some(*pblock),
            Mapping::Hole { .. } => None,
        }
    }

    /// Returns whether reading these blocks must return zeros without device I/O.
    #[cfg(ktest)]
    pub(super) const fn reads_as_zeros(&self) -> bool {
        !matches!(self, Mapping::Mapped { written: true, .. })
    }
}

/// Maps an inode's logical blocks to physical blocks via its [`ExtentTree`],
/// which owns the authoritative tree + `i_blocks` accounting.
///
/// Thin delegation over the tree: this type contributes the lock (position ③
/// in the global order, report §5.1), the filesystem back-reference, and the
/// `PageCache` backend surface — mirroring ext2's `InodeBlockManager` over
/// `BlockPtrTree`.
pub(super) struct ExtentManager {
    /// The authoritative extent tree (the ③ "ExtentTree" lock).
    state: RwMutex<ExtentTree>,
    /// Cached page count for the `PageCache` backend.
    npages: AtomicUsize,
    /// Back-reference to the filesystem, for the block device and allocator.
    fs: Weak<super::super::fs::Ext4>,
    /// The owning inode's `metadata_csum` seed, `Some` only when the feature is
    /// on. Threaded into every external-node write so leaf/interior blocks carry
    /// a correct extent-block tail checksum (`ext4_extent_block_csum`); `None`
    /// leaves those blocks byte-identical to the pre-feature layout.
    csum_seed: Option<u32>,
}

impl ExtentManager {
    /// Validates `root` (see [`ExtentTree::try_new`]) and builds the manager.
    pub(super) fn try_new(
        root: [u32; RAW_BLOCK_PTRS_LEN],
        sector_count: u64,
        fs: Weak<super::super::fs::Ext4>,
        npages: usize,
        csum_seed: Option<u32>,
    ) -> Result<Self> {
        Ok(Self {
            state: RwMutex::new(ExtentTree::try_new(root, sector_count)?),
            npages: AtomicUsize::new(npages),
            fs,
            csum_seed,
        })
    }

    /// Returns a strong reference to the owning filesystem.
    fn fs(&self) -> Result<Arc<super::super::fs::Ext4>> {
        self.fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem dropped"))
    }

    /// Maps logical block `iblock` to a physical run.
    ///
    /// The returned length spans from `iblock` to the end of the covering
    /// extent, so callers can batch contiguous reads.
    pub(super) fn map_blocks(&self, iblock: Iblock) -> Result<Mapping> {
        let fs = self.fs()?;
        let tree = self.state.read();

        match tree.lookup(&fs, iblock)? {
            Some(extent) => {
                let offset_in_extent = iblock - extent.block();
                Ok(Mapping::Mapped {
                    pblock: extent.start() + offset_in_extent as Ext4Bid,
                    len: extent.len() as u32 - offset_in_extent,
                    written: !extent.is_unwritten(),
                })
            }
            None => Ok(Mapping::Hole { len: 1 }),
        }
    }

    /// Returns the inode's `i_blocks` (512-byte sectors) accounting.
    pub(super) fn sector_count(&self) -> u64 {
        self.state.read().sector_count()
    }

    /// Returns a copy of the inode's 60-byte `i_block` (extent-tree root),
    /// snapshotted under the lock — the inode-writeback serialization boundary.
    pub(super) fn root_snapshot(&self) -> [u32; RAW_BLOCK_PTRS_LEN] {
        *self.state.read().root_bytes()
    }

    /// Returns the extent-tree depth (0 = inline leaf, 1 = one index level).
    #[cfg(ktest)]
    pub(super) fn root_depth(&self) -> u16 {
        self.state.read().depth()
    }

    /// Returns whether the tree or `i_blocks` has changed since last writeback.
    pub(super) fn is_dirty(&self) -> bool {
        self.state.read().is_dirty()
    }

    /// Clears the dirty flag after a successful inode writeback.
    pub(super) fn clear_dirty(&self) {
        self.state.write().clear_dirty();
    }

    /// Updates the cached page-cache capacity bound.
    pub(super) fn set_npages(&self, npages: usize) {
        self.npages.store(npages, Ordering::Release);
    }

    /// Allocates data blocks (as UNWRITTEN) for every hole in `[start_iblock,
    /// end_iblock)`, and records them in the extent tree.
    ///
    /// Planning is done from a single snapshot of the current tree: existing
    /// written and unwritten extents are left untouched, and blocks are
    /// allocated only where the snapshot showed a true hole. The Unwritten-first
    /// protocol (see [`mark_range_written`](Self::mark_range_written)) means the
    /// fresh blocks stay read-as-zeros until the caller's data lands and
    /// `write_at` converts the range to written — so a partial-block write never
    /// exposes the freshly recycled block's stale contents. `i_blocks` grows by
    /// every data block allocated plus the net extent-tree metadata blocks.
    ///
    /// On an allocation error mid-way the partial allocation stays in `state`;
    /// the caller's `rollback_write` truncates it away (the just-allocated
    /// unwritten blocks read as zeros meanwhile, so nothing leaks).
    pub(super) fn ensure_allocated(
        &self,
        start_iblock: Iblock,
        end_iblock: Iblock,
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        if start_iblock >= end_iblock {
            return Ok(());
        }
        let fs = self.fs()?;
        let mut tree = self.state.write();

        // Plan hole runs from a snapshot of the current tree by interval-
        // subtracting the existing (sorted, non-overlapping) extents.
        //
        // Fresh holes are allocated as UNWRITTEN, not written: the block stays
        // read-as-zeros until the caller's data lands and `write_at` converts
        // it (in the same transaction). This is the Unwritten-first protocol —
        // a partial-block write's page-cache read-fill of an unwritten block
        // returns zeros instead of the freshly recycled block's stale contents,
        // so no stale data (another file's freed blocks) can leak into the
        // file, on disk or across a crash (ledger: hole-alloc-stale-exposure).
        // Any pre-existing unwritten extent in range (e.g. from a future
        // fallocate) is likewise left unwritten here and converted post-write.
        let extents = tree.extents(&fs)?;
        let holes = compute_holes(&extents, start_iblock, end_iblock);

        for hole in holes {
            let mut ib = hole.start;
            // `goal` is the previous extent's physical end for locality; full
            // locality tuning is deferred to Phase 9.
            let goal = extents
                .iter()
                .rev()
                .find(|e| e.block() < ib)
                .map(|e| e.start() + e.len() as Ext4Bid)
                .unwrap_or(0);
            while ib < hole.end {
                let want = hole.end - ib;
                let range = fs.alloc_blocks(want, goal, handle)?;
                let got = (range.end - range.start) as u32;
                debug_assert!(got > 0 && got <= want);
                // If recording the extent fails, the just-allocated data blocks
                // are not reachable through the inode, so free them here rather
                // than leak them (`rollback_write` only reclaims blocks in the
                // extent tree).
                if let Err(err) = tree.insert(
                    &fs,
                    ib,
                    range.start,
                    got as u16,
                    node::ExtentKind::Unwritten,
                    handle,
                    self.csum_seed,
                ) {
                    let _ = fs.free_blocks(range.start, got, handle);
                    return Err(err);
                }
                ib += got;
            }
        }
        Ok(())
    }

    /// Converts every unwritten extent in the logical block range `[start,
    /// end)` to written, in the caller's transaction.
    ///
    /// The Unwritten-first counterpart to [`ensure_allocated`](Self::ensure_allocated):
    /// `write_at` calls this AFTER the data pages are in the page cache, so the
    /// extent-tree metadata that makes the blocks readable-as-data commits no
    /// earlier than the ordered-data flush of those pages (crash red-line: a
    /// crash before this transaction commits leaves the blocks unwritten —
    /// read-as-zeros, never stale). Written extents (a plain overwrite) and
    /// unmapped tails are untouched. No data blocks move; `i_blocks` shifts
    /// only by the metadata delta a split may cause.
    pub(super) fn mark_range_written(
        &self,
        start_iblock: Iblock,
        end_iblock: Iblock,
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        if start_iblock >= end_iblock {
            return Ok(());
        }
        let fs = self.fs()?;
        self.state.write().convert_unwritten(
            &fs,
            start_iblock,
            end_iblock - start_iblock,
            handle,
            self.csum_seed,
        )
    }

    /// Allocates a single data block for logical block `iblock` (assumed a hole)
    /// and records it. Used by the `submit_write_bio` hole fallback.
    fn allocate_one(&self, iblock: Iblock) -> Result<Ext4Bid> {
        let fs = self.fs()?;
        // On a journaled volume this fallback is unreachable from any legal
        // path: every dirty page comes from write_at/resize/write_link, which
        // allocate under a transaction BEFORE dirtying. A hole under a dirty
        // page here would mean an unjournaled bitmap/GDT/superblock/extent
        // mutation — exactly the write-around-the-journal class the crash
        // review banned (and since the sync paths stopped direct-writing
        // metadata, such a mutation would not even reach the disk). Fail loud
        // instead of allocating outside WAL; the legitimate future consumer
        // (mmap write-fault allocation) needs its own transaction plumbing
        // (ledger: mmap-hole-writeback).
        if fs.journal().is_some() {
            return_errno_with_message!(
                Errno::EIO,
                "writeback hit an unallocated block with no transaction on a journaled volume"
            );
        }
        let mut tree = self.state.write();
        // The page-cache writeback fallback has no open handle to thread.
        let range = fs.alloc_blocks(1, 0, None)?;
        let pblock = range.start;
        if let Err(err) = tree.insert(
            &fs,
            iblock,
            pblock,
            1,
            node::ExtentKind::Written,
            None,
            self.csum_seed,
        ) {
            // Free the just-allocated block rather than leak it.
            let _ = fs.free_blocks(pblock, 1, None);
            return Err(err);
        }
        Ok(pblock)
    }

    /// Frees every data block and extent-tree metadata block mapping a logical
    /// region at or beyond `new_size` bytes, rewriting the tree and updating
    /// `i_blocks`. Used by `rollback_write`; Phase 4 extends it (partial-block
    /// zeroing, the public `resize`).
    pub(super) fn truncate_to_byte_len(
        &self,
        new_size: usize,
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        let fs = self.fs()?;
        self.state
            .write()
            .truncate_to_byte_len(&fs, new_size, handle, self.csum_seed)
    }
}

/// A contiguous run of unmapped logical blocks.
struct HoleRun {
    start: Iblock,
    end: Iblock,
}

/// Computes the hole runs (unmapped logical blocks) within `[start, end)` by
/// interval-subtracting the sorted, non-overlapping `extents`.
fn compute_holes(extents: &[node::Extent], start: Iblock, end: Iblock) -> Vec<HoleRun> {
    let mut holes = Vec::new();
    let mut cursor = start;
    for e in extents {
        let e_start = e.block();
        let e_end = e_start + e.len() as Iblock;
        if e_end <= cursor {
            continue;
        }
        if e_start >= end {
            break;
        }
        if e_start > cursor {
            holes.push(HoleRun {
                start: cursor,
                end: e_start.min(end),
            });
        }
        cursor = cursor.max(e_end);
        if cursor >= end {
            break;
        }
    }
    if cursor < end {
        holes.push(HoleRun { start: cursor, end });
    }
    holes
}

impl BlockAsPageCacheBackend for ExtentManager {
    fn submit_read_bio(
        &self,
        idx: usize,
        bio_segment: BioSegment,
        complete_fn: BioCompleteFn,
        io_batch: &mut IoBatch,
    ) -> Result<()> {
        if idx >= self.npages.load(Ordering::Acquire) {
            return_errno_with_message!(Errno::EINVAL, "read past end of inode");
        }
        let iblock = Iblock::try_from(idx)
            .map_err(|_| Error::with_message(Errno::EINVAL, "logical block number overflow"))?;

        let Mapping::Mapped {
            pblock,
            written: true,
            ..
        } = self.map_blocks(iblock)?
        else {
            // Holes and unwritten extents read as zeros without device I/O.
            complete_fn(BioStatus::Zeros);
            return Ok(());
        };

        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem dropped"))?;
        fs.read_blocks_async(pblock, bio_segment, Some(complete_fn), io_batch)
    }

    fn submit_write_bio(
        &self,
        idx: usize,
        bio_segment: BioSegment,
        complete_fn: BioCompleteFn,
        io_batch: &mut IoBatch,
    ) -> Result<()> {
        if idx >= self.npages.load(Ordering::Acquire) {
            return_errno_with_message!(Errno::EINVAL, "write past end of inode");
        }
        let iblock = Iblock::try_from(idx)
            .map_err(|_| Error::with_message(Errno::EINVAL, "logical block number overflow"))?;
        let fs = self.fs()?;

        // Buffered writes pre-allocate in `prepare_write`, so the block is
        // usually already mapped (written or unwritten). The hole branch is the
        // defensive fallback for mmap-dirtied pages, which the upper layer does
        // not pre-allocate.
        let pblock = match self.map_blocks(iblock)? {
            Mapping::Mapped { pblock, .. } => pblock,
            Mapping::Hole { .. } => self.allocate_one(iblock)?,
        };
        fs.write_blocks_async(pblock, bio_segment, Some(complete_fn), io_batch)
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::{
        super::super::test_utils::Ext4FixtureBuilder,
        node::{EXTENT_MAGIC, RawExtent, RawExtentHeader},
        *,
    };

    fn inline_root(extents: &[RawExtent]) -> [u32; RAW_BLOCK_PTRS_LEN] {
        let mut block = [0u32; RAW_BLOCK_PTRS_LEN];
        let bytes = block.as_mut_bytes();
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: extents.len() as u16,
            max: 4,
            depth: 0,
            generation: 0,
        };
        bytes[0..12].copy_from_slice(header.as_bytes());
        for (i, extent) in extents.iter().enumerate() {
            let off = 12 * (1 + i);
            bytes[off..off + 12].copy_from_slice(extent.as_bytes());
        }
        block
    }

    #[ktest]
    fn map_written_extent() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let root = inline_root(&[RawExtent {
            block: 0,
            len: 4,
            start_hi: 0,
            start_lo: 100,
        }]);
        let em = ExtentManager::try_new(root, 4 * 8, f.ext4.this(), 4, None).unwrap();

        let m0 = em.map_blocks(0).unwrap();
        assert_eq!(m0.state(), MapState::Written);
        assert_eq!(m0.mapped_pblock(), Some(100));
        assert_eq!(m0.len(), 4);

        // Mapping from the middle returns the remaining run.
        let m2 = em.map_blocks(2).unwrap();
        assert_eq!(m2.mapped_pblock(), Some(102));
        assert_eq!(m2.len(), 2);

        // Past the extent: a hole.
        let m4 = em.map_blocks(4).unwrap();
        assert_eq!(m4.state(), MapState::Hole);
        assert!(m4.reads_as_zeros());
    }

    #[ktest]
    fn map_unwritten_extent_reads_as_zeros() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let root = inline_root(&[RawExtent {
            block: 0,
            len: 32768 + 2, // unwritten, length 2
            start_hi: 0,
            start_lo: 500,
        }]);
        let em = ExtentManager::try_new(root, 2 * 8, f.ext4.this(), 2, None).unwrap();
        let m = em.map_blocks(0).unwrap();
        assert_eq!(m.state(), MapState::Unwritten);
        assert!(m.reads_as_zeros());
        assert_eq!(m.mapped_pblock(), Some(500));
    }

    /// Regression: when `allocate_one` allocates a data block but the following
    /// `insert_extent` fails (here the inline→depth-1 grow needs a leaf block and
    /// the disk is out of space), the data block must be freed, not leaked.
    #[ktest]
    fn allocate_one_frees_block_when_insert_fails() {
        // Exactly one free block: enough for the data block, not the tree leaf.
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_free_blocks(1)
            .build()
            .unwrap();
        // A full inline root (4 extents); inserting a 5th forces a depth-1 grow.
        let root = inline_root(&[
            RawExtent {
                block: 0,
                len: 1,
                start_hi: 0,
                start_lo: 100,
            },
            RawExtent {
                block: 2,
                len: 1,
                start_hi: 0,
                start_lo: 200,
            },
            RawExtent {
                block: 4,
                len: 1,
                start_hi: 0,
                start_lo: 300,
            },
            RawExtent {
                block: 6,
                len: 1,
                start_hi: 0,
                start_lo: 400,
            },
        ]);
        let em = ExtentManager::try_new(root, 4 * 8, f.ext4.this(), 8, None).unwrap();

        let free_before = f.ext4.super_block().free_blocks_count();
        assert_eq!(free_before, 1);

        // The 5th mapping triggers inline→depth-1; the leaf allocation hits ENOSPC.
        assert!(em.allocate_one(8).is_err());

        // The data block allocated before the failed insert was reclaimed.
        assert_eq!(f.ext4.super_block().free_blocks_count(), free_before);
    }

    /// Grows `em` from empty to a depth-1 tree (5 disjoint single-block extents
    /// overflow the 4-entry inline root) inside one journaled op.
    fn grow_to_depth_1(f: &super::super::super::test_utils::Ext4Fixture, em: &ExtentManager) {
        let op = f.ext4.begin_op(8).unwrap();
        for ib in [0u32, 2, 4, 6, 8] {
            em.ensure_allocated(ib, ib + 1, op.get()).unwrap();
        }
        drop(op);
        assert_eq!(em.root_depth(), 1, "tree grew an external leaf");
    }

    /// A1-B0 regression (read side of the B-1 hazard): while a leaf's newest
    /// bytes sit only in the journal — the running transaction's capture, or the
    /// committed-but-un-checkpointed image — tree reads must be served from
    /// them. A bare device read returns the pre-op bytes (all zeros for a leaf
    /// grown this op, since WAL suppressed its direct write), so every lookup
    /// on the file failed `EUCLEAN` until checkpoint caught the device up.
    #[ktest]
    fn journaled_tree_read_sees_uncheckpointed_leaf() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_journal_inode(64)
            .build()
            .unwrap();
        let journal = f.ext4.journal().unwrap();
        journal.stop_commit_thread();

        let em = ExtentManager::try_new(inline_root(&[]), 0, f.ext4.this(), 0, None).unwrap();
        grow_to_depth_1(&f, &em);

        // The leaf exists only as the running transaction's capture; the device
        // still holds zeros. The mapping must come from the capture — decoding
        // it at all (not `EUCLEAN`) is the point; `ensure_allocated` yields
        // unwritten extents (Unwritten-first), converted to written only when a
        // write's data lands.
        let m = em.map_blocks(8).unwrap();
        assert_eq!(m.state(), MapState::Unwritten);

        // Same across the commit boundary: the image is now retained
        // un-checkpointed (the commit thread is stopped, so nothing applies it
        // to its final location).
        journal.commit_now_for_test();
        let m = em.map_blocks(4).unwrap();
        assert_eq!(m.state(), MapState::Unwritten);
    }

    /// A1-B0 regression (`reused-leaf-missing-capture`): re-serializing into a
    /// REUSED external leaf block must capture it first — only freshly
    /// allocated leaves get `get_create_access` (in `alloc_meta_block`), so the
    /// reuse path's `dirty_metadata` failed dirty-without-access (`EIO`) on
    /// every journaled mutation of a depth-1 tree whose leaf survived from an
    /// earlier transaction.
    #[ktest]
    fn journaled_reserialize_captures_reused_leaf() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_journal_inode(64)
            .build()
            .unwrap();
        let journal = f.ext4.journal().unwrap();
        journal.stop_commit_thread();

        let em = ExtentManager::try_new(inline_root(&[]), 0, f.ext4.this(), 0, None).unwrap();
        grow_to_depth_1(&f, &em);

        // Commit + checkpoint: the leaf's capture retires, the device becomes
        // authoritative — the next mutation reuses the on-disk leaf in place
        // and must journal it itself.
        journal.flush_on_unmount().unwrap();

        {
            let op = f.ext4.begin_op(8).unwrap();
            em.ensure_allocated(10, 11, op.get()).unwrap();
        }
        // `ensure_allocated` yields unwritten extents (Unwritten-first); the
        // point here is that the reused leaf was captured and the mapping is
        // served (not `EIO`/`EUCLEAN`), for both the new and pre-flush extents.
        let m = em.map_blocks(10).unwrap();
        assert_eq!(m.state(), MapState::Unwritten);
        // The pre-flush extents survived the in-place rewrite.
        let m = em.map_blocks(0).unwrap();
        assert_eq!(m.state(), MapState::Unwritten);
    }
}
