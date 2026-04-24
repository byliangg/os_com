use alloc::format;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::ext4_defs::*;
use crate::prelude::*;
use crate::return_errno_with_message;
use crate::simple_interface::SimpleBlockRange;
use crate::utils::path_check;
// use std::time::{Duration, Instant};

// Keep sparse-write allocation modest: large enough to avoid pathological
// one-block extent growth under random sparse writes, but still conservative
// enough not to explode mapping size under truncate-heavy workloads.
const WRITE_PREALLOC_BLOCKS: u32 = 16;
const SMALL_WRITE_PREALLOC_BLOCKS: u32 = 1;
const GENERIC014_MAP_PROFILE_LIMIT: u64 = 64;

static GENERIC014_MAP_PROFILE_SEQ: AtomicU64 = AtomicU64::new(0);

impl Ext4 {
    fn initial_write_alloc_bgid(
        &self,
        inode_ref: &Ext4InodeRef,
        lblock_start: u32,
        lblock_end: u32,
    ) -> u32 {
        let try_mapped_bgid =
            |lblock: u32| self.get_pblock_idx(inode_ref, lblock).ok().map(|pblock| self.get_bgid_of_block(pblock));

        if let Some(prev_lblock) = lblock_start.checked_sub(1) {
            if let Some(bgid) = try_mapped_bgid(prev_lblock) {
                return bgid;
            }
        }

        if let Some(bgid) = try_mapped_bgid(lblock_end) {
            return bgid;
        }

        if (inode_ref.inode.flags() & EXT4_INODE_FLAG_EXTENTS as u32) != 0 {
            if let Ok(path) = self.find_extent(inode_ref, lblock_start) {
                if let Some(node) = path.path.last() {
                    if let Some(extent) = node.extent {
                        let ext_start = extent.get_first_block();
                        let ext_len = extent.get_actual_len() as u32;
                        let candidate_pblock = if ext_len == 0 {
                            None
                        } else if lblock_start <= ext_start {
                            Some(extent.get_pblock())
                        } else {
                            Some(extent.get_pblock() + ext_len.saturating_sub(1) as u64)
                        };
                        if let Some(pblock) = candidate_pblock {
                            return self.get_bgid_of_block(pblock);
                        }
                    }
                }
            }
        }

        self.get_bgid_of_inode(inode_ref.inode_num)
    }

    fn extent_tree_debug_summary(&self, inode_ref: &Ext4InodeRef) -> String {
        let root_header = inode_ref.inode.root_extent_header();
        let root_capacity = (inode_ref.inode.block.len().saturating_sub(3)) / 3;
        if (inode_ref.inode.flags() & EXT4_INODE_FLAG_EXTENTS as u32) == 0 {
            return "legacy".to_string();
        }

        if root_header.entries_count == 0 {
            return format!(
                "root(depth={},entries=0,max={})",
                root_header.depth, root_header.max_entries_count
            );
        }

        if root_header.entries_count as usize > root_capacity {
            return format!(
                "root(depth={},entries={},max={},invalid_root_capacity={})",
                root_header.depth,
                root_header.entries_count,
                root_header.max_entries_count,
                root_capacity
            );
        }

        if root_header.depth == 0 {
            let first = Ext4Extent::load_from_u32(&inode_ref.inode.block[3..]);
            let last_off = 3 + (root_header.entries_count as usize - 1) * 3;
            let last = Ext4Extent::load_from_u32(&inode_ref.inode.block[last_off..]);
            return format!(
                "root(depth=0,entries={},first=[lb={},pb={},len={}],last=[lb={},pb={},len={}])",
                root_header.entries_count,
                first.first_block,
                first.get_pblock(),
                first.get_actual_len(),
                last.first_block,
                last.get_pblock(),
                last.get_actual_len()
            );
        }

        let block_size = self.super_block.block_size() as usize;
        let first_index = Ext4ExtentIndex::load_from_u32(&inode_ref.inode.block[3..]);
        let child_block = first_index.get_pblock();
        let child = Block::load(&self.block_device, child_block as usize * block_size);
        let child_header = Ext4ExtentHeader::load_from_u8(&child.data[..EXT4_EXTENT_HEADER_SIZE]);
        let entry_size = if child_header.depth == 0 {
            EXT4_EXTENT_SIZE
        } else {
            EXT4_EXTENT_INDEX_SIZE
        };
        let child_capacity = child
            .data
            .len()
            .saturating_sub(EXT4_EXTENT_HEADER_SIZE)
            / entry_size;

        if child_header.entries_count == 0 {
            return format!(
                "root(depth={},entries={},first_idx=[lb={},child={}]) child(depth={},entries=0,max={})",
                root_header.depth,
                root_header.entries_count,
                first_index.first_block,
                child_block,
                child_header.depth,
                child_header.max_entries_count
            );
        }

        if child_header.entries_count as usize > child_capacity {
            return format!(
                "root(depth={},entries={},first_idx=[lb={},child={}]) child(depth={},entries={},max={},invalid_child_capacity={})",
                root_header.depth,
                root_header.entries_count,
                first_index.first_block,
                child_block,
                child_header.depth,
                child_header.entries_count,
                child_header.max_entries_count,
                child_capacity
            );
        }

        if child_header.depth == 0 {
            let first = Ext4Extent::load_from_u8(
                &child.data[EXT4_EXTENT_HEADER_SIZE..EXT4_EXTENT_HEADER_SIZE + EXT4_EXTENT_SIZE],
            );
            let last_off = EXT4_EXTENT_HEADER_SIZE
                + (child_header.entries_count as usize - 1) * EXT4_EXTENT_SIZE;
            let last = Ext4Extent::load_from_u8(&child.data[last_off..last_off + EXT4_EXTENT_SIZE]);
            return format!(
                "root(depth={},entries={},first_idx=[lb={},child={}]) child(depth=0,entries={},max={},first=[lb={},pb={},len={}],last=[lb={},pb={},len={}])",
                root_header.depth,
                root_header.entries_count,
                first_index.first_block,
                child_block,
                child_header.entries_count,
                child_header.max_entries_count,
                first.first_block,
                first.get_pblock(),
                first.get_actual_len(),
                last.first_block,
                last.get_pblock(),
                last.get_actual_len()
            );
        }

        let first_child_idx = Ext4ExtentIndex::load_from_u8(
            &child.data[EXT4_EXTENT_HEADER_SIZE..EXT4_EXTENT_HEADER_SIZE + EXT4_EXTENT_INDEX_SIZE],
        );
        let last_off = EXT4_EXTENT_HEADER_SIZE
            + (child_header.entries_count as usize - 1) * EXT4_EXTENT_INDEX_SIZE;
        let last_child_idx =
            Ext4ExtentIndex::load_from_u8(&child.data[last_off..last_off + EXT4_EXTENT_INDEX_SIZE]);
        format!(
            "root(depth={},entries={},first_idx=[lb={},child={}]) child(depth={},entries={},max={},first_idx=[lb={},child={}],last_idx=[lb={},child={}])",
            root_header.depth,
            root_header.entries_count,
            first_index.first_block,
            child_block,
            child_header.depth,
            child_header.entries_count,
            child_header.max_entries_count,
            first_child_idx.first_block,
            first_child_idx.get_pblock(),
            last_child_idx.first_block,
            last_child_idx.get_pblock()
        )
    }

    fn extent_leaf_debug_entries(&self, inode_ref: &Ext4InodeRef, limit: usize) -> String {
        let root_header = inode_ref.inode.root_extent_header();
        let root_capacity = (inode_ref.inode.block.len().saturating_sub(3)) / 3;
        if (inode_ref.inode.flags() & EXT4_INODE_FLAG_EXTENTS as u32) == 0 {
            return "legacy".to_string();
        }
        if root_header.entries_count == 0 {
            return "[]".to_string();
        }
        if root_header.entries_count as usize > root_capacity {
            return format!(
                "[invalid_root_header:entries={},capacity={}]",
                root_header.entries_count,
                root_capacity
            );
        }

        let mut entries = Vec::new();
        if root_header.depth == 0 {
            let count = usize::min(root_header.entries_count as usize, limit);
            for i in 0..count {
                let off = 3 + i * 3;
                let ext = Ext4Extent::load_from_u32(&inode_ref.inode.block[off..]);
                entries.push(format!(
                    "#{}:[lb={},pb={},len={}]",
                    i,
                    ext.first_block,
                    ext.get_pblock(),
                    ext.get_actual_len()
                ));
            }
            return format!("[{}]", entries.join(","));
        }

        let block_size = self.super_block.block_size() as usize;
        let first_index = Ext4ExtentIndex::load_from_u32(&inode_ref.inode.block[3..]);
        let child_block = first_index.get_pblock();
        let child = Block::load(&self.block_device, child_block as usize * block_size);
        let child_header = Ext4ExtentHeader::load_from_u8(&child.data[..EXT4_EXTENT_HEADER_SIZE]);
        let child_capacity = child
            .data
            .len()
            .saturating_sub(EXT4_EXTENT_HEADER_SIZE)
            / EXT4_EXTENT_SIZE;
        if child_header.depth != 0 || child_header.entries_count == 0 {
            return format!(
                "[child_header:magic={:x},entries={},max={},depth={}]",
                child_header.magic,
                child_header.entries_count,
                child_header.max_entries_count,
                child_header.depth
            );
        }
        if child_header.entries_count as usize > child_capacity {
            return format!(
                "[invalid_child_header:entries={},capacity={},depth={}]",
                child_header.entries_count,
                child_capacity,
                child_header.depth
            );
        }

        let count = usize::min(child_header.entries_count as usize, limit);
        for i in 0..count {
            let off = EXT4_EXTENT_HEADER_SIZE + i * EXT4_EXTENT_SIZE;
            let ext = Ext4Extent::load_from_u8(&child.data[off..off + EXT4_EXTENT_SIZE]);
            entries.push(format!(
                "#{}:[lb={},pb={},len={}]",
                i,
                ext.first_block,
                ext.get_pblock(),
                ext.get_actual_len()
            ));
        }
        format!("[{}]", entries.join(","))
    }

    fn push_block_range(
        mappings: &mut Vec<SimpleBlockRange>,
        lblock: u32,
        pblock: u64,
        len: u32,
    ) {
        if len == 0 {
            return;
        }

        if let Some(last) = mappings.last_mut() {
            let last_lend = last.lblock.saturating_add(last.len);
            let last_pend = last.pblock.saturating_add(last.len as u64);
            if last_lend == lblock && last_pend == pblock {
                last.len = last.len.saturating_add(len);
                return;
            }
        }

        mappings.push(SimpleBlockRange { lblock, pblock, len });
    }

    fn resolve_block_mapping(
        &self,
        inode_ref: &Ext4InodeRef,
        uses_extents: bool,
        extent_cache: &mut Option<(u32, u32, Ext4Fsblk)>,
        lblock: u32,
    ) -> Result<Option<Ext4Fsblk>> {
        if uses_extents {
            if let Some((ext_start, ext_end, pblock_start)) = *extent_cache {
                if lblock >= ext_start && lblock < ext_end {
                    return Ok(Some(pblock_start + (lblock - ext_start) as u64));
                }
            }

            match self.find_extent(inode_ref, lblock) {
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
                        *extent_cache = Some((ext_start, ext_end, pblock_start));
                        return Ok(Some(pblock_start + (lblock - ext_start) as u64));
                    }

                    Ok(None)
                }
                Err(e) if e.error() == Errno::ENOENT => Ok(None),
                Err(e) => Err(e),
            }
        } else {
            match self.get_pblock_idx(inode_ref, lblock) {
                Ok(pblock) => Ok(Some(pblock)),
                Err(e) if e.error() == Errno::ENOENT => Ok(None),
                Err(e) => Err(e),
            }
        }
    }

    fn collect_block_ranges(
        &self,
        inode_ref: &Ext4InodeRef,
        lblock_start: u32,
        lblock_count: u32,
    ) -> Result<Vec<SimpleBlockRange>> {
        if lblock_count == 0 {
            return Ok(Vec::new());
        }

        let lblock_end = lblock_start
            .checked_add(lblock_count)
            .ok_or_else(|| Ext4Error::new(Errno::EFBIG))?;
        let uses_extents = (inode_ref.inode.flags() & EXT4_INODE_FLAG_EXTENTS as u32) != 0;
        let mut extent_cache: Option<(u32, u32, Ext4Fsblk)> = None;
        let mut mappings = Vec::new();
        let mut cursor = lblock_start;

        while cursor < lblock_end {
            let Some(pblock_start) =
                self.resolve_block_mapping(inode_ref, uses_extents, &mut extent_cache, cursor)?
            else {
                cursor += 1;
                continue;
            };

            if uses_extents {
                if let Some((ext_start, ext_end, cached_pblock_start)) = extent_cache {
                    if cursor >= ext_start && cursor < ext_end {
                        let run_end = ext_end.min(lblock_end);
                        Self::push_block_range(
                            &mut mappings,
                            cursor,
                            cached_pblock_start + (cursor - ext_start) as u64,
                            run_end - cursor,
                        );
                        cursor = run_end;
                        continue;
                    }
                }
            }

            let run_start = cursor;
            cursor += 1;
            while cursor < lblock_end {
                let Some(next_pblock) = self.resolve_block_mapping(
                    inode_ref,
                    uses_extents,
                    &mut extent_cache,
                    cursor,
                )?
                else {
                    break;
                };
                if next_pblock != pblock_start + (cursor - run_start) as u64 {
                    break;
                }
                cursor += 1;
            }

            Self::push_block_range(&mut mappings, run_start, pblock_start, cursor - run_start);
        }

        Ok(mappings)
    }

    fn collect_allocated_block_ranges(
        &self,
        lblock_start: u32,
        allocated_blocks: &[Ext4Fsblk],
    ) -> Vec<SimpleBlockRange> {
        let mut mappings = Vec::new();
        if allocated_blocks.is_empty() {
            return mappings;
        }

        let mut logical = lblock_start;
        let mut seg_begin = 0usize;
        while seg_begin < allocated_blocks.len() {
            let mut seg_end = seg_begin + 1;
            while seg_end < allocated_blocks.len()
                && allocated_blocks[seg_end] == allocated_blocks[seg_end - 1] + 1
            {
                seg_end += 1;
            }

            let run_len = (seg_end - seg_begin) as u32;
            Self::push_block_range(
                &mut mappings,
                logical,
                allocated_blocks[seg_begin],
                run_len,
            );
            logical = logical.saturating_add(run_len);
            seg_begin = seg_end;
        }

        mappings
    }

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

    /// Like `create` but uses `dir_add_entry_unchecked` (no fallback scan).
    /// Returns `(inode_ref, dir_byte_offset)` where `dir_byte_offset` is the
    /// absolute byte offset of the new entry in the parent directory stream.
    /// Call only when the caller has verified the name is absent via the
    /// kernel-layer directory cache, avoiding the O(n) scan in large dirs.
    pub fn create_unchecked(&self, parent: u32, name: &str, inode_mode: u16) -> Result<(Ext4InodeRef, u64)> {
        let mut parent_inode_ref = self.get_inode_ref(parent);

        let init_child_ref = self.create_inode(inode_mode)?;
        self.write_back_inode_without_csum(&init_child_ref);
        let mut child_inode_ref = self.get_inode_ref(init_child_ref.inode_num);

        let dir_byte_offset = self.link_unchecked(&mut parent_inode_ref, &mut child_inode_ref, name)?;

        self.write_back_inode(&mut parent_inode_ref);
        self.write_back_inode(&mut child_inode_ref);

        Ok((child_inode_ref, dir_byte_offset))
    }

    /// Like `link` but uses `dir_add_entry_unchecked` for the parent entry.
    /// Returns the absolute byte offset of the new entry in the parent directory stream.
    /// The "." and ".." entries in the new child dir still use dir_add_entry
    /// (they always fit in the first block, so no scan happens there).
    pub fn link_unchecked(
        &self,
        parent: &mut Ext4InodeRef,
        child: &mut Ext4InodeRef,
        name: &str,
    ) -> Result<u64> {
        let dir_byte_offset = self.dir_add_entry_unchecked(parent, child, name)?;
        self.write_back_inode_without_csum(parent);

        if child.inode.is_dir() {
            let new_child_ref = Ext4InodeRef {
                inode_num: child.inode_num,
                inode: child.inode,
            };
            self.dir_add_entry(child, &new_child_ref, ".")?;
            self.dir_add_entry(child, parent, "..")?;
            child.inode.set_links_count(2);
            let link_cnt = parent.inode.links_count() + 1;
            parent.inode.set_links_count(link_cnt);
            return Ok(dir_byte_offset);
        }

        let link_cnt = child.inode.links_count() + 1;
        child.inode.set_links_count(link_cnt);
        Ok(dir_byte_offset)
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

        if is_dir || inode_file_type == InodeFileType::S_IFREG {
            inode.set_flags(EXT4_INODE_FLAG_EXTENTS as u32);
            inode.extent_tree_init();
        } else {
            inode.set_flags(0);
        }

        let inode_ref = Ext4InodeRef {
            inode_num,
            inode,
        };

        if inode_file_type == InodeFileType::S_IFREG
            && (inode_ref.inode.flags() & EXT4_INODE_FLAG_EXTENTS as u32) == 0
        {
            log::warn!(
                "[create_inode] regular inode created without extents: inode={} mode={:#o} flags={:#x}",
                inode_ref.inode_num,
                inode_ref.inode.mode(),
                inode_ref.inode.flags()
            );
        }

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

    pub fn map_blocks(
        &self,
        inode: u32,
        lblock_start: u32,
        lblock_count: u32,
    ) -> Result<Vec<SimpleBlockRange>> {
        let inode_ref = self.get_inode_ref(inode);
        self.collect_block_ranges(&inode_ref, lblock_start, lblock_count)
    }

    pub fn plan_direct_read(
        &self,
        inode: u32,
        offset: usize,
        len: usize,
    ) -> Result<(usize, Vec<SimpleBlockRange>)> {
        let block_size = self.super_block.block_size() as usize;
        if len == 0 {
            return Ok((0, Vec::new()));
        }

        let inode_ref = self.get_inode_ref(inode);
        let file_size =
            usize::try_from(inode_ref.inode.size()).map_err(|_| Ext4Error::new(Errno::EFBIG))?;
        let read_end = offset.saturating_add(len).min(file_size);
        let read_len = read_end.saturating_sub(offset);
        let direct_len = read_len / block_size * block_size;
        if direct_len == 0 {
            return Ok((0, Vec::new()));
        }

        let lblock_start =
            u32::try_from(offset / block_size).map_err(|_| Ext4Error::new(Errno::EFBIG))?;
        let lblock_count =
            u32::try_from(direct_len / block_size).map_err(|_| Ext4Error::new(Errno::EFBIG))?;
        let mappings = self.collect_block_ranges(&inode_ref, lblock_start, lblock_count)?;
        Ok((direct_len, mappings))
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

        let uses_extents = (inode_ref.inode.flags() & EXT4_INODE_FLAG_EXTENTS as u32) != 0;
        let requested_blocks = end_lblock.saturating_sub(start_lblock);
        let prealloc_blocks = if requested_blocks <= 1 {
            SMALL_WRITE_PREALLOC_BLOCKS
        } else {
            WRITE_PREALLOC_BLOCKS
        };
        let profile_seq = if uses_extents && requested_blocks <= 1 {
            let seq = GENERIC014_MAP_PROFILE_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
            if seq <= GENERIC014_MAP_PROFILE_LIMIT || seq % 128 == 0 {
                Some(seq)
            } else {
                None
            }
        } else {
            None
        };
        if let Some(seq) = profile_seq {
            log::debug!(
                "ext4_rs: generic014-map seq={} phase=start inode={} lblock=[{}, {}) start_bgid={} file_size={} flags={:#x}",
                seq,
                inode_ref.inode_num,
                start_lblock,
                end_lblock,
                *start_bgid,
                inode_ref.inode.size(),
                inode_ref.inode.flags()
            );
        }
        let mut allocated_total = 0usize;
        let mut cursor = start_lblock;

        if !uses_extents {
            while cursor < end_lblock {
                match self.get_pblock_idx(inode_ref, cursor) {
                    Ok(_) => cursor += 1,
                    Err(e) if e.error() == Errno::ENOENT => {
                        self.legacy_map_block_from(inode_ref, cursor, start_bgid)?;
                        allocated_total += 1;
                        cursor += 1;
                    }
                    Err(e) => return Err(e),
                }
            }
            return Ok(allocated_total);
        }

        while cursor < end_lblock {
            if let Some(seq) = profile_seq {
                log::debug!(
                "ext4_rs: generic014-map seq={} phase=initial_probe_start inode={} cursor={}",
                    seq,
                    inode_ref.inode_num,
                    cursor
                );
            }
            let initial_probe = self.get_pblock_idx(inode_ref, cursor);
            match initial_probe {
                Ok(pblock) => {
                    if let Some(seq) = profile_seq {
                        log::debug!(
                "ext4_rs: generic014-map seq={} phase=initial_probe_mapped inode={} cursor={} pblock={}",
                            seq,
                            inode_ref.inode_num,
                            cursor,
                            pblock
                        );
                    }
                    cursor += 1;
                }
                Err(e) if e.error() == Errno::ENOENT => {
                    let run_start = cursor;
                    if let Some(seq) = profile_seq {
                        log::debug!(
                "ext4_rs: generic014-map seq={} phase=hole_scan_start inode={} run_start={}",
                            seq,
                            inode_ref.inode_num,
                            run_start
                        );
                    }
                    cursor += 1;
                    while cursor < end_lblock {
                        match self.get_pblock_idx(inode_ref, cursor) {
                            Ok(_) => break,
                            Err(next_e) if next_e.error() == Errno::ENOENT => cursor += 1,
                            Err(next_e) => return Err(next_e),
                        }
                    }
                    if let Some(seq) = profile_seq {
                        log::debug!(
                "ext4_rs: generic014-map seq={} phase=hole_scan_done inode={} run_start={} cursor={}",
                            seq,
                            inode_ref.inode_num,
                            run_start,
                            cursor
                        );
                    }

                    let prealloc_end = run_start.saturating_add(prealloc_blocks);
                    if let Some(seq) = profile_seq {
                        log::debug!(
                "ext4_rs: generic014-map seq={} phase=prealloc_probe_start inode={} cursor={} prealloc_end={} prealloc_blocks={}",
                            seq,
                            inode_ref.inode_num,
                            cursor,
                            prealloc_end,
                            prealloc_blocks
                        );
                    }
                    while cursor < prealloc_end {
                        match self.get_pblock_idx(inode_ref, cursor) {
                            Ok(_) => break,
                            Err(next_e) if next_e.error() == Errno::ENOENT => cursor += 1,
                            Err(next_e) => return Err(next_e),
                        }
                    }
                    if let Some(seq) = profile_seq {
                        log::debug!(
                "ext4_rs: generic014-map seq={} phase=prealloc_probe_done inode={} run_start={} cursor={} run_len={}",
                            seq,
                            inode_ref.inode_num,
                            run_start,
                            cursor,
                            cursor - run_start
                        );
                    }

                    let run_len = (cursor - run_start) as usize;
                    if let Some(seq) = profile_seq {
                        log::debug!(
                "ext4_rs: generic014-map seq={} phase=alloc_start inode={} run_start={} run_len={} start_bgid={}",
                            seq,
                            inode_ref.inode_num,
                            run_start,
                            run_len,
                            *start_bgid
                        );
                    }
                    let allocated_blocks =
                        self.balloc_alloc_block_batch(inode_ref, start_bgid, run_len)?;
                    if let Some(seq) = profile_seq {
                        log::debug!(
                "ext4_rs: generic014-map seq={} phase=alloc_done inode={} requested={} got={} start_bgid={} blocks={:?}",
                            seq,
                            inode_ref.inode_num,
                            run_len,
                            allocated_blocks.len(),
                            *start_bgid,
                            allocated_blocks
                        );
                    }
                    if allocated_blocks.is_empty() {
                        return_errno_with_message!(
                            Errno::ENOSPC,
                            "no free blocks while mapping write range"
                        );
                    }

                    if let Some(seq) = profile_seq {
                        log::debug!(
                "ext4_rs: generic014-map seq={} phase=insert_start inode={} run_start={} blocks={:?}",
                            seq,
                            inode_ref.inode_num,
                            run_start,
                            allocated_blocks
                        );
                    }
                    let inserted = self.insert_allocated_blocks_as_extents(
                        inode_ref,
                        run_start,
                        &allocated_blocks,
                    )?;
                    if let Some(seq) = profile_seq {
                        log::debug!(
                "ext4_rs: generic014-map seq={} phase=insert_done inode={} run_start={} inserted={} allocated_total_before={}",
                            seq,
                            inode_ref.inode_num,
                            run_start,
                            inserted,
                            allocated_total
                        );
                    }
                    allocated_total += inserted;

                    if inserted > 0 {
                        let verify_last = run_start + inserted as u32 - 1;
                        if let Some(seq) = profile_seq {
                            log::debug!(
                "ext4_rs: generic014-map seq={} phase=verify_start inode={} verify_lblock={}",
                                seq,
                                inode_ref.inode_num,
                                verify_last
                            );
                        }
                        if let Err(verify_err) = self.get_pblock_idx(inode_ref, verify_last) {
                            let root_header = inode_ref.inode.root_extent_header();
                            let reloaded = self.get_inode_ref(inode_ref.inode_num);
                            let reloaded_root = reloaded.inode.root_extent_header();
                            let inmem_tree = self.extent_tree_debug_summary(inode_ref);
                            let reloaded_tree = self.extent_tree_debug_summary(&reloaded);
                            let inmem_leaf_entries = self.extent_leaf_debug_entries(inode_ref, 12);
                            let reloaded_leaf_entries =
                                self.extent_leaf_debug_entries(&reloaded, 12);
                            log::error!(
                                "[ensure_write_range_mapped] verification failed after extent insert: inode={} run_start={} inserted={} verify_lblock={} err={:?} allocated_blocks={:?} inmem_flags={:#x} inmem_size={} inmem_root={{magic:{:x},entries:{},max:{},depth:{}}} reloaded_flags={:#x} reloaded_size={} reloaded_root={{magic:{:x},entries:{},max:{},depth:{}}} inmem_blocks={:?} reloaded_blocks={:?} inmem_tree={} reloaded_tree={} inmem_leaf_entries={} reloaded_leaf_entries={}",
                                inode_ref.inode_num,
                                run_start,
                                inserted,
                                verify_last,
                                verify_err,
                                allocated_blocks,
                                inode_ref.inode.flags(),
                                inode_ref.inode.size(),
                                root_header.magic,
                                root_header.entries_count,
                                root_header.max_entries_count,
                                root_header.depth,
                                reloaded.inode.flags(),
                                reloaded.inode.size(),
                                reloaded_root.magic,
                                reloaded_root.entries_count,
                                reloaded_root.max_entries_count,
                                reloaded_root.depth,
                                inode_ref.inode.block,
                                reloaded.inode.block,
                                inmem_tree,
                                reloaded_tree,
                                inmem_leaf_entries,
                                reloaded_leaf_entries
                            );
                        } else if let Some(seq) = profile_seq {
                            log::debug!(
                "ext4_rs: generic014-map seq={} phase=verify_done inode={} verify_lblock={}",
                                seq,
                                inode_ref.inode_num,
                                verify_last
                            );
                        }
                    }

                    if inserted == 0 {
                        return_errno_with_message!(
                            Errno::ENOSPC,
                            "no free blocks while mapping write range"
                        );
                    }

                    if inserted < run_len {
                        cursor = run_start + inserted as u32;
                    }
                }
                Err(e) => return Err(e),
            }
        }

        if let Some(seq) = profile_seq {
            log::debug!(
                "ext4_rs: generic014-map seq={} phase=done inode={} allocated_total={}",
                seq,
                inode_ref.inode_num,
                allocated_total
            );
        }
        Ok(allocated_total)
    }

    pub fn prepare_write_at(
        &self,
        inode: u32,
        offset: usize,
        len: usize,
    ) -> Result<Vec<SimpleBlockRange>> {
        let block_size = self.super_block.block_size() as usize;
        if len == 0 {
            return Ok(Vec::new());
        }
        if offset % block_size != 0 || len % block_size != 0 {
            return_errno_with_message!(Errno::EINVAL, "direct write range is not block aligned");
        }

        let mut inode_ref = self.get_inode_ref(inode);
        let file_size = inode_ref.inode.size();
        let write_end = offset
            .checked_add(len)
            .ok_or_else(|| Ext4Error::new(Errno::EFBIG))?;
        let lblock_start = u32::try_from(offset / block_size)
            .map_err(|_| Ext4Error::new(Errno::EFBIG))?;
        let lblock_end = u32::try_from(write_end / block_size)
            .map_err(|_| Ext4Error::new(Errno::EFBIG))?;

        let mut start_bgid = self.initial_write_alloc_bgid(&inode_ref, lblock_start, lblock_end);
        self.ensure_write_range_mapped(
            &mut inode_ref,
            lblock_start,
            lblock_end,
            &mut start_bgid,
        )?;

        if write_end > file_size as usize {
            if write_end > EXT4_MAX_FILE_SIZE as usize {
                return_errno_with_message!(Errno::EFBIG, "file size too large");
            }
            inode_ref.inode.set_size(write_end as u64);
            self.write_back_inode(&mut inode_ref);
        }

        self.collect_block_ranges(&inode_ref, lblock_start, lblock_end - lblock_start)
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
        let generic014_like_write = write_buf.len() == 512 && lblock_end == lblock_start + 1;
        let was_mapped_before = if generic014_like_write {
            match self.get_pblock_idx(&inode_ref, lblock_start) {
                Ok(_) => Some(true),
                Err(err) if err.error() == Errno::ENOENT => Some(false),
                Err(_) => None,
            }
        } else {
            None
        };

        let mut start_bgid = self.initial_write_alloc_bgid(&inode_ref, lblock_start, lblock_end);
        let uses_extents = (inode_ref.inode.flags() & EXT4_INODE_FLAG_EXTENTS as u32) != 0;
        if !uses_extents && inode_ref.inode.is_file() {
            log::warn!(
                "[write_at] regular inode using legacy mapping: inode={} mode={:#o} flags={:#x} size={} blocks={:?}",
                inode_ref.inode_num,
                inode_ref.inode.mode(),
                inode_ref.inode.flags(),
                inode_ref.inode.size(),
                inode_ref.inode.block
            );
        }
        let mapping_result = self.ensure_write_range_mapped(
            &mut inode_ref,
            lblock_start,
            lblock_end,
            &mut start_bgid,
        );

        let allocated_total = mapping_result.map_err(|e| {
            if e.error() == Errno::ENOSPC {
                log::debug!(
                    "[write_at] ensure_write_range_mapped returned ENOSPC: inode={} lblock=[{}, {}) offset={} len={}",
                    inode,
                    lblock_start,
                    lblock_end,
                    offset,
                    write_buf.len()
                );
            } else {
                log::error!(
                    "[write_at] ensure_write_range_mapped failed: inode={} lblock=[{}, {}) offset={} len={} err={:?}",
                    inode,
                    lblock_start,
                    lblock_end,
                    offset,
                    write_buf.len(),
                    e
                );
            }
            e
        })?;
        if generic014_like_write {
            log::debug!(
                "ext4_rs: generic014-like mapping inode={} offset={} lblock={} start_bgid={} was_mapped_before={:?} allocated_blocks={} file_size_before={}",
                inode,
                offset,
                lblock_start,
                start_bgid,
                was_mapped_before,
                allocated_total,
                file_size
            );
        }

        let block_offset = |pblock_idx: Ext4Fsblk| -> Result<usize> {
            let fs_blocks = self.super_block.blocks_count() as u64;
            if pblock_idx >= fs_blocks {
                log::error!(
                    "[write_at] mapped block out of range: inode={} offset={} len={} pblock={} fs_blocks={} uses_extents={} inode_blocks={:?}",
                    inode,
                    offset,
                    write_buf.len(),
                    pblock_idx,
                    fs_blocks,
                    uses_extents,
                    inode_ref.inode.block
                );
                return_errno_with_message!(Errno::EIO, "mapped block out of range");
            }
            let pblock = usize::try_from(pblock_idx).map_err(|_| Ext4Error::new(Errno::EFBIG))?;
            pblock
                .checked_mul(block_size)
                .ok_or_else(|| Ext4Error::new(Errno::EFBIG))
        };

        let mut resolve_pblock = |lblock: u32| -> Result<Ext4Fsblk> {
            self.get_pblock_idx(&inode_ref, lblock).map_err(|e| {
                let root_header = inode_ref.inode.root_extent_header();
                let reloaded = self.get_inode_ref(inode_ref.inode_num);
                let reloaded_root = reloaded.inode.root_extent_header();
                let inmem_tree = self.extent_tree_debug_summary(&inode_ref);
                let reloaded_tree = self.extent_tree_debug_summary(&reloaded);
                let inmem_leaf_entries = self.extent_leaf_debug_entries(&inode_ref, 12);
                let reloaded_leaf_entries = self.extent_leaf_debug_entries(&reloaded, 12);
                log::error!(
                    "[write_at] get_pblock_idx failed after map: inode={} lblock={} offset={} len={} err={:?} inmem_flags={:#x} inmem_size={} inmem_root={{magic:{:x},entries:{},max:{},depth:{}}} reloaded_flags={:#x} reloaded_size={} reloaded_root={{magic:{:x},entries:{},max:{},depth:{}}} inmem_blocks={:?} reloaded_blocks={:?} inmem_tree={} reloaded_tree={} inmem_leaf_entries={} reloaded_leaf_entries={}",
                    inode,
                    lblock,
                    offset,
                    write_buf.len(),
                    e,
                    inode_ref.inode.flags(),
                    inode_ref.inode.size(),
                    root_header.magic,
                    root_header.entries_count,
                    root_header.max_entries_count,
                    root_header.depth,
                    reloaded.inode.flags(),
                    reloaded.inode.size(),
                    reloaded_root.magic,
                    reloaded_root.entries_count,
                    reloaded_root.max_entries_count,
                    reloaded_root.depth,
                    inode_ref.inode.block,
                    reloaded.inode.block,
                    inmem_tree,
                    reloaded_tree,
                    inmem_leaf_entries,
                    reloaded_leaf_entries
                );
                e
            })
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

        let block_size = block_size as u64;
        let new_blocks_cnt = ((new_size + block_size - 1) / block_size) as u32;
        let old_blocks_cnt = ((old_size + block_size - 1) / block_size) as u32;
        let diff_blocks_cnt = old_blocks_cnt - new_blocks_cnt;

        if diff_blocks_cnt > 0 {
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
