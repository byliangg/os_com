// SPDX-License-Identifier: MPL-2.0

//! Ext4 linear directory read operations: `lookup` and `readdir`.
//!
//! A directory inode is page-cache-backed like a regular file; these methods
//! read its data blocks through the page cache and parse linear directory
//! entries. The htree index (Phase 6) only accelerates lookup; `readdir`
//! always walks blocks in physical order.

mod dir_entry;
mod hash;

use ostd::sync::RwMutexWriteGuard;

use self::dir_entry::{
    DIR_TAIL_LEN, DOT_BYTE, DOT_DOT_BYTE, DirBlockView, DirEntryFileType, DirEntryHeader,
    EXT4_FT_DIR_CSUM,
};
use super::{
    super::{checksum, fs::Ext4, journal, prelude::*, utils},
    FileFlags, FilePerm, Inode, InodeInner, InodeSeed, MAX_LINK_COUNT,
};
use crate::fs::utils::NAME_MAX;

/// A candidate slot found by [`InodeInner::find_dir_slot`] or freshly created
/// by [`InodeInner::grow_dir_block`], where a new entry can be written.
#[derive(Clone, Copy, Debug)]
struct DirSlotInfo {
    /// Byte offset of the slot within the directory
    /// (`block_idx * BLOCK_SIZE + offset_in_block`).
    dir_offset: usize,
    /// Current `rec_len` of the candidate slot.
    slot_rec_len: usize,
    /// Minimal occupied length of the existing entry head (0 if the slot is a
    /// free entry or a freshly grown empty block).
    used_rec_len: usize,
}

/// A located, live directory entry returned by [`InodeInner::find_entry_info`],
/// describing where it sits so it can be deleted.
#[derive(Clone, Copy, Debug)]
struct DirEntryInfo {
    /// Inode number of the located entry. Read by the unlink/rmdir path to
    /// fetch the child inode; the delete primitive itself only needs the offset
    /// and record length.
    ino: Ext4Ino,
    /// Byte offset of the entry within the directory.
    dir_offset: usize,
    /// `rec_len` of the entry.
    entry_rec_len: usize,
}

impl InodeInner {
    /// Iterates entries from byte `offset`, feeding each active entry to
    /// `visitor`. Returns the number of bytes advanced.
    fn readdir_at(&self, offset: usize, visitor: &mut dyn DirentVisitor) -> Result<usize> {
        if self.desc.type_() != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }
        let size = self.file_size();
        let min_rec_len = DirEntryHeader::min_rec_len(1) as usize;
        if size < min_rec_len || offset > size - min_rec_len {
            return Ok(0);
        }

        let start_block = offset / BLOCK_SIZE;
        let mut current_offset = offset;
        let mut advanced = 0usize;
        let total_blocks = size.div_ceil(BLOCK_SIZE);
        let page_cache = self.page_cache()?;

        for block_idx in start_block..total_blocks {
            let block_offset = block_idx * BLOCK_SIZE;
            if block_offset >= size {
                break;
            }
            let block = DirBlockView::from_index(page_cache, block_idx, size);
            let mut iter = block.iter_entries();
            while let Some((entry_offset_in_block, entry)) = iter.next_entry()? {
                let entry_offset = block_offset + entry_offset_in_block;
                let rec_len = entry.header.rec_len as usize;
                let next_offset = entry_offset + rec_len;

                // Skip entries already reported before `offset`.
                if next_offset <= current_offset {
                    continue;
                }
                if entry_offset < current_offset {
                    current_offset = next_offset;
                    continue;
                }

                let ino = entry.header.ino;
                if ino != 0 {
                    let name = core::str::from_utf8(entry.name)
                        .map_err(|_| Error::with_message(Errno::EIO, "invalid dir entry name"))?;
                    let file_type = DirEntryFileType::try_from(entry.header.file_type)
                        .unwrap_or(DirEntryFileType::Unknown);
                    let inode_type = InodeType::from(file_type);
                    if visitor
                        .visit(name, ino as u64, inode_type, next_offset)
                        .is_err()
                    {
                        return Ok(current_offset - offset);
                    }
                }
                current_offset = next_offset;
            }
            advanced = current_offset - offset;
        }
        Ok(advanced)
    }

    /// Finds a reusable slot for a directory entry name of `name_len` bytes.
    ///
    /// Returns `None` when no existing block has reusable space; the caller then
    /// grows the directory. A returned slot is either a free entry (`ino == 0`)
    /// or the spare tail of a live entry that can be split.
    ///
    /// Directory-index seam (Phase 6 htree): together with
    /// [`Self::find_entry_info`] this is the only linear scan; when the `INDEX`
    /// flag is set the htree index will pick the target leaf block here instead
    /// of walking blocks in physical order.
    fn find_dir_slot(&self, name_len: usize) -> Result<Option<DirSlotInfo>> {
        if self.desc.type_() != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let new_rec_len = DirEntryHeader::min_rec_len(name_len) as usize;
        debug_assert!(new_rec_len <= BLOCK_SIZE);

        let file_size = self.file_size();
        let data_blocks = file_size.div_ceil(BLOCK_SIZE);
        let page_cache = self.page_cache()?;

        for block_idx in 0..data_blocks {
            let block_offset = block_idx * BLOCK_SIZE;
            let block = DirBlockView::from_index(page_cache, block_idx, file_size);
            let mut iter = block.iter_entries();

            while let Some((entry_offset, header)) = iter.next_entry_header()? {
                // The checksum tail is not free space: never split or reuse it.
                if header.file_type == EXT4_FT_DIR_CSUM {
                    continue;
                }
                let ino = header.ino;
                let rec_len = header.rec_len as usize;

                let used_rec_len = if ino == 0 {
                    0
                } else {
                    DirEntryHeader::min_rec_len(header.name_len as usize) as usize
                };

                // A free entry can be reused; a live entry can be split.
                if (ino == 0 && rec_len >= new_rec_len)
                    || (ino != 0 && rec_len >= used_rec_len + new_rec_len)
                {
                    return Ok(Some(DirSlotInfo {
                        dir_offset: block_offset + entry_offset,
                        slot_rec_len: rec_len,
                        used_rec_len,
                    }));
                }
            }
        }

        Ok(None)
    }

    /// Grows the directory by exactly one filesystem block and returns a slot
    /// spanning the whole new block.
    ///
    /// Unlike ext2 (which allocates an indirect-mapped block), ext4 directory
    /// data is extent-mapped: this routes through the Phase-2 extent write path
    /// (`prepare_write` → `ExtentManager::ensure_allocated`) to map and allocate
    /// the new logical block, then publishes the larger `file_size`. The journal
    /// seam for the block *allocation* is already threaded inside
    /// `ensure_allocated`; the entry bytes written here go through the page
    /// cache as data.
    ///
    /// The new block must not be left zeroed: a zero `rec_len` would make the
    /// entry iterator spin forever. It is therefore initialized as a single
    /// empty entry (`ino == 0`, `rec_len == BLOCK_SIZE`) spanning the block.
    fn grow_dir_block(
        &mut self,
        fs: &Ext4,
        handle: Option<&journal::Handle>,
    ) -> Result<DirSlotInfo> {
        if self.desc.type_() != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let old_size = self.file_size();
        let new_size = old_size + BLOCK_SIZE;

        // Map and allocate the new logical block through the extent engine.
        self.prepare_write(fs, old_size, new_size, handle)?;

        // Initialize the new block as one empty entry spanning the whole block
        // before publishing the new size, so any reader that observes the grown
        // size sees a well-formed (empty) entry chain rather than zeros.
        let payload_len = self.dir_payload_len();
        let has_csum = self.csum_seed.is_some();
        let init_result = (|| -> Result<()> {
            let page_cache = self.page_cache()?;
            page_cache.fill_zeros(old_size..new_size)?;
            let block = DirBlockView::create_view(page_cache, old_size, BLOCK_SIZE);
            // The empty entry spans the usable payload; on a metadata_csum volume
            // the last `DIR_TAIL_LEN` bytes are the checksum tail, so the entry
            // chain stops short of them.
            let empty_header = DirEntryHeader {
                ino: 0,
                rec_len: (payload_len as u16).to_le(),
                name_len: 0,
                file_type: DirEntryFileType::Unknown as u8,
            };
            block.write_entry(0, empty_header, &[])?;
            if has_csum {
                // Reserve the checksum tail as a fake `EXT4_FT_DIR_CSUM` entry;
                // `det_checksum` is filled by `seal_dir_block` at journal time.
                block.write_entry(payload_len, DirEntryHeader::dir_tail(), &[])?;
            }
            Ok(())
        })();

        if let Err(err) = init_result {
            self.rollback_write(old_size, new_size, handle);
            return Err(err);
        }

        // `prepare_write` allocated the new block UNWRITTEN (Unwritten-first);
        // its full contents (the empty-entry chain) are now in the page cache,
        // so convert it to written in this transaction. A directory block that
        // stayed unwritten would read back as zeros — a lost, fsck-inconsistent
        // directory. The whole block was `fill_zeros`ed, so no stale sub-range
        // is exposed.
        let start_block = (old_size / BLOCK_SIZE) as Iblock;
        if let Err(err) = self
            .extent_manager()
            .and_then(|em| em.mark_range_written(start_block, start_block + 1, handle))
        {
            self.rollback_write(old_size, new_size, handle);
            return Err(err);
        }

        self.set_file_size(new_size);

        Ok(DirSlotInfo {
            dir_offset: old_size,
            slot_rec_len: payload_len,
            used_rec_len: 0,
        })
    }

    /// Usable payload length of a directory block: the whole block, minus the
    /// [`DIR_TAIL_LEN`]-byte checksum tail on a `metadata_csum` volume. Real
    /// entries pack into `[0, dir_payload_len())`; the fake tail entry occupies
    /// the rest.
    fn dir_payload_len(&self) -> usize {
        BLOCK_SIZE
            - if self.csum_seed.is_some() {
                DIR_TAIL_LEN
            } else {
                0
            }
    }

    /// Recomputes and writes a directory block's `metadata_csum` tail into the
    /// page cache (Linux `ext4_dirent_csum_set`): crc32c of the block up to the
    /// last word, stored in `det_checksum`. A no-op when the feature is off.
    fn seal_dir_block(&self, logical: Iblock) -> Result<()> {
        let Some(seed) = self.csum_seed else {
            return Ok(());
        };
        // The checksum covers the block up to the tail entry — the whole payload,
        // EXCLUDING all `DIR_TAIL_LEN` tail bytes (the fake entry's 8-byte header
        // and its 4-byte det_checksum): Linux `ext4_dirblock_csum` runs over
        // `(char *)EXT4_DIRENT_TAIL - b_data == blocksize - sizeof(tail)`. The
        // result is stored in det_checksum, the block's last word.
        const CSUM_COVER: usize = BLOCK_SIZE - DIR_TAIL_LEN;
        const DET_CHECKSUM_OFFSET: usize = BLOCK_SIZE - size_of::<u32>();
        let page_cache = self.page_cache()?;
        let block_offset = logical as usize * BLOCK_SIZE;
        let block: [u8; BLOCK_SIZE] = page_cache.read_val(block_offset).map_err(|_| {
            Error::with_message(Errno::EIO, "failed to read directory block for checksum")
        })?;
        let csum = checksum::crc32c(seed.get(), &block[..CSUM_COVER]);
        page_cache.write_val(block_offset + DET_CHECKSUM_OFFSET, &csum.to_le())?;
        Ok(())
    }

    /// Captures a directory block's after-image into the operation's transaction
    /// after it has been modified in the page cache.
    ///
    /// Directory blocks are metadata in ext4, so a namespace operation must
    /// journal the block it edits atomically with the inode / bitmap changes it
    /// makes — otherwise a crash could leave a committed inode allocation with a
    /// torn or missing directory entry (a dangling or lost name). `dir_offset` is
    /// any byte offset within the modified block.
    ///
    /// A whole-block capture: `get_create_access` seeds zeros (irrelevant, since
    /// the `dirty_metadata` closure overwrites the whole block with the page
    /// cache's current content), so no device read is issued for a block we fully
    /// replace.
    fn journal_dir_block(&self, dir_offset: usize, handle: Option<&journal::Handle>) -> Result<()> {
        let logical = (dir_offset / BLOCK_SIZE) as Iblock;
        // Seal the checksum tail (into the page cache) whether or not the volume
        // is journaled, so the block that later reaches disk — via checkpoint of
        // this capture, or a direct page-cache flush — carries a valid checksum.
        self.seal_dir_block(logical)?;
        if handle.is_none() {
            return Ok(());
        }
        let Some(phys) = self.extent_manager()?.map_blocks(logical)?.mapped_pblock() else {
            return_errno_with_message!(Errno::EIO, "directory block not mapped for journaling");
        };
        let block: [u8; BLOCK_SIZE] = self
            .page_cache()?
            .read_val(logical as usize * BLOCK_SIZE)
            .map_err(|_| {
                Error::with_message(Errno::EIO, "failed to read directory block for journaling")
            })?;
        journal::get_create_access(handle, phys, journal::TriggerType::DirBlock)?
            .patch(|buf| buf.copy_from_slice(&block))
    }

    /// Writes a new entry into the selected slot, splitting the predecessor's
    /// `rec_len` first when reusing the spare tail of a live entry.
    fn add_entry(
        &mut self,
        slot: &DirSlotInfo,
        name: &str,
        ino: Ext4Ino,
        file_type: DirEntryFileType,
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        debug_assert_ne!(ino, 0);

        let name_bytes = name.as_bytes();
        // The VFS resolver already rejects names over `NAME_MAX`; revalidate at
        // the single write boundary (every entry insertion funnels through
        // here) so the truncating `name_len: u8` encode below can never
        // disagree with the written name bytes. Mirrors the read-side check in
        // `dir_entry.rs`.
        if name_bytes.len() > NAME_MAX {
            return_errno_with_message!(Errno::ENAMETOOLONG, "directory entry name is too long");
        }
        let new_rec_len = DirEntryHeader::min_rec_len(name_bytes.len()) as usize;
        debug_assert!(new_rec_len <= slot.slot_rec_len);

        let page_cache = self.page_cache()?;
        let mut entry_offset = slot.dir_offset;
        let mut entry_rec_len = slot.slot_rec_len;
        if slot.used_rec_len != 0 {
            debug_assert!(slot.used_rec_len < slot.slot_rec_len);
            // Splitting a live entry: shrink the predecessor's `rec_len` to its
            // minimal occupied length first, then write the new entry in the
            // reclaimed tail.
            let prev_view =
                DirBlockView::create_view(page_cache, slot.dir_offset, slot.slot_rec_len);
            prev_view.set_rec_len(0, slot.used_rec_len as u16)?;
            entry_offset = slot.dir_offset + slot.used_rec_len;
            entry_rec_len = slot.slot_rec_len - slot.used_rec_len;
        }

        let view = DirBlockView::create_view(page_cache, entry_offset, entry_rec_len);
        let header = DirEntryHeader {
            ino: ino.to_le(),
            rec_len: (entry_rec_len as u16).to_le(),
            name_len: name_bytes.len() as u8,
            file_type: file_type as u8,
        };
        view.write_entry(0, header, name_bytes)?;
        // The predecessor split (if any) and the new entry both land in the slot's
        // block; journal that one block's after-image.
        self.journal_dir_block(slot.dir_offset, handle)?;
        Ok(())
    }

    /// Inserts a new directory entry, growing the directory by one block when no
    /// existing block has a reusable slot.
    fn add_new_entry(
        &mut self,
        fs: &Ext4,
        name: &str,
        ino: Ext4Ino,
        file_type: DirEntryFileType,
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        let slot = match self.find_dir_slot(name.len())? {
            Some(slot) => slot,
            None => self.grow_dir_block(fs, handle)?,
        };
        self.add_entry(&slot, name, ino, file_type, handle)
    }

    /// Repoints the live entry named `name` at a new inode and file type in
    /// place. Used by rename to replace an existing destination name rather than
    /// adding a second entry. Mirrors ext2 `InodeInner::overwrite_entry`.
    fn overwrite_entry(
        &mut self,
        name: &str,
        new_ino: Ext4Ino,
        new_file_type: DirEntryFileType,
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        let entry_info = self.find_entry_info(name)?;
        self.set_entry_target(&entry_info, new_ino, new_file_type, handle)
    }

    /// Repoints a located entry at a new inode and file type. Used by rename to
    /// overwrite a destination name and to update a moved directory's `..` so it
    /// points at its new parent. Mirrors ext2 `InodeInner::set_entry_target`.
    fn set_entry_target(
        &mut self,
        entry: &DirEntryInfo,
        new_ino: Ext4Ino,
        new_file_type: DirEntryFileType,
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        let block_idx = entry.dir_offset / BLOCK_SIZE;
        let entry_offset = entry.dir_offset - block_idx * BLOCK_SIZE;

        let block = DirBlockView::from_index(self.page_cache()?, block_idx, self.file_size());
        block.set_inode(entry_offset, new_ino)?;
        block.set_file_type(entry_offset, new_file_type)?;
        self.journal_dir_block(entry.dir_offset, handle)?;
        Ok(())
    }

    /// Locates a live entry by name, recording where it sits for deletion.
    fn find_entry_info(&self, name: &str) -> Result<DirEntryInfo> {
        if self.desc.type_() != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let file_size = self.file_size();
        let name_bytes = name.as_bytes();
        let page_cache = self.page_cache()?;

        for block_idx in 0..file_size.div_ceil(BLOCK_SIZE) {
            let block_offset = block_idx * BLOCK_SIZE;
            let block = DirBlockView::from_index(page_cache, block_idx, file_size);
            let mut iter = block.iter_entries();
            while let Some((entry_offset, entry)) = iter.next_entry()? {
                let ino = entry.header.ino;
                if ino == 0 || entry.name != name_bytes {
                    continue;
                }
                return Ok(DirEntryInfo {
                    ino,
                    dir_offset: block_offset + entry_offset,
                    entry_rec_len: entry.header.rec_len as usize,
                });
            }
        }

        return_errno!(Errno::ENOENT)
    }

    /// Deletes a located entry by zeroing its inode and merging its space into
    /// the predecessor entry. The first entry in a block (always `.`) has no
    /// predecessor and is never the delete target.
    fn delete_entry(
        &mut self,
        target: &DirEntryInfo,
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        let block_idx = target.dir_offset / BLOCK_SIZE;
        let entry_offset = target.dir_offset - block_idx * BLOCK_SIZE;

        let block = DirBlockView::from_index(self.page_cache()?, block_idx, self.file_size());
        block.delete_entry(entry_offset, target.entry_rec_len)?;
        self.journal_dir_block(target.dir_offset, handle)?;
        Ok(())
    }

    /// Initializes a freshly created directory's first block with `.` and `..`.
    ///
    /// `.` points to the directory itself (`ino`) and `..` to its parent
    /// (`parent_ino`); `..` spans the rest of the block. The parent's link
    /// count bump lives in the create path (Task 3), not here.
    fn make_empty(
        &mut self,
        fs: &Ext4,
        ino: Ext4Ino,
        parent_ino: Ext4Ino,
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        // Grow the empty directory by its first block; this maps and allocates
        // the block (rolling back on its own failure) and initializes it as one
        // empty entry spanning the block.
        let slot = self.grow_dir_block(fs, handle)?;
        debug_assert_eq!(slot.dir_offset, 0);

        // Overwrite the empty entry with `.` (this dir) followed by `..` (the
        // parent), with `..` spanning the rest of the block. If a write fails
        // here the block stays mapped and the size published; the create path
        // (Task 4) clears the link count so `Drop` reclaims the inode.
        let dot_len = DirEntryHeader::min_rec_len(DOT_BYTE.len()) as usize;
        let page_cache = self.page_cache()?;
        let block = DirBlockView::from_index(page_cache, 0, self.file_size());

        let dot_header = DirEntryHeader {
            ino: ino.to_le(),
            rec_len: (dot_len as u16).to_le(),
            name_len: DOT_BYTE.len() as u8,
            file_type: DirEntryFileType::Dir as u8,
        };
        block.write_entry(0, dot_header, DOT_BYTE)?;

        // `..` spans the rest of the usable payload (the slot from
        // `grow_dir_block` already excludes the checksum tail).
        let dot_dot_header = DirEntryHeader {
            ino: parent_ino.to_le(),
            rec_len: ((slot.slot_rec_len - dot_len) as u16).to_le(),
            name_len: DOT_DOT_BYTE.len() as u8,
            file_type: DirEntryFileType::Dir as u8,
        };
        block.write_entry(dot_len, dot_dot_header, DOT_DOT_BYTE)?;

        // Journal the initialized first block (`.`/`..`).
        self.journal_dir_block(0, handle)?;
        Ok(())
    }

    /// Returns whether this directory holds only the `.` and `..` entries.
    fn empty_dir(&self, self_ino: Ext4Ino) -> bool {
        if self.desc.type_() != InodeType::Dir {
            return false;
        }

        let file_size = self.file_size();
        let data_blocks = file_size.div_ceil(BLOCK_SIZE);
        let Ok(page_cache) = self.page_cache() else {
            return false;
        };

        for block_idx in 0..data_blocks {
            let block = DirBlockView::from_index(page_cache, block_idx, file_size);
            let mut iter = block.iter_entries();

            loop {
                let entry = match iter.next_entry() {
                    Ok(Some((_, entry))) => entry,
                    Ok(None) => break,
                    Err(_) => return false,
                };

                if entry.header.ino == 0 {
                    continue;
                }

                let name = entry.name;
                if name == DOT_BYTE {
                    if entry.header.ino != self_ino {
                        return false;
                    }
                    continue;
                }
                if name == DOT_DOT_BYTE {
                    continue;
                }
                return false;
            }
        }

        true
    }
}

impl Inode {
    /// Looks up a child entry by name and reads its inode.
    pub(in crate::fs::fs_impls::ext4) fn lookup(&self, name: &str) -> Result<Arc<Inode>> {
        let ino = self.inner.read().find_entry_info(name)?.ino;
        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem dropped"))?;
        fs.read_inode(ino)
    }

    /// Iterates directory entries from `offset`, feeding them to `visitor`.
    pub(in crate::fs::fs_impls::ext4) fn readdir_at(
        &self,
        offset: usize,
        visitor: &mut dyn DirentVisitor,
    ) -> Result<usize> {
        self.inner.read().readdir_at(offset, visitor)
    }

    /// Creates a child inode and directory entry under this directory.
    ///
    /// Only regular files, directories, and symlinks are supported; special
    /// files (devices, FIFOs, sockets) are deferred to a later phase and rejected
    /// with `EINVAL` (P3 plan §10.4). A freshly created symlink starts empty and
    /// extent-flagged; `write_link` later adjusts its payload/flag.
    ///
    /// Mirrors ext2 `Inode::create`.
    pub(in crate::fs::fs_impls::ext4) fn create(
        &self,
        name: &str,
        type_: InodeType,
        perm: FilePerm,
    ) -> Result<Arc<Inode>> {
        let seed = match type_ {
            InodeType::File | InodeType::Dir | InodeType::SymLink => InodeSeed::ExtentRoot,
            // FIFOs and sockets (a Unix socket bind arrives here as a plain
            // `create`) carry no data and no device number.
            InodeType::NamedPipe | InodeType::Socket => InodeSeed::Nothing,
            // Devices need a device number; they arrive via `mknod` →
            // `create_with_device`.
            _ => return_errno!(Errno::EINVAL),
        };
        self.create_with_seed(name, type_, perm, seed)
    }

    /// Creates a character/block device node ([`create`](Self::create) for devices): the device
    /// number is encoded into the fresh inode's `i_block` inside the creating
    /// transaction, so no crash window can leave a device node with rdev 0
    /// (ext2 sets the id in a second step; Linux ext4 initializes it under the
    /// same handle, like this).
    pub(in crate::fs::fs_impls::ext4) fn create_with_device(
        &self,
        name: &str,
        type_: InodeType,
        perm: FilePerm,
        device_id: u64,
    ) -> Result<Arc<Inode>> {
        if !matches!(type_, InodeType::CharDevice | InodeType::BlockDevice) {
            return_errno!(Errno::EINVAL);
        }
        self.create_with_seed(name, type_, perm, InodeSeed::Device(device_id))
    }

    /// Creates a symlink together with its target in **one transaction**
    /// (Linux `ext4_symlink`): the crash harness proved the two-step VFS
    /// default (`create` + `write_link`) persists a target-less symlink —
    /// a state fsck rejects — whenever a commit boundary lands between the
    /// steps.
    pub(in crate::fs::fs_impls::ext4) fn create_symlink(
        &self,
        name: &str,
        perm: FilePerm,
        target: &str,
    ) -> Result<Arc<Inode>> {
        // Same bound `write_link` enforces; checked before allocating anything.
        if target.len() >= BLOCK_SIZE {
            return_errno!(Errno::ENAMETOOLONG);
        }
        self.create_with_seed_and_init(
            name,
            InodeType::SymLink,
            perm,
            InodeSeed::ExtentRoot,
            |child, fs, handle| {
                let mut child_inner = child.inner.write();
                let wrote_slow_target = child_inner.write_link(fs, target, handle)?;
                // data=ordered for a slow target (see `Inode::write_link`).
                if wrote_slow_target
                    && let Some(handle) = handle
                    && let Ok(pages) = child_inner.page_cache()
                {
                    handle.register_ordered_data(
                        child.ino(),
                        Arc::downgrade(child),
                        pages.clone(),
                        child_inner.file_size(),
                    )?;
                }
                Ok(())
            },
        )
    }

    fn create_with_seed(
        &self,
        name: &str,
        type_: InodeType,
        perm: FilePerm,
        seed: InodeSeed,
    ) -> Result<Arc<Inode>> {
        self.create_with_seed_and_init(name, type_, perm, seed, |_, _, _| Ok(()))
    }

    fn create_with_seed_and_init(
        &self,
        name: &str,
        type_: InodeType,
        perm: FilePerm,
        seed: InodeSeed,
        init_child: impl FnOnce(&Arc<Inode>, &Arc<Ext4>, Option<&journal::Handle>) -> Result<()>,
    ) -> Result<Arc<Inode>> {
        let is_dir = type_ == InodeType::Dir;
        let dir_entry_file_type = DirEntryFileType::from(type_);

        // Find a slot before creating the child inode to avoid wasting an inode
        // allocation if the directory cannot accept a new entry. The VFS dentry
        // layer has already validated that `name` is absent.
        let fs = self.fs()?;
        let mut parent_inner = self.inner.write();
        // Open the journal handle after the inner lock (lock order: inner ① →
        // handle ②); it captures the inode/block-bitmap, group-descriptor and
        // extent after-images the allocations below dirty, and closes on drop.
        let op = fs.begin_op(Ext4::CREATE_CREDITS)?;
        let slot = match parent_inner.find_dir_slot(name.len())? {
            Some(slot) => slot,
            None => parent_inner.grow_dir_block(&fs, op.get())?,
        };

        // The new inode is not yet visible in the inode cache until
        // `insert_inode` below. This is safe because the VFS dentry layer holds
        // an `upread` guard on the children set, preventing concurrent `create` /
        // `lookup_via_fs` on this directory, so a concurrent `lookup_via_fs`
        // won't build a second `Arc<Inode>` from the on-disk desc and insert it.
        let child = fs.create_inode(self.ino, type_, perm, seed, op.get())?;
        let child_ino = child.ino();

        // Taking `child.inner.write()` while holding `parent_inner.write()` does
        // not need ino-ordering: the child is brand-new and unpublished (absent
        // from the inode cache, invisible to other threads), so no other thread
        // can hold or contend its `inner` lock.
        let result = if is_dir {
            child
                .inner
                .write()
                .make_empty(&fs, child_ino, self.ino, op.get())
                .and_then(|_| {
                    parent_inner.add_entry(&slot, name, child_ino, dir_entry_file_type, op.get())
                })
        } else {
            // Type-specific initialization (a symlink's target) runs inside
            // this same transaction, *before* the name goes live: the entry
            // must never point at a half-built inode, in memory or on disk.
            init_child(&child, &fs, op.get()).and_then(|_| {
                parent_inner.add_entry(&slot, name, child_ino, dir_entry_file_type, op.get())
            })
        };

        if let Err(err) = result {
            // Clear the link count so other resources are reclaimed by `Drop`
            // (Task 4). The half-built inode is never inserted into the cache.
            {
                let mut child_inner = child.inner.write();
                child_inner.set_link_count(0);
            }
            // Close this operation's handle BEFORE `child` drops at scope end:
            // the Drop-reclaim opens its own `begin_op`, and with the blocking
            // `journal_start` a nested open against a full transaction would
            // wait for a commit that cannot happen while our handle pins the
            // running transaction — a self-deadlock (review finding).
            drop(op);
            return Err(err);
        }

        // Link the child dir's `..` back to this parent.
        if is_dir {
            parent_inner.inc_link_count(1);
        }
        parent_inner.set_mtime_ctime(utils::now());
        // Journaled: persist the parent's link-count (for a subdir) and timestamp
        // change in *this* operation's transaction, atomically with the new entry
        // and the child inode — otherwise a crash after commit but before the
        // deferred fsync would leave the parent's link count too low (e2fsck
        // "link count wrong"). Non-journaled keeps the buffered writeback.
        //
        // The CHILD descriptor is re-captured too: `create_inode` journaled
        // its v0 (size 0, seed-only i_block), but `make_empty` / `init_child`
        // may have grown it since — a directory's "."/".." block and extent
        // root, a symlink's target. Committing only v0 persists a name that
        // points at a half-built inode (the crash matrix reconstructed a
        // zero-length directory whose journaled dir block leaked as an
        // unreferenced bitmap bit). For plain files the re-capture is an
        // idempotent no-op patch.
        if op.get().is_some() {
            parent_inner.write_back_inode_desc(&fs, self.ino, op.get())?;
            child
                .inner
                .write()
                .write_back_inode_desc(&fs, child_ino, op.get())?;
        }
        fs.insert_inode(child.clone());
        Ok(child)
    }

    /// Removes a non-directory entry from this directory.
    ///
    /// On the link count reaching 0 the inode is dropped from the cache and
    /// reclaimed by the last surviving `Arc` (see the drop-order note below).
    /// Mirrors ext2 `Inode::unlink`.
    pub(in crate::fs::fs_impls::ext4) fn unlink(&self, name: &str) -> Result<()> {
        let entry_info = {
            let parent_inner = self.inner.read();
            parent_inner.find_entry_info(name)?
        };
        let fs = self.fs()?;

        // CRITICAL drop ordering (mirrors ext2): `child` is declared *before*
        // `guards`, so at scope end Rust drops `guards` first (reverse
        // declaration order), releasing `child.inner.write()` before `child`
        // itself drops. If `child` is the last `Arc` (no fd holds it open) its
        // `Drop` runs `try_reclaim_deleted_inode`, which takes
        // `child.inner.write()`; were the guard still held this would self-
        // deadlock. Do NOT reorder these two locals.
        let child = fs.read_inode(entry_info.ino)?;

        // The `DirDentry.children` lock in the VFS layer keeps the parent
        // directory entry stable during this operation, so we only need to lock
        // all related inodes in order, without rechecking the lookup result.
        let mut guards = MultiInodeInnerGuards::lock(&[self, child.as_ref()]);
        // Handle after the inner locks (inner ① → handle ②), and declared *after*
        // `guards` so it drops first — this operation's transaction closes before
        // `guards` releases and before `child`'s Drop opens its own reclaim
        // transaction (keeping the two transactions separate).
        let op = fs.begin_op(Ext4::UNLINK_CREDITS)?;

        let child_inner = guards.inner_mut(child.ino());
        if child_inner.inode_type() == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }

        let parent_inner = guards.inner_mut(self.ino());
        parent_inner.delete_entry(&entry_info, op.get())?;
        parent_inner.set_mtime_ctime(utils::now());

        // Update timestamps before dropping the target link count.
        let child_inner = guards.inner_mut(child.ino());
        child_inner.set_ctime(utils::now());
        child_inner.dec_link_count(1);
        let reached_zero = child_inner.link_count() == 0;
        if reached_zero {
            // Link the fully unlinked inode onto the on-disk orphan list and
            // persist the previous head in its `i_dtime` + the zeroed link
            // count, all in this transaction, so a crash between it and the
            // (separate) reclaim transaction leaves a chain recovery can
            // finish. Ordered handle ② → `s_orphan_lock` → superblock ⑤; a
            // no-op (empty link) without a journal.
            let link = fs.orphan_add(child.ino(), op.get())?;
            let child_inner = guards.inner_mut(child.ino());
            child_inner.persist_as_orphan(&fs, entry_info.ino, link, op.get())?;
            // Drop the cache's `Arc`; if an fd still holds one the inode stays
            // alive until that last `Arc` drops, then `Drop` reclaims it. We do
            // NOT force reclaim here — refcount + `Drop` handle unlink-of-open.
            let _ = fs.remove_inode(entry_info.ino);
        } else if op.get().is_some() {
            // Persist the surviving hard link's (crash-consistent) link count
            // in this transaction; non-journaled defers to fsync, as before.
            let child_inner = guards.inner_mut(child.ino());
            child_inner.write_back_inode_desc(&fs, entry_info.ino, op.get())?;
        }
        Ok(())
    }

    /// Removes an empty sub-directory.
    ///
    /// Mirrors ext2 `Inode::rmdir`. The same `child`-before-`guards` drop
    /// ordering as [`unlink`](Self::unlink) is required and observed here.
    pub(in crate::fs::fs_impls::ext4) fn rmdir(&self, name: &str) -> Result<()> {
        let entry_info = {
            let parent_inner = self.inner.read();
            parent_inner.find_entry_info(name)?
        };
        let fs = self.fs()?;

        // CRITICAL drop ordering: `child` declared before `guards` so the multi-
        // inode lock is released before `child` drops and its `Drop` reclaim
        // re-takes `child.inner.write()`. See `unlink` for the full rationale.
        let child = fs.read_inode(entry_info.ino)?;

        // The `DirDentry.children` lock in the VFS layer keeps the parent
        // directory entry stable during this operation, so we only need to lock
        // all related inodes in order, without rechecking the lookup result.
        let mut guards = MultiInodeInnerGuards::lock(&[self, child.as_ref()]);
        // Handle after the inner locks; declared after `guards` (see `unlink`).
        let op = fs.begin_op(Ext4::UNLINK_CREDITS)?;

        let child_inner = guards.inner_mut(child.ino());
        if child_inner.inode_type() != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }
        if !child_inner.empty_dir(child.ino()) {
            return_errno!(Errno::ENOTEMPTY);
        }

        child_inner.set_ctime(utils::now());
        // The child loses its own `.` self-link and the parent's directory entry.
        child_inner.dec_link_count(2);
        if child_inner.link_count() == 0 {
            // Link onto the orphan list and persist the child (`i_dtime` =
            // orphan-next pointer) in this transaction (see `unlink`).
            let link = fs.orphan_add(child.ino(), op.get())?;
            let child_inner = guards.inner_mut(child.ino());
            child_inner.persist_as_orphan(&fs, entry_info.ino, link, op.get())?;
            let _ = fs.remove_inode(entry_info.ino);
        }

        let parent_inner = guards.inner_mut(self.ino());
        parent_inner.delete_entry(&entry_info, op.get())?;
        // The parent loses the `..` reference the removed child held back to it.
        parent_inner.dec_link_count(1);
        parent_inner.set_mtime_ctime(utils::now());
        // Journaled: persist the parent's link-count drop in this transaction (see
        // `create`).
        if op.get().is_some() {
            parent_inner.write_back_inode_desc(&fs, self.ino(), op.get())?;
        }

        Ok(())
    }

    /// Adds a hard link in this directory to an existing inode.
    ///
    /// The VFS layer rejects hard links to directories (with `EPERM`) before
    /// reaching here, so — like ext2 — this does not re-check the type. It
    /// rejects only an overflowing link count (`EMLINK`, as Linux ext4 does).
    /// The two inodes (`self` and `old`) are locked through
    /// [`MultiInodeInnerGuards`] in ino order.
    pub(in crate::fs::fs_impls::ext4) fn link(&self, old: &Inode, name: &str) -> Result<()> {
        let fs = self.fs()?;
        let dir_entry_file_type = DirEntryFileType::from(old.inode_type());
        let mut guards = MultiInodeInnerGuards::lock(&[self, old]);
        // Handle after the inner locks (inner ① → handle ②).
        let op = fs.begin_op(Ext4::LINK_CREDITS)?;

        if guards.inner(old.ino()).link_count() >= MAX_LINK_COUNT {
            return_errno!(Errno::EMLINK);
        }

        let dir_inner = guards.inner_mut(self.ino());
        let slot = match dir_inner.find_dir_slot(name.len())? {
            Some(slot) => slot,
            None => dir_inner.grow_dir_block(&fs, op.get())?,
        };
        dir_inner.add_entry(&slot, name, old.ino(), dir_entry_file_type, op.get())?;
        dir_inner.set_mtime_ctime(utils::now());
        // Journaled: persist the directory inode (its size/i_blocks change if the
        // entry grew a new block) atomically with the entry.
        if op.get().is_some() {
            dir_inner.write_back_inode_desc(&fs, self.ino(), op.get())?;
        }

        let old_inner = guards.inner_mut(old.ino());
        old_inner.set_ctime(utils::now());
        old_inner.inc_link_count(1);
        // Journaled: persist the linked inode's incremented link count in this
        // transaction (crash-consistent). Non-journaled defers to fsync.
        if op.get().is_some() {
            old_inner.write_back_inode_desc(&fs, old.ino(), op.get())?;
        }
        Ok(())
    }

    /// Renames or moves the entry `old_name` in this directory to `new_name` in
    /// the `target` directory (`target` may be `self`).
    ///
    /// The four-phase algorithm mirrors ext2 `Inode::rename`:
    ///
    /// 1. Resolve, under read locks, the inode of `old_name` (must exist) and of
    ///    `new_name` if it is being replaced.
    /// 2. Take the participating inodes' `inner` write locks in ino order through
    ///    [`MultiInodeInnerGuards`].
    /// 3. Validate the rename invariants ([`validate_rename_invariants`]).
    /// 4. Apply the directory-entry and link-count mutations
    ///    ([`apply_dir_mutations`]).
    ///
    /// Loop prevention (a directory moved inside its own subtree) is enforced by
    /// the syscall layer, so it is not re-checked here (P3 plan §10.3).
    ///
    /// [`validate_rename_invariants`]: Self::validate_rename_invariants
    /// [`apply_dir_mutations`]: Self::apply_dir_mutations
    pub(in crate::fs::fs_impls::ext4) fn rename(
        &self,
        old_name: &str,
        target: &Inode,
        new_name: &str,
    ) -> Result<()> {
        let fs = self.fs()?;
        let is_same_dir = self.ino() == target.ino();
        if is_same_dir && old_name == new_name {
            return Ok(());
        }

        // Step 1: read the source entry without write locks — the ino tells us
        // which inodes to lock in step 2, and the full `DirEntryInfo` feeds the
        // cross-directory delete (the VFS `DirDentry.children` lock keeps the
        // entry stable across the gap, the same argument unlink/rmdir rely on).
        let old_info = self.inner.read().find_entry_info(old_name)?;
        let old_ino = old_info.ino;

        // CRITICAL drop ordering (mirrors ext2; same hazard as unlink/rmdir):
        // `old_inode` and `replaced_inode` are declared *before* `guards`, so at
        // scope end Rust drops `guards` first (reverse declaration order),
        // releasing every held `inner.write()` before these `Arc`s drop. If the
        // replaced inode's link count hit 0 below and its last `Arc` is here, its
        // `Drop` reclaim re-takes `inner.write()`; were a guard still held this
        // would self-deadlock. Do NOT reorder these locals after `guards`.
        let old_inode = fs.read_inode(old_ino)?;
        let replaced_inode = {
            let target_inner = target.inner.read();
            target_inner
                .find_entry_info(new_name)
                .ok()
                .map(|entry_info| fs.read_inode(entry_info.ino))
                .transpose()?
        };

        // The `DirDentry.children` lock in the VFS layer keeps both directory
        // entries stable during this operation, so we only need to lock all
        // related inodes in order, without rechecking the lookup results.
        // Step 2: lock all participating inodes in global ino order. A duplicate
        // (e.g. same-dir rename where `self == target`, or `replaced_inode`
        // absent) is deduplicated by `MultiInodeInnerGuards::lock`.
        let mut guards = MultiInodeInnerGuards::lock(&[
            self as &Inode,
            target,
            old_inode.as_ref(),
            replaced_inode.as_deref().unwrap_or(old_inode.as_ref()),
        ]);
        // Handle after the inner locks; declared after `guards` so it drops first,
        // before `guards` releases and before a replaced inode's Drop reclaim (see
        // unlink).
        let op = fs.begin_op(Ext4::RENAME_CREDITS)?;

        // Step 3: validate invariants under lock.
        self.validate_rename_invariants(&guards, &old_inode, replaced_inode.as_deref())?;

        // Step 4: apply directory mutations and metadata updates.
        self.apply_dir_mutations(
            &mut guards,
            target,
            old_name,
            &old_info,
            &old_inode,
            replaced_inode.as_deref(),
            new_name,
            op.get(),
        )?;

        Ok(())
    }

    /// Validates the rename invariants under the locks taken by [`rename`].
    ///
    /// Mirrors ext2 `validate_rename_invariants`:
    /// - if the moved inode is a directory, its `..` must still point at the
    ///   source parent (`self`), else the on-disk state is corrupt (`EIO`);
    /// - overwrite constraints when `new_name` already exists: directory onto a
    ///   non-directory is `ENOTDIR`, non-directory onto a directory is `EISDIR`,
    ///   and directory onto a non-empty directory is `ENOTEMPTY`.
    ///
    /// [`rename`]: Self::rename
    fn validate_rename_invariants(
        &self,
        guards: &MultiInodeInnerGuards,
        old_inode: &Inode,
        replaced_inode: Option<&Inode>,
    ) -> Result<()> {
        // Step 3.1: sanity-check that the moved directory's `..` still points at
        // the source parent. A mismatch indicates on-disk corruption; bail out
        // before we silently write a wrong `..` update.
        if old_inode.inode_type() == InodeType::Dir {
            let old_inner = guards.inner(old_inode.ino());
            let parent_ino = old_inner.find_entry_info("..")?.ino;
            if parent_ino != self.ino() {
                return_errno_with_message!(Errno::EIO, "dotdot entry inconsistent with source dir");
            }
        }

        // Step 3.2: validate the overwrite constraints.
        if let Some(replaced) = replaced_inode {
            let replaced_is_dir = replaced.inode_type() == InodeType::Dir;
            let old_is_dir = old_inode.inode_type() == InodeType::Dir;
            if old_is_dir && !replaced_is_dir {
                return_errno!(Errno::ENOTDIR);
            }
            if !old_is_dir && replaced_is_dir {
                return_errno!(Errno::EISDIR);
            }
            if replaced_is_dir {
                let replaced_inner = guards.inner(replaced.ino());
                if !replaced_inner.empty_dir(replaced.ino()) {
                    return_errno!(Errno::ENOTEMPTY);
                }
            }
        }

        Ok(())
    }

    /// Applies the directory-entry and link-count mutations for [`rename`].
    ///
    /// Mirrors ext2 `apply_dir_mutations`. The directory-entry mutation differs
    /// between the same-directory and cross-directory cases; the moved
    /// directory's `..` is repointed at `target` on a cross-directory directory
    /// move; the replaced inode's link count is dropped (by 2 if it is a
    /// directory — losing its own `.` and the entry — else by 1) and the inode
    /// reclaimed if it reaches 0.
    ///
    /// [`rename`]: Self::rename
    // The rename mutation genuinely involves this many distinct participants (the
    // lock set, both directories, the moved and replaced inodes, the names, and
    // the journal handle); bundling them into a struct would only obscure the flow.
    #[expect(clippy::too_many_arguments)]
    fn apply_dir_mutations(
        &self,
        guards: &mut MultiInodeInnerGuards,
        target: &Inode,
        old_name: &str,
        old_info: &DirEntryInfo,
        old_inode: &Inode,
        replaced_inode: Option<&Inode>,
        new_name: &str,
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        let old_is_dir = old_inode.inode_type() == InodeType::Dir;
        let has_replaced = replaced_inode.is_some();
        let old_ino = old_inode.ino();
        let is_same_dir = self.ino() == target.ino();
        let moved_file_type = DirEntryFileType::from(old_inode.inode_type());
        let fs = self.fs()?;

        // Step 4.1: apply the directory-entry mutations.
        if is_same_dir {
            let dir_inner = guards.inner_mut(self.ino());
            if has_replaced {
                dir_inner.overwrite_entry(new_name, old_ino, moved_file_type, handle)?;
            } else {
                dir_inner.add_new_entry(&fs, new_name, old_ino, moved_file_type, handle)?;
            }
            // Re-read the source entry — the ONE place the step-1
            // `DirEntryInfo` cannot be trusted: `add_new_entry` mutated THIS
            // directory and may have split the source entry (shrinking its
            // `rec_len`). The cross-directory branch below keeps the step-1
            // token instead, because there the source directory is untouched.
            let old_info = dir_inner.find_entry_info(old_name)?;
            dir_inner.delete_entry(&old_info, handle)?;
            // Replacing a directory with a directory in the same parent: the
            // parent loses the replaced directory's `..` back-reference.
            if old_is_dir && has_replaced {
                dir_inner.dec_link_count(1);
            }
            dir_inner.set_mtime_ctime(utils::now());
            // Journaled: persist the directory inode (link count / size) in this
            // transaction, atomically with its entry mutations.
            if handle.is_some() {
                dir_inner.write_back_inode_desc(&fs, self.ino(), handle)?;
            }
        } else {
            let target_inner = guards.inner_mut(target.ino());
            if has_replaced {
                target_inner.overwrite_entry(new_name, old_ino, moved_file_type, handle)?;
            } else {
                target_inner.add_new_entry(&fs, new_name, old_ino, moved_file_type, handle)?;
            }
            // Moving a directory into a fresh name in `target`: `target` gains the
            // moved directory's new `..` back-reference. When replacing, the slot
            // already counted that reference, so `target`'s count is unchanged.
            if old_is_dir && !has_replaced {
                target_inner.inc_link_count(1);
            }
            target_inner.set_mtime_ctime(utils::now());
            // Journaled: persist the destination directory (link count / size).
            if handle.is_some() {
                target_inner.write_back_inode_desc(&fs, target.ino(), handle)?;
            }

            let source_inner = guards.inner_mut(self.ino());
            // The step-1 `DirEntryInfo` is still valid here: only `target` was
            // mutated above, the source directory is untouched, and the VFS
            // `DirDentry.children` lock kept the entry stable across the
            // read-lock → write-lock gap (the same token-trust unlink/rmdir
            // already exercise). No re-walk needed.
            source_inner.delete_entry(old_info, handle)?;
            // Moving a directory out of `self`: `self` loses the moved
            // directory's `..` back-reference.
            if old_is_dir {
                source_inner.dec_link_count(1);
            }
            source_inner.set_mtime_ctime(utils::now());
            // Journaled: persist the source directory's link-count drop.
            if handle.is_some() {
                source_inner.write_back_inode_desc(&fs, self.ino(), handle)?;
            }
        }

        // Step 4.2: drop the replaced inode's link count and reclaim it if it
        // reaches 0.
        if let Some(replaced) = replaced_inode {
            let replaced_inner = guards.inner_mut(replaced.ino());
            replaced_inner.set_ctime(utils::now());
            // A replaced directory loses both its own `.` self-link and the
            // entry; a replaced non-directory loses only the entry.
            if old_is_dir {
                replaced_inner.dec_link_count(1);
            }
            replaced_inner.dec_link_count(1);

            if replaced_inner.link_count() == 0 {
                // Link onto the orphan list and persist the replaced inode
                // (`i_dtime` = orphan-next pointer) in this transaction (see
                // `unlink`).
                let link = fs.orphan_add(replaced.ino(), handle)?;
                let replaced_inner = guards.inner_mut(replaced.ino());
                replaced_inner.persist_as_orphan(&fs, replaced.ino(), link, handle)?;
                // Drop the cache's `Arc`. If an fd still holds one the inode stays
                // alive until that last `Arc` (here in the caller's locals, dropped
                // after `guards`) drops, then `Drop` reclaims it.
                let _ = fs.remove_inode(replaced.ino());
            }
        }

        // Step 4.3: repoint a moved directory's `..` at its new parent.
        let old_inner = guards.inner_mut(old_ino);
        if old_is_dir && !is_same_dir {
            let dotdot_entry_info = old_inner.find_entry_info("..")?;
            old_inner.set_entry_target(
                &dotdot_entry_info,
                target.ino(),
                DirEntryFileType::Dir,
                handle,
            )?;
            // The htree index would describe the now-stale block layout; the
            // moved directory must be re-indexed on its next insert (P6).
            old_inner.remove_flags(FileFlags::INDEX);
            old_inner.set_mtime_ctime(utils::now());
        } else {
            old_inner.set_ctime(utils::now());
        }
        // Journaled: persist the moved inode's descriptor change in this
        // transaction — the cleared INDEX flag on a cross-directory directory
        // move (else its ctime). Otherwise a crash after commit but before the
        // deferred fsync would leave `EXT4_INDEX_FL` set on a directory whose
        // block was rewritten linear (e2fsck "htree corrupted"). This is the last
        // rename participant; every other one is already written back above.
        if handle.is_some() {
            old_inner.write_back_inode_desc(&fs, old_ino, handle)?;
        }

        Ok(())
    }
}

const MAX_MULTI_INODE_LOCKS: usize = 4;

/// A guard holding up to [`MAX_MULTI_INODE_LOCKS`] inodes' `inner.write()` locks
/// at once, acquired in ascending ino order with duplicates removed.
///
/// This is the global multi-inode locking primitive (report §5.1): operations
/// that touch several inodes (rmdir/unlink now; rename in Task 5) take their
/// `inner` write locks through here so every path acquires them in the same
/// order, preventing deadlock. Mirrors ext2 `MultiInodeInnerGuards`.
struct MultiInodeInnerGuards<'a> {
    entries: [Option<(Ext4Ino, RwMutexWriteGuard<'a, InodeInner>)>; MAX_MULTI_INODE_LOCKS],
    len: usize,
}

impl<'a> MultiInodeInnerGuards<'a> {
    /// Acquires `inner.write()` locks on deduplicated inodes in ascending ino
    /// order.
    fn lock(inodes: &[&'a Inode]) -> Self {
        let mut sorted_inodes: [Option<&'a Inode>; MAX_MULTI_INODE_LOCKS] =
            [None; MAX_MULTI_INODE_LOCKS];
        let count = inodes.len().min(MAX_MULTI_INODE_LOCKS);
        for (i, inode) in inodes.iter().take(count).enumerate() {
            sorted_inodes[i] = Some(*inode);
        }
        sorted_inodes[..count].sort_by_key(|opt| opt.unwrap().ino);

        let mut entries: [Option<(Ext4Ino, RwMutexWriteGuard<'a, InodeInner>)>;
            MAX_MULTI_INODE_LOCKS] = [None, None, None, None];
        let mut len = 0;
        let mut prev_ino = None;
        for slot in sorted_inodes.iter().take(count).flatten() {
            if prev_ino == Some(slot.ino) {
                continue;
            }
            prev_ino = Some(slot.ino);
            entries[len] = Some((slot.ino, slot.inner.write()));
            len += 1;
        }
        Self { entries, len }
    }

    /// Returns a shared reference to the held `inner` of inode `ino`.
    fn inner(&self, ino: Ext4Ino) -> &InodeInner {
        let (_, guard) = self.entries[..self.len]
            .iter()
            .flatten()
            .find(|(entry_ino, _)| *entry_ino == ino)
            .expect("requested inode inner lock must be held");
        guard
    }

    /// Returns a mutable reference to the held `inner` of inode `ino`.
    fn inner_mut(&mut self, ino: Ext4Ino) -> &mut InodeInner {
        let (_, guard) = self.entries[..self.len]
            .iter_mut()
            .flatten()
            .find(|(entry_ino, _)| *entry_ino == ino)
            .expect("requested inode inner lock must be held");
        guard
    }
}

#[cfg(ktest)]
mod tests {
    use alloc::{
        format,
        string::{String, ToString},
        sync::Arc,
        vec::Vec,
    };

    use aster_block::BLOCK_SIZE;
    use ostd::{
        mm::{VmIo, VmReader},
        prelude::*,
    };

    use super::{
        super::super::test_utils::{
            Ext4FixtureBuilder, make_dir_block, make_dir_inode, make_file_inode,
        },
        DirEntryFileType, Inode,
    };
    use crate::{
        fs::{file::InodeType, utils::DirentVisitor},
        prelude::{Errno, Error, Result},
    };

    /// The `metadata_csum` directory tail is a fake `EXT4_FT_DIR_CSUM` entry an
    /// old kernel skips as deleted, and the block checksum covers the block up to
    /// its last word (`det_checksum`). Integration (tail reservation on grow,
    /// skip in slot search, seal correctness) is validated by the guest e2fsck on
    /// a real metadata_csum image, since a checksummed volume mounts only after
    /// the admission task.
    #[ktest]
    fn dir_tail_marker_and_checksum_formula() {
        use super::{DIR_TAIL_LEN, DirEntryHeader, EXT4_FT_DIR_CSUM, checksum};

        let tail = DirEntryHeader::dir_tail();
        assert_eq!(tail.ino, 0);
        assert_eq!(u16::from_le(tail.rec_len) as usize, DIR_TAIL_LEN);
        assert_eq!(tail.name_len, 0);
        assert_eq!(tail.file_type, EXT4_FT_DIR_CSUM);
        assert_eq!(BLOCK_SIZE - DIR_TAIL_LEN, 4084);

        // The checksum covers the block EXCLUDING the whole 12-byte tail (Linux
        // `ext4_dirblock_csum` covers `blocksize - sizeof(ext4_dir_entry_tail)`).
        let mut block = [0u8; BLOCK_SIZE];
        block[..8].copy_from_slice(b"DIRDATA!");
        let seed = 0xD1E5;
        const COVER: usize = BLOCK_SIZE - DIR_TAIL_LEN;
        let csum = checksum::crc32c(seed, &block[..COVER]);
        // Neither the tail's 8 header bytes nor its det_checksum word are covered.
        let mut probe = block;
        probe[COVER..].copy_from_slice(&[0xAB; DIR_TAIL_LEN]);
        assert_eq!(checksum::crc32c(seed, &probe[..COVER]), csum);
        // A covered byte is.
        probe[0] ^= 1;
        assert_ne!(checksum::crc32c(seed, &probe[..COVER]), csum);
    }

    #[ktest]
    fn lookup_and_readdir() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();

        // A directory at ino 12 backed by data block 101 with three entries.
        let dir_block = 101u32;
        let block = make_dir_block(&[(2, ".", 2), (2, "..", 2), (11, "hello.txt", 1)]);
        f.write_data_block(dir_block, &block);
        f.write_raw_inode(12, &make_dir_inode(dir_block));
        // The looked-up file inode must exist to be read back.
        f.write_raw_inode(11, &make_file_inode(100, 0));

        let dir = f.ext4.read_inode(12).unwrap();

        let child = dir.lookup("hello.txt").unwrap();
        assert_eq!(child.ino(), 11);
        assert_eq!(child.inode_type(), InodeType::File);
        assert!(dir.lookup("nonexistent").is_err());

        let mut names: Vec<String> = Vec::new();
        dir.readdir_at(0, &mut names).unwrap();
        assert_eq!(names.len(), 3);
        assert_eq!(names[0], ".");
        assert_eq!(names[1], "..");
        assert_eq!(names[2], "hello.txt");
    }

    /// A visitor that simulates a `getdents` buffer holding `cap` entries: it
    /// records each entry, then fails (as a full user buffer would) once `cap`
    /// entries have been accepted in the current call.
    struct CappedVisitor {
        cap: usize,
        used: usize,
        names: Vec<String>,
    }

    impl DirentVisitor for CappedVisitor {
        fn visit(
            &mut self,
            name: &str,
            _ino: u64,
            _type_: InodeType,
            _offset: usize,
        ) -> Result<()> {
            if self.used >= self.cap {
                return Err(Error::with_message(Errno::EINVAL, "simulated buffer full"));
            }
            self.used += 1;
            self.names.push(name.to_string());
            Ok(())
        }
    }

    /// Drives `readdir_at` exactly the way `InodeHandle::readdir` does — advancing
    /// the directory offset by the returned byte count across multiple calls.
    /// This exercises offset resumption that the single-call test above cannot,
    /// mirroring a real mke2fs root layout (`.`, `..`, `lost+found`, files).
    #[ktest]
    fn readdir_resumes_across_getdents_calls() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let dir_block = 101u32;
        let block = make_dir_block(&[
            (2, ".", 2),
            (2, "..", 2),
            (11, "lost+found", 2),
            (12, "hello.txt", 1),
            (13, "subdir", 2),
            (15, "big.txt", 1),
        ]);
        f.write_data_block(dir_block, &block);
        f.write_raw_inode(12, &make_dir_inode(dir_block));
        let dir = f.ext4.read_inode(12).unwrap();

        let expected = [".", "..", "lost+found", "hello.txt", "subdir", "big.txt"];

        // Including one entry per call, which forces the most resumption steps.
        for cap in [1usize, 2, 3, 6] {
            let mut all: Vec<String> = Vec::new();
            let mut offset = 0usize;
            loop {
                let mut visitor = CappedVisitor {
                    cap,
                    used: 0,
                    names: Vec::new(),
                };
                let read_cnt = dir.readdir_at(offset, &mut visitor).unwrap();
                all.extend(visitor.names);
                if read_cnt == 0 {
                    break;
                }
                offset += read_cnt;
                assert!(
                    all.len() <= expected.len(),
                    "readdir over-reported (cap={cap})"
                );
            }
            assert_eq!(all, expected, "readdir mismatch at cap={cap}");
        }
    }

    use super::super::super::{
        super_block::{RawSuperBlock, SUPER_BLOCK_OFFSET},
        test_utils::{Ext4Fixture, make_empty_file_inode},
    };
    use crate::time::clocks;

    const DIR_INO: u32 = 12;

    /// A fixture with a realistic bitmap and an empty (size 0) directory inode
    /// at `DIR_INO`, ready to be populated through the directory write path.
    fn fixture_with_empty_dir() -> Ext4Fixture {
        clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        // An empty directory: same on-disk shape as an empty file (size 0,
        // i_blocks 0, empty inline extent root) but typed as a directory.
        let mut raw = make_empty_file_inode();
        raw.mode = 0o040755; // S_IFDIR | 0755
        raw.link_count = 2;
        f.write_raw_inode(DIR_INO, &raw);
        f
    }

    /// Collects the live entry names of a directory via `readdir_at`.
    /// Test shorthand: the ino of `name` in `dir` (the lookup path's walk).
    fn entry_ino(dir: &Inode, name: &str) -> Result<super::super::super::prelude::Ext4Ino> {
        dir.inner.read().find_entry_info(name).map(|e| e.ino)
    }

    fn readdir_names(dir: &Inode) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        dir.readdir_at(0, &mut names).unwrap();
        names
    }

    /// `add_new_entry` then `find_entry_info`/`readdir_at` see the new name, and
    /// multiple adds in one block all become visible.
    #[ktest]
    fn add_new_entries_visible() {
        let f = fixture_with_empty_dir();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        {
            let mut inner = dir.inner.write();
            inner.make_empty(&f.ext4, DIR_INO, 2, None).unwrap();
            inner
                .add_new_entry(&f.ext4, "alpha", 21, DirEntryFileType::File, None)
                .unwrap();
            inner
                .add_new_entry(&f.ext4, "beta", 22, DirEntryFileType::Dir, None)
                .unwrap();
            inner
                .add_new_entry(&f.ext4, "gamma", 23, DirEntryFileType::File, None)
                .unwrap();
        }

        // Each name resolves to the inode it was added with.
        assert_eq!(dir.inner.read().find_entry_info("alpha").unwrap().ino, 21);
        assert_eq!(dir.inner.read().find_entry_info("beta").unwrap().ino, 22);
        assert_eq!(dir.inner.read().find_entry_info("gamma").unwrap().ino, 23);
        assert_eq!(
            entry_ino(&dir, "missing").unwrap_err().error(),
            Errno::ENOENT
        );

        // `.`/`..` plus the three names, in insertion order.
        assert_eq!(readdir_names(&dir), [".", "..", "alpha", "beta", "gamma"]);
        // Still one block; the directory did not need to grow.
        assert_eq!(dir.size(), BLOCK_SIZE);
    }

    /// An add that splits a live predecessor's slack reuses the same block.
    #[ktest]
    fn add_splits_predecessor_slack() {
        let f = fixture_with_empty_dir();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        // After `make_empty`, `..` owns all the slack to the block end. Adding a
        // name must split that slack rather than grow the directory.
        {
            let mut inner = dir.inner.write();
            inner.make_empty(&f.ext4, DIR_INO, 2, None).unwrap();
            inner
                .add_new_entry(&f.ext4, "split-me", 31, DirEntryFileType::File, None)
                .unwrap();
        }

        assert_eq!(dir.size(), BLOCK_SIZE);
        assert_eq!(entry_ino(&dir, "split-me").unwrap(), 31);
        assert_eq!(entry_ino(&dir, "..").unwrap(), 2);
        assert_eq!(readdir_names(&dir), [".", "..", "split-me"]);
    }

    /// Filling a block forces the next add to grow the directory; entries in
    /// both blocks are then found by `readdir_at`, and `file_size` grew by
    /// exactly one block.
    #[ktest]
    fn add_grows_directory_into_second_block() {
        let f = fixture_with_empty_dir();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        // 16-byte names => 24-byte records (min_rec_len(16) == 24). After the
        // 12+12 used by `.`/`..`, a 4 KiB block holds (4096-24)/24 = 169 such
        // records; add enough to overflow into a second block.
        let count = 200usize;
        {
            let mut inner = dir.inner.write();
            inner.make_empty(&f.ext4, DIR_INO, 2, None).unwrap();
            for i in 0..count {
                let name = format!("entry_file_{i:05}"); // 16 bytes
                assert_eq!(name.len(), 16);
                inner
                    .add_new_entry(
                        &f.ext4,
                        &name,
                        1000 + i as u32,
                        DirEntryFileType::File,
                        None,
                    )
                    .unwrap();
            }
        }

        // The directory grew past one block.
        assert!(dir.size() > BLOCK_SIZE, "directory did not grow");
        assert_eq!(dir.size() % BLOCK_SIZE, 0, "size not block-aligned");

        // Every name (including ones that landed in the grown block) is found.
        for i in 0..count {
            let name = format!("entry_file_{i:05}");
            assert_eq!(entry_ino(&dir, &name).unwrap(), 1000 + i as u32);
        }
        // readdir sees `.`/`..` plus all names across both blocks.
        assert_eq!(readdir_names(&dir).len(), count + 2);

        // Unwritten-first regression: every directory block, including the grown
        // one, must be WRITTEN on the extent tree. A block left unwritten (as
        // `grow_dir_block` did before it converted the range) reads back as
        // zeros on disk — every entry in it lost, fsck-inconsistent. The
        // page-cache-served reads above would not reveal that (they hit the
        // still-cached bytes); the on-tree extent state does.
        let inner = dir.inner.read();
        let em = inner.extent_manager().unwrap();
        for blk in 0..(dir.size() / BLOCK_SIZE) as u32 {
            assert_eq!(
                em.map_blocks(blk).unwrap().state(),
                super::super::extent_manager::MapState::Written,
                "directory block {blk} left unwritten"
            );
        }
    }

    /// `delete_entry` removes a name, merges its space into the predecessor, and
    /// the reclaimed slot can hold a same-or-smaller name again.
    #[ktest]
    fn delete_entry_merges_and_reclaims() {
        let f = fixture_with_empty_dir();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        {
            let mut inner = dir.inner.write();
            inner.make_empty(&f.ext4, DIR_INO, 2, None).unwrap();
            inner
                .add_new_entry(&f.ext4, "keep", 41, DirEntryFileType::File, None)
                .unwrap();
            inner
                .add_new_entry(&f.ext4, "victim", 42, DirEntryFileType::File, None)
                .unwrap();
            inner
                .add_new_entry(&f.ext4, "tail", 43, DirEntryFileType::File, None)
                .unwrap();
        }
        assert_eq!(readdir_names(&dir).len(), 5); // . .. keep victim tail

        // Delete the middle entry; its space merges into `keep`.
        {
            let mut inner = dir.inner.write();
            let info = inner.find_entry_info("victim").unwrap();
            inner.delete_entry(&info, None).unwrap();
        }
        assert_eq!(
            entry_ino(&dir, "victim").unwrap_err().error(),
            Errno::ENOENT
        );
        assert_eq!(readdir_names(&dir), [".", "..", "keep", "tail"]);
        let size_after_delete = dir.size();

        // Re-add a same-or-smaller name; it must reuse the reclaimed slack
        // inside the existing block (no growth).
        {
            let mut inner = dir.inner.write();
            inner
                .add_new_entry(&f.ext4, "reuse", 44, DirEntryFileType::File, None)
                .unwrap();
        }
        assert_eq!(dir.size(), size_after_delete, "re-add should not grow dir");
        assert_eq!(entry_ino(&dir, "reuse").unwrap(), 44);
        assert_eq!(readdir_names(&dir).len(), 5);
    }

    /// `make_empty` lays down `.`/`..`, `empty_dir` reports true, and a later add
    /// flips it to false.
    #[ktest]
    fn make_empty_then_empty_dir() {
        let f = fixture_with_empty_dir();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        {
            let mut inner = dir.inner.write();
            inner.make_empty(&f.ext4, DIR_INO, 2, None).unwrap();
        }

        // `.` points to self, `..` to the parent (ino 2).
        assert_eq!(entry_ino(&dir, ".").unwrap(), DIR_INO);
        assert_eq!(entry_ino(&dir, "..").unwrap(), 2);
        assert_eq!(dir.size(), BLOCK_SIZE);
        assert!(dir.inner.read().empty_dir(DIR_INO));
        // `..` pointing elsewhere does not count as an extra live name.
        assert_eq!(readdir_names(&dir), [".", ".."]);

        // After adding a real entry, the directory is no longer empty.
        {
            let mut inner = dir.inner.write();
            inner
                .add_new_entry(&f.ext4, "child", 51, DirEntryFileType::Dir, None)
                .unwrap();
        }
        assert!(!dir.inner.read().empty_dir(DIR_INO));
    }

    use super::super::FilePerm;

    /// A fixture whose block *and* inode bitmaps are marked, with a `.`/`..`
    /// directory at `DIR_INO` ready to receive `create`d children. `DIR_INO`'s
    /// `..` points to the root inode (2).
    fn fixture_for_create() -> Ext4Fixture {
        clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_inode_bitmap_metadata_marked()
            // Reserve the pre-placed directory's inode so a created child never
            // gets handed `DIR_INO` back (which would alias the directory).
            .with_reserved_inode(DIR_INO)
            .build()
            .unwrap();
        let mut raw = make_empty_file_inode();
        raw.mode = 0o040755; // S_IFDIR | 0755
        raw.link_count = 2;
        f.write_raw_inode(DIR_INO, &raw);
        let dir = f.ext4.read_inode(DIR_INO).unwrap();
        dir.inner
            .write()
            .make_empty(&f.ext4, DIR_INO, 2, None)
            .unwrap();
        f
    }

    fn perm() -> FilePerm {
        FilePerm::from_bits_truncate(0o644)
    }

    /// `create` of a regular file: `lookup` finds it (type File, link count 1),
    /// the parent's mtime advances, and the child has a valid empty extent root
    /// with the `EXTENTS` flag (from the inode allocator).
    #[ktest]
    fn create_regular_file() {
        let f = fixture_for_create();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();
        let mtime_before = dir.mtime();

        let child = dir.create("file.txt", InodeType::File, perm()).unwrap();
        assert_eq!(child.inode_type(), InodeType::File);
        assert_eq!(child.link_count(), 1);
        assert_eq!(child.size(), 0);
        // A fresh extent-mapped file: extent flag set, no data block yet.
        assert!(child.inner.read().desc.is_extent_based());
        assert_eq!(child.sector_count(), 0);

        // The name resolves to the child, and the cached child is identity-equal.
        let looked_up = dir.lookup("file.txt").unwrap();
        assert_eq!(looked_up.ino(), child.ino());
        assert!(Arc::ptr_eq(&looked_up, &child));
        assert_eq!(readdir_names(&dir), [".", "..", "file.txt"]);

        // The parent's mtime/ctime advanced.
        assert!(dir.mtime() >= mtime_before);
        // The directory itself did not gain a link (only subdir creation does).
        assert_eq!(dir.link_count(), 2);
    }

    /// `create` of a subdirectory: it has `.`/`..` (empty_dir true, `..` -> the
    /// parent), child link count 2, and the parent's link count gains one.
    #[ktest]
    fn create_subdirectory() {
        let f = fixture_for_create();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();
        let parent_links_before = dir.link_count();

        let child = dir.create("sub", InodeType::Dir, perm()).unwrap();
        assert_eq!(child.inode_type(), InodeType::Dir);
        assert_eq!(child.link_count(), 2); // `.` and the parent's entry.

        // The new directory has only `.`/`..`, and `..` points back to DIR_INO.
        assert!(child.inner.read().empty_dir(child.ino()));
        assert_eq!(entry_ino(&child, ".").unwrap(), child.ino());
        assert_eq!(entry_ino(&child, "..").unwrap(), DIR_INO);

        // The parent gained one link for the child's `..` reference.
        assert_eq!(dir.link_count(), parent_links_before + 1);
        assert_eq!(dir.lookup("sub").unwrap().ino(), child.ino());
    }

    /// Creating many children forces the parent directory to grow a second
    /// block; every name remains resolvable afterwards.
    #[ktest]
    fn create_grows_parent_into_second_block() {
        let f = fixture_for_create();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        // 16-byte names => 24-byte records; ~169 fit in the first block after
        // `.`/`..`, so 200 overflow into a second block.
        let count = 200usize;
        for i in 0..count {
            let name = format!("child_file_{i:05}"); // 16 bytes
            assert_eq!(name.len(), 16);
            dir.create(&name, InodeType::File, perm()).unwrap();
        }

        assert!(dir.size() > BLOCK_SIZE, "parent directory did not grow");
        assert_eq!(dir.size() % BLOCK_SIZE, 0, "size not block-aligned");
        for i in 0..count {
            let name = format!("child_file_{i:05}");
            assert!(dir.lookup(&name).is_ok(), "missing {name}");
        }
        assert_eq!(readdir_names(&dir).len(), count + 2);
    }

    /// Devices cannot come through plain `create` (they need a device number,
    /// via `mknod` → `create_with_device`); FIFOs and sockets can — a Unix
    /// socket bind arrives here as a plain `create`.
    #[ktest]
    fn create_routes_special_files() {
        let f = fixture_for_create();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        for type_ in [InodeType::CharDevice, InodeType::BlockDevice] {
            assert_eq!(
                dir.create("special", type_, perm())
                    .map(|_| ())
                    .unwrap_err()
                    .error(),
                Errno::EINVAL
            );
        }
        // No half-built entry was left behind.
        assert_eq!(readdir_names(&dir), [".", ".."]);

        let fifo = dir.create("fifo", InodeType::NamedPipe, perm()).unwrap();
        assert_eq!(fifo.inode_type(), InodeType::NamedPipe);
        assert!(fifo.pipe().is_some());
        // Special files carry no extent tree.
        assert!(!fifo.inner.read().desc.is_extent_based());

        let sock = dir.create("sock", InodeType::Socket, perm()).unwrap();
        assert_eq!(sock.inode_type(), InodeType::Socket);
        assert!(sock.pipe().is_none());

        assert_eq!(readdir_names(&dir), [".", "..", "fifo", "sock"]);
    }

    /// `mknod` devices: the device number is encoded into `i_block` inside the
    /// creating transaction (no rdev-0 crash window) and survives the on-disk
    /// round-trip, in both the old (8-bit) and wide encodings.
    #[ktest]
    fn create_with_device_encodes_rdev() {
        let f = fixture_for_create();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        let small = device_id::encode_device_numbers(8, 1);
        let chr = dir
            .create_with_device("chr", InodeType::CharDevice, perm(), small)
            .unwrap();
        assert_eq!(chr.device_id(), Some(small));

        let wide = device_id::encode_device_numbers(300, 70000);
        let blk = dir
            .create_with_device("blk", InodeType::BlockDevice, perm(), wide)
            .unwrap();
        assert_eq!(blk.device_id(), Some(wide));

        // Non-device types are rejected.
        assert_eq!(
            dir.create_with_device("f", InodeType::File, perm(), small)
                .map(|_| ())
                .unwrap_err()
                .error(),
            Errno::EINVAL
        );

        // Round-trip through the on-disk inode table.
        chr.sync_metadata().unwrap();
        let desc = f.ext4.read_inode_desc(chr.ino()).unwrap();
        assert_eq!(desc.type_(), InodeType::CharDevice);
        assert_eq!(desc.device_id(), Some(small));
    }

    /// When the child inode is allocated but the directory write fails — here a
    /// subdirectory whose `make_empty` cannot allocate its first block because
    /// free blocks are exhausted — `create` leaves the child link count at 0
    /// (ready for reclaim) and the name absent.
    #[ktest]
    fn create_error_path_clears_link_count() {
        clocks::init_for_ktest();
        // Cap the image to exactly one free block, which the parent directory's
        // `make_empty` then consumes — leaving zero for the child's.
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_free_blocks(1)
            .with_inode_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut raw = make_empty_file_inode();
        raw.mode = 0o040755; // S_IFDIR | 0755
        raw.link_count = 2;
        f.write_raw_inode(DIR_INO, &raw);
        let dir = f.ext4.read_inode(DIR_INO).unwrap();
        // Consumes the one free block for the parent's first directory block.
        dir.inner
            .write()
            .make_empty(&f.ext4, DIR_INO, 2, None)
            .unwrap();
        let parent_links_before = dir.link_count();

        // The child inode is allocated and its on-disk desc written, but its
        // `make_empty` block allocation fails (no free blocks left).
        let err = dir
            .create("doomed", InodeType::Dir, perm())
            .map(|_| ())
            .unwrap_err();
        assert_eq!(err.error(), Errno::ENOSPC);

        // The name was never published, the parent did not gain a link, and no
        // live inode for "doomed" lingers in the cache.
        assert_eq!(
            dir.inner
                .read()
                .find_entry_info("doomed")
                .unwrap_err()
                .error(),
            Errno::ENOENT
        );
        assert_eq!(dir.link_count(), parent_links_before);
        assert_eq!(readdir_names(&dir), [".", ".."]);
    }

    /// `unlink` of a regular file with no open handle: the name disappears, the
    /// parent's mtime advances, and the child inode *and* its data blocks are
    /// reclaimed — free-inode and free-block counts return to their pre-create
    /// values and the inode's bitmap bit is cleared.
    #[ktest]
    fn unlink_frees_inode_and_blocks() {
        let f = fixture_for_create();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        let free_inodes_before = f.ext4.block_group(0).free_inodes_count();
        let free_blocks_before = f.ext4.block_group(0).free_blocks_count();

        // Create a file and write a block of data so reclaim must free a block.
        let child = dir.create("victim.txt", InodeType::File, perm()).unwrap();
        let child_ino = child.ino();
        let mut reader = VmReader::from(&[7u8; 16][..]).to_fallible();
        child.write_at(0, &mut reader).unwrap();
        assert!(child.sector_count() > 0);
        assert!(f.ext4.is_inode_allocated(child_ino));
        // Drop our handle so unlink holds the only remaining reference.
        drop(child);

        let mtime_before = dir.mtime();
        dir.unlink("victim.txt").unwrap();

        // The name is gone, the parent's mtime advanced, and the inode + its
        // data block were reclaimed (counts restored, bitmap bit cleared).
        assert!(dir.lookup("victim.txt").is_err());
        assert_eq!(readdir_names(&dir), [".", ".."]);
        assert!(dir.mtime() >= mtime_before);
        assert!(!f.ext4.is_inode_allocated(child_ino));
        assert_eq!(
            f.ext4.block_group(0).free_inodes_count(),
            free_inodes_before
        );
        assert_eq!(
            f.ext4.block_group(0).free_blocks_count(),
            free_blocks_before
        );
    }

    /// Unlink-of-open: while an `Arc` to the child is held, `unlink` removes the
    /// name but does NOT free the inode (it stays allocated). Dropping the held
    /// `Arc` then reclaims it via `Drop`, restoring the free counts. This is the
    /// key refcount/reclaim test.
    #[ktest]
    fn unlink_of_open_defers_reclaim() {
        let f = fixture_for_create();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        let free_inodes_before = f.ext4.block_group(0).free_inodes_count();
        let free_blocks_before = f.ext4.block_group(0).free_blocks_count();

        // Create a file with data, and keep an `Arc` open across the unlink.
        let open = dir.create("open.txt", InodeType::File, perm()).unwrap();
        let child_ino = open.ino();
        let mut reader = VmReader::from(&[3u8; 32][..]).to_fallible();
        open.write_at(0, &mut reader).unwrap();

        dir.unlink("open.txt").unwrap();

        // Name gone, but the inode is still allocated: an fd holds it open.
        assert!(dir.lookup("open.txt").is_err());
        assert!(
            f.ext4.is_inode_allocated(child_ino),
            "inode freed while still open"
        );
        assert_eq!(open.link_count(), 0);

        // Releasing the last `Arc` triggers `Drop` reclaim.
        drop(open);
        assert!(!f.ext4.is_inode_allocated(child_ino));
        assert_eq!(
            f.ext4.block_group(0).free_inodes_count(),
            free_inodes_before
        );
        assert_eq!(
            f.ext4.block_group(0).free_blocks_count(),
            free_blocks_before
        );
    }

    /// `unlink` on a directory is rejected with `EISDIR`; the entry survives.
    #[ktest]
    fn unlink_directory_rejected() {
        let f = fixture_for_create();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();
        dir.create("subdir", InodeType::Dir, perm()).unwrap();

        assert_eq!(
            dir.unlink("subdir").unwrap_err().error(),
            Errno::EISDIR,
            "unlink of a directory must fail with EISDIR"
        );
        assert!(dir.lookup("subdir").is_ok());
    }

    /// `rmdir` on a non-empty directory is rejected with `ENOTEMPTY`.
    #[ktest]
    fn rmdir_non_empty_rejected() {
        let f = fixture_for_create();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();
        let sub = dir.create("full", InodeType::Dir, perm()).unwrap();
        sub.create("inhabitant", InodeType::File, perm()).unwrap();

        assert_eq!(
            dir.rmdir("full").unwrap_err().error(),
            Errno::ENOTEMPTY,
            "rmdir of a non-empty directory must fail with ENOTEMPTY"
        );
        assert!(dir.lookup("full").is_ok());
    }

    /// `rmdir` on a regular file is rejected with `ENOTDIR`.
    #[ktest]
    fn rmdir_file_rejected() {
        let f = fixture_for_create();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();
        dir.create("plain.txt", InodeType::File, perm()).unwrap();

        assert_eq!(
            dir.rmdir("plain.txt").unwrap_err().error(),
            Errno::ENOTDIR,
            "rmdir of a regular file must fail with ENOTDIR"
        );
        assert!(dir.lookup("plain.txt").is_ok());
    }

    /// `rmdir` of an empty directory: the name disappears, the parent loses one
    /// link (for the removed child's `..`), the child inode is freed, and its
    /// `.`/`..` data block is freed.
    #[ktest]
    fn rmdir_empty_directory() {
        let f = fixture_for_create();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        let free_inodes_before = f.ext4.block_group(0).free_inodes_count();
        let free_blocks_before = f.ext4.block_group(0).free_blocks_count();
        let parent_links_before = dir.link_count();

        let sub = dir.create("gone", InodeType::Dir, perm()).unwrap();
        let sub_ino = sub.ino();
        // A fresh directory has one data block (its `.`/`..`).
        assert!(sub.sector_count() > 0);
        assert_eq!(dir.link_count(), parent_links_before + 1);
        drop(sub);

        dir.rmdir("gone").unwrap();

        assert!(dir.lookup("gone").is_err());
        assert_eq!(readdir_names(&dir), [".", ".."]);
        // The parent dropped the `..` back-reference link it gained on create.
        assert_eq!(dir.link_count(), parent_links_before);
        assert!(!f.ext4.is_inode_allocated(sub_ino));
        assert_eq!(
            f.ext4.block_group(0).free_inodes_count(),
            free_inodes_before
        );
        assert_eq!(
            f.ext4.block_group(0).free_blocks_count(),
            free_blocks_before
        );
    }

    /// Creating and unlinking a file in a loop reuses inode and block numbers,
    /// proving reclaim returns resources to the allocator (no leak).
    #[ktest]
    fn create_unlink_loop_reuses_resources() {
        let f = fixture_for_create();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        let free_inodes_before = f.ext4.block_group(0).free_inodes_count();
        let free_blocks_before = f.ext4.block_group(0).free_blocks_count();

        let mut first_ino = None;
        for _ in 0..8 {
            let child = dir.create("churn", InodeType::File, perm()).unwrap();
            let mut reader = VmReader::from(&[1u8; 8][..]).to_fallible();
            child.write_at(0, &mut reader).unwrap();
            let ino = child.ino();
            match first_ino {
                None => first_ino = Some(ino),
                // Each iteration frees the inode before the next allocates, so
                // the same number is handed back out.
                Some(expected) => assert_eq!(ino, expected, "inode number not reused"),
            }
            drop(child);
            dir.unlink("churn").unwrap();
        }

        // No drift in the free counts after a full create/unlink cycle.
        assert_eq!(
            f.ext4.block_group(0).free_inodes_count(),
            free_inodes_before
        );
        assert_eq!(
            f.ext4.block_group(0).free_blocks_count(),
            free_blocks_before
        );
    }

    /// On a non-journaled volume the orphan machinery is inert (journal-only,
    /// Linux parity): unlink + reclaim succeed, the inode is freed, and the
    /// superblock's orphan head never budges.
    #[ktest]
    fn orphan_machinery_inert_without_journal() {
        let f = fixture_for_create();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();
        let child = dir.create("seam", InodeType::File, perm()).unwrap();
        let child_ino = child.ino();
        drop(child);

        dir.unlink("seam").unwrap();
        assert!(!f.ext4.is_inode_allocated(child_ino));
        assert_eq!(f.ext4.super_block().last_orphan(), None);
    }

    /// `fixture_for_create` plus a journal, for the orphan-list flow tests. The
    /// commit thread is stopped so every assertion window is deterministic; the
    /// 64-block log comfortably fits the accumulated transactions.
    fn journaled_fixture_for_create() -> Ext4Fixture {
        clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_inode_bitmap_metadata_marked()
            .with_reserved_inode(DIR_INO)
            .with_journal_inode(64)
            .build()
            .unwrap();
        let mut raw = make_empty_file_inode();
        raw.mode = 0o040755; // S_IFDIR | 0755
        raw.link_count = 2;
        f.write_raw_inode(DIR_INO, &raw);
        f.ext4.journal().unwrap().stop_commit_thread();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();
        dir.inner
            .write()
            .make_empty(&f.ext4, DIR_INO, 2, None)
            .unwrap();
        f
    }

    /// The Task 8 story end to end: unlink-of-open chains both inodes onto the
    /// orphan list (head = the newest, `i_dtime` = the successor, all
    /// journaled); reclaiming the *tail* first exercises the non-head splice
    /// (the predecessor's on-disk pointer skips it); reclaiming the head drains
    /// the list. Every intermediate on-disk state a crash could expose is a
    /// well-formed chain of exactly the still-allocated orphans.
    #[ktest]
    fn unlink_of_open_chains_then_reclaims_drain() {
        let f = journaled_fixture_for_create();
        let journal = f.ext4.journal().unwrap();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        let a = dir.create("a", InodeType::File, perm()).unwrap();
        let b = dir.create("b", InodeType::File, perm()).unwrap();
        let (a_ino, b_ino) = (a.ino(), b.ino());
        journal.commit_now_for_test();

        // Unlink both while "open" (our Arcs stand in for fds): reclaim defers,
        // the chain grows at the head: b → a.
        dir.unlink("a").unwrap();
        assert_eq!(
            f.ext4.super_block().last_orphan(),
            Some(a_ino),
            "a is the head"
        );
        assert!(f.ext4.is_inode_allocated(a_ino), "reclaim deferred (open)");
        dir.unlink("b").unwrap();
        assert_eq!(
            f.ext4.super_block().last_orphan(),
            Some(b_ino),
            "b is the head"
        );

        // The chain is durable: commit + checkpoint, then read it off the disk —
        // this is the state a crash would hand to the next mount's orphan scan.
        journal.commit_now_for_test();
        journal.flush_on_unmount().unwrap();
        let sb_disk: RawSuperBlock = f.disk.segment().read_val(SUPER_BLOCK_OFFSET).unwrap();
        assert_eq!(sb_disk.last_orphan, b_ino, "on-disk head");
        assert_eq!(
            f.read_raw_inode(b_ino).dtime,
            a_ino,
            "b's on-disk i_dtime chains to a"
        );

        // Reclaim the TAIL first (a): the non-head case — b's on-disk pointer is
        // spliced to a's successor (0), the head stays b.
        drop(a);
        assert!(!f.ext4.is_inode_allocated(a_ino), "a freed");
        assert_eq!(
            f.ext4.super_block().last_orphan(),
            Some(b_ino),
            "head unchanged"
        );
        journal.commit_now_for_test();
        journal.flush_on_unmount().unwrap();
        assert_eq!(
            f.read_raw_inode(b_ino).dtime,
            0,
            "b's on-disk pointer spliced past the reclaimed a"
        );

        // Reclaim the head (b): the list drains, on disk too.
        drop(b);
        assert!(!f.ext4.is_inode_allocated(b_ino), "b freed");
        assert_eq!(f.ext4.super_block().last_orphan(), None, "list drained");
        journal.commit_now_for_test();
        journal.flush_on_unmount().unwrap();
        let sb_disk: RawSuperBlock = f.disk.segment().read_val(SUPER_BLOCK_OFFSET).unwrap();
        assert_eq!(sb_disk.last_orphan, 0, "drained head persisted");
    }

    // ── link ────────────────────────────────────────────────────────────────

    /// A hard link makes a second name resolve to the same inode and bumps its
    /// link count; unlinking one name leaves the other live with the count back
    /// at 1.
    #[ktest]
    fn link_hardlinks_a_file() {
        let f = fixture_for_create();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        let original = dir.create("a", InodeType::File, perm()).unwrap();
        let ino = original.ino();
        assert_eq!(original.link_count(), 1);

        // Add a second name "b" linking the same inode.
        dir.link(&original, "b").unwrap();
        assert_eq!(original.link_count(), 2);
        assert_eq!(dir.lookup("a").unwrap().ino(), ino);
        assert_eq!(dir.lookup("b").unwrap().ino(), ino);
        assert!(Arc::ptr_eq(
            &dir.lookup("a").unwrap(),
            &dir.lookup("b").unwrap()
        ));

        // Unlinking one name leaves the other live; the inode is not reclaimed.
        dir.unlink("a").unwrap();
        assert!(dir.lookup("a").is_err());
        assert_eq!(dir.lookup("b").unwrap().ino(), ino);
        assert_eq!(original.link_count(), 1);
        assert!(f.ext4.is_inode_allocated(ino));
    }

    /// Hard-linking a directory is rejected above ext4: the VFS/syscall layer
    /// returns `EPERM` before `Inode::link` is reached (see `syscall/link.rs`),
    /// so — like ext2 — ext4's `link` performs no inode-level type check. This
    /// test documents that the primitive itself only governs the supported
    /// (non-directory) path; the directory guard lives in the caller.
    #[ktest]
    fn link_directory_guard_is_in_caller() {
        let f = fixture_for_create();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();
        let file = dir.create("f", InodeType::File, perm()).unwrap();
        dir.link(&file, "g").unwrap();
        assert_eq!(dir.lookup("g").unwrap().ino(), file.ino());
        assert_eq!(file.link_count(), 2);
    }

    // ── rename within one directory ──────────────────────────────────────────

    /// Renaming `a` to a non-existent `b` in the same directory: `b` resolves to
    /// the old inode and `a` is gone.
    #[ktest]
    fn rename_same_dir_no_replace() {
        let f = fixture_for_create();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        let a = dir.create("a", InodeType::File, perm()).unwrap();
        let a_ino = a.ino();

        dir.rename("a", &dir, "b").unwrap();
        assert!(dir.lookup("a").is_err());
        assert_eq!(dir.lookup("b").unwrap().ino(), a_ino);
        assert_eq!(readdir_names(&dir), [".", "..", "b"]);
        // The moved inode kept its single link.
        assert_eq!(a.link_count(), 1);
    }

    /// Renaming a no-op onto itself (`a` -> `a`) succeeds and changes nothing.
    #[ktest]
    fn rename_same_name_is_noop() {
        let f = fixture_for_create();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();
        let a = dir.create("a", InodeType::File, perm()).unwrap();

        dir.rename("a", &dir, "a").unwrap();
        assert_eq!(dir.lookup("a").unwrap().ino(), a.ino());
        assert_eq!(readdir_names(&dir), [".", "..", "a"]);
    }

    /// Renaming `a` onto an existing `b` (file onto file) in the same directory:
    /// `b` now resolves to `a`'s inode, and `b`'s old inode (link count 1) is
    /// reclaimed, restoring the free counts.
    #[ktest]
    fn rename_same_dir_replaces_file() {
        let f = fixture_for_create();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        let free_inodes_before = f.ext4.block_group(0).free_inodes_count();
        let free_blocks_before = f.ext4.block_group(0).free_blocks_count();

        let a = dir.create("a", InodeType::File, perm()).unwrap();
        let a_ino = a.ino();
        let b = dir.create("b", InodeType::File, perm()).unwrap();
        let b_ino = b.ino();
        // Give `b` a data block so reclaim must free it.
        let mut reader = VmReader::from(&[9u8; 16][..]).to_fallible();
        b.write_at(0, &mut reader).unwrap();
        assert!(b.sector_count() > 0);
        drop(b);

        dir.rename("a", &dir, "b").unwrap();

        assert!(dir.lookup("a").is_err());
        assert_eq!(dir.lookup("b").unwrap().ino(), a_ino);
        assert_eq!(a.link_count(), 1);
        // `b`'s old inode and its data block were reclaimed.
        assert!(!f.ext4.is_inode_allocated(b_ino));
        assert_eq!(readdir_names(&dir), [".", "..", "b"]);
        // Net effect: only `a` survives, so exactly one inode and the blocks it
        // does not use are back; counts match a single surviving empty file.
        let a_alive = f.ext4.read_inode(a_ino).unwrap();
        assert_eq!(a_alive.size(), 0);
        // `a` (no data) plus the reclaimed `b` restore both counts to one
        // allocated inode below the pre-create baseline.
        assert_eq!(
            f.ext4.block_group(0).free_inodes_count(),
            free_inodes_before - 1
        );
        assert_eq!(
            f.ext4.block_group(0).free_blocks_count(),
            free_blocks_before
        );
    }

    /// Renaming onto an existing name whose inode is also hard-linked elsewhere
    /// (link count 2): the replaced name's inode survives with link count 1.
    #[ktest]
    fn rename_replace_hardlinked_target_survives() {
        let f = fixture_for_create();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        let a = dir.create("a", InodeType::File, perm()).unwrap();
        let a_ino = a.ino();
        let b = dir.create("b", InodeType::File, perm()).unwrap();
        let b_ino = b.ino();
        // `b` is also reachable as "b_alias", so its link count is 2.
        dir.link(&b, "b_alias").unwrap();
        assert_eq!(b.link_count(), 2);

        dir.rename("a", &dir, "b").unwrap();

        // "b" now points at `a`; the old `b` inode survives via "b_alias".
        assert_eq!(dir.lookup("b").unwrap().ino(), a_ino);
        assert!(f.ext4.is_inode_allocated(b_ino));
        assert_eq!(dir.lookup("b_alias").unwrap().ino(), b_ino);
        assert_eq!(b.link_count(), 1);
    }

    // ── rename across directories ────────────────────────────────────────────

    /// Creates two sibling subdirectories `dir1` and `dir2` under `DIR_INO` and
    /// returns them.
    fn two_subdirs(f: &Ext4Fixture) -> (Arc<Inode>, Arc<Inode>) {
        let root = f.ext4.read_inode(DIR_INO).unwrap();
        let dir1 = root.create("dir1", InodeType::Dir, perm()).unwrap();
        let dir2 = root.create("dir2", InodeType::Dir, perm()).unwrap();
        (dir1, dir2)
    }

    /// Moving a file from `dir1` to `dir2`: gone from `dir1`, present in `dir2`,
    /// same inode.
    #[ktest]
    fn rename_moves_file_across_dirs() {
        let f = fixture_for_create();
        let (dir1, dir2) = two_subdirs(&f);

        let file = dir1.create("f", InodeType::File, perm()).unwrap();
        let ino = file.ino();

        dir1.rename("f", &dir2, "g").unwrap();
        assert!(dir1.lookup("f").is_err());
        assert_eq!(dir2.lookup("g").unwrap().ino(), ino);
        assert_eq!(file.link_count(), 1);
        assert_eq!(readdir_names(&dir1), [".", ".."]);
        assert_eq!(readdir_names(&dir2), [".", "..", "g"]);
    }

    /// Moving a directory across directories: its `..` now points at the new
    /// parent, the old parent loses a link, and the new parent gains one.
    #[ktest]
    fn rename_moves_dir_across_dirs_repoints_dotdot() {
        let f = fixture_for_create();
        let (dir1, dir2) = two_subdirs(&f);

        let moved = dir1.create("sub", InodeType::Dir, perm()).unwrap();
        let moved_ino = moved.ino();
        // dir1 gained a link for `sub`'s `..`; dir2 has only its own.
        let dir1_links_before = dir1.link_count();
        let dir2_links_before = dir2.link_count();
        // `sub`'s `..` points at dir1.
        assert_eq!(
            moved.inner.read().find_entry_info("..").unwrap().ino,
            dir1.ino()
        );

        dir1.rename("sub", &dir2, "sub").unwrap();

        assert!(dir1.lookup("sub").is_err());
        assert_eq!(dir2.lookup("sub").unwrap().ino(), moved_ino);
        // `..` now points at dir2.
        assert_eq!(
            moved.inner.read().find_entry_info("..").unwrap().ino,
            dir2.ino()
        );
        // Old parent lost the back-link, new parent gained one.
        assert_eq!(dir1.link_count(), dir1_links_before - 1);
        assert_eq!(dir2.link_count(), dir2_links_before + 1);
        // The moved directory's own link count is unchanged (still `.` + entry).
        assert_eq!(moved.link_count(), 2);
    }

    // ── rename invariant rejections ──────────────────────────────────────────

    /// Renaming a directory onto a non-empty directory is rejected with
    /// `ENOTEMPTY`, leaving both names intact.
    #[ktest]
    fn rename_dir_onto_nonempty_dir_enotempty() {
        let f = fixture_for_create();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        let src = dir.create("src", InodeType::Dir, perm()).unwrap();
        let dst = dir.create("dst", InodeType::Dir, perm()).unwrap();
        // Make `dst` non-empty.
        dst.create("inhabitant", InodeType::File, perm()).unwrap();

        assert_eq!(
            dir.rename("src", &dir, "dst").unwrap_err().error(),
            Errno::ENOTEMPTY
        );
        assert_eq!(dir.lookup("src").unwrap().ino(), src.ino());
        assert_eq!(dir.lookup("dst").unwrap().ino(), dst.ino());
    }

    /// Renaming a directory onto a regular file is rejected with `ENOTDIR`.
    #[ktest]
    fn rename_dir_onto_file_enotdir() {
        let f = fixture_for_create();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        dir.create("src", InodeType::Dir, perm()).unwrap();
        dir.create("dst", InodeType::File, perm()).unwrap();

        assert_eq!(
            dir.rename("src", &dir, "dst").unwrap_err().error(),
            Errno::ENOTDIR
        );
        assert!(dir.lookup("src").is_ok());
        assert!(dir.lookup("dst").is_ok());
    }

    /// Renaming a regular file onto a directory is rejected with `EISDIR`.
    #[ktest]
    fn rename_file_onto_dir_eisdir() {
        let f = fixture_for_create();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        dir.create("src", InodeType::File, perm()).unwrap();
        dir.create("dst", InodeType::Dir, perm()).unwrap();

        assert_eq!(
            dir.rename("src", &dir, "dst").unwrap_err().error(),
            Errno::EISDIR
        );
        assert!(dir.lookup("src").is_ok());
        assert!(dir.lookup("dst").is_ok());
    }

    /// Renaming a missing source is `ENOENT`.
    #[ktest]
    fn rename_missing_source_enoent() {
        let f = fixture_for_create();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();
        assert_eq!(
            dir.rename("nope", &dir, "x").unwrap_err().error(),
            Errno::ENOENT
        );
    }
}
