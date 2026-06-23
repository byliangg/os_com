// SPDX-License-Identifier: MPL-2.0
//! 安全块分配器（balloc）——逐位复刻 `ext4_rs/src/ext4_impls/balloc.rs` 的扫描序、
//! goal 启发式与计数/csum 更新顺序（含已知 parity bug），但全程零 unsafe、元数据写
//! 只经 [`MetadataWriter`]。
//!
//! 三入口：
//! - [`BlockAllocator::balloc_alloc_block`]（带 goal）—— 对照 ext4_rs `balloc_alloc_block` :270-420；
//! - [`BlockAllocator::balloc_alloc_block_from`]（游标）—— 对照 `balloc_alloc_block_from` :430-584；
//! - [`BlockAllocator::balloc_alloc_block_batch`]（跨组批量，部分成功）—— 对照 `balloc_alloc_block_batch` :716-983。
//!
//! 设计要点（与 ext4_rs 一致，保证落盘字节逐字节可对拍）：
//! - 分配器持一份**可变** [`RawSuperblock`]（free-blocks 计数的运行期权威），每轮组迭代
//!   从盘上**重新读**组描述符（对照 ext4_rs `Ext4BlockGroup::load_new` 每次都读盘）。
//! - 元数据落盘三处：位图块（整块）、组描述符（64 字节，写在 GDT 块内偏移）、超级块
//!   （1024 字节，写在块 0 内偏移 1024）。后两者经「读整块 → 拼接 → 写整块」的 RMW
//!   达到与 ext4_rs 字节级一致（ext4_rs 用 byte-offset 直写，core 的 [`MetadataWriter`]
//!   是块号粒度，故 RMW）。
//! - 系统保留区（system zone）在 core 内自行计算（对照 ext4_rs `get_system_zone`），
//!   不依赖 ext4_rs；`alloc_guard` 本 Task 用恒「不含」桩占位（Task 5 接真实 guard）。
//!
//! inode 侧：本 Phase 尚无 extent 逻辑（Phase 3），故 [`InodeAllocCtx::maps_block`] 恒
//! 返回 `false`，`i_blocks` 仅在内存累加（balloc 不写 inode 表）。
//!
//! [对照来源] kernel/libs/ext4_rs/src/ext4_impls/balloc.rs

use core::cmp::min;

use super::bitmap::{
    ext4_bmap_bit_find_clr, ext4_bmap_bit_set, ext4_bmap_bits_free, ext4_bmap_is_bit_clr,
};
use super::block_group::{GroupGeometry, RawGroupDescriptor, SystemZone};
use super::io::BlockReader;
use super::metadata_writer::MetadataWriter;
use super::prelude::*;
use super::superblock::RawSuperblock;

/// 每个文件系统块折算成的 512-byte「inode 块」数的分母。
/// [对照] ext4_rs `EXT4_INODE_BLOCK_SIZE`（consts.rs:19）。
const EXT4_INODE_BLOCK_SIZE: u64 = 512;

/// 分配占位 guard（Task 5 接真实 `OperationAllocGuard`）。
///
/// ext4_rs balloc 在三处用 `self.alloc_guard.contains_current_block(block)` 跳过「本次
/// 操作已预留」的块，并在命中后 `reserve_current_block` 登记。本 Task 尚无并发操作语义，
/// 故 [`contains`] 恒返回 `false`、[`reserve`] 为空——与 ext4_rs 在「单次独立分配、guard
/// 为空」时的行为一致（差分两侧都不触发 guard 跳过）。Task 5 会把这里换成真实 guard。
struct AllocGuardStub;

impl AllocGuardStub {
    /// 恒「不含」——占位语义，Task 5 接实。
    #[inline]
    fn contains(&self, _block: Ext4Fsblk) -> bool {
        false
    }

    /// 空登记——占位语义，Task 5 接实。
    #[inline]
    fn reserve(&self, _block: Ext4Fsblk) {}

    /// 空批量登记——占位语义，Task 5 接实。
    #[inline]
    fn reserve_blocks(&self, _blocks: &[Ext4Fsblk]) {}
}

/// 分配路径所需的 inode 视图（i_blocks 累加 + 已映射块查询）。
///
/// 对照 ext4_rs `Ext4InodeRef` 在 balloc 里被用到的两件事：
/// - `inode.blocks_count()` / `set_blocks_count()`（i_blocks，单位 512B）；
/// - `inode_already_maps_block(inode_ref, block)`（extent 映射查询）。
///
/// 本 Phase 无 extent 逻辑（Phase 3 落地），故 [`maps_block`] 恒 `false`。
pub(super) struct InodeAllocCtx {
    /// i_blocks（512-byte 单位），与 ext4_rs `inode.blocks_count()` 同语义。
    i_blocks: u64,
}

impl InodeAllocCtx {
    /// 以给定初始 i_blocks 构造（差分两侧建议都从 0 起）。
    pub(super) fn new(i_blocks: u64) -> Self {
        Self { i_blocks }
    }

    /// 当前 i_blocks（512-byte 单位）。
    pub(super) fn i_blocks(&self) -> u64 {
        self.i_blocks
    }

    /// 设置 i_blocks。
    pub(super) fn set_i_blocks(&mut self, v: u64) {
        self.i_blocks = v;
    }

    /// 该 inode 是否已映射物理块 `block`。
    ///
    /// **P3 填实**：本 Phase 无 extent / 间接映射逻辑，恒返回 `false`（与差分两侧用
    /// 空 extent inode 时 ext4_rs `inode_already_maps_block` 的结果一致）。
    /// [对照] ext4_rs `inode_already_maps_block`（balloc.rs:156-172）。
    #[inline]
    pub(super) fn maps_block(&self, _block: Ext4Fsblk) -> bool {
        false
    }
}

/// 安全块分配器。持一份可变超级块（free-blocks 计数权威）+ 读接缝 + 元数据写回 +
/// 系统保留区 + alloc_guard 占位。**不复制 ext4_rs 的 `Ext4` god-object**。
pub(super) struct BlockAllocator<'a, R: BlockReader, W: MetadataWriter> {
    /// 运行期可变超级块（free-blocks 随分配递减；其余字段同盘上初值）。
    sb: RawSuperblock,
    /// 读盘接缝（每轮组迭代重读组描述符 / 位图）。
    reader: &'a R,
    /// 元数据写回接缝（位图 / 组描述符 / 超级块）。
    writer: &'a W,
    /// 系统保留区列表（core 自算，对照 ext4_rs `get_system_zone`）。
    zones: Vec<SystemZone>,
    /// 分配占位 guard（Task 5 接实）。
    guard: AllocGuardStub,
    /// 写元数据用的 JBD2 handle id 占位（本 Phase 直写，handle 语义 Phase 5 接入）。
    handle_id: u64,
}

impl<'a, R: BlockReader, W: MetadataWriter> BlockAllocator<'a, R, W> {
    /// 用初始超级块字节 + 读/写接缝构造，并自算系统保留区。
    pub(super) fn new(sb: RawSuperblock, reader: &'a R, writer: &'a W) -> Self {
        let zones = compute_system_zones(&sb, reader);
        Self {
            sb,
            reader,
            writer,
            zones,
            guard: AllocGuardStub,
            handle_id: 0,
        }
    }

    /// 当前（运行期）超级块快照——差分用例跑完后据此 `snapshot_meta`。
    pub(super) fn superblock(&self) -> &RawSuperblock {
        &self.sb
    }

    // ------------------------------------------------------------------
    // 内部 helper（块号 ↔ 偏移、读组描述符 / 位图、落盘）。
    // ------------------------------------------------------------------

    fn block_size(&self) -> usize {
        self.sb.block_size()
    }

    fn geom(&self) -> GroupGeometry<'_> {
        GroupGeometry::new(&self.sb)
    }

    /// 块号 `block` 是否落在系统保留区。
    /// [对照] ext4_rs `is_system_reserved_block`（balloc.rs:691-706）。
    fn is_system_reserved_block(&self, block: Ext4Fsblk) -> bool {
        self.geom().is_system_reserved_block(block, Some(&self.zones))
    }

    /// 从盘上重新读第 `bgid` 组的组描述符（对照 ext4_rs `Ext4BlockGroup::load_new`）。
    fn load_group_desc(&self, bgid: u32) -> RawGroupDescriptor {
        let bs = self.block_size();
        let desc_size = self.sb.group_desc_size();
        let dsc_cnt = bs / desc_size;
        let dsc_id = bgid as usize / dsc_cnt;
        let first_data_block = self.sb.first_data_block() as usize;
        let block_id = first_data_block + dsc_id + 1;
        let offset_in_block = (bgid as usize % dsc_cnt) * desc_size;
        let off = block_id * bs + offset_in_block;
        // ext4_rs `read_offset_as::<Ext4BlockGroup>` 读 64 字节；core 同样读满 64 字节后解析
        // （desc_size==64 的真镜像下与 ext4_rs 完全一致）。
        let mut buf = [0u8; 64];
        self.reader.read_at(off, &mut buf);
        RawGroupDescriptor::from_bytes(&buf)
    }

    /// 读第 `bgid` 组的块位图整块（block_size 字节）。
    fn load_block_bitmap(&self, desc: &RawGroupDescriptor) -> Vec<u8> {
        let bs = self.block_size();
        let bmp_blk = desc.block_bitmap();
        let mut data = vec![0u8; bs];
        self.reader.read_at(bmp_blk as usize * bs, data.as_mut_slice());
        data
    }

    /// 把位图整块写回（块号粒度，整块）——与 ext4_rs `write_metadata(bmp*bs, full_block)` 等价。
    fn write_block_bitmap(&self, bmp_blk: Ext4Fsblk, data: &[u8]) -> Result<()> {
        self.writer
            .write_metadata_for_handle(self.handle_id, bmp_blk, data)
    }

    /// 把组描述符的 64 字节写回 GDT 块内偏移（读整块 → 拼接 → 写整块，保证字节级一致）。
    /// 对照 ext4_rs `sync_block_group_to_disk`：写 `size_of::<Ext4BlockGroup>()`（64）字节
    /// 到 `block_id*bs + offset`。
    fn write_group_desc(&self, bgid: u32, desc: &RawGroupDescriptor) -> Result<()> {
        let bs = self.block_size();
        let desc_size = self.sb.group_desc_size();
        let dsc_cnt = bs / desc_size;
        let dsc_id = bgid as usize / dsc_cnt;
        let first_data_block = self.sb.first_data_block() as usize;
        let block_id = first_data_block + dsc_id + 1;
        let offset_in_block = (bgid as usize % dsc_cnt) * desc_size;

        let mut block = vec![0u8; bs];
        self.reader.read_at(block_id * bs, block.as_mut_slice());
        // ext4_rs 写满 64 字节（结构体大小），不看 desc_size——原样复刻。
        let bytes = desc.as_bytes();
        block[offset_in_block..offset_in_block + bytes.len()].copy_from_slice(bytes);
        self.writer
            .write_metadata_for_handle(self.handle_id, block_id as u64, &block)
    }

    /// 把运行期超级块写回（1024 字节，写在块 0 内偏移 1024；先重算 csum）。
    /// 对照 ext4_rs `Ext4Superblock::sync_to_disk_with_csum`：写 `size_of::<Ext4Superblock>()`
    /// （1024）字节到 `SUPERBLOCK_OFFSET`（1024），写前算 `crc32c(INIT, bytes, 0x3fc)`。
    fn write_superblock(&mut self) -> Result<()> {
        let bs = self.block_size();
        self.sb.recompute_csum();
        let sb_bytes = self.sb.as_bytes();
        // 超级块固定落在盘内偏移 1024；其所在块 = 1024 / bs，块内偏移 = 1024 % bs。
        let sb_off = 1024usize;
        let block_id = sb_off / bs;
        let offset_in_block = sb_off % bs;
        let mut block = vec![0u8; bs];
        self.reader.read_at(block_id * bs, block.as_mut_slice());
        block[offset_in_block..offset_in_block + sb_bytes.len()].copy_from_slice(sb_bytes);
        self.writer
            .write_metadata_for_handle(self.handle_id, block_id as u64, &block)
    }

    /// 命中一个块后的「计数 + csum」更新序——逐位复刻 ext4_rs `update_free_block_counts`
    /// （balloc.rs:586-611），唯一区别：core 不写 inode 表（i_blocks 留内存）。
    ///
    /// 顺序：① 超级块 free_blocks -1 + csum + 写；② inode i_blocks += bs/512（内存）；
    /// ③ 组描述符 free_blocks -1 + csum + 写。三处落盘区互不相交，故顺序不影响最终盘面。
    fn update_free_block_counts(
        &mut self,
        inode: &mut InodeAllocCtx,
        bgid: u32,
        desc: &mut RawGroupDescriptor,
    ) -> Result<()> {
        let block_size = self.block_size() as u64;

        // ① 超级块 free_blocks -1（对照 subtract_superblock_free_blocks(1)）。
        let free = self.sb.free_blocks_count();
        self.sb.set_free_blocks_count(free - 1);
        self.write_superblock()?;

        // ② inode i_blocks += block_size/512（内存累加；core 不落 inode 表）。
        let mut inode_blocks = inode.i_blocks();
        inode_blocks += block_size / EXT4_INODE_BLOCK_SIZE;
        inode.set_i_blocks(inode_blocks);

        // ③ 组描述符 free_blocks -1 + csum + 写。
        let mut fb = desc.get_free_blocks_count();
        fb -= 1;
        desc.set_free_blocks_count(fb as u32);
        // sync_to_disk_with_csum：先 set_checksum 再写盘。
        desc.set_checksum(bgid, &self.sb);
        self.write_group_desc(bgid, desc)?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // 入口 1：balloc_alloc_block(goal)
    // [对照] ext4_rs balloc.rs:270-420
    // ------------------------------------------------------------------

    /// 分配一个新块。`goal` 有值则从其所在组/下标起扫，无值则默认 bgid=1、idx=0。
    pub(super) fn balloc_alloc_block(
        &mut self,
        inode: &mut InodeAllocCtx,
        goal: Option<Ext4Fsblk>,
    ) -> Result<Ext4Fsblk> {
        let geom = GroupGeometry::new(&self.sb);
        let blocks_per_group = self.sb.blocks_per_group();
        let mut bgid;
        let mut idx_in_bg;

        if let Some(goal) = goal {
            bgid = geom.get_bgid_of_block(goal);
            idx_in_bg = geom.addr_to_idx_bg(goal);
        } else {
            bgid = 1;
            idx_in_bg = 0;
        }

        let block_group_count = self.sb.block_group_count();
        if bgid >= block_group_count {
            bgid = 0;
        }
        let mut count = block_group_count;

        while count > 0 {
            let mut desc = self.load_group_desc(bgid);

            let free_blocks = desc.get_free_blocks_count();
            if free_blocks == 0 {
                bgid = (bgid + 1) % block_group_count;
                count -= 1;
                if count == 0 {
                    return Err(Error::with_message(
                        Errno::ENOSPC,
                        "No free blocks available in all block groups",
                    ));
                }
                continue;
            }

            // 组内起点 clamp。
            let geom = GroupGeometry::new(&self.sb);
            let first_in_bg = geom.get_block_of_bgid(bgid);
            let first_in_bg_index = geom.addr_to_idx_bg(first_in_bg);
            let first_data_idx = geom.first_non_reserved_idx_in_group(bgid, Some(&self.zones));
            idx_in_bg = idx_in_bg.max(first_in_bg_index).max(first_data_idx);

            let bmp_blk_adr = desc.block_bitmap();
            let mut bitmap = self.load_block_bitmap(&desc);

            // (1) 测 goal 位。
            if ext4_bmap_is_bit_clr(&bitmap, idx_in_bg) {
                let block_num = geom.bg_idx_to_addr(idx_in_bg, bgid);
                if self.is_system_reserved_block(block_num)
                    || self.guard.contains(block_num)
                    || inode.maps_block(block_num)
                {
                    // 跳过 system zone
                } else {
                    ext4_bmap_bit_set(&mut bitmap, idx_in_bg);
                    desc.set_block_bitmap_csum(&self.sb, &bitmap);
                    self.write_block_bitmap(bmp_blk_adr, &bitmap)?;
                    let alloc = geom.bg_idx_to_addr(idx_in_bg, bgid);
                    self.update_free_block_counts(inode, bgid, &mut desc)?;
                    self.guard.reserve(alloc);
                    return Ok(alloc);
                }
            }

            // (2) 从 idx+1 短扫到下个 64-bit 边界。
            let blk_in_bg = blocks_per_group;
            let end_idx = min((idx_in_bg + 63) & !63, blk_in_bg);
            for tmp_idx in (idx_in_bg + 1)..end_idx {
                if ext4_bmap_is_bit_clr(&bitmap, tmp_idx) {
                    let block_num = geom.bg_idx_to_addr(tmp_idx, bgid);
                    if self.is_system_reserved_block(block_num) || self.guard.contains(block_num) {
                        continue;
                    }
                    ext4_bmap_bit_set(&mut bitmap, tmp_idx);
                    desc.set_block_bitmap_csum(&self.sb, &bitmap);
                    self.write_block_bitmap(bmp_blk_adr, &bitmap)?;
                    let alloc = geom.bg_idx_to_addr(tmp_idx, bgid);
                    self.update_free_block_counts(inode, bgid, &mut desc)?;
                    self.guard.reserve(alloc);
                    return Ok(alloc);
                }
            }

            // (3) 整组 find_clr。
            let mut rel_blk_idx = 0;
            if ext4_bmap_bit_find_clr(&bitmap, idx_in_bg, blk_in_bg, &mut rel_blk_idx) {
                let block_num = geom.bg_idx_to_addr(rel_blk_idx, bgid);
                if !self.is_system_reserved_block(block_num) && !self.guard.contains(block_num) {
                    ext4_bmap_bit_set(&mut bitmap, rel_blk_idx);
                    desc.set_block_bitmap_csum(&self.sb, &bitmap);
                    self.write_block_bitmap(bmp_blk_adr, &bitmap)?;
                    let alloc = geom.bg_idx_to_addr(rel_blk_idx, bgid);
                    self.update_free_block_counts(inode, bgid, &mut desc)?;
                    self.guard.reserve(alloc);
                    return Ok(alloc);
                }
            }

            // 本组无果，下一组。
            bgid = (bgid + 1) % block_group_count;
            count -= 1;
        }

        Err(Error::with_message(
            Errno::ENOSPC,
            "No free blocks available in all block groups",
        ))
    }

    // ------------------------------------------------------------------
    // 入口 2：balloc_alloc_block_from(&mut start_bgid)
    // [对照] ext4_rs balloc.rs:430-584
    // ------------------------------------------------------------------

    /// 从 `*start_bgid`、idx=0 起扫的无 goal 变体；命中后把组号回写 `*start_bgid`。
    pub(super) fn balloc_alloc_block_from(
        &mut self,
        inode: &mut InodeAllocCtx,
        start_bgid: &mut u32,
    ) -> Result<Ext4Fsblk> {
        let block_size = self.block_size();
        let max_blocks_in_bitmap = (block_size * 8) as u32;

        let mut bgid = *start_bgid;
        let mut idx_in_bg = 0u32;

        let block_group_count = self.sb.block_group_count();
        let mut count = block_group_count;

        while count > 0 {
            let mut desc = self.load_group_desc(bgid);

            let free_blocks = desc.get_free_blocks_count();
            if free_blocks == 0 {
                bgid = (bgid + 1) % block_group_count;
                count -= 1;
                if count == 0 {
                    return Err(Error::with_message(
                        Errno::ENOSPC,
                        "No free blocks available in all block groups",
                    ));
                }
                continue;
            }

            let geom = GroupGeometry::new(&self.sb);
            let first_in_bg = geom.get_block_of_bgid(bgid);
            let first_in_bg_index = geom.addr_to_idx_bg(first_in_bg);
            let first_data_idx = geom.first_non_reserved_idx_in_group(bgid, Some(&self.zones));
            idx_in_bg = idx_in_bg.max(first_in_bg_index).max(first_data_idx);

            // idx 越界本组位图 → 跳下一组。
            if idx_in_bg >= max_blocks_in_bitmap {
                bgid = (bgid + 1) % block_group_count;
                count -= 1;
                idx_in_bg = 0;
                continue;
            }

            let bmp_blk_adr = desc.block_bitmap();
            let mut bitmap = self.load_block_bitmap(&desc);

            // (1) 测起点位。注意：_from 这里只判 guard（不判 system zone / maps_block），
            //     命中 guard 时 idx+1 后 `continue`（不递减 count，不换组）——原样复刻。
            if ext4_bmap_is_bit_clr(&bitmap, idx_in_bg) {
                let block_num = geom.bg_idx_to_addr(idx_in_bg, bgid);
                if self.guard.contains(block_num) {
                    idx_in_bg = idx_in_bg.saturating_add(1);
                    continue;
                }
                ext4_bmap_bit_set(&mut bitmap, idx_in_bg);
                desc.set_block_bitmap_csum(&self.sb, &bitmap);
                self.write_block_bitmap(bmp_blk_adr, &bitmap)?;
                let alloc = block_num;
                self.update_free_block_counts(inode, bgid, &mut desc)?;
                *start_bgid = bgid;
                self.guard.reserve(alloc);
                return Ok(alloc);
            }

            // (2) 短扫到下个 64-bit 边界（上界用位图位数）。
            let end_idx = min((idx_in_bg + 63) & !63, max_blocks_in_bitmap);
            for tmp_idx in (idx_in_bg + 1)..end_idx {
                if ext4_bmap_is_bit_clr(&bitmap, tmp_idx) {
                    let block_num = geom.bg_idx_to_addr(tmp_idx, bgid);
                    if self.is_system_reserved_block(block_num) || self.guard.contains(block_num) {
                        continue;
                    }
                    ext4_bmap_bit_set(&mut bitmap, tmp_idx);
                    desc.set_block_bitmap_csum(&self.sb, &bitmap);
                    self.write_block_bitmap(bmp_blk_adr, &bitmap)?;
                    let alloc = geom.bg_idx_to_addr(tmp_idx, bgid);
                    self.update_free_block_counts(inode, bgid, &mut desc)?;
                    *start_bgid = bgid;
                    self.guard.reserve(alloc);
                    return Ok(alloc);
                }
            }

            // (3) 整位图 find_clr（上界用位图位数）。
            let mut rel_blk_idx = 0;
            if ext4_bmap_bit_find_clr(&bitmap, idx_in_bg, max_blocks_in_bitmap, &mut rel_blk_idx) {
                let block_num = geom.bg_idx_to_addr(rel_blk_idx, bgid);
                if !self.is_system_reserved_block(block_num) && !self.guard.contains(block_num) {
                    ext4_bmap_bit_set(&mut bitmap, rel_blk_idx);
                    desc.set_block_bitmap_csum(&self.sb, &bitmap);
                    self.write_block_bitmap(bmp_blk_adr, &bitmap)?;
                    let alloc = geom.bg_idx_to_addr(rel_blk_idx, bgid);
                    self.update_free_block_counts(inode, bgid, &mut desc)?;
                    *start_bgid = bgid;
                    self.guard.reserve(alloc);
                    return Ok(alloc);
                }
            }

            bgid = (bgid + 1) % block_group_count;
            count -= 1;
            idx_in_bg = 0;
        }

        Err(Error::with_message(
            Errno::ENOSPC,
            "No free blocks available in all block groups",
        ))
    }

    // ------------------------------------------------------------------
    // 入口 3：balloc_alloc_block_batch(&mut start_bgid, count)
    // [对照] ext4_rs balloc.rs:716-983
    // ------------------------------------------------------------------

    /// 跨组分配 `count` 块（允许部分成功），回写 `*start_bgid`。返回实际分到的块号。
    pub(super) fn balloc_alloc_block_batch(
        &mut self,
        inode: &mut InodeAllocCtx,
        start_bgid: &mut u32,
        count: usize,
    ) -> Result<Vec<Ext4Fsblk>> {
        let block_size = self.block_size();
        if count == 0 {
            return Ok(Vec::new());
        }

        let block_group_count = self.sb.block_group_count();
        if block_group_count == 0 {
            return Err(Error::with_message(Errno::EINVAL, "Invalid block group count"));
        }
        if *start_bgid >= block_group_count {
            *start_bgid = 0;
        }

        let mut bgid = *start_bgid;
        let mut result: Vec<Ext4Fsblk> = Vec::with_capacity(count);
        let mut remaining = count;
        let mut groups_checked = 0u32;

        while remaining > 0 && groups_checked < block_group_count {
            let mut desc = self.load_group_desc(bgid);

            let free_blocks = desc.get_free_blocks_count();
            if free_blocks == 0 {
                bgid = (bgid + 1) % block_group_count;
                groups_checked += 1;
                continue;
            }

            let bmp_blk_adr = desc.block_bitmap();
            let mut bitmap = self.load_block_bitmap(&desc);

            let geom = GroupGeometry::new(&self.sb);
            let first_in_bg = geom.get_block_of_bgid(bgid);
            let first_in_bg_index = geom.addr_to_idx_bg(first_in_bg);
            let idx_in_bg =
                first_in_bg_index.max(geom.first_non_reserved_idx_in_group(bgid, Some(&self.zones)));
            let blocks_per_group = self.sb.blocks_per_group();

            let mut found_blocks = 0usize;
            let max_to_find = min(remaining, free_blocks as usize);
            let mut rel_blk_idx = 0u32;
            let mut current_idx = idx_in_bg;

            // 顺扫段：从 current_idx 起逐位。
            while found_blocks < max_to_find && current_idx < blocks_per_group {
                if current_idx >= block_size as u32 * 8 {
                    break;
                }
                if ext4_bmap_is_bit_clr(&bitmap, current_idx) {
                    let block_num = geom.bg_idx_to_addr(current_idx, bgid);
                    if self.is_system_reserved_block(block_num) {
                        current_idx += 1;
                        continue;
                    }
                    if self.guard.contains(block_num) {
                        current_idx += 1;
                        continue;
                    }
                    if inode.maps_block(block_num) {
                        current_idx += 1;
                        continue;
                    }
                    ext4_bmap_bit_set(&mut bitmap, current_idx);
                    let block_num = geom.bg_idx_to_addr(current_idx, bgid);
                    result.push(block_num);
                    found_blocks += 1;
                }
                current_idx += 1;
            }

            // 不够则用 find_clr 续找。
            if found_blocks < max_to_find {
                let mut start_idx = current_idx;
                while found_blocks < max_to_find {
                    let end_idx = min(blocks_per_group, block_size as u32 * 8);
                    if !ext4_bmap_bit_find_clr(&bitmap, start_idx, end_idx, &mut rel_blk_idx) {
                        break;
                    }
                    let block_num = geom.bg_idx_to_addr(rel_blk_idx, bgid);
                    if self.is_system_reserved_block(block_num) {
                        start_idx = rel_blk_idx + 1;
                        continue;
                    }
                    if self.guard.contains(block_num) {
                        start_idx = rel_blk_idx + 1;
                        continue;
                    }
                    if inode.maps_block(block_num) {
                        start_idx = rel_blk_idx + 1;
                        continue;
                    }
                    ext4_bmap_bit_set(&mut bitmap, rel_blk_idx);
                    let block_num = geom.bg_idx_to_addr(rel_blk_idx, bgid);
                    result.push(block_num);
                    found_blocks += 1;
                }
            }

            // 命中则更新元数据（顺序逐位复刻 ext4_rs：位图 → 组 free → SB free → 组 csum+写 → inode）。
            if found_blocks > 0 {
                desc.set_block_bitmap_csum(&self.sb, &bitmap);
                self.write_block_bitmap(bmp_blk_adr, &bitmap)?;

                // 组 free_blocks -= found（ext4_rs 直接用 free_blocks - found，不再重读）。
                let new_free_count = free_blocks - found_blocks as u64;
                desc.set_free_blocks_count(new_free_count as u32);

                // SB free_blocks -= found。
                let sb_free = self.sb.free_blocks_count();
                self.sb.set_free_blocks_count(sb_free - found_blocks as u64);
                self.write_superblock()?;

                // 组 csum + 写。
                desc.set_checksum(bgid, &self.sb);
                self.write_group_desc(bgid, &desc)?;

                // inode i_blocks += found * (bs/512)（内存）。
                let blocks_per_fs_block = block_size as u64 / EXT4_INODE_BLOCK_SIZE;
                let mut inode_blocks = inode.i_blocks();
                inode_blocks += found_blocks as u64 * blocks_per_fs_block;
                inode.set_i_blocks(inode_blocks);

                remaining -= found_blocks;
            }

            bgid = (bgid + 1) % block_group_count;
            groups_checked += 1;
        }

        // 回写游标。
        *start_bgid = bgid;

        if !result.is_empty() {
            self.guard.reserve_blocks(&result);
            // ext4_rs 此处 write_back_inode（落 inode 表）；core 不落 inode 表，i_blocks 已在内存累加。
        }

        Ok(result)
    }

    // ------------------------------------------------------------------
    // 入口 4：balloc_free_blocks(&mut inode, start, count)
    // [对照] ext4_rs balloc.rs:612-688
    // ------------------------------------------------------------------

    /// 释放从 `start` 起的连续 `count` 个块：逐组定位区间 → `ext4_bmap_bits_free`（闭区间）
    /// 清位 → 位图 csum + 写 → 超级块 free_blocks += → inode i_blocks -= → 组 free_blocks += +
    /// csum + 写。逐位复刻 ext4_rs `balloc_free_blocks`（含 §3.1 怪癖，见下）。
    ///
    /// **§3.1 first_data_block 怪癖（PARITY）**：ext4_rs free 路径用裸除法
    /// `bg = start / blocks_per_group`、`idx_in_bg = start % blocks_per_group` 定位组与组内
    /// 下标，**不**像几何 helper `get_bgid_of_block`/`addr_to_idx_bg` 那样在
    /// `first_data_block != 0 && baddr != 0` 时先减 1。故此处**刻意不走** [`GroupGeometry`]，
    /// 直接裸除——与 ext4_rs 字节级一致。真镜像 `first_data_block == 0`，两种算法本就重合；
    /// 在 `first_data_block != 0` 的盘上会与 alloc 侧的几何 helper 产生偏差，但这是 ext4_rs
    /// 既有行为，parity-first 原样复刻（bug 修复推迟，见 roadmap §5）。
    pub(super) fn balloc_free_blocks(
        &mut self,
        inode: &mut InodeAllocCtx,
        start: Ext4Fsblk,
        count: u32,
    ) {
        let block_size = self.block_size();
        let mut count = count as usize;
        let mut start = start;

        let mut any_freed = false;
        let mut inode_blocks = inode.i_blocks();

        let blocks_per_group = self.sb.blocks_per_group();
        let max_bits_per_bitmap = block_size * 8;
        let max_bits_per_group = min(blocks_per_group as usize, max_bits_per_bitmap);

        // §3.1 怪癖：裸除法定位组（不减 first_data_block）。
        let mut bg_first = start / blocks_per_group as u64;
        let bg_last = (start + count as u64 - 1) / blocks_per_group as u64;

        while bg_first <= bg_last {
            let idx_in_bg = (start % blocks_per_group as u64) as usize;
            if idx_in_bg >= max_bits_per_group {
                // 防御越界位图偏移：跳到下一组（与 ext4_rs 一致：仅 bg_first += 1，不动 start/count）。
                bg_first += 1;
                continue;
            }

            let current_bgid = bg_first as u32;
            // ext4_rs 此处 lock_block_group(current_bgid)；core 单线程 ktest 无并发，
            // 锁语义在集成层接入（Phase 5），此处不复制锁，行为等价。
            let mut desc = self.load_group_desc(current_bgid);

            let block_bitmap_block = desc.block_bitmap();
            let mut bitmap = self.load_block_bitmap(&desc);

            let mut free_cnt = max_bits_per_group - idx_in_bg;
            if count <= free_cnt {
                free_cnt = count;
            }
            if free_cnt == 0 {
                bg_first += 1;
                continue;
            }

            // 闭区间清位：[idx_in_bg, idx_in_bg + free_cnt - 1]。
            ext4_bmap_bits_free(
                &mut bitmap,
                idx_in_bg as u32,
                idx_in_bg as u32 + free_cnt as u32 - 1,
            );

            count -= free_cnt;
            start += free_cnt as u64;

            // 位图 csum + 写（与 ext4_rs 顺序：先 set_csum 再 write_metadata）。
            desc.set_block_bitmap_csum(&self.sb, &bitmap);
            self.write_block_bitmap(block_bitmap_block, &bitmap)
                .expect("write block bitmap");

            // 超级块 free_blocks += free_cnt + csum + 写（对照 add_superblock_free_blocks）。
            let free = self.sb.free_blocks_count();
            self.sb.set_free_blocks_count(free + free_cnt as u64);
            self.write_superblock().expect("write superblock");

            // inode i_blocks -= free_cnt * (block_size/512)（内存累减；core 不落 inode 表）。
            inode_blocks -= (free_cnt * (block_size / EXT4_INODE_BLOCK_SIZE as usize)) as u64;
            any_freed = true;

            // 组 free_blocks += free_cnt + csum（用更新后 SB）+ 写。
            let mut fb_cnt = desc.get_free_blocks_count();
            fb_cnt += free_cnt as u64;
            desc.set_free_blocks_count(fb_cnt as u32);
            desc.set_checksum(current_bgid, &self.sb);
            self.write_group_desc(current_bgid, &desc)
                .expect("write group desc");

            bg_first += 1;
        }

        if any_freed {
            inode.set_i_blocks(inode_blocks);
            // ext4_rs 此处 write_back_inode（落 inode 表）；core 不落 inode 表，i_blocks 已在内存累减。
        }
    }
}

/// 计算系统保留区列表——逐位复刻 ext4_rs `Ext4::get_system_zone`（ext4.rs:45-88）：
/// 每组贡献 ① base meta 块区（有 super 备份时）② 块位图（1 块）③ inode 位图（1 块）
/// ④ inode 表（itable_per_group 块）。core 自算、不依赖 ext4_rs。
fn compute_system_zones<R: BlockReader>(sb: &RawSuperblock, reader: &R) -> Vec<SystemZone> {
    let geom = GroupGeometry::new(sb);
    let group_count = sb.block_group_count();
    let inodes_per_group = sb.inodes_per_group() as u64;
    let inode_size = sb.inode_size() as u64;
    let block_size = sb.block_size() as u64;

    let bs = sb.block_size();
    let desc_size = sb.group_desc_size();
    let dsc_cnt = bs / desc_size;
    let first_data_block = sb.first_data_block() as usize;

    let mut zones: Vec<SystemZone> = Vec::new();
    for bgid in 0..group_count {
        // ① base meta 块（含 super 备份 + GDT）。
        let meta_blks = geom.num_base_meta_blocks(bgid);
        if meta_blks != 0 {
            let start = geom.get_block_of_bgid(bgid);
            zones.push(SystemZone {
                group: bgid,
                start_blk: start,
                end_blk: start + meta_blks as u64 - 1,
            });
        }

        // 读该组描述符（拿位图 / inode 表块号）。
        let dsc_id = bgid as usize / dsc_cnt;
        let block_id = first_data_block + dsc_id + 1;
        let offset_in_block = (bgid as usize % dsc_cnt) * desc_size;
        let mut buf = [0u8; 64];
        reader.read_at(block_id * bs + offset_in_block, &mut buf);
        let desc = RawGroupDescriptor::from_bytes(&buf);

        // ② 块位图（1 块）。
        let blk_bmp = desc.block_bitmap();
        zones.push(SystemZone {
            group: bgid,
            start_blk: blk_bmp,
            end_blk: blk_bmp,
        });
        // ③ inode 位图（1 块）。
        let ino_bmp = desc.inode_bitmap();
        zones.push(SystemZone {
            group: bgid,
            start_blk: ino_bmp,
            end_blk: ino_bmp,
        });
        // ④ inode 表（itable_per_group 块）。
        let ino_tbl = desc.inode_table();
        let itb_per_group = (inodes_per_group * inode_size + block_size - 1) / block_size;
        zones.push(SystemZone {
            group: bgid,
            start_blk: ino_tbl,
            end_blk: ino_tbl + itb_per_group - 1,
        });
    }
    zones
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    // `OperationAllocGuard` 需在作用域内才能在 `Arc<dyn OperationAllocGuard>` 上调用
    // `clear_current_operation()`（见 `balloc_diff_free_then_realloc` 的对称化处理）。
    use ext4_rs::OperationAllocGuard;

    use super::{BlockAllocator, InodeAllocCtx};
    use crate::fs::ext4::core::diff_harness::{
        assert_meta_eq, snapshot_meta, DirectMetadataWriter, MemDisk,
    };
    use crate::fs::ext4::core::io::BlockReader;
    use crate::fs::ext4::core::superblock::RawSuperblock;
    use crate::fs::ext4::core::test_util::EXT4_IMAGE;
    use crate::prelude::*;

    /// 读镜像超级块（盘内偏移 1024、长 1024）。
    fn read_sb(disk: &MemDisk) -> RawSuperblock {
        let mut buf = vec![0u8; 1024];
        disk.read_at(1024, buf.as_mut_slice());
        RawSuperblock::from_bytes(&buf)
    }

    /// 构造一个空 extent / 不映射任何块的旧侧 inode_ref：直接拿 `Ext4Inode::default()`
    /// （flags=0 → 走 slow 路径，size=0 → file_blocks=0 → maps_block 恒 false；blocks=0 → i_blocks 从 0 起）。
    /// 不读盘、不分配 inode，故对旧盘**零分歧**（仅 update_free_block_counts 末尾的
    /// write_back_inode 会写该 inode 表槽，落在 snapshot_meta 覆盖范围之外，不影响对拍）。
    /// inode_num 取一个合法值（inodes_count 内）即可，仅供 write_back_inode 定位表槽。
    fn make_old_inode_ref(inode_num: u32) -> ext4_rs::Ext4InodeRef {
        ext4_rs::Ext4InodeRef {
            inode_num,
            inode: ext4_rs::Ext4Inode::default(),
        }
    }

    /// 差分一个「带 goal」分配：新旧各跑一次 balloc_alloc_block(Some(goal))，比
    /// (A) 返回块号；(B) snapshot_meta 逐字节；(C) i_blocks。
    fn diff_one_goal(goal: Option<u64>) {
        let disk_old = MemDisk::from_image(EXT4_IMAGE);
        let disk_new = MemDisk::from_image(EXT4_IMAGE);

        // --- 旧侧 ---
        let old = ext4_rs::Ext4::open(Arc::new(disk_old.clone()));
        let mut old_inode = make_old_inode_ref(11);
        let old_ret = old.balloc_alloc_block(&mut old_inode, goal);

        // --- 新侧 ---
        let sb = read_sb(&disk_new);
        let bs = sb.block_size();
        let writer = DirectMetadataWriter::new(disk_new.clone(), bs);
        let mut alloc = BlockAllocator::new(sb, &disk_new, &writer);
        let mut new_inode = InodeAllocCtx::new(0);
        let new_ret = alloc.balloc_alloc_block(&mut new_inode, goal);

        // (A) 返回块号一致（含错误码一致）。
        match (&old_ret, &new_ret) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "alloc block mismatch goal={goal:?}"),
            (Err(_), Err(_)) => {}
            _ => panic!("alloc ok/err mismatch goal={goal:?}: old={old_ret:?} new={new_ret:?}"),
        }

        // (B) 盘面逐字节一致。
        let sb_old = read_sb(&disk_old);
        let snap_old = snapshot_meta(&disk_old, &sb_old);
        let snap_new = snapshot_meta(&disk_new, alloc.superblock());
        assert_meta_eq(&snap_old, &snap_new);

        // (C) i_blocks 一致。
        assert_eq!(
            old_inode.inode.blocks_count(),
            new_inode.i_blocks(),
            "i_blocks mismatch goal={goal:?}"
        );
    }

    /// 单块分配差分：goal 命中（指向某组内一个空闲数据块）。
    /// 用真镜像 group0 数据区起点附近一个块号作 goal。
    #[ktest]
    fn balloc_diff_alloc_block_goal_hit() {
        // 选一个落在 group0 数据区、当前空闲的块作 goal。真镜像 4096B 块、bpg=32768，
        // group0 元数据占低位若干块；取一个明显在数据区的块号（如 5000）作 goal。
        diff_one_goal(Some(5000));
    }

    /// 单块分配差分：无 goal（默认 bgid=1、idx=0），走默认组扫描。
    #[ktest]
    fn balloc_diff_alloc_block_no_goal() {
        diff_one_goal(None);
    }

    /// 单块分配差分：goal 落在系统保留区（组首元数据块，如块 1）——须跳过保留区另寻。
    #[ktest]
    fn balloc_diff_alloc_block_goal_in_reserved() {
        diff_one_goal(Some(1));
    }

    /// 单块分配差分：goal=0（first_data_block==0 时块 0 即 boot/super 块，必落保留区）。
    #[ktest]
    fn balloc_diff_alloc_block_goal_zero() {
        diff_one_goal(Some(0));
    }

    /// 单块分配差分：goal 指向最后一组（验证 bgid clamp / 跨组回绕）。
    #[ktest]
    fn balloc_diff_alloc_block_goal_last_group() {
        let disk = MemDisk::from_image(EXT4_IMAGE);
        let sb = read_sb(&disk);
        let bpg = sb.blocks_per_group() as u64;
        let gc = sb.block_group_count() as u64;
        // 最后一组首块附近。
        let goal = (gc - 1) * bpg + 100;
        diff_one_goal(Some(goal));
    }

    /// 连续多次 goal 分配差分：在同一对盘上连跑多次（每次喂新旧相同 goal），
    /// 验证多步分配后盘面 + i_blocks 仍逐字节/逐值一致（覆盖位图推进、计数累减）。
    #[ktest]
    fn balloc_diff_alloc_block_sequence() {
        let disk_old = MemDisk::from_image(EXT4_IMAGE);
        let disk_new = MemDisk::from_image(EXT4_IMAGE);

        let old = ext4_rs::Ext4::open(Arc::new(disk_old.clone()));
        let mut old_inode = make_old_inode_ref(11);

        let sb = read_sb(&disk_new);
        let bs = sb.block_size();
        let writer = DirectMetadataWriter::new(disk_new.clone(), bs);
        let mut alloc = BlockAllocator::new(sb, &disk_new, &writer);
        let mut new_inode = InodeAllocCtx::new(0);

        // 一串混合 goal：命中数据区、落保留区、相邻（触发短扫）、无 goal。
        let goals: [Option<u64>; 8] = [
            Some(5000),
            Some(5001),
            Some(1),
            None,
            Some(20000),
            Some(20000),
            Some(2),
            None,
        ];
        for (i, &g) in goals.iter().enumerate() {
            let old_ret = old.balloc_alloc_block(&mut old_inode, g);
            let new_ret = alloc.balloc_alloc_block(&mut new_inode, g);
            match (&old_ret, &new_ret) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "seq step {i} block mismatch g={g:?}"),
                (Err(_), Err(_)) => {}
                _ => panic!("seq step {i} ok/err mismatch g={g:?}: old={old_ret:?} new={new_ret:?}"),
            }
        }

        let sb_old = read_sb(&disk_old);
        assert_meta_eq(
            &snapshot_meta(&disk_old, &sb_old),
            &snapshot_meta(&disk_new, alloc.superblock()),
        );
        assert_eq!(
            old_inode.inode.blocks_count(),
            new_inode.i_blocks(),
            "sequence final i_blocks mismatch"
        );
    }

    /// `balloc_alloc_block_from` 游标差分：新旧各从同一 start_bgid 起分配若干次，
    /// 每次比返回块号 + 游标回写值，最后比盘面 + i_blocks。
    #[ktest]
    fn balloc_diff_alloc_block_from_cursor() {
        let disk_old = MemDisk::from_image(EXT4_IMAGE);
        let disk_new = MemDisk::from_image(EXT4_IMAGE);

        let old = ext4_rs::Ext4::open(Arc::new(disk_old.clone()));
        let mut old_inode = make_old_inode_ref(11);

        let sb = read_sb(&disk_new);
        let bs = sb.block_size();
        let writer = DirectMetadataWriter::new(disk_new.clone(), bs);
        let mut alloc = BlockAllocator::new(sb, &disk_new, &writer);
        let mut new_inode = InodeAllocCtx::new(0);

        let mut old_cursor = 0u32;
        let mut new_cursor = 0u32;
        for i in 0..6 {
            let old_ret = old.balloc_alloc_block_from(&mut old_inode, &mut old_cursor);
            let new_ret = alloc.balloc_alloc_block_from(&mut new_inode, &mut new_cursor);
            match (&old_ret, &new_ret) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "from step {i} block mismatch"),
                (Err(_), Err(_)) => {}
                _ => panic!("from step {i} ok/err mismatch: old={old_ret:?} new={new_ret:?}"),
            }
            assert_eq!(old_cursor, new_cursor, "from step {i} cursor mismatch");
        }

        let sb_old = read_sb(&disk_old);
        assert_meta_eq(
            &snapshot_meta(&disk_old, &sb_old),
            &snapshot_meta(&disk_new, alloc.superblock()),
        );
        assert_eq!(
            old_inode.inode.blocks_count(),
            new_inode.i_blocks(),
            "from final i_blocks mismatch"
        );
    }

    /// `balloc_alloc_block_batch` 差分：新旧各请求同一批量 count，比返回块号向量逐个、
    /// 游标回写、盘面、i_blocks。覆盖单组内可满足的中等批量。
    #[ktest]
    fn balloc_diff_alloc_block_batch_small() {
        diff_batch(0, 16);
    }

    /// `balloc_alloc_block_batch` 部分成功差分：请求一个超大 count（远超单组可用），
    /// 迫使跨组 + 部分成功，比返回块号向量逐个、游标、盘面、i_blocks。
    #[ktest]
    fn balloc_diff_alloc_block_batch_partial_cross_group() {
        // 一个很大的 count：超过单组空闲，迫使跨多组与（极可能）部分成功。
        diff_batch(0, 100_000);
    }

    /// batch 差分公共体：同一 start_bgid + count 喂新旧，比 (A) 块号向量逐个 + 游标；
    /// (B) 盘面逐字节；(C) i_blocks。
    fn diff_batch(start_bgid: u32, count: usize) {
        let disk_old = MemDisk::from_image(EXT4_IMAGE);
        let disk_new = MemDisk::from_image(EXT4_IMAGE);

        let old = ext4_rs::Ext4::open(Arc::new(disk_old.clone()));
        let mut old_inode = make_old_inode_ref(11);
        let mut old_cursor = start_bgid;
        let old_vec = old
            .balloc_alloc_block_batch(&mut old_inode, &mut old_cursor, count)
            .expect("old batch");

        let sb = read_sb(&disk_new);
        let bs = sb.block_size();
        let writer = DirectMetadataWriter::new(disk_new.clone(), bs);
        let mut alloc = BlockAllocator::new(sb, &disk_new, &writer);
        let mut new_inode = InodeAllocCtx::new(0);
        let mut new_cursor = start_bgid;
        let new_vec = alloc
            .balloc_alloc_block_batch(&mut new_inode, &mut new_cursor, count)
            .expect("new batch");

        // (A) 块号向量逐个一致 + 游标一致。
        assert_eq!(old_vec.len(), new_vec.len(), "batch len mismatch count={count}");
        for (i, (a, b)) in old_vec.iter().zip(new_vec.iter()).enumerate() {
            assert_eq!(a, b, "batch block[{i}] mismatch count={count}");
        }
        assert_eq!(old_cursor, new_cursor, "batch cursor mismatch count={count}");

        // (B) 盘面逐字节。
        let sb_old = read_sb(&disk_old);
        assert_meta_eq(
            &snapshot_meta(&disk_old, &sb_old),
            &snapshot_meta(&disk_new, alloc.superblock()),
        );

        // (C) i_blocks。
        assert_eq!(
            old_inode.inode.blocks_count(),
            new_inode.i_blocks(),
            "batch i_blocks mismatch count={count}"
        );
    }

    // ============================== balloc_free_blocks 差分 ==============================

    /// free 差分公共体：两侧先用 `balloc_alloc_block(None)` 各分配 `n_alloc` 个块（无 goal、
    /// 同序列 → 同块号集合），再 `balloc_free_blocks(free_start, free_count)` 释放同一区间，
    /// 比 (A) 分配阶段每步块号一致；(B) 释放后 snapshot_meta 逐字节；(C) i_blocks。
    /// `free_start` 若为 `None`，则取首次分配返回的块号作为释放区间起点。
    fn diff_free(n_alloc: usize, free_start: Option<u64>, free_count: u32) {
        let disk_old = MemDisk::from_image(EXT4_IMAGE);
        let disk_new = MemDisk::from_image(EXT4_IMAGE);

        // --- 旧侧分配 ---
        let old = ext4_rs::Ext4::open(Arc::new(disk_old.clone()));
        let mut old_inode = make_old_inode_ref(11);

        // --- 新侧分配 ---
        let sb = read_sb(&disk_new);
        let bs = sb.block_size();
        let writer = DirectMetadataWriter::new(disk_new.clone(), bs);
        let mut alloc = BlockAllocator::new(sb, &disk_new, &writer);
        let mut new_inode = InodeAllocCtx::new(0);

        let mut first_block: Option<u64> = None;
        for i in 0..n_alloc {
            let old_ret = old.balloc_alloc_block(&mut old_inode, None);
            let new_ret = alloc.balloc_alloc_block(&mut new_inode, None);
            match (&old_ret, &new_ret) {
                (Ok(a), Ok(b)) => {
                    assert_eq!(a, b, "free-setup alloc step {i} block mismatch");
                    if first_block.is_none() {
                        first_block = Some(*a);
                    }
                }
                (Err(_), Err(_)) => {}
                _ => panic!(
                    "free-setup alloc step {i} ok/err mismatch: old={old_ret:?} new={new_ret:?}"
                ),
            }
        }

        // 分配后两侧盘面应一致（前置自检）。
        {
            let sb_old = read_sb(&disk_old);
            assert_meta_eq(
                &snapshot_meta(&disk_old, &sb_old),
                &snapshot_meta(&disk_new, alloc.superblock()),
            );
        }

        let start = free_start.or(first_block).expect("no block to free");

        // --- 两侧释放同一区间 ---
        old.balloc_free_blocks(&mut old_inode, start, free_count);
        alloc.balloc_free_blocks(&mut new_inode, start, free_count);

        // (B) 释放后盘面逐字节一致。
        let sb_old = read_sb(&disk_old);
        assert_meta_eq(
            &snapshot_meta(&disk_old, &sb_old),
            &snapshot_meta(&disk_new, alloc.superblock()),
        );

        // (C) i_blocks 一致。
        assert_eq!(
            old_inode.inode.blocks_count(),
            new_inode.i_blocks(),
            "free i_blocks mismatch start={start} count={free_count}"
        );
    }

    /// 单块释放差分：分配 4 块后释放首块（free_count=1）。
    #[ktest]
    fn balloc_diff_free_single_block() {
        diff_free(4, None, 1);
    }

    /// 多块释放差分：分配 8 块后从首块起释放 4 块（连续区间，落在同一组内）。
    #[ktest]
    fn balloc_diff_free_multi_block() {
        diff_free(8, None, 4);
    }

    /// 释放后可重分配差分（guard 无关）：分配 4 块 → 清空 alloc_guard → 释放首块 → 再分配
    /// 一次（应落回刚释放的位）。比每步块号 + 最终盘面 + i_blocks，验证 free 把位真正清回、
    /// 可被后续 alloc 复用。
    ///
    /// 为何要先清空 guard：ext4_rs 的 `balloc_alloc_block` 在每次命中后把块登记进当前操作的
    /// `alloc_guard`，而 `balloc_free_blocks` 只清位、**不释放 guard 预留**。本测试在同一个
    /// `old` 句柄、同一次默认操作内连续分配/释放/再分配——若不清 guard，旧侧再分配会跳过刚
    /// 释放的块（仍被 guard 预留），单组镜像下耗尽扫描返回 ENOSPC，而新侧的 `AllocGuardStub`
    /// 不预留任何块，两侧 guard 行为不对称，构不成有效差分（guard 在 Task 5 才两侧接实）。
    /// 故先 `clear_current_operation()` 把旧侧默认操作清空，使两侧 guard 同为空——本测试遂成
    /// 「free 清位即可被复用」这一 **guard 无关** 的有效差分。
    /// guard 敏感的 free-then-realloc 复用（不清 guard、应跳过刚释放块）留待 Task 5 两侧接实
    /// 真实 guard 后专门验证。
    #[ktest]
    fn balloc_diff_free_then_realloc() {
        let disk_old = MemDisk::from_image(EXT4_IMAGE);
        let disk_new = MemDisk::from_image(EXT4_IMAGE);

        let old = ext4_rs::Ext4::open(Arc::new(disk_old.clone()));
        let mut old_inode = make_old_inode_ref(11);

        let sb = read_sb(&disk_new);
        let bs = sb.block_size();
        let writer = DirectMetadataWriter::new(disk_new.clone(), bs);
        let mut alloc = BlockAllocator::new(sb, &disk_new, &writer);
        let mut new_inode = InodeAllocCtx::new(0);

        // 分配 4 块，记录首块。
        let mut first_block = 0u64;
        for i in 0..4 {
            let o = old.balloc_alloc_block(&mut old_inode, None).expect("old alloc");
            let n = alloc.balloc_alloc_block(&mut new_inode, None).expect("new alloc");
            assert_eq!(o, n, "realloc-setup step {i} mismatch");
            if i == 0 {
                first_block = o;
            }
        }

        // 清空旧侧默认操作的 alloc_guard，使两侧 guard 同为空（新侧 AllocGuardStub 本就不预留）。
        // 这样刚释放的块在两侧都不再被 guard 跳过，本测试遂只验证「free 清位 → 可被复用」。
        old.alloc_guard.clear_current_operation();

        // 释放首块。
        old.balloc_free_blocks(&mut old_inode, first_block, 1);
        alloc.balloc_free_blocks(&mut new_inode, first_block, 1);

        // 再分配一次：两侧应返回同一块号（极可能就是刚释放的 first_block）。
        let o = old.balloc_alloc_block(&mut old_inode, None).expect("old realloc");
        let n = alloc.balloc_alloc_block(&mut new_inode, None).expect("new realloc");
        assert_eq!(o, n, "realloc block number mismatch");

        let sb_old = read_sb(&disk_old);
        assert_meta_eq(
            &snapshot_meta(&disk_old, &sb_old),
            &snapshot_meta(&disk_new, alloc.superblock()),
        );
        assert_eq!(
            old_inode.inode.blocks_count(),
            new_inode.i_blocks(),
            "free-then-realloc final i_blocks mismatch"
        );
    }
}
