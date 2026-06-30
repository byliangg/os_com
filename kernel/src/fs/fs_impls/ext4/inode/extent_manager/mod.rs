// SPDX-License-Identifier: MPL-2.0

//! Ext4 extent block-mapping engine (the Phase 1 read path).
//!
//! This replaces ext2's indirect-block tree: it maps a file's logical blocks to
//! physical device blocks by walking an on-disk extent tree rooted inline in
//! the inode's `i_block`. `map_blocks` is the single translation entry point;
//! the `PageCache` backend that drives reads through it is wired up with the
//! file read path in a later task.
//!
//! Interior tree nodes are read directly from the device for now; a frame cache
//! (an allocation-free fast path for repeated lookups) is a later optimization.

use core::sync::atomic::{AtomicUsize, Ordering};

use super::{super::prelude::*, RAW_BLOCK_PTRS_LEN};

mod node;
mod tree;

/// State of a mapped logical block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MapState {
    /// Backed by written data on disk.
    Written,
    /// Allocated but never written; reads as zeros.
    Unwritten,
    /// Not allocated; reads as zeros.
    Hole,
}

/// The result of mapping a logical block: a contiguous physical run.
#[derive(Clone, Copy, Debug)]
pub(super) struct Mapping {
    pblock: Ext4Bid,
    len: u32,
    state: MapState,
}

impl Mapping {
    /// Returns the starting physical block of the run (meaningless for a `Hole`).
    pub(super) const fn pblock(&self) -> Ext4Bid {
        self.pblock
    }

    /// Returns the number of contiguous logical blocks this mapping describes.
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) const fn len(&self) -> u32 {
        self.len
    }

    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) const fn state(&self) -> MapState {
        self.state
    }

    /// Returns whether reading these blocks must return zeros without device I/O.
    pub(super) const fn reads_as_zeros(&self) -> bool {
        matches!(self.state, MapState::Hole | MapState::Unwritten)
    }
}

/// Maps an inode's logical blocks to physical blocks via its extent tree.
pub(super) struct ExtentManager {
    /// A copy of the inode's 60-byte `i_block` (the extent-tree root).
    root: [u32; RAW_BLOCK_PTRS_LEN],
    /// Back-reference to the filesystem, for the block device.
    fs: Weak<super::super::fs::Ext4>,
    /// Cached page count (kept for the `PageCache` backend wired up later).
    npages: AtomicUsize,
}

impl ExtentManager {
    pub(super) fn new(
        root: [u32; RAW_BLOCK_PTRS_LEN],
        fs: Weak<super::super::fs::Ext4>,
        npages: usize,
    ) -> Self {
        Self {
            root,
            fs,
            npages: AtomicUsize::new(npages),
        }
    }

    /// Maps logical block `iblock` to a physical run.
    ///
    /// The returned length spans from `iblock` to the end of the covering
    /// extent, so callers can batch contiguous reads.
    pub(super) fn map_blocks(&self, iblock: Iblock) -> Result<Mapping> {
        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem dropped"))?;
        let device = fs.block_device().as_ref();

        match tree::find_extent(&self.root, device, iblock)? {
            Some(extent) => {
                let offset_in_extent = iblock - extent.block();
                let pblock = extent.start() + offset_in_extent as Ext4Bid;
                let len = extent.len() as u32 - offset_in_extent;
                let state = if extent.is_unwritten() {
                    MapState::Unwritten
                } else {
                    MapState::Written
                };
                Ok(Mapping { pblock, len, state })
            }
            None => Ok(Mapping {
                pblock: 0,
                len: 1,
                state: MapState::Hole,
            }),
        }
    }
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

        let mapping = self.map_blocks(iblock)?;
        if mapping.reads_as_zeros() {
            // Holes and unwritten extents read as zeros without device I/O.
            complete_fn(BioStatus::Zeros);
            return Ok(());
        }

        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem dropped"))?;
        fs.read_blocks_async(mapping.pblock(), bio_segment, Some(complete_fn), io_batch)
    }

    fn submit_write_bio(
        &self,
        _idx: usize,
        _bio_segment: BioSegment,
        _complete_fn: BioCompleteFn,
        _io_batch: &mut IoBatch,
    ) -> Result<()> {
        // Phase 1 is read-only; writes are gated at the VFS entry points, but
        // the backend refuses them too.
        return_errno_with_message!(Errno::EROFS, "ext4 is read-only in phase 1")
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
        let em = ExtentManager::new(root, f.ext4.this(), 4);

        let m0 = em.map_blocks(0).unwrap();
        assert_eq!(m0.state(), MapState::Written);
        assert_eq!(m0.pblock(), 100);
        assert_eq!(m0.len(), 4);

        // Mapping from the middle returns the remaining run.
        let m2 = em.map_blocks(2).unwrap();
        assert_eq!(m2.pblock(), 102);
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
        let em = ExtentManager::new(root, f.ext4.this(), 2);
        let m = em.map_blocks(0).unwrap();
        assert_eq!(m.state(), MapState::Unwritten);
        assert!(m.reads_as_zeros());
        assert_eq!(m.pblock(), 500);
    }
}
