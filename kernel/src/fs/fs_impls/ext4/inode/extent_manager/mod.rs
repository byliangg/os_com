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

use aster_block::bio::BioDirection;
// `Segment` is re-exported by the module prelude only under `ktest`; the
// prefetch override below needs it in production builds too.
#[cfg(not(ktest))]
use ostd::mm::Segment;
// The writeback override snapshots each page into a to-device segment, which
// needs `reader`/`writer` from this trait (as the single-page write path does).
use ostd::mm::io::util::HasVmReaderWriter;

use super::{
    super::{checksum::InodeCsumSeed, journal, prelude::*},
    RAW_BLOCK_PTRS_LEN,
};
use crate::vm::page_cache::{
    CachePage, CachePageExt, LockedCachePage, read_run_complete_fn, write_run_complete_fn,
};

mod es;
mod node;
mod path;
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

/// Whether [`fill_holes`](ExtentManager::fill_holes) allocates the whole
/// requested range in the caller's single transaction or stops at a credit
/// boundary so the caller can restart onto a fresh one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AllocBound {
    /// Allocate every hole in one transaction; an overflow is the loud
    /// [`charge_fresh_capture`](journal) `ENOSPC` backstop. Used by the
    /// single-transaction paths (`prepare_write`, non-journaled writes).
    WholeRange,
    /// Stop before an insert that would overflow the transaction, returning the
    /// block reached; the unbounded write spine restarts and continues.
    CreditChunk,
}

/// The outcome of a credit-bounded [`fill_holes`](ExtentManager::fill_holes):
/// the whole requested range was allocated, or the fill stopped at a credit
/// boundary and reports where it stopped and the reservation the insert that
/// did not fit needs.
pub(super) enum HoleFill {
    /// Every hole in the requested range was allocated; its end block was
    /// reached. Whole-range mode always reports this (it never early-stops).
    Filled,
    /// A credit-aware early stop (chunked mode only) halted before the range
    /// end. `reached` is the first logical block NOT allocated — equal to the
    /// range start when even the first insert did not fit, greater when partial
    /// progress was made. `need` is the reservation the insert that triggered
    /// the stop requires (the in-place insert's O(depth) bound plus the
    /// per-chunk inode descriptor / convert, `chunk_insert_credits`), so the
    /// write spine restarts onto a fresh transaction reserving exactly it
    /// rather than the smaller per-chunk `write_credits` estimate.
    Stopped { reached: Iblock, need: usize },
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
    csum_seed: Option<InodeCsumSeed>,
    /// The fallback allocation goal (the inode's own group's first block,
    /// [`Ext4::inode_goal_block`]): used when a hole has no in-range
    /// predecessor to hint locality from — files stay near their inodes
    /// instead of piling into group 0.
    inode_goal: Ext4Bid,
    /// The owning inode's revoke rule for its freed DATA blocks (Linux
    /// `get_default_free_blocks_flags`): [`Forget`](journal::DataForgetPolicy::Forget)
    /// for directories (journaled dir blocks) and symlinks (Linux-conservative
    /// slow-target revoke), [`PlainData`](journal::DataForgetPolicy::PlainData)
    /// for regular files. Fixed at inode type, threaded to the truncate free
    /// sites the same route as `csum_seed`.
    data_forget_policy: journal::DataForgetPolicy,
}

impl ExtentManager {
    /// Validates `root` (see [`ExtentTree::try_new`]) and builds the manager.
    pub(super) fn try_new(
        root: [u32; RAW_BLOCK_PTRS_LEN],
        sector_count: u64,
        fs: Weak<super::super::fs::Ext4>,
        npages: usize,
        csum_seed: Option<InodeCsumSeed>,
        inode_goal: Ext4Bid,
        data_forget_policy: journal::DataForgetPolicy,
    ) -> Result<Self> {
        Ok(Self {
            state: RwMutex::new(ExtentTree::try_new(root, sector_count)?),
            npages: AtomicUsize::new(npages),
            fs,
            csum_seed,
            inode_goal,
            data_forget_policy,
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

    /// Whether the extent tree's overwrite fast-path hint currently covers
    /// `[start, end)` — every block in that range is known mapped by a single
    /// written extent, so [`InodeInner::write_at`](super::super::InodeInner) may
    /// skip both the hole-fill probe and the unwritten→written convert (P9b
    /// knife 2). A one-shot ③ read.
    ///
    /// The caller MUST hold the owning inode's `inner` write lock (①) so the
    /// answer stays valid through the ensuing page-cache write: every tree
    /// mutation for this inode takes ① before touching the ③ tree and clears the
    /// hint, so the hint cannot flip between this read and the write —
    /// EXCEPT [`allocate_one`](Self::allocate_one), the `submit_write_bio` hole
    /// fallback, which writeback/commit threads reach with no ① held. That lone
    /// unlocked-① mutator cannot break this fast path today, for three
    /// independent reasons: its `insert` only fills a Gap, and a live hint's
    /// range is covered by one written extent (no Gap inside it to insert);
    /// `insert`'s entry invalidation can only CLEAR the hint, never forge
    /// coverage; and on a journaled volume the path fails `EIO` before touching
    /// the tree. **A future mmap-hole-writeback implementation (ledger:
    /// `mmap-hole-writeback`) must re-justify or remove this carve-out before
    /// letting that path allocate.** Distinct inodes never share an
    /// `ExtentManager`.
    pub(super) fn written_hint_covers(&self, start: Iblock, end: Iblock) -> bool {
        self.state.read().written_hint_covers(start, end)
    }

    /// Debug-only cross-check for the overwrite fast path: walks `[start, end)`
    /// read-only and returns whether every block is mapped by a WRITTEN extent
    /// (no hole, no unwritten). `write_at` asserts this whenever it took the
    /// `written_hint` shortcut, so a hint that outlived a mutation that should
    /// have cleared it turns a silent Unwritten-first violation into a loud
    /// ktest failure (ktest is a debug build) rather than latent corruption.
    #[cfg(debug_assertions)]
    pub(super) fn debug_range_all_written(&self, start: Iblock, end: Iblock) -> Result<bool> {
        let fs = self.fs()?;
        let tree = self.state.read();
        let end = end as u64;
        // Read-only walk (no side effects): track how far written coverage
        // reaches and whether any gap or unwritten extent breaks it.
        let mut covered_upto = start as u64;
        let mut gap_or_unwritten = false;
        tree.walk_range(&fs, start as u64..end, &mut |e| {
            let e_start = e.block() as u64;
            if e_start > covered_upto || e.is_unwritten() {
                gap_or_unwritten = true;
                return core::ops::ControlFlow::Break(());
            }
            covered_upto = covered_upto.max(e_start + e.len() as u64);
            core::ops::ControlFlow::Continue(())
        })?;
        Ok(!gap_or_unwritten && covered_upto >= end)
    }

    /// Returns a copy of the inode's 60-byte `i_block` (extent-tree root),
    /// snapshotted under the lock — the inode-writeback serialization boundary.
    pub(super) fn root_snapshot(&self) -> [u32; RAW_BLOCK_PTRS_LEN] {
        *self.state.read().root_bytes()
    }

    /// Returns the extent-tree depth (0 = inline leaf, 1 = one index level).
    ///
    /// Used by the write/truncate/reclaim credit estimates
    /// ([`Ext4::write_credits`](super::super::fs::Ext4)) to size the per-chunk
    /// extent-tree reservation to the tree's live depth.
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
    ///
    /// Allocates the WHOLE range in the caller's single transaction: if that
    /// captures more metadata than the transaction holds,
    /// [`charge_fresh_capture`](journal) fails it loud (`ENOSPC`). The chunked
    /// write spine uses [`ensure_allocated_chunk`](Self::ensure_allocated_chunk)
    /// instead, which stops at a credit boundary so the caller can restart.
    pub(super) fn ensure_allocated(
        &self,
        start_iblock: Iblock,
        end_iblock: Iblock,
        handle: Option<&journal::Handle>,
        new_mappings: &mut NewMappings,
    ) -> Result<()> {
        // Whole-range mode never early-stops (it presses on and lets
        // `charge_fresh_capture` be the backstop), so the fill always reports
        // `Filled`; the outcome is discarded. The runs it allocates are recorded
        // into `new_mappings` for a failed write's precise rollback.
        self.fill_holes(
            start_iblock,
            end_iblock,
            handle,
            AllocBound::WholeRange,
            new_mappings,
        )?;
        Ok(())
    }

    /// Allocates data blocks (as UNWRITTEN) for the holes in `[start_iblock,
    /// end_iblock)` up to a credit boundary, reporting either that the whole
    /// range was filled or where the credit-aware early stop halted and the
    /// reservation the insert that did not fit needs (see [`HoleFill`]).
    ///
    /// The unbounded write spine allocates, writes, and converts only the filled
    /// prefix this transaction and restarts for the rest: the chunk's inserts
    /// accumulate captures a single transaction eventually cannot hold, so
    /// the loop must be able to end a chunk BEFORE a capture would overflow —
    /// and the restart cannot run here, under the ExtentTree lock (③) the
    /// committer's ordered flush needs (it releases ③ by returning; the OUTER
    /// spine restarts, reserving the reported `need`). A [`HoleFill::Stopped`]
    /// whose `reached` equals `start_iblock` means even one insert did not fit;
    /// the caller restarts unless that insert's `need` exceeds a whole
    /// transaction's capacity — its `EFBIG` floor.
    pub(super) fn ensure_allocated_chunk(
        &self,
        start_iblock: Iblock,
        end_iblock: Iblock,
        handle: Option<&journal::Handle>,
        new_mappings: &mut NewMappings,
    ) -> Result<HoleFill> {
        self.fill_holes(
            start_iblock,
            end_iblock,
            handle,
            AllocBound::CreditChunk,
            new_mappings,
        )
    }

    /// Shared hole-filling core of [`ensure_allocated`](Self::ensure_allocated)
    /// and [`ensure_allocated_chunk`](Self::ensure_allocated_chunk): plans hole
    /// runs from a single snapshot of the current tree and allocates them as
    /// UNWRITTEN, honoring `bound` for whether to stop at a credit boundary.
    fn fill_holes(
        &self,
        start_iblock: Iblock,
        end_iblock: Iblock,
        handle: Option<&journal::Handle>,
        bound: AllocBound,
        new_mappings: &mut NewMappings,
    ) -> Result<HoleFill> {
        if start_iblock >= end_iblock {
            return Ok(HoleFill::Filled);
        }
        let fs = self.fs()?;
        let mut tree = self.state.write();

        // Plan hole runs from a snapshot of the current tree — a BOUNDED one
        // (P9a-T6): instead of flattening the whole tree into a Vec on every
        // write (an O(file-size) buffer — the SQLite mega-allocation), walk
        // only the extents overlapping the range and stream the gaps between
        // them into the hole list. Each hole carries its allocation `goal`
        // (the preceding extent's physical end, for locality); the first
        // hole's predecessor may live before the walked range, so it comes
        // from the landing search instead — its in-leaf predecessor, with a
        // predecessor in an earlier leaf falling back to the inode-affinity
        // goal (an allocator hint, not a correctness input; P9b-b2).
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
        // One descent, not two (P9b b-ext 1b): the old code opened with a
        // `find(start_iblock)` purely to seed the FIRST hole's goal with the
        // predecessor's physical end, then walked the range separately. Here the
        // walk streams the holes with a `None` leading goal, and the predecessor
        // is recovered by a single landing search AFTER the walk — only when a
        // leading hole at `start_iblock` actually needs it, so the common
        // (covered / no-leading-hole) case never pays a second descent.
        let mut holes: Vec<PlannedHole> = Vec::new();
        let mut cursor = start_iblock as u64;
        let mut last_phys_end: Option<Ext4Bid> = None;
        // R1 (es-cache population): every extent this planning walk visits is
        // a fact just proven under the ③ write lock — record it in passing,
        // at zero extra descent. The inserts below invalidate only the HOLES
        // they fill (disjoint from the visited extents), and a neighbour
        // merge preserves per-block truth, so these facts survive the fill.
        let es_cache = &tree.es_cache;
        tree.walk_range(
            &fs,
            start_iblock as u64..end_iblock as u64,
            &mut |e: &node::Extent| {
                es_cache.record(e);
                let e_start = e.block() as u64;
                if e_start > cursor {
                    holes.push(PlannedHole {
                        // Lossless: pushed only while `cursor < e_start`, and
                        // an extent's start key is a u32 block index.
                        run: HoleRun {
                            start: cursor as Iblock,
                            end: e_start as Iblock,
                        },
                        goal: last_phys_end,
                    });
                }
                cursor = cursor.max(e_start + e.len() as u64);
                last_phys_end = Some(e.start() + e.len() as Ext4Bid);
                core::ops::ControlFlow::Continue(())
            },
        )?;
        if cursor < end_iblock as u64 {
            holes.push(PlannedHole {
                // Lossless: guarded by `cursor < end_iblock` (an Iblock).
                run: HoleRun {
                    start: cursor as Iblock,
                    end: end_iblock,
                },
                goal: last_phys_end,
            });
        }

        // A hole beginning exactly at `start_iblock` has no in-range predecessor
        // (its goal came out `None`); its true predecessor is the extent just
        // before the range, which the landing search reports as `Gap`'s `prev`.
        // Recover it with the one descent the old code always paid up front —
        // now taken only when a leading hole exists (a `Covered` landing means
        // `start_iblock` is mapped, so there is no leading hole to fix, and the
        // condition below short-circuits before the search). A predecessor in an
        // earlier leaf (`prev == None`) keeps the inode-affinity fallback, as
        // before — the goal is an allocator hint, not a correctness input.
        if let Some(first) = holes.first_mut()
            && first.run.start == start_iblock
            && first.goal.is_none()
            && let path::Search::Gap { prev: Some(p), .. } = tree.find(&fs, start_iblock)?
        {
            first.goal = Some(p.start() + p.len() as Ext4Bid);
        }

        // Report the runs this fill will newly allocate — the true holes, never
        // a pre-existing extent, so never old data or a fallocate KEEP_SIZE
        // preallocation. A failed write's `rollback_write` frees exactly these
        // (ledger: fallocate-keepsize-swallowed-by-rollback). Recorded UP FRONT
        // so a mid-fill allocation error still hands the caller every run it may
        // have touched; freeing a planned run the fill never reached is a no-op
        // (the range stays a hole).
        new_mappings.runs.extend(holes.iter().map(|hole| hole.run));

        for hole in &holes {
            let mut ib = hole.run.start;
            // No in-range predecessor → the inode-affinity fallback (P9b-b2
            // goal rule 3); and the goal ADVANCES with each allocation
            // (rule 1) so a hole filled in several pieces stays physically
            // contiguous instead of re-hinting the same spot.
            let mut goal = hole.goal.unwrap_or(self.inode_goal);
            while ib < hole.run.end {
                // Credit-aware early stop (chunked mode only): if the NEXT
                // insert will not fit the handle's transaction even after
                // growing in place, stop with the progress made so far — `ib`
                // is fully allocated up to here — and let the OUTER write
                // spine restart onto a fresh transaction. Not restarting here
                // is the ③-drop red line: the restart's re-admission may wait,
                // which is illegal under this lock. Whole-range mode presses
                // on and lets `charge_fresh_capture` be the loud backstop; a
                // non-journaled volume (no handle) has no per-transaction
                // ceiling either way. The bound is the in-place insert's
                // O(depth) cost (P9a-T6) — the whole-tree reserialize bound
                // retired with the rebuild path it priced.
                if bound == AllocBound::CreditChunk
                    && let Some(h) = handle
                {
                    let need = fs.chunk_insert_credits(tree.depth());
                    if journal::try_reserve_next(h, need)? == journal::ExtendOutcome::NeedsRestart {
                        // Report the bound the reservation was checked against —
                        // not the smaller per-chunk `write_credits` — so the
                        // restart reserves enough for the insert that did not fit.
                        return Ok(HoleFill::Stopped { reached: ib, need });
                    }
                }
                // Cap each allocation request at the widest length an *unwritten*
                // extent can bias-encode. Without this, a full-group run of
                // `MAX_WRITTEN_LEN` (32768) blocks would encode as `len +
                // MAX_WRITTEN_LEN` = 65536, wrap the 16-bit `ee_len` to 0, and
                // silently drop the whole run on decode (Linux clamps identically
                // in `ext4_ext_map_blocks`).
                let want = (hole.run.end - ib).min(node::MAX_UNWRITTEN_LEN as u32);
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
                    // Fresh unwritten blocks never referenced by the tree:
                    // no journaled life is ending with this free, so it
                    // carries no revoke duty.
                    let _ = fs.free_blocks(
                        journal::BlockFreeAuth::without_revoke_duty(range.start, got),
                        handle,
                    );
                    return Err(err);
                }
                goal = range.end;
                ib += got;
            }
        }
        Ok(HoleFill::Filled)
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

    /// Converts every WRITTEN extent in `[start_iblock, end_iblock)` to
    /// unwritten (reads-as-zero) in place, keeping the physical mapping — the
    /// mirror of [`mark_range_written`](Self::mark_range_written), used by
    /// `fallocate` ZERO_RANGE over its block-aligned middle. Holes and
    /// already-unwritten extents are untouched.
    pub(super) fn mark_range_unwritten(
        &self,
        start_iblock: Iblock,
        end_iblock: Iblock,
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        if start_iblock >= end_iblock {
            return Ok(());
        }
        let fs = self.fs()?;
        self.state.write().mark_range_unwritten(
            &fs,
            start_iblock,
            end_iblock - start_iblock,
            handle,
            self.csum_seed,
        )
    }

    /// Plans a COLLAPSE_RANGE (`fallocate`) with no journal handle — reads the
    /// tree and computes the post-shift survivors and freed runs; delegates to
    /// [`ExtentTree::plan_collapse_range`]. The caller runs this BEFORE `begin_op`
    /// so a malformed-tree read fails without aborting the journal, then feeds the
    /// plan to [`apply_collapse_range`](Self::apply_collapse_range).
    pub(super) fn plan_collapse_range(
        &self,
        punch_start: Iblock,
        punch_stop: Iblock,
    ) -> Result<tree::CollapsePlan> {
        let fs = self.fs()?;
        self.state
            .read()
            .plan_collapse_range(&fs, punch_start, punch_stop)
    }

    /// Applies a planned COLLAPSE_RANGE under `handle`: frees the removed window's
    /// data blocks and rewrites the tree in one transaction; delegates to
    /// [`ExtentTree::apply_collapse_range`]. Any error here has captured journaled
    /// writes, so the caller aborts.
    pub(super) fn apply_collapse_range(
        &self,
        plan: tree::CollapsePlan,
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        let fs = self.fs()?;
        self.state.write().apply_collapse_range(
            &fs,
            plan,
            handle,
            self.csum_seed,
            self.data_forget_policy,
        )
    }

    /// Plans an INSERT_RANGE (`fallocate`) with no journal handle — reads the tree,
    /// validates the SHIFT_RIGHT overflow bound, and computes the shifted extents;
    /// delegates to [`ExtentTree::plan_insert_range`]. Run BEFORE `begin_op` so the
    /// benign overflow `EINVAL` never aborts the journal.
    pub(super) fn plan_insert_range(
        &self,
        offset: Iblock,
        len: Iblock,
    ) -> Result<tree::InsertPlan> {
        let fs = self.fs()?;
        self.state.read().plan_insert_range(&fs, offset, len)
    }

    /// Applies a planned INSERT_RANGE under `handle`: rewrites the tree in one
    /// transaction; delegates to [`ExtentTree::apply_insert_range`]. Any error here
    /// has captured journaled writes, so the caller aborts.
    pub(super) fn apply_insert_range(
        &self,
        plan: tree::InsertPlan,
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        let fs = self.fs()?;
        self.state
            .write()
            .apply_insert_range(&fs, plan, handle, self.csum_seed)
    }

    /// The tree's current external-node count — the input the collapse/insert
    /// credit gate reserves against (a whole-tree rebuild captures one write per
    /// external node). Zero for a depth-0 inline tree.
    pub(super) fn external_node_count(&self) -> Result<usize> {
        let fs = self.fs()?;
        // `keep_blocks = u32::MAX` frees nothing, so the walk yields only the
        // external-node count (the shape input we want).
        Ok(self
            .state
            .read()
            .shrink_shape(&fs, Iblock::MAX)?
            .external_nodes)
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
        let range = fs.alloc_blocks(1, self.inode_goal, None)?;
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
            // Free the just-allocated block rather than leak it (a data
            // block that never entered the tree: no journaled life ends
            // with this free, so it carries no revoke duty).
            let _ = fs.free_blocks(journal::BlockFreeAuth::without_revoke_duty(pblock, 1), None);
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
        self.state.write().truncate_to_byte_len(
            &fs,
            new_size,
            handle,
            self.csum_seed,
            self.data_forget_policy,
        )
    }

    /// Frees exactly the blocks a failed write newly allocated — the runs
    /// [`ensure_allocated`](Self::ensure_allocated) recorded into `new_mappings`
    /// — restoring them to holes, and skipping any run below `keep_blocks`.
    ///
    /// Only the write's own fresh allocations are recorded, so pre-existing
    /// extents — old file data and any `fallocate` KEEP_SIZE preallocation in or
    /// past the write range — are never touched (ledger:
    /// `fallocate-keepsize-swallowed-by-rollback`, the swallow the blunt
    /// `truncate_to_byte_len(old_size)` this replaces caused). `keep_blocks` is
    /// the first block fully past the pre-write `i_size`: a hole this write also
    /// filled BELOW EOF is left as a benign unwritten over-allocation (its dirty
    /// page keeps a backing block — see `write_at`'s cleanup), matching the old
    /// truncate, which only freed past the old size. Each surviving run is
    /// punched, so an edge that merged with adjacent preallocation is split back
    /// and no block outside the run is disturbed.
    pub(super) fn free_new_mappings(
        &self,
        new_mappings: &NewMappings,
        keep_blocks: Iblock,
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        for run in &new_mappings.runs {
            let start = run.start.max(keep_blocks);
            if start < run.end {
                self.punch_range(start, run.end, handle)?;
            }
        }
        Ok(())
    }

    /// One credit-bounded step of shrinking the tree to `new_size` bytes: frees
    /// a bounded batch of doomed tail extents IN PLACE (trimming the covering
    /// leaves and pruning emptied nodes) in the caller's transaction,
    /// returning the frontier the tree now references
    /// and the reservation the next chunk should start from (see
    /// [`ExtentTree::truncate_chunk`]). The
    /// restartable-truncate spine calls this in a `journal_restart` loop until
    /// the frontier reaches `keep_blocks(new_size)`; `max_credits` is the
    /// journal's per-transaction ceiling, the honest `EFBIG` floor.
    pub(super) fn truncate_chunk(
        &self,
        new_size: usize,
        handle: Option<&journal::Handle>,
        max_credits: usize,
    ) -> Result<TruncateChunk> {
        let fs = self.fs()?;
        self.state.write().truncate_chunk(
            &fs,
            new_size,
            handle,
            self.csum_seed,
            self.data_forget_policy,
            Some(max_credits),
        )
    }

    /// One credit-bounded step of freeing the mapped blocks in the middle logical
    /// range `[start_block, end_block)` (fallocate punch-hole), delegating to
    /// [`ExtentTree::punch_chunk`]. The punch spine calls this in a
    /// `journal_restart` loop until no doomed block remains; `max_credits` is the
    /// journal's per-transaction ceiling (the EFBIG floor).
    pub(super) fn punch_chunk(
        &self,
        start_block: Iblock,
        end_block: Iblock,
        handle: Option<&journal::Handle>,
        max_credits: usize,
    ) -> Result<PunchChunk> {
        let fs = self.fs()?;
        self.state.write().punch_chunk(
            &fs,
            start_block..end_block,
            handle,
            self.csum_seed,
            self.data_forget_policy,
            Some(max_credits),
        )
    }

    /// Frees the mapped blocks in `[start_block, end_block)` in ONE transaction —
    /// the non-journaled (or single-transaction) punch path, with no credit bound.
    pub(super) fn punch_range(
        &self,
        start_block: Iblock,
        end_block: Iblock,
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        let fs = self.fs()?;
        self.state.write().punch_chunk(
            &fs,
            start_block..end_block,
            handle,
            self.csum_seed,
            self.data_forget_policy,
            None,
        )?;
        Ok(())
    }

    /// One structural walk that routes a shrink to `new_size`: the whole-truncate
    /// credit estimate (the fast/slow gate) AND the chunked-spine EFBIG floor,
    /// so the caller can reject a genuinely un-splittable shrink BEFORE it
    /// mutates the inode. The walk ([`ExtentTree::shrink_shape`]) streams (P9a-T6,
    /// retiring the whole-tree flatten Vec): it counts the tree's exact external
    /// nodes for the gate's whole-tree reserialize shape and the doomed extents
    /// past `keep_blocks`, with O(1) memory.
    ///
    /// `floor_efbig` mirrors [`ExtentTree::truncate_chunk`]'s internal O(depth)
    /// floor exactly (`free_cost + (free_cost * depth + 2) > max` — one free plus
    /// the node write-backs / prune cascade it can trigger). Checking it here,
    /// before `prepare_shrink` lowers `i_size` and before `orphan_add`, is what
    /// lets a genuine EFBIG leave the in-memory inode unchanged (the "nothing
    /// changed" contract). It is only meaningful on the chunked route: when the
    /// whole truncate fits one transaction (`whole_estimate <= max`) there is no
    /// per-chunk floor.
    pub(super) fn plan_shrink(&self, new_size: usize, max_credits: usize) -> Result<ShrinkPlan> {
        let fs = self.fs()?;
        let keep_blocks = Iblock::try_from(new_size.div_ceil(BLOCK_SIZE))
            .map_err(|_| Error::with_message(Errno::EFBIG, "block index exceeds 32 bits"))?;
        let tree = self.state.read();
        // One structural walk yields the EXACT external-node count and the
        // doomed-extent count — the two shape inputs the gate reserves against.
        // Counting the real nodes (not a dense `ceil(extents / fanout)` lower
        // bound) is what keeps a sparse or depth-3+ tree from under-reserving
        // and misrouting a big truncate into a single transaction that then
        // stops mid-truncate on a tiny journal.
        let shape = tree.shrink_shape(&fs, keep_blocks)?;
        let revoke_per_block = fs
            .journal()
            .map(|j| j.revoke_entries_per_block())
            .unwrap_or(1);
        let whole_estimate = fs.whole_truncate_credit_bound(
            shape.external_nodes,
            shape.freed_extents,
            revoke_per_block,
        );
        // `freed_extents > 0` is the floor's `has_work` (a doomed extent or the
        // straddler both extend past `keep_blocks`): a shrink freeing nothing
        // (e.g. rounding within the last block) can never trip EFBIG.
        //
        // The per-chunk floor mirrors the in-place `truncate_chunk` engine's
        // own O(depth) floor (`free_cost + free_cost*depth + 2`), NOT the
        // retired whole-tree reserialize bound: the in-place spine frees a
        // bounded batch per chunk and prunes ≤ depth nodes, so a depth-2 file
        // on a small journal that the old formula wrongly rejected as EFBIG
        // now shrinks by chunking — the debt this surgery retires. (The
        // `whole_estimate` above stays whole-tree-sized: the single-
        // transaction fast path genuinely touches every freed node.)
        let free_cost = fs.extent_free_credits();
        let depth = tree.depth() as usize;
        let floor_efbig =
            shape.freed_extents > 0 && free_cost + (free_cost * depth + 2) > max_credits;
        Ok(ShrinkPlan {
            whole_estimate,
            floor_efbig,
        })
    }
}

/// The routing verdict for one shrink, from a single counting tree walk
/// ([`ExtentManager::plan_shrink`]).
pub(super) struct ShrinkPlan {
    /// Whole-truncate single-transaction credit upper bound — `> max_credits`
    /// routes to the chunked, orphan-protected spine, else the atomic fast path.
    pub(super) whole_estimate: usize,
    /// The chunked spine's per-chunk EFBIG floor is unfittable on the current
    /// tree: not even one free plus its O(depth) node write-backs fits a
    /// transaction.
    /// Meaningful only when `whole_estimate > max_credits` (the chunked route).
    pub(super) floor_efbig: bool,
}

/// The outcome of one [`ExtentManager::truncate_chunk`]: the frontier the tree
/// now references and the reservation the next chunk's fresh transaction should
/// start from.
pub(super) struct TruncateChunk {
    /// The lowest logical block boundary the survivor still fully retains:
    /// `keep_blocks(new_size)` when the truncate is complete, higher when a
    /// credit stop cut the chunk short (the outer spine restarts and calls
    /// again).
    pub(super) reached: Iblock,
    /// The reservation the outer spine hands `journal_restart` for the next
    /// chunk: one free PLUS the O(depth) node headroom its prune cascade and
    /// write-backs can need. Covering the free (not just the node writes) is
    /// what guarantees forward progress — `journal_restart` may rejoin the
    /// current, partly-captured transaction, so the reservation must alone
    /// satisfy the next chunk's first `free_cost + node_headroom` probe.
    /// Always ≤ `max_credits` (a tree that cleared the same EFBIG floor).
    pub(super) next_bound: usize,
}

/// The outcome of one [`ExtentManager::punch_chunk`]: whether doomed blocks
/// remain in the range (the outer spine restarts) and the reservation the next
/// chunk's fresh transaction should start from.
pub(super) struct PunchChunk {
    /// `true` when a credit stop left doomed extents un-freed; the outer spine
    /// `journal_restart`s (never under the ExtentTree lock — iron law 1) and
    /// calls again, converging because each chunk frees ≥ 1 extent.
    pub(super) more: bool,
    /// The reservation the outer spine hands `journal_restart` for the next
    /// chunk: one free PLUS the O(depth) node headroom, so the next
    /// chunk's first probe is satisfied by the reservation alone (see
    /// [`TruncateChunk::next_bound`]). Always ≤ `max_credits`.
    pub(super) next_bound: usize,
}

/// A contiguous run of unmapped logical blocks.
#[derive(Clone, Copy)]
struct HoleRun {
    start: Iblock,
    end: Iblock,
}

/// One hole the fill plan will allocate, with the goal its filler should hint:
/// the preceding extent's physical end for locality, `None` when no
/// predecessor is known (the allocator then picks).
struct PlannedHole {
    run: HoleRun,
    goal: Option<Ext4Bid>,
}

/// The logical block runs a single [`ensure_allocated`](ExtentManager::ensure_allocated)
/// pass newly allocated — the true holes it filled, never a pre-existing
/// extent. A failed write's `rollback_write` frees EXACTLY these (via
/// [`free_new_mappings`](ExtentManager::free_new_mappings)) instead of
/// truncating everything past the old size, so old file data and any
/// `fallocate` KEEP_SIZE preallocation in or past the write range survive the
/// rollback (ledger: `fallocate-keepsize-swallowed-by-rollback`).
#[derive(Default)]
pub(super) struct NewMappings {
    runs: Vec<HoleRun>,
}

/// The largest number of page-sized segments packed into one prefetch read BIO.
/// The virtio block queue refuses a BIO once its segment count reaches the
/// device's per-BIO limit (`QUEUE_SIZE - 2`, currently 62), so a run is kept
/// strictly below it; a longer physically contiguous extent is issued as several
/// back-to-back BIOs (the request queue may still merge adjacent ones).
const MAX_RUN_SEGMENTS: usize = 61;

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

    /// Coalesces a batched prefetch into as few BIOs as the extent map allows.
    ///
    /// `pages` is `(page index, locked uninitialized page)` sorted ascending but
    /// possibly with gaps (already-cached pages the caller skipped). Each maximal
    /// run of input-consecutive pages that maps to a physically contiguous
    /// written extent becomes one multi-segment read BIO (split at the per-BIO
    /// segment cap); an unwritten extent or a hole reads as zeros and is filled
    /// in process context with no device I/O. A gap in the input, a physical
    /// discontinuity, or a mapping-kind change all break a run.
    ///
    /// Best-effort like the single-page path: an out-of-bounds page, a mapping
    /// error, or a submission failure just leaves the page(s) uninitialized for
    /// the next synchronous reader to re-read.
    ///
    /// Lock discipline: this is called on the prefetch path with no inode `inner`
    /// held. It only consults the extent tree (via `map_blocks`, which takes the
    /// tree read lock) and submits through the fs read funnel — never the journal
    /// write funnel, never `inner`.
    fn submit_read_pages(&self, pages: Vec<(usize, LockedCachePage)>, io_batch: &mut IoBatch) {
        let Ok(fs) = self.fs() else {
            // Filesystem dropped: dropping `pages` releases every page lock,
            // leaving them uninitialized for on-demand re-read.
            return;
        };
        let npages = self.npages.load(Ordering::Acquire);
        let mut pages = pages.into_iter().peekable();

        while let Some((start_idx, first_page)) = pages.next() {
            // Out-of-bounds guard, mirroring `submit_read_bio`'s EINVAL: dropping
            // `first_page` here leaves the page uninitialized.
            if start_idx >= npages {
                continue;
            }
            let Ok(iblock) = Iblock::try_from(start_idx) else {
                continue;
            };
            let Ok(mapping) = self.map_blocks(iblock) else {
                continue;
            };

            match mapping {
                Mapping::Mapped {
                    pblock,
                    len,
                    written: true,
                } => {
                    // A device-backed run: physically contiguous for `len` blocks
                    // from `pblock`, capped by the per-BIO segment ceiling.
                    // Extend it only across input-consecutive pages — a gap would
                    // break both the segment layout and the physical contiguity.
                    let cap = (len as usize).min(MAX_RUN_SEGMENTS);
                    let mut segments = Vec::with_capacity(cap);
                    let mut run_pages = Vec::with_capacity(cap);
                    segments.push(BioSegment::new_from_segment(
                        Segment::from(first_page.deref().clone()).into(),
                        BioDirection::FromDevice,
                    ));
                    run_pages.push(first_page);
                    let mut next_idx = start_idx + 1;
                    while run_pages.len() < cap
                        && pages.peek().is_some_and(|&(idx, _)| idx == next_idx)
                    {
                        let (_, page) = pages.next().unwrap();
                        segments.push(BioSegment::new_from_segment(
                            Segment::from(page.deref().clone()).into(),
                            BioDirection::FromDevice,
                        ));
                        run_pages.push(page);
                        next_idx += 1;
                    }
                    // On a submission error the BIO drops both `segments` and the
                    // completion callback without invoking it, so the run's page
                    // locks are released and the pages stay uninitialized — the
                    // next reader re-reads them.
                    let complete_fn = read_run_complete_fn(run_pages);
                    let _ = fs.read_segments_async(pblock, segments, Some(complete_fn), io_batch);
                }
                Mapping::Mapped {
                    written: false,
                    len,
                    ..
                } => {
                    // Preallocated-unwritten: reads as zeros with no device I/O.
                    // Gather the input-consecutive pages the mapping covers and
                    // apply the zero transition in place (reusing the shared
                    // completion helper, which fills and marks them up to date).
                    let run_end = start_idx.saturating_add(len as usize);
                    let mut run_pages = vec![first_page];
                    let mut next_idx = start_idx + 1;
                    while next_idx < run_end
                        && pages.peek().is_some_and(|&(idx, _)| idx == next_idx)
                    {
                        run_pages.push(pages.next().unwrap().1);
                        next_idx += 1;
                    }
                    read_run_complete_fn(run_pages)(BioStatus::Zeros);
                }
                Mapping::Hole { .. } => {
                    // A hole reads as zeros; the mapping reports one block at a
                    // time, so fill just this page (the next hole page maps on its
                    // own next iteration).
                    read_run_complete_fn(vec![first_page])(BioStatus::Zeros);
                }
            }
        }
    }

    /// Coalesces a batched writeback into as few BIOs as the extent map allows —
    /// the write twin of [`submit_read_pages`](Self::submit_read_pages).
    ///
    /// `pages` is `(page index, dirty page)` sorted ascending but possibly with
    /// gaps (clean pages the caller skipped). Each maximal run of
    /// input-consecutive pages that maps to a physically contiguous written
    /// extent is snapshotted into one multi-segment write BIO (split at the
    /// per-BIO segment cap) with a single completion callback owning the run's
    /// pages. A gap in the input, a physical discontinuity (a fresh mapping),
    /// or the segment cap all break a run.
    ///
    /// A dirty page maps to a written extent in the steady state — the write path
    /// converts unwritten-first and pre-allocates in `prepare_write`. The
    /// remaining cases (an unwritten extent, a hole from an mmap-dirtied page, an
    /// out-of-bounds index, or a mapping error) each fall back to one page through
    /// the blanket single-page [`write_page_async`], which re-derives the mapping
    /// (allocating a hole) and reproduces the exact single-page state machine —
    /// never a silent drop.
    ///
    /// Unlike the best-effort prefetch, this carries `fsync` semantics: the first
    /// submission error is propagated after re-dirtying the run it failed to queue
    /// (the single-page submit-error arm), and the caller waits on the batch and
    /// propagates any completion error.
    ///
    /// Lock discipline: only the extent tree (via `map_blocks`, tree read lock)
    /// and the fs write-data funnel are touched — never the journal metadata
    /// funnel, never the inode `inner`.
    fn submit_write_pages(
        &self,
        pages: Vec<(usize, CachePage)>,
        io_batch: &mut IoBatch,
    ) -> Result<()> {
        let fs = self.fs()?;
        let npages = self.npages.load(Ordering::Acquire);
        let mut pages = pages.into_iter().peekable();

        while let Some((start_idx, first_page)) = pages.next() {
            // Identify a physically contiguous *written* run starting here. Any
            // other outcome routes this one page through the single-page path.
            let run = (start_idx < npages)
                .then(|| Iblock::try_from(start_idx).ok())
                .flatten()
                .and_then(|iblock| self.map_blocks(iblock).ok())
                .and_then(|mapping| match mapping {
                    Mapping::Mapped {
                        pblock,
                        len,
                        written: true,
                    } => Some((pblock, len)),
                    _ => None,
                });

            let Some((pblock, len)) = run else {
                // Single-page fallback (hole / unwritten / out-of-bounds / mapping
                // error): the blanket write path re-derives the mapping through
                // `submit_write_bio` — allocating for an mmap-dirtied hole — and
                // reproduces the single-page failure/completion semantics exactly.
                let locked_page = first_page.lock();
                <Self as PageCacheBackend>::write_page_async(
                    self,
                    start_idx,
                    locked_page,
                    io_batch,
                )?;
                continue;
            };

            // Gather the run's input-consecutive pages, capped by the per-BIO
            // segment ceiling and the extent's contiguous length. A gap breaks the
            // run: a skipped page would break both the segment layout and the
            // physical contiguity (page `start_idx + k` maps to `pblock + k`).
            let cap = (len as usize).min(MAX_RUN_SEGMENTS);
            let mut run_input: Vec<(usize, CachePage)> = Vec::with_capacity(cap);
            run_input.push((start_idx, first_page));
            let mut next_idx = start_idx + 1;
            while run_input.len() < cap && pages.peek().is_some_and(|&(idx, _)| idx == next_idx) {
                run_input.push(pages.next().unwrap());
                next_idx += 1;
            }

            // Snapshot each page into its own to-device DMA segment — the blanket
            // single-page write's segment construction — driving the identical
            // per-page state transitions (wait out in-flight writeback, snapshot,
            // set writing-back, set up-to-date, unlock).
            let mut segments = Vec::with_capacity(run_input.len());
            let mut run_pages: Vec<(usize, CachePage)> = Vec::with_capacity(run_input.len());
            for (idx, page) in run_input {
                let locked_page = page.lock();
                locked_page.wait_until_finish_writing_back();
                let bio_segment = BioSegment::alloc(1, BioDirection::ToDevice);
                bio_segment
                    .writer()
                    .unwrap()
                    .write(&mut locked_page.reader());
                locked_page.set_writing_back();
                locked_page.set_up_to_date();
                segments.push(bio_segment);
                run_pages.push((idx, locked_page.unlock()));
            }

            // Submit the run as one BIO. On a submission error the BIO drops both
            // `segments` and the completion callback without invoking it, so
            // re-dirty every page in the run (the single-page submit-error arm)
            // and propagate the first error — fsync must not report as durable a
            // write it never queued.
            let complete_fn = write_run_complete_fn(run_pages.clone());
            if let Err(e) = fs.write_segments_async(pblock, segments, Some(complete_fn), io_batch) {
                for (_, page) in &run_pages {
                    let locked_page = page.lock_guard();
                    locked_page.set_dirty();
                    locked_page.clear_writing_back();
                }
                return Err(e);
            }
        }
        Ok(())
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
        let em = ExtentManager::try_new(
            root,
            4 * 8,
            f.ext4.this(),
            4,
            None,
            0,
            journal::DataForgetPolicy::PlainData,
        )
        .unwrap();

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
        let em = ExtentManager::try_new(
            root,
            2 * 8,
            f.ext4.this(),
            2,
            None,
            0,
            journal::DataForgetPolicy::PlainData,
        )
        .unwrap();
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
        let em = ExtentManager::try_new(
            root,
            4 * 8,
            f.ext4.this(),
            8,
            None,
            0,
            journal::DataForgetPolicy::PlainData,
        )
        .unwrap();

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
            em.ensure_allocated(ib, ib + 1, op.get(), &mut NewMappings::default())
                .unwrap();
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

        let em = ExtentManager::try_new(
            inline_root(&[]),
            0,
            f.ext4.this(),
            0,
            None,
            0,
            journal::DataForgetPolicy::PlainData,
        )
        .unwrap();
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

        let em = ExtentManager::try_new(
            inline_root(&[]),
            0,
            f.ext4.this(),
            0,
            None,
            0,
            journal::DataForgetPolicy::PlainData,
        )
        .unwrap();
        grow_to_depth_1(&f, &em);

        // Commit + checkpoint: the leaf's capture retires, the device becomes
        // authoritative — the next mutation reuses the on-disk leaf in place
        // and must journal it itself.
        journal.flush_on_unmount().unwrap();

        {
            let op = f.ext4.begin_op(8).unwrap();
            em.ensure_allocated(10, 11, op.get(), &mut NewMappings::default())
                .unwrap();
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
