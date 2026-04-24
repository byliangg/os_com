use crate::prelude::*;
use crate::return_errno_with_message;
use crate::ext4_defs::*;
use alloc::format;
use core::mem::size_of;
use crate::utils::crc::*;


impl Ext4 {
    fn inode_root_first_index(inode: &Ext4Inode) -> Ext4ExtentIndex {
        Ext4ExtentIndex::load_from_u32(&inode.block[3..])
    }

    fn extent_block_readback_matches(
        &self,
        block_addr: usize,
        expected_entries: u16,
        expected_depth: u16,
        expected_extent_pos: Option<(usize, Ext4Extent)>,
    ) -> bool {
        let block_size = self.super_block.block_size() as usize;
        let block = Block::load(&self.block_device, block_addr * block_size);
        let header: Ext4ExtentHeader = block.read_offset_as(0);
        if header.magic != EXT4_EXTENT_MAGIC
            || header.entries_count != expected_entries
            || header.depth != expected_depth
        {
            return false;
        }

        if let Some((pos, expected)) = expected_extent_pos {
            if pos >= header.entries_count as usize {
                return false;
            }
            let header_size = core::mem::size_of::<Ext4ExtentHeader>();
            let extent_size = core::mem::size_of::<Ext4Extent>();
            let offset = header_size + pos * extent_size;
            let actual: Ext4Extent = block.read_offset_as(offset);
            if actual.first_block != expected.first_block
                || actual.get_pblock() != expected.get_pblock()
                || actual.get_actual_len() != expected.get_actual_len()
                || actual.is_unwritten() != expected.is_unwritten()
            {
                return false;
            }
        }

        true
    }

    fn inode_root_readback_matches(
        &self,
        inode_num: u32,
        expected_header: Ext4ExtentHeader,
        expected_first_index: Option<Ext4ExtentIndex>,
    ) -> bool {
        let reloaded = self.get_inode_ref(inode_num);
        let actual_header = reloaded.inode.root_extent_header();
        if actual_header.magic != expected_header.magic
            || actual_header.entries_count != expected_header.entries_count
            || actual_header.max_entries_count != expected_header.max_entries_count
            || actual_header.depth != expected_header.depth
        {
            return false;
        }

        if let Some(expected_index) = expected_first_index {
            let actual_index = Self::inode_root_first_index(&reloaded.inode);
            if actual_index.first_block != expected_index.first_block
                || actual_index.get_pblock() != expected_index.get_pblock()
            {
                return false;
            }
        }

        true
    }

    fn set_extent_block_checksum_in_block(
        &self,
        inode_ref: &Ext4InodeRef,
        block_addr: usize,
        ext4block: &mut Block,
    ) -> Result<()> {
        let features_ro_compat = self.super_block.features_read_only;
        let has_metadata_checksums = (features_ro_compat & 0x400) != 0;
        if !has_metadata_checksums {
            return Ok(());
        }

        let header = ext4block.read_offset_as::<Ext4ExtentHeader>(0);
        if header.magic != EXT4_EXTENT_MAGIC {
            return_errno_with_message!(Errno::EINVAL, "Invalid extent magic");
        }

        let tail_offset = ext4_extent_tail_offset(&header);
        let data_for_checksum = ext4block.data[..tail_offset].to_vec();
        let checksum =
            self.calculate_extent_block_checksum(inode_ref, &data_for_checksum, block_addr);
        let tail: &mut Ext4ExtentTail = ext4block.read_offset_as_mut(tail_offset);
        tail.et_checksum = checksum;
        Ok(())
    }

    fn update_index_first_block_in_node(
        &self,
        inode_ref: &mut Ext4InodeRef,
        node: &ExtentPathNode,
        pos: usize,
        first_block: u32,
    ) -> Result<()> {
        let block_size = self.super_block.block_size() as usize;
        let idx_size = core::mem::size_of::<Ext4ExtentIndex>();
        let header_size = core::mem::size_of::<Ext4ExtentHeader>();

        if node.pblock_of_node == 0 {
            let entries = node.header.entries_count as usize;
            if pos >= entries {
                return return_errno_with_message!(
                    Errno::EINVAL,
                    "root index position out of range"
                );
            }
            unsafe {
                let header_ptr = inode_ref.inode.block.as_mut_ptr() as *mut Ext4ExtentHeader;
                let index_ptr = header_ptr.add(1) as *mut Ext4ExtentIndex;
                (*index_ptr.add(pos)).first_block = first_block;
            }
            self.write_back_inode(inode_ref);
            return Ok(());
        }

        let mut node_block = Block::load(&self.block_device, node.pblock_of_node * block_size);
        let header: Ext4ExtentHeader = node_block.read_offset_as(0);
        let entries = header.entries_count as usize;
        if pos >= entries {
            return return_errno_with_message!(Errno::EINVAL, "index position out of range");
        }

        let idx_off = header_size + pos * idx_size;
        let idx: &mut Ext4ExtentIndex = node_block.read_offset_as_mut(idx_off);
        idx.first_block = first_block;
        self.set_extent_block_checksum_in_block(inode_ref, node.pblock_of_node, &mut node_block)?;
        node_block.sync_blk_to_disk(&self.metadata_writer);
        Ok(())
    }

    fn propagate_first_block_to_ancestors(
        &self,
        inode_ref: &mut Ext4InodeRef,
        search_path: &SearchPath,
        mut child_level: usize,
        first_block: u32,
    ) -> Result<()> {
        while child_level > 0 {
            let parent_level = child_level - 1;
            let parent_node = &search_path.path[parent_level];
            let parent_pos = parent_node.position;
            self.update_index_first_block_in_node(inode_ref, parent_node, parent_pos, first_block)?;
            if parent_pos != 0 {
                break;
            }
            child_level = parent_level;
        }
        Ok(())
    }

    /// Find an extent in the extent tree.
    ///
    /// Params:
    /// inode_ref: &Ext4InodeRef - inode reference
    /// lblock: Ext4Lblk - logical block id
    ///
    /// Returns:
    /// `Result<SearchPath>` - search path
    ///
    /// If depth > 0, search for the extent_index that corresponds to the target lblock.
    /// If depth = 0, directly search for the extent in the root node that corresponds to the target lblock.
    pub fn find_extent(&self, inode_ref: &Ext4InodeRef, lblock: Ext4Lblk) -> Result<SearchPath> {
        let block_size = self.super_block.block_size() as usize;
        let mut search_path = SearchPath::new();

        // Load the root node
        let root_data: &[u8; 60] =
            unsafe { core::mem::transmute::<&[u32; 15], &[u8; 60]>(&inode_ref.inode.block) };
        let mut node = ExtentNode::load_from_data(root_data, true)?;

        let mut depth = node.header.depth;

        // Traverse down the tree if depth > 0
        let mut pblock_of_node = 0;
        while depth > 0 {
            let index_pos = node.binsearch_idx(lblock);
            if let Some(pos) = index_pos {
                let index = node.get_index(pos)?;
                let next_block = index.get_pblock();
                let next_block_usize = match usize::try_from(next_block) {
                    Ok(v) => v,
                    Err(_) => {
                        return return_errno_with_message!(
                            Errno::EIO,
                            "extent index points to block out of usize range"
                        );
                    }
                };

                search_path.path.push(ExtentPathNode {
                    header: node.header,
                    index: Some(index),
                    extent: None,
                    position: pos,
                    pblock: next_block,
                    pblock_of_node,
                });

                let mut next_data = self
                    .block_device
                    .read_offset(next_block_usize * block_size);
                if next_data.len() < block_size {
                    next_data.resize(block_size, 0);
                } else if next_data.len() > block_size {
                    next_data.truncate(block_size);
                }
                node = ExtentNode::load_from_data_mut(&mut next_data, false)?;
                depth -= 1;
                search_path.depth += 1;
                pblock_of_node = next_block_usize;
            } else {
                return_errno_with_message!(Errno::ENOENT, "Extentindex not found");
            }
        }

        // Handle the case where depth is 0
        if let Some((extent, pos)) = node.binsearch_extent(lblock) {
            search_path.path.push(ExtentPathNode {
                header: node.header,
                index: None,
                extent: Some(extent),
                position: pos,
                pblock: lblock as u64 - extent.get_first_block() as u64 + extent.get_pblock(),
                pblock_of_node,
            });
            search_path.maxdepth = node.header.depth;

            Ok(search_path)
        } else {
            let mut fallback_pos = 0usize;
            let mut fallback_extent = None;
            if let Some(pos) = node.binsearch_extent_pos(lblock) {
                fallback_pos = pos;
                fallback_extent = node.get_extent(pos);
            }

            search_path.path.push(ExtentPathNode {
                header: node.header,
                index: None,
                extent: fallback_extent,
                position: fallback_pos,
                pblock: 0,
                pblock_of_node,
            });
            Ok(search_path)
        }
    }

    /// Insert an extent into the extent tree.
    pub fn insert_extent(
        &self,
        inode_ref: &mut Ext4InodeRef,
        newex: &mut Ext4Extent,
    ) -> Result<()> {
        let newex_first_block = newex.first_block;
        let mut search_path = self.find_extent(inode_ref, newex_first_block)?;
        
        let depth = search_path.depth as usize;
        let node = &search_path.path[depth]; // Get the node at the current depth

        let at_root = node.pblock_of_node == 0;
        let header = node.header;

        // Node is empty (no extents)
        if header.entries_count == 0 {
            self.insert_new_extent(inode_ref, &mut search_path, newex)?;
            return Ok(());
        }

        // Insert to exsiting extent
        if let Some(mut ex) = node.extent {
            let pos = node.position;
            let last_extent_pos = header.entries_count as usize - 1;

            // Try to Insert to found_ext
            // found_ext:   |<---found_ext--->|         |<---ext2--->|
            //              20              30         50          60
            // insert:      |<---found_ext---><---newex--->|         |<---ext2--->|
            //              20              30            40         50          60
            // merge:       |<---newex--->|      |<---ext2--->|
            //              20           40      50          60
            if self.can_merge(&ex, newex) {
                self.merge_extent(&search_path, &mut ex, newex)?;

                if at_root {
                    // we are at root
                    *inode_ref.inode.root_extent_mut_at(node.position) = ex;
                }
                return Ok(());
            }

            // Do not merge with neighbouring extents here.
            //
            // These "insert-left/insert-right" fast paths only hold local copies of the
            // neighbour extents, but `merge_extent()` persists the result using
            // `search_path.path[depth].position`, i.e. the slot of the currently found
            // extent. For non-root leaves that can write the merged extent back to the
            // wrong on-disk entry and silently corrupt the extent tree.
            //
            // Falling through to the regular insertion path is slower but keeps the tree
            // structurally correct until we have a neighbour-aware merge implementation
            // that updates the right slot and removes the consumed neighbour entry.
        }

        // Check if there's space to insert the new extent
        //                full         full
        // Before:   |<---ext1--->|<---ext2--->|
        //           10           20          30

        //                full          full
        // insert:   |<---ext1--->|<---ext2--->|<---newex--->|
        //           10           20           30           35
        if header.entries_count < header.max_entries_count {
            self.insert_new_extent(inode_ref, &mut search_path, newex)?;
        } else {
            self.create_new_leaf(inode_ref, &mut search_path, newex)?;
        }

        Ok(())
    }

    /// Get extent from the node at the given position.
    fn get_extent_from_node(
        &self,
        inode_ref: &mut Ext4InodeRef,
        node: &ExtentPathNode,
        pos: usize,
    ) -> Result<Ext4Extent> {
        if node.pblock_of_node == 0 {
            let entries = node.header.entries_count as usize;
            if pos >= entries {
                return_errno_with_message!(Errno::EINVAL, "extent position out of range in root");
            }
            return Ok(inode_ref.inode.root_extent_at(pos));
        }

        let block_size = self.super_block.block_size() as usize;
        let data = self
            .block_device
            .read_offset(node.pblock_of_node * block_size);
        let mut data = data;
        if data.len() < block_size {
            data.resize(block_size, 0);
        } else if data.len() > block_size {
            data.truncate(block_size);
        }
        let extent_node = ExtentNode::load_from_data(&data, false)?;

        match extent_node.get_extent(pos) {
            Some(extent) => Ok(extent),
            None => return_errno_with_message!(Errno::EINVAL, "Failed to get extent from node"),
        }
    }

    /// Get index from the node at the given position.
    fn get_index_from_node(
        &self,
        inode_ref: &mut Ext4InodeRef,
        node: &ExtentPathNode,
        pos: usize,
    ) -> Result<Ext4ExtentIndex> {
        if node.pblock_of_node == 0 {
            let entries = node.header.entries_count as usize;
            if pos >= entries {
                return_errno_with_message!(Errno::EINVAL, "index position out of range in root");
            }
            let idx = unsafe {
                let header_ptr = inode_ref.inode.block.as_ptr() as *const Ext4ExtentHeader;
                *((header_ptr.add(1) as *const Ext4ExtentIndex).add(pos))
            };
            return Ok(idx);
        }

        let block_size = self.super_block.block_size() as usize;
        let data = self
            .block_device
            .read_offset(node.pblock_of_node * block_size);
        let mut data = data;
        if data.len() < block_size {
            data.resize(block_size, 0);
        } else if data.len() > block_size {
            data.truncate(block_size);
        }
        let extent_node = ExtentNode::load_from_data(&data, false)?;

        extent_node.get_index(pos)
    }


    /// Check if two extents can be merged.
    ///
    /// This function determines whether two extents, `ex1` and `ex2`, can be merged
    /// into a single extent. Extents are contiguous ranges of blocks in the ext4
    /// filesystem that map logical block numbers to physical block numbers.
    ///
    /// # Arguments
    ///
    /// * `ex1` - The first extent to check.
    /// * `ex2` - The second extent to check.
    ///
    /// # Returns
    ///
    /// * `true` if the extents can be merged.
    /// * `false` otherwise.
    ///
    /// # Merge Conditions
    ///
    /// 1. **Same Unwritten State**:
    ///    - The `is_unwritten` state of both extents must be the same.
    ///    - Unwritten extents are placeholders for blocks that are allocated but not initialized.
    ///
    /// 2. **Contiguous Block Ranges**:
    ///    - The logical block range of the first extent must immediately precede
    ///      the logical block range of the second extent.
    ///
    /// 3. **Maximum Length**:
    ///    - The total length of the merged extent must not exceed the maximum allowed
    ///      extent length (`EXT_INIT_MAX_LEN`).
    ///    - If the extents are unwritten, the total length must also not exceed
    ///      the maximum length for unwritten extents (`EXT_UNWRITTEN_MAX_LEN`).
    ///
    /// 4. **Contiguous Physical Blocks**:
    ///    - The physical block range of the first extent must immediately precede
    ///      the physical block range of the second extent. This ensures that the
    ///      physical storage is contiguous.
    fn can_merge(&self, ex1: &Ext4Extent, ex2: &Ext4Extent) -> bool {
        // Check if the extents have the same unwritten state
        if ex1.is_unwritten() != ex2.is_unwritten() {
            return false;
        }
        let ext1_ee_len = ex1.get_actual_len() as usize;
        let ext2_ee_len = ex2.get_actual_len() as usize;
        
        // Check if the block ranges are contiguous
        if ex1.first_block + ext1_ee_len as u32 != ex2.first_block {
            return false;
        }

        // Check if the merged length would exceed the maximum allowed length
        if ext1_ee_len + ext2_ee_len > EXT_INIT_MAX_LEN as usize{
            return false;
        }

        // Check if the physical blocks are contiguous
        if ex1.get_pblock() + ext1_ee_len as u64 == ex2.get_pblock() {
            return true;
        }
        false
    }


    fn merge_extent(
        &self,
        search_path: &SearchPath,
        left_ext: &mut Ext4Extent,
        right_ext: &Ext4Extent,
    ) -> Result<()> {
        let block_size = self.super_block.block_size() as usize;
        let depth = search_path.depth as usize;
        
        let unwritten = left_ext.is_unwritten();
        let len = left_ext.get_actual_len() + right_ext.get_actual_len();
        left_ext.set_actual_len(len);
        if unwritten {
            left_ext.mark_unwritten();
        }
        let header = search_path.path[depth].header;

        if header.max_entries_count > 4 {
            let node = &search_path.path[depth];
            let block = node.pblock_of_node;
            let new_ex_offset = core::mem::size_of::<Ext4ExtentHeader>() + core::mem::size_of::<Ext4Extent>() * (node.position);
            let mut ext4block = Block::load(&self.block_device, block * block_size);
            let left_ext:&mut Ext4Extent = ext4block.read_offset_as_mut(new_ex_offset);

            let unwritten = left_ext.is_unwritten();
            let len = left_ext.get_actual_len() + right_ext.get_actual_len();
            left_ext.set_actual_len(len);
            if unwritten {
                left_ext.mark_unwritten();
            }
            ext4block.sync_blk_to_disk(&self.metadata_writer);
        }

        Ok(())
    }

    fn insert_new_extent(
        &self,
        inode_ref: &mut Ext4InodeRef,
        search_path: &mut SearchPath,
        new_extent: &mut Ext4Extent,
    ) -> Result<()> {
        let block_size = self.super_block.block_size() as usize;
        let depth = search_path.depth as usize;
        let node = &mut search_path.path[depth]; // Get the node at the current depth
        let header = node.header;

        // insert at root
        if depth == 0 {
            // Node is empty (no extents)
            if header.entries_count == 0 {
                *inode_ref.inode.root_extent_mut_at(node.position) = *new_extent;
                inode_ref.inode.root_extent_header_mut().entries_count += 1;

                self.write_back_inode(inode_ref);
                return Ok(());
            }
            // Check if root node is full, need to grow in depth
            if header.entries_count == header.max_entries_count {
                self.ext_grow_indepth(inode_ref)?;
                // After growing, re-insert
                return self.insert_extent(inode_ref, new_extent);
            }

            
            // Not empty, choose insert position by key order.
            let insert_pos = if let Some(cur) = node.extent {
                if new_extent.first_block < cur.first_block {
                    node.position
                } else {
                    node.position + 1
                }
            } else {
                node.position + 1
            };
            let entries = header.entries_count as usize;
            if insert_pos < entries {
                for i in (insert_pos..entries).rev() {
                    let moved = inode_ref.inode.root_extent_at(i);
                    *inode_ref.inode.root_extent_mut_at(i + 1) = moved;
                }
            }
            *inode_ref.inode.root_extent_mut_at(insert_pos) = *new_extent;
            inode_ref.inode.root_extent_header_mut().entries_count += 1;
            self.write_back_inode(inode_ref);
            return Ok(());
        } else {
            // insert at nonroot
            let insert_pos = if let Some(cur) = node.extent {
                if new_extent.first_block < cur.first_block {
                    node.position
                } else {
                    node.position + 1
                }
            } else {
                node.position + 1
            };
            // load block
            let node_block = node.pblock_of_node;
            let mut ext4block =
            Block::load(&self.block_device, node_block * block_size);
            let extent_size = core::mem::size_of::<Ext4Extent>();
            let ext_header_size = core::mem::size_of::<Ext4ExtentHeader>();
            let entries_count = {
                // read_offset_as returns a value, not a reference
                let header: Ext4ExtentHeader = ext4block.read_offset_as(0);
                header.entries_count as usize
            };

            if insert_pos < entries_count {
                let src = ext_header_size + insert_pos * extent_size;
                let dst = ext_header_size + (insert_pos + 1) * extent_size;
                let bytes_to_move = (entries_count - insert_pos) * extent_size;
                ext4block
                    .data
                    .copy_within(src..src + bytes_to_move, dst);
            }

            // insert new extent
            let new_ex_offset = ext_header_size + extent_size * insert_pos;
            let ex: &mut Ext4Extent = ext4block.read_offset_as_mut(new_ex_offset);
            *ex = *new_extent;
            {
                let header: &mut Ext4ExtentHeader = ext4block.read_offset_as_mut(0);

                // update entry count
                header.entries_count += 1;
            }

            // Set the checksum for the updated extent block
            if let Err(e) =
                self.set_extent_block_checksum_in_block(inode_ref, node_block, &mut ext4block)
            {
                log::warn!("[insert_new_extent] Failed to set extent block checksum: {:?}", e);
            }
            ext4block.sync_blk_to_disk(&self.metadata_writer);
            if insert_pos == 0 {
                self.propagate_first_block_to_ancestors(
                    inode_ref,
                    search_path,
                    depth,
                    new_extent.first_block,
                )?;
            }
            
            return Ok(());
        }

        return_errno_with_message!(Errno::ENOTSUP, "Not supported insert extent at nonroot");
    }

    // finds empty index and adds new leaf. if no free index is found, then it requests in-depth growing.
    fn create_new_leaf(
        &self,
        inode_ref: &mut Ext4InodeRef,
        search_path: &mut SearchPath,
        new_extent: &mut Ext4Extent,
    ) -> Result<()> {
        let block_size = self.super_block.block_size() as usize;
        let depth = search_path.depth as usize;

        // Non-root leaf full: split by allocating a sibling leaf and inserting
        // a new index into its parent (currently supports parent at root).
        if depth > 0 {
            let parent = &search_path.path[depth - 1];
            let new_leaf_block = self.balloc_alloc_block(inode_ref, None)?;
            let mut leaf_block = Block::load(&self.block_device, new_leaf_block as usize * block_size);
            leaf_block.data.fill(0);

            let leaf_header = Ext4ExtentHeader::new(
                EXT4_EXTENT_MAGIC,
                1,
                ((block_size - EXT4_EXTENT_HEADER_SIZE) / EXT4_EXTENT_SIZE) as u16,
                0,
                0,
            );
            let leaf_header_bytes = unsafe {
                core::slice::from_raw_parts(
                    &leaf_header as *const _ as *const u8,
                    EXT4_EXTENT_HEADER_SIZE,
                )
            };
            leaf_block.data[..EXT4_EXTENT_HEADER_SIZE].copy_from_slice(leaf_header_bytes);

            let new_extent_bytes = unsafe {
                core::slice::from_raw_parts(
                    new_extent as *const _ as *const u8,
                    EXT4_EXTENT_SIZE,
                )
            };
            leaf_block.data[EXT4_EXTENT_HEADER_SIZE..EXT4_EXTENT_HEADER_SIZE + EXT4_EXTENT_SIZE]
                .copy_from_slice(new_extent_bytes);

            self.set_extent_block_checksum_in_block(
                inode_ref,
                new_leaf_block as usize,
                &mut leaf_block,
            )?;
            leaf_block.sync_blk_to_disk(&self.metadata_writer);

            let insert_pos = if let Some(cur_idx) = parent.index {
                if new_extent.first_block < cur_idx.first_block {
                    parent.position
                } else {
                    parent.position + 1
                }
            } else {
                parent.position + 1
            };

            if parent.pblock_of_node == 0 {
                // Parent is root index node stored in inode body.
                let root_header = inode_ref.inode.root_extent_header();
                if root_header.entries_count >= root_header.max_entries_count {
                    // Root index is full, grow and retry.
                    self.ext_grow_indepth(inode_ref)?;
                    return self.insert_extent(inode_ref, new_extent);
                }

                let parent_entries = root_header.entries_count as usize;
                unsafe {
                    let header_ptr = inode_ref.inode.block.as_mut_ptr() as *mut Ext4ExtentHeader;
                    let index_ptr = header_ptr.add(1) as *mut Ext4ExtentIndex;

                    if insert_pos < parent_entries {
                        for i in (insert_pos..parent_entries).rev() {
                            let moved = *index_ptr.add(i);
                            *index_ptr.add(i + 1) = moved;
                        }
                    }

                    let new_index = index_ptr.add(insert_pos);
                    (*new_index).first_block = new_extent.first_block;
                    (*new_index).store_pblock(new_leaf_block);
                    (*new_index).padding = 0;
                }
                inode_ref.inode.root_extent_header_mut().entries_count += 1;
                self.write_back_inode(inode_ref);
                return Ok(());
            }

            // Parent is a non-root internal node in an extent block.
            let mut parent_block =
                Block::load(&self.block_device, parent.pblock_of_node * block_size);
            let parent_header: Ext4ExtentHeader = parent_block.read_offset_as(0);
            let parent_entries = parent_header.entries_count as usize;
            let parent_max = parent_header.max_entries_count as usize;
            if parent_entries >= parent_max {
                return return_errno_with_message!(
                    Errno::ENOTSUP,
                    "split leaf with full non-root parent is not supported"
                );
            }

            let index_size = core::mem::size_of::<Ext4ExtentIndex>();
            let header_size = core::mem::size_of::<Ext4ExtentHeader>();
            if insert_pos < parent_entries {
                let src = header_size + insert_pos * index_size;
                let dst = header_size + (insert_pos + 1) * index_size;
                let bytes_to_move = (parent_entries - insert_pos) * index_size;
                parent_block
                    .data
                    .copy_within(src..src + bytes_to_move, dst);
            }

            let idx_off = header_size + insert_pos * index_size;
            {
                let new_index: &mut Ext4ExtentIndex = parent_block.read_offset_as_mut(idx_off);
                new_index.first_block = new_extent.first_block;
                new_index.store_pblock(new_leaf_block);
                new_index.padding = 0;
            }
            {
                let header: &mut Ext4ExtentHeader = parent_block.read_offset_as_mut(0);
                header.entries_count += 1;
            }

            self.set_extent_block_checksum_in_block(
                inode_ref,
                parent.pblock_of_node,
                &mut parent_block,
            )?;
            parent_block.sync_blk_to_disk(&self.metadata_writer);
            if insert_pos == 0 {
                self.propagate_first_block_to_ancestors(
                    inode_ref,
                    search_path,
                    depth - 1,
                    new_extent.first_block,
                )?;
            }
            return Ok(());
        }

        // tree is full, time to grow in depth
        self.ext_grow_indepth(inode_ref)?;
        // insert again
        self.insert_extent(inode_ref, new_extent)
    }

    
    // allocates new block
    // moves top-level data (index block or leaf) into the new block
    // initializes new top-level, creating index that points to the
    // just created block
    fn ext_grow_indepth(&self, inode_ref: &mut Ext4InodeRef) -> Result<()>{
        let block_size = self.super_block.block_size() as usize;
        // Allocate new block to store original root node content
        let new_block = self.balloc_alloc_block(inode_ref, None)?;

        // Load new block
        let mut new_ext4block =
            Block::load(&self.block_device, new_block as usize * block_size);

        // Clear new block to ensure no garbage data
        new_ext4block.data.fill(0);

        // Save original root node information
        let old_root_header = inode_ref.inode.root_extent_header();
        let old_depth = old_root_header.depth;
        let old_entries_count = old_root_header.entries_count;
        
        // Get logical block number of first child entry.
        let first_logical_block = if old_entries_count > 0 {
            if old_depth == 0 {
                inode_ref.inode.root_extent_at(0).first_block
            } else {
                let first_idx = Ext4ExtentIndex::load_from_u32(&inode_ref.inode.block[3..]);
                first_idx.first_block
            }
        } else {
            0
        };

        // Copy root node extents data to new block
        // extent start position in inode block is 12 bytes (after header)
        // extent start position in new block is also 12 bytes (after header)
        let header_size = EXT4_EXTENT_HEADER_SIZE;
        
        // Copy header first
        let mut new_header = Ext4ExtentHeader::new(
            EXT4_EXTENT_MAGIC,
            old_entries_count,
            ((block_size - header_size) / EXT4_EXTENT_SIZE) as u16, // Maximum entries the new block can hold
            old_depth, // Preserve old depth in moved subtree root
            0  // generation field, usually 0
        );
        
        // Write header to new block
        let header_bytes = unsafe {
            core::slice::from_raw_parts(
                &new_header as *const _ as *const u8,
                header_size
            )
        };
        new_ext4block.data[..header_size].copy_from_slice(header_bytes);
        
        // Copy extents data
        if old_entries_count > 0 {
            // Copy extents from root block to new block
            // extent start position in inode block is 12 bytes (after header)
            // extent start position in new block is also 12 bytes (after header)
            let root_extents_size = old_entries_count as usize * EXT4_EXTENT_SIZE;
            
            // Use temporary variable to store block data to avoid mutable borrow conflicts
            let block_data = unsafe {
                let block_ptr = inode_ref.inode.block.as_ptr();
                core::slice::from_raw_parts(block_ptr as *const u8, 60)
            };
            
            let root_extents_bytes = &block_data[header_size..header_size + root_extents_size];
            new_ext4block.data[header_size..header_size + root_extents_size]
                .copy_from_slice(root_extents_bytes);
        }
        
        // Set checksum for the new extent block
        if let Err(e) =
            self.set_extent_block_checksum_in_block(inode_ref, new_block as usize, &mut new_ext4block)
        {
            log::warn!("[ext_grow_indepth] Failed to set extent block checksum: {:?}", e);
        }
        new_ext4block.sync_blk_to_disk(&self.metadata_writer);
        
        // First read the block number of the first extent (if any), then update root node
        let first_logical_block_saved = first_logical_block;
        
        // Update root node to be an index node
        {
            let mut root_header = inode_ref.inode.root_extent_header_mut();
            root_header.set_magic(); // Set magic
            root_header.set_entries_count(1); // Index node initially has one entry
            root_header.set_max_entries_count(4); // Root index node typically has 4 entries
            root_header.add_depth(); // Increase depth
        }
        
        // Clear extents data in original root node
        unsafe {
            let root_block_ptr = inode_ref.inode.block.as_mut_ptr() as *mut u8;
            // Skip header part, only clear the extent data after it
            let extents_ptr = root_block_ptr.add(header_size);
            core::ptr::write_bytes(extents_ptr, 0, 60 - header_size);
        }
        
        // Create first index entry in root node pointing to new block
        {
            let mut root_first_index = inode_ref.inode.root_first_index_mut();
            root_first_index.first_block = first_logical_block_saved; // Set starting logical block number
            root_first_index.store_pblock(new_block); // Store physical address of new block
        }

        // Write updated inode back to disk
        self.write_back_inode(inode_ref);
        Ok(())
    }

}

impl Ext4 {
    // Assuming init state
    // depth 0 (root node)
    // +--------+--------+--------+
    // |  idx1  |  idx2  |  idx3  |
    // +--------+--------+--------+
    //     |         |         |
    //     v         v         v
    //
    // depth 1 (internal node)
    // +--------+...+--------+  +--------+...+--------+ ......
    // |  idx1  |...|  idxn  |  |  idx1  |...|  idxn  | ......
    // +--------+...+--------+  +--------+...+--------+ ......
    //     |           |         |             |
    //     v           v         v             v
    //
    // depth 2 (leaf nodes)
    // +--------+...+--------+  +--------+...+--------+  ......
    // | ext1   |...| extn   |  | ext1   |...| extn   |  ......
    // +--------+...+--------+  +--------+...+--------+  ......
    pub fn extent_remove_space(
        &self,
        inode_ref: &mut Ext4InodeRef,
        from: u32,
        to: u32,
    ) -> Result<usize> {
        let block_size = self.super_block.block_size() as usize;
        // log::info!("Remove space from {:x?} to {:x?}", from, to);
        let mut search_path = self.find_extent(inode_ref, from)?;

        // for i in search_path.path.iter() {
        //     log::info!("from Path: {:x?}", i);
        // }

        let depth = search_path.depth as usize;

        /* If we do remove_space inside the range of an extent */
        if let Some(mut ex) = search_path.path[depth].extent {
            if ex.get_first_block() < from
                && to < (ex.get_first_block() + ex.get_actual_len() as u32 - 1)
            {
                let mut newex = Ext4Extent::default();
                let unwritten = ex.is_unwritten();
                let ee_block = ex.first_block;
                let block_count = ex.block_count;
                let newblock = to + 1 - ee_block + ex.get_pblock() as u32;
                ex.block_count = from as u16 - ee_block as u16;

                if unwritten {
                    ex.mark_unwritten();
                }
                newex.first_block = to + 1;
                newex.block_count = (ee_block + block_count as u32 - 1 - to) as u16;
                newex.start_lo = newblock;
                newex.start_hi = ((newblock as u64) >> 32) as u16;

                self.insert_extent(inode_ref, &mut newex)?;

                return Ok(EOK);
            }
        }

        // log::warn!("Remove space in depth: {:x?}", depth);

        let mut i = depth as isize;

        while i >= 0 {
            // we are at the leaf node
            // depth 0 (root node)
            // +--------+--------+--------+
            // |  idx1  |  idx2  |  idx3  |
            // +--------+--------+--------+
            //              |path
            //              v
            //              idx2
            // depth 1 (internal node)
            // +--------+--------+--------+ ......
            // |  idx1  |  idx2  |  idx3  | ......
            // +--------+--------+--------+ ......
            //              |path
            //              v
            //              ext2
            // depth 2 (leaf nodes)
            // +--------+--------+..+--------+
            // | ext1   | ext2   |..|last_ext|
            // +--------+--------+..+--------+
            //            ^            ^
            //            |            |
            //            from         to(exceed last ext, rest of the extents will be removed)
            if i as usize == depth {
                let node_pblock = search_path.path[i as usize].pblock_of_node;

                let header = search_path.path[i as usize].header;
                let entries_count = header.entries_count;

                // we are at root
                if node_pblock == 0 {
                    let first_ex = inode_ref.inode.root_extent_at(0);
                    let last_ex = inode_ref.inode.root_extent_at(entries_count as usize - 1);

                    let mut leaf_from = first_ex.first_block;
                    let mut leaf_to = last_ex.first_block + last_ex.get_actual_len() as u32 - 1;
                    if leaf_from < from {
                        leaf_from = from;
                    }
                    if leaf_to > to {
                        leaf_to = to;
                    }
                    // log::trace!("from {:x?} to {:x?} leaf_from {:x?} leaf_to {:x?}", from, to, leaf_from, leaf_to);
                    self.ext_remove_leaf(inode_ref, &mut search_path, leaf_from, leaf_to)?;

                    i -= 1;
                    continue;
                }
                let ext4block =
                    Block::load(&self.block_device, node_pblock * block_size);

                let header = search_path.path[i as usize].header;
                let entries_count = header.entries_count;

                let first_ex: Ext4Extent = ext4block.read_offset_as(size_of::<Ext4ExtentHeader>());
                let last_ex: Ext4Extent = ext4block.read_offset_as(
                    size_of::<Ext4ExtentHeader>()
                        + size_of::<Ext4Extent>() * (entries_count - 1) as usize,
                );

                let mut leaf_from = first_ex.first_block;
                let mut leaf_to = last_ex.first_block + last_ex.get_actual_len() as u32 - 1;

                if leaf_from < from {
                    leaf_from = from;
                }
                if leaf_to > to {
                    leaf_to = to;
                }
                // log::trace!(
                //     "from {:x?} to {:x?} leaf_from {:x?} leaf_to {:x?}",
                //     from,
                //     to,
                //     leaf_from,
                //     leaf_to
                // );

                self.ext_remove_leaf(inode_ref, &mut search_path, leaf_from, leaf_to)?;

                i -= 1;
                continue;
            }

            // log::trace!("---at level---{:?}\n", i);

            // we are at index
            // example i=1, depth=2
            // depth 0 (root node) - Index node being processed
            // +--------+--------+--------+
            // |  idx1  |  idx2  |  idx3  |
            // +--------+--------+--------+
            //            |path     | Next node to process (more_to_rm?)
            //            v         v
            //           idx2
            //
            // depth 1 (internal node)
            // +--------++--------+...+--------+
            // |  idx1  ||  idx2  |...|  idxn  |
            // +--------++--------+...+--------+
            //            |path
            //            v
            //            ext2
            // depth 2 (leaf nodes)
            // +--------+--------+..+--------+
            // | ext1   | ext2   |..|last_ext|
            // +--------+--------+..+--------+
            let header = search_path.path[i as usize].header;
            if self.more_to_rm(&search_path.path[i as usize], to) {
                // todo
                // load next idx

                // go to this node's child
                i += 1;
            } else {
                if i > 0 {
                    // empty
                    if header.entries_count == 0 {
                        self.ext_remove_idx(inode_ref, &mut search_path, i as u16 - 1)?;
                    }
                }

                let idx = i;
                if idx - 1 < 0 {
                    break;
                }
                i -= 1;
            }
        }

        Ok(EOK)
    }

    pub fn ext_remove_leaf(
        &self,
        inode_ref: &mut Ext4InodeRef,
        path: &mut SearchPath,
        from: u32,
        to: u32,
    ) -> Result<usize> {
        let block_size = self.super_block.block_size() as usize;
        // log::trace!("Remove leaf from {:x?} to {:x?}", from, to);

        // depth 0 (root node)
        // +--------+--------+--------+
        // |  idx1  |  idx2  |  idx3  |
        // +--------+--------+--------+
        //     |         |         |
        //     v         v         v
        //     ^
        //     Current position
        let depth = inode_ref.inode.root_header_depth();
        let mut header = path.path[depth as usize].header;

        /* find where to start removing */
        let pos = path.path[depth as usize].position;
        let entry_count = header.entries_count;

        // depth 1 (internal node)
        // +--------+...+--------+  +--------+...+--------+ ......
        // |  idx1  |...|  idxn  |  |  idx1  |...|  idxn  | ......
        // +--------+...+--------+  +--------+...+--------+ ......
        //     |           |         |             |
        //     v           v         v             v
        //     ^
        //     Current loaded node

        // load node data
        let node_disk_pos = path.path[depth as usize].pblock_of_node * block_size;

        let mut ext4block = if node_disk_pos == 0 {
            // we are at root
            Block::load_inode_root_block(&inode_ref.inode.block)
        } else {
            Block::load(&self.block_device, node_disk_pos)
        };

        // depth 2 (leaf nodes)
        // +--------+...+--------+  +--------+...+--------+  ......
        // | ext1   |...| extn   |  | ext1   |...| extn   |  ......
        // +--------+...+--------+  +--------+...+--------+  ......
        //     ^
        //     Current start extent

        let extent_size = size_of::<Ext4Extent>();
        let extent_area_off = size_of::<Ext4ExtentHeader>();
        let mut write_pos = pos;

        for i in pos..entry_count as usize {
            let offset = extent_area_off + i * extent_size;
            let mut ex: Ext4Extent = ext4block.read_offset_as(offset);

            if ex.first_block > to {
                if write_pos != i {
                    let dst = extent_area_off + write_pos * extent_size;
                    let dst_ex: &mut Ext4Extent = ext4block.read_offset_as_mut(dst);
                    *dst_ex = ex;
                }
                write_pos += 1;
                continue;
            }

            let end = ex.first_block + ex.get_actual_len() as u32 - 1;
            if end < from {
                if write_pos != i {
                    let dst = extent_area_off + write_pos * extent_size;
                    let dst_ex: &mut Ext4Extent = ext4block.read_offset_as_mut(dst);
                    *dst_ex = ex;
                }
                write_pos += 1;
                continue;
            }

            let mut kept = None;
            if ex.first_block < from {
                let remove_from = from;
                let remove_to = end.min(to);
                self.ext_remove_blocks(inode_ref, &mut ex, remove_from, remove_to);
                ex.block_count = (from - ex.first_block) as u16;
                kept = Some(ex);
            } else if end > to {
                let remove_from = ex.first_block;
                let remove_to = to;
                self.ext_remove_blocks(inode_ref, &mut ex, remove_from, remove_to);
                let unwritten = ex.is_unwritten();
                let new_start = to + 1;
                let new_pblock = ex.get_pblock() + (new_start - ex.first_block) as u64;
                ex.first_block = new_start;
                ex.store_pblock(new_pblock);
                ex.block_count = (end - to) as u16;
                if unwritten {
                    ex.mark_unwritten();
                }
                kept = Some(ex);
            } else {
                let remove_from = ex.first_block;
                self.ext_remove_blocks(inode_ref, &mut ex, remove_from, end);
            }

            if let Some(kept_extent) = kept {
                let dst = extent_area_off + write_pos * extent_size;
                let dst_ex: &mut Ext4Extent = ext4block.read_offset_as_mut(dst);
                *dst_ex = kept_extent;
                write_pos += 1;
            }
        }

        for i in write_pos..entry_count as usize {
            let offset = extent_area_off + i * extent_size;
            ext4block.data[offset..offset + extent_size].fill(0);
        }

        let new_entry_count = write_pos as u16;
        header.entries_count = new_entry_count;

        let block_header: &mut Ext4ExtentHeader = ext4block.read_offset_as_mut(0);
        block_header.entries_count = new_entry_count;

        if node_disk_pos == 0 {
            let data = unsafe {
                let ptr = ext4block.data.as_ptr() as *const u32;
                core::slice::from_raw_parts(ptr, 15)
            };
            inode_ref.inode.block.copy_from_slice(data);
            self.write_back_inode(inode_ref);
        } else {
            ext4block.sync_blk_to_disk(&self.metadata_writer);
            if let Err(e) = self.set_extent_block_checksum(inode_ref, path.path[depth as usize].pblock_of_node) {
                log::warn!("Failed to set extent block checksum: {:?}", e);
            }
        }

        /*
         * If the extent pointer is pointed to the first extent of the node, and
         * there's still extents presenting, we may need to correct the indexes
         * of the paths.
         */
        if pos == 0 && new_entry_count > 0 {
            let first_extent: Ext4Extent = ext4block.read_offset_as(extent_area_off);
            self.ext_correct_indexes(inode_ref, path, depth as usize, first_extent.first_block)?;
        }

        /* if this leaf is free, then we should
         * remove it from index block above */
        if new_entry_count == 0 {
            // if we are at root?
            if path.path[depth as usize].pblock_of_node == 0 {
                return Ok(EOK);
            }
            self.ext_remove_idx(inode_ref, path, depth - 1)?;
        } else if depth > 0 {
            // go to next index
            path.path[depth as usize - 1].position += 1;
        }

        Ok(EOK)
    }

    fn ext_remove_index_block(&self, inode_ref: &mut Ext4InodeRef, index: &mut Ext4ExtentIndex) {
        let block_to_free = index.get_pblock();

        // log::trace!("remove index's block {:x?}", block_to_free);
        self.balloc_free_blocks(inode_ref, block_to_free as _, 1);
    }

    fn ext_remove_idx(
        &self,
        inode_ref: &mut Ext4InodeRef,
        path: &mut SearchPath,
        depth: u16,
    ) -> Result<usize> {
        let block_size = self.super_block.block_size() as usize;
        // log::trace!("Remove index at depth {:x?}", depth);

        // Initial state:
        // +--------+--------+--------+
        // |  idx1  |  idx2  |  idx3  |
        // +--------+--------+--------+
        //           ^
        // Current index to remove (pos=1)

        // Removing index:
        // +--------+--------+--------+
        // |  idx1  |[empty] |  idx3  |
        // +--------+--------+--------+
        //           ^

        let i = depth as usize;
        let mut header = path.path[i].header;

        // Get the index block to delete
        let leaf_block = path.path[i].index.unwrap().get_pblock();

        let node_pblock = path.path[i].pblock_of_node;
        let node_disk_pos = node_pblock * block_size;
        let mut ext4block = if node_disk_pos == 0 {
            Block::load_inode_root_block(&inode_ref.inode.block)
        } else {
            Block::load(&self.block_device, node_disk_pos)
        };

        // If current index is not the last one, move subsequent indexes forward
        if path.path[i].position != header.entries_count as usize - 1 {
            let start_pos = size_of::<Ext4ExtentHeader>()
                + path.path[i].position * size_of::<Ext4ExtentIndex>();
            let end_pos = size_of::<Ext4ExtentHeader>()
                + (header.entries_count as usize) * size_of::<Ext4ExtentIndex>();

            let remaining_indexes: Vec<u8> =
                ext4block.data[start_pos + size_of::<Ext4ExtentIndex>()..end_pos].to_vec();
            ext4block.data[start_pos..start_pos + remaining_indexes.len()]
                .copy_from_slice(&remaining_indexes);
            let remaining_size = remaining_indexes.len();

            // Clear the remaining positions
            let empty_start = start_pos + remaining_size;
            let empty_end = end_pos;
            ext4block.data[empty_start..empty_end].fill(0);
        }

        // Update the entries_count in the header
        header.entries_count -= 1;
        let block_header: &mut Ext4ExtentHeader = ext4block.read_offset_as_mut(0);
        block_header.entries_count = header.entries_count;

        if node_disk_pos == 0 {
            // If the last root index is removed, collapse the root back to an
            // empty leaf header. Leaving depth=1 with zero index entries makes
            // later lookups fail with "Extentindex not found" instead of
            // treating the inode as having an empty extent tree.
            if header.entries_count == 0 {
                let root_header = inode_ref.inode.root_extent_header_mut();
                root_header.magic = EXT4_EXTENT_MAGIC;
                root_header.entries_count = 0;
                root_header.max_entries_count = 4;
                root_header.depth = 0;
                root_header.generation = 0;

                unsafe {
                    let root_block_ptr = inode_ref.inode.block.as_mut_ptr() as *mut u8;
                    let extents_ptr = root_block_ptr.add(size_of::<Ext4ExtentHeader>());
                    core::ptr::write_bytes(extents_ptr, 0, 60 - size_of::<Ext4ExtentHeader>());
                }

                self.write_back_inode(inode_ref);
                self.ext_remove_index_block(inode_ref, &mut path.path[i].index.unwrap());
                return Ok(EOK);
            }

            let data = unsafe {
                let ptr = ext4block.data.as_ptr() as *const u32;
                core::slice::from_raw_parts(ptr, 15)
            };
            inode_ref.inode.block.copy_from_slice(data);
            self.write_back_inode(inode_ref);
        } else {
            ext4block.sync_blk_to_disk(&self.metadata_writer);
            if let Err(e) = self.set_extent_block_checksum(inode_ref, node_pblock) {
                log::warn!("Failed to set extent block checksum: {:?}", e);
            }
        }

        // Free the index block
        self.ext_remove_index_block(inode_ref, &mut path.path[i].index.unwrap());

        if path.path[i].position == 0 && header.entries_count > 0 {
            let first_block = if node_disk_pos == 0 {
                Ext4ExtentIndex::load_from_u32(&inode_ref.inode.block[3..]).first_block
            } else {
                let first_index: Ext4ExtentIndex =
                    ext4block.read_offset_as(size_of::<Ext4ExtentHeader>());
                first_index.first_block
            };
            self.ext_correct_indexes(inode_ref, path, i, first_block)?;
        }

        Ok(EOK)
    }

    /// Correct parent indexes after the first entry of a child node changes.
    fn ext_correct_indexes(
        &self,
        inode_ref: &mut Ext4InodeRef,
        path: &SearchPath,
        child_level: usize,
        first_block: u32,
    ) -> Result<usize> {
        if child_level > 0 {
            self.propagate_first_block_to_ancestors(inode_ref, path, child_level, first_block)?;
        }
        Ok(EOK)
    }

    fn ext_remove_blocks(
        &self,
        inode_ref: &mut Ext4InodeRef,
        ex: &mut Ext4Extent,
        from: u32,
        to: u32,
    ) {
        let len = to - from + 1;
        let num = from - ex.first_block;
        let start: u32 = ex.get_pblock() as u32 + num;
        self.balloc_free_blocks(inode_ref, start as _, len);
    }

    pub fn more_to_rm(&self, path: &ExtentPathNode, to: u32) -> bool {
        let block_size = self.super_block.block_size() as usize;
        let header = path.header;

        // No Sibling exists
        if header.entries_count == 1 {
            return false;
        }

        let pos = path.position;
        if pos > header.entries_count as usize - 1 {
            return false;
        }

        // Check if index is out of bounds
        if let Some(index) = path.index {
            let last_index_pos = header.entries_count as usize - 1;
            let node_disk_pos = path.pblock_of_node * block_size;
            let ext4block = Block::load(&self.block_device, node_disk_pos);
            let last_index: Ext4ExtentIndex =
                ext4block.read_offset_as(size_of::<Ext4ExtentIndex>() * last_index_pos);

            if path.position > last_index_pos || index.first_block > last_index.first_block {
                return false;
            }

            // Check if index's first_block is greater than 'to'
            if index.first_block > to {
                return false;
            }
        }

        true
    }
}

impl Ext4 {
    /// Calculate and set the extent block checksum in the extent tail
    fn set_extent_block_checksum(&self, inode_ref: &Ext4InodeRef, block_addr: usize) -> Result<()> {
        let block_size = self.super_block.block_size() as usize;
        let mut ext4block = Block::load(&self.block_device, block_addr * block_size);
        self.set_extent_block_checksum_in_block(inode_ref, block_addr, &mut ext4block)?;
        ext4block.sync_blk_to_disk(&self.metadata_writer);
        Ok(())
    }
    
    /// Calculate the checksum for an extent block
    fn calculate_extent_block_checksum(&self, inode_ref: &Ext4InodeRef, data: &[u8], block_addr: usize) -> u32 {
        let mut checksum = 0;
        
        // If metadata checksums are not enabled, return 0
        let features_ro_compat = self.super_block.features_read_only;
        // EXT4_FEATURE_RO_COMPAT_METADATA_CSUM is typically 0x400
        let has_metadata_checksums = (features_ro_compat & 0x400) != 0;
        
        if !has_metadata_checksums {
            return 0;
        }
        
        // Get UUID from superblock
        let uuid = &self.super_block.uuid;
        
        // Calculate checksum - first using UUID
        checksum = ext4_crc32c(EXT4_CRC32_INIT, uuid, uuid.len() as u32);
        
        // Add inode number to checksum
        let ino_index = inode_ref.inode_num;
        checksum = ext4_crc32c(checksum, &ino_index.to_le_bytes(), 4);
        
        // Add inode generation to checksum
        let ino_gen = inode_ref.inode.generation;
        checksum = ext4_crc32c(checksum, &ino_gen.to_le_bytes(), 4);
        
        // Finally add the extent block data
        checksum = ext4_crc32c(checksum, data, data.len() as u32);
        
        checksum
    }

}

/// Calculate the offset of the extent tail
pub fn ext4_extent_tail_offset(header: &Ext4ExtentHeader) -> usize {
    size_of::<Ext4ExtentHeader>() + 
    (header.max_entries_count as usize * size_of::<Ext4Extent>())
}
