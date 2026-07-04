// SPDX-License-Identifier: MPL-2.0

//! VFS `FileOps` and `Inode` trait implementations for the ext4 `Inode`.
//!
//! Translates VFS requests into ext4-internal operations: data I/O through the
//! page cache, attribute getters/setters, the directory namespace
//! (create/mknod/link/unlink/rmdir/rename and symlink read/write), and the
//! per-open layer — special files open into their live kernel objects, and
//! directory fds get a thin [`Ext4DirFile`] shim so ioctls
//! (`EXT4_IOC_SHUTDOWN` first) can reach the filesystem.

use core::time::Duration;

use aster_block::BLOCK_SIZE;
use device_id::DeviceId;
use ostd::{mm::VmIo, task::Task};

use crate::{
    device,
    events::IoEvents,
    fs::{
        file::{AccessMode, InodeMode, InodeType, PerOpenFileOps, StatusFlags},
        fs_impls::ext4::{FilePerm, Inode as Ext4Inode, fs::GoingDown},
        utils::DirentVisitor,
        vfs::{
            file_system::FileSystem,
            inode::{Extension, FileOps, Inode, Metadata, MknodType, SymbolicLink},
        },
    },
    prelude::*,
    process::{
        Gid, Uid,
        credentials::capabilities::CapSet,
        posix_thread::AsPosixThread,
        signal::{PollHandle, Pollable},
    },
    security::lsm::hooks as lsm_hooks,
    util::ioctl::{RawIoctl, dispatch_ioctl},
    vm::page_cache::PageCache,
};

/// Applies Linux's default `relatime` policy on access: bump atime only when
/// it is not newer than mtime/ctime, or is at least a day stale.
///
/// The VFS mount layer models the per-mount atime options
/// (noatime/relatime/strictatime/nodiratime) but nothing plumbs them down to
/// filesystem implementations yet, so ext4 applies the default-mount policy
/// itself (exfat likewise updates times fs-locally). `<=` instead of Linux's
/// strict `<` tolerates second-granularity timestamps: a file created and
/// written within one second must still get its first-access update.
fn touch_atime_relatime(inode: &Ext4Inode) {
    const A_DAY: Duration = Duration::from_secs(24 * 60 * 60);
    let atime = inode.atime();
    let now = crate::fs::fs_impls::ext4::utils::now();
    if atime <= inode.mtime() || atime <= inode.ctime() || now.saturating_sub(atime) >= A_DAY {
        inode.set_atime(now);
    }
}

impl FileOps for Ext4Inode {
    fn read_at(
        &self,
        offset: usize,
        writer: &mut VmWriter,
        _status_flags: StatusFlags,
    ) -> Result<usize> {
        // Phase 1 serves O_DIRECT reads through the page cache as well.
        let len = self.read_at(offset, writer)?;
        touch_atime_relatime(self);
        Ok(len)
    }

    fn write_at(
        &self,
        offset: usize,
        reader: &mut VmReader,
        status_flags: StatusFlags,
    ) -> Result<usize> {
        if status_flags.contains(StatusFlags::O_DIRECT) {
            // Buffered-only: real O_DIRECT (bypassing the page cache) is P9
            // performance work at the earliest, and outside the current
            // requirements. P5's xfstests runs exclude the direct-IO groups.
            return_errno_with_message!(Errno::EOPNOTSUPP, "ext4 O_DIRECT write unimplemented");
        }
        let len = self.write_at(offset, reader)?;
        // O_SYNC/O_DSYNC: write(2) on such an fd must not return before the
        // data (and for O_SYNC, the metadata) is durable — silently ignoring
        // the flags turns every O_SYNC write into a durability lie that a
        // crash harness immediately exposes. Linux opens O_SYNC as
        // __O_SYNC|O_DSYNC, so the O_SYNC check must come first. (sync_data
        // currently equals sync_all; it narrows when P7 enables datasync_tid.)
        if status_flags.contains(StatusFlags::O_SYNC) {
            Inode::sync_all(self)?;
        } else if status_flags.contains(StatusFlags::O_DSYNC) {
            Inode::sync_data(self)?;
        }
        Ok(len)
    }

    fn readdir_at(&self, offset: usize, visitor: &mut dyn DirentVisitor) -> Result<usize> {
        let count = self.readdir_at(offset, visitor)?;
        touch_atime_relatime(self);
        Ok(count)
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
            self_dev_id: self.device_id().and_then(DeviceId::from_encoded_u64),
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

    fn open(
        &self,
        access_mode: AccessMode,
        status_flags: StatusFlags,
    ) -> Option<Result<Box<dyn PerOpenFileOps>>> {
        match self.inode_type() {
            // Special files route to their live kernel objects, like ext2.
            inode_type @ (InodeType::BlockDevice | InodeType::CharDevice) => {
                let Some(device_id) = self.device_id().and_then(DeviceId::from_encoded_u64) else {
                    return Some(Err(Error::with_message(
                        Errno::ENODEV,
                        "the device ID is invalid",
                    )));
                };
                let device_type = inode_type
                    .device_type()
                    .expect("BlockDevice and CharDevice always have a device type");
                let Some(device) = device::lookup(device_type, device_id) else {
                    return Some(Err(Error::with_message(
                        Errno::ENODEV,
                        "the required device ID does not exist",
                    )));
                };
                Some(device.open())
            }
            InodeType::NamedPipe => {
                let pipe = self.pipe().expect("NamedPipe inode must have a pipe");
                Some(pipe.open_named(access_mode, status_flags))
            }
            // Directories get a thin per-open shim so ioctls (EXT4_IOC_SHUTDOWN
            // first — xfstests' godown fires it at the mountpoint fd) can reach
            // the filesystem; regular files stay on the direct inode path (a
            // per-open object would bypass the handle layer's O_APPEND offset
            // repositioning — a known VFS limitation).
            InodeType::Dir => {
                let Some(inode) = self.self_arc() else {
                    return Some(Err(Error::with_message(
                        Errno::EIO,
                        "inode is being dropped",
                    )));
                };
                Some(Ok(Box::new(Ext4DirFile { inode })))
            }
            _ => None,
        }
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>> {
        Ok(self.lookup(name)?)
    }

    fn create(&self, name: &str, type_: InodeType, mode: InodeMode) -> Result<Arc<dyn Inode>> {
        Ok(self.create(name, type_, mode.into())?)
    }

    fn create_symlink(&self, name: &str, mode: InodeMode, target: &str) -> Result<Arc<dyn Inode>> {
        // One transaction for inode + target (Linux ext4_symlink) — the VFS
        // default's create-then-write_link window persists a target-less
        // symlink on a crash, which fsck rejects.
        Ok(self.create_symlink(name, mode.into(), target)?)
    }

    fn mknod(&self, name: &str, mode: InodeMode, type_: MknodType) -> Result<Arc<dyn Inode>> {
        // Validate the user-supplied device number at the boundary: the ext4
        // on-disk encoding holds a 12-bit major / 20-bit minor, and an
        // unfittable id would be silently truncated into a DIFFERENT device's
        // rdev (review finding). `DeviceId::from_encoded_u64` is the same
        // check the read side (`open`, `metadata`) already applies.
        let checked = |device_id: u64| -> Result<u64> {
            DeviceId::from_encoded_u64(device_id)
                .ok_or_else(|| Error::with_message(Errno::EINVAL, "device number out of range"))?;
            Ok(device_id)
        };
        let new_inode = match type_ {
            MknodType::CharDevice(device_id) => self.create_with_device(
                name,
                InodeType::CharDevice,
                mode.into(),
                checked(device_id)?,
            )?,
            MknodType::BlockDevice(device_id) => self.create_with_device(
                name,
                InodeType::BlockDevice,
                mode.into(),
                checked(device_id)?,
            )?,
            MknodType::NamedPipe => self.create(name, InodeType::NamedPipe, mode.into())?,
        };
        Ok(new_inode)
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

/// The per-open shim for ext4 **directory** fds.
///
/// Its only reason to exist is `ioctl`: the handle layer dispatches ioctls
/// exclusively to a per-open object, and ext4 previously provided none, so
/// every ioctl died `ENOTTY` before reaching the filesystem. Data paths
/// forward straight back to the inode's `FileOps`, keeping directory reads
/// and `readdir` byte-identical to the shim-less behavior.
struct Ext4DirFile {
    inode: Arc<Ext4Inode>,
}

impl FileOps for Ext4DirFile {
    fn read_at(
        &self,
        offset: usize,
        writer: &mut VmWriter,
        status_flags: StatusFlags,
    ) -> Result<usize> {
        FileOps::read_at(self.inode.as_ref(), offset, writer, status_flags)
    }

    fn write_at(
        &self,
        offset: usize,
        reader: &mut VmReader,
        status_flags: StatusFlags,
    ) -> Result<usize> {
        FileOps::write_at(self.inode.as_ref(), offset, reader, status_flags)
    }

    fn readdir_at(&self, offset: usize, visitor: &mut dyn DirentVisitor) -> Result<usize> {
        FileOps::readdir_at(self.inode.as_ref(), offset, visitor)
    }
}

impl Pollable for Ext4DirFile {
    fn poll(&self, mask: IoEvents, _poller: Option<&mut PollHandle>) -> IoEvents {
        // Same readiness the handle layer reports without a per-open object.
        (IoEvents::IN | IoEvents::OUT) & mask
    }
}

impl PerOpenFileOps for Ext4DirFile {
    fn check_seekable(&self) -> Result<()> {
        Ok(())
    }

    fn is_offset_aware(&self) -> bool {
        true
    }

    fn ioctl(&self, raw_ioctl: RawIoctl) -> Result<i32> {
        use ioctl_defs::*;

        dispatch_ioctl!(match raw_ioctl {
            _cmd @ Shutdown => {
                // Linux gates the shutdown ioctl on CAP_SYS_ADMIN.
                ensure_sys_admin()?;
                // The `_IOR`-encoded argument is *read* from userspace — an
                // XFS-inherited quirk of this ioctl's encoding (Linux uses
                // `get_user` despite the "read" direction).
                let flags: u32 = crate::context::current_userspace!().read_val(raw_ioctl.arg())?;
                let fs = self.inode.fs()?;
                fs.shutdown(GoingDown::try_from(flags)?)?;
                Ok(0)
            }
            _ => return_errno_with_message!(Errno::ENOTTY, "the ioctl command is unknown"),
        })
    }
}

/// Errors `EPERM` unless the current thread holds `CAP_SYS_ADMIN` in its user
/// namespace (the gate Linux applies to `EXT4_IOC_SHUTDOWN`).
fn ensure_sys_admin() -> Result<()> {
    let Some(task) = Task::current() else {
        return_errno_with_message!(Errno::EPERM, "no current task");
    };
    let Some(posix_thread) = task.as_posix_thread() else {
        return_errno_with_message!(Errno::EPERM, "not a POSIX thread");
    };
    let Some(thread_local) = task.as_thread_local() else {
        return_errno_with_message!(Errno::EPERM, "no thread-local state");
    };
    let user_ns = thread_local.borrow_user_ns();
    lsm_hooks::on_capable(lsm_hooks::CapableContext::new(
        user_ns.as_ref(),
        posix_thread,
        CapSet::SYS_ADMIN,
    ))
}

mod ioctl_defs {
    use crate::util::ioctl::{OutData, ioc};

    /// `EXT4_IOC_SHUTDOWN`: `_IOR('X', 125, __u32)` — the same wire value as
    /// `XFS_IOC_GOINGDOWN` (0x8004587d), which is what xfstests' `godown`
    /// sends. The direction says "read" but the flag argument is fetched
    /// *from* userspace (an XFS-inherited encoding quirk), so the handler
    /// reads it via the raw pointer instead of this type's `write`.
    pub type Shutdown = ioc!(EXT4_IOC_SHUTDOWN, b'X', 125, OutData<u32>);
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
