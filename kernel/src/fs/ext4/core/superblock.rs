// SPDX-License-Identifier: MPL-2.0
use ostd::const_assert;

use super::crc::{ext4_crc32c, EXT4_CRC32_INIT};
use super::prelude::*;

const SUPERBLOCK_SIZE: usize = 1024;
const EXT4_MAGIC: u16 = 0xEF53;
const INCOMPAT_EXTENTS: u32 = 0x40;
const COMPAT_HAS_JOURNAL: u32 = 0x4;
/// `EXT4_FEATURE_INCOMPAT_RECOVER`（journal "needs_recovery" / dirty-log 标志）。
/// [对照] ext4_rs `EXT4_FEATURE_INCOMPAT_RECOVER`（consts.rs:40 == 0x0004）。
const INCOMPAT_RECOVER: u32 = 0x0004;

/// crc32c 覆盖范围上界（与 ext4_rs `sync_to_disk_with_csum` 的 0x3fc 一致）。
/// 超级块 `checksum` 字段位于偏移 0x3fc，校验和覆盖 `[0, 0x3fc)`，恰好排除自身。
const SUPERBLOCK_CSUM_LEN: usize = 0x3fc;

/// ext4 on-disk 超级块（1024 字节，小端）。逐字段镜像磁盘布局。
#[repr(C)]
// 不派生 Default：超级块含 >32 元素数组（[u8;64]/[u32;100] 等），Rust 数组 Default 仅到 32。
// 本阶段一律经 Pod `from_bytes` 解析，无需 Default。
#[derive(Clone, Copy, Debug, Pod)]
pub(in crate::fs::ext4) struct RawSuperblock {
    pub inodes_count: u32,
    pub blocks_count_lo: u32,
    pub reserved_blocks_count_lo: u32,
    pub free_blocks_count_lo: u32,
    pub free_inodes_count: u32,
    pub first_data_block: u32,
    pub log_block_size: u32,
    pub log_cluster_size: u32,
    pub blocks_per_group: u32,
    pub frags_per_group: u32,
    pub inodes_per_group: u32,
    pub mount_time: u32,
    pub write_time: u32,
    pub mount_count: u16,
    pub max_mount_count: u16,
    pub magic: u16,
    pub state: u16,
    pub errors: u16,
    pub minor_rev_level: u16,
    pub last_check_time: u32,
    pub check_interval: u32,
    pub creator_os: u32,
    pub rev_level: u32,
    pub def_resuid: u16,
    pub def_resgid: u16,
    pub first_inode: u32,
    pub inode_size: u16,
    pub block_group_index: u16,
    pub features_compatible: u32,
    pub features_incompatible: u32,
    pub features_read_only: u32,
    pub uuid: [u8; 16],
    pub volume_name: [u8; 16],
    pub last_mounted: [u8; 64],
    pub algorithm_usage_bitmap: u32,
    pub s_prealloc_blocks: u8,
    pub s_prealloc_dir_blocks: u8,
    pub s_reserved_gdt_blocks: u16,
    pub journal_uuid: [u8; 16],
    pub journal_inode_number: u32,
    pub journal_dev: u32,
    pub last_orphan: u32,
    pub hash_seed: [u32; 4],
    pub default_hash_version: u8,
    pub journal_backup_type: u8,
    pub desc_size: u16,
    pub default_mount_opts: u32,
    pub first_meta_bg: u32,
    pub mkfs_time: u32,
    pub journal_blocks: [u32; 17],
    pub blocks_count_hi: u32,
    pub reserved_blocks_count_hi: u32,
    pub free_blocks_count_hi: u32,
    pub min_extra_isize: u16,
    pub want_extra_isize: u16,
    pub flags: u32,
    pub raid_stride: u16,
    pub mmp_interval: u16,
    pub mmp_block: u64,
    pub raid_stripe_width: u32,
    pub log_groups_per_flex: u8,
    pub checksum_type: u8,
    pub reserved_pad: u16,
    pub kbytes_written: u64,
    pub snapshot_inum: u32,
    pub snapshot_id: u32,
    pub snapshot_r_blocks_count: u64,
    pub snapshot_list: u32,
    pub error_count: u32,
    pub first_error_time: u32,
    pub first_error_ino: u32,
    pub first_error_block: u64,
    pub first_error_func: [u8; 32],
    pub first_error_line: u32,
    pub last_error_time: u32,
    pub last_error_ino: u32,
    pub last_error_line: u32,
    pub last_error_block: u64,
    pub last_error_func: [u8; 32],
    pub mount_opts: [u8; 64],
    pub usr_quota_inum: u32,
    pub grp_quota_inum: u32,
    pub overhead_clusters: u32,
    pub backup_bgs: [u32; 2],
    pub encrypt_algos: [u8; 4],
    pub encrypt_pw_salt: [u8; 16],
    pub lpf_ino: u32,
    pub padding: [u32; 100],
    pub checksum: u32,
}

const_assert!(size_of::<RawSuperblock>() == SUPERBLOCK_SIZE);

impl RawSuperblock {
    /// magic（应为 0xEF53）。
    pub fn magic(&self) -> u16 {
        self.magic
    }
    pub fn inodes_count(&self) -> u32 {
        self.inodes_count
    }
    pub fn inode_size(&self) -> u16 {
        self.inode_size
    }
    pub fn desc_size(&self) -> u16 {
        self.desc_size
    }
    /// 块大小 = 1024 << log_block_size。
    pub fn block_size(&self) -> usize {
        1024usize << self.log_block_size
    }
    /// 总块数（lo | hi<<32）。
    pub fn blocks_count(&self) -> Ext4Fsblk {
        (self.blocks_count_lo as u64) | ((self.blocks_count_hi as u64) << 32)
    }
    /// 接缝1：是否 extent 映射（未置位 = ext2/3 间接路径，Phase 3 stub）。
    pub fn has_feature_extents(&self) -> bool {
        self.features_incompatible & INCOMPAT_EXTENTS != 0
    }
    pub fn has_journal(&self) -> bool {
        self.features_compatible & COMPAT_HAS_JOURNAL != 0
    }

    /// journal `needs_recovery` / dirty-log 标志（`EXT4_FEATURE_INCOMPAT_RECOVER`，incompat 位 0x4）。
    /// [对照] ext4_rs `Ext4Superblock::needs_recovery`（super_block.rs:268）。
    pub(in crate::fs::ext4) fn needs_recovery(&self) -> bool {
        self.features_incompatible & INCOMPAT_RECOVER != 0
    }

    /// 置 / 清 `needs_recovery` 标志（toggle incompat 位 0x4）。
    /// [对照] ext4_rs `Ext4Superblock::set_needs_recovery`（super_block.rs:288-294）。
    pub(in crate::fs::ext4) fn set_needs_recovery(&mut self, enabled: bool) {
        if enabled {
            self.features_incompatible |= INCOMPAT_RECOVER;
        } else {
            self.features_incompatible &= !INCOMPAT_RECOVER;
        }
    }

    /// journal inode 号（`s_journal_inum`）。挂载时解析 journal 文件物理块向量用。
    /// [对照] ext4_rs `Ext4Superblock::journal_inode_number`（super_block.rs:276）。
    pub(in crate::fs::ext4) fn journal_inode_number(&self) -> u32 {
        self.journal_inode_number
    }
    /// 组描述符尺寸：有 desc_size 用之（64bit），否则 32。
    pub fn group_desc_size(&self) -> usize {
        if self.desc_size >= 32 {
            self.desc_size as usize
        } else {
            32
        }
    }

    // ------------------------------------------------------------------
    // 分配器路径计数访问（与 ext4_rs `Ext4Superblock` 逐位一致）。
    //
    // 差分比的是落盘字节，而 alloc 路径「读计数→增/减→写回」，故 getter/setter
    // 必须与 ext4_rs 同语义，否则写回字节不同、差分失败。
    // [对照来源] ext4_rs/src/ext4_defs/super_block.rs:213-221, 131-133, 205-211
    // ------------------------------------------------------------------

    /// 空闲块计数（lo | hi<<32）。
    /// [对照] ext4_rs `free_blocks_count`（super_block.rs:213）。
    pub(in crate::fs::ext4) fn free_blocks_count(&self) -> u64 {
        let (lo, hi) = (self.free_blocks_count_lo, self.free_blocks_count_hi);
        (lo as u64) | ((hi as u64) << 32)
    }

    /// 写空闲块计数（lo = v&0xffffffff, hi = v>>32）。
    /// [对照] ext4_rs `set_free_blocks_count`（super_block.rs:217）。
    pub(in crate::fs::ext4) fn set_free_blocks_count(&mut self, free_blocks: u64) {
        self.free_blocks_count_lo = (free_blocks & 0xffff_ffff) as u32;
        self.free_blocks_count_hi = (free_blocks >> 32) as u32;
    }

    /// 空闲 inode 计数。ext4_rs 超级块此处是**单 u32 字段、无 hi 半**（不同于组描述符）。
    /// [对照] ext4_rs `free_inodes_count`（super_block.rs:131）。
    pub(in crate::fs::ext4) fn free_inodes_count(&self) -> u32 {
        self.free_inodes_count
    }

    /// 写空闲 inode 计数（单 u32 字段）。ext4_rs 实际用 `increase/decrease_free_inodes_count`
    /// 做 ±1；此处提供直接 set 供 alloc 路径在算好新值后写回，落盘字节一致。
    /// [对照] ext4_rs `decrease_free_inodes_count`/`increase_free_inodes_count`（super_block.rs:205-211）。
    pub(in crate::fs::ext4) fn set_free_inodes_count(&mut self, free_inodes: u32) {
        self.free_inodes_count = free_inodes;
    }

    /// 当前 checksum 字段值（供测试/校验对拍）。
    pub(in crate::fs::ext4) fn checksum(&self) -> u32 {
        self.checksum
    }

    // ------------------------------------------------------------------
    // 组几何 / csum 计算所需的超级块字段读取（packed/数组先拷局部再用）。
    // ------------------------------------------------------------------

    /// 卷 UUID（128 位）。crc32c 种子。
    pub(in crate::fs::ext4) fn uuid(&self) -> [u8; 16] {
        self.uuid
    }
    /// RO-compat 特性位（含 metadata_csum 0x400）。
    pub(in crate::fs::ext4) fn features_read_only(&self) -> u32 {
        self.features_read_only
    }
    /// INCOMPAT 特性位（含 meta_bg 0x10）。
    pub(in crate::fs::ext4) fn features_incompatible(&self) -> u32 {
        self.features_incompatible
    }
    /// 首数据块号（first_data_block）。组几何 ±1 调整的判据。
    pub(in crate::fs::ext4) fn first_data_block(&self) -> u32 {
        self.first_data_block
    }
    /// 每组块数。
    pub(in crate::fs::ext4) fn blocks_per_group(&self) -> u32 {
        self.blocks_per_group
    }
    /// 每组 inode 数。
    pub(in crate::fs::ext4) fn inodes_per_group(&self) -> u32 {
        self.inodes_per_group
    }
    /// 在线增长保留的 GDT 块数。
    pub(in crate::fs::ext4) fn reserved_gdt_blocks(&self) -> u16 {
        self.s_reserved_gdt_blocks
    }
    /// 组总数：`ceil(blocks_count / blocks_per_group)`。
    /// [对照] ext4_rs `block_group_count`（super_block.rs:161-173）。
    pub(in crate::fs::ext4) fn block_group_count(&self) -> u32 {
        let blocks_count = self.blocks_count();
        let blocks_per_group = self.blocks_per_group as u64;
        let mut count = blocks_count / blocks_per_group;
        if blocks_count % blocks_per_group != 0 {
            count += 1;
        }
        count as u32
    }

    // ------------------------------------------------------------------
    // 块/inode 位图校验和（crc32c，种子 = crc32c(uuid)）。
    // [对照来源] ext4_rs/src/ext4_defs/super_block.rs:290-307
    // ------------------------------------------------------------------

    /// 块分配位图 crc32c：`crc32c(crc32c(INIT, uuid), bitmap[..blocks_per_group/8])`。
    /// [对照] ext4_rs `ext4_balloc_bitmap_csum`（super_block.rs:290-297）。
    pub(in crate::fs::ext4) fn balloc_bitmap_csum(&self, bitmap: &[u8]) -> u32 {
        let uuid = self.uuid;
        let len = (self.blocks_per_group / 8) as usize;
        let csum = ext4_crc32c(EXT4_CRC32_INIT, &uuid);
        ext4_crc32c(csum, &bitmap[..len])
    }

    /// inode 分配位图 crc32c：长度取 `(inodes_per_group + 7) / 8`。
    /// [对照] ext4_rs `ext4_ialloc_bitmap_csum`（super_block.rs:300-307）。
    pub(in crate::fs::ext4) fn ialloc_bitmap_csum(&self, bitmap: &[u8]) -> u32 {
        let uuid = self.uuid;
        let len = ((self.inodes_per_group + 7) / 8) as usize;
        let csum = ext4_crc32c(EXT4_CRC32_INIT, &uuid);
        ext4_crc32c(csum, &bitmap[..len])
    }

    /// 重算并写入超级块校验和：`crc32c(INIT, &self_bytes[0, 0x3fc))` → `checksum` 字段。
    /// 安全实现：序列化全 1024 字节、对前 0x3fc 字节求 crc，再写回 `checksum`（不取 packed 引用）。
    /// [对照] ext4_rs `sync_to_disk_with_csum`（super_block.rs:230-241）的 csum 计算半部。
    pub(in crate::fs::ext4) fn recompute_csum(&mut self) {
        let bytes = self.as_bytes();
        let csum = ext4_crc32c(EXT4_CRC32_INIT, &bytes[..SUPERBLOCK_CSUM_LEN]);
        self.checksum = csum;
    }
}
