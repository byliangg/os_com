// SPDX-License-Identifier: MPL-2.0
use ostd::const_assert;

use super::prelude::*;

/// 目录项尾标识：reserved_ft == 0xDE 表示该槽是目录块尾校验和结构。
const DIR_TAIL_MARKER: u8 = 0xDE;

/// ext4_rs `size_of::<Ext4DirEntry>()` 的值（`#[repr(C)]`：u32+u16+u8+u8+`[u8;255]`
/// → 263 → 对齐到 264）。**这是内存结构尺寸泄漏进磁盘布局的关键 parity 常量**：
/// ext4_rs 的 `try_insert_to_existing_block` / `insert_to_new_block` 用它作
/// ① 插入所需空间下界 `required_len = align4(264 + name.len())`、② 写新项时整 264 字节
/// `copy_to_slice`（name 尾零填到 255 + 对齐）。Task 2 的切槽/写项必须逐字复刻 264。
// PARITY: size_of::<ext4_rs Ext4DirEntry> 泄漏进磁盘布局
// Task 2 wires the slot-split / new-entry write path against this constant.
#[allow(dead_code)]
pub(super) const EXT4_DIR_ENTRY_INMEM_SIZE: usize = 264;

/// 目录项指向的 inode 类型（filetype 特性下的 file_type 字节）。
/// 安全替换旧实现里的 `union Ext4DirEnInternal`：当成 1 字节 + 按 filetype 解释。
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirEntryFileType {
    Unknown = 0,
    RegFile = 1,
    Dir = 2,
    Chrdev = 3,
    Blkdev = 4,
    Fifo = 5,
    Sock = 6,
    Symlink = 7,
}

impl From<u8> for DirEntryFileType {
    fn from(v: u8) -> Self {
        match v {
            1 => Self::RegFile,
            2 => Self::Dir,
            3 => Self::Chrdev,
            4 => Self::Blkdev,
            5 => Self::Fifo,
            6 => Self::Sock,
            7 => Self::Symlink,
            _ => Self::Unknown,
        }
    }
}

/// ext4 目录项定长头（8 字节，小端）。名字是其后的变长尾（name_len 字节），
/// 不整体 Pod 化变长结构（旧实现含 255 字节 name + union）。变长尾的解析留 Phase 4。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub(super) struct RawDirEntryHeader {
    pub inode: u32,
    pub rec_len: u16,
    pub name_len: u8,
    pub file_type: u8,
}
const_assert!(size_of::<RawDirEntryHeader>() == 8);

/// 目录块尾的校验和结构（12 字节）。占用一个普通目录项槽，靠 reserved_ft==0xDE 识别。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub(super) struct RawDirEntryTail {
    pub reserved_zero1: u32,
    pub rec_len: u16,
    pub reserved_zero2: u8,
    pub reserved_ft: u8,
    pub checksum: u32,
}
const_assert!(size_of::<RawDirEntryTail>() == 12);

impl RawDirEntryHeader {
    pub fn inode(&self) -> u32 {
        self.inode
    }
    pub fn rec_len(&self) -> u16 {
        self.rec_len
    }
    pub fn name_len(&self) -> u8 {
        self.name_len
    }
    pub fn file_type(&self) -> DirEntryFileType {
        DirEntryFileType::from(self.file_type)
    }
    /// inode==0 表示未使用的空槽。
    pub fn is_unused(&self) -> bool {
        self.inode == 0
    }
}

impl RawDirEntryTail {
    /// 是否目录块尾结构（reserved_ft==0xDE 且 inode 字段位置为 0）。
    pub fn is_tail(&self) -> bool {
        self.reserved_ft == DIR_TAIL_MARKER && self.reserved_zero1 == 0
    }
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::{DIR_TAIL_MARKER, DirEntryFileType, RawDirEntryHeader, RawDirEntryTail};
    use crate::fs::ext4::core::block_group::RawGroupDescriptor;
    use crate::fs::ext4::core::extents::{RawExtent, RawExtentHeader};
    use crate::fs::ext4::core::superblock::RawSuperblock;
    use crate::fs::ext4::core::test_util::slice_at;
    use crate::prelude::*;

    #[ktest]
    fn dirent_header_handcrafted_roundtrip() {
        let mut b = [0u8; 8];
        b[0..4].copy_from_slice(&2u32.to_le_bytes());
        b[4..6].copy_from_slice(&12u16.to_le_bytes());
        b[6] = 1;
        b[7] = 2;
        let h = RawDirEntryHeader::from_bytes(&b);
        assert_eq!(h.as_bytes(), &b[..]);
        assert_eq!(h.inode(), 2);
        assert_eq!(h.name_len(), 1);
        assert_eq!(h.file_type(), DirEntryFileType::Dir);
    }

    #[ktest]
    fn dirent_filetype_enum() {
        assert_eq!(DirEntryFileType::from(2), DirEntryFileType::Dir);
        assert_eq!(DirEntryFileType::from(1), DirEntryFileType::RegFile);
        assert_eq!(DirEntryFileType::from(99), DirEntryFileType::Unknown);
    }

    #[ktest]
    fn dirent_tail_roundtrip() {
        let mut t = RawDirEntryTail::default();
        t.reserved_ft = DIR_TAIL_MARKER;
        t.rec_len = 12;
        let bytes = t.as_bytes().to_vec();
        let t2 = RawDirEntryTail::from_bytes(&bytes);
        assert_eq!(t2.as_bytes(), &bytes[..]);
        assert!(t2.is_tail());
    }

    #[ktest]
    fn dirent_root_first_entry_real_image() {
        // 根 inode(ino=2) → extent → 首数据块 = 根目录块；首项是 "." 指向 inode 2。
        let sb = RawSuperblock::from_bytes(slice_at(1024, 1024));
        let bs = sb.block_size();
        let gd = RawGroupDescriptor::from_bytes(slice_at((sb.first_data_block as usize + 1) * bs, 64));
        let inode_off = gd.inode_table() as usize * bs + (2 - 1) * sb.inode_size() as usize;
        let eh = RawExtentHeader::from_bytes(slice_at(inode_off + 40, 12));
        assert!(eh.is_valid());
        assert!(eh.entries_count() >= 1);
        let ext = RawExtent::from_bytes(slice_at(inode_off + 40 + 12, 12));
        let dir_block = ext.start() as usize * bs;
        let de = RawDirEntryHeader::from_bytes(slice_at(dir_block, 8));
        assert_eq!(de.as_bytes(), slice_at(dir_block, 8));
        assert_eq!(de.inode(), 2, "root dir first entry '.' -> inode 2");
        assert_eq!(de.name_len(), 1);
        assert_eq!(de.file_type(), DirEntryFileType::Dir);
    }
}
