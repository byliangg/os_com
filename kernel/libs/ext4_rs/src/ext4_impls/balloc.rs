use crate::ext4_defs::*;
use crate::prelude::*;
use crate::return_errno_with_message;
use crate::utils::bitmap::*;

use super::{
    is_operation_allocated_block, reserve_operation_allocated_block,
    reserve_operation_allocated_blocks,
};

impl Ext4 {
    fn verify_bitmap_bits_visible(
        &self,
        bmp_blk_adr: Ext4Fsblk,
        block_size: usize,
        bgid: u32,
        rel_idxs: &[u32],
        allocs: &[Ext4Fsblk],
    ) {
        if rel_idxs.is_empty() {
            return;
        }

        let bitmap_readback = self
            .block_device
            .read_offset(bmp_blk_adr as usize * block_size);
        let mismatched: Vec<(u32, Ext4Fsblk)> = rel_idxs
            .iter()
            .copied()
            .zip(allocs.iter().copied())
            .filter(|(rel_idx, _)| ext4_bmap_is_bit_clr(&bitmap_readback, *rel_idx))
            .collect();
        if !mismatched.is_empty() {
            log::error!(
                "[Block Alloc] bitmap visibility mismatch: bgid={} bmp_blk={} rel_idxs={:?} allocs={:?}",
                bgid,
                bmp_blk_adr,
                mismatched.iter().map(|(rel_idx, _)| *rel_idx).collect::<Vec<_>>(),
                mismatched.iter().map(|(_, alloc)| *alloc).collect::<Vec<_>>()
            );
        }
    }

    fn extent_maps_pblock(extent: Ext4Extent, target: Ext4Fsblk) -> bool {
        let start = extent.get_pblock();
        let len = extent.get_actual_len() as u64;
        target >= start && target - start < len
    }

    fn extent_node_maps_block(
        &self,
        node_pblock: Ext4Fsblk,
        expected_depth: u16,
        target: Ext4Fsblk,
    ) -> Result<bool> {
        let block_size = self.super_block.block_size() as usize;
        let node_block = Block::load(&self.block_device, node_pblock as usize * block_size);
        let header = Ext4ExtentHeader::load_from_u8(&node_block.data[..EXT4_EXTENT_HEADER_SIZE]);
        if header.magic != EXT4_EXTENT_MAGIC || header.depth != expected_depth {
            return Err(Ext4Error::new(Errno::EIO));
        }

        if header.depth == 0 {
            let capacity = node_block
                .data
                .len()
                .saturating_sub(EXT4_EXTENT_HEADER_SIZE)
                / EXT4_EXTENT_SIZE;
            if header.entries_count as usize > capacity {
                return Err(Ext4Error::new(Errno::EIO));
            }
            for pos in 0..header.entries_count as usize {
                let off = EXT4_EXTENT_HEADER_SIZE + pos * EXT4_EXTENT_SIZE;
                let extent = Ext4Extent::load_from_u8(&node_block.data[off..off + EXT4_EXTENT_SIZE]);
                if Self::extent_maps_pblock(extent, target) {
                    return Ok(true);
                }
            }
            return Ok(false);
        }

        let capacity = node_block
            .data
            .len()
            .saturating_sub(EXT4_EXTENT_HEADER_SIZE)
            / EXT4_EXTENT_INDEX_SIZE;
        if header.entries_count as usize > capacity {
            return Err(Ext4Error::new(Errno::EIO));
        }
        for pos in 0..header.entries_count as usize {
            let off = EXT4_EXTENT_HEADER_SIZE + pos * EXT4_EXTENT_INDEX_SIZE;
            let index = Ext4ExtentIndex::load_from_u8(
                &node_block.data[off..off + EXT4_EXTENT_INDEX_SIZE],
            );
            if self.extent_node_maps_block(index.get_pblock(), header.depth - 1, target)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn inode_extent_tree_maps_block(
        &self,
        inode_ref: &Ext4InodeRef,
        target: Ext4Fsblk,
    ) -> Result<bool> {
        let header = inode_ref.inode.root_extent_header();
        if header.magic != EXT4_EXTENT_MAGIC {
            return Err(Ext4Error::new(Errno::EIO));
        }

        let root_capacity = (inode_ref.inode.block.len().saturating_sub(3)) / 3;
        if header.entries_count as usize > root_capacity {
            return Err(Ext4Error::new(Errno::EIO));
        }

        if header.depth == 0 {
            for pos in 0..header.entries_count as usize {
                let off = 3 + pos * 3;
                let extent = Ext4Extent::load_from_u32(&inode_ref.inode.block[off..]);
                if Self::extent_maps_pblock(extent, target) {
                    return Ok(true);
                }
            }
            return Ok(false);
        }

        for pos in 0..header.entries_count as usize {
            let off = 3 + pos * 3;
            let index = Ext4ExtentIndex::load_from_u32(&inode_ref.inode.block[off..]);
            if self.extent_node_maps_block(index.get_pblock(), header.depth - 1, target)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn inode_already_maps_block_slow(&self, inode_ref: &Ext4InodeRef, target: Ext4Fsblk) -> bool {
        let block_size = self.super_block.block_size() as u64;
        if block_size == 0 {
            return false;
        }
        let file_blocks = inode_ref.inode.size().div_ceil(block_size);
        let Ok(file_blocks) = u32::try_from(file_blocks) else {
            return false;
        };
        for lblock in 0..file_blocks {
            if let Ok(mapped) = self.get_pblock_idx(inode_ref, lblock) {
                if mapped == target {
                    return true;
                }
            }
        }
        false
    }

    fn inode_already_maps_block(&self, inode_ref: &Ext4InodeRef, target: Ext4Fsblk) -> bool {
        if (inode_ref.inode.flags() & EXT4_INODE_FLAG_EXTENTS as u32) != 0 {
            match self.inode_extent_tree_maps_block(inode_ref, target) {
                Ok(mapped) => return mapped,
                Err(err) => {
                    log::warn!(
                        "[Block Alloc] fast mapped-block check failed: inode={} target={} err={:?}; falling back to logical scan",
                        inode_ref.inode_num,
                        target,
                        err
                    );
                }
            }
        }

        self.inode_already_maps_block_slow(inode_ref, target)
    }

    /// Return the first candidate bit index in a block group that is not known
    /// to belong to ext4 metadata/system-reserved regions.
    fn first_non_reserved_idx_in_group(&self, bgid: u32) -> u32 {
        let mut idx = self.addr_to_idx_bg(self.get_block_of_bgid(bgid));

        if let Some(zones) = &self.system_zone_cache {
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

    /// Compute number of block group from block address.
    ///
    /// Params:
    ///
    /// `baddr` - Absolute address of block.
    ///
    /// # Returns
    /// `u32` - Block group index
    pub fn get_bgid_of_block(&self, baddr: u64) -> u32 {
        let mut baddr = baddr;
        if self.super_block.first_data_block() != 0 && baddr != 0 {
            baddr -= 1;
        }
        (baddr / self.super_block.blocks_per_group() as u64) as u32
    }

    /// Compute the starting block address of a block group.
    ///
    /// Params:
    /// `bgid` - Block group index
    ///
    /// Returns:
    /// `u64` - Block address
    pub fn get_block_of_bgid(&self, bgid: u32) -> u64 {
        let mut baddr = 0;
        if self.super_block.first_data_block() != 0 {
            baddr += 1;
        }
        baddr + bgid as u64 * self.super_block.blocks_per_group() as u64
    }

    /// Convert block address to relative index in block group.
    ///
    /// Params:
    /// `baddr` - Block number to convert.
    ///
    /// Returns:
    /// `u32` - Relative number of block.
    pub fn addr_to_idx_bg(&self, baddr: u64) -> u32 {
        let mut baddr = baddr;
        if self.super_block.first_data_block() != 0 && baddr != 0 {
            baddr -= 1;
        }
        (baddr % self.super_block.blocks_per_group() as u64) as u32
    }

    /// Convert relative block address in group to absolute address.
    ///
    /// # Arguments
    ///
    /// * `index` - Relative block address.
    /// * `bgid` - Block group.
    ///
    /// # Returns
    ///
    /// * `Ext4Fsblk` - Absolute block address.
    pub fn bg_idx_to_addr(&self, index: u32, bgid: u32) -> Ext4Fsblk {
        let mut index = index;
        if self.super_block.first_data_block() != 0 {
            index += 1;
        }
        (self.super_block.blocks_per_group() as u64 * bgid as u64) + index as u64
    }


    /// Allocate a new block.
    ///
    /// Params:
    /// `inode_ref` - Reference to the inode.
    /// `goal` - Absolute address of the block.
    ///
    /// Returns:
    /// `Result<Ext4Fsblk>` - The physical block number allocated.
    pub fn balloc_alloc_block(
        &self,
        inode_ref: &mut Ext4InodeRef,
        goal: Option<Ext4Fsblk>,
    ) -> Result<Ext4Fsblk> {
        let block_size = self.super_block.block_size() as usize;
        let mut alloc: Ext4Fsblk = 0;
        let super_block = &self.super_block;
        let blocks_per_group = super_block.blocks_per_group();
        let mut bgid;
        let mut idx_in_bg;

        if let Some(goal) = goal {
            bgid = self.get_bgid_of_block(goal);
            idx_in_bg = self.addr_to_idx_bg(goal);
        } else {
            bgid = 1;
            idx_in_bg = 0;
        }

        let block_group_count = super_block.block_group_count();
        let mut count = block_group_count;

        while count > 0 {
            // Load block group reference
            let mut block_group =
                Ext4BlockGroup::load_new(&self.block_device, super_block, bgid as usize);

            let free_blocks = block_group.get_free_blocks_count();
            if free_blocks == 0 {
                // Try next block group
                bgid = (bgid + 1) % block_group_count;
                count -= 1;

                if count == 0 {
                    log::trace!("No free blocks available in all block groups");
                    return_errno_with_message!(Errno::ENOSPC, "No free blocks available in all block groups");
                }
                continue;
            }

            // Compute indexes
            let first_in_bg = self.get_block_of_bgid(bgid);
            let first_in_bg_index = self.addr_to_idx_bg(first_in_bg);
            let first_data_idx = self.first_non_reserved_idx_in_group(bgid);
            idx_in_bg = idx_in_bg.max(first_in_bg_index).max(first_data_idx);

            // Load block with bitmap
            let bmp_blk_adr = block_group.get_block_bitmap_block(super_block);
            let mut bitmap_block =
                Block::load(&self.block_device, bmp_blk_adr as usize * block_size);

            // Check if goal is free
            if ext4_bmap_is_bit_clr(&bitmap_block.data, idx_in_bg) {
                let block_num = self.bg_idx_to_addr(idx_in_bg, bgid);
                if self.is_system_reserved_block(block_num, bgid)
                    || is_operation_allocated_block(block_num)
                    || self.inode_already_maps_block(inode_ref, block_num)
                {
                    // 跳过 system zone
                } else {
                    ext4_bmap_bit_set(&mut bitmap_block.data, idx_in_bg);
                    block_group.set_block_group_balloc_bitmap_csum(super_block, &bitmap_block.data);
                    self.write_metadata(bmp_blk_adr as usize * block_size, &bitmap_block.data);
                    alloc = self.bg_idx_to_addr(idx_in_bg, bgid);
                    self.verify_bitmap_bits_visible(
                        bmp_blk_adr,
                        block_size,
                        bgid,
                        &[idx_in_bg],
                        &[alloc],
                    );

                    /* Update free block counts */
                    self.update_free_block_counts(inode_ref, &mut block_group, bgid as usize)?;
                    reserve_operation_allocated_block(alloc);
                    return Ok(alloc);
                }
            }

            // Try to find free block near to goal
            let blk_in_bg = blocks_per_group;
            let end_idx = min((idx_in_bg + 63) & !63, blk_in_bg);

            for tmp_idx in (idx_in_bg + 1)..end_idx {
                if ext4_bmap_is_bit_clr(&bitmap_block.data, tmp_idx) {
                    // Check if this is a system reserved block
                    let block_num = self.bg_idx_to_addr(tmp_idx, bgid);
                    if self.is_system_reserved_block(block_num, bgid)
                        || is_operation_allocated_block(block_num)
                    {
                        continue;
                    }
                    
                    ext4_bmap_bit_set(&mut bitmap_block.data, tmp_idx);
                    block_group.set_block_group_balloc_bitmap_csum(super_block, &bitmap_block.data);
                    self.write_metadata(bmp_blk_adr as usize * block_size, &bitmap_block.data);
                    alloc = self.bg_idx_to_addr(tmp_idx, bgid);
                    self.verify_bitmap_bits_visible(
                        bmp_blk_adr,
                        block_size,
                        bgid,
                        &[tmp_idx],
                        &[alloc],
                    );
                    self.update_free_block_counts(inode_ref, &mut block_group, bgid as usize)?;
                    reserve_operation_allocated_block(alloc);
                    return Ok(alloc);
                }
            }

            // Find free bit in bitmap
            let mut rel_blk_idx = 0;
            if ext4_bmap_bit_find_clr(&bitmap_block.data, idx_in_bg, blk_in_bg, &mut rel_blk_idx) {
                // Check if this is a system reserved block
                let block_num = self.bg_idx_to_addr(rel_blk_idx, bgid);
                if !self.is_system_reserved_block(block_num, bgid)
                    && !is_operation_allocated_block(block_num)
                {
                    ext4_bmap_bit_set(&mut bitmap_block.data, rel_blk_idx);
                    block_group.set_block_group_balloc_bitmap_csum(super_block, &bitmap_block.data);
                    self.write_metadata(bmp_blk_adr as usize * block_size, &bitmap_block.data);
                    alloc = self.bg_idx_to_addr(rel_blk_idx, bgid);
                    self.verify_bitmap_bits_visible(
                        bmp_blk_adr,
                        block_size,
                        bgid,
                        &[rel_blk_idx],
                        &[alloc],
                    );
                    self.update_free_block_counts(inode_ref, &mut block_group, bgid as usize)?;
                    reserve_operation_allocated_block(alloc);
                    return Ok(alloc);
                }
            }

            // No free block found in this group, try other block groups
            bgid = (bgid + 1) % block_group_count;
            count -= 1;
        }

        return_errno_with_message!(Errno::ENOSPC, "No free blocks available in all block groups");
    }

    /// Allocate a new block start from a specific bgid
    ///
    /// Params:
    /// `inode_ref` - Reference to the inode.
    /// `start_bgid` - Start bgid of free block search
    ///
    /// Returns:
    /// `Result<Ext4Fsblk>` - The physical block number allocated.
    pub fn balloc_alloc_block_from(
        &self,
        inode_ref: &mut Ext4InodeRef,
        start_bgid: &mut u32,
    ) -> Result<Ext4Fsblk> {
        let block_size = self.super_block.block_size() as usize;
        let mut alloc: Ext4Fsblk = 0;
        let super_block = &self.super_block;
        let blocks_per_group = super_block.blocks_per_group();
        // Maximum number of blocks that can be represented by a bitmap block
        let max_blocks_in_bitmap = block_size * 8;

        let mut bgid = *start_bgid;
        let mut idx_in_bg = 0;

        let block_group_count = super_block.block_group_count();
        let mut count = block_group_count;

        while count > 0 {
            // Load block group reference
            let mut block_group =
                Ext4BlockGroup::load_new(&self.block_device, super_block, bgid as usize);

            let free_blocks = block_group.get_free_blocks_count();
            if free_blocks == 0 {
                // Try next block group
                bgid = (bgid + 1) % block_group_count;
                count -= 1;

                if count == 0 {
                    log::trace!("No free blocks available in all block groups");
                    return_errno_with_message!(Errno::ENOSPC, "No free blocks available in all block groups");
                }
                continue;
            }

            // Compute indexes
            let first_in_bg = self.get_block_of_bgid(bgid);
            let first_in_bg_index = self.addr_to_idx_bg(first_in_bg);
            let first_data_idx = self.first_non_reserved_idx_in_group(bgid);
            idx_in_bg = idx_in_bg.max(first_in_bg_index).max(first_data_idx);

            // Ensure idx_in_bg doesn't exceed bitmap size
            if idx_in_bg >= max_blocks_in_bitmap as u32 {
                // Try next block group if we've reached the end of this bitmap
                bgid = (bgid + 1) % block_group_count;
                count -= 1;
                idx_in_bg = 0;
                continue;
            }

            // Load block with bitmap
            let bmp_blk_adr = block_group.get_block_bitmap_block(super_block);
            let mut bitmap_block =
                Block::load(&self.block_device, bmp_blk_adr as usize * block_size);

            // Check if goal is free
            if ext4_bmap_is_bit_clr(&bitmap_block.data, idx_in_bg) {
                let block_num = self.bg_idx_to_addr(idx_in_bg, bgid);
                if is_operation_allocated_block(block_num) {
                    idx_in_bg = idx_in_bg.saturating_add(1);
                    continue;
                }
                ext4_bmap_bit_set(&mut bitmap_block.data, idx_in_bg);
                block_group.set_block_group_balloc_bitmap_csum(super_block, &bitmap_block.data);
                self.write_metadata(bmp_blk_adr as usize * block_size, &bitmap_block.data);
                alloc = block_num;
                self.verify_bitmap_bits_visible(
                    bmp_blk_adr,
                    block_size,
                    bgid,
                    &[idx_in_bg],
                    &[alloc],
                );

                /* Update free block counts */
                self.update_free_block_counts(inode_ref, &mut block_group, bgid as usize)?;

                *start_bgid = bgid;
                reserve_operation_allocated_block(alloc);
                return Ok(alloc);
            }

            // Try to find free block near to goal
            let end_idx = min((idx_in_bg + 63) & !63, max_blocks_in_bitmap as u32);

            for tmp_idx in (idx_in_bg + 1)..end_idx {
                if ext4_bmap_is_bit_clr(&bitmap_block.data, tmp_idx) {
                    // Check if this is a system reserved block
                    let block_num = self.bg_idx_to_addr(tmp_idx, bgid);
                    if self.is_system_reserved_block(block_num, bgid)
                        || is_operation_allocated_block(block_num)
                    {
                        continue;
                    }
                    
                    ext4_bmap_bit_set(&mut bitmap_block.data, tmp_idx);
                    block_group.set_block_group_balloc_bitmap_csum(super_block, &bitmap_block.data);
                    self.write_metadata(bmp_blk_adr as usize * block_size, &bitmap_block.data);
                    alloc = self.bg_idx_to_addr(tmp_idx, bgid);
                    self.verify_bitmap_bits_visible(
                        bmp_blk_adr,
                        block_size,
                        bgid,
                        &[tmp_idx],
                        &[alloc],
                    );
                    self.update_free_block_counts(inode_ref, &mut block_group, bgid as usize)?;

                    *start_bgid = bgid;
                    reserve_operation_allocated_block(alloc);
                    return Ok(alloc);
                }
            }

            // Find free bit in bitmap
            let mut rel_blk_idx = 0;
            if ext4_bmap_bit_find_clr(&bitmap_block.data, idx_in_bg, max_blocks_in_bitmap as u32, &mut rel_blk_idx) {
                // Check if this is a system reserved block
                let block_num = self.bg_idx_to_addr(rel_blk_idx, bgid);
                if !self.is_system_reserved_block(block_num, bgid)
                    && !is_operation_allocated_block(block_num)
                {
                    ext4_bmap_bit_set(&mut bitmap_block.data, rel_blk_idx);
                    block_group.set_block_group_balloc_bitmap_csum(super_block, &bitmap_block.data);
                    self.write_metadata(bmp_blk_adr as usize * block_size, &bitmap_block.data);
                    alloc = self.bg_idx_to_addr(rel_blk_idx, bgid);
                    self.verify_bitmap_bits_visible(
                        bmp_blk_adr,
                        block_size,
                        bgid,
                        &[rel_blk_idx],
                        &[alloc],
                    );
                    self.update_free_block_counts(inode_ref, &mut block_group, bgid as usize)?;

                    *start_bgid = bgid;
                    reserve_operation_allocated_block(alloc);
                    return Ok(alloc);
                }
            }

            // No free block found in this group, try other block groups
            bgid = (bgid + 1) % block_group_count;
            count -= 1;
            idx_in_bg = 0;
        }

        return_errno_with_message!(Errno::ENOSPC, "No free blocks available in all block groups");
    }

    fn update_free_block_counts(
        &self,
        inode_ref: &mut Ext4InodeRef,
        block_group: &mut Ext4BlockGroup,
        bgid: usize,
    ) -> Result<()> {
        let block_size = self.super_block.block_size() as usize;
        let mut super_block = self.super_block;
        let block_size = block_size as u64;

        // Update superblock free blocks count
        let mut super_blk_free_blocks = super_block.free_blocks_count();
        super_blk_free_blocks -= 1;
        super_block.set_free_blocks_count(super_blk_free_blocks);
        super_block.sync_to_disk_with_csum(&self.metadata_writer);

        // Update inode blocks (different block size!) count
        let mut inode_blocks = inode_ref.inode.blocks_count();
        inode_blocks += block_size / EXT4_INODE_BLOCK_SIZE as u64;
        inode_ref.inode.set_blocks_count(inode_blocks);
        self.write_back_inode(inode_ref);

        // Update block group free blocks count
        let mut fb_cnt = block_group.get_free_blocks_count();
        fb_cnt -= 1;
        block_group.set_free_blocks_count(fb_cnt as u32);
        block_group.sync_to_disk_with_csum(&self.metadata_writer, bgid, &super_block);

        Ok(())
    }

    #[allow(unused)]
    pub fn balloc_free_blocks(&self, inode_ref: &mut Ext4InodeRef, start: Ext4Fsblk, count: u32) {
        let block_size = self.super_block.block_size() as usize;
        // log::trace!("balloc_free_blocks start {:x?} count {:x?}", start, count);
        let mut count = count as usize;
        let mut start = start;

        let mut super_block = self.super_block;
        let mut any_freed = false;
        let mut inode_blocks = inode_ref.inode.blocks_count();

        let blocks_per_group = super_block.blocks_per_group();
        let max_bits_per_bitmap = block_size * 8;
        let max_bits_per_group = core::cmp::min(blocks_per_group as usize, max_bits_per_bitmap);

        let mut bg_first = start / blocks_per_group as u64;
        let mut bg_last = (start + count as u64 - 1) / blocks_per_group as u64;

        while bg_first <= bg_last {
            let idx_in_bg = start % blocks_per_group as u64;
            let idx_in_bg = idx_in_bg as usize;
            if idx_in_bg >= max_bits_per_group {
                // Guard against malformed metadata leading to invalid bitmap offsets.
                // Skip to the next group to avoid out-of-bounds bitmap access.
                bg_first += 1;
                continue;
            }

            let current_bgid = bg_first as usize;

            let mut bg =
                Ext4BlockGroup::load_new(&self.block_device, &super_block, current_bgid);

            let block_bitmap_block = bg.get_block_bitmap_block(&super_block);
            let mut raw_data = self
                .block_device
                .read_offset(block_bitmap_block as usize * block_size);
            let mut data: &mut Vec<u8> = &mut raw_data;

            let mut free_cnt = max_bits_per_group - idx_in_bg;
            if count <= free_cnt {
                free_cnt = count;
            }
            if free_cnt == 0 {
                bg_first += 1;
                continue;
            }

            ext4_bmap_bits_free(data, idx_in_bg as u32, idx_in_bg as u32 + free_cnt as u32 - 1);

            count -= free_cnt;
            start += free_cnt as u64;

            bg.set_block_group_balloc_bitmap_csum(&super_block, data);
            self.write_metadata(block_bitmap_block as usize * block_size, data);

            /* Update free block counts in memory; flush shared inode/superblock once below. */
            let mut super_blk_free_blocks = super_block.free_blocks_count();
            super_blk_free_blocks += free_cnt as u64;
            super_block.set_free_blocks_count(super_blk_free_blocks);

            inode_blocks -= (free_cnt * (block_size / EXT4_INODE_BLOCK_SIZE)) as u64;
            any_freed = true;

            /* Update block group free blocks count */
            let mut fb_cnt = bg.get_free_blocks_count();
            fb_cnt += free_cnt as u64;
            bg.set_free_blocks_count(fb_cnt as u32);
            bg.sync_to_disk_with_csum(&self.metadata_writer, current_bgid, &super_block);

            bg_first += 1;
        }

        if any_freed {
            inode_ref.inode.set_blocks_count(inode_blocks);
            self.write_back_inode(inode_ref);
            super_block.sync_to_disk_with_csum(&self.metadata_writer);
        }
    }


    pub fn is_system_reserved_block(&self, block_num: u64, _bgid: u32) -> bool {

        // 如果缓存未初始化，则不判断
        if self.system_zone_cache.is_none() {
            return false;
        }
        // 查缓存
        if let Some(zones) = &self.system_zone_cache {
            for zone in zones {
                if block_num >= zone.start_blk && block_num <= zone.end_blk {
                    return true;
                }
            }
        }
        false
    }
    /// Optimized block allocation inspired by lwext4
    /// 
    /// Params:
    /// `inode_ref` - Reference to the inode
    /// `start_bgid` - Starting block group ID, will be updated to the last used block group
    /// `count` - Number of blocks to allocate
    /// 
    /// Returns:
    /// `Result<Vec<Ext4Fsblk>>` - Vector of allocated physical block numbers
    pub fn balloc_alloc_block_batch(
        &self,
        inode_ref: &mut Ext4InodeRef,
        start_bgid: &mut u32,
        count: usize,
    ) -> Result<Vec<Ext4Fsblk>> {
        let block_size = self.super_block.block_size() as usize;
        if count == 0 {
            return Ok(Vec::new());
        }
        
        log::debug!("[Block Alloc] Requesting {} blocks starting from bgid {}", count, *start_bgid);
        
        let super_block = &self.super_block;
        let block_group_count = super_block.block_group_count();
        
        // Validate inputs
        if block_group_count == 0 {
            log::error!("[Block Alloc] Invalid block group count: 0");
            return return_errno_with_message!(Errno::EINVAL, "Invalid block group count");
        }
        
        if *start_bgid >= block_group_count {
            log::warn!("[Block Alloc] Invalid start_bgid {}, resetting to 0", *start_bgid);
            *start_bgid = 0;
        }
        
        let mut bgid = *start_bgid;
        let mut result = Vec::with_capacity(count);
        let mut remaining = count;
        
        // Search through all block groups
        let mut groups_checked = 0;
        
        while remaining > 0 && groups_checked < block_group_count {
            // Load block group reference
            let mut block_group = 
                Ext4BlockGroup::load_new(&self.block_device, super_block, bgid as usize);
            
            // Check if this group has free blocks
            let free_blocks = block_group.get_free_blocks_count();
            if free_blocks == 0 {
                log::debug!("[Block Alloc] Block group {} has no free blocks", bgid);
                bgid = (bgid + 1) % block_group_count;
                groups_checked += 1;
                continue;
            }
            
            // Get block bitmap for this group
            let bmp_blk_adr = block_group.get_block_bitmap_block(super_block);
            let mut bitmap_data = 
                self.block_device.read_offset(bmp_blk_adr as usize * block_size);
            
            // Compute indexes and limits
            let first_in_bg = self.get_block_of_bgid(bgid);
            let first_in_bg_index = self.addr_to_idx_bg(first_in_bg);
            let idx_in_bg = first_in_bg_index.max(self.first_non_reserved_idx_in_group(bgid));
            let blocks_per_group = super_block.blocks_per_group();
            
            // Find free blocks in bitmap
            let mut found_blocks = 0;
            let max_to_find = core::cmp::min(remaining, free_blocks as usize);
            let mut rel_blk_idx = 0;
            let mut current_idx = idx_in_bg;
            let mut allocated_rel_idxs = Vec::new();
            
            // First try to find blocks in a simple loop starting from current_idx
            while found_blocks < max_to_find && current_idx < blocks_per_group {
                // Ensure we don't go beyond bitmap size (block_size * 8 bits)
                if current_idx >= block_size as u32 * 8 {
                    break;
                }
                
                if ext4_bmap_is_bit_clr(&bitmap_data, current_idx) {
                    // Check if this is a system reserved block
                    let block_num = self.bg_idx_to_addr(current_idx, bgid);
                    if self.is_system_reserved_block(block_num, bgid) {
                        log::trace!(
                            "[Block Alloc] Skip reserved block at {:#x} (bgid={})",
                            block_num,
                            bgid
                        );
                        current_idx += 1;
                        continue;
                    }
                    if is_operation_allocated_block(block_num) {
                        current_idx += 1;
                        continue;
                    }
                    if self.inode_already_maps_block(inode_ref, block_num) {
                        log::warn!(
                            "[Block Alloc] Skip inode-mapped block at {:#x} (inode={}, bgid={})",
                            block_num,
                            inode_ref.inode_num,
                            bgid
                        );
                        current_idx += 1;
                        continue;
                    }
                    
                    // Found a free block
                    ext4_bmap_bit_set(&mut bitmap_data, current_idx);
                    
                    // Calculate physical block address
                    let block_num = self.bg_idx_to_addr(current_idx, bgid);
                    
                    // Add to result
                    result.push(block_num);
                    allocated_rel_idxs.push(current_idx);
                    found_blocks += 1;
                    
                    // For debugging continuity issues
                    if result.len() > 1 {
                        let prev_block = result[result.len() - 2];
                        if block_num != prev_block + 1 {
                            log::debug!("[Block Alloc] Non-contiguous blocks: prev={}, current={}, diff={}",
                                prev_block, block_num, block_num - prev_block);
                        }
                    }
                }
                
                current_idx += 1;
            }
            
            // If we didn't find enough blocks using sequential search, use bitmap search function
            if found_blocks < max_to_find {
                let mut start_idx = current_idx;
                
                while found_blocks < max_to_find {
                    // Make sure we don't exceed the bitmap size
                    let end_idx = core::cmp::min(blocks_per_group, block_size as u32 * 8);
                    
                    // Find next clear bit
                    if !ext4_bmap_bit_find_clr(&bitmap_data, start_idx, end_idx, &mut rel_blk_idx) {
                        break; // No more free blocks in this group
                    }
                    
                    // Check if this is a system reserved block
                    let block_num = self.bg_idx_to_addr(rel_blk_idx, bgid);
                    if self.is_system_reserved_block(block_num, bgid) {
                        // Skip this block and continue search
                        log::trace!(
                            "[Block Alloc] Skip reserved block at {:#x} (bgid={})",
                            block_num,
                            bgid
                        );
                        start_idx = rel_blk_idx + 1;
                        continue;
                    }
                    if is_operation_allocated_block(block_num) {
                        start_idx = rel_blk_idx + 1;
                        continue;
                    }
                    if self.inode_already_maps_block(inode_ref, block_num) {
                        log::warn!(
                            "[Block Alloc] Skip inode-mapped block at {:#x} (inode={}, bgid={})",
                            block_num,
                            inode_ref.inode_num,
                            bgid
                        );
                        start_idx = rel_blk_idx + 1;
                        continue;
                    }
                    
                    ext4_bmap_bit_set(&mut bitmap_data, rel_blk_idx);
                    
                    // Calculate physical block address
                    let block_num = self.bg_idx_to_addr(rel_blk_idx, bgid);
                    
                    // Add to result
                    result.push(block_num);
                    allocated_rel_idxs.push(rel_blk_idx);
                    found_blocks += 1;
                    
                    // For debugging continuity issues
                    if result.len() > 1 {
                        let prev_block = result[result.len() - 2];
                        if block_num != prev_block + 1 {
                            log::debug!("[Block Alloc] Non-contiguous blocks: prev={}, current={}, diff={}",
                                prev_block, block_num, block_num - prev_block);
                        }
                    }
                }
            }
            
            // If we found any blocks, update metadata
            if found_blocks > 0 {
                // Update bitmap on disk
                block_group.set_block_group_balloc_bitmap_csum(super_block, &bitmap_data);
                self.write_metadata(bmp_blk_adr as usize * block_size, &bitmap_data);

                let verify_bitmap =
                    self.block_device.read_offset(bmp_blk_adr as usize * block_size);
                let mut missing_visible_bits = Vec::new();
                for &rel_idx in &allocated_rel_idxs {
                    if ext4_bmap_is_bit_clr(&verify_bitmap, rel_idx) {
                        missing_visible_bits.push(rel_idx);
                    }
                }
                if !missing_visible_bits.is_empty() {
                    let missing_blocks: Vec<_> = missing_visible_bits
                        .iter()
                        .map(|&rel_idx| self.bg_idx_to_addr(rel_idx, bgid))
                        .collect();
                    log::error!(
                        "[Block Alloc] bitmap visibility mismatch: bgid={} bitmap_block={} found_blocks={} rel_idxs={:?} abs_blocks={:?} visible_missing_rel_idxs={:?} visible_missing_abs={:?}",
                        bgid,
                        bmp_blk_adr,
                        found_blocks,
                        allocated_rel_idxs,
                        &result[result.len().saturating_sub(found_blocks)..],
                        missing_visible_bits,
                        missing_blocks
                    );
                }
                
                // Update block group free blocks count
                let new_free_count = free_blocks - found_blocks as u64;
                block_group.set_free_blocks_count(new_free_count as u32);
                block_group.sync_to_disk_with_csum(&self.metadata_writer, bgid as usize, super_block);
                
                // Update superblock free blocks count
                let mut sb_copy = *super_block;
                let sb_free_blocks = sb_copy.free_blocks_count();
                sb_copy.set_free_blocks_count(sb_free_blocks - found_blocks as u64);
                sb_copy.sync_to_disk_with_csum(&self.metadata_writer);
                
                // Update inode blocks count
                let blocks_per_fs_block = block_size as u64 / EXT4_INODE_BLOCK_SIZE as u64;
                let mut inode_blocks = inode_ref.inode.blocks_count();
                inode_blocks += found_blocks as u64 * blocks_per_fs_block;
                inode_ref.inode.set_blocks_count(inode_blocks);
                
                // Decrement remaining blocks to allocate
                remaining -= found_blocks;
                
                log::debug!("[Block Alloc] Allocated {} blocks from bg {}", found_blocks, bgid);
            }
            
            // Try next block group
            bgid = (bgid + 1) % block_group_count;
            groups_checked += 1;
        }
        
        // Log allocation results
        let allocated_count = result.len();
        log::debug!("[Block Alloc] Allocated {}/{} blocks", allocated_count, count);
        
        // Even if we couldn't allocate all requested blocks, return what we got
        if remaining > 0 {
            log::warn!("[Block Alloc] Could only allocate {} out of {} blocks. Remaining: {}", 
                allocated_count, count, remaining);
        }
        
        // Update start_bgid to continue from where we left off next time
        *start_bgid = bgid;
        
        // Write back inode to save block count changes
        if allocated_count > 0 {
            reserve_operation_allocated_blocks(&result);
            self.write_back_inode(inode_ref);
        }
        
        Ok(result)
    }

    /// Returns the number of meta blocks for a given block group, like Linux ext4_num_base_meta_blocks.
    pub fn num_base_meta_blocks(&self, bgid: u32) -> u32 {
        let has_super = self.ext4_bg_has_super(bgid);
        let gdt_blocks = self.ext4_bg_num_gdb(bgid);
        let meta_blocks = if has_super { 1 + gdt_blocks } else { 0 };
        // log::info!(
        //     "[num_base_meta_blocks] group={} has_super={} gdt_blocks={} meta_blocks={}",
        //     bgid, has_super, gdt_blocks, meta_blocks
        // );
        meta_blocks
    }

    /// 判断group是否有superblock备份（与Linux ext4_bg_has_super一致）
    pub fn ext4_bg_has_super(&self, group: u32) -> bool {
        if group == 0 {
            return true;
        }
        // Linux: group号为3/5/7的幂也有superblock备份
        fn is_power_of(mut n: u32, base: u32) -> bool {
            if n < base { return false; }
            while n % base == 0 { n /= base; }
            n == 1
        }
        is_power_of(group, 3) || is_power_of(group, 5) || is_power_of(group, 7)
    }

    /// 判断是否有meta_bg特性（与Linux ext4_has_feature_meta_bg一致）
    pub fn ext4_has_feature_meta_bg(&self) -> bool {
        // EXT4_FEATURE_INCOMPAT_META_BG = 0x0010
        const EXT4_FEATURE_INCOMPAT_META_BG: u32 = 0x0010;
        (self.super_block.incompat_features() & EXT4_FEATURE_INCOMPAT_META_BG) != 0
    }

    /// 返回该group的GDT blocks数（与Linux ext4_bg_num_gdb一致）
    pub fn ext4_bg_num_gdb(&self, group: u32) -> u32 {
        let sb = &self.super_block;
        let group_count = sb.block_group_count();
        let block_size = sb.block_size();
        let desc_size = sb.desc_size() as u32;
        let reserved_gdt_blocks = sb.reserved_gdt_blocks() as u32;
        let desc_blocks = ((group_count as u64 * desc_size as u64 + block_size as u64 - 1) / block_size as u64) as u32;

        if !self.ext4_bg_has_super(group) {
            return 0;
        }
        if group == 0 {
            return desc_blocks + reserved_gdt_blocks;
        }
        if self.ext4_has_feature_meta_bg() {
            1
        } else {
            desc_blocks + reserved_gdt_blocks
        }
    }
}
