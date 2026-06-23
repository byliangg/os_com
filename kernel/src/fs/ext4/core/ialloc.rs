// SPDX-License-Identifier: MPL-2.0
//! 安全 inode 分配器（ialloc）——逐位复刻 `ext4_rs/src/ext4_impls/ialloc.rs` 的线性扫描、
//! 计数/csum 更新顺序（含 Task 2 已复刻的两处计数 bug），全程零 unsafe、元数据写只经
//! [`MetadataWriter`]。
//!
//! 两入口：
//! - [`InodeAllocator::ialloc_alloc_inode`]（按 is_dir 分配一个 inode 号）—— 对照 ext4_rs
//!   `ialloc_alloc_inode` :7-83；
//! - [`InodeAllocator::ialloc_free_inode`]（释放给定 inode 号）—— 对照 `ialloc_free_inode` :85-122。
//!
//! 与 ext4_rs 的关键一致点（保证落盘字节逐字节可对拍）：
//! - **无 Orlov**：alloc 从 bgid 0 线性扫，取首个 `free_inodes > 0` 的组（ext4_rs 即如此）。
//! - inode 号 **1-based**：`bgid * inodes_per_group + (idx_in_bg + 1)`。
//! - **不写 inode 表**：alloc/free 只动 inode 位图块、组描述符、超级块三处元数据；inode 内容
//!   由上层写。三处都在 [`super::diff_harness::snapshot_meta`] 覆盖范围内（SB + GDT + 每组
//!   inode 位图块），故差分无盲区。
//! - is_dir 走 `set_used_dirs_count`，**命中 Task 2 复刻的 bug**（误写 `itable_unused`）——
//!   parity-first 原样保留。
//! - `itable_unused` 仅当 `idx >= inodes_in_bg - unused` 时才更新为 `inodes_in_bg - (idx+1)`。
//!
//! 持一份**可变** [`RawSuperblock`]（free-inodes 计数的运行期权威），每轮组迭代从盘上
//! **重新读**组描述符（对照 ext4_rs `Ext4BlockGroup::load_new`）。落盘三处的 RMW 拼块策略
//! 与 [`super::balloc`] 一致（见该模块文档）。
//!
//! [对照来源] kernel/libs/ext4_rs/src/ext4_impls/ialloc.rs

use super::bitmap::{ext4_bmap_bit_clr, ext4_bmap_bit_find_clr, ext4_bmap_bit_set};
use super::block_group::{GroupGeometry, RawGroupDescriptor};
use super::io::BlockReader;
use super::metadata_writer::MetadataWriter;
use super::prelude::*;
use super::superblock::RawSuperblock;

/// 安全 inode 分配器。持一份可变超级块（free-inodes 计数权威）+ 读接缝 + 元数据写回。
/// **不复制 ext4_rs 的 `Ext4` god-object**；锁（`lock_block_group` / `lock_superblock_counter`）
/// 语义在集成层接入（Phase 5），本结构单线程语义等价。
pub(super) struct InodeAllocator<'a, R: BlockReader, W: MetadataWriter> {
    /// 运行期可变超级块（free-inodes 随 alloc 递减 / free 递增；其余字段同盘上初值）。
    sb: RawSuperblock,
    /// 读盘接缝（每轮组迭代重读组描述符 / inode 位图）。
    reader: &'a R,
    /// 元数据写回接缝（inode 位图 / 组描述符 / 超级块）。
    writer: &'a W,
    /// 写元数据用的 JBD2 handle id 占位（本 Phase 直写，handle 语义 Phase 5 接入）。
    handle_id: u64,
}

impl<'a, R: BlockReader, W: MetadataWriter> InodeAllocator<'a, R, W> {
    /// 用初始超级块字节 + 读/写接缝构造。
    pub(super) fn new(sb: RawSuperblock, reader: &'a R, writer: &'a W) -> Self {
        Self {
            sb,
            reader,
            writer,
            handle_id: 0,
        }
    }

    /// 当前（运行期）超级块快照——差分用例跑完后据此 `snapshot_meta`。
    pub(super) fn superblock(&self) -> &RawSuperblock {
        &self.sb
    }

    // ------------------------------------------------------------------
    // 内部 helper（块号 ↔ 偏移、读组描述符 / 位图、落盘）。
    // 与 balloc 同款 RMW 拼块策略；inode 侧位图块号取 desc.inode_bitmap()。
    // ------------------------------------------------------------------

    fn block_size(&self) -> usize {
        self.sb.block_size()
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
        let mut buf = [0u8; 64];
        self.reader.read_at(off, &mut buf);
        RawGroupDescriptor::from_bytes(&buf)
    }

    /// 读第 `bgid` 组的 inode 位图整块（block_size 字节）。
    fn load_inode_bitmap(&self, desc: &RawGroupDescriptor) -> Vec<u8> {
        let bs = self.block_size();
        let bmp_blk = desc.inode_bitmap();
        let mut data = vec![0u8; bs];
        self.reader.read_at(bmp_blk as usize * bs, data.as_mut_slice());
        data
    }

    /// 把 inode 位图整块写回（块号粒度，整块）——等价 ext4_rs `write_metadata(bmp*bs, full_block)`。
    fn write_inode_bitmap(&self, bmp_blk: Ext4Fsblk, data: &[u8]) -> Result<()> {
        self.writer
            .write_metadata_for_handle(self.handle_id, bmp_blk, data)
    }

    /// 把组描述符的 64 字节写回 GDT 块内偏移（读整块 → 拼接 → 写整块，保证字节级一致）。
    /// 与 [`super::balloc::BlockAllocator::write_group_desc`] 同策略。
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
        let bytes = desc.as_bytes();
        block[offset_in_block..offset_in_block + bytes.len()].copy_from_slice(bytes);
        self.writer
            .write_metadata_for_handle(self.handle_id, block_id as u64, &block)
    }

    /// 把运行期超级块写回（1024 字节，写在块 0 内偏移 1024；先重算 csum）。
    /// 与 [`super::balloc::BlockAllocator::write_superblock`] 同策略。
    fn write_superblock(&mut self) -> Result<()> {
        let bs = self.block_size();
        self.sb.recompute_csum();
        let sb_bytes = self.sb.as_bytes();
        let sb_off = 1024usize;
        let block_id = sb_off / bs;
        let offset_in_block = sb_off % bs;
        let mut block = vec![0u8; bs];
        self.reader.read_at(block_id * bs, block.as_mut_slice());
        block[offset_in_block..offset_in_block + sb_bytes.len()].copy_from_slice(sb_bytes);
        self.writer
            .write_metadata_for_handle(self.handle_id, block_id as u64, &block)
    }

    /// 该组的 inode 数：非末组为 `inodes_per_group`，末组为余数。
    /// [对照] ext4_rs `Ext4Superblock::get_inodes_in_group_cnt`（super_block.rs:206-216）。
    fn inodes_in_group_cnt(&self, bgid: u32) -> u32 {
        let block_group_count = self.sb.block_group_count();
        let inodes_per_group = self.sb.inodes_per_group();
        let total_inodes = self.sb.inodes_count();
        if bgid < block_group_count - 1 {
            inodes_per_group
        } else {
            total_inodes - ((block_group_count - 1) * inodes_per_group)
        }
    }

    // ------------------------------------------------------------------
    // 入口 1：ialloc_alloc_inode(is_dir)
    // [对照] ext4_rs ialloc.rs:7-83
    // ------------------------------------------------------------------

    /// 分配一个 inode：从 bgid 0 线性扫到首个有空闲 inode 的组，置位 + 更新计数/csum，
    /// 返回 **1-based** inode 号。无 Orlov。逐位复刻 ext4_rs `ialloc_alloc_inode`。
    pub(super) fn ialloc_alloc_inode(&mut self, is_dir: bool) -> Result<u32> {
        let mut bgid = 0u32;
        let bg_count = self.sb.block_group_count();

        // ext4_rs 循环结构：`while bgid <= bg_count`，bgid==bg_count 时复位为 0 续扫
        // （若所有组都满则空转——latent bug，parity-first 原样复刻；真盘恒有空闲 inode）。
        while bgid <= bg_count {
            if bgid == bg_count {
                bgid = 0;
                continue;
            }

            // ext4_rs 此处 lock_block_group(bgid)；core 单线程，锁语义 Phase 5 接入。
            let mut desc = self.load_group_desc(bgid);

            let mut free_inodes = desc.get_free_inodes_count();

            if free_inodes > 0 {
                let inode_bitmap_block = desc.inode_bitmap();
                let mut bitmap = self.load_inode_bitmap(&desc);

                let inodes_in_bg = self.inodes_in_group_cnt(bgid);

                // 从 bit 0 找首个清零位；ext4_rs 忽略返回值，命中位写进 idx_in_bg。
                let mut idx_in_bg = 0u32;
                ext4_bmap_bit_find_clr(&bitmap, 0, inodes_in_bg, &mut idx_in_bg);
                ext4_bmap_bit_set(&mut bitmap, idx_in_bg);

                // 先写位图，再 set 位图 csum（与 ext4_rs 顺序一致）。
                self.write_inode_bitmap(inode_bitmap_block, &bitmap)
                    .expect("write inode bitmap");
                desc.set_inode_bitmap_csum(&self.sb, &bitmap);

                // 组 free_inodes -= 1。
                free_inodes -= 1;
                desc.set_free_inodes_count(&self.sb, free_inodes);

                // is_dir：used_dirs += 1（命中 set_used_dirs_count 写 itable_unused 的 bug）。
                if is_dir {
                    let used_dirs = desc.get_used_dirs_count(&self.sb) + 1;
                    desc.set_used_dirs_count(&self.sb, used_dirs);
                }

                // itable_unused 条件更新：仅当 idx >= inodes_in_bg - unused 时设为 inodes_in_bg-(idx+1)。
                let mut unused = desc.get_itable_unused(&self.sb);
                let free = inodes_in_bg - unused;
                if idx_in_bg >= free {
                    unused = inodes_in_bg - (idx_in_bg + 1);
                    desc.set_itable_unused(&self.sb, unused);
                }

                // 超级块 free_inodes -= 1 + csum + 写（对照 decrease_superblock_free_inodes）。
                let sb_free = self.sb.free_inodes_count();
                self.sb.set_free_inodes_count(sb_free - 1);
                self.write_superblock().expect("write superblock");

                // 组描述符 csum（用更新后 SB）+ 写。
                desc.set_checksum(bgid, &self.sb);
                self.write_group_desc(bgid, &desc)
                    .expect("write group desc");

                // 绝对 inode 号（1-based）。
                let inodes_per_group = self.sb.inodes_per_group();
                let inode_num = bgid * inodes_per_group + (idx_in_bg + 1);
                return Ok(inode_num);
            }

            bgid += 1;
        }

        Err(Error::with_message(Errno::ENOSPC, "alloc inode fail"))
    }

    // ------------------------------------------------------------------
    // 入口 2：ialloc_free_inode(index, is_dir)
    // [对照] ext4_rs ialloc.rs:85-122
    // ------------------------------------------------------------------

    /// 释放 inode `index`：定位组 + 组内下标，清位 + 计数回退 + csum。逐位复刻 ext4_rs。
    pub(super) fn ialloc_free_inode(&mut self, index: u32, is_dir: bool) {
        let geom = GroupGeometry::new(&self.sb);
        let bgid = geom.get_bgid_of_inode(index);
        // ext4_rs 此处 lock_block_group(bgid)；core 单线程，锁语义 Phase 5 接入。
        let mut desc = self.load_group_desc(bgid);

        let inode_bitmap_block = desc.inode_bitmap();
        let mut bitmap = self.load_inode_bitmap(&desc);

        // 组内下标 + 清位。
        let geom = GroupGeometry::new(&self.sb);
        let index_in_group = geom.inode_to_bgidx(index);
        ext4_bmap_bit_clr(&mut bitmap, index_in_group);

        // 先写位图，再 set 位图 csum（与 ext4_rs 顺序一致）。
        self.write_inode_bitmap(inode_bitmap_block, &bitmap)
            .expect("write inode bitmap");
        desc.set_inode_bitmap_csum(&self.sb, &bitmap);

        // 组 free_inodes += 1。
        let free_inodes = desc.get_free_inodes_count() + 1;
        desc.set_free_inodes_count(&self.sb, free_inodes);

        // is_dir：used_dirs -= 1（同样命中写 itable_unused 的 bug）。
        if is_dir {
            let used_dirs = desc.get_used_dirs_count(&self.sb) - 1;
            desc.set_used_dirs_count(&self.sb, used_dirs);
        }

        // 超级块 free_inodes += 1 + csum + 写（对照 increase_superblock_free_inodes）。
        let sb_free = self.sb.free_inodes_count();
        self.sb.set_free_inodes_count(sb_free + 1);
        self.write_superblock().expect("write superblock");

        // 组描述符 csum（用更新后 SB）+ 写。
        desc.set_checksum(bgid, &self.sb);
        self.write_group_desc(bgid, &desc)
            .expect("write group desc");
    }
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::InodeAllocator;
    use crate::fs::ext4::core::balloc::{BlockAllocator, InodeAllocCtx};
    use crate::fs::ext4::core::diff_harness::{
        assert_meta_eq, snapshot_meta, DirectMetadataWriter, MemDisk,
    };
    use crate::fs::ext4::core::io::BlockReader;
    use crate::fs::ext4::core::superblock::RawSuperblock;
    use crate::fs::ext4::core::test_util::EXT4_IMAGE;
    use crate::prelude::*;

    /// 构造空 extent / 不映射任何块的旧侧 inode_ref（同 balloc 差分用法）：`Ext4Inode::default()`
    /// → flags=0、size=0 → maps_block 恒 false；blocks=0 → i_blocks 从 0 起。其 write_back_inode
    /// 写的 inode 表槽落在 snapshot_meta 覆盖范围之外，不影响对拍。
    fn make_old_inode_ref(inode_num: u32) -> ext4_rs::Ext4InodeRef {
        ext4_rs::Ext4InodeRef {
            inode_num,
            inode: ext4_rs::Ext4Inode::default(),
        }
    }

    /// 读镜像超级块（盘内偏移 1024、长 1024）。
    fn read_sb(disk: &MemDisk) -> RawSuperblock {
        let mut buf = vec![0u8; 1024];
        disk.read_at(1024, buf.as_mut_slice());
        RawSuperblock::from_bytes(&buf)
    }

    /// ialloc 差分公共体：两侧跑相同 `is_dir` 序列，逐步比返回 inode 号；
    /// 序列跑完比 snapshot_meta（含每组 inode 位图块 + 组描述符 free_inodes/used_dirs/
    /// itable_unused + 超级块 free_inodes）逐字节。ialloc **不写 inode 表**，故 snapshot 无盲区。
    /// 旧侧直接 `ext4_rs::Ext4::ialloc_alloc_inode(is_dir)`（无需 inode_ref）。
    fn diff_ialloc_seq(seq: &[bool]) {
        let disk_old = MemDisk::from_image(EXT4_IMAGE);
        let disk_new = MemDisk::from_image(EXT4_IMAGE);

        let old = ext4_rs::Ext4::open(Arc::new(disk_old.clone()));

        let sb = read_sb(&disk_new);
        let bs = sb.block_size();
        let writer = DirectMetadataWriter::new(disk_new.clone(), bs);
        let mut alloc = InodeAllocator::new(sb, &disk_new, &writer);

        for (i, &is_dir) in seq.iter().enumerate() {
            let old_ret = old.ialloc_alloc_inode(is_dir);
            let new_ret = alloc.ialloc_alloc_inode(is_dir);
            match (&old_ret, &new_ret) {
                (Ok(a), Ok(b)) => {
                    assert_eq!(a, b, "ialloc step {i} inode-num mismatch is_dir={is_dir}")
                }
                (Err(_), Err(_)) => {}
                _ => panic!(
                    "ialloc step {i} ok/err mismatch is_dir={is_dir}: old={old_ret:?} new={new_ret:?}"
                ),
            }
        }

        let sb_old = read_sb(&disk_old);
        assert_meta_eq(
            &snapshot_meta(&disk_old, &sb_old),
            &snapshot_meta(&disk_new, alloc.superblock()),
        );
    }

    /// 连续分配（全 file）：验证 inode 号 1-based 推进 + 位图/计数逐步一致。
    #[ktest]
    fn ialloc_diff_alloc_consecutive_files() {
        let seq = [false, false, false, false, false, false, false, false];
        diff_ialloc_seq(&seq);
    }

    /// 连续分配（全 dir）：验证 is_dir 路径的 used_dirs（命中写 itable_unused 的 bug）一致。
    #[ktest]
    fn ialloc_diff_alloc_consecutive_dirs() {
        let seq = [true, true, true, true];
        diff_ialloc_seq(&seq);
    }

    /// 混合 dir/file 序列：验证 is_dir true/false 交错时计数差异逐步一致。
    #[ktest]
    fn ialloc_diff_alloc_mixed_dir_file() {
        let seq = [true, false, true, true, false, false, true, false];
        diff_ialloc_seq(&seq);
    }

    /// free 后重分配差分：两侧各分配 5 个 inode → 释放第 2 个分到的 → 再分配一个，
    /// 比每步 inode 号 + 最终盘面。验证 free 把位真正清回、可被后续 alloc 复用。
    /// 释放的 inode 当作目录释放（is_dir=true），命中 used_dirs 回退路径。
    #[ktest]
    fn ialloc_diff_free_then_realloc() {
        let disk_old = MemDisk::from_image(EXT4_IMAGE);
        let disk_new = MemDisk::from_image(EXT4_IMAGE);

        let old = ext4_rs::Ext4::open(Arc::new(disk_old.clone()));

        let sb = read_sb(&disk_new);
        let bs = sb.block_size();
        let writer = DirectMetadataWriter::new(disk_new.clone(), bs);
        let mut alloc = InodeAllocator::new(sb, &disk_new, &writer);

        // 分配 5 个（都当目录，使 used_dirs 有累加），记录第 2 个 inode 号。
        let mut to_free = 0u32;
        for i in 0..5 {
            let o = old.ialloc_alloc_inode(true).expect("old ialloc");
            let n = alloc.ialloc_alloc_inode(true).expect("new ialloc");
            assert_eq!(o, n, "realloc-setup step {i} mismatch");
            if i == 1 {
                to_free = o;
            }
        }

        // 释放第 2 个分到的 inode（按目录释放）。
        old.ialloc_free_inode(to_free, true);
        alloc.ialloc_free_inode(to_free, true);

        // 再分配一个：两侧应返回同一 inode 号（极可能就是刚释放的 to_free）。
        let o = old.ialloc_alloc_inode(true).expect("old realloc");
        let n = alloc.ialloc_alloc_inode(true).expect("new realloc");
        assert_eq!(o, n, "ialloc realloc inode-num mismatch");

        let sb_old = read_sb(&disk_old);
        assert_meta_eq(
            &snapshot_meta(&disk_old, &sb_old),
            &snapshot_meta(&disk_new, alloc.superblock()),
        );
    }

    // ============================== 混合序列差分 ==============================

    /// 混合序列差分：alloc inode（dir）→ balloc 4 块 → free 这 4 块 → free inode，
    /// 比每步返回值 + 最终盘面（位图/GDT/SB 逐字节）。
    ///
    /// **新侧 SB 一致性**：ext4_rs 的 ialloc/balloc/free 共享**同一份** in-memory 超级块
    /// （`lock_superblock_counter`），而 core 的 `BlockAllocator` / `InodeAllocator` 各持一份
    /// SB 副本、写盘时整块覆盖。为忠实镜像旧侧单一 SB，新侧**每步操作前都从当前盘面重读 SB
    /// 重建对应 allocator**——这样新侧那份 SB 永远与盘上最新一致（每个 allocator 又都从盘
    /// 重读组描述符），与旧侧单一共享 SB 字节级等价。
    #[ktest]
    fn ialloc_balloc_diff_mixed_sequence() {
        let disk_old = MemDisk::from_image(EXT4_IMAGE);
        let disk_new = MemDisk::from_image(EXT4_IMAGE);

        let old = ext4_rs::Ext4::open(Arc::new(disk_old.clone()));
        let mut old_inode = make_old_inode_ref(11);

        let bs = read_sb(&disk_new).block_size();
        let writer = DirectMetadataWriter::new(disk_new.clone(), bs);
        // 新侧 inode 上下文跨步保留（i_blocks 累加/累减需连续）。
        let mut new_inode = InodeAllocCtx::new(0);

        // 步 1：alloc inode（dir）。
        let old_ino = old.ialloc_alloc_inode(true).expect("old ialloc");
        let new_ino = {
            let mut ia = InodeAllocator::new(read_sb(&disk_new), &disk_new, &writer);
            ia.ialloc_alloc_inode(true).expect("new ialloc")
        };
        assert_eq!(old_ino, new_ino, "mixed: ialloc inode-num mismatch");
        {
            let sb_old = read_sb(&disk_old);
            assert_meta_eq(
                &snapshot_meta(&disk_old, &sb_old),
                &snapshot_meta(&disk_new, &read_sb(&disk_new)),
            );
        }

        // 步 2：balloc 4 块（无 goal、同序列），记录首块。
        let mut first_block = 0u64;
        {
            let mut ba = BlockAllocator::new(read_sb(&disk_new), &disk_new, &writer);
            for i in 0..4 {
                let o = old.balloc_alloc_block(&mut old_inode, None).expect("old balloc");
                let n = ba.balloc_alloc_block(&mut new_inode, None).expect("new balloc");
                assert_eq!(o, n, "mixed: balloc step {i} block mismatch");
                if i == 0 {
                    first_block = o;
                }
            }
        }
        {
            let sb_old = read_sb(&disk_old);
            assert_meta_eq(
                &snapshot_meta(&disk_old, &sb_old),
                &snapshot_meta(&disk_new, &read_sb(&disk_new)),
            );
        }

        // 步 3：free 这 4 块。
        {
            let mut ba = BlockAllocator::new(read_sb(&disk_new), &disk_new, &writer);
            old.balloc_free_blocks(&mut old_inode, first_block, 4);
            ba.balloc_free_blocks(&mut new_inode, first_block, 4);
        }
        {
            let sb_old = read_sb(&disk_old);
            assert_meta_eq(
                &snapshot_meta(&disk_old, &sb_old),
                &snapshot_meta(&disk_new, &read_sb(&disk_new)),
            );
        }

        // 步 4：free inode（dir）。
        {
            let mut ia = InodeAllocator::new(read_sb(&disk_new), &disk_new, &writer);
            old.ialloc_free_inode(old_ino, true);
            ia.ialloc_free_inode(new_ino, true);
        }

        // 最终盘面逐字节一致 + i_blocks 一致（balloc 4 块再全 free → 回到 0）。
        let sb_old = read_sb(&disk_old);
        assert_meta_eq(
            &snapshot_meta(&disk_old, &sb_old),
            &snapshot_meta(&disk_new, &read_sb(&disk_new)),
        );
        assert_eq!(
            old_inode.inode.blocks_count(),
            new_inode.i_blocks(),
            "mixed: final i_blocks mismatch"
        );
    }
}
