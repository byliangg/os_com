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
    // 分配器路径计数 get/set —— 与 ext4_rs `Ext4BlockGroup` **逐位一致**（含其磁盘 bug）。
    //
    // ⚠️ parity 陷阱：差分比落盘字节，但 alloc「读计数→±→写回」。若 getter 用干净
    // `lo|hi<<16`，则在 64bit/大 fs 上算出的新值与 ext4_rs 不同 → 写回字节不同 → 差分失败。
    // 故此处刻意复刻 ext4_rs 的位级语义（含两处 bug）。phase1 曾有干净版 getter
    // (`free_blocks_count`/`free_inodes_count` = lo|hi<<16)，已移除以免与本 parity 版混淆；
    // 当前无外部消费者（fs.rs 用的是 ext4_rs 的 Ext4Superblock，不是本结构）。
    // [对照来源] ext4_rs/src/ext4_defs/block_group.rs:107-264
    // ------------------------------------------------------------------

    /// 取空闲块计数（u64）。注意 set 在 bit16 拆分而 get 在 bit32 重组——**不对称怪癖**，
    /// 且仅 `hi != 0` 时才 OR hi（原样复刻）。
    /// [对照] ext4_rs `get_free_blocks_count`（block_group.rs:234-240）。
    pub(in crate::fs::ext4) fn get_free_blocks_count(&self) -> u64 {
        let lo = self.free_blocks_count_lo;
        let hi = self.free_blocks_count_hi;
        let mut v = lo as u64;
        // PARITY: replicate ext4_rs bug, fix deferred (roadmap §5)
        // set 写 hi = cnt>>16，get 却在 <<32 处重组，且仅 hi!=0 才并入 —— 与 set 不对称。
        if hi != 0 {
            v |= (hi as u64) << 32;
        }
        v
    }

    /// 写空闲块计数（lo = cnt&0xffff, hi = cnt>>16）。hi 无条件写（不看 desc_size）。
    /// [对照] ext4_rs `set_free_blocks_count`（block_group.rs:243-246）。
    pub(in crate::fs::ext4) fn set_free_blocks_count(&mut self, cnt: u32) {
        self.free_blocks_count_lo = (cnt & 0xffff) as u16;
        self.free_blocks_count_hi = (cnt >> 16) as u16;
    }

    /// 取空闲 inode 计数。
    /// [对照] ext4_rs `get_free_inodes_count`（block_group.rs:131-133）。
    pub(in crate::fs::ext4) fn get_free_inodes_count(&self) -> u32 {
        let lo = self.free_inodes_count_lo;
        let _hi = self.free_inodes_count_hi;
        // PARITY: replicate ext4_rs bug, fix deferred (roadmap §5)
        // ext4_rs 原式 `((hi as u64)<<32) as u32 | lo`：`(hi<<32) as u32` 恒为 0 → hi 被丢，只返回 lo。
        ((_hi as u64) << 32) as u32 | lo as u32
    }

    /// 写空闲 inode 计数（lo = cnt&0xffff；hi 仅 desc_size>min 时写 cnt>>16）。
    /// [对照] ext4_rs `set_free_inodes_count`（block_group.rs:123-128）。
    pub(in crate::fs::ext4) fn set_free_inodes_count(&mut self, sb: &RawSuperblock, cnt: u32) {
        self.free_inodes_count_lo = (cnt & 0xffff) as u16;
        if sb.group_desc_size() as u16 > EXT4_MIN_DESC_SIZE {
            self.free_inodes_count_hi = (cnt >> 16) as u16;
        }
    }

    /// 取已用目录数。
    /// [对照] ext4_rs `get_used_dirs_count`（block_group.rs:98-104）。
    pub(in crate::fs::ext4) fn get_used_dirs_count(&self, sb: &RawSuperblock) -> u32 {
        let lo = self.used_dirs_count_lo;
        let hi = self.used_dirs_count_hi;
        let mut v = lo as u32;
        if sb.group_desc_size() as u16 > EXT4_MIN_DESC_SIZE {
            // PARITY: replicate ext4_rs bug, fix deferred (roadmap §5)
            // 同 free_inodes：`((hi as u64)<<32) as u32` 恒为 0，hi 实际被丢。
            v |= ((hi as u64) << 32) as u32;
        }
        v
    }

    /// 写已用目录数。
    /// [对照] ext4_rs `set_used_dirs_count`（block_group.rs:107-112）。
    pub(in crate::fs::ext4) fn set_used_dirs_count(&mut self, sb: &RawSuperblock, cnt: u32) {
        // PARITY: replicate ext4_rs bug, fix deferred (roadmap §5)
        // ext4_rs 此函数误写进 itable_unused_lo/hi，而非 used_dirs_count_lo/hi —— 原样复刻。
        self.itable_unused_lo = (cnt & 0xffff) as u16;
        if sb.group_desc_size() as u16 > EXT4_MIN_DESC_SIZE {
            self.itable_unused_hi = (cnt >> 16) as u16;
        }
    }

    /// 取未用 inode 计数（itable_unused）。
    /// [对照] ext4_rs `get_itable_unused`（block_group.rs:89-95）。
    pub(in crate::fs::ext4) fn get_itable_unused(&self, sb: &RawSuperblock) -> u32 {
        let lo = self.itable_unused_lo;
        let hi = self.itable_unused_hi;
        let mut v = lo as u32;
        if sb.group_desc_size() as u16 > EXT4_MIN_DESC_SIZE {
            // PARITY: replicate ext4_rs bug, fix deferred (roadmap §5)
            // 同样 `((hi as u64)<<32) as u32` 恒 0，hi 被丢。
            v |= ((hi as u64) << 32) as u32;
        }
        v
    }

    /// 写未用 inode 计数（itable_unused）。
    /// [对照] ext4_rs `set_itable_unused`（block_group.rs:115-120）。
    pub(in crate::fs::ext4) fn set_itable_unused(&mut self, sb: &RawSuperblock, cnt: u32) {
        // 注：与 set_used_dirs_count 同样写 itable_unused —— ext4_rs 两个 setter 实现相同。
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
