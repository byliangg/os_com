// SPDX-License-Identifier: MPL-2.0

//! VFS `FileOps` and `Inode` trait implementations for the ext4 `Inode`.
//!
//! Translates VFS requests into ext4-internal operations: data I/O through the
//! page cache, attribute getters/setters, and the directory namespace
//! (create/link/unlink/rmdir/rename and symlink read/write). Special files
//! (devices, FIFOs, sockets) are deferred to a later phase: `mknod` returns
//! `EOPNOTSUPP`.

use core::time::Duration;

use aster_block::BLOCK_SIZE;
use device_id::DeviceId;

use crate::{
    fs::{
        file::{InodeMode, InodeType, StatusFlags},
        fs_impls::ext4::{FilePerm, Inode as Ext4Inode},
        utils::DirentVisitor,
        vfs::{
            file_system::FileSystem,
            inode::{Extension, FileOps, Inode, Metadata, MknodType, SymbolicLink},
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
        offset: usize,
        reader: &mut VmReader,
        status_flags: StatusFlags,
    ) -> Result<usize> {
        if status_flags.contains(StatusFlags::O_DIRECT) {
            // Buffered-only in Phase 2; O_DIRECT writes arrive with a later task.
            return_errno_with_message!(Errno::EOPNOTSUPP, "ext4 O_DIRECT write unimplemented");
        }
        self.write_at(offset, reader)
    }

    fn readdir_at(&self, offset: usize, visitor: &mut dyn DirentVisitor) -> Result<usize> {
        self.readdir_at(offset, visitor)
    }
}

impl Inode for Ext4Inode {
    fn size(&self) -> usize {
        self.size()
    }

    fn resize(&self, new_size: usize) -> Result<()> {
        self.resize(new_size)
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

    fn set_mode(&self, mode: InodeMode) -> Result<()> {
        self.set_mode(mode);
        Ok(())
    }

    fn owner(&self) -> Result<Uid> {
        Ok(Uid::new(self.uid()))
    }

    fn set_owner(&self, uid: Uid) -> Result<()> {
        self.set_owner(u32::from(uid));
        Ok(())
    }

    fn group(&self) -> Result<Gid> {
        Ok(Gid::new(self.gid()))
    }

    fn set_group(&self, gid: Gid) -> Result<()> {
        self.set_group(u32::from(gid));
        Ok(())
    }

    fn atime(&self) -> Duration {
        self.atime()
    }

    fn set_atime(&self, time: Duration) {
        self.set_atime(time);
    }

    fn mtime(&self) -> Duration {
        self.mtime()
    }

    fn set_mtime(&self, time: Duration) {
        self.set_mtime(time);
    }

    fn ctime(&self) -> Duration {
        self.ctime()
    }

    fn set_ctime(&self, time: Duration) {
        self.set_ctime(time);
    }

    fn sync_all(&self) -> Result<()> {
        // Flush the block-side metadata the allocator touched (bitmap/GDT/
        // superblock), then this inode's data pages + metadata with a barrier.
        let fs = self.fs()?;
        fs.sync_metadata()?;
        self.sync_data_and_meta()
    }

    fn sync_data(&self) -> Result<()> {
        let fs = self.fs()?;
        fs.sync_metadata()?;
        self.sync_data_and_meta()
    }

    fn page_cache(&self) -> Option<PageCache> {
        self.page_cache()
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>> {
        Ok(self.lookup(name)?)
    }

    fn create(&self, name: &str, type_: InodeType, mode: InodeMode) -> Result<Arc<dyn Inode>> {
        Ok(self.create(name, type_, mode.into())?)
    }

    fn mknod(&self, _name: &str, _mode: InodeMode, _type_: MknodType) -> Result<Arc<dyn Inode>> {
        // Special files (devices, FIFOs, sockets) are deferred to a later phase;
        // the internal `create` rejects them, so we do not even attempt it here.
        return_errno_with_message!(
            Errno::EOPNOTSUPP,
            "ext4 mknod (special files) unimplemented"
        );
    }

    fn link(&self, old: &Arc<dyn Inode>, name: &str) -> Result<()> {
        let old = old
            .downcast_ref::<Ext4Inode>()
            .ok_or_else(|| Error::with_message(Errno::EXDEV, "not same fs"))?;
        self.link(old, name)
    }

    fn unlink(&self, name: &str) -> Result<()> {
        self.unlink(name)
    }

    fn rmdir(&self, name: &str) -> Result<()> {
        self.rmdir(name)
    }

    fn rename(&self, old_name: &str, target: &Arc<dyn Inode>, new_name: &str) -> Result<()> {
        let target = target
            .downcast_ref::<Ext4Inode>()
            .ok_or_else(|| Error::with_message(Errno::EXDEV, "not same fs"))?;
        self.rename(old_name, target, new_name)
    }

    fn read_link(&self) -> Result<SymbolicLink> {
        self.read_link().map(SymbolicLink::Plain)
    }

    fn write_link(&self, target: &str) -> Result<()> {
        self.write_link(target)
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.fs().unwrap()
    }

    fn extension(&self) -> &Extension {
        self.extension()
    }
}

impl From<InodeMode> for FilePerm {
    fn from(mode: InodeMode) -> Self {
        Self::from_bits_truncate(mode.bits() as _)
    }
}

#[cfg(ktest)]
mod tests {
    use alloc::sync::Arc;

    use ostd::{mm::VmWriter, prelude::*};

    use crate::{
        fs::{
            file::{InodeMode, InodeType, StatusFlags},
            fs_impls::ext4::test_utils::{
                Ext4FixtureBuilder, make_dir_block, make_dir_inode, make_file_inode,
            },
            vfs::{file_system::FileSystem, inode::Inode},
        },
        process::{Gid, Uid},
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

    /// chmod/chown/chgrp through the VFS `Inode` trait update the in-memory
    /// metadata and persist across a write-back + reload, preserving the inode
    /// type bits.
    #[ktest]
    fn attr_writes_persist() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        f.write_data_block(100, b"x");
        f.write_raw_inode(11, &make_file_inode(100, 1));

        let inode = f.ext4.read_inode(11).unwrap();
        let dyn_inode: Arc<dyn Inode> = inode.clone();

        dyn_inode
            .set_mode(InodeMode::from_bits_truncate(0o600))
            .unwrap();
        dyn_inode.set_owner(Uid::new(4242)).unwrap();
        dyn_inode.set_group(Gid::new(8484)).unwrap();
        assert_eq!(dyn_inode.mode().unwrap().bits() & 0o777, 0o600);
        assert_eq!(dyn_inode.owner().unwrap(), Uid::new(4242));

        // Persist to the inode table and reload from disk.
        inode.sync_metadata().unwrap();
        let reloaded = f.ext4.read_inode(11).unwrap();
        assert_eq!(reloaded.mode().bits() & 0o777, 0o600);
        assert_eq!(reloaded.uid(), 4242);
        assert_eq!(reloaded.gid(), 8484);
        // chmod's RMW kept the on-disk type bits: still a regular file.
        assert_eq!(reloaded.inode_type(), InodeType::File);
    }
}
