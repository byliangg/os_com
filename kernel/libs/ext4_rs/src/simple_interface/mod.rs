use core::panic::RefUnwindSafe;

use crate::prelude::*;

use crate::ext4_defs::*;
use crate::return_errno;
use crate::return_errno_with_message;
use crate::utils::path_check;

// export some definitions
pub use crate::ext4_defs::Ext4;
pub use crate::ext4_defs::BLOCK_SIZE;
pub use crate::ext4_defs::BlockDevice;
pub use crate::ext4_defs::InodeFileType;
pub use crate::ext4_defs::ROOT_INODE as EXT4_ROOT_INODE;

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
        self.dir_find_entry(parent, name, &mut search_result)?;
        Ok(search_result.dentry.inode)
    }

    /// Create a file under a specific directory inode and return new inode number.
    pub fn ext4_create_at(&self, parent: u32, name: &str, mode: u16) -> Result<u32> {
        let inode_ref = self.create(parent, name, mode)?;
        Ok(inode_ref.inode_num)
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

    /// Remove a file under a specific directory inode.
    pub fn ext4_unlink_at(&self, parent: u32, name: &str) -> Result<usize> {
        let child_inode = self.ext4_lookup_at(parent, name)?;
        let mut child_inode_ref = self.get_inode_ref(child_inode);
        if child_inode_ref.inode.is_dir() {
            return_errno_with_message!(Errno::EISDIR, "target is a directory");
        }

        let child_link_cnt = child_inode_ref.inode.links_count();
        if child_link_cnt == 1 {
            self.truncate_inode(&mut child_inode_ref, 0)?;
        }

        let mut parent_inode_ref = self.get_inode_ref(parent);
        self.unlink(&mut parent_inode_ref, &mut child_inode_ref, name)?;
        Ok(EOK)
    }

    /// Remove an empty directory under a specific directory inode.
    pub fn ext4_rmdir_at(&self, parent: u32, name: &str) -> Result<usize> {
        self.dir_remove(parent, name)
    }

    fn update_dotdot_for_moved_dir(
        &self,
        moved_dir: &Ext4InodeRef,
        new_parent: u32,
    ) -> Result<()> {
        let block_size = self.super_block.block_size() as usize;
        let mut search_result = Ext4DirSearchResult::new(Ext4DirEntry::default());
        self.dir_find_entry(moved_dir.inode_num, "..", &mut search_result)?;

        let mut block = Block::load(&self.block_device, search_result.pblock_id * block_size);
        let dotdot: &mut Ext4DirEntry = block.read_offset_as_mut(search_result.offset);
        dotdot.inode = new_parent;

        self.dir_set_csum(&mut block, moved_dir.inode.generation());
        block.sync_blk_to_disk(&self.block_device);
        Ok(())
    }

    fn is_dir_descendant_of(&self, mut node: u32, ancestor: u32) -> Result<bool> {
        if node == ancestor {
            return Ok(true);
        }

        for _ in 0..1024 {
            if node == ROOT_INODE {
                return Ok(false);
            }

            let mut search_result = Ext4DirSearchResult::new(Ext4DirEntry::default());
            self.dir_find_entry(node, "..", &mut search_result)?;
            let parent = search_result.dentry.inode;

            if parent == ancestor {
                return Ok(true);
            }
            if parent == node {
                return Ok(false);
            }
            node = parent;
        }

        return_errno_with_message!(Errno::EINVAL, "directory ancestry loop detected");
    }

    /// Rename an entry under ext4.
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

        if old_parent == new_parent && old_name == new_name {
            return Ok(EOK);
        }

        let old_ino = self.ext4_lookup_at(old_parent, old_name)?;
        let old_inode_ref = self.get_inode_ref(old_ino);
        let old_is_dir = old_inode_ref.inode.is_dir();
        let same_parent = old_parent == new_parent;
        let mut replaced_dir = false;

        if old_is_dir && self.is_dir_descendant_of(new_parent, old_ino)? {
            return_errno_with_message!(Errno::EINVAL, "cannot move directory into its subtree");
        }

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

                replaced_dir = true;
                self.truncate_inode(&mut new_inode_ref, 0)?;
                let mut parent_inode_ref = self.get_inode_ref(new_parent);
                self.unlink(&mut parent_inode_ref, &mut new_inode_ref, new_name)?;
                self.write_back_inode(&mut parent_inode_ref);
            } else {
                if new_inode_ref.inode.is_dir() {
                    return_errno_with_message!(Errno::EISDIR, "cannot overwrite directory");
                }

                if new_inode_ref.inode.links_count() == 1 {
                    self.truncate_inode(&mut new_inode_ref, 0)?;
                }
                let mut parent_inode_ref = self.get_inode_ref(new_parent);
                self.unlink(&mut parent_inode_ref, &mut new_inode_ref, new_name)?;
            }
        }

        if same_parent {
            let mut parent_inode_ref = self.get_inode_ref(old_parent);
            self.dir_remove_entry(&mut parent_inode_ref, old_name)?;
            if let Err(err) = self.dir_add_entry(&mut parent_inode_ref, &old_inode_ref, new_name) {
                let _ = self.dir_add_entry(&mut parent_inode_ref, &old_inode_ref, old_name);
                self.write_back_inode(&mut parent_inode_ref);
                return Err(err);
            }

            // unlink() on an overwritten empty dir decrements nlink;
            // compensate when we place a directory back under the same parent.
            if old_is_dir && replaced_dir {
                let links = parent_inode_ref.inode.links_count();
                parent_inode_ref.inode.set_links_count(links.saturating_add(1));
            }
            self.write_back_inode(&mut parent_inode_ref);
            return Ok(EOK);
        }

        let mut old_parent_inode_ref = self.get_inode_ref(old_parent);
        self.dir_remove_entry(&mut old_parent_inode_ref, old_name)?;

        let mut new_parent_inode_ref = self.get_inode_ref(new_parent);
        if let Err(err) = self.dir_add_entry(&mut new_parent_inode_ref, &old_inode_ref, new_name) {
            let _ = self.dir_add_entry(&mut old_parent_inode_ref, &old_inode_ref, old_name);
            self.write_back_inode(&mut old_parent_inode_ref);
            self.write_back_inode(&mut new_parent_inode_ref);
            return Err(err);
        }

        if old_is_dir {
            let old_links = old_parent_inode_ref.inode.links_count();
            if old_links > 0 {
                old_parent_inode_ref.inode.set_links_count(old_links - 1);
            }
            let new_links = new_parent_inode_ref.inode.links_count();
            new_parent_inode_ref
                .inode
                .set_links_count(new_links.saturating_add(1));

            self.update_dotdot_for_moved_dir(&old_inode_ref, new_parent)?;
        }

        self.write_back_inode(&mut old_parent_inode_ref);
        self.write_back_inode(&mut new_parent_inode_ref);

        Ok(EOK)
    }

    /// Read bytes from a file inode.
    pub fn ext4_read_at(&self, inode: u32, offset: usize, read_buf: &mut [u8]) -> Result<usize> {
        self.read_at(inode, offset, read_buf)
    }

    /// Write bytes to a file inode.
    pub fn ext4_write_at(&self, inode: u32, offset: usize, write_buf: &[u8]) -> Result<usize> {
        self.write_at(inode, offset, write_buf)
    }

    /// Truncate a file inode to the specified size.
    pub fn ext4_truncate(&self, inode: u32, new_size: u64) -> Result<usize> {
        let mut inode_ref = self.get_inode_ref(inode);
        if new_size > inode_ref.inode.size() {
            return_errno_with_message!(Errno::EFBIG, "extend by truncate is not supported");
        }
        self.truncate_inode(&mut inode_ref, new_size)
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
        let entries = self.dir_get_entries_with_next_offset(inode);
        let mut simple_entries = Vec::new();
        for (entry, next_offset) in entries {
            simple_entries.push(SimpleDirEntry {
                inode: entry.inode,
                de_type: entry.get_de_type(),
                name: entry.get_name(),
                next_offset,
            });
        }
        simple_entries
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
