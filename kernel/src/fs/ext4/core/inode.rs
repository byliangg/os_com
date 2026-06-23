// SPDX-License-Identifier: MPL-2.0
use ostd::const_assert;

use super::prelude::*;

/// ext4 on-disk inode 的 OS-dependent #2 区（Linux 变体，12 字节）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub(super) struct RawOsd2 {
    pub l_i_blocks_high: u16,
    pub l_i_file_acl_high: u16,
    pub l_i_uid_high: u16,
    pub l_i_gid_high: u16,
    pub l_i_checksum_lo: u16,
    pub l_i_reserved: u16,
}

/// ext4 on-disk inode（156 字节，小端）。逐字段镜像磁盘布局。
/// base 128B（mode..osd2）+ extra-isize 28B（i_extra_isize..i_version_hi）。
/// block:[u32;15] 兼作 extent 树根（接缝4，本阶段只保字节、不解释）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub(super) struct RawInode {
    pub mode: u16,
    pub uid: u16,
    pub size: u32,
    pub atime: u32,
    pub ctime: u32,
    pub mtime: u32,
    pub dtime: u32,
    pub gid: u16,
    pub links_count: u16,
    pub blocks: u32,
    pub flags: u32,
    pub osd1: u32,
    pub block: [u32; 15],
    pub generation: u32,
    pub file_acl: u32,
    pub size_hi: u32,
    pub faddr: u32,
    pub osd2: RawOsd2,
    pub i_extra_isize: u16,
    pub i_checksum_hi: u16,
    pub i_ctime_extra: u32,
    pub i_mtime_extra: u32,
    pub i_atime_extra: u32,
    pub i_crtime: u32,
    pub i_crtime_extra: u32,
    pub i_version_hi: u32,
}

const_assert!(size_of::<RawInode>() == 156);

const S_IFMT: u16 = 0xF000;
const S_IFDIR: u16 = 0x4000;

impl RawInode {
    pub fn mode(&self) -> u16 {
        self.mode
    }
    pub fn links_count(&self) -> u16 {
        self.links_count
    }
    /// 文件大小（size | size_hi<<32）。
    pub fn size(&self) -> u64 {
        (self.size as u64) | ((self.size_hi as u64) << 32)
    }
    /// 已分配块数（blocks | osd2.l_i_blocks_high<<32）。
    pub fn blocks(&self) -> u64 {
        (self.blocks as u64) | ((self.osd2.l_i_blocks_high as u64) << 32)
    }
    /// i_block 原始 60 字节（extent/间接的解释留 Phase 3）。
    pub fn i_block(&self) -> [u32; 15] {
        self.block
    }
    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::RawInode;
    use crate::fs::ext4::core::block_group::RawGroupDescriptor;
    use crate::fs::ext4::core::superblock::RawSuperblock;
    use crate::fs::ext4::core::test_util::slice_at;
    use crate::prelude::*;

    #[ktest]
    fn inode_roundtrip_handcrafted() {
        let mut bytes = [0u8; 156];
        bytes[0..2].copy_from_slice(&0o100644u16.to_le_bytes()); // i_mode
        bytes[4..8].copy_from_slice(&4096u32.to_le_bytes()); // i_size_lo
        let raw = RawInode::from_bytes(&bytes);
        assert_eq!(raw.as_bytes(), &bytes[..]);
        assert_eq!(raw.mode(), 0o100644);
        assert_eq!(raw.size(), 4096);
    }

    #[ktest]
    fn inode_root_diff_old_real_image() {
        // 定位根 inode（ino=2）：组0描述符的 inode_table 块 + (2-1)*inode_size。
        let sb = RawSuperblock::from_bytes(slice_at(1024, 1024));
        let bs = sb.block_size();
        let gdt_off = (sb.first_data_block as usize + 1) * bs;
        let gd = RawGroupDescriptor::from_bytes(slice_at(gdt_off, 64));
        let inode_size = sb.inode_size() as usize;
        let itable = gd.inode_table() as usize;
        let off = itable * bs + (2 - 1) * inode_size;
        let n = size_of::<RawInode>();
        let bytes = slice_at(off, n);
        let raw = RawInode::from_bytes(bytes);
        assert_eq!(raw.as_bytes(), bytes, "round-trip");
        // 根 inode 必是目录。
        assert!(raw.is_dir(), "root inode is a directory");
        // 全字段对拍旧实现（Ext4Inode 全字段 pub）。
        let old = ext4_rs::Ext4Inode::from_bytes(bytes);
        assert_eq!(raw.mode, old.mode, "mode");
        assert_eq!(raw.uid, old.uid, "uid");
        assert_eq!(raw.size, old.size, "size_lo");
        assert_eq!(raw.gid, old.gid, "gid");
        assert_eq!(raw.links_count, old.links_count, "links_count");
        assert_eq!(raw.blocks, old.blocks, "blocks");
        assert_eq!(raw.flags, old.flags, "flags");
        assert_eq!(raw.block, old.block, "i_block[15]");
        assert_eq!(raw.generation, old.generation, "generation");
        assert_eq!(raw.size_hi, old.size_hi, "size_hi");
    }
}
