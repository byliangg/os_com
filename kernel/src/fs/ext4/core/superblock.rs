// SPDX-License-Identifier: MPL-2.0
use ostd::const_assert;

use super::prelude::*;

const SUPERBLOCK_SIZE: usize = 1024;
const EXT4_MAGIC: u16 = 0xEF53;
const INCOMPAT_EXTENTS: u32 = 0x40;
const COMPAT_HAS_JOURNAL: u32 = 0x4;

/// ext4 on-disk 超级块（1024 字节，小端）。逐字段镜像磁盘布局。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub struct RawSuperblock {
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
    /// 组描述符尺寸：有 desc_size 用之（64bit），否则 32。
    pub fn group_desc_size(&self) -> usize {
        if self.desc_size >= 32 {
            self.desc_size as usize
        } else {
            32
        }
    }
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::{EXT4_MAGIC, RawSuperblock};
    use crate::fs::ext4::core::test_util::slice_at;
    use crate::prelude::*;

    const SB_OFFSET: usize = 1024;
    const SB_SIZE: usize = 1024;

    #[ktest]
    fn superblock_roundtrip_and_magic() {
        let bytes = slice_at(SB_OFFSET, SB_SIZE);
        let raw = RawSuperblock::from_bytes(bytes);
        assert_eq!(raw.as_bytes(), bytes, "Pod round-trip byte-identical");
        assert_eq!(raw.magic(), EXT4_MAGIC, "ext4 magic");
        assert!(raw.block_size().is_power_of_two());
    }

    #[ktest]
    fn superblock_diff_old_public_fields() {
        let bytes = slice_at(SB_OFFSET, SB_SIZE);
        let raw = RawSuperblock::from_bytes(bytes);
        let old = ext4_rs::Ext4Superblock::from_bytes(bytes);
        assert_eq!(raw.inodes_count(), old.inodes_count);
        assert_eq!(raw.inode_size(), old.inode_size);
        assert_eq!(raw.desc_size(), old.desc_size);
    }
}
