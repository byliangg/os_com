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
//!   不依赖 ext4_rs；`alloc_guard`（每操作块预留，[`super::alloc_guard`]）在扫描候选时
//!   `contains_current_block` 跳过本操作已预留块、命中后 `reserve_current_block` 登记，
//!   调用点逐位对齐 ext4_rs。注意：本 Phase 的 [`MetadataWriter`] 立即写、无 JBD2 overlay，
//!   位图当场即权威，故 guard 的端到端 skip 效果不会真正触发（要等 Phase 5 deferred 写才
//!   显现）；本 Phase 验的是 guard 机制 + balloc 接线点 parity。
//!
//! inode 侧：本 Phase 尚无 extent 逻辑（Phase 3），故 [`InodeAllocCtx::maps_block`] 恒
//! 返回 `false`，`i_blocks` 仅在内存累加（balloc 不写 inode 表）。
//!
//! [对照来源] kernel/libs/ext4_rs/src/ext4_impls/balloc.rs

use core::cmp::min;

use super::alloc_guard::{LocalOperationAllocGuard, OperationAllocGuard};
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

/// 分配路径所需的 inode 视图（i_blocks 累加 + 已映射块查询）。
///
/// 对照 ext4_rs `Ext4InodeRef` 在 balloc 里被用到的两件事：
/// - `inode.blocks_count()` / `set_blocks_count()`（i_blocks，单位 512B）；
/// - `inode_already_maps_block(inode_ref, block)`（extent 映射查询）。
///
/// 本 Phase 无 extent 逻辑（Phase 3 落地），故 [`maps_block`] 恒 `false`。
pub(in crate::fs::ext4) struct InodeAllocCtx {
    /// i_blocks（512-byte 单位），与 ext4_rs `inode.blocks_count()` 同语义。
    i_blocks: u64,
}

impl InodeAllocCtx {
    /// 以给定初始 i_blocks 构造（差分两侧建议都从 0 起）。
    pub(in crate::fs::ext4) fn new(i_blocks: u64) -> Self {
        Self { i_blocks }
    }

    /// 当前 i_blocks（512-byte 单位）。
    pub(in crate::fs::ext4) fn i_blocks(&self) -> u64 {
        self.i_blocks
    }

    /// 设置 i_blocks。
    pub(in crate::fs::ext4) fn set_i_blocks(&mut self, v: u64) {
        self.i_blocks = v;
    }

    /// 该 inode 是否已映射物理块 `block`。
    ///
    /// **P3 填实**：本 Phase 无 extent / 间接映射逻辑，恒返回 `false`（与差分两侧用
    /// 空 extent inode 时 ext4_rs `inode_already_maps_block` 的结果一致）。
    /// [对照] ext4_rs `inode_already_maps_block`（balloc.rs:156-172）。
    #[inline]
    pub(in crate::fs::ext4) fn maps_block(&self, _block: Ext4Fsblk) -> bool {
        false
    }
}

/// 安全块分配器。持一份可变超级块（free-blocks 计数权威）+ 读接缝 + 元数据写回 +
/// 系统保留区 + 每操作块预留 guard。**不复制 ext4_rs 的 `Ext4` god-object**。
pub(in crate::fs::ext4) struct BlockAllocator<'a, R: BlockReader, W: MetadataWriter> {
    /// 运行期可变超级块（free-blocks 随分配递减；其余字段同盘上初值）。
    sb: RawSuperblock,
    /// 读盘接缝（每轮组迭代重读组描述符 / 位图）。
    reader: &'a R,
    /// 元数据写回接缝（位图 / 组描述符 / 超级块）。
    writer: &'a W,
    /// 系统保留区列表（core 自算，对照 ext4_rs `get_system_zone`）。
    zones: Vec<SystemZone>,
    /// 每操作块预留 guard。balloc 在扫描候选时 `contains_current_block` 跳过本操作已预留
    /// 的块、命中后 `reserve_current_block` 登记——调用点逐位对齐 ext4_rs `balloc_*`。
    /// [`new`] 默认装一个 `Arc<LocalOperationAllocGuard>`，与 ext4_rs `Ext4::open` 安装的
    /// 默认 guard（默认操作 id=0）行为一致；[`with_guard`] 可注入共享 guard（差分用）。
    ///
    /// [`new`]: BlockAllocator::new
    /// [`with_guard`]: BlockAllocator::with_guard
    guard: Arc<dyn OperationAllocGuard>,
    /// 写元数据用的 JBD2 handle id 占位（本 Phase 直写，handle 语义 Phase 5 接入）。
    handle_id: u64,
}

impl<'a, R: BlockReader, W: MetadataWriter> BlockAllocator<'a, R, W> {
    /// 用初始超级块字节 + 读/写接缝构造，并自算系统保留区。默认装一个空的
    /// `LocalOperationAllocGuard`（默认操作 id=0），与 ext4_rs `Ext4::open` 一致。
    pub(in crate::fs::ext4) fn new(sb: RawSuperblock, reader: &'a R, writer: &'a W) -> Self {
        Self::with_guard(sb, reader, writer, Arc::new(LocalOperationAllocGuard::new()))
    }

    /// 用外部注入的共享 guard 构造（差分用：跨多步保留同一 guard 以便读 `debug_stats`、
    /// 验证 free 后块仍被预留等 guard 敏感行为）。
    pub(in crate::fs::ext4) fn with_guard(
        sb: RawSuperblock,
        reader: &'a R,
        writer: &'a W,
        guard: Arc<dyn OperationAllocGuard>,
    ) -> Self {
        let zones = compute_system_zones(&sb, reader);
        Self {
            sb,
            reader,
            writer,
            zones,
            guard,
            handle_id: 0,
        }
    }

    /// 当前（运行期）超级块快照——差分用例跑完后据此 `snapshot_meta`。
    pub(in crate::fs::ext4) fn superblock(&self) -> &RawSuperblock {
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
    pub(in crate::fs::ext4) fn balloc_alloc_block(
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
                    || self.guard.contains_current_block(block_num)
                    || inode.maps_block(block_num)
                {
                    // 跳过 system zone
                } else {
                    ext4_bmap_bit_set(&mut bitmap, idx_in_bg);
                    desc.set_block_bitmap_csum(&self.sb, &bitmap);
                    self.write_block_bitmap(bmp_blk_adr, &bitmap)?;
                    let alloc = geom.bg_idx_to_addr(idx_in_bg, bgid);
                    self.update_free_block_counts(inode, bgid, &mut desc)?;
                    self.guard.reserve_current_block(alloc);
                    return Ok(alloc);
                }
            }

            // (2) 从 idx+1 短扫到下个 64-bit 边界。
            let blk_in_bg = blocks_per_group;
            let end_idx = min((idx_in_bg + 63) & !63, blk_in_bg);
            for tmp_idx in (idx_in_bg + 1)..end_idx {
                if ext4_bmap_is_bit_clr(&bitmap, tmp_idx) {
                    let block_num = geom.bg_idx_to_addr(tmp_idx, bgid);
                    if self.is_system_reserved_block(block_num) || self.guard.contains_current_block(block_num) {
                        continue;
                    }
                    ext4_bmap_bit_set(&mut bitmap, tmp_idx);
                    desc.set_block_bitmap_csum(&self.sb, &bitmap);
                    self.write_block_bitmap(bmp_blk_adr, &bitmap)?;
                    let alloc = geom.bg_idx_to_addr(tmp_idx, bgid);
                    self.update_free_block_counts(inode, bgid, &mut desc)?;
                    self.guard.reserve_current_block(alloc);
                    return Ok(alloc);
                }
            }

            // (3) 整组 find_clr。
            let mut rel_blk_idx = 0;
            if ext4_bmap_bit_find_clr(&bitmap, idx_in_bg, blk_in_bg, &mut rel_blk_idx) {
                let block_num = geom.bg_idx_to_addr(rel_blk_idx, bgid);
                if !self.is_system_reserved_block(block_num) && !self.guard.contains_current_block(block_num) {
                    ext4_bmap_bit_set(&mut bitmap, rel_blk_idx);
                    desc.set_block_bitmap_csum(&self.sb, &bitmap);
                    self.write_block_bitmap(bmp_blk_adr, &bitmap)?;
                    let alloc = geom.bg_idx_to_addr(rel_blk_idx, bgid);
                    self.update_free_block_counts(inode, bgid, &mut desc)?;
                    self.guard.reserve_current_block(alloc);
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
    pub(in crate::fs::ext4) fn balloc_alloc_block_from(
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
                if self.guard.contains_current_block(block_num) {
                    idx_in_bg = idx_in_bg.saturating_add(1);
                    continue;
                }
                ext4_bmap_bit_set(&mut bitmap, idx_in_bg);
                desc.set_block_bitmap_csum(&self.sb, &bitmap);
                self.write_block_bitmap(bmp_blk_adr, &bitmap)?;
                let alloc = block_num;
                self.update_free_block_counts(inode, bgid, &mut desc)?;
                *start_bgid = bgid;
                self.guard.reserve_current_block(alloc);
                return Ok(alloc);
            }

            // (2) 短扫到下个 64-bit 边界（上界用位图位数）。
            let end_idx = min((idx_in_bg + 63) & !63, max_blocks_in_bitmap);
            for tmp_idx in (idx_in_bg + 1)..end_idx {
                if ext4_bmap_is_bit_clr(&bitmap, tmp_idx) {
                    let block_num = geom.bg_idx_to_addr(tmp_idx, bgid);
                    if self.is_system_reserved_block(block_num) || self.guard.contains_current_block(block_num) {
                        continue;
                    }
                    ext4_bmap_bit_set(&mut bitmap, tmp_idx);
                    desc.set_block_bitmap_csum(&self.sb, &bitmap);
                    self.write_block_bitmap(bmp_blk_adr, &bitmap)?;
                    let alloc = geom.bg_idx_to_addr(tmp_idx, bgid);
                    self.update_free_block_counts(inode, bgid, &mut desc)?;
                    *start_bgid = bgid;
                    self.guard.reserve_current_block(alloc);
                    return Ok(alloc);
                }
            }

            // (3) 整位图 find_clr（上界用位图位数）。
            let mut rel_blk_idx = 0;
            if ext4_bmap_bit_find_clr(&bitmap, idx_in_bg, max_blocks_in_bitmap, &mut rel_blk_idx) {
                let block_num = geom.bg_idx_to_addr(rel_blk_idx, bgid);
                if !self.is_system_reserved_block(block_num) && !self.guard.contains_current_block(block_num) {
                    ext4_bmap_bit_set(&mut bitmap, rel_blk_idx);
                    desc.set_block_bitmap_csum(&self.sb, &bitmap);
                    self.write_block_bitmap(bmp_blk_adr, &bitmap)?;
                    let alloc = geom.bg_idx_to_addr(rel_blk_idx, bgid);
                    self.update_free_block_counts(inode, bgid, &mut desc)?;
                    *start_bgid = bgid;
                    self.guard.reserve_current_block(alloc);
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
    pub(in crate::fs::ext4) fn balloc_alloc_block_batch(
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
                    if self.guard.contains_current_block(block_num) {
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
                    if self.guard.contains_current_block(block_num) {
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
            self.guard.reserve_current_blocks(&result);
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
    /// csum + 写。
    ///
    /// **first_data_block 几何（BUG-4 修复，ext4-spec-correct）**：组号 / 组内下标必须经
    /// [`GroupGeometry::get_bgid_of_block`] / [`GroupGeometry::addr_to_idx_bg`]——它们在
    /// `first_data_block != 0 && baddr != 0` 时先减 1，与 alloc 侧（`balloc_alloc_block` 等）
    /// 完全一致。旧 ext4_rs free 路径用裸除法 `start / blocks_per_group`、`start %
    /// blocks_per_group`，**不减** first_data_block → 在 `first_data_block == 1`（1K 块 fs，
    /// 如 `ext4_multigroup.img`）上组定位偏移一个块、清错位图位 + 记错组计数。`first_data_block
    /// == 0`（4K 默认）下两种算法重合。
    /// [对照] ext4 中块号 `baddr` 的组号 = `(baddr - first_data_block) / blocks_per_group`、
    ///   组内下标 = `(baddr - first_data_block) % blocks_per_group`（Linux `ext4_get_group_no_and_offset`）。
    pub(in crate::fs::ext4) fn balloc_free_blocks(
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

        // BUG-4 修复：用几何 helper 定位组（减 first_data_block），与 alloc 侧一致。
        // first_data_block / blocks_per_group 在释放过程中不变，故每轮新建 geom 等价
        // （与本文件其它 entry 一致：循环内 `GroupGeometry::new(&self.sb)`），避免跨可变借用持有。
        let (mut bg_first, bg_last) = {
            let geom = GroupGeometry::new(&self.sb);
            (
                geom.get_bgid_of_block(start) as u64,
                geom.get_bgid_of_block(start + count as u64 - 1) as u64,
            )
        };

        while bg_first <= bg_last {
            let idx_in_bg = GroupGeometry::new(&self.sb).addr_to_idx_bg(start) as usize;
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
            // BUG-14 fix（ext4-spec-correct，非 parity）：ext4 规范下 i_blocks 永不下溢。
            // ext4_rs 此处用无符号 `-=`，当单次释放的 512B 当量超过当前 i_blocks 时 debug
            // 直接 panic（`attempt to subtract with overflow`）。改用 `saturating_sub` 夹零——
            // 正常删除（释放量 ≤ i_blocks）数值不变；异常/超额释放夹到 0 而非崩溃。
            // [对照] Linux `ext4_free_blocks` 经 `dquot_free_block` 调 `ext4_inode_blocks`
            //   记账，i_blocks 是 512B 单位且永不为负。BUG-15 修好后本路径不再超额释放，
            //   此处 saturate 作纵深防御保留。
            let dec = (free_cnt * (block_size / EXT4_INODE_BLOCK_SIZE as usize)) as u64;
            inode_blocks = inode_blocks.saturating_sub(dec);
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
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use ostd::prelude::*;

    use super::{BlockAllocator, InodeAllocCtx, RawSuperblock};
    use crate::fs::ext4::core::io::BlockReader;
    use crate::fs::ext4::core::metadata_writer::MetadataWriter;
    use crate::fs::ext4::core::test_util::EXT4_IMAGE;
    use crate::fs::ext4::core::types::Ext4Fsblk;
    // 全量带进 Pod(as_bytes/from_bytes) / vec! 等；Result 单独显式导入消歧。
    use crate::prelude::*;
    use crate::prelude::Result;

    /// 内存盘：读 / 元数据写打到同一份字节（balloc 只需读位图 + 写位图/SB/组描述符）。
    struct MemImage {
        bytes: RefCell<Vec<u8>>,
        block_size: usize,
    }
    impl MemImage {
        fn new(seed: &[u8]) -> Self {
            let sb = RawSuperblock::from_bytes(&seed[1024..2048]);
            MemImage {
                bytes: RefCell::new(seed.to_vec()),
                block_size: sb.block_size(),
            }
        }

        /// 读第 `bgid` 组的组描述符（与 [`BlockAllocator::load_group_desc`] 同几何）。
        fn read_group_desc(&self, sb: &RawSuperblock, bgid: u32) -> super::RawGroupDescriptor {
            use crate::fs::ext4::core::block_group::RawGroupDescriptor;
            let bs = self.block_size;
            let desc_size = sb.group_desc_size();
            let dsc_cnt = bs / desc_size;
            let dsc_id = bgid as usize / dsc_cnt;
            let first_data_block = sb.first_data_block() as usize;
            let block_id = first_data_block + dsc_id + 1;
            let offset_in_block = (bgid as usize % dsc_cnt) * desc_size;
            let off = block_id * bs + offset_in_block;
            let mut buf = [0u8; 64];
            self.read_at(off, &mut buf);
            RawGroupDescriptor::from_bytes(&buf)
        }

        /// 读绝对块号 `blk` 的整块字节。
        fn read_block(&self, blk: u64) -> Vec<u8> {
            let mut out = vec![0u8; self.block_size];
            self.read_at(blk as usize * self.block_size, out.as_mut_slice());
            out
        }

        /// 直接写绝对块号 `blk` 的整块字节（测试用，绕过 handle）。
        fn poke_block(&self, blk: u64, data: &[u8]) {
            let base = blk as usize * self.block_size;
            let mut b = self.bytes.borrow_mut();
            for (i, byte) in data.iter().enumerate() {
                if let Some(slot) = b.get_mut(base + i) {
                    *slot = *byte;
                }
            }
        }
    }
    impl BlockReader for MemImage {
        fn read_at(&self, off: usize, out: &mut [u8]) {
            let b = self.bytes.borrow();
            for (i, slot) in out.iter_mut().enumerate() {
                *slot = b.get(off + i).copied().unwrap_or(0);
            }
        }
    }
    impl MetadataWriter for MemImage {
        fn write_metadata_for_handle(
            &self,
            _handle_id: u64,
            block: Ext4Fsblk,
            data: &[u8],
        ) -> Result<()> {
            let base = block as usize * self.block_size;
            let mut b = self.bytes.borrow_mut();
            for (i, byte) in data.iter().enumerate() {
                if let Some(slot) = b.get_mut(base + i) {
                    *slot = *byte;
                }
            }
            Ok(())
        }
    }

    /// BUG-14 修复：单次 `balloc_free_blocks` 释放的 512B 当量超过当前 i_blocks 时，
    /// i_blocks 必须 `saturating_sub` 夹到 0，而非无符号下溢 panic。
    #[ktest]
    fn balloc_free_i_blocks_saturates_no_underflow() {
        let disk = MemImage::new(EXT4_IMAGE);
        let sb = RawSuperblock::from_bytes(&EXT4_IMAGE[1024..2048]);
        let bs = sb.block_size();
        let per_block = (bs / 512) as u64; // 每块的 512B 当量。

        let mut alloc = BlockAllocator::new(sb, &disk, &disk);

        // i_blocks 只够 1 个块；却释放 4 个块（4*per_block 远超）→ 旧码 panic，新码夹零。
        let mut inode = InodeAllocCtx::new(per_block);
        let start: Ext4Fsblk = 5000;
        alloc.balloc_free_blocks(&mut inode, start, 4);

        assert_eq!(
            inode.i_blocks(),
            0,
            "i_blocks must saturate to 0 on over-free, never underflow/panic"
        );

        // 正常释放（释放量 ≤ i_blocks）数值精确：i_blocks=10*per_block，释放 3 块 → 7*per_block。
        let mut inode2 = InodeAllocCtx::new(10 * per_block);
        alloc.balloc_free_blocks(&mut inode2, 5100, 3);
        assert_eq!(
            inode2.i_blocks(),
            7 * per_block,
            "normal free must decrement exactly, no saturation artifact"
        );
    }

    /// BUG-4 修复：`balloc_free_blocks` 用 `get_bgid_of_block`/`addr_to_idx_bg` 定位组（减
    /// first_data_block），对 `first_data_block == 1`（1K 块多组镜像）正确。
    ///
    /// 取 `block == blocks_per_group`（= 8192）：first_data_block=1 下属 **组 0**
    /// （`(8192-1)/8192 == 0`，组内下标 8191）；旧裸除法 `8192/8192 == 1` 会误判为组 1。
    /// 先在组 0 块位图把该块标为已分配，free 后断言：组 0 位图该位被清、组 0 free_blocks
    /// 计数 +1；组 1 完全不受影响——证明组定位减了 first_data_block。
    #[ktest]
    fn balloc_free_uses_first_data_block_geometry() {
        use crate::fs::ext4::core::bitmap::{ext4_bmap_bit_set, ext4_bmap_is_bit_clr};
        use crate::fs::ext4::core::test_util::EXT4_MULTIGROUP_IMAGE;

        let disk = MemImage::new(EXT4_MULTIGROUP_IMAGE);
        let sb = RawSuperblock::from_bytes(&EXT4_MULTIGROUP_IMAGE[1024..2048]);
        assert_eq!(sb.first_data_block(), 1, "fixture must have first_data_block=1");
        let bpg = sb.blocks_per_group();
        assert_eq!(bpg, 8192);

        let block_to_free: Ext4Fsblk = bpg as u64; // 8192 → 组 0、组内下标 8191（正确几何）。
        let idx_in_g0: u32 = bpg - 1; // (8192-1) % 8192

        // 组 0 / 组 1 的初始描述符 + 块位图块号。
        let g0 = disk.read_group_desc(&sb, 0);
        let g1 = disk.read_group_desc(&sb, 1);
        let g0_bmp_blk = g0.block_bitmap();
        let g1_bmp_blk = g1.block_bitmap();
        let g0_free_before = g0.get_free_blocks_count();
        let g1_free_before = g1.get_free_blocks_count();

        // 在组 0 块位图把 idx_in_g0 置为已分配（这样 free 才有可观察的「清位」效果）。
        let mut g0_bmp = disk.read_block(g0_bmp_blk);
        ext4_bmap_bit_set(&mut g0_bmp, idx_in_g0);
        disk.poke_block(g0_bmp_blk, &g0_bmp);
        // 快照组 1 位图（应保持不变）。
        let g1_bmp_before = disk.read_block(g1_bmp_blk);

        let mut alloc = BlockAllocator::new(sb, &disk, &disk);
        let mut inode = InodeAllocCtx::new(1_000_000); // 足够大，避免 i_blocks 干扰。
        alloc.balloc_free_blocks(&mut inode, block_to_free, 1);

        // 组 0：该位被清。
        let g0_bmp_after = disk.read_block(g0_bmp_blk);
        assert!(
            ext4_bmap_is_bit_clr(&g0_bmp_after, idx_in_g0),
            "freed bit must clear in group 0 bitmap (BUG-4 geometry)"
        );
        // 组 0 free_blocks 计数 +1。
        let g0_after = disk.read_group_desc(&sb, 0);
        assert_eq!(
            g0_after.get_free_blocks_count(),
            g0_free_before + 1,
            "group 0 free_blocks must increment (BUG-4 geometry)"
        );

        // 组 1：位图与 free 计数都不受影响（旧裸除法会误动组 1）。
        let g1_bmp_after = disk.read_block(g1_bmp_blk);
        assert_eq!(
            g1_bmp_after, g1_bmp_before,
            "group 1 bitmap must be untouched (BUG-4: raw divide would wrongly hit group 1)"
        );
        let g1_after = disk.read_group_desc(&sb, 1);
        assert_eq!(
            g1_after.get_free_blocks_count(),
            g1_free_before,
            "group 1 free_blocks must be unchanged (BUG-4 geometry)"
        );
    }
}
