// SPDX-License-Identifier: MPL-2.0
use ostd::const_assert;

use super::prelude::*;

/// ext4 on-disk 组描述符（64 字节，小端，packed）。
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub struct RawGroupDescriptor {
    pub block_bitmap_lo: u32,
    pub inode_bitmap_lo: u32,
    pub inode_table_first_block_lo: u32,
    pub free_blocks_count_lo: u16,
    pub free_inodes_count_lo: u16,
    pub used_dirs_count_lo: u16,
    pub flags: u16,
    pub exclude_bitmap_lo: u32,
    pub block_bitmap_csum_lo: u16,
    pub inode_bitmap_csum_lo: u16,
    pub itable_unused_lo: u16,
    pub checksum: u16,
    pub block_bitmap_hi: u32,
    pub inode_bitmap_hi: u32,
    pub inode_table_first_block_hi: u32,
    pub free_blocks_count_hi: u16,
    pub free_inodes_count_hi: u16,
    pub used_dirs_count_hi: u16,
    pub itable_unused_hi: u16,
    pub exclude_bitmap_hi: u32,
    pub block_bitmap_csum_hi: u16,
    pub inode_bitmap_csum_hi: u16,
    pub reserved: u32,
}

const_assert!(size_of::<RawGroupDescriptor>() == 64);

impl RawGroupDescriptor {
    pub fn block_bitmap(&self) -> Ext4Fsblk {
        let (lo, hi) = (self.block_bitmap_lo, self.block_bitmap_hi);
        (lo as u64) | ((hi as u64) << 32)
    }
    pub fn inode_bitmap(&self) -> Ext4Fsblk {
        let (lo, hi) = (self.inode_bitmap_lo, self.inode_bitmap_hi);
        (lo as u64) | ((hi as u64) << 32)
    }
    pub fn inode_table(&self) -> Ext4Fsblk {
        let (lo, hi) = (self.inode_table_first_block_lo, self.inode_table_first_block_hi);
        (lo as u64) | ((hi as u64) << 32)
    }
    pub fn free_blocks_count(&self) -> u32 {
        let (lo, hi) = (self.free_blocks_count_lo, self.free_blocks_count_hi);
        (lo as u32) | ((hi as u32) << 16)
    }
    pub fn free_inodes_count(&self) -> u32 {
        let (lo, hi) = (self.free_inodes_count_lo, self.free_inodes_count_hi);
        (lo as u32) | ((hi as u32) << 16)
    }
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::RawGroupDescriptor;
    use crate::fs::ext4::core::superblock::RawSuperblock;
    use crate::fs::ext4::core::test_util::slice_at;

    #[ktest]
    fn group_desc_roundtrip_and_diff_old() {
        // 从超级块推导组描述符表偏移（兼容任意 block_size）。
        let sb = RawSuperblock::from_bytes(slice_at(1024, 1024));
        let bs = sb.block_size();
        let gdt_off = (sb.first_data_block as usize + 1) * bs;
        let n = size_of::<RawGroupDescriptor>();
        let bytes = slice_at(gdt_off, n);
        let raw = RawGroupDescriptor::from_bytes(bytes);
        assert_eq!(raw.as_bytes(), bytes);
        // 对拍旧实现公开字段（packed：先拷出再比）。
        let old = ext4_rs::Ext4BlockGroup::from_bytes(bytes);
        let (a, b) = (raw.block_bitmap_lo, old.block_bitmap_lo);
        assert_eq!(a, b, "block_bitmap_lo");
        let (a, b) = (raw.inode_table_first_block_lo, old.inode_table_first_block_lo);
        assert_eq!(a, b, "inode_table_first_block_lo");
    }
}
