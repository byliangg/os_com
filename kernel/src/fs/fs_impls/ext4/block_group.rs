// SPDX-License-Identifier: MPL-2.0

//! Ext4 block-group descriptors.
//!
//! Phase 1 reads the 32-byte descriptors (the `64BIT` feature is off, so the
//! high halves are absent) to locate each group's inode table. Per-group
//! bitmaps, allocation, and the inode-table page cache arrive in later phases.

use super::prelude::*;

const_assert!(size_of::<RawBlockGroup>() == 32);

/// On-disk block-group descriptor, 32 bytes (without the `64BIT` high halves).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(super) struct RawBlockGroup {
    pub block_bitmap_lo: u32,
    pub inode_bitmap_lo: u32,
    pub inode_table_lo: u32,
    pub free_blocks_count_lo: u16,
    pub free_inodes_count_lo: u16,
    pub used_dirs_count_lo: u16,
    /// `bg_flags` (e.g. `INODE_UNINIT`, `BLOCK_UNINIT`, `INODE_ZEROED`).
    pub flags: u16,
    pub exclude_bitmap_lo: u32,
    pub block_bitmap_csum_lo: u16,
    pub inode_bitmap_csum_lo: u16,
    pub itable_unused_lo: u16,
    pub checksum: u16,
}

/// Validated, Rust-typed block-group descriptor.
///
/// Block numbers are `Ext4Bid` (`u64`) so the `64BIT` high halves slot in later
/// without widening; in Phase 1 they are the 32-bit low halves.
#[derive(Clone, Copy, Debug)]
pub(super) struct BlockGroupDesc {
    block_bitmap_bid: Ext4Bid,
    inode_bitmap_bid: Ext4Bid,
    inode_table_bid: Ext4Bid,
    free_blocks_count: u32,
    free_inodes_count: u32,
    used_dirs_count: u32,
}

impl BlockGroupDesc {
    /// Returns the starting block of this group's inode table.
    pub(super) const fn inode_table_bid(&self) -> Ext4Bid {
        self.inode_table_bid
    }

    #[expect(dead_code)]
    pub(super) const fn block_bitmap_bid(&self) -> Ext4Bid {
        self.block_bitmap_bid
    }

    #[expect(dead_code)]
    pub(super) const fn inode_bitmap_bid(&self) -> Ext4Bid {
        self.inode_bitmap_bid
    }

    #[expect(dead_code)]
    pub(super) const fn free_blocks_count(&self) -> u32 {
        self.free_blocks_count
    }

    #[expect(dead_code)]
    pub(super) const fn free_inodes_count(&self) -> u32 {
        self.free_inodes_count
    }

    #[expect(dead_code)]
    pub(super) const fn used_dirs_count(&self) -> u32 {
        self.used_dirs_count
    }
}

impl From<&RawBlockGroup> for BlockGroupDesc {
    fn from(raw: &RawBlockGroup) -> Self {
        Self {
            block_bitmap_bid: raw.block_bitmap_lo as Ext4Bid,
            inode_bitmap_bid: raw.inode_bitmap_lo as Ext4Bid,
            inode_table_bid: raw.inode_table_lo as Ext4Bid,
            free_blocks_count: raw.free_blocks_count_lo as u32,
            free_inodes_count: raw.free_inodes_count_lo as u32,
            used_dirs_count: raw.used_dirs_count_lo as u32,
        }
    }
}
