// SPDX-License-Identifier: MPL-2.0
use ostd::const_assert;

use super::crc::{ext4_crc32c, EXT4_CRC32_INIT};
use super::prelude::*;
use super::superblock::RawSuperblock;

/// 最小组描述符尺寸（32 字节，无 _hi 字段）。`desc_size > 此值` 才读写 _hi 半。
/// [对照] ext4_rs `EXT4_MIN_BLOCK_GROUP_DESCRIPTOR_SIZE`（consts.rs:34）。
pub(in crate::fs::ext4) const EXT4_MIN_DESC_SIZE: u16 = 32;
/// 最大组描述符尺寸（64 字节，含 _hi 字段）。位图 csum 高半仅在 `desc_size == 此值` 时写。
/// [对照] ext4_rs `EXT4_MAX_BLOCK_GROUP_DESCRIPTOR_SIZE`（consts.rs:35）。
pub(in crate::fs::ext4) const EXT4_MAX_DESC_SIZE: u16 = 64;

/// `features_read_only` 中的 metadata_csum 位（RO-compat 0x400）。门控位图 csum 写入。
const RO_COMPAT_METADATA_CSUM: u32 = 0x400;

/// `features_incompatible` 中的 meta_bg 位（INCOMPAT 0x10）。影响 GDT 块数计算。
/// [对照] ext4_rs `EXT4_FEATURE_INCOMPAT_META_BG`（balloc.rs:1012）。
const INCOMPAT_META_BG: u32 = 0x0010;

/// 组描述符 `checksum` 字段在结构内的字节偏移（crc16 计算时此处置 0）。
const GROUP_DESC_CHECKSUM_OFFSET: usize = 0x1e;

/// ext4 on-disk 组描述符（64 字节，小端，packed）。
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub(in crate::fs::ext4) struct RawGroupDescriptor {
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
    // ------------------------------------------------------------------
    // 块号定位（与 phase1 一致，desc_size==64 / 64bit 镜像上等价于 ext4_rs）。
    // 这些被 inode/dir/extents 解析路径已使用，保持原行为不动。
    // ------------------------------------------------------------------

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

    // ------------------------------------------------------------------
    // 分配器路径计数 get/set —— ext4 组描述符 lo/hi 字段编码（ext4-spec-correct）。
    //
    // 64BIT 特性下，free_blocks/free_inodes/used_dirs/itable_unused 计数都分
    // `_lo`（低 16 位）+ `_hi`（高 16 位，仅 desc_size>32 / 64BIT 时有效）两半，
    // 有效值 = `lo | (hi << 16)`。getter/setter 必须在 **bit 16** 处对称拆/拼。
    // Phase 7 Task 2 已修复 BUG-1/2/3（旧 ext4_rs getter 在 bit32 处错误重组 → hi 被丢；
    // set_used_dirs_count 误写 itable_unused 字段）。
    // [对照] Linux fs/ext4/ext4.h 的 ext4_free_inodes_count / ext4_free_group_clusters /
    //   ext4_used_dirs_count / ext4_itable_unused_count（lo | hi<<16）。
    // ------------------------------------------------------------------

    /// 取空闲块计数（u64）。lo/hi 在 **bit 16** 处拼接，与 [`set_free_blocks_count`] 的
    /// `hi = cnt >> 16` 拆分对称。
    /// [对照] ext4 组描述符 `bg_free_blocks_count_lo`（低 16 位）+ `bg_free_blocks_count_hi`
    ///   （高 16 位，64BIT 特性下有效），有效计数 = `lo | (hi << 16)`（Linux `ext4_free_group_clusters`）。
    ///   修复 BUG-2：旧 ext4_rs getter 在 bit32 处重组且仅 hi!=0 才并入，与 setter 的 >>16 拆分
    ///   不对称——hi!=0 的大 fs 上读数错乱。此处统一 bit16 拼接。
    pub(in crate::fs::ext4) fn get_free_blocks_count(&self) -> u64 {
        let lo = self.free_blocks_count_lo;
        let hi = self.free_blocks_count_hi;
        (lo as u64) | ((hi as u64) << 16)
    }

    /// 写空闲块计数（lo = cnt&0xffff, hi = cnt>>16）。hi 无条件写（不看 desc_size）。
    /// [对照] ext4_rs `set_free_blocks_count`（block_group.rs:243-246）。
    pub(in crate::fs::ext4) fn set_free_blocks_count(&mut self, cnt: u32) {
        self.free_blocks_count_lo = (cnt & 0xffff) as u16;
        self.free_blocks_count_hi = (cnt >> 16) as u16;
    }

    /// 取空闲 inode 计数。lo/hi 在 **bit 16** 处拼接（`lo | (hi << 16)`），与
    /// [`set_free_inodes_count`] 的 `hi = cnt >> 16` 拆分对称。
    /// [对照] ext4 组描述符 `bg_free_inodes_count_lo/hi`（Linux `ext4_free_inodes_count`）。
    ///   修复 BUG-1：旧 ext4_rs getter 用 `((hi as u64)<<32) as u32`，该子表达式恒为 0 → hi
    ///   高 16 位被整段丢弃，只返回 lo。此处按规范 bit16 拼接。
    pub(in crate::fs::ext4) fn get_free_inodes_count(&self) -> u32 {
        let lo = self.free_inodes_count_lo;
        let hi = self.free_inodes_count_hi;
        (lo as u32) | ((hi as u32) << 16)
    }

    /// 写空闲 inode 计数（lo = cnt&0xffff；hi 仅 desc_size>min 时写 cnt>>16）。
    /// [对照] ext4_rs `set_free_inodes_count`（block_group.rs:123-128）。
    pub(in crate::fs::ext4) fn set_free_inodes_count(&mut self, sb: &RawSuperblock, cnt: u32) {
        self.free_inodes_count_lo = (cnt & 0xffff) as u16;
        if sb.group_desc_size() as u16 > EXT4_MIN_DESC_SIZE {
            self.free_inodes_count_hi = (cnt >> 16) as u16;
        }
    }

    /// 取已用目录数。lo/hi 在 **bit 16** 处拼接（仅 desc_size>min 时并入 hi）。
    /// [对照] ext4 组描述符 `bg_used_dirs_count_lo/hi`（Linux `ext4_used_dirs_count`）。
    ///   修复 BUG-1：旧 ext4_rs 用 `((hi as u64)<<32) as u32` 恒 0 → hi 被丢。
    pub(in crate::fs::ext4) fn get_used_dirs_count(&self, sb: &RawSuperblock) -> u32 {
        let lo = self.used_dirs_count_lo;
        let hi = self.used_dirs_count_hi;
        let mut v = lo as u32;
        if sb.group_desc_size() as u16 > EXT4_MIN_DESC_SIZE {
            v |= (hi as u32) << 16;
        }
        v
    }

    /// 写已用目录数（lo = cnt&0xffff；hi 仅 desc_size>min 时写 cnt>>16，**bit16 拆分**）。
    /// [对照] ext4 组描述符 `bg_used_dirs_count_lo/hi`（Linux `ext4_used_dirs_set`）。
    ///   修复 BUG-3：旧 ext4_rs 此函数误写进 `itable_unused_lo/hi` → 盘上 `used_dirs_count`
    ///   永不更新、且污染 `itable_unused`。此处写正确的 `used_dirs_count_lo/hi` 字段。
    pub(in crate::fs::ext4) fn set_used_dirs_count(&mut self, sb: &RawSuperblock, cnt: u32) {
        self.used_dirs_count_lo = (cnt & 0xffff) as u16;
        if sb.group_desc_size() as u16 > EXT4_MIN_DESC_SIZE {
            self.used_dirs_count_hi = (cnt >> 16) as u16;
        }
    }

    /// 取未用 inode 计数（itable_unused）。lo/hi 在 **bit 16** 处拼接（仅 desc_size>min 时并入 hi）。
    /// [对照] ext4 组描述符 `bg_itable_unused_lo/hi`（Linux `ext4_itable_unused_count`）。
    ///   修复 BUG-1：旧 ext4_rs 用 `((hi as u64)<<32) as u32` 恒 0 → hi 被丢。
    pub(in crate::fs::ext4) fn get_itable_unused(&self, sb: &RawSuperblock) -> u32 {
        let lo = self.itable_unused_lo;
        let hi = self.itable_unused_hi;
        let mut v = lo as u32;
        if sb.group_desc_size() as u16 > EXT4_MIN_DESC_SIZE {
            v |= (hi as u32) << 16;
        }
        v
    }

    /// 写未用 inode 计数（itable_unused，lo = cnt&0xffff；hi 仅 desc_size>min 时写 cnt>>16）。
    /// [对照] ext4 组描述符 `bg_itable_unused_lo/hi`（Linux `ext4_itable_unused_set`）。
    ///   正确写 `itable_unused_lo/hi`（BUG-3 修复后已与 `set_used_dirs_count` 写不同字段）。
    pub(in crate::fs::ext4) fn set_itable_unused(&mut self, sb: &RawSuperblock, cnt: u32) {
        self.itable_unused_lo = (cnt & 0xffff) as u16;
        if sb.group_desc_size() as u16 > EXT4_MIN_DESC_SIZE {
            self.itable_unused_hi = (cnt >> 16) as u16;
        }
    }

    /// 当前 checksum 字段（供测试/校验对拍）。
    pub(in crate::fs::ext4) fn checksum(&self) -> u16 {
        self.checksum
    }

    // ------------------------------------------------------------------
    // 校验和（crc16 描述符 + crc32c 块/inode 位图）。
    // [对照来源] ext4_rs/src/ext4_defs/block_group.rs:145-264 + super_block.rs:290-307
    // ------------------------------------------------------------------

    /// 计算组描述符 crc16：`crc32c(uuid) → crc32c(le bgid) → crc32c(desc_bytes[..desc_size])`，
    /// 取低 16 位；计算前把 checksum 字段置 0（在字节副本上操作，不改 self）。
    /// [对照] ext4_rs `get_block_group_checksum`（block_group.rs:145-176）。
    pub(in crate::fs::ext4) fn compute_checksum(&self, bgid: u32, sb: &RawSuperblock) -> u16 {
        // 用 clamped desc_size（ext4_rs `super_block.desc_size()` 把 <32 夹到 32）。
        let desc_size = sb.group_desc_size();
        // 在 64 字节副本上把 checksum 字段清零（ext4_rs 临时清 self.checksum 再恢复，等价）。
        let mut bytes = self.as_bytes().to_vec();
        bytes[GROUP_DESC_CHECKSUM_OFFSET] = 0;
        bytes[GROUP_DESC_CHECKSUM_OFFSET + 1] = 0;

        let uuid = sb.uuid();
        let mut checksum = ext4_crc32c(EXT4_CRC32_INIT, &uuid);
        checksum = ext4_crc32c(checksum, &bgid.to_le_bytes());
        // ext4_rs 对全 0x40 字节切片求 crc，但长度参数取 desc_size —— 即只覆盖前 desc_size 字节。
        checksum = ext4_crc32c(checksum, &bytes[..desc_size]);
        (checksum & 0xFFFF) as u16
    }

    /// 计算并写入组描述符校验和。
    /// [对照] ext4_rs `set_block_group_checksum`（block_group.rs:200-203）。
    pub(in crate::fs::ext4) fn set_checksum(&mut self, bgid: u32, sb: &RawSuperblock) {
        self.checksum = self.compute_checksum(bgid, sb);
    }

    /// 写块分配位图校验和。门控 RO-compat metadata_csum；高半仅 desc_size==64 时写。
    /// [对照] ext4_rs `set_block_group_balloc_bitmap_csum`（block_group.rs:217-231）。
    pub(in crate::fs::ext4) fn set_block_bitmap_csum(&mut self, sb: &RawSuperblock, bitmap: &[u8]) {
        let desc_size = sb.group_desc_size() as u16;
        let csum = sb.balloc_bitmap_csum(bitmap);
        let lo_csum = (csum & 0xFFFF) as u16;
        let hi_csum = (csum >> 16) as u16;

        // metadata_csum 关 → 不写（原样复刻 ext4_rs 的提前 return）。
        if (sb.features_read_only() & RO_COMPAT_METADATA_CSUM) >> 10 == 0 {
            return;
        }
        self.block_bitmap_csum_lo = lo_csum;
        if desc_size == EXT4_MAX_DESC_SIZE {
            self.block_bitmap_csum_hi = hi_csum;
        }
    }

    /// 写 inode 分配位图校验和。门控同上。
    /// [对照] ext4_rs `set_block_group_ialloc_bitmap_csum`（block_group.rs:250-264）。
    pub(in crate::fs::ext4) fn set_inode_bitmap_csum(&mut self, sb: &RawSuperblock, bitmap: &[u8]) {
        let desc_size = sb.group_desc_size() as u16;
        let csum = sb.ialloc_bitmap_csum(bitmap);
        let lo_csum = (csum & 0xFFFF) as u16;
        let hi_csum = (csum >> 16) as u16;

        if (sb.features_read_only() & RO_COMPAT_METADATA_CSUM) >> 10 == 0 {
            return;
        }
        self.inode_bitmap_csum_lo = lo_csum;
        if desc_size == EXT4_MAX_DESC_SIZE {
            self.inode_bitmap_csum_hi = hi_csum;
        }
    }
}

/// 一段「系统保留区」：某组内被 ext4 元数据占用、分配器须跳过的块区间（闭区间）。
/// 镜像 ext4_rs `SystemZone`（ext4_defs/ext4.rs:7）；alloc/free 用它判断块是否可分配。
/// 本结构与 ext4_rs 同布局，但只承载 helper 所需字段（纯几何计算，不碰盘）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::fs::ext4) struct SystemZone {
    pub group: u32,
    pub start_blk: u64,
    pub end_blk: u64,
}

/// 组几何视图：在 `&RawSuperblock` 上做块/inode ↔ 组号/组内下标的换算，以及组元数据布局
/// （super 备份、GDT 块数、保留区）。**逐位复刻 ext4_rs `Ext4` 上的同名 helper**
/// （它们读 `self.super_block.*`），故本视图也只依赖超级块字段。
///
/// `is_system_reserved_block` / `first_non_reserved_idx_in_group` 在 ext4_rs 里读
/// `self.system_zone_cache`（运行期算出的保留区列表）；本视图把该列表作为入参传入，
/// 保持纯函数、不碰盘（保留区列表的构建在 Phase 2 后续 task 落地）。
///
/// [对照来源] ext4_rs/src/ext4_impls/balloc.rs:176-257,689-1037 + inode.rs:164-170
pub(in crate::fs::ext4) struct GroupGeometry<'a> {
    sb: &'a RawSuperblock,
}

impl<'a> GroupGeometry<'a> {
    pub(in crate::fs::ext4) fn new(sb: &'a RawSuperblock) -> Self {
        Self { sb }
    }

    /// 块地址 → 组号。`first_data_block != 0 && baddr != 0` 时先减 1。
    /// [对照] ext4_rs `get_bgid_of_block`（balloc.rs:203-209）。
    pub(in crate::fs::ext4) fn get_bgid_of_block(&self, baddr: u64) -> u32 {
        let mut baddr = baddr;
        if self.sb.first_data_block() != 0 && baddr != 0 {
            baddr -= 1;
        }
        (baddr / self.sb.blocks_per_group() as u64) as u32
    }

    /// 组号 → 组首块地址。`first_data_block != 0` 时基址 +1。
    /// [对照] ext4_rs `get_block_of_bgid`（balloc.rs:218-224）。
    pub(in crate::fs::ext4) fn get_block_of_bgid(&self, bgid: u32) -> u64 {
        let mut baddr = 0u64;
        if self.sb.first_data_block() != 0 {
            baddr += 1;
        }
        baddr + bgid as u64 * self.sb.blocks_per_group() as u64
    }

    /// 块地址 → 组内相对下标。`first_data_block != 0 && baddr != 0` 时先减 1。
    /// [对照] ext4_rs `addr_to_idx_bg`（balloc.rs:233-239）。
    pub(in crate::fs::ext4) fn addr_to_idx_bg(&self, baddr: u64) -> u32 {
        let mut baddr = baddr;
        if self.sb.first_data_block() != 0 && baddr != 0 {
            baddr -= 1;
        }
        (baddr % self.sb.blocks_per_group() as u64) as u32
    }

    /// 组内相对下标 → 绝对块地址。`first_data_block != 0` 时下标 +1。
    /// [对照] ext4_rs `bg_idx_to_addr`（balloc.rs:251-257）。
    pub(in crate::fs::ext4) fn bg_idx_to_addr(&self, index: u32, bgid: u32) -> Ext4Fsblk {
        let mut index = index;
        if self.sb.first_data_block() != 0 {
            index += 1;
        }
        (self.sb.blocks_per_group() as u64 * bgid as u64) + index as u64
    }

    /// inode 号 → 组号。
    /// [对照] ext4_rs `get_bgid_of_inode`（inode.rs:164-166）。
    pub(in crate::fs::ext4) fn get_bgid_of_inode(&self, inode_num: u32) -> u32 {
        inode_num.saturating_sub(1) / self.sb.inodes_per_group()
    }

    /// inode 号 → 组内下标。
    /// [对照] ext4_rs `inode_to_bgidx`（inode.rs:168-170）。
    pub(in crate::fs::ext4) fn inode_to_bgidx(&self, inode_num: u32) -> u32 {
        inode_num.saturating_sub(1) % self.sb.inodes_per_group()
    }

    /// 组首块的组内下标。ext4_rs 内联为 `addr_to_idx_bg(get_block_of_bgid(bgid))`。
    /// [对照] ext4_rs balloc.rs:314-315（内联表达式）。
    pub(in crate::fs::ext4) fn first_in_bg_index(&self, bgid: u32) -> u32 {
        self.addr_to_idx_bg(self.get_block_of_bgid(bgid))
    }

    /// 该组是否带超级块备份（group 0 或 3/5/7 的幂）。
    /// [对照] ext4_rs `ext4_bg_has_super`（balloc.rs:996-1007）。
    pub(in crate::fs::ext4) fn ext4_bg_has_super(&self, group: u32) -> bool {
        if group == 0 {
            return true;
        }
        is_power_of(group, 3) || is_power_of(group, 5) || is_power_of(group, 7)
    }

    /// 是否启用 meta_bg 特性。
    /// [对照] ext4_rs `ext4_has_feature_meta_bg`（balloc.rs:1010-1014）。
    fn has_feature_meta_bg(&self) -> bool {
        self.sb.features_incompatible() & INCOMPAT_META_BG != 0
    }

    /// 该组的 GDT 块数。
    /// [对照] ext4_rs `ext4_bg_num_gdb`（balloc.rs:1017-1036）。
    pub(in crate::fs::ext4) fn ext4_bg_num_gdb(&self, group: u32) -> u32 {
        let sb = self.sb;
        let group_count = sb.block_group_count();
        let block_size = sb.block_size() as u64;
        // clamped desc_size（与 ext4_rs `super_block.desc_size()` 一致）。
        let desc_size = sb.group_desc_size() as u64;
        let reserved_gdt_blocks = sb.reserved_gdt_blocks() as u32;
        let desc_blocks =
            ((group_count as u64 * desc_size + block_size - 1) / block_size) as u32;

        if !self.ext4_bg_has_super(group) {
            return 0;
        }
        if group == 0 {
            return desc_blocks + reserved_gdt_blocks;
        }
        if self.has_feature_meta_bg() {
            1
        } else {
            desc_blocks + reserved_gdt_blocks
        }
    }

    /// 该组的基础元数据块数（有 super 备份则 `1 + gdt`，否则 0）。
    /// [对照] ext4_rs `num_base_meta_blocks`（balloc.rs:984-993）。
    pub(in crate::fs::ext4) fn num_base_meta_blocks(&self, bgid: u32) -> u32 {
        let has_super = self.ext4_bg_has_super(bgid);
        let gdt_blocks = self.ext4_bg_num_gdb(bgid);
        if has_super {
            1 + gdt_blocks
        } else {
            0
        }
    }

    /// 块是否落在系统保留区。`zones` 为 None（缓存未建）时一律返回 false。
    /// [对照] ext4_rs `is_system_reserved_block`（balloc.rs:689-704）。
    pub(in crate::fs::ext4) fn is_system_reserved_block(
        &self,
        block_num: u64,
        zones: Option<&[SystemZone]>,
    ) -> bool {
        let Some(zones) = zones else {
            return false;
        };
        for zone in zones {
            if block_num >= zone.start_blk && block_num <= zone.end_blk {
                return true;
            }
        }
        false
    }

    /// 组内首个「非保留」候选下标：从组首块下标起，越过本组各保留区尾部。
    /// `zones` 为 None 时即组首块下标（ext4_rs 在 cache 未建时同此）。
    /// [对照] ext4_rs `first_non_reserved_idx_in_group`（balloc.rs:176-193）。
    pub(in crate::fs::ext4) fn first_non_reserved_idx_in_group(
        &self,
        bgid: u32,
        zones: Option<&[SystemZone]>,
    ) -> u32 {
        let mut idx = self.addr_to_idx_bg(self.get_block_of_bgid(bgid));
        if let Some(zones) = zones {
            for zone in zones {
                if zone.group != bgid {
                    continue;
                }
                let next_blk = zone.end_blk.saturating_add(1);
                let next_idx = self.addr_to_idx_bg(next_blk);
                if next_idx > idx {
                    idx = next_idx;
                }
            }
        }
        idx
    }
}

/// `n` 是否为 `base` 的整数幂（含 `base^0`? 否——`n < base` 直接 false）。
/// [对照] ext4_rs `is_power_of`（balloc.rs:1001-1005，内层 fn）。
fn is_power_of(mut n: u32, base: u32) -> bool {
    if n < base {
        return false;
    }
    while n % base == 0 {
        n /= base;
    }
    n == 1
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::{RawGroupDescriptor, RawSuperblock};
    use crate::fs::ext4::core::test_util::EXT4_IMAGE;
    // 带进 Pod 的 from_bytes / as_bytes。
    use crate::prelude::*;

    /// 真镜像超级块（desc_size==64 > min，开 hi 半位写/读路径）。
    fn sb_with_hi() -> RawSuperblock {
        let sb = RawSuperblock::from_bytes(&EXT4_IMAGE[1024..2048]);
        assert!(
            sb.group_desc_size() > 32,
            "fixture must have 64-byte descriptors so hi halves are live"
        );
        sb
    }

    /// BUG-1/2 修复：计数 lo/hi 在 bit16 处拆/拼，set→get 必须往返完整 32 位值（含 hi != 0）。
    /// 旧 ext4_rs getter 丢 hi → 大于 0xFFFF 的值读回只剩低 16 位，断言会失败。
    #[ktest]
    fn counter_lo_hi_roundtrip_full_32bit() {
        let sb = sb_with_hi();
        let mut desc = RawGroupDescriptor::default();

        // hi != 0 的代表值：0x0003_4567 → lo=0x4567, hi=0x0003。
        let val: u32 = 0x0003_4567;
        assert_eq!(val >> 16, 0x0003, "test value must exercise the hi half");

        // BUG-1: free_inodes
        desc.set_free_inodes_count(&sb, val);
        assert_eq!(desc.get_free_inodes_count(), val, "free_inodes hi dropped (BUG-1)");
        assert_eq!(
            { desc.free_inodes_count_lo },
            0x4567,
            "free_inodes lo must hold low 16 bits"
        );
        assert_eq!(
            { desc.free_inodes_count_hi },
            0x0003,
            "free_inodes hi must hold high 16 bits (bit16 split)"
        );

        // BUG-1: itable_unused
        desc.set_itable_unused(&sb, val);
        assert_eq!(desc.get_itable_unused(&sb), val, "itable_unused hi dropped (BUG-1)");

        // BUG-2: free_blocks (u64 path, set splits at bit16, get must reassemble at bit16)
        desc.set_free_blocks_count(val);
        assert_eq!(
            desc.get_free_blocks_count(),
            val as u64,
            "free_blocks get/set asymmetric (BUG-2)"
        );
        assert_eq!(
            { desc.free_blocks_count_hi },
            0x0003,
            "free_blocks hi must hold high 16 bits"
        );
    }

    /// BUG-3 修复：`set_used_dirs_count` 写 `used_dirs_count_*` 字段（非 `itable_unused_*`）。
    /// 设 used_dirs 后：get_used_dirs 取回该值，且 itable_unused 不被污染（保持先前写入值）。
    #[ktest]
    fn used_dirs_writes_own_field_not_itable_unused() {
        let sb = sb_with_hi();
        let mut desc = RawGroupDescriptor::default();

        // 先给 itable_unused 一个独立标记值。
        desc.set_itable_unused(&sb, 0x0002_1111);
        assert_eq!(desc.get_itable_unused(&sb), 0x0002_1111);

        // 写 used_dirs（hi != 0），随后核对：used_dirs 取回正确 + itable_unused 不变。
        let dirs: u32 = 0x0001_ABCD;
        desc.set_used_dirs_count(&sb, dirs);
        assert_eq!(
            desc.get_used_dirs_count(&sb),
            dirs,
            "used_dirs_count not stored in its own field (BUG-3)"
        );
        assert_eq!(
            desc.get_itable_unused(&sb),
            0x0002_1111,
            "set_used_dirs_count clobbered itable_unused (BUG-3)"
        );
        // 直接核对底层字段：used_dirs_count_lo/hi 被写、itable_unused_lo/hi 未被 used_dirs 触碰。
        assert_eq!({ desc.used_dirs_count_lo }, 0xABCD);
        assert_eq!({ desc.used_dirs_count_hi }, 0x0001);
    }
}
