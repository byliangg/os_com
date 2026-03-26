// SPDX-License-Identifier: MPL-2.0

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicBool, Ordering};

use aster_block::{BlockDevice, SECTOR_SIZE};
use ext4_rs::{
    BLOCK_SIZE as EXT4_BLOCK_SIZE, BlockDevice as Ext4BlockDevice, EXT4_ROOT_INODE, Ext4,
    SimpleDirEntry, SimpleInodeMeta,
};
use ostd::mm::VmIo;

use crate::{
    fs::{
        registry::{FsProperties, FsType},
        utils::{FileSystem, FsEventSubscriberStats, FsFlags, Inode, NAME_MAX, SuperBlock},
    },
    prelude::*,
};

const EXT4_MAGIC: u64 = 0xEF53;
const EXT4_SUPERBLOCK_OFFSET: usize = 1024;
const EXT4_SB_LOG_BLOCK_SIZE_OFFSET: usize = 24;
const EXT4_SB_BLOCKS_PER_GROUP_OFFSET: usize = 32;
const EXT4_SB_INODES_PER_GROUP_OFFSET: usize = 40;
const EXT4_SB_MAGIC_OFFSET: usize = 56;
const EXT4_SB_DESC_SIZE_OFFSET: usize = 254;

#[derive(Debug, Default)]
struct DirEntryCache {
    loaded: bool,
    entries: BTreeMap<String, u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirLookupCacheResult {
    Hit(u32),
    Miss,
    Unknown,
}

#[derive(Debug)]
struct KernelBlockDeviceAdapter {
    inner: Arc<dyn BlockDevice>,
    io_failed: AtomicBool,
}

impl KernelBlockDeviceAdapter {
    fn new(inner: Arc<dyn BlockDevice>) -> Self {
        Self {
            inner,
            io_failed: AtomicBool::new(false),
        }
    }

    #[inline]
    fn align_down(offset: usize) -> usize {
        offset / SECTOR_SIZE * SECTOR_SIZE
    }

    #[inline]
    fn align_up(offset: usize) -> usize {
        offset.div_ceil(SECTOR_SIZE) * SECTOR_SIZE
    }

    #[inline]
    fn mark_io_failure(&self) {
        self.io_failed.store(true, Ordering::Release);
    }

    fn clear_io_failure(&self) {
        self.io_failed.store(false, Ordering::Release);
    }

    fn consume_io_failure(&self) -> bool {
        self.io_failed.swap(false, Ordering::AcqRel)
    }

    fn device_size_bytes(&self) -> usize {
        self.inner.metadata().nr_sectors.saturating_mul(SECTOR_SIZE)
    }
}

impl Ext4BlockDevice for KernelBlockDeviceAdapter {
    fn read_offset(&self, offset: usize) -> Vec<u8> {
        let dev_size = self.device_size_bytes();
        let Some(read_end) = offset.checked_add(EXT4_BLOCK_SIZE) else {
            self.mark_io_failure();
            error!("ext4 block read overflow at offset {}", offset);
            return vec![0u8; EXT4_BLOCK_SIZE];
        };
        if read_end > dev_size {
            self.mark_io_failure();
            error!(
                "ext4 block read out of range: offset={} len={} device_size={}",
                offset, EXT4_BLOCK_SIZE, dev_size
            );
            return vec![0u8; EXT4_BLOCK_SIZE];
        }

        let aligned_start = Self::align_down(offset);
        let aligned_end = Self::align_up(offset + EXT4_BLOCK_SIZE);
        let aligned_len = aligned_end - aligned_start;

        let mut aligned = vec![0u8; aligned_len];
        let mut writer = VmWriter::from(aligned.as_mut_slice()).to_fallible();
        if let Err(err) = self.inner.read(aligned_start, &mut writer) {
            self.mark_io_failure();
            error!("ext4 block read failed at offset {}: {:?}", offset, err);
            return vec![0u8; EXT4_BLOCK_SIZE];
        }

        let start = offset - aligned_start;
        aligned[start..start + EXT4_BLOCK_SIZE].to_vec()
    }

    fn write_offset(&self, offset: usize, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        let dev_size = self.device_size_bytes();
        let Some(write_end) = offset.checked_add(data.len()) else {
            self.mark_io_failure();
            error!("ext4 block write overflow at offset {} len={}", offset, data.len());
            return;
        };
        if write_end > dev_size {
            self.mark_io_failure();
            error!(
                "ext4 block write out of range: offset={} len={} device_size={}",
                offset, data.len(), dev_size
            );
            return;
        }

        let aligned_start = Self::align_down(offset);
        let aligned_end = Self::align_up(offset + data.len());
        let aligned_len = aligned_end - aligned_start;
        let mut aligned = vec![0u8; aligned_len];

        // Preserve neighboring bytes when ext4_rs issues unaligned writes.
        if aligned_start != offset || aligned_len != data.len() {
            let mut writer = VmWriter::from(aligned.as_mut_slice()).to_fallible();
            if let Err(err) = self.inner.read(aligned_start, &mut writer) {
                self.mark_io_failure();
                error!(
                    "ext4 block pre-read failed at offset {}: {:?}",
                    aligned_start, err
                );
                return;
            }
        }

        let start = offset - aligned_start;
        aligned[start..start + data.len()].copy_from_slice(data);

        let mut reader = VmReader::from(aligned.as_slice()).to_fallible();
        if let Err(err) = self.inner.write(aligned_start, &mut reader) {
            self.mark_io_failure();
            error!("ext4 block write failed at offset {}: {:?}", offset, err);
        }
    }
}

pub(super) struct Ext4Fs {
    inner: Mutex<Ext4>,
    block_device: Arc<dyn BlockDevice>,
    adapter: Arc<KernelBlockDeviceAdapter>,
    dir_entry_cache: Mutex<BTreeMap<u32, DirEntryCache>>,
    fs_event_subscriber_stats: FsEventSubscriberStats,
    self_ref: Weak<Self>,
}

impl Ext4Fs {
    pub fn open(block_device: Arc<dyn BlockDevice>) -> Arc<Self> {
        let adapter = Arc::new(KernelBlockDeviceAdapter::new(block_device.clone()));
        let ext4 = Ext4::open(adapter.clone());

        Arc::new_cyclic(|weak_ref| Self {
            inner: Mutex::new(ext4),
            block_device,
            adapter,
            dir_entry_cache: Mutex::new(BTreeMap::new()),
            fs_event_subscriber_stats: FsEventSubscriberStats::new(),
            self_ref: weak_ref.clone(),
        })
    }

    pub(super) fn lock_inner(&self) -> MutexGuard<'_, Ext4> {
        self.inner.lock()
    }

    fn prepare_ext4_io(&self) {
        self.adapter.clear_io_failure();
    }

    fn finish_ext4_io(&self) -> Result<()> {
        if self.adapter.consume_io_failure() {
            return_errno_with_message!(Errno::EIO, "ext4 block I/O failure");
        }
        Ok(())
    }

    pub(super) fn run_ext4<T>(
        &self,
        f: impl FnOnce(&Ext4) -> core::result::Result<T, ext4_rs::Ext4Error>,
    ) -> Result<T> {
        self.prepare_ext4_io();
        let result = {
            let inner = self.lock_inner();
            f(&inner).map_err(map_ext4_error)?
        };
        self.finish_ext4_io()?;
        Ok(result)
    }

    pub(super) fn run_ext4_noerr<T>(&self, f: impl FnOnce(&Ext4) -> T) -> Result<T> {
        self.prepare_ext4_io();
        let result = {
            let inner = self.lock_inner();
            f(&inner)
        };
        self.finish_ext4_io()?;
        Ok(result)
    }

    pub(super) fn stat(&self, ino: u32) -> Result<SimpleInodeMeta> {
        self.run_ext4_noerr(|ext4| ext4.ext4_stat(ino))
    }

    fn lookup_cache(&self, parent: u32, name: &str) -> DirLookupCacheResult {
        let caches = self.dir_entry_cache.lock();
        let Some(cache) = caches.get(&parent) else {
            return DirLookupCacheResult::Unknown;
        };
        if let Some(ino) = cache.entries.get(name) {
            return DirLookupCacheResult::Hit(*ino);
        }
        if cache.loaded {
            return DirLookupCacheResult::Miss;
        }
        DirLookupCacheResult::Unknown
    }

    fn load_dir_cache_if_needed(&self, parent: u32) -> Result<()> {
        {
            let caches = self.dir_entry_cache.lock();
            if let Some(cache) = caches.get(&parent) {
                if cache.loaded {
                    return Ok(());
                }
            }
        }

        let meta = self.stat(parent)?;
        if meta.file_type != ext4_rs::InodeFileType::S_IFDIR.bits() {
            return_errno_with_message!(Errno::ENOTDIR, "parent inode is not a directory");
        }

        let entries = self.readdir(parent)?;
        let mut entry_map = BTreeMap::new();
        for entry in entries {
            entry_map.insert(entry.name, entry.inode);
        }

        let mut caches = self.dir_entry_cache.lock();
        let cache = caches.entry(parent).or_default();
        if !cache.loaded {
            cache.entries = entry_map;
            cache.loaded = true;
        }
        Ok(())
    }

    fn cache_insert_entry(&self, parent: u32, name: &str, child: u32) {
        let mut caches = self.dir_entry_cache.lock();
        let cache = caches.entry(parent).or_default();
        cache.entries.insert(name.to_string(), child);
    }

    fn cache_remove_entry(&self, parent: u32, name: &str) {
        let mut caches = self.dir_entry_cache.lock();
        if let Some(cache) = caches.get_mut(&parent) {
            cache.entries.remove(name);
        }
    }

    fn cache_remove_dir(&self, ino: u32) {
        let mut caches = self.dir_entry_cache.lock();
        caches.remove(&ino);
    }

    pub(super) fn lookup_at(&self, parent: u32, name: &str) -> Result<u32> {
        match self.lookup_cache(parent, name) {
            DirLookupCacheResult::Hit(ino) => return Ok(ino),
            DirLookupCacheResult::Miss => {
                return_errno_with_message!(Errno::ENOENT, "No such file or directory");
            }
            DirLookupCacheResult::Unknown => {}
        }

        self.load_dir_cache_if_needed(parent)?;
        match self.lookup_cache(parent, name) {
            DirLookupCacheResult::Hit(ino) => return Ok(ino),
            DirLookupCacheResult::Miss => {
                return_errno_with_message!(Errno::ENOENT, "No such file or directory");
            }
            DirLookupCacheResult::Unknown => {}
        }

        let ino = self.run_ext4(|ext4| ext4.ext4_lookup_at(parent, name))?;
        self.cache_insert_entry(parent, name, ino);
        Ok(ino)
    }

    pub(super) fn dir_open(&self, path: &str) -> Result<u32> {
        self.run_ext4(|ext4| ext4.ext4_dir_open(path))
    }

    pub(super) fn create_at(&self, parent: u32, name: &str, mode: u16) -> Result<u32> {
        let ino = self.run_ext4(|ext4| ext4.ext4_create_at(parent, name, mode))?;
        self.cache_insert_entry(parent, name, ino);
        self.cache_remove_dir(ino);
        Ok(ino)
    }

    pub(super) fn mkdir_at(&self, parent: u32, name: &str, mode: u16) -> Result<u32> {
        let ino = self.run_ext4(|ext4| ext4.ext4_mkdir_at(parent, name, mode))?;
        self.cache_insert_entry(parent, name, ino);
        self.cache_remove_dir(ino);
        Ok(ino)
    }

    pub(super) fn unlink_at(&self, parent: u32, name: &str) -> Result<()> {
        self.run_ext4(|ext4| ext4.ext4_unlink_at(parent, name))?;
        self.cache_remove_entry(parent, name);
        Ok(())
    }

    pub(super) fn rmdir_at(&self, parent: u32, name: &str) -> Result<()> {
        let child_ino = self.lookup_at(parent, name)?;
        self.run_ext4(|ext4| ext4.ext4_rmdir_at(parent, name))?;
        self.cache_remove_entry(parent, name);
        self.cache_remove_dir(child_ino);
        Ok(())
    }

    pub(super) fn read_at(&self, ino: u32, offset: usize, data: &mut [u8]) -> Result<usize> {
        self.run_ext4(|ext4| ext4.ext4_read_at(ino, offset, data))
    }

    pub(super) fn write_at(&self, ino: u32, offset: usize, data: &[u8]) -> Result<usize> {
        self.run_ext4(|ext4| ext4.ext4_write_at(ino, offset, data))
    }

    pub(super) fn truncate(&self, ino: u32, new_size: u64) -> Result<()> {
        self.run_ext4(|ext4| ext4.ext4_truncate(ino, new_size))?;
        Ok(())
    }

    pub(super) fn readdir(&self, ino: u32) -> Result<Vec<SimpleDirEntry>> {
        self.run_ext4_noerr(|ext4| ext4.ext4_readdir(ino))
    }

    pub(super) fn dev_id(&self) -> u64 {
        self.block_device.id().as_encoded_u64()
    }

    pub(super) fn this(&self) -> Arc<Self> {
        self.self_ref.upgrade().unwrap()
    }

    pub(super) fn make_inode(self: &Arc<Self>, ino: u32, path: String) -> Arc<dyn Inode> {
        Arc::new(super::inode::Ext4Inode::new(Arc::downgrade(self), ino, path))
    }
}

impl FileSystem for Ext4Fs {
    fn name(&self) -> &'static str {
        "ext4"
    }

    fn sync(&self) -> Result<()> {
        self.block_device.sync()?;
        Ok(())
    }

    fn root_inode(&self) -> Arc<dyn Inode> {
        self.this().make_inode(EXT4_ROOT_INODE, String::new())
    }

    fn sb(&self) -> SuperBlock {
        let ext4_sb = self.lock_inner().super_block;
        let block_size = ext4_sb.block_size() as usize;
        let blocks = ext4_sb.blocks_count() as usize;
        let bfree = ext4_sb
            .free_blocks_count()
            .min(usize::MAX as u64) as usize;
        let files = ext4_sb.total_inodes() as usize;
        let ffree = ext4_sb.free_inodes_count() as usize;
        let fsid = u64::from_le_bytes(ext4_sb.uuid[..8].try_into().unwrap_or([0u8; 8]));

        SuperBlock {
            magic: EXT4_MAGIC,
            bsize: block_size,
            blocks,
            bfree,
            bavail: bfree,
            files,
            ffree,
            fsid,
            namelen: NAME_MAX,
            frsize: block_size,
            flags: 0,
        }
    }

    fn fs_event_subscriber_stats(&self) -> &FsEventSubscriberStats {
        &self.fs_event_subscriber_stats
    }
}

pub(super) struct Ext4Type;

impl FsType for Ext4Type {
    fn name(&self) -> &'static str {
        "ext4"
    }

    fn properties(&self) -> FsProperties {
        FsProperties::NEED_DISK
    }

    fn create(
        &self,
        _flags: FsFlags,
        _args: Option<CString>,
        disk: Option<Arc<dyn BlockDevice>>,
    ) -> Result<Arc<dyn FileSystem>> {
        let disk =
            disk.ok_or_else(|| Error::with_message(Errno::EINVAL, "missing block device"))?;
        verify_ext4_superblock(disk.as_ref())?;
        Ok(Ext4Fs::open(disk) as Arc<dyn FileSystem>)
    }

    fn sysnode(&self) -> Option<Arc<dyn aster_systree::SysNode>> {
        None
    }
}

pub(super) fn map_ext4_error(err: ext4_rs::Ext4Error) -> Error {
    Error::new(map_ext4_errno(err.error()))
}

fn map_ext4_errno(errno: ext4_rs::Errno) -> Errno {
    match errno {
        ext4_rs::Errno::EPERM => Errno::EPERM,
        ext4_rs::Errno::ENOENT => Errno::ENOENT,
        ext4_rs::Errno::EINTR => Errno::EINTR,
        ext4_rs::Errno::EIO => Errno::EIO,
        ext4_rs::Errno::ENXIO => Errno::ENXIO,
        ext4_rs::Errno::E2BIG => Errno::E2BIG,
        ext4_rs::Errno::EBADF => Errno::EBADF,
        ext4_rs::Errno::EAGAIN => Errno::EAGAIN,
        ext4_rs::Errno::ENOMEM => Errno::ENOMEM,
        ext4_rs::Errno::EACCES => Errno::EACCES,
        ext4_rs::Errno::EFAULT => Errno::EFAULT,
        ext4_rs::Errno::ENOTBLK => Errno::ENOTBLK,
        ext4_rs::Errno::EBUSY => Errno::EBUSY,
        ext4_rs::Errno::EEXIST => Errno::EEXIST,
        ext4_rs::Errno::EXDEV => Errno::EXDEV,
        ext4_rs::Errno::ENODEV => Errno::ENODEV,
        ext4_rs::Errno::ENOTDIR => Errno::ENOTDIR,
        ext4_rs::Errno::EISDIR => Errno::EISDIR,
        ext4_rs::Errno::EINVAL => Errno::EINVAL,
        ext4_rs::Errno::ENFILE => Errno::ENFILE,
        ext4_rs::Errno::EMFILE => Errno::EMFILE,
        ext4_rs::Errno::ENOTTY => Errno::ENOTTY,
        ext4_rs::Errno::ETXTBSY => Errno::ETXTBSY,
        ext4_rs::Errno::EFBIG => Errno::EFBIG,
        ext4_rs::Errno::ENOSPC => Errno::ENOSPC,
        ext4_rs::Errno::ESPIPE => Errno::ESPIPE,
        ext4_rs::Errno::EROFS => Errno::EROFS,
        ext4_rs::Errno::EMLINK => Errno::EMLINK,
        ext4_rs::Errno::EPIPE => Errno::EPIPE,
        ext4_rs::Errno::ENAMETOOLONG => Errno::ENAMETOOLONG,
        ext4_rs::Errno::ENOTEMPTY => Errno::ENOTEMPTY,
        ext4_rs::Errno::ENOTSUP => Errno::EOPNOTSUPP,
    }
}

fn verify_ext4_superblock(block_device: &dyn BlockDevice) -> Result<()> {
    let mut superblock_sector = [0u8; SECTOR_SIZE];
    let mut writer = VmWriter::from(superblock_sector.as_mut_slice()).to_fallible();
    block_device
        .read(EXT4_SUPERBLOCK_OFFSET, &mut writer)
        .map_err(|_| Error::with_message(Errno::EIO, "failed to read ext4 superblock"))?;

    let magic = u16::from_le_bytes([
        superblock_sector[EXT4_SB_MAGIC_OFFSET],
        superblock_sector[EXT4_SB_MAGIC_OFFSET + 1],
    ]);
    if magic != EXT4_MAGIC as u16 {
        return_errno_with_message!(Errno::EINVAL, "not an ext4 filesystem");
    }

    let log_block_size = u32::from_le_bytes([
        superblock_sector[EXT4_SB_LOG_BLOCK_SIZE_OFFSET],
        superblock_sector[EXT4_SB_LOG_BLOCK_SIZE_OFFSET + 1],
        superblock_sector[EXT4_SB_LOG_BLOCK_SIZE_OFFSET + 2],
        superblock_sector[EXT4_SB_LOG_BLOCK_SIZE_OFFSET + 3],
    ]);
    let Some(block_size) = 1024usize.checked_shl(log_block_size) else {
        return_errno_with_message!(Errno::EINVAL, "invalid ext4 block size");
    };
    if !matches!(block_size, 1024 | 2048 | 4096) {
        return_errno_with_message!(Errno::EINVAL, "unsupported ext4 block size");
    }

    let blocks_per_group = u32::from_le_bytes([
        superblock_sector[EXT4_SB_BLOCKS_PER_GROUP_OFFSET],
        superblock_sector[EXT4_SB_BLOCKS_PER_GROUP_OFFSET + 1],
        superblock_sector[EXT4_SB_BLOCKS_PER_GROUP_OFFSET + 2],
        superblock_sector[EXT4_SB_BLOCKS_PER_GROUP_OFFSET + 3],
    ]);
    if blocks_per_group == 0 {
        return_errno_with_message!(Errno::EINVAL, "invalid ext4 blocks_per_group");
    }

    let inodes_per_group = u32::from_le_bytes([
        superblock_sector[EXT4_SB_INODES_PER_GROUP_OFFSET],
        superblock_sector[EXT4_SB_INODES_PER_GROUP_OFFSET + 1],
        superblock_sector[EXT4_SB_INODES_PER_GROUP_OFFSET + 2],
        superblock_sector[EXT4_SB_INODES_PER_GROUP_OFFSET + 3],
    ]);
    if inodes_per_group == 0 {
        return_errno_with_message!(Errno::EINVAL, "invalid ext4 inodes_per_group");
    }

    let desc_size = u16::from_le_bytes([
        superblock_sector[EXT4_SB_DESC_SIZE_OFFSET],
        superblock_sector[EXT4_SB_DESC_SIZE_OFFSET + 1],
    ]);
    if desc_size == 0 {
        return_errno_with_message!(Errno::EINVAL, "invalid ext4 group descriptor size");
    }
    let desc_size = desc_size as usize;
    if desc_size > block_size || (block_size % desc_size) != 0 {
        return_errno_with_message!(Errno::EINVAL, "unsupported ext4 group descriptor size");
    }

    Ok(())
}
