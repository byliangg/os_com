use crate::ext4_defs::*;
use crate::prelude::*;
use crate::return_errno_with_message;
use crate::utils::bitmap::*;

// Superblock free-inode counters are not consumed on the in-kernel fast path.
// Avoid per-op superblock writeback in hot create/unlink loops.
const HOTPATH_SYNC_SUPERBLOCK: bool = false;

impl Ext4 {
    pub fn ialloc_alloc_inode(&self, is_dir: bool, preferred_bgid: Option<u32>) -> Result<u32> {
        let block_size = self.super_block.block_size() as usize;
        let bg_count = self.super_block.block_group_count();
        if bg_count == 0 {
            return_errno_with_message!(Errno::EINVAL, "invalid block group count");
        }
        let mut bgid = preferred_bgid.unwrap_or(0) % bg_count;
        let mut scanned = 0u32;
        let mut super_block = self.super_block;

        while scanned < bg_count {
            let mut bg =
                Ext4BlockGroup::load_new(&self.block_device, &super_block, bgid as usize);

            let mut free_inodes = bg.get_free_inodes_count();

            if free_inodes > 0 {
                let inode_bitmap_block = bg.get_inode_bitmap_block(&super_block);

                let mut raw_data = self
                    .block_device
                    .read_offset(inode_bitmap_block as usize * block_size);

                let inodes_in_bg = super_block.get_inodes_in_group_cnt(bgid);

                let mut bitmap_data = &mut raw_data[..];

                // Hot path hint: inode allocation in our workload is mostly append-like,
                // so start from the used-prefix tail instead of scanning from 0 each time.
                let mut idx_in_bg = inodes_in_bg.saturating_sub(free_inodes as u32);
                let found = ext4_bmap_bit_find_clr(
                    bitmap_data,
                    idx_in_bg,
                    inodes_in_bg,
                    &mut idx_in_bg,
                ) || ext4_bmap_bit_find_clr(bitmap_data, 0, inodes_in_bg, &mut idx_in_bg);
                if !found {
                    bgid = (bgid + 1) % bg_count;
                    scanned += 1;
                    continue;
                }
                ext4_bmap_bit_set(bitmap_data, idx_in_bg);

                // update bitmap in disk
                self.block_device
                    .write_offset(inode_bitmap_block as usize * block_size, bitmap_data);

                bg.set_block_group_ialloc_bitmap_csum(&super_block, bitmap_data);

                /* Modify filesystem counters */
                free_inodes -= 1;
                bg.set_free_inodes_count(&super_block, free_inodes);

                /* Increment used directories counter */
                if is_dir {
                    let used_dirs = bg.get_used_dirs_count(&super_block) + 1;
                    bg.set_used_dirs_count(&super_block, used_dirs);
                }

                /* Decrease unused inodes count */
                let mut unused = bg.get_itable_unused(&super_block);
                let free = inodes_in_bg - unused;
                if idx_in_bg >= free {
                    unused = inodes_in_bg - (idx_in_bg + 1);
                    bg.set_itable_unused(&super_block, unused);
                }

                bg.sync_to_disk_with_csum(&self.block_device, bgid as usize, &super_block);

                /* Update superblock */
                super_block.decrease_free_inodes_count();
                if HOTPATH_SYNC_SUPERBLOCK {
                    super_block.sync_to_disk_with_csum(&self.block_device);
                }

                /* Compute the absolute i-nodex number */
                let inodes_per_group = super_block.inodes_per_group();
                let inode_num = bgid * inodes_per_group + (idx_in_bg + 1);

                return Ok(inode_num);
            }

            bgid = (bgid + 1) % bg_count;
            scanned += 1;
        }

        return_errno_with_message!(Errno::ENOSPC, "alloc inode fail");
    }

    pub fn ialloc_free_inode(&self, index: u32, is_dir: bool) {
        let block_size = self.super_block.block_size() as usize;
        // Compute index of block group
        let bgid = self.get_bgid_of_inode(index);

        let mut super_block = self.super_block;
        let mut bg =
            Ext4BlockGroup::load_new(&self.block_device, &super_block, bgid as usize);

        // Load inode bitmap block
        let inode_bitmap_block = bg.get_inode_bitmap_block(&self.super_block);
        let mut bitmap_data = self
            .block_device
            .read_offset(inode_bitmap_block as usize * block_size);

        // Find index within group and clear bit
        let index_in_group = self.inode_to_bgidx(index);
        ext4_bmap_bit_clr(&mut bitmap_data, index_in_group);

        // Set new checksum after modification
        // update bitmap in disk
        self.block_device
            .write_offset(inode_bitmap_block as usize * block_size, &bitmap_data);
        bg.set_block_group_ialloc_bitmap_csum(&super_block, &bitmap_data);

        // Update free inodes count in block group
        let free_inodes = bg.get_free_inodes_count() + 1;
        bg.set_free_inodes_count(&self.super_block, free_inodes);

        // If inode was a directory, decrement the used directories count
        if is_dir {
            let used_dirs = bg.get_used_dirs_count(&self.super_block) - 1;
            bg.set_used_dirs_count(&self.super_block, used_dirs);
        }

        bg.sync_to_disk_with_csum(&self.block_device, bgid as usize, &super_block);

        super_block.increase_free_inodes_count();
        if HOTPATH_SYNC_SUPERBLOCK {
            super_block.sync_to_disk_with_csum(&self.block_device);
        }
    }
}
