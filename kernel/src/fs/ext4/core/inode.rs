// SPDX-License-Identifier: MPL-2.0
use ostd::const_assert;

use super::crc::{ext4_crc32c, EXT4_CRC32_INIT};
use super::prelude::*;
use super::superblock::RawSuperblock;

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

/// inode 元数据校验和（crc32c）——**纯函数**，逐字节复刻 ext4_rs
/// `Ext4Inode::get_inode_checksum`（`ext4_defs/inode.rs:425`）。
///
/// 计算前在**本地拷贝**上把两个 csum 字段清零（`osd2.l_i_checksum_lo` / `i_checksum_hi`），
/// **不**改入参 `raw`；要写回 lo/hi 用 [`write_inode_checksum_into`]。
///
/// 步骤（与 ext4_rs 一一对应）：
/// 1. 本地拷贝清零 csum lo/hi；
/// 2. `c = crc32c(INIT, uuid)`（16 字节）；
/// 3. `c = crc32c(c, inode_id.to_le_bytes())`（4 字节小端）；
/// 4. `c = crc32c(c, generation.to_le_bytes())`（4 字节小端）；
/// 5. 把 inode 的**前 0x9c = 156 字节**拷进 256 字节零缓冲（其余 100 字节保持 0）；
/// 6. `c = crc32c(c, &raw_data[..inode_size])`（覆盖 156 真实 + 100 零字节）；
/// 7. `if inode_size == 128 { c &= 0xFFFF }`。
pub(super) fn inode_checksum(raw: &RawInode, inode_id: u32, sb: &RawSuperblock) -> u32 {
    let inode_size = sb.inode_size() as usize;

    // 1) 本地拷贝（不动入参 raw），在拷贝上清零 csum lo/hi——与 ext4_rs 在算前
    //    把 `osd2.l_i_checksum_lo`/`i_checksum_hi` 置 0 一致（否则把旧 csum 算进去）。
    let mut work = *raw;
    work.osd2.l_i_checksum_lo = 0;
    work.i_checksum_hi = 0;

    // 2) crc32c(INIT, uuid)（16 字节）。
    let uuid = sb.uuid();
    let mut c = ext4_crc32c(EXT4_CRC32_INIT, &uuid);
    // 3) inode_id（4 字节小端）。
    c = ext4_crc32c(c, &inode_id.to_le_bytes());
    // 4) generation（4 字节小端）。
    c = ext4_crc32c(c, &work.generation.to_le_bytes());

    // 5) PARITY: replicate ext4_rs behavior, fix deferred (roadmap §5)
    //    ext4_rs `copy_to_slice`(inode.rs:417) 只拷 0x9c=156 字节进 256 字节零缓冲——
    //    这是实现细节（非 ext4 规范的整 inode），逐字节复刻。`RawInode` 恰 156 字节，
    //    故 `as_bytes()`（156 字节）即那 0x9c 字节；其余 100 字节保持 0。
    let mut raw_data = [0u8; 0x100];
    let work_bytes = work.as_bytes();
    raw_data[..work_bytes.len()].copy_from_slice(work_bytes);

    // 6) PARITY: replicate ext4_rs behavior, fix deferred (roadmap §5)
    //    crc 覆盖 `&raw_data[..inode_size]`——长度是 `sb.inode_size()`（本镜像=256），
    //    即 156 真实字节 + 100 零字节，而非仅 156。
    c = ext4_crc32c(c, &raw_data[..inode_size]);

    // 7) 128B inode 只有 lo 半。
    if inode_size == 128 {
        c &= 0xFFFF;
    }
    c
}

/// 算出 csum 后写回 `raw` 的 lo/hi 字段，对齐 ext4_rs `set_inode_checksum`（`inode.rs:461`）：
/// `osd2.l_i_checksum_lo = c & 0xFFFF`；`if inode_size > 128 { i_checksum_hi = c >> 16 }`。
#[allow(dead_code)]
pub(super) fn write_inode_checksum_into(raw: &mut RawInode, inode_id: u32, sb: &RawSuperblock) {
    let c = inode_checksum(raw, inode_id, sb);
    raw.osd2.l_i_checksum_lo = (c & 0xFFFF) as u16;
    if sb.inode_size() > 128 {
        raw.i_checksum_hi = (c >> 16) as u16;
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
