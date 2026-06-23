// SPDX-License-Identifier: MPL-2.0
use ostd::const_assert;

use super::prelude::*;

const EXTENT_MAGIC: u16 = 0xF30A;
/// block_count 高于此值表示 unwritten extent（实际长度 = block_count - 此值）。
const UNWRITTEN_MAX_LEN: u16 = 32768;

/// extent 树节点头（12 字节，小端）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub struct RawExtentHeader {
    pub magic: u16,
    pub entries_count: u16,
    pub max_entries_count: u16,
    pub depth: u16,
    pub generation: u32,
}
const_assert!(size_of::<RawExtentHeader>() == 12);

/// extent 树内部索引项（12 字节）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub struct RawExtentIndex {
    pub first_block: u32,
    pub leaf_lo: u32,
    pub leaf_hi: u16,
    pub padding: u16,
}
const_assert!(size_of::<RawExtentIndex>() == 12);

/// extent 叶子项（12 字节）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub struct RawExtent {
    pub first_block: u32,
    pub block_count: u16,
    pub start_hi: u16,
    pub start_lo: u32,
}
const_assert!(size_of::<RawExtent>() == 12);

/// 非根 extent 块尾的校验和（4 字节）。
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub struct RawExtentTail {
    pub et_checksum: u32,
}
const_assert!(size_of::<RawExtentTail>() == 4);

impl RawExtentHeader {
    pub fn magic(&self) -> u16 {
        self.magic
    }
    pub fn entries_count(&self) -> u16 {
        self.entries_count
    }
    pub fn depth(&self) -> u16 {
        self.depth
    }
    /// 是否合法 extent 头（magic == 0xF30A）。
    pub fn is_valid(&self) -> bool {
        self.magic == EXTENT_MAGIC
    }
}

impl RawExtentIndex {
    pub fn first_block(&self) -> u32 {
        self.first_block
    }
    /// 指向的下层块号（leaf_lo | leaf_hi<<32）。
    pub fn leaf(&self) -> Ext4Fsblk {
        (self.leaf_lo as u64) | ((self.leaf_hi as u64) << 32)
    }
}

impl RawExtent {
    pub fn first_block(&self) -> u32 {
        self.first_block
    }
    /// 是否 unwritten extent（block_count 高位标志）。
    pub fn is_unwritten(&self) -> bool {
        self.block_count > UNWRITTEN_MAX_LEN
    }
    /// 实际覆盖块数（去掉 unwritten 标志）。
    pub fn len(&self) -> u16 {
        if self.block_count > UNWRITTEN_MAX_LEN {
            self.block_count - UNWRITTEN_MAX_LEN
        } else {
            self.block_count
        }
    }
    /// 物理起始块号（start_lo | start_hi<<32）。
    pub fn start(&self) -> Ext4Fsblk {
        (self.start_lo as u64) | ((self.start_hi as u64) << 32)
    }
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::{EXTENT_MAGIC, RawExtent, RawExtentHeader};
    use crate::fs::ext4::core::block_group::RawGroupDescriptor;
    use crate::fs::ext4::core::superblock::RawSuperblock;
    use crate::fs::ext4::core::test_util::slice_at;
    use crate::prelude::*;

    #[ktest]
    fn extent_header_handcrafted_roundtrip() {
        let mut bytes = [0u8; 12];
        bytes[0..2].copy_from_slice(&EXTENT_MAGIC.to_le_bytes());
        bytes[2..4].copy_from_slice(&1u16.to_le_bytes());
        bytes[4..6].copy_from_slice(&4u16.to_le_bytes());
        let h = RawExtentHeader::from_bytes(&bytes);
        assert_eq!(h.as_bytes(), &bytes[..]);
        assert!(h.is_valid());
        assert_eq!(h.entries_count(), 1);
    }

    #[ktest]
    fn extent_unwritten_flag() {
        let mut e = RawExtent::default();
        e.block_count = 32768 + 5;
        assert!(e.is_unwritten());
        assert_eq!(e.len(), 5);
        e.block_count = 10;
        assert!(!e.is_unwritten());
        assert_eq!(e.len(), 10);
    }

    #[ktest]
    fn extent_root_header_real_image() {
        // 根 inode（ino=2）的 i_block 前 12 字节是 extent 头（根目录 extent-mapped）。
        let sb = RawSuperblock::from_bytes(slice_at(1024, 1024));
        let bs = sb.block_size();
        let gd = RawGroupDescriptor::from_bytes(slice_at((sb.first_data_block as usize + 1) * bs, 64));
        let inode_off = gd.inode_table() as usize * bs + (2 - 1) * sb.inode_size() as usize;
        // i_block 位于 inode 内偏移 40 起。
        let h_bytes = slice_at(inode_off + 40, 12);
        let h = RawExtentHeader::from_bytes(h_bytes);
        assert_eq!(h.as_bytes(), h_bytes);
        assert!(h.is_valid(), "root i_block must start with extent header magic 0xF30A");
        let old = ext4_rs::Ext4ExtentHeader::from_bytes(h_bytes);
        assert_eq!(h.magic, old.magic);
        assert_eq!(h.entries_count, old.entries_count);
        assert_eq!(h.depth, old.depth);
    }
}
