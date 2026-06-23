// SPDX-License-Identifier: MPL-2.0
use ostd::const_assert;

use super::crc::{ext4_crc32c, EXT4_CRC32_INIT};
use super::prelude::*;
use super::superblock::RawSuperblock;

/// 最小组描述符尺寸（32 字节，无 _hi 字段）。`desc_size > 此值` 才读写 _hi 半。
/// [对照] ext4_rs `EXT4_MIN_BLOCK_GROUP_DESCRIPTOR_SIZE`（consts.rs:34）。
pub(super) const EXT4_MIN_DESC_SIZE: u16 = 32;
/// 最大组描述符尺寸（64 字节，含 _hi 字段）。位图 csum 高半仅在 `desc_size == 此值` 时写。
/// [对照] ext4_rs `EXT4_MAX_BLOCK_GROUP_DESCRIPTOR_SIZE`（consts.rs:35）。
pub(super) const EXT4_MAX_DESC_SIZE: u16 = 64;

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
pub(super) struct RawGroupDescriptor {
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
    pub(super) fn get_free_blocks_count(&self) -> u64 {
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
    pub(super) fn set_free_blocks_count(&mut self, cnt: u32) {
        self.free_blocks_count_lo = (cnt & 0xffff) as u16;
        self.free_blocks_count_hi = (cnt >> 16) as u16;
    }

    /// 取空闲 inode 计数。
    /// [对照] ext4_rs `get_free_inodes_count`（block_group.rs:131-133）。
    pub(super) fn get_free_inodes_count(&self) -> u32 {
        let lo = self.free_inodes_count_lo;
        let _hi = self.free_inodes_count_hi;
        // PARITY: replicate ext4_rs bug, fix deferred (roadmap §5)
        // ext4_rs 原式 `((hi as u64)<<32) as u32 | lo`：`(hi<<32) as u32` 恒为 0 → hi 被丢，只返回 lo。
        ((_hi as u64) << 32) as u32 | lo as u32
    }

    /// 写空闲 inode 计数（lo = cnt&0xffff；hi 仅 desc_size>min 时写 cnt>>16）。
    /// [对照] ext4_rs `set_free_inodes_count`（block_group.rs:123-128）。
    pub(super) fn set_free_inodes_count(&mut self, sb: &RawSuperblock, cnt: u32) {
        self.free_inodes_count_lo = (cnt & 0xffff) as u16;
        if sb.group_desc_size() as u16 > EXT4_MIN_DESC_SIZE {
            self.free_inodes_count_hi = (cnt >> 16) as u16;
        }
    }

    /// 取已用目录数。
    /// [对照] ext4_rs `get_used_dirs_count`（block_group.rs:98-104）。
    pub(super) fn get_used_dirs_count(&self, sb: &RawSuperblock) -> u32 {
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
    pub(super) fn set_used_dirs_count(&mut self, sb: &RawSuperblock, cnt: u32) {
        // PARITY: replicate ext4_rs bug, fix deferred (roadmap §5)
        // ext4_rs 此函数误写进 itable_unused_lo/hi，而非 used_dirs_count_lo/hi —— 原样复刻。
        self.itable_unused_lo = (cnt & 0xffff) as u16;
        if sb.group_desc_size() as u16 > EXT4_MIN_DESC_SIZE {
            self.itable_unused_hi = (cnt >> 16) as u16;
        }
    }

    /// 取未用 inode 计数（itable_unused）。
    /// [对照] ext4_rs `get_itable_unused`（block_group.rs:89-95）。
    pub(super) fn get_itable_unused(&self, sb: &RawSuperblock) -> u32 {
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
    pub(super) fn set_itable_unused(&mut self, sb: &RawSuperblock, cnt: u32) {
        // 注：与 set_used_dirs_count 同样写 itable_unused —— ext4_rs 两个 setter 实现相同。
        self.itable_unused_lo = (cnt & 0xffff) as u16;
        if sb.group_desc_size() as u16 > EXT4_MIN_DESC_SIZE {
            self.itable_unused_hi = (cnt >> 16) as u16;
        }
    }

    /// 当前 checksum 字段（供测试/校验对拍）。
    pub(super) fn checksum(&self) -> u16 {
        self.checksum
    }

    // ------------------------------------------------------------------
    // 校验和（crc16 描述符 + crc32c 块/inode 位图）。
    // [对照来源] ext4_rs/src/ext4_defs/block_group.rs:145-264 + super_block.rs:290-307
    // ------------------------------------------------------------------

    /// 计算组描述符 crc16：`crc32c(uuid) → crc32c(le bgid) → crc32c(desc_bytes[..desc_size])`，
    /// 取低 16 位；计算前把 checksum 字段置 0（在字节副本上操作，不改 self）。
    /// [对照] ext4_rs `get_block_group_checksum`（block_group.rs:145-176）。
    pub(super) fn compute_checksum(&self, bgid: u32, sb: &RawSuperblock) -> u16 {
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
    pub(super) fn set_checksum(&mut self, bgid: u32, sb: &RawSuperblock) {
        self.checksum = self.compute_checksum(bgid, sb);
    }

    /// 写块分配位图校验和。门控 RO-compat metadata_csum；高半仅 desc_size==64 时写。
    /// [对照] ext4_rs `set_block_group_balloc_bitmap_csum`（block_group.rs:217-231）。
    pub(super) fn set_block_bitmap_csum(&mut self, sb: &RawSuperblock, bitmap: &[u8]) {
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
    pub(super) fn set_inode_bitmap_csum(&mut self, sb: &RawSuperblock, bitmap: &[u8]) {
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
pub(super) struct SystemZone {
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
pub(super) struct GroupGeometry<'a> {
    sb: &'a RawSuperblock,
}

impl<'a> GroupGeometry<'a> {
    pub(super) fn new(sb: &'a RawSuperblock) -> Self {
        Self { sb }
    }

    /// 块地址 → 组号。`first_data_block != 0 && baddr != 0` 时先减 1。
    /// [对照] ext4_rs `get_bgid_of_block`（balloc.rs:203-209）。
    pub(super) fn get_bgid_of_block(&self, baddr: u64) -> u32 {
        let mut baddr = baddr;
        if self.sb.first_data_block() != 0 && baddr != 0 {
            baddr -= 1;
        }
        (baddr / self.sb.blocks_per_group() as u64) as u32
    }

    /// 组号 → 组首块地址。`first_data_block != 0` 时基址 +1。
    /// [对照] ext4_rs `get_block_of_bgid`（balloc.rs:218-224）。
    pub(super) fn get_block_of_bgid(&self, bgid: u32) -> u64 {
        let mut baddr = 0u64;
        if self.sb.first_data_block() != 0 {
            baddr += 1;
        }
        baddr + bgid as u64 * self.sb.blocks_per_group() as u64
    }

    /// 块地址 → 组内相对下标。`first_data_block != 0 && baddr != 0` 时先减 1。
    /// [对照] ext4_rs `addr_to_idx_bg`（balloc.rs:233-239）。
    pub(super) fn addr_to_idx_bg(&self, baddr: u64) -> u32 {
        let mut baddr = baddr;
        if self.sb.first_data_block() != 0 && baddr != 0 {
            baddr -= 1;
        }
        (baddr % self.sb.blocks_per_group() as u64) as u32
    }

    /// 组内相对下标 → 绝对块地址。`first_data_block != 0` 时下标 +1。
    /// [对照] ext4_rs `bg_idx_to_addr`（balloc.rs:251-257）。
    pub(super) fn bg_idx_to_addr(&self, index: u32, bgid: u32) -> Ext4Fsblk {
        let mut index = index;
        if self.sb.first_data_block() != 0 {
            index += 1;
        }
        (self.sb.blocks_per_group() as u64 * bgid as u64) + index as u64
    }

    /// inode 号 → 组号。
    /// [对照] ext4_rs `get_bgid_of_inode`（inode.rs:164-166）。
    pub(super) fn get_bgid_of_inode(&self, inode_num: u32) -> u32 {
        inode_num.saturating_sub(1) / self.sb.inodes_per_group()
    }

    /// inode 号 → 组内下标。
    /// [对照] ext4_rs `inode_to_bgidx`（inode.rs:168-170）。
    pub(super) fn inode_to_bgidx(&self, inode_num: u32) -> u32 {
        inode_num.saturating_sub(1) % self.sb.inodes_per_group()
    }

    /// 组首块的组内下标。ext4_rs 内联为 `addr_to_idx_bg(get_block_of_bgid(bgid))`。
    /// [对照] ext4_rs balloc.rs:314-315（内联表达式）。
    pub(super) fn first_in_bg_index(&self, bgid: u32) -> u32 {
        self.addr_to_idx_bg(self.get_block_of_bgid(bgid))
    }

    /// 该组是否带超级块备份（group 0 或 3/5/7 的幂）。
    /// [对照] ext4_rs `ext4_bg_has_super`（balloc.rs:996-1007）。
    pub(super) fn ext4_bg_has_super(&self, group: u32) -> bool {
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
    pub(super) fn ext4_bg_num_gdb(&self, group: u32) -> u32 {
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
    pub(super) fn num_base_meta_blocks(&self, bgid: u32) -> u32 {
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
    pub(super) fn is_system_reserved_block(
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
    pub(super) fn first_non_reserved_idx_in_group(
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

    use super::{GroupGeometry, RawGroupDescriptor, SystemZone};
    use crate::fs::ext4::core::diff_harness::MemDisk;
    use crate::fs::ext4::core::superblock::RawSuperblock;
    use crate::fs::ext4::core::test_util::{slice_at, EXT4_IMAGE};
    use crate::prelude::*;

    /// 从真镜像取第 0 组描述符的 64 字节（GDT 紧跟超级块块）。
    fn real_group0_desc_bytes() -> Vec<u8> {
        let sb = RawSuperblock::from_bytes(slice_at(1024, 1024));
        let bs = sb.block_size();
        let gdt_off = (sb.first_data_block() as usize + 1) * bs;
        slice_at(gdt_off, size_of::<RawGroupDescriptor>()).to_vec()
    }

    /// 手工 64 位组描述符：desc_size==64、**所有 _hi 半非 0**，专门暴露：
    /// - free_blocks 的 set(>>16)/get(<<32) 不对称 + hi!=0 才并入；
    /// - free_inodes/itable_unused/used_dirs 的 `((hi)<<32) as u32` 截断 bug；
    /// - set_used_dirs/set_itable 都写 itable_unused 的 bug。
    fn handcrafted_64bit_desc_bytes() -> Vec<u8> {
        let mut d = vec![0u8; 64];
        // lo 字段
        d[0..4].copy_from_slice(&0x0000_1111u32.to_le_bytes()); // block_bitmap_lo
        d[4..8].copy_from_slice(&0x0000_2222u32.to_le_bytes()); // inode_bitmap_lo
        d[8..12].copy_from_slice(&0x0000_3333u32.to_le_bytes()); // inode_table_lo
        d[12..14].copy_from_slice(&0xABCDu16.to_le_bytes()); // free_blocks_count_lo
        d[14..16].copy_from_slice(&0x1234u16.to_le_bytes()); // free_inodes_count_lo
        d[16..18].copy_from_slice(&0x5678u16.to_le_bytes()); // used_dirs_count_lo
        d[18..20].copy_from_slice(&0x0001u16.to_le_bytes()); // flags
        d[24..26].copy_from_slice(&0x9999u16.to_le_bytes()); // block_bitmap_csum_lo
        d[26..28].copy_from_slice(&0x8888u16.to_le_bytes()); // inode_bitmap_csum_lo
        d[28..30].copy_from_slice(&0x4321u16.to_le_bytes()); // itable_unused_lo
        d[30..32].copy_from_slice(&0xDEADu16.to_le_bytes()); // checksum（应被重算覆盖）
        // hi 字段（全非 0，暴露 hi 路径）
        d[32..36].copy_from_slice(&0x0000_0007u32.to_le_bytes()); // block_bitmap_hi
        d[36..40].copy_from_slice(&0x0000_0008u32.to_le_bytes()); // inode_bitmap_hi
        d[40..44].copy_from_slice(&0x0000_0009u32.to_le_bytes()); // inode_table_hi
        d[44..46].copy_from_slice(&0x0002u16.to_le_bytes()); // free_blocks_count_hi
        d[46..48].copy_from_slice(&0x0003u16.to_le_bytes()); // free_inodes_count_hi
        d[48..50].copy_from_slice(&0x0004u16.to_le_bytes()); // used_dirs_count_hi
        d[50..52].copy_from_slice(&0x0005u16.to_le_bytes()); // itable_unused_hi
        d[56..58].copy_from_slice(&0x7777u16.to_le_bytes()); // block_bitmap_csum_hi
        d[58..60].copy_from_slice(&0x6666u16.to_le_bytes()); // inode_bitmap_csum_hi
        d
    }

    /// 真镜像 1024 字节超级块（desc_size==64、metadata_csum 开、first_data_block==0）。
    fn real_sb_bytes() -> Vec<u8> {
        slice_at(1024, 1024).to_vec()
    }

    /// 真镜像超级块副本，但清掉 RO-compat 特性位（→ metadata_csum 关）。
    /// features_read_only 字段在超级块内偏移 0x64（100）。
    fn sb_bytes_metadata_csum_off() -> Vec<u8> {
        let mut sb = real_sb_bytes();
        for b in &mut sb[100..104] {
            *b = 0;
        }
        sb
    }

    // ============================== 计数 get/set ==============================

    /// 计数 get/set —— 对真镜像第 0 组描述符 + 手工 64 位描述符，逐位复刻 ext4_rs（含两处 bug）。
    /// 跑「读→±→写回」alloc 序列，落盘字节逐字节对拍；并显式断言 bug 行为一致。
    #[ktest]
    fn bg_logic_diff_counts() {
        for (label, desc_bytes) in [
            ("real_group0", real_group0_desc_bytes()),
            ("handcrafted_64bit", handcrafted_64bit_desc_bytes()),
        ] {
            let sb_new = RawSuperblock::from_bytes(&real_sb_bytes());
            let sb_old = ext4_rs::Ext4Superblock::from_bytes(&real_sb_bytes());

            // --- 读初值逐位一致（含截断/不对称 bug）。 ---
            let gd = RawGroupDescriptor::from_bytes(&desc_bytes);
            let og = ext4_rs::Ext4BlockGroup::from_bytes(&desc_bytes);
            assert_eq!(
                gd.get_free_blocks_count(),
                og.get_free_blocks_count(),
                "{label}: get_free_blocks_count (u64, hi!=0-only quirk)"
            );
            assert_eq!(
                gd.get_free_inodes_count(),
                og.get_free_inodes_count(),
                "{label}: get_free_inodes_count (hi-truncation bug)"
            );
            assert_eq!(
                gd.get_used_dirs_count(&sb_new),
                og.get_used_dirs_count(&sb_old),
                "{label}: get_used_dirs_count"
            );
            // get_itable_unused 在 ext4_rs 取 &mut self；克隆一份再取。
            let mut og_mut = og;
            assert_eq!(
                gd.get_itable_unused(&sb_new),
                og_mut.get_itable_unused(&sb_old),
                "{label}: get_itable_unused"
            );

            // --- set_free_blocks（含 alloc 风格 read-modify-write：fb-1）后字节一致。 ---
            {
                let mut a = RawGroupDescriptor::from_bytes(&desc_bytes);
                let mut b = ext4_rs::Ext4BlockGroup::from_bytes(&desc_bytes);
                // 复刻 update_free_block_counts: fb_cnt = get(); fb_cnt -= 1; set(fb_cnt as u32);
                let fb_a = a.get_free_blocks_count() - 1;
                let fb_b = b.get_free_blocks_count() - 1;
                a.set_free_blocks_count(fb_a as u32);
                b.set_free_blocks_count(fb_b as u32);
                assert_eq!(
                    a.as_bytes(),
                    b.bytes_for_diff().as_slice(),
                    "{label}: set_free_blocks_count -1 bytes"
                );
            }
            // 直接 set 一组数值（含 >2^16，hi 半被 >>16 写入）。
            for &v in &[0u32, 1, 0xffff, 0x1_0000, 0x12_3456, 0xffff_ffff] {
                let mut a = RawGroupDescriptor::from_bytes(&desc_bytes);
                let mut b = ext4_rs::Ext4BlockGroup::from_bytes(&desc_bytes);
                a.set_free_blocks_count(v);
                b.set_free_blocks_count(v);
                assert_eq!(
                    a.as_bytes(),
                    b.bytes_for_diff().as_slice(),
                    "{label}: set_free_blocks_count v={v} bytes"
                );
            }

            // --- set_free_inodes（ialloc 风格 +1）后字节一致。 ---
            {
                let mut a = RawGroupDescriptor::from_bytes(&desc_bytes);
                let mut b = ext4_rs::Ext4BlockGroup::from_bytes(&desc_bytes);
                let fi_a = a.get_free_inodes_count() + 1;
                let fi_b = b.get_free_inodes_count() + 1;
                a.set_free_inodes_count(&sb_new, fi_a);
                b.set_free_inodes_count(&sb_old, fi_b);
                assert_eq!(
                    a.as_bytes(),
                    b.bytes_for_diff().as_slice(),
                    "{label}: set_free_inodes_count +1 bytes"
                );
            }
            for &v in &[0u32, 1, 0xffff, 0x1_0000, 0x7_ffff, 0xffff_ffff] {
                let mut a = RawGroupDescriptor::from_bytes(&desc_bytes);
                let mut b = ext4_rs::Ext4BlockGroup::from_bytes(&desc_bytes);
                a.set_free_inodes_count(&sb_new, v);
                b.set_free_inodes_count(&sb_old, v);
                assert_eq!(
                    a.as_bytes(),
                    b.bytes_for_diff().as_slice(),
                    "{label}: set_free_inodes_count v={v} bytes"
                );
            }

            // --- set_used_dirs_count（bug：写 itable_unused）+ set_itable_unused 字节一致。 ---
            for &v in &[0u32, 1, 0x1_0000, 0x12_3456] {
                let mut a = RawGroupDescriptor::from_bytes(&desc_bytes);
                let mut b = ext4_rs::Ext4BlockGroup::from_bytes(&desc_bytes);
                a.set_used_dirs_count(&sb_new, v);
                b.set_used_dirs_count(&sb_old, v);
                assert_eq!(
                    a.as_bytes(),
                    b.bytes_for_diff().as_slice(),
                    "{label}: set_used_dirs_count v={v} bytes (bug: writes itable_unused)"
                );

                let mut a = RawGroupDescriptor::from_bytes(&desc_bytes);
                let mut b = ext4_rs::Ext4BlockGroup::from_bytes(&desc_bytes);
                a.set_itable_unused(&sb_new, v);
                b.set_itable_unused(&sb_old, v);
                assert_eq!(
                    a.as_bytes(),
                    b.bytes_for_diff().as_slice(),
                    "{label}: set_itable_unused v={v} bytes"
                );
            }
        }

        // --- 显式钉死两处 bug（不只是「与旧一致」，还要确认 bug 真存在）。 ---
        let desc = handcrafted_64bit_desc_bytes();
        let sb_new = RawSuperblock::from_bytes(&real_sb_bytes());
        let gd = RawGroupDescriptor::from_bytes(&desc);
        // free_inodes hi=0x0003、lo=0x1234；正确值应含 hi，但 bug 截断 → 仅 lo。
        assert_eq!(gd.get_free_inodes_count(), 0x1234, "free_inodes hi-truncation bug present");
        // set_used_dirs_count 写 itable_unused，不动 used_dirs_count_lo。
        let mut g2 = RawGroupDescriptor::from_bytes(&desc);
        g2.set_used_dirs_count(&sb_new, 0x00AA);
        // packed 字段先拷局部再断言（不取 packed 引用，满足 forbid(unsafe)）。
        let used_dirs_lo = g2.used_dirs_count_lo;
        let itable_lo = g2.itable_unused_lo;
        assert_eq!(used_dirs_lo, 0x5678, "set_used_dirs left used_dirs_count_lo untouched (bug)");
        assert_eq!(itable_lo, 0x00AA, "set_used_dirs wrote into itable_unused_lo (bug)");
    }

    // ============================== csum ==============================

    /// 组描述符 crc16 + 块/inode 位图 crc32c —— 对真镜像 + 手工描述符，metadata_csum 开/关两态，
    /// 与 ext4_rs 逐位对拍。
    #[ktest]
    fn bg_logic_diff_csum() {
        // 一段真实位图字节（块 2 = 块位图，整块 4096B；长度足够覆盖 blocks_per_group/8）。
        let bitmap = slice_at(2 * 4096, 4096).to_vec();

        for (label, desc_bytes) in [
            ("real_group0", real_group0_desc_bytes()),
            ("handcrafted_64bit", handcrafted_64bit_desc_bytes()),
        ] {
            for (csum_label, sb_bytes) in [
                ("csum_on", real_sb_bytes()),
                ("csum_off", sb_bytes_metadata_csum_off()),
            ] {
                let sb_new = RawSuperblock::from_bytes(&sb_bytes);
                let sb_old = ext4_rs::Ext4Superblock::from_bytes(&sb_bytes);

                // --- 组描述符 crc16（compute + 写入后字节对拍）。 ---
                for bgid in [0u32, 1, 5, 49, 1000] {
                    let gd = RawGroupDescriptor::from_bytes(&desc_bytes);
                    let mut og = ext4_rs::Ext4BlockGroup::from_bytes(&desc_bytes);
                    // ext4_rs get_block_group_checksum 取 &mut self（临时清零再恢复）。
                    let old_csum = og.get_block_group_checksum(bgid, &sb_old);
                    assert_eq!(
                        gd.compute_checksum(bgid, &sb_new),
                        old_csum,
                        "{label}/{csum_label}: crc16 compute bgid={bgid}"
                    );

                    let mut a = RawGroupDescriptor::from_bytes(&desc_bytes);
                    let mut b = ext4_rs::Ext4BlockGroup::from_bytes(&desc_bytes);
                    a.set_checksum(bgid, &sb_new);
                    b.set_block_group_checksum(bgid, &sb_old);
                    assert_eq!(
                        a.as_bytes(),
                        b.bytes_for_diff().as_slice(),
                        "{label}/{csum_label}: set_checksum bgid={bgid} bytes"
                    );
                }

                // --- 块位图 csum（门控 metadata_csum；off 时不写）。 ---
                {
                    let mut a = RawGroupDescriptor::from_bytes(&desc_bytes);
                    let mut b = ext4_rs::Ext4BlockGroup::from_bytes(&desc_bytes);
                    a.set_block_bitmap_csum(&sb_new, &bitmap);
                    b.set_block_group_balloc_bitmap_csum(&sb_old, &bitmap);
                    assert_eq!(
                        a.as_bytes(),
                        b.bytes_for_diff().as_slice(),
                        "{label}/{csum_label}: block bitmap csum bytes"
                    );
                }
                // --- inode 位图 csum。 ---
                {
                    let mut a = RawGroupDescriptor::from_bytes(&desc_bytes);
                    let mut b = ext4_rs::Ext4BlockGroup::from_bytes(&desc_bytes);
                    a.set_inode_bitmap_csum(&sb_new, &bitmap);
                    b.set_block_group_ialloc_bitmap_csum(&sb_old, &bitmap);
                    assert_eq!(
                        a.as_bytes(),
                        b.bytes_for_diff().as_slice(),
                        "{label}/{csum_label}: inode bitmap csum bytes"
                    );
                }
            }
        }
    }

    // ============================== 组几何 ==============================

    /// 纯几何 helper（块/inode ↔ 组号/下标、has_super、num_gdb、num_base_meta、first_in_bg_index）
    /// 对真镜像 vs ext4_rs `Ext4` 同名方法逐值对拍。
    #[ktest]
    fn bg_logic_diff_geometry() {
        let sb = RawSuperblock::from_bytes(&real_sb_bytes());
        let geo = GroupGeometry::new(&sb);

        // 构造旧引擎（共享内存盘）以调用其几何 helper。
        let disk = MemDisk::from_image(EXT4_IMAGE);
        let old = ext4_rs::Ext4::open(Arc::new(disk));

        let bpg = sb.blocks_per_group() as u64;
        let group_count = sb.block_group_count();

        // 块号样本：0、组首/尾、跨组、边界 ±1、几个大值。
        let mut blocks: Vec<u64> = vec![0, 1, 2, 3, 63, 64, 1024, 2065];
        for g in 0..group_count.min(8) {
            let base = g as u64 * bpg;
            blocks.push(base);
            blocks.push(base + 1);
            if base >= 1 {
                blocks.push(base - 1);
            }
            blocks.push(base + bpg - 1);
        }
        for &b in &blocks {
            assert_eq!(geo.get_bgid_of_block(b), old.get_bgid_of_block(b), "get_bgid_of_block b={b}");
            assert_eq!(geo.addr_to_idx_bg(b), old.addr_to_idx_bg(b), "addr_to_idx_bg b={b}");
        }

        // 组号样本（含 0、3/5/7 的幂、非幂、跨界）。
        let groups = [0u32, 1, 2, 3, 4, 5, 7, 9, 25, 27, 48, 49, 343, 1000];
        for &g in &groups {
            assert_eq!(geo.get_block_of_bgid(g), old.get_block_of_bgid(g), "get_block_of_bgid g={g}");
            assert_eq!(geo.first_in_bg_index(g), old.addr_to_idx_bg(old.get_block_of_bgid(g)), "first_in_bg_index g={g}");
            assert_eq!(geo.ext4_bg_has_super(g), old.ext4_bg_has_super(g), "ext4_bg_has_super g={g}");
            assert_eq!(geo.ext4_bg_num_gdb(g), old.ext4_bg_num_gdb(g), "ext4_bg_num_gdb g={g}");
            assert_eq!(geo.num_base_meta_blocks(g), old.num_base_meta_blocks(g), "num_base_meta_blocks g={g}");
            // bg_idx_to_addr：对若干组内下标对拍。
            for &idx in &[0u32, 1, 63, 1024, 2065] {
                assert_eq!(geo.bg_idx_to_addr(idx, g), old.bg_idx_to_addr(idx, g), "bg_idx_to_addr idx={idx} g={g}");
            }
        }

        // inode 号 → 组号 / 组内下标。
        let inodes = [1u32, 2, 11, 16383, 16384, 16385, 32768, 100000];
        for &ino in &inodes {
            assert_eq!(geo.get_bgid_of_inode(ino), old.get_bgid_of_inode(ino), "get_bgid_of_inode ino={ino}");
            assert_eq!(geo.inode_to_bgidx(ino), old.inode_to_bgidx(ino), "inode_to_bgidx ino={ino}");
        }
    }

    /// 专测 `first_data_block != 0` 的 ±1 调整分支（真镜像 first_data_block==0 覆盖不到）。
    /// 把镜像超级块的 first_data_block 字段（偏移 0x14）改成 1，新旧两侧读同一份字节，
    /// 对纯算术 helper（不依赖运行期 zone）逐值对拍。
    #[ktest]
    fn bg_logic_diff_geometry_first_data_block_nonzero() {
        // 复制镜像并把 first_data_block 置 1（superblock@1024，字段偏移 0x14 → 盘内 1024+0x14=1044）。
        let mut img = EXT4_IMAGE.to_vec();
        img[1044..1048].copy_from_slice(&1u32.to_le_bytes());

        let sb = RawSuperblock::from_bytes(&img[1024..2048]);
        assert_eq!(sb.first_data_block(), 1, "edited first_data_block reads back as 1");
        let geo = GroupGeometry::new(&sb);

        let disk = MemDisk::from_image(&img);
        let old = ext4_rs::Ext4::open(Arc::new(disk));

        // 块号样本覆盖 baddr==0（不减 1）与 baddr!=0（减 1）两支。
        for b in [0u64, 1, 2, 3, 64, 32768, 32769, 65536, 100000] {
            assert_eq!(geo.get_bgid_of_block(b), old.get_bgid_of_block(b), "fdb!=0 get_bgid_of_block b={b}");
            assert_eq!(geo.addr_to_idx_bg(b), old.addr_to_idx_bg(b), "fdb!=0 addr_to_idx_bg b={b}");
        }
        for g in [0u32, 1, 2, 3] {
            assert_eq!(geo.get_block_of_bgid(g), old.get_block_of_bgid(g), "fdb!=0 get_block_of_bgid g={g}");
            for idx in [0u32, 1, 64, 32767] {
                assert_eq!(geo.bg_idx_to_addr(idx, g), old.bg_idx_to_addr(idx, g), "fdb!=0 bg_idx_to_addr idx={idx} g={g}");
            }
        }
    }

    /// `is_system_reserved_block` / `first_non_reserved_idx_in_group` —— 喂入 ext4_rs 运行期算出的
    /// 同一份 system-zone 列表，与 ext4_rs `Ext4` 对应方法逐值对拍。
    #[ktest]
    fn bg_logic_diff_system_zone() {
        let sb = RawSuperblock::from_bytes(&real_sb_bytes());
        let geo = GroupGeometry::new(&sb);

        let disk = MemDisk::from_image(EXT4_IMAGE);
        let old = ext4_rs::Ext4::open(Arc::new(disk));

        // 取旧引擎运行期算出的保留区，原样镜像进本地 SystemZone（保证两侧 zone 一致）。
        let old_zones = old.get_system_zone();
        let zones: Vec<SystemZone> = old_zones
            .iter()
            .map(|z| SystemZone {
                group: z.group,
                start_blk: z.start_blk,
                end_blk: z.end_blk,
            })
            .collect();

        let bpg = sb.blocks_per_group() as u64;
        let group_count = sb.block_group_count();

        // 探测块：每组前若干块（覆盖元数据区/位图/inode表起始）、组尾、跨组。
        let mut blocks: Vec<u64> = Vec::new();
        for g in 0..group_count.min(4) {
            let base = g as u64 * bpg;
            for off in [0u64, 1, 2, 3, 9, 10, 11, 1024, 1090, 2065, 5000] {
                blocks.push(base + off);
            }
            blocks.push(base + bpg - 1);
        }
        for &b in &blocks {
            let bgid = geo.get_bgid_of_block(b);
            assert_eq!(
                geo.is_system_reserved_block(b, Some(&zones)),
                old.is_system_reserved_block(b, bgid),
                "is_system_reserved_block b={b} bgid={bgid}"
            );
        }

        // first_non_reserved_idx_in_group：逐组对拍。
        for g in 0..group_count {
            assert_eq!(
                geo.first_non_reserved_idx_in_group(g, Some(&zones)),
                old.first_non_reserved_idx_in_group(g),
                "first_non_reserved_idx_in_group g={g}"
            );
        }

        // zones=None 路径：不应保留任何块（与 ext4_rs cache 未建时一致）。
        assert!(
            !geo.is_system_reserved_block(0, None),
            "is_system_reserved_block(None) must be false"
        );
    }

    #[ktest]
    fn group_desc_roundtrip_and_diff_old() {
        // 从超级块推导组描述符表偏移（兼容任意 block_size）。
        let sb = RawSuperblock::from_bytes(slice_at(1024, 1024));
        let bs = sb.block_size();
        let gdt_off = (sb.first_data_block() as usize + 1) * bs;
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
