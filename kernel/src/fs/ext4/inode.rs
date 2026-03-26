// SPDX-License-Identifier: MPL-2.0

use alloc::format;
use core::time::Duration;

use ext4_rs::{EXT4_ROOT_INODE, InodeFileType, SimpleInodeMeta};

use super::fs::Ext4Fs;
use crate::{
    fs::utils::{
        DirentVisitor, Extension, FileSystem, Inode, InodeIo, InodeMode, InodeType, Metadata,
        StatusFlags, SymbolicLink,
    },
    prelude::*,
    process::{Gid, Uid},
};

#[derive(Debug)]
pub(super) struct Ext4Inode {
    fs: Weak<Ext4Fs>,
    ino: u32,
    path: String,
    extension: Extension,
}

impl Ext4Inode {
    pub fn new(fs: Weak<Ext4Fs>, ino: u32, path: String) -> Self {
        Self {
            fs,
            ino,
            path,
            extension: Extension::new(),
        }
    }

    fn ext4_fs(&self) -> Result<Arc<Ext4Fs>> {
        self.fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "ext4 fs is dropped"))
    }

    fn join_child_path(&self, name: &str) -> String {
        if self.path.is_empty() {
            name.to_string()
        } else {
            format!("{}/{}", self.path, name)
        }
    }

    fn parent_path(&self) -> String {
        match self.path.rsplit_once('/') {
            Some((parent, _)) => parent.to_string(),
            None => String::new(),
        }
    }

    fn stat_meta(&self) -> SimpleInodeMeta {
        let Ok(fs) = self.ext4_fs() else {
            return SimpleInodeMeta {
                ino: self.ino,
                mode: InodeFileType::S_IFREG.bits(),
                file_type: InodeFileType::S_IFREG.bits(),
                uid: 0,
                gid: 0,
                nlink: 1,
                size: 0,
                blocks: 0,
                atime: 0,
                mtime: 0,
                ctime: 0,
                rdev: 0,
                flags: 0,
            };
        };
        fs.stat(self.ino).unwrap_or(SimpleInodeMeta {
            ino: self.ino,
            mode: InodeFileType::S_IFREG.bits(),
            file_type: InodeFileType::S_IFREG.bits(),
            uid: 0,
            gid: 0,
            nlink: 1,
            size: 0,
            blocks: 0,
            atime: 0,
            mtime: 0,
            ctime: 0,
            rdev: 0,
            flags: 0,
        })
    }

    fn type_from_mode(mode: u16) -> InodeType {
        InodeType::from_raw_mode(mode).unwrap_or(InodeType::Unknown)
    }

    fn type_from_dirent_type(de_type: u8) -> InodeType {
        match de_type {
            1 => InodeType::File,
            2 => InodeType::Dir,
            3 => InodeType::CharDevice,
            4 => InodeType::BlockDevice,
            5 => InodeType::NamedPipe,
            6 => InodeType::Socket,
            7 => InodeType::SymLink,
            _ => InodeType::Unknown,
        }
    }

    fn ext4_mode(type_: InodeType, mode: InodeMode) -> Result<u16> {
        let type_bits = match type_ {
            InodeType::File => InodeFileType::S_IFREG.bits(),
            InodeType::Dir => InodeFileType::S_IFDIR.bits(),
            InodeType::SymLink => InodeFileType::S_IFLNK.bits(),
            InodeType::CharDevice => InodeFileType::S_IFCHR.bits(),
            InodeType::BlockDevice => InodeFileType::S_IFBLK.bits(),
            InodeType::NamedPipe => InodeFileType::S_IFIFO.bits(),
            InodeType::Socket => InodeFileType::S_IFSOCK.bits(),
            InodeType::Unknown => {
                return_errno_with_message!(Errno::EINVAL, "unsupported inode type")
            }
        };
        Ok(type_bits | (mode.bits() as u16 & 0x0FFF))
    }
}

impl InodeIo for Ext4Inode {
    fn read_at(
        &self,
        offset: usize,
        writer: &mut VmWriter,
        _status_flags: StatusFlags,
    ) -> Result<usize> {
        if writer.avail() == 0 {
            return Ok(0);
        }
        if self.type_() == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }

        let fs = self.ext4_fs()?;
        let mut data = vec![0u8; writer.avail()];
        let read_len = fs.read_at(self.ino, offset, data.as_mut_slice())?;
        writer.write_fallible(&mut VmReader::from(&data[..read_len]).to_fallible())?;
        Ok(read_len)
    }

    fn write_at(
        &self,
        offset: usize,
        reader: &mut VmReader,
        _status_flags: StatusFlags,
    ) -> Result<usize> {
        if reader.remain() == 0 {
            return Ok(0);
        }
        if self.type_() != InodeType::File {
            return_errno!(Errno::EISDIR);
        }

        let mut data = vec![0u8; reader.remain()];
        reader.read_fallible(&mut VmWriter::from(data.as_mut_slice()).to_fallible())?;

        let fs = self.ext4_fs()?;
        fs.write_at(self.ino, offset, data.as_slice())
    }
}

impl Inode for Ext4Inode {
    fn size(&self) -> usize {
        self.stat_meta().size as usize
    }

    fn resize(&self, new_size: usize) -> Result<()> {
        let fs = self.ext4_fs()?;
        fs.truncate(self.ino, new_size as u64)
    }

    fn metadata(&self) -> Metadata {
        let meta = self.stat_meta();
        let dev = self.ext4_fs().map(|fs| fs.dev_id()).unwrap_or(0);
        Metadata {
            dev,
            ino: meta.ino as u64,
            size: meta.size as usize,
            blk_size: ext4_rs::BLOCK_SIZE,
            blocks: meta.blocks as usize,
            atime: Duration::from_secs(meta.atime as u64),
            mtime: Duration::from_secs(meta.mtime as u64),
            ctime: Duration::from_secs(meta.ctime as u64),
            type_: Self::type_from_mode(meta.mode),
            mode: InodeMode::from_bits_truncate(meta.mode as _),
            nlinks: meta.nlink as usize,
            uid: Uid::new(meta.uid as u32),
            gid: Gid::new(meta.gid as u32),
            rdev: meta.rdev as u64,
        }
    }

    fn atime(&self) -> Duration {
        Duration::from_secs(self.stat_meta().atime as u64)
    }

    fn set_atime(&self, _time: Duration) {}

    fn mtime(&self) -> Duration {
        Duration::from_secs(self.stat_meta().mtime as u64)
    }

    fn set_mtime(&self, _time: Duration) {}

    fn ctime(&self) -> Duration {
        Duration::from_secs(self.stat_meta().ctime as u64)
    }

    fn set_ctime(&self, _time: Duration) {}

    fn ino(&self) -> u64 {
        self.ino as u64
    }

    fn type_(&self) -> InodeType {
        Self::type_from_mode(self.stat_meta().mode)
    }

    fn mode(&self) -> Result<InodeMode> {
        Ok(InodeMode::from_bits_truncate(self.stat_meta().mode as _))
    }

    fn set_mode(&self, _mode: InodeMode) -> Result<()> {
        return_errno_with_message!(Errno::EOPNOTSUPP, "set_mode is not supported in stage1");
    }

    fn owner(&self) -> Result<Uid> {
        Ok(Uid::new(self.stat_meta().uid as u32))
    }

    fn set_owner(&self, _uid: Uid) -> Result<()> {
        return_errno_with_message!(Errno::EOPNOTSUPP, "set_owner is not supported in stage1");
    }

    fn group(&self) -> Result<Gid> {
        Ok(Gid::new(self.stat_meta().gid as u32))
    }

    fn set_group(&self, _gid: Gid) -> Result<()> {
        return_errno_with_message!(Errno::EOPNOTSUPP, "set_group is not supported in stage1");
    }

    fn create(&self, name: &str, type_: InodeType, mode: InodeMode) -> Result<Arc<dyn Inode>> {
        if self.type_() != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let fs = self.ext4_fs()?;
        let child_ino = if type_ == InodeType::Dir {
            let ext4_mode = Self::ext4_mode(type_, mode)?;
            fs.mkdir_at(self.ino, name, ext4_mode)?
        } else if type_ == InodeType::File {
            let ext4_mode = Self::ext4_mode(type_, mode)?;
            fs.create_at(self.ino, name, ext4_mode)?
        } else {
            return_errno_with_message!(Errno::EOPNOTSUPP, "unsupported inode type in stage1");
        };

        Ok(fs.make_inode(child_ino, self.join_child_path(name)))
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>> {
        let fs = self.ext4_fs()?;

        if name == "." {
            return Ok(fs.make_inode(self.ino, self.path.clone()));
        }
        if name == ".." {
            let parent_path = self.parent_path();
            let parent_ino = if self.path.is_empty() {
                EXT4_ROOT_INODE
            } else if parent_path.is_empty() {
                EXT4_ROOT_INODE
            } else {
                fs.dir_open(parent_path.as_str())?
            };
            return Ok(fs.make_inode(parent_ino, parent_path));
        }

        let child_ino = fs.lookup_at(self.ino, name)?;
        Ok(fs.make_inode(child_ino, self.join_child_path(name)))
    }

    fn readdir_at(&self, offset: usize, visitor: &mut dyn DirentVisitor) -> Result<usize> {
        if self.type_() != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let fs = self.ext4_fs()?;
        let entries = fs.readdir(self.ino)?;
        let mut iterate_offset = offset;
        let start_idx = if offset == 0 {
            0
        } else {
            entries
                .iter()
                .position(|entry| entry.next_offset > offset)
                .unwrap_or(entries.len())
        };

        if start_idx >= entries.len() {
            return Ok(0);
        }

        let mut visited = false;
        for entry in entries.iter().skip(start_idx) {
            let type_ = Self::type_from_dirent_type(entry.de_type);
            if let Err(err) = visitor.visit(
                entry.name.as_str(),
                entry.inode as u64,
                type_,
                entry.next_offset,
            ) {
                if !visited {
                    return Err(err);
                }
                break;
            }
            visited = true;
            iterate_offset = entry.next_offset;
        }
        Ok(iterate_offset.saturating_sub(offset))
    }

    fn unlink(&self, name: &str) -> Result<()> {
        if self.type_() != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }
        let fs = self.ext4_fs()?;
        fs.unlink_at(self.ino, name)
    }

    fn rmdir(&self, name: &str) -> Result<()> {
        if self.type_() != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }
        let fs = self.ext4_fs()?;
        fs.rmdir_at(self.ino, name)
    }

    fn read_link(&self) -> Result<SymbolicLink> {
        return_errno_with_message!(Errno::EOPNOTSUPP, "symlink is not supported in stage1");
    }

    fn sync_all(&self) -> Result<()> {
        self.ext4_fs()?.sync()
    }

    fn sync_data(&self) -> Result<()> {
        self.ext4_fs()?.sync()
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.ext4_fs().unwrap()
    }

    fn extension(&self) -> &Extension {
        &self.extension
    }
}
