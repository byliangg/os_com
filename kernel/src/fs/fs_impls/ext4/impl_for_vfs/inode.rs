// SPDX-License-Identifier: MPL-2.0

//! VFS `FileOps` and `Inode` trait implementations for the ext4 `Inode`.
//!
//! Phase 1 is read-only: read/lookup/readdir translate to ext4-internal
//! operations; mutating methods return `EROFS`; timestamp setters are no-ops.

use core::time::Duration;

use aster_block::BLOCK_SIZE;
use device_id::DeviceId;

use crate::{
    fs::{
        file::{InodeMode, InodeType, StatusFlags},
        fs_impls::ext4::Inode as Ext4Inode,
        utils::DirentVisitor,
        vfs::{
            file_system::FileSystem,
            inode::{Extension, FileOps, Inode, Metadata},
        },
    },
    prelude::*,
    process::{Gid, Uid},
    vm::page_cache::PageCache,
};

impl FileOps for Ext4Inode {
    fn read_at(
        &self,
        offset: usize,
        writer: &mut VmWriter,
        _status_flags: StatusFlags,
    ) -> Result<usize> {
        // Phase 1 serves O_DIRECT reads through the page cache as well.
        self.read_at(offset, writer)
    }

    fn write_at(
        &self,
        _offset: usize,
        _reader: &mut VmReader,
        _status_flags: StatusFlags,
    ) -> Result<usize> {
        return_errno_with_message!(Errno::EROFS, "ext4 is read-only in phase 1")
    }

    fn readdir_at(&self, offset: usize, visitor: &mut dyn DirentVisitor) -> Result<usize> {
        self.readdir_at(offset, visitor)
    }
}

impl Inode for Ext4Inode {
    fn size(&self) -> usize {
        self.size()
    }

    fn resize(&self, _new_size: usize) -> Result<()> {
        return_errno_with_message!(Errno::EROFS, "ext4 is read-only in phase 1")
    }

    fn metadata(&self) -> Metadata {
        let container_dev_id = self
            .fs()
            .map(|fs| fs.container_device_id())
            .unwrap_or_else(|_| DeviceId::null());
        Metadata {
            ino: self.ino() as u64,
            size: self.size(),
            optimal_block_size: BLOCK_SIZE,
            nr_sectors_allocated: self.sector_count() as usize,
            last_access_at: self.atime(),
            last_modify_at: self.mtime(),
            last_meta_change_at: self.ctime(),
            type_: self.inode_type(),
            mode: self.mode(),
            nr_hard_links: self.link_count() as usize,
            uid: Uid::new(self.uid()),
            gid: Gid::new(self.gid()),
            container_dev_id,
            self_dev_id: None,
            birth_at: self.crtime(),
        }
    }

    fn ino(&self) -> u64 {
        self.ino() as u64
    }

    fn type_(&self) -> InodeType {
        self.inode_type()
    }

    fn mode(&self) -> Result<InodeMode> {
        Ok(self.mode())
    }

    fn set_mode(&self, _mode: InodeMode) -> Result<()> {
        return_errno_with_message!(Errno::EROFS, "ext4 is read-only in phase 1")
    }

    fn owner(&self) -> Result<Uid> {
        Ok(Uid::new(self.uid()))
    }

    fn set_owner(&self, _uid: Uid) -> Result<()> {
        return_errno_with_message!(Errno::EROFS, "ext4 is read-only in phase 1")
    }

    fn group(&self) -> Result<Gid> {
        Ok(Gid::new(self.gid()))
    }

    fn set_group(&self, _gid: Gid) -> Result<()> {
        return_errno_with_message!(Errno::EROFS, "ext4 is read-only in phase 1")
    }

    fn atime(&self) -> Duration {
        self.atime()
    }

    fn set_atime(&self, _time: Duration) {}

    fn mtime(&self) -> Duration {
        self.mtime()
    }

    fn set_mtime(&self, _time: Duration) {}

    fn ctime(&self) -> Duration {
        self.ctime()
    }

    fn set_ctime(&self, _time: Duration) {}

    fn page_cache(&self) -> Option<PageCache> {
        self.page_cache()
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>> {
        Ok(self.lookup(name)?)
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.fs().unwrap()
    }

    fn extension(&self) -> &Extension {
        self.extension()
    }
}

#[cfg(ktest)]
mod tests {
    use alloc::sync::Arc;

    use ostd::{mm::VmWriter, prelude::*};

    use crate::fs::{
        file::{InodeType, StatusFlags},
        fs_impls::ext4::test_utils::{
            Ext4FixtureBuilder, make_dir_block, make_dir_inode, make_file_inode,
        },
        vfs::{file_system::FileSystem, inode::Inode},
    };

    /// Drives a mounted ext4 filesystem entirely through the VFS traits:
    /// `FileSystem` for stat, and `Inode`/`FileOps` for type, lookup, metadata,
    /// and reads.
    #[ktest]
    fn mount_via_vfs_and_read() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();

        // A directory (ino 12) with an entry "data" pointing at a file (ino 11).
        let dir_block = 102u32;
        let block = make_dir_block(&[(2, ".", 2), (2, "..", 2), (11, "data", 1)]);
        f.write_data_block(dir_block, &block);
        f.write_raw_inode(12, &make_dir_inode(dir_block));
        let content = b"vfs read works";
        f.write_data_block(103, content);
        f.write_raw_inode(11, &make_file_inode(103, content.len() as u32));

        // `FileSystem` trait.
        let fs: Arc<dyn FileSystem> = f.ext4.clone();
        assert_eq!(fs.name(), "ext4");
        assert_eq!(fs.sb().bsize, 4096);

        // `Inode` trait via dynamic dispatch.
        let dir_dyn: Arc<dyn Inode> = f.ext4.read_inode(12).unwrap();
        assert_eq!(dir_dyn.type_(), InodeType::Dir);

        let file_dyn = dir_dyn.lookup("data").unwrap();
        assert_eq!(file_dyn.ino(), 11);
        assert_eq!(file_dyn.size(), content.len());
        assert_eq!(file_dyn.metadata().type_, InodeType::File);

        // `FileOps::read_at` through the trait object.
        let mut buf = [0u8; 64];
        let mut writer = VmWriter::from(&mut buf[..content.len()]).to_fallible();
        let read = file_dyn
            .read_at(0, &mut writer, StatusFlags::empty())
            .unwrap();
        assert_eq!(read, content.len());
        assert_eq!(&buf[..content.len()], content);
    }
}
