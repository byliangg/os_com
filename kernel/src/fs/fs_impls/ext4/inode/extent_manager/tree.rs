// SPDX-License-Identifier: MPL-2.0

//! Extent-tree lookup: maps a logical block to its leaf extent.
//!
//! The tree root lives inline in the inode's 60-byte `i_block`. Interior nodes
//! hold index entries pointing to child blocks read from the device; leaf nodes
//! hold the extents that map logical blocks to physical runs. Phase 1 walks the
//! tree read-only.

use super::{
    super::super::prelude::*,
    node::{Extent, ExtentHeader, ExtentIdx, RawExtent, RawExtentHeader, RawExtentIdx},
};

/// Size of one extent-tree entry (header, index, or leaf), in bytes.
const ENTRY_SIZE: usize = 12;

/// Maximum extent-tree depth, mirroring `EXT4_MAX_EXTENT_DEPTH`.
const MAX_DEPTH: u32 = 5;

/// The outcome of searching a single extent-tree node for `iblock`.
enum Step {
    /// A leaf extent that covers `iblock`.
    Found(Extent),
    /// No extent covers `iblock`: a hole.
    Hole,
    /// An interior node points to a child at this physical block.
    Descend(Ext4Bid),
}

/// Searches one node's bytes for the entry covering `iblock`.
///
/// Entries are sorted by logical block, so the covering entry is the last one
/// whose starting block is `<= iblock`. Phase 1 scans linearly (nodes hold at
/// most a few hundred entries); a binary search is a later optimization.
fn search_node(bytes: &[u8], iblock: Iblock) -> Result<Step> {
    let header = ExtentHeader::try_from(&RawExtentHeader::from_bytes(&bytes[0..ENTRY_SIZE]))?;
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
            // `iblock` lies before the first index entry: a hole.
            None => Ok(Step::Hole),
        }
    }
}

/// Walks the extent tree rooted in `root` (the inode's `i_block`) to find the
/// extent covering `iblock`, returning `None` for a hole.
pub(super) fn find_extent(
    root: &[u32; super::super::RAW_BLOCK_PTRS_LEN],
    device: &dyn BlockDevice,
    iblock: Iblock,
) -> Result<Option<Extent>> {
    let mut next_bid = match search_node(root.as_bytes(), iblock)? {
        Step::Found(extent) => return Ok(Some(extent)),
        Step::Hole => return Ok(None),
        Step::Descend(bid) => bid,
    };

    for _ in 0..MAX_DEPTH {
        let block = device.read_val::<[u8; BLOCK_SIZE]>(next_bid as usize * BLOCK_SIZE)?;
        match search_node(&block, iblock)? {
            Step::Found(extent) => return Ok(Some(extent)),
            Step::Hole => return Ok(None),
            Step::Descend(bid) => next_bid = bid,
        }
    }
    return_errno_with_message!(Errno::EUCLEAN, "extent tree deeper than maximum depth");
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::{super::node::EXTENT_MAGIC, *};
    use crate::fs::fs_impls::ext4::test_utils::Ext4FixtureBuilder;

    /// Writes a depth-0 extent root (header + extents) into a 60-byte `i_block`.
    fn inline_root(extents: &[RawExtent]) -> [u32; super::super::super::RAW_BLOCK_PTRS_LEN] {
        let mut block = [0u32; super::super::super::RAW_BLOCK_PTRS_LEN];
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
        block
    }

    #[ktest]
    fn inline_single_extent_lookup() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let device = f.ext4.block_device().as_ref();
        // One extent mapping logical 0..4 to physical 100..104.
        let root = inline_root(&[RawExtent {
            block: 0,
            len: 4,
            start_hi: 0,
            start_lo: 100,
        }]);

        let mapped = find_extent(&root, device, 2).unwrap().unwrap();
        assert_eq!(mapped.start(), 100);
        assert_eq!(mapped.block(), 0);

        // Block 4 is beyond the extent: a hole.
        assert!(find_extent(&root, device, 4).unwrap().is_none());
    }

    #[ktest]
    fn inline_multiple_extents_lookup() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let device = f.ext4.block_device().as_ref();
        let root = inline_root(&[
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
        let mapped = find_extent(&root, device, 6).unwrap().unwrap();
        assert_eq!(mapped.start() + (6 - mapped.block()) as u64, 301);

        // Logical 3 falls in the gap between the two extents: a hole.
        assert!(find_extent(&root, device, 3).unwrap().is_none());
    }

    #[ktest]
    fn empty_root_is_all_holes() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let device = f.ext4.block_device().as_ref();
        let root = inline_root(&[]);
        assert!(find_extent(&root, device, 0).unwrap().is_none());
    }

    /// Writes a depth-1 index root into a 60-byte `i_block`, pointing at a single
    /// external leaf node at physical block `leaf_block`.
    fn index_root(leaf_block: u32) -> [u32; super::super::super::RAW_BLOCK_PTRS_LEN] {
        let mut block = [0u32; super::super::super::RAW_BLOCK_PTRS_LEN];
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
        block
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
        let device = f.ext4.block_device().as_ref();

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
        let root = index_root(leaf_block);

        // Logical 1 → descend to the leaf → first extent (0..2) → physical 301.
        let m0 = find_extent(&root, device, 1).unwrap().unwrap();
        assert_eq!(m0.start() + (1 - m0.block()) as u64, 301);
        // Logical 6 → second extent (5..8) → physical 401.
        let m1 = find_extent(&root, device, 6).unwrap().unwrap();
        assert_eq!(m1.start() + (6 - m1.block()) as u64, 401);
        // Logical 3 → gap between the leaf's extents → hole.
        assert!(find_extent(&root, device, 3).unwrap().is_none());
        // Logical 100 → beyond all extents → hole.
        assert!(find_extent(&root, device, 100).unwrap().is_none());
    }
}
