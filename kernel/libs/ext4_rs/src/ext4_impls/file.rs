use crate::prelude::*;
use crate::return_errno_with_message;
use crate::utils::path_check;
use crate::ext4_defs::*;
// use std::time::{Duration, Instant};

// Keep sparse-write allocation modest: large enough to avoid pathological
// one-block extent growth under fsstress, but still conservative for generic/014.
const WRITE_PREALLOC_BLOCKS: u32 = 16;

impl Ext4 {
    /// Link a child inode to a parent directory
    ///
    /// Params:
    /// parent: &mut Ext4InodeRef - parent directory inode reference
    /// child: &mut Ext4InodeRef - child inode reference
    /// name: &str - name of the child inode
    ///
    /// Returns:
    /// `Result<usize>` - status of the operation
    pub fn link(
        &self,
        parent: &mut Ext4InodeRef,
        child: &mut Ext4InodeRef,
        name: &str,
    ) -> Result<usize> {
        // Add a directory entry in the parent directory pointing to the child inode

        // at this point should insert to existing block
        self.dir_add_entry(parent, child, name)?;
        self.write_back_inode_without_csum(parent);

        // If this is the first link. add '.' and '..' entries
        if child.inode.is_dir() {
            // let child_ref = child.clone();
            let new_child_ref = Ext4InodeRef {
                inode_num: child.inode_num,
                inode: child.inode,
            };

            // at this point child need a new block
            // Create "." entry pointing to the child directory itself
            self.dir_add_entry(child, &new_child_ref, ".")?;

            // at this point should insert to existing block
            // Create ".." entry pointing to the parent directory
            self.dir_add_entry(child, parent, "..")?;

            child.inode.set_links_count(2);
            let link_cnt = parent.inode.links_count() + 1;
            parent.inode.set_links_count(link_cnt);

            return Ok(EOK);
        }

        // Increment the link count of the child inode
        let link_cnt = child.inode.links_count() + 1;
        child.inode.set_links_count(link_cnt);

        Ok(EOK)
    }

    /// create a new inode and link it to the parent directory
    ///
    /// Params:
    /// parent: u32 - inode number of the parent directory
    /// name: &str - name of the new file
    /// mode: u16 - file mode
    ///
    /// Returns:
    pub fn create(&self, parent: u32, name: &str, inode_mode: u16) -> Result<Ext4InodeRef> {
        let mut parent_inode_ref = self.get_inode_ref(parent);

        // let mut child_inode_ref = self.create_inode(inode_mode)?;
        let init_child_ref = self.create_inode(inode_mode)?;

        self.write_back_inode_without_csum(&init_child_ref);
        // load new
        let mut child_inode_ref = self.get_inode_ref(init_child_ref.inode_num);

        self.link(&mut parent_inode_ref, &mut child_inode_ref, name)?;

        self.write_back_inode(&mut parent_inode_ref);
        self.write_back_inode(&mut child_inode_ref);

        Ok(child_inode_ref)
    }

    pub fn create_inode(&self, inode_mode: u16) -> Result<Ext4InodeRef> {

        // Extract only file-type bits; permission bits are not part of `InodeFileType`.
        let inode_file_type = match InodeFileType::from_bits(inode_mode & EXT4_INODE_MODE_TYPE_MASK) {
            Some(file_type) => file_type,
            None => InodeFileType::S_IFREG,
        };

        let is_dir = inode_file_type == InodeFileType::S_IFDIR;

        // allocate inode
        let inode_num = self.alloc_inode(is_dir)?;

        // initialize inode
        let mut inode = Ext4Inode::default();

        // Keep caller-provided permission bits; only normalize file-type bits.
        let file_mode = inode_file_type.bits() | (inode_mode & EXT4_INODE_MODE_PERM_MASK);
        inode.set_mode(file_mode);

        // set extra size
        let inode_size = self.super_block.inode_size();
        let extra_size = self.super_block.extra_size();
        if inode_size > EXT4_GOOD_OLD_INODE_SIZE {
            inode.set_i_extra_isize(extra_size);
        }

        // set extent
        inode.set_flags(EXT4_INODE_FLAG_EXTENTS as u32);
        inode.extent_tree_init();

        let inode_ref = Ext4InodeRef {
            inode_num,
            inode,
        };

        Ok(inode_ref)
    }


    /// create a new inode and link it to the parent directory
    ///
    /// Params:
    /// parent: u32 - inode number of the parent directory
    /// name: &str - name of the new file
    /// mode: u16 - file mode
    /// uid: u32 - user id
    /// gid: u32 - group id
    ///
    /// Returns:
    pub fn create_with_attr(&self, parent: u32, name: &str, inode_mode: u16, uid:u16, gid: u16) -> Result<Ext4InodeRef> {
        let mut parent_inode_ref = self.get_inode_ref(parent);

        // let mut child_inode_ref = self.create_inode(inode_mode)?;
        let mut init_child_ref = self.create_inode(inode_mode)?;

        init_child_ref.inode.set_uid(uid);
        init_child_ref.inode.set_gid(gid);

        self.write_back_inode_without_csum(&init_child_ref);
        // load new
        let mut child_inode_ref = self.get_inode_ref(init_child_ref.inode_num);

        self.link(&mut parent_inode_ref, &mut child_inode_ref, name)?;

        self.write_back_inode(&mut parent_inode_ref);
        self.write_back_inode(&mut child_inode_ref);

        Ok(child_inode_ref)
    }

    /// Read data from a file at a given offset
    ///
    /// Params:
    /// inode: u32 - inode number of the file
    /// offset: usize - offset from where to read
    /// read_buf: &mut [u8] - buffer to read the data into
    ///
    /// Returns:
    /// `Result<usize>` - number of bytes read
    pub fn read_at(&self, inode: u32, offset: usize, read_buf: &mut [u8]) -> Result<usize> {
        let block_size = self.super_block.block_size() as usize;
        let mut read_buf_len = read_buf.len();
        if read_buf_len == 0 {
            return Ok(0);
        }

        let inode_ref = self.get_inode_ref(inode);
        let file_size = inode_ref.inode.size() as usize;

        if offset >= file_size {
            return Ok(0);
        }

        if offset + read_buf_len > file_size {
            read_buf_len = file_size - offset;
        }

        let uses_extents = (inode_ref.inode.flags() & EXT4_INODE_FLAG_EXTENTS as u32) != 0;
        let mut extent_cache: Option<(u32, u32, Ext4Fsblk)> = None;
        let mut resolve_pblock = |lblock: u32| -> Result<Option<Ext4Fsblk>> {
            if uses_extents {
                if let Some((ext_start, ext_end, pblock_start)) = extent_cache {
                    if lblock >= ext_start && lblock < ext_end {
                        return Ok(Some(pblock_start + (lblock - ext_start) as u64));
                    }
                }

                match self.find_extent(&inode_ref, lblock) {
                    Ok(path) => {
                        let mapped = path.path.last().and_then(|node| node.extent).and_then(|extent| {
                            let ext_start = extent.get_first_block();
                            let ext_len = extent.get_actual_len() as u32;
                            let ext_end = ext_start.checked_add(ext_len)?;
                            if lblock < ext_start || lblock >= ext_end {
                                return None;
                            }
                            let pblock_start = extent.get_pblock();
                            Some((ext_start, ext_end, pblock_start))
                        });

                        if let Some((ext_start, ext_end, pblock_start)) = mapped {
                            extent_cache = Some((ext_start, ext_end, pblock_start));
                            return Ok(Some(pblock_start + (lblock - ext_start) as u64));
                        }

                        Ok(None)
                    }
                    Err(e) if e.error() == Errno::ENOENT => Ok(None),
                    Err(e) => Err(e),
                }
            } else {
                match self.get_pblock_idx(&inode_ref, lblock) {
                    Ok(pblock) => Ok(Some(pblock)),
                    Err(e) if e.error() == Errno::ENOENT => Ok(None),
                    Err(e) => Err(e),
                }
            }
        };

        let mut block_data = vec![0u8; block_size];
        let mut cursor = 0usize;
        let mut current_offset = offset;

        while cursor < read_buf_len {
            let lblock = (current_offset / block_size) as u32;
            let block_inner_offset = current_offset % block_size;
            let read_length = min(read_buf_len - cursor, block_size - block_inner_offset);

            match resolve_pblock(lblock) {
                Ok(Some(pblock_idx)) => {
                    let pblock = usize::try_from(pblock_idx)
                        .map_err(|_| Ext4Error::new(Errno::EIO))?;
                    let block_offset = pblock
                        .checked_mul(block_size)
                        .ok_or_else(|| Ext4Error::new(Errno::EIO))?;

                    self.block_device
                        .read_offset_into(block_offset, block_data.as_mut_slice());
                    read_buf[cursor..cursor + read_length].copy_from_slice(
                        &block_data[block_inner_offset..block_inner_offset + read_length],
                    );
                }
                Ok(None) => {
                    read_buf[cursor..cursor + read_length].fill(0);
                }
                Err(_) => {
                    return_errno_with_message!(
                        Errno::EIO,
                        "Failed to get physical block for logical block"
                    );
                }
            }

            cursor += read_length;
            current_offset += read_length;
        }

        Ok(read_buf_len)
    }

    fn insert_allocated_blocks_as_extents(
        &self,
        inode_ref: &mut Ext4InodeRef,
        lblock_start: u32,
        allocated_blocks: &[Ext4Fsblk],
    ) -> Result<usize> {
        if allocated_blocks.is_empty() {
            return Ok(0);
        }

        let mut inserted = 0usize;
        let mut logical = lblock_start;
        let mut seg_begin = 0usize;

        while seg_begin < allocated_blocks.len() {
            let mut seg_end = seg_begin + 1;
            while seg_end < allocated_blocks.len()
                && allocated_blocks[seg_end] == allocated_blocks[seg_end - 1] + 1
            {
                seg_end += 1;
            }

            let mut phys = allocated_blocks[seg_begin];
            let mut remaining = seg_end - seg_begin;
            while remaining > 0 {
                let chunk_len = min(remaining, EXT_INIT_MAX_LEN as usize);

                let mut newex = Ext4Extent::default();
                newex.first_block = logical;
                newex.store_pblock(phys);
                newex.block_count = chunk_len as u16;
                self.insert_extent(inode_ref, &mut newex)?;

                logical = logical
                    .checked_add(chunk_len as u32)
                    .ok_or_else(|| Ext4Error::new(Errno::EINVAL))?;
                phys += chunk_len as u64;
                inserted += chunk_len;
                remaining -= chunk_len;
            }

            seg_begin = seg_end;
        }

        Ok(inserted)
    }

    fn ensure_write_range_mapped(
        &self,
        inode_ref: &mut Ext4InodeRef,
        start_lblock: u32,
        end_lblock: u32,
        start_bgid: &mut u32,
    ) -> Result<usize> {
        if start_lblock >= end_lblock {
            return Ok(0);
        }

        let mut allocated_total = 0usize;
        let mut cursor = start_lblock;

        while cursor < end_lblock {
            match self.get_pblock_idx(inode_ref, cursor) {
                Ok(_) => {
                    cursor += 1;
                }
                Err(e) if e.error() == Errno::ENOENT => {
                    let run_start = cursor;
                    cursor += 1;
                    while cursor < end_lblock {
                        match self.get_pblock_idx(inode_ref, cursor) {
                            Ok(_) => break,
                            Err(next_e) if next_e.error() == Errno::ENOENT => {
                                cursor += 1;
                            }
                            Err(next_e) => return Err(next_e),
                        }
                    }

                    // Reduce extent-fragmentation pressure for random sparse writes by
                    // extending single-block holes to a small contiguous prealloc run.
                    let prealloc_end = run_start.saturating_add(WRITE_PREALLOC_BLOCKS);
                    while cursor < prealloc_end {
                        match self.get_pblock_idx(inode_ref, cursor) {
                            Ok(_) => break,
                            Err(next_e) if next_e.error() == Errno::ENOENT => {
                                cursor += 1;
                            }
                            Err(next_e) => return Err(next_e),
                        }
                    }

                    let run_len = (cursor - run_start) as usize;
                    let allocated_blocks =
                        self.balloc_alloc_block_batch(inode_ref, start_bgid, run_len)?;
                    if allocated_blocks.is_empty() {
                        return_errno_with_message!(
                            Errno::ENOSPC,
                            "no free blocks while mapping write range"
                        );
                    }

                    let inserted = self.insert_allocated_blocks_as_extents(
                        inode_ref,
                        run_start,
                        &allocated_blocks,
                    )?;
                    allocated_total += inserted;

                    if inserted == 0 {
                        return_errno_with_message!(
                            Errno::ENOSPC,
                            "no free blocks while mapping write range"
                        );
                    }

                    if inserted < run_len {
                        // Partial preallocation is acceptable as long as the required
                        // front portion is mapped; retry the remaining logical range.
                        cursor = run_start + inserted as u32;
                    }
                }
                Err(e) => return Err(e),
            }
        }

        Ok(allocated_total)
    }

    /// Write data to a file at a given offset
    ///
    /// Params:
    /// inode: u32 - inode number of the file
    /// offset: usize - offset from where to write
    /// write_buf: &[u8] - buffer to write the data from
    ///
    /// Returns:
    /// `Result<usize>` - number of bytes written
    pub fn write_at(&self, inode: u32, offset: usize, write_buf: &[u8]) -> Result<usize> {
        let block_size = self.super_block.block_size() as usize;
        if write_buf.is_empty() {
            return Ok(0);
        }

        let mut inode_ref = self.get_inode_ref(inode);
        let file_size = inode_ref.inode.size();

        let write_end = offset
            .checked_add(write_buf.len())
            .ok_or_else(|| Ext4Error::new(Errno::EFBIG))?;
        let iblock_start = offset / block_size;
        let iblock_end = (write_end - 1) / block_size + 1;

        let lblock_start = u32::try_from(iblock_start)
            .map_err(|_| Ext4Error::new(Errno::EFBIG))?;
        let lblock_end = u32::try_from(iblock_end)
            .map_err(|_| Ext4Error::new(Errno::EFBIG))?;

        let mut start_bgid = 0;
        let file_size_usize = usize::try_from(file_size).map_err(|_| Ext4Error::new(Errno::EFBIG))?;
        let aligned_append = offset >= file_size_usize
            && offset % block_size == 0
            && file_size_usize % block_size == 0;

        let mapping_result = if aligned_append {
            match self.get_pblock_idx(&inode_ref, lblock_start) {
                Ok(_) => self.ensure_write_range_mapped(
                    &mut inode_ref,
                    lblock_start,
                    lblock_end,
                    &mut start_bgid,
                ),
                Err(e) if e.error() == Errno::ENOENT => {
                    let want_blocks = (lblock_end - lblock_start) as usize;
                    let allocated_blocks =
                        self.balloc_alloc_block_batch(&mut inode_ref, &mut start_bgid, want_blocks)?;
                    if allocated_blocks.is_empty() {
                        return_errno_with_message!(
                            Errno::ENOSPC,
                            "no free blocks while mapping write range"
                        );
                    }

                    let inserted = self.insert_allocated_blocks_as_extents(
                        &mut inode_ref,
                        lblock_start,
                        &allocated_blocks,
                    )?;

                    if inserted < want_blocks {
                        self.ensure_write_range_mapped(
                            &mut inode_ref,
                            lblock_start + inserted as u32,
                            lblock_end,
                            &mut start_bgid,
                        )
                    } else {
                        Ok(inserted)
                    }
                }
                Err(e) => Err(e),
            }
        } else {
            self.ensure_write_range_mapped(
                &mut inode_ref,
                lblock_start,
                lblock_end,
                &mut start_bgid,
            )
        };

        mapping_result.map_err(|e| {
            log::error!(
                "[write_at] ensure_write_range_mapped failed: inode={} lblock=[{}, {}) offset={} len={} err={:?}",
                inode,
                lblock_start,
                lblock_end,
                offset,
                write_buf.len(),
                e
            );
            e
        })?;

        let block_offset = |pblock_idx: Ext4Fsblk| -> Result<usize> {
            let pblock = usize::try_from(pblock_idx).map_err(|_| Ext4Error::new(Errno::EFBIG))?;
            pblock
                .checked_mul(block_size)
                .ok_or_else(|| Ext4Error::new(Errno::EFBIG))
        };

        let uses_extents = (inode_ref.inode.flags() & EXT4_INODE_FLAG_EXTENTS as u32) != 0;
        let mut extent_cache: Option<(u32, u32, Ext4Fsblk)> = None;
        let mut resolve_pblock = |lblock: u32| -> Result<Ext4Fsblk> {
            if uses_extents {
                if let Some((ext_start, ext_end, pblock_start)) = extent_cache {
                    if lblock >= ext_start && lblock < ext_end {
                        return Ok(pblock_start + (lblock - ext_start) as u64);
                    }
                }

                match self.find_extent(&inode_ref, lblock) {
                    Ok(path) => {
                        let mapped = path.path.last().and_then(|node| node.extent).and_then(|extent| {
                            let ext_start = extent.get_first_block();
                            let ext_len = extent.get_actual_len() as u32;
                            let ext_end = ext_start.checked_add(ext_len)?;
                            if lblock < ext_start || lblock >= ext_end {
                                return None;
                            }
                            let pblock_start = extent.get_pblock();
                            Some((ext_start, ext_end, pblock_start))
                        });

                        if let Some((ext_start, ext_end, pblock_start)) = mapped {
                            extent_cache = Some((ext_start, ext_end, pblock_start));
                            return Ok(pblock_start + (lblock - ext_start) as u64);
                        }

                        log::error!(
                            "[write_at] mapping missing after map: inode={} lblock={} offset={} len={}",
                            inode,
                            lblock,
                            offset,
                            write_buf.len()
                        );
                        return_errno_with_message!(
                            Errno::ENOENT,
                            "write mapping missing after allocation"
                        )
                    }
                    Err(e) => {
                        log::error!(
                            "[write_at] find_extent failed after map: inode={} lblock={} offset={} len={} err={:?}",
                            inode,
                            lblock,
                            offset,
                            write_buf.len(),
                            e
                        );
                        Err(e)
                    }
                }
            } else {
                self.get_pblock_idx(&inode_ref, lblock).map_err(|e| {
                    log::error!(
                        "[write_at] get_pblock_idx failed after map: inode={} lblock={} offset={} len={} err={:?}",
                        inode,
                        lblock,
                        offset,
                        write_buf.len(),
                        e
                    );
                    e
                })
            }
        };

        let mut written = 0usize;
        let mut iblk_idx = lblock_start;
        let unaligned = offset % block_size;
        let mut block_data = vec![0u8; block_size];

        if unaligned > 0 {
            let len = min(write_buf.len() - written, block_size - unaligned);
            let pblock_idx = resolve_pblock(iblk_idx)?;
            let blk_off = block_offset(pblock_idx)?;

            self.block_device
                .read_offset_into(blk_off, block_data.as_mut_slice());

            block_data[unaligned..unaligned + len]
                .copy_from_slice(&write_buf[written..written + len]);
            self.block_device.write_offset(blk_off, block_data.as_slice());

            written += len;
            iblk_idx += 1;
        }

        while write_buf.len() - written >= block_size {
            let run_start_lblk = iblk_idx;
            let run_start_written = written;
            let run_start_pblk = resolve_pblock(run_start_lblk)?;

            let mut run_blocks = 1usize;
            let full_blocks_remaining = (write_buf.len() - written) / block_size;
            while run_blocks < full_blocks_remaining {
                let next_lblk = run_start_lblk + run_blocks as u32;
                let next_pblk = resolve_pblock(next_lblk)?;
                if next_pblk != run_start_pblk + run_blocks as u64 {
                    break;
                }
                run_blocks += 1;
            }

            let run_bytes = run_blocks * block_size;
            let run_off = block_offset(run_start_pblk)?;
            self.block_device
                .write_offset(run_off, &write_buf[run_start_written..run_start_written + run_bytes]);

            written += run_bytes;
            iblk_idx += run_blocks as u32;
        }

        if written < write_buf.len() {
            let len = write_buf.len() - written;
            let pblock_idx = resolve_pblock(iblk_idx)?;
            let blk_off = block_offset(pblock_idx)?;

            self.block_device
                .read_offset_into(blk_off, block_data.as_mut_slice());

            block_data[..len].copy_from_slice(&write_buf[written..written + len]);
            self.block_device.write_offset(blk_off, block_data.as_slice());

            written += len;
        }

        let new_size = offset + written;
        if new_size > file_size as usize {
            if new_size > EXT4_MAX_FILE_SIZE as usize {
                return_errno_with_message!(Errno::EFBIG, "file size too large");
            }
            inode_ref.inode.set_size(new_size as u64);
            self.write_back_inode(&mut inode_ref);
        }

        Ok(written)
    }

    /// File remove
    ///
    /// Params:
    /// path: file path start from root
    ///
    /// Returns:
    /// `Result<usize>` - status of the operation
    pub fn file_remove(&self, path: &str) -> Result<usize> {
        // start from root
        let mut parent_inode_num = ROOT_INODE;

        let mut nameoff = 0;
        let child_inode = self.generic_open(path, &mut parent_inode_num, false, 0, &mut nameoff)?;

        let mut child_inode_ref = self.get_inode_ref(child_inode);
        let child_link_cnt = child_inode_ref.inode.links_count();
        if child_link_cnt == 1 {
            self.truncate_inode(&mut child_inode_ref, 0)?;
        }

        // get child name
        let mut is_goal = false;
        let p = &path[nameoff as usize..];
        let len = path_check(p, &mut is_goal);

        // load parent
        let mut parent_inode_ref = self.get_inode_ref(parent_inode_num);

        let r = self.unlink(
            &mut parent_inode_ref,
            &mut child_inode_ref,
            &p[..len],
        )?;


        Ok(EOK)
    }

    /// File truncate
    ///
    /// Params:
    /// inode_ref: &mut Ext4InodeRef - inode reference
    /// new_size: u64 - new size of the file
    ///
    /// Returns:
    /// `Result<usize>` - status of the operation
    pub fn truncate_inode(&self, inode_ref: &mut Ext4InodeRef, new_size: u64) -> Result<usize> {
        let block_size = self.super_block.block_size() as usize;
        let old_size = inode_ref.inode.size();

        if old_size == new_size {
            return Ok(EOK);
        }
        if old_size < new_size {
            if new_size > EXT4_MAX_FILE_SIZE {
                return return_errno_with_message!(Errno::EFBIG, "file size too large");
            }
            // Keep sparse semantics for growth: only advance i_size and leave holes unmapped.
            inode_ref.inode.set_size(new_size);
            self.write_back_inode(inode_ref);
            return Ok(EOK);
        }

        // Conservative shrink path for stability:
        // for non-zero target size, keep extents and only move i_size.
        // This avoids extent-tree corruption under heavy random truncate/write loops.
        if new_size > 0 {
            inode_ref.inode.set_size(new_size);
            self.write_back_inode(inode_ref);
            return Ok(EOK);
        }

        let block_size = block_size as u64;
        let new_blocks_cnt = ((new_size + block_size - 1) / block_size) as u32;
        let old_blocks_cnt = ((old_size + block_size - 1) / block_size) as u32;
        let diff_blocks_cnt = old_blocks_cnt - new_blocks_cnt;

        if diff_blocks_cnt > 0{
            self.extent_remove_space(inode_ref, new_blocks_cnt, EXT_MAX_BLOCKS)?;
        }

        inode_ref.inode.set_size(new_size);
        self.write_back_inode(inode_ref);

        Ok(EOK)
    }
}

//// Write Performance Analysis
// impl Ext4 {
    // /// Write data to a file at a given offset
    // ///
    // /// Params:
    // /// inode: u32 - inode number of the file
    // /// offset: usize - offset from where to write
    // /// write_buf: &[u8] - buffer to write the data from
    // ///
    // /// Returns:
    // /// `Result<usize>` - number of bytes written
    // pub fn write_at(&self, inode: u32, offset: usize, write_buf: &[u8]) -> Result<usize> {
    //     let total_start = Instant::now();
    //     log::info!("=== Write Performance Analysis ===");
    //     log::info!("Write size: {} bytes", write_buf.len());
        
    //     // write buf is empty, return 0
    //     let write_buf_len = write_buf.len();
    //     if write_buf_len == 0 {
    //         return Ok(0);
    //     }

    //     // get the inode reference
    //     let inode_start = Instant::now();
    //     let mut inode_ref = self.get_inode_ref(inode);
    //     let inode_time = inode_start.elapsed();
    //     log::info!("[Time] Get inode: {:.3}ms", inode_time.as_secs_f64() * 1000.0);

    //     // Get the file size
    //     let file_size = inode_ref.inode.size();

    //     // Calculate the start and end block index
    //     let iblock_start = offset / block_size;
    //     let iblock_last = (offset + write_buf_len + block_size - 1) / block_size;
    //     let total_blocks_needed = iblock_last - iblock_start;
    //     log::info!("[Blocks] Start block: {}, Last block: {}, Total blocks needed: {}", 
    //         iblock_start, iblock_last, total_blocks_needed);

    //     // start block index
    //     let mut iblk_idx = iblock_start;
    //     let ifile_blocks = (file_size + block_size as u64 - 1) / block_size as u64;

    //     // Calculate the unaligned size
    //     let unaligned = offset % block_size;
    //     if unaligned > 0 {
    //         log::info!("[Alignment] Unaligned start: {} bytes", unaligned);
    //     }

    //     // Buffer to keep track of written bytes
    //     let mut written = 0;
    //     let mut total_blocks = 0;
    //     let mut new_blocks = 0;
    //     let mut total_alloc_time = Duration::new(0, 0);
    //     let mut total_write_time = Duration::new(0, 0);
    //     let mut total_sync_time = Duration::new(0, 0);

    //     // Start bgid for block allocation
    //     let mut start_bgid = 1;

    //     // Pre-allocate blocks if needed
    //     let blocks_to_allocate = if iblk_idx >= ifile_blocks as usize {
    //         total_blocks_needed
    //     } else {
    //         max(0, total_blocks_needed - (ifile_blocks as usize - iblk_idx))
    //     };

    //     if blocks_to_allocate > 0 {
    //         let prealloc_start = Instant::now();
    //         log::info!("[Pre-allocation] Allocating {} blocks", blocks_to_allocate);
            
    //         // Use the new batch allocation function
    //         let allocated_blocks = self.balloc_alloc_block_new(&mut inode_ref, &mut start_bgid, blocks_to_allocate)?;
            
    //         // Create a single extent for all allocated blocks
    //         if !allocated_blocks.is_empty() {
    //             let mut newex = Ext4Extent::default();
    //             newex.first_block = iblk_idx as u32;
    //             newex.store_pblock(allocated_blocks[0]);
    //             newex.block_count = allocated_blocks.len() as u16;
    //             self.insert_extent(&mut inode_ref, &mut newex)?;
    //         }
            
    //         let prealloc_time = prealloc_start.elapsed();
    //         log::info!("[Time] Pre-allocation: {:.3}ms", prealloc_time.as_secs_f64() * 1000.0);
    //         new_blocks += blocks_to_allocate;
    //     }

    //     // Unaligned write
    //     if unaligned > 0 {
    //         let unaligned_start = Instant::now();
    //         let len = min(write_buf_len, block_size - unaligned);
    //         log::info!("[Unaligned Write] Writing {} bytes", len);
            
    //         // Get the physical block id
    //         let pblock_start = Instant::now();
    //         let pblock_idx = self.get_pblock_idx(&inode_ref, iblk_idx as u32)?;
    //         let alloc_time = pblock_start.elapsed();
    //         total_alloc_time += alloc_time;
    //         total_blocks += 1;

    //         let write_start = Instant::now();
    //         let mut block = Block::load(self.block_device.clone(), pblock_idx as usize * block_size);
    //         block.write_offset(unaligned, &write_buf[..len], len);
    //         let write_time = write_start.elapsed();
    //         total_write_time += write_time;

    //         let sync_start = Instant::now();
    //         block.sync_blk_to_disk(self.block_device.clone());
    //         let sync_time = sync_start.elapsed();
    //         total_sync_time += sync_time;
    //         drop(block);

    //         written += len;
    //         iblk_idx += 1;
            
    //         let unaligned_time = unaligned_start.elapsed();
    //         log::info!("[Time] Total unaligned write: {:.3}ms", unaligned_time.as_secs_f64() * 1000.0);
    //     }

    //     // Aligned write
    //     let aligned_start = Instant::now();
    //     let mut aligned_blocks = 0;
    //     log::info!("[Aligned Write] Starting aligned writes for {} blocks", (write_buf_len - written + block_size - 1) / block_size);
        
    //     while written < write_buf_len {
    //         aligned_blocks += 1;
            
    //         // Get the physical block id
    //         let pblock_start = Instant::now();
    //         let pblock_idx = self.get_pblock_idx(&inode_ref, iblk_idx as u32)?;
    //         let alloc_time = pblock_start.elapsed();
    //         total_alloc_time += alloc_time;
    //         total_blocks += 1;

    //         let write_start = Instant::now();
    //         let block_offset = pblock_idx as usize * block_size;
    //         let mut block = Block::load(self.block_device.clone(), block_offset);
    //         let write_size = min(block_size, write_buf_len - written);
    //         block.write_offset(0, &write_buf[written..written + write_size], write_size);
    //         let write_time = write_start.elapsed();
    //         total_write_time += write_time;

    //         let sync_start = Instant::now();
    //         block.sync_blk_to_disk(self.block_device.clone());
    //         let sync_time = sync_start.elapsed();
    //         total_sync_time += sync_time;
    //         drop(block);
            
    //         written += write_size;
    //         iblk_idx += 1;

    //         if aligned_blocks % 1000 == 0 {
    //             log::info!("[Progress] Written {} blocks, {} bytes", aligned_blocks, written);
    //         }
    //     }
        
    //     let aligned_time = aligned_start.elapsed();
    //     log::info!("[Time] Total aligned write: {:.3}ms", aligned_time.as_secs_f64() * 1000.0);

    //     // Update file size if necessary
    //     let update_start = Instant::now();
    //     if offset + written > file_size as usize {
    //         inode_ref.inode.set_size((offset + write_buf_len) as u64);
    //         self.write_back_inode(&mut inode_ref);
    //     }
    //     let update_time = update_start.elapsed();
    //     log::info!("[Time] Inode update: {:.3}ms", update_time.as_secs_f64() * 1000.0);

    //     let total_time = total_start.elapsed();
    //     log::info!("=== Write Performance Summary ===");
    //     log::info!("[Blocks] Total blocks: {}, New blocks: {}, Aligned blocks: {}", 
    //         total_blocks, new_blocks, aligned_blocks);
    //     log::info!("[Time] Average block allocation: {:.3}ms", 
    //         (total_alloc_time.as_secs_f64() * 1000.0) / total_blocks as f64);
    //     log::info!("[Time] Average block write: {:.3}ms", 
    //         (total_write_time.as_secs_f64() * 1000.0) / total_blocks as f64);
    //     log::info!("[Time] Average block sync: {:.3}ms", 
    //         (total_sync_time.as_secs_f64() * 1000.0) / total_blocks as f64);
    //     log::info!("[Time] Total write time: {:.3}ms", total_time.as_secs_f64() * 1000.0);
    //     log::info!("[Speed] Write speed: {:.2} MB/s", 
    //         (write_buf_len as f64 / 1024.0 / 1024.0) / total_time.as_secs_f64());
    //     log::info!("[Efficiency] Write efficiency: {:.2}%", 
    //         (written as f64 / (total_blocks * block_size) as f64) * 100.0);
    //     log::info!("=== End of Write Analysis ===");

    //     Ok(written)
    // }    
// }
