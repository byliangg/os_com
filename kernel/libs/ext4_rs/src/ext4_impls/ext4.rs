use crate::prelude::*;
use crate::return_errno_with_message;
use crate::utils::*;

use crate::ext4_defs::*;
use crate::ext4_impls::LocalOperationAllocGuard;

impl Ext4 {
    #[inline]
    pub fn write_metadata(&self, offset: usize, data: &[u8]) {
        self.metadata_writer.write_metadata(offset, data);
    }

    /// 获取system zone缓存
    pub fn get_system_zone(&self) -> Vec<SystemZone> {
        let mut zones = Vec::new();
        let group_count = self.super_block.block_group_count();
        let inodes_per_group = self.super_block.inodes_per_group();
        let inode_size = self.super_block.inode_size() as u64;
        let block_size = self.super_block.block_size() as u64;
        for bgid in 0..group_count {
            // meta blocks
            let meta_blks = self.num_base_meta_blocks(bgid);
            if meta_blks != 0 {
                let start = self.get_block_of_bgid(bgid);
                zones.push(SystemZone {
                    group: bgid,
                    start_blk: start,
                    end_blk: start + meta_blks as u64 - 1,
                });
            }
            // block group描述符
            let block_group = Ext4BlockGroup::load_new(&self.block_device, &self.super_block, bgid as usize);
            // block bitmap
            let blk_bmp = block_group.get_block_bitmap_block(&self.super_block);
            zones.push(SystemZone {
                group: bgid,
                start_blk: blk_bmp,
                end_blk: blk_bmp,
            });
            // inode bitmap
            let ino_bmp = block_group.get_inode_bitmap_block(&self.super_block);
            zones.push(SystemZone {
                group: bgid,
                start_blk: ino_bmp,
                end_blk: ino_bmp,
            });
            // inode table
            let ino_tbl = block_group.get_inode_table_blk_num() as u64;
            let itb_per_group = ((inodes_per_group as u64 * inode_size + block_size - 1) / block_size) as u64;
            zones.push(SystemZone {
                group: bgid,
                start_blk: ino_tbl,
                end_blk: ino_tbl + itb_per_group - 1,
            });
        }
        zones
    }

    fn load_inode_table_blocks(&self) -> Vec<Ext4Fsblk> {
        let group_count = self.super_block.block_group_count();
        let mut inode_table_blocks = Vec::with_capacity(group_count as usize);
        for bgid in 0..group_count {
            let block_group =
                Ext4BlockGroup::load_new(&self.block_device, &self.super_block, bgid as usize);
            inode_table_blocks.push(block_group.get_inode_table_blk_num() as Ext4Fsblk);
        }
        inode_table_blocks
    }
    /// Opens and loads an Ext4 from the `block_device`.
    pub fn open(block_device: Arc<dyn BlockDevice>) -> Self {
        // Load the superblock
        let block = Block::load(&block_device, SUPERBLOCK_OFFSET, BLOCK_SIZE);
        let super_block: Ext4Superblock = block.read_as();

        // drop(block);
        
        let ext4_tmp = Ext4 {
            metadata_writer: Arc::new(PassthroughMetadataWriter::new(block_device.clone())),
            alloc_guard: Arc::new(LocalOperationAllocGuard::new()),
            block_device,
            super_block,
            system_zone_cache: None,
            inode_table_blocks: Vec::new(),
        };
        let zones = ext4_tmp.get_system_zone();
        let inode_table_blocks = ext4_tmp.load_inode_table_blocks();

        Ext4 {
            system_zone_cache: Some(zones),
            inode_table_blocks,
            ..ext4_tmp
        }
    }

    // with dir result search path offset
    pub fn generic_open(
        &self,
        path: &str,
        parent_inode_num: &mut u32,
        create: bool,
        ftype: u16,
        name_off: &mut u32,
    ) -> Result<u32> {
        let mut is_goal = false;

        let mut parent = parent_inode_num;

        let mut search_path = path;

        let mut dir_search_result = Ext4DirSearchResult::new(Ext4DirEntry::default());

        loop {
            while search_path.starts_with('/') {
                *name_off += 1; // Skip the slash
                search_path = &search_path[1..];
            }

            let len = path_check(search_path, &mut is_goal);

            let current_path = &search_path[..len];

            if len == 0 || search_path.is_empty() {
                break;
            }

            search_path = &search_path[len..];

            let r = self.dir_find_entry(*parent, current_path, &mut dir_search_result);

            // log::trace!("find in parent {:x?} r {:?} name {:?}", parent, r, current_path);
            if let Err(e) = r {
                if e.error() != Errno::ENOENT || !create {
                    return_errno_with_message!(Errno::ENOENT, "No such file or directory");
                }

                let mut inode_mode = 0;
                if is_goal {
                    inode_mode = ftype;
                } else {
                    inode_mode = InodeFileType::S_IFDIR.bits();
                }

                let new_inode_ref = self.create(*parent, current_path, inode_mode)?;

                // Update parent to the new inode
                *parent = new_inode_ref.inode_num;

                // Now, update dir_search_result to reflect the new inode
                dir_search_result.dentry.inode = new_inode_ref.inode_num;

                continue;
            }

            if is_goal {
                break;
            } else {
                // update parent
                *parent = dir_search_result.dentry.inode;
            }
            *name_off += len as u32;
        }

        if is_goal {
            return Ok(dir_search_result.dentry.inode);
        }

        Ok(dir_search_result.dentry.inode)
    }

    #[allow(unused)]
    pub fn dir_mk(&self, path: &str) -> Result<usize> {
        let mut nameoff = 0;

        let filetype = InodeFileType::S_IFDIR;

        // todo get this path's parent

        // start from root
        let mut parent = ROOT_INODE;

        let r = self.generic_open(path, &mut parent, true, filetype.bits(), &mut nameoff);
        Ok(EOK)
    }

    pub fn unlink(
        &self,
        parent: &mut Ext4InodeRef,
        child: &mut Ext4InodeRef,
        name: &str,
    ) -> Result<usize> {
        self.dir_remove_entry(parent, name)?;

        let is_dir = child.inode.is_dir();
        let mut free_child = false;

        if is_dir {
            let parent_links = parent.inode.links_count();
            if parent_links > 0 {
                parent.inode.set_links_count(parent_links - 1);
            }
            // rmdir removes both "." and ".." references.
            child.inode.set_links_count(0);
            // Keep directory inode allocated when nlink drops to zero.
            // Similar to regular-file unlink, immediate bitmap recycle can
            // reuse inode numbers while other references still exist
            // (e.g. concurrent cwd/path walkers), causing lookup corruption.
            // We do not yet have orphan-cleanup on last ref, so defer recycle.
            self.write_back_inode(child);
            self.write_back_inode(parent);
        } else {
            let child_links = child.inode.links_count();
            if child_links > 1 {
                child.inode.set_links_count(child_links - 1);
                self.write_back_inode(child);
            } else {
                child.inode.set_links_count(0);
                // Keep regular-file inode allocated when nlink drops to zero.
                // POSIX requires unlink to keep an opened-but-unlinked inode alive
                // until the last file reference is closed. We currently do not have
                // close-time orphan cleanup, so avoid immediate inode bitmap recycle
                // to prevent inode reuse corruption in open-unlink races.
                self.write_back_inode(child);
            }
        }

        if free_child {
            // Persist zero nlink before releasing inode bitmap entry.
            self.write_back_inode(child);
            self.ialloc_free_inode(child.inode_num, is_dir);
        }

        Ok(EOK)
    }
}
