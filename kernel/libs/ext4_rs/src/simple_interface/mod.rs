use core::panic::RefUnwindSafe;

use crate::prelude::*;

use crate::ext4_defs::*;
use crate::return_errno;
use crate::return_errno_with_message;
use crate::utils::path_check;

// export some definitions
pub use crate::ext4_defs::Ext4;
pub use crate::ext4_defs::Ext4Fsblk;
pub use crate::ext4_defs::BLOCK_SIZE;
pub use crate::ext4_defs::BlockDevice;
pub use crate::ext4_defs::CommitBlock;
pub use crate::ext4_defs::EXT4_FEATURE_INCOMPAT_RECOVER;
pub use crate::ext4_defs::InodeFileType;
pub use crate::ext4_defs::JBD2_CHECKSUM_BYTES;
pub use crate::ext4_defs::JBD2_COMMIT_BLOCK;
pub use crate::ext4_defs::JBD2_DESCRIPTOR_BLOCK;
pub use crate::ext4_defs::JBD2_FEATURE_INCOMPAT_64BIT;
pub use crate::ext4_defs::JBD2_FEATURE_INCOMPAT_CSUM_V2;
pub use crate::ext4_defs::JBD2_FEATURE_INCOMPAT_CSUM_V3;
pub use crate::ext4_defs::JBD2_FLAG_ESCAPE;
pub use crate::ext4_defs::JBD2_FLAG_LAST_TAG;
pub use crate::ext4_defs::JBD2_FLAG_SAME_UUID;
pub use crate::ext4_defs::JBD2_MAGIC_NUMBER;
pub use crate::ext4_defs::JournalBlockTag;
pub use crate::ext4_defs::JournalBlockTag3;
pub use crate::ext4_defs::JournalHeader;
pub use crate::ext4_defs::JournalSuperblock;
pub use crate::ext4_defs::MetadataWriter;
pub use crate::ext4_defs::OperationAllocGuard;
pub use crate::ext4_defs::OperationAllocGuardDebugStats;
pub use crate::ext4_defs::ROOT_INODE as EXT4_ROOT_INODE;
pub use crate::ext4_impls::Jbd2Journal;
pub use crate::ext4_impls::JournalCommitBlock;
pub use crate::ext4_impls::JournalCommitPlan;
pub use crate::ext4_impls::JournalCommitWriteStage;
pub use crate::ext4_impls::JournalHandle;
pub use crate::ext4_impls::JournalHandleSummary;
pub use crate::ext4_impls::JournalRecoveryResult;
pub use crate::ext4_impls::JournalRuntime;
pub use crate::ext4_impls::JournalTransaction;
pub use crate::ext4_impls::JournalTransactionState;
pub use crate::ext4_impls::LocalOperationAllocGuard;
pub use crate::ext4_impls::OperationScopedAllocGuard;

#[derive(Clone, Debug)]
pub struct SimpleDirEntry {
    pub inode: u32,
    pub de_type: u8,
    pub name: String,
    pub next_offset: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct SimpleInodeMeta {
    pub ino: u32,
    pub mode: u16,
    pub file_type: u16,
    pub uid: u16,
    pub gid: u16,
    pub nlink: u16,
    pub size: u64,
    pub blocks: u64,
    pub atime: u32,
    pub mtime: u32,
    pub ctime: u32,
    pub rdev: u32,
    pub flags: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct SimpleBlockRange {
    pub lblock: u32,
    pub pblock: u64,
    pub len: u32,
}


/// simple interface for ext4
impl Ext4 {

    /// Parse the file access flags (such as "r", "w", "a", etc.) and convert them to system constants.
    ///
    /// This method parses common file access flags into their corresponding bitwise constants defined in `libc`.
    ///
    /// # Arguments
    /// * `flags` - The string representation of the file access flags (e.g., "r", "w", "a", "r+", etc.).
    ///
    /// # Returns
    /// * `Result<i32>` - The corresponding bitwise flag constants (e.g., `O_RDONLY`, `O_WRONLY`, etc.), or an error if the flags are invalid.
    fn ext4_parse_flags(&self, flags: &str) -> Result<i32> {
        match flags {
            "r" | "rb" => Ok(O_RDONLY),
            "w" | "wb" => Ok(O_WRONLY | O_CREAT | O_TRUNC),
            "a" | "ab" => Ok(O_WRONLY | O_CREAT | O_APPEND),
            "r+" | "rb+" | "r+b" => Ok(O_RDWR),
            "w+" | "wb+" | "w+b" => Ok(O_RDWR | O_CREAT | O_TRUNC),
            "a+" | "ab+" | "a+b" => Ok(O_RDWR | O_CREAT | O_APPEND),
            _ => Err(Ext4Error::new(Errno::EINVAL)),
        }
    }

    /// Open a file at the specified path and return the corresponding inode number.
    ///
    /// Open a file by searching for the given path starting from the root directory (`ROOT_INODE`).
    /// If the file does not exist and the `O_CREAT` flag is specified, the file will be created.
    ///
    /// # Arguments
    /// * `path` - The path of the file to open.
    /// * `flags` - The access flags (e.g., "r", "w", "a", etc.).
    ///
    /// # Returns
    /// * `Result<u32>` - Returns the inode number of the opened file if successful.
    pub fn ext4_file_open(
        &self,
        path: &str,
        flags: &str,
    ) -> Result<u32> {
        let mut parent_inode_num = ROOT_INODE;
        let filetype = InodeFileType::S_IFREG;

        let iflags = self.ext4_parse_flags(flags)?;

        let mut create = false;
        if iflags & O_CREAT != 0 {
            create = true;
        }

        self.generic_open(path, &mut parent_inode_num, create, filetype.bits(), &mut 0)
    }

    /// Create a new directory at the specified path.
    /// 
    /// Checks if the directory already exists by searching from the root directory (`ROOT_INODE`).
    /// If the directory does not exist, it creates the directory under the root directory and returns its inode number.
    /// 
    /// # Arguments
    /// * `path` - The path where the directory will be created.
    /// 
    /// # Returns
    /// * `Result<u32>` - The inode number of the newly created directory if successful, 
    ///   or an error (`Errno::EEXIST`) if the directory already exists.
    pub fn ext4_dir_mk(&self, path: &str) -> Result<u32> {
        let mut search_result = Ext4DirSearchResult::new(Ext4DirEntry::default());
        let r = self.dir_find_entry(ROOT_INODE, path, &mut search_result);
        if r.is_ok() {
            return_errno!(Errno::EEXIST);
        }
        let mut parent_inode_num = ROOT_INODE;
        let filetype = InodeFileType::S_IFDIR;

        self.generic_open(path, &mut parent_inode_num, true, filetype.bits(), &mut 0)
    }


    /// Open a directory at the specified path and return the corresponding inode number.
    ///
    /// Opens a directory by searching for the given path starting from the root directory (`ROOT_INODE`).
    ///
    /// # Arguments
    /// * `path` - The path of the directory to open.
    ///
    /// # Returns
    /// * `Result<u32>` - Returns the inode number of the opened directory if successful.
    pub fn ext4_dir_open(
        &self,
        path: &str,
    ) -> Result<u32> {
        let mut parent_inode_num = ROOT_INODE;
        let filetype = InodeFileType::S_IFDIR;
        self.generic_open(path, &mut parent_inode_num, false, filetype.bits(), &mut 0)
    }

    /// Get dir entries of a inode
    ///
    /// Params:
    /// inode: u32 - inode number of the directory
    /// assert!(inode.is_dir());
    ///
    /// Returns:
    /// `Vec<Ext4DirEntry>` - list of directory entries
    pub fn ext4_dir_get_entries(&self, inode: u32) -> Vec<Ext4DirEntry> {
        let mut entries = self.dir_get_entries(inode);
        entries
    }

    /// Lookup a child entry under a specific directory inode.
    pub fn ext4_lookup_at(&self, parent: u32, name: &str) -> Result<u32> {
        let mut search_result = Ext4DirSearchResult::new(Ext4DirEntry::default());
        match self.dir_find_entry(parent, name, &mut search_result) {
            Ok(_) => Ok(search_result.dentry.inode),
            Err(e) => {
                if e.error() == Errno::ENOENT {
                    log::debug!(
                        "ext4_lookup_at miss: parent={} name='{}'",
                        parent,
                        name
                    );
                } else {
                    log::error!(
                        "ext4_lookup_at failed: parent={} name='{}' err={:?}",
                        parent,
                        name,
                        e
                    );
                }
                Err(e)
            }
        }
    }

    /// Create a file under a specific directory inode and return new inode number.
    pub fn ext4_create_at(&self, parent: u32, name: &str, mode: u16) -> Result<u32> {
        match self.create(parent, name, mode) {
            Ok(inode_ref) => Ok(inode_ref.inode_num),
            Err(e) => {
                log::error!(
                    "ext4_create_at failed: parent={} name='{}' mode={:#o} err={:?}",
                    parent,
                    name,
                    mode,
                    e
                );
                Err(e)
            }
        }
    }

    /// Create a directory under a specific directory inode and return new inode number.
    pub fn ext4_mkdir_at(&self, parent: u32, name: &str, mode: u16) -> Result<u32> {
        let mut search_result = Ext4DirSearchResult::new(Ext4DirEntry::default());
        if self.dir_find_entry(parent, name, &mut search_result).is_ok() {
            return_errno!(Errno::EEXIST);
        }
        let inode_ref = self.create(parent, name, mode)?;
        Ok(inode_ref.inode_num)
    }

    /// Create a directory without checking whether the name already exists and
    /// without scanning earlier directory blocks for free slots (append-only).
    /// Returns `(child_ino, dir_byte_offset)` where `dir_byte_offset` is the
    /// absolute byte offset of the new entry in the parent directory stream.
    /// The caller MUST guarantee the name is absent (e.g. via a fully-loaded
    /// directory cache) before calling this to avoid duplicate entries.
    pub fn ext4_mkdir_unchecked_at(&self, parent: u32, name: &str, mode: u16) -> Result<(u32, u64)> {
        let (inode_ref, dir_byte_offset) = self.create_unchecked(parent, name, mode)?;
        Ok((inode_ref.inode_num, dir_byte_offset))
    }

    /// Remove an empty directory using a pre-computed byte offset in the parent's
    /// directory stream, bypassing the O(n) `dir_find_entry` scan entirely.
    /// The caller must verify the directory is empty before calling this.
    pub fn ext4_rmdir_at_fast(
        &self,
        parent_ino: u32,
        child_ino: u32,
        dir_byte_offset: u64,
    ) -> Result<usize> {
        let mut parent_inode_ref = self.get_inode_ref(parent_ino);
        let mut child_inode_ref = self.get_inode_ref(child_ino);

        if !child_inode_ref.inode.is_dir() {
            return_errno_with_message!(Errno::ENOTDIR, "target is not a directory");
        }

        // Free child's data blocks
        self.truncate_inode(&mut child_inode_ref, 0)?;

        // Remove the entry from the parent block (O(1) — no scan)
        self.dir_remove_entry_at_offset(&mut parent_inode_ref, dir_byte_offset)?;

        // Adjust link counts (same as unlink for directories)
        let parent_links = parent_inode_ref.inode.links_count();
        if parent_links > 0 {
            parent_inode_ref.inode.set_links_count(parent_links - 1);
        }
        child_inode_ref.inode.set_links_count(0);

        self.write_back_inode(&mut child_inode_ref);
        self.write_back_inode(&mut parent_inode_ref);

        Ok(EOK)
    }

    /// Remove a file under a specific directory inode.
    pub fn ext4_unlink_at(&self, parent: u32, name: &str) -> Result<usize> {
        let child_inode = self.ext4_lookup_at(parent, name)?;
        let mut child_inode_ref = self.get_inode_ref(child_inode);
        if child_inode_ref.inode.is_dir() {
            return_errno_with_message!(Errno::EISDIR, "target is a directory");
        }

        let mut parent_inode_ref = self.get_inode_ref(parent);
        self.unlink(&mut parent_inode_ref, &mut child_inode_ref, name)?;
        Ok(EOK)
    }

    /// Remove an empty directory under a specific directory inode.
    pub fn ext4_rmdir_at(&self, parent: u32, name: &str) -> Result<usize> {
        self.dir_remove(parent, name)
    }

    /// Rename an entry under ext4.
    ///
    /// Current scope keeps semantics needed by phase4_part2:
    /// - same-directory rename
    /// - overwrite regular file / empty directory
    /// - reject cross-directory rename for now
    pub fn ext4_rename_at(
        &self,
        old_parent: u32,
        old_name: &str,
        new_parent: u32,
        new_name: &str,
    ) -> Result<usize> {
        if old_name == "." || old_name == ".." || new_name == "." || new_name == ".." {
            return_errno_with_message!(Errno::EISDIR, "rename on . or .. is not allowed");
        }

        if old_parent != new_parent {
            return_errno_with_message!(Errno::EXDEV, "cross-directory rename is not supported");
        }

        if old_name == new_name {
            return Ok(EOK);
        }

        let old_ino = self.ext4_lookup_at(old_parent, old_name)?;
        let old_inode_ref = self.get_inode_ref(old_ino);
        let old_is_dir = old_inode_ref.inode.is_dir();

        if let Ok(new_ino) = self.ext4_lookup_at(new_parent, new_name) {
            if new_ino == old_ino {
                return Ok(EOK);
            }

            let mut new_inode_ref = self.get_inode_ref(new_ino);
            if old_is_dir {
                if !new_inode_ref.inode.is_dir() {
                    return_errno_with_message!(Errno::ENOTDIR, "cannot overwrite non-directory");
                }
                if self.dir_has_entry(new_inode_ref.inode_num)? {
                    return_errno_with_message!(Errno::ENOTEMPTY, "directory not empty");
                }

                self.truncate_inode(&mut new_inode_ref, 0)?;
                let mut parent_inode_ref = self.get_inode_ref(new_parent);
                self.unlink(&mut parent_inode_ref, &mut new_inode_ref, new_name)?;
                self.write_back_inode(&mut parent_inode_ref);
            } else {
                if new_inode_ref.inode.is_dir() {
                    return_errno_with_message!(Errno::EISDIR, "cannot overwrite directory");
                }
                let mut parent_inode_ref = self.get_inode_ref(new_parent);
                self.unlink(&mut parent_inode_ref, &mut new_inode_ref, new_name)?;
            }
        }

        let mut parent_inode_ref = self.get_inode_ref(old_parent);
        self.dir_remove_entry(&mut parent_inode_ref, old_name)?;
        self.dir_add_entry(&mut parent_inode_ref, &old_inode_ref, new_name)?;

        Ok(EOK)
    }

    /// Read bytes from a file inode.
    pub fn ext4_read_at(&self, inode: u32, offset: usize, read_buf: &mut [u8]) -> Result<usize> {
        self.read_at(inode, offset, read_buf)
    }

    /// Map a logical block range to contiguous physical block ranges.
    pub fn ext4_map_blocks(
        &self,
        inode: u32,
        lblock_start: u32,
        lblock_count: u32,
    ) -> Result<Vec<SimpleBlockRange>> {
        self.map_blocks(inode, lblock_start, lblock_count)
    }

    /// Build a direct-read plan with a single inode load.
    pub fn ext4_plan_direct_read(
        &self,
        inode: u32,
        offset: usize,
        len: usize,
    ) -> Result<(usize, Vec<SimpleBlockRange>)> {
        self.plan_direct_read(inode, offset, len)
    }

    /// Prepare a write range by allocating missing blocks and returning the final mapping.
    pub fn ext4_prepare_write_at(
        &self,
        inode: u32,
        offset: usize,
        len: usize,
    ) -> Result<Vec<SimpleBlockRange>> {
        self.prepare_write_at(inode, offset, len)
    }

    /// Write bytes to a file inode.
    pub fn ext4_write_at(&self, inode: u32, offset: usize, write_buf: &[u8]) -> Result<usize> {
        self.write_at(inode, offset, write_buf)
    }

    /// Truncate a file inode to the specified size.
    pub fn ext4_truncate(&self, inode: u32, new_size: u64) -> Result<usize> {
        let mut inode_ref = self.get_inode_ref(inode);
        self.truncate_inode(&mut inode_ref, new_size)
    }

    /// Update inode timestamps (seconds since epoch).
    ///
    /// `None` means keeping the existing value.
    pub fn ext4_set_inode_times(
        &self,
        inode: u32,
        atime: Option<u32>,
        mtime: Option<u32>,
        ctime: Option<u32>,
    ) -> Result<usize> {
        let mut inode_ref = self.get_inode_ref(inode);
        if let Some(v) = atime {
            inode_ref.inode.set_atime(v);
        }
        if let Some(v) = mtime {
            inode_ref.inode.set_mtime(v);
        }
        if let Some(v) = ctime {
            inode_ref.inode.set_ctime(v);
        }
        self.write_back_inode(&mut inode_ref);
        Ok(EOK)
    }

    /// Update inode mode while keeping inode type bits unchanged.
    pub fn ext4_set_inode_mode(&self, inode: u32, mode: u16) -> Result<usize> {
        let mut inode_ref = self.get_inode_ref(inode);
        let current = inode_ref.inode.mode();
        let next = (current & EXT4_INODE_MODE_TYPE_MASK) | (mode & EXT4_INODE_MODE_PERM_MASK);
        inode_ref.inode.set_mode(next);
        self.write_back_inode(&mut inode_ref);
        Ok(EOK)
    }

    /// Update inode owner uid.
    pub fn ext4_set_inode_uid(&self, inode: u32, uid: u16) -> Result<usize> {
        let mut inode_ref = self.get_inode_ref(inode);
        inode_ref.inode.set_uid(uid);
        self.write_back_inode(&mut inode_ref);
        Ok(EOK)
    }

    /// Update inode group gid.
    pub fn ext4_set_inode_gid(&self, inode: u32, gid: u16) -> Result<usize> {
        let mut inode_ref = self.get_inode_ref(inode);
        inode_ref.inode.set_gid(gid);
        self.write_back_inode(&mut inode_ref);
        Ok(EOK)
    }

    /// Update inode device id (stored in i_faddr in current ext4_rs).
    pub fn ext4_set_inode_rdev(&self, inode: u32, rdev: u32) -> Result<usize> {
        let mut inode_ref = self.get_inode_ref(inode);
        inode_ref.inode.set_faddr(rdev);
        self.write_back_inode(&mut inode_ref);
        Ok(EOK)
    }

    /// Get simplified inode metadata.
    pub fn ext4_stat(&self, inode: u32) -> SimpleInodeMeta {
        let inode_ref = self.get_inode_ref(inode);
        let raw = inode_ref.inode;
        SimpleInodeMeta {
            ino: inode_ref.inode_num,
            mode: raw.mode(),
            file_type: raw.file_type().bits(),
            uid: raw.uid(),
            gid: raw.gid(),
            nlink: raw.links_count(),
            size: raw.size(),
            blocks: raw.blocks_count(),
            atime: raw.atime(),
            mtime: raw.mtime(),
            ctime: raw.ctime(),
            rdev: raw.faddr(),
            flags: raw.flags(),
        }
    }

    /// Get simplified directory entries under a directory inode.
    pub fn ext4_readdir(&self, inode: u32) -> Vec<SimpleDirEntry> {
        // Iterate directory blocks directly to avoid a fat intermediate Vec<(Ext4DirEntry, usize)>.
        // Ext4DirEntry is 264 bytes; for 65537 entries the intermediate Vec requires a single
        // 34 MB contiguous allocation. SimpleDirEntry is ~56 bytes, keeping peak around 7 MB.
        let block_size = self.super_block.block_size() as usize;
        let mut simple_entries = Vec::new();

        let inode_ref = self.get_inode_ref(inode);
        if !inode_ref.inode.is_dir() {
            return simple_entries;
        }

        let inode_size = inode_ref.inode.size();
        let total_blocks = (inode_size + block_size as u64 - 1) / block_size as u64;
        let mut iblock = 0u64;

        while iblock < total_blocks {
            if let Ok(fblock) = self.get_pblock_idx(&inode_ref, iblock as u32) {
                let ext4block = Block::load(
                    &self.block_device,
                    fblock as usize * block_size,
                    block_size,
                );
                let mut offset = 0usize;

                while offset < block_size - core::mem::size_of::<Ext4DirEntryTail>() {
                    let de: Ext4DirEntry = ext4block.read_offset_as(offset);
                    let rec_len = de.entry_len() as usize;
                    if rec_len == 0 || rec_len > block_size - offset {
                        break;
                    }
                    if !de.unused() {
                        let next_offset = iblock as usize * block_size + offset + rec_len;
                        simple_entries.push(SimpleDirEntry {
                            inode: de.inode,
                            de_type: de.get_de_type(),
                            name: de.get_name(),
                            next_offset,
                        });
                    }
                    offset += rec_len;
                }
            }
            iblock += 1;
        }

        simple_entries
    }

    /// Like `ext4_readdir` but also returns each entry's absolute byte offset in the
    /// directory stream, as `(name, ino, entry_byte_offset)`. Used to populate the
    /// kernel-layer directory cache with offsets for O(1) rmdir.
    pub fn ext4_readdir_with_offsets(&self, inode: u32) -> Vec<(String, u32, u64)> {
        // Iterate directory blocks directly to avoid building a fat Vec<(Ext4DirEntry, usize)>.
        // Ext4DirEntry is 264 bytes; for 65537 entries the intermediate Vec would require a
        // single 34 MB contiguous allocation (capacity doubles to 131072 × 272 bytes = 0x2200000).
        // Building Vec<(String, u32, u64)> directly keeps peak allocation at ~5 MB.
        let block_size = self.super_block.block_size() as usize;
        let mut result = Vec::new();

        let inode_ref = self.get_inode_ref(inode);
        if !inode_ref.inode.is_dir() {
            return result;
        }

        let inode_size = inode_ref.inode.size();
        let total_blocks = (inode_size + block_size as u64 - 1) / block_size as u64;
        let mut iblock = 0u64;

        while iblock < total_blocks {
            if let Ok(fblock) = self.get_pblock_idx(&inode_ref, iblock as u32) {
                let ext4block = Block::load(
                    &self.block_device,
                    fblock as usize * block_size,
                    block_size,
                );
                let mut offset = 0usize;

                while offset < block_size - core::mem::size_of::<Ext4DirEntryTail>() {
                    let de: Ext4DirEntry = ext4block.read_offset_as(offset);
                    let rec_len = de.entry_len() as usize;
                    if rec_len == 0 || rec_len > block_size - offset {
                        break;
                    }
                    if !de.unused() {
                        let entry_offset = (iblock as usize * block_size + offset) as u64;
                        result.push((de.get_name(), de.inode, entry_offset));
                    }
                    offset += rec_len;
                }
            }
            iblock += 1;
        }

        result
    }

    /// Read data from a file starting from a given offset.
    ///
    /// Reads data from the file starting at the specified inode (`ino`), with a given offset and size.
    ///
    /// # Arguments
    /// * `ino` - The inode number of the file to read from.
    /// * `size` - The number of bytes to read.
    /// * `offset` - The offset from where to start reading.
    ///
    /// # Returns
    /// * `Result<Vec<u8>>` - The data read from the file.
    pub fn ext4_file_read(
        &self,
        ino: u64,
        size: u32,
        offset: i64,
    ) -> Result<Vec<u8>> {
        let mut data = vec![0u8; size as usize];
        let read_size = self.read_at(ino as u32, offset as usize, &mut data)?;
        let r = data[..read_size].to_vec();
        Ok(r)
    }

    /// Write data to a file starting at a given offset.
    ///
    /// Writes data to the file starting at the specified inode (`ino`) and offset.
    ///
    /// # Arguments
    /// * `ino` - The inode number of the file to write to.
    /// * `offset` - The offset in the file where the data will be written.
    /// * `data` - The data to write to the file.
    ///
    /// # Returns
    /// * `Result<usize>` - The number of bytes written to the file.
    pub fn ext4_file_write(
        &self,
        ino: u64,
        offset: i64,
        data: &[u8],
    ) -> Result<usize> {
        let write_size = self.write_at(ino as u32, offset as usize, data)?;
        Ok(write_size)
    }

}
