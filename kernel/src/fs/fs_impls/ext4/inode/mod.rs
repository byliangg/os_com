// SPDX-License-Identifier: MPL-2.0

//! Ext4 inodes: shared type aliases, the on-disk inode, and its validated
//! in-memory form.
//!
//! Phase 1 decodes inode metadata (type, permissions, owners, size, times,
//! flags) and keeps the raw `i_block` bytes for the extent reader (Task 3) to
//! interpret. The inode object, payload, and read paths build on `InodeDesc`
//! in later tasks.

use super::{fs::Ext4, prelude::*};

mod dir;
mod extent_manager;

use self::extent_manager::ExtentManager;
use crate::fs::{file::InodeMode, vfs::inode::Extension};

/// Number of 32-bit slots in `i_block` (60 bytes total).
///
/// In ext4 these 60 bytes hold the inline extent-tree root rather than the
/// direct/indirect block pointers of ext2.
pub(super) const RAW_BLOCK_PTRS_LEN: usize = 15;

/// Logical (file-relative) block index (Linux `ext4_lblk_t`, 32-bit).
pub(super) type Iblock = u32;

/// Physical block number on the device. 64-bit from day one so enabling the
/// `64BIT` feature later needs no widening (report §3.3); the on-disk extent
/// encodes a 48-bit physical block.
pub(super) type Ext4Bid = u64;

/// Inode number.
pub(super) type Ext4Ino = u32;

/// `i_flags` value marking an inode whose `i_block` holds an extent tree
/// (Linux `EXT4_EXTENTS_FL`). Phase 1 only reads extent-mapped data inodes.
#[cfg_attr(not(ktest), expect(dead_code))]
pub(super) const EXTENTS_FL: u32 = 0x0008_0000;

/// `i_flags` value marking an inode with inline data (Linux
/// `EXT4_INLINE_DATA_FL`); unsupported in Phase 1.
#[expect(dead_code)]
pub(super) const INLINE_DATA_FL: u32 = 0x1000_0000;

/// File permission bits (the low 12 bits of `i_mode`).
#[derive(Clone, Copy, Debug)]
pub(super) struct FilePerm(u16);

impl FilePerm {
    /// Constructs a `FilePerm` from raw mode bits, keeping only the low 12.
    pub(super) fn from_bits_truncate(bits: u16) -> Self {
        Self(bits & 0o7777)
    }

    /// Returns the raw permission bits.
    pub(super) const fn bits(&self) -> u16 {
        self.0
    }
}

bitflags! {
    /// Inode flags (`i_flags`).
    pub(super) struct FileFlags: u32 {
        const SECURE_DEL = 1 << 0;
        const UNDELETE = 1 << 1;
        const COMPRESS = 1 << 2;
        const SYNC = 1 << 3;
        const IMMUTABLE = 1 << 4;
        const APPEND = 1 << 5;
        const NODUMP = 1 << 6;
        const NOATIME = 1 << 7;
        /// Directory uses an htree hash index.
        const INDEX = 1 << 12;
        /// `i_blocks` is counted in filesystem blocks, not 512-byte sectors.
        const HUGE_FILE = 1 << 18;
        /// `i_block` holds an extent tree (`EXT4_EXTENTS_FL`).
        const EXTENTS = 1 << 19;
        const EA_INODE = 1 << 21;
        /// The inode has inline data.
        const INLINE_DATA = 1 << 28;
    }
}

/// Validated, Rust-typed in-memory inode metadata.
///
/// The raw `i_block` bytes are retained in `block` for the extent reader; their
/// interpretation as an extent tree happens in Task 3.
#[derive(Clone, Debug)]
pub(super) struct InodeDesc {
    type_: InodeType,
    perm: FilePerm,
    uid: u32,
    gid: u32,
    size: u64,
    atime: Duration,
    ctime: Duration,
    mtime: Duration,
    crtime: Duration,
    #[expect(dead_code)]
    dtime: Duration,
    link_count: u16,
    /// `i_blocks` in 512-byte sectors (48-bit: low 32 + high 16).
    sector_count: u64,
    flags: FileFlags,
    #[expect(dead_code)]
    file_acl: u64,
    #[expect(dead_code)]
    generation: u32,
    /// Raw `i_block` (60 bytes) — the inline extent-tree root.
    block: [u32; RAW_BLOCK_PTRS_LEN],
}

impl InodeDesc {
    pub(super) const fn type_(&self) -> InodeType {
        self.type_
    }

    pub(super) const fn size(&self) -> u64 {
        self.size
    }

    pub(super) const fn link_count(&self) -> u16 {
        self.link_count
    }

    #[expect(dead_code)]
    pub(super) const fn flags(&self) -> FileFlags {
        self.flags
    }

    pub(super) const fn sector_count(&self) -> u64 {
        self.sector_count
    }

    pub(super) const fn perm(&self) -> FilePerm {
        self.perm
    }

    pub(super) const fn uid(&self) -> u32 {
        self.uid
    }

    pub(super) const fn gid(&self) -> u32 {
        self.gid
    }

    pub(super) const fn atime(&self) -> Duration {
        self.atime
    }

    pub(super) const fn mtime(&self) -> Duration {
        self.mtime
    }

    pub(super) const fn ctime(&self) -> Duration {
        self.ctime
    }

    pub(super) const fn crtime(&self) -> Duration {
        self.crtime
    }

    /// Returns the raw `i_block` bytes holding the extent-tree root.
    pub(super) const fn raw_block(&self) -> &[u32; RAW_BLOCK_PTRS_LEN] {
        &self.block
    }

    /// Returns whether this inode's data is mapped by an extent tree.
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn is_extent_based(&self) -> bool {
        self.flags.contains(FileFlags::EXTENTS)
    }
}

/// Decodes an ext4 timestamp from its seconds field and the `*_extra` field.
///
/// The extra field packs a 2-bit epoch (extending seconds past 2038) in its low
/// bits and nanoseconds in the upper bits (report §4.3).
fn decode_time(secs: u32, extra: u32) -> Duration {
    let epoch = (extra & 0x3) as u64;
    let nsec = extra >> 2;
    Duration::new((secs as u64) | (epoch << 32), nsec)
}

impl TryFrom<&RawInode> for InodeDesc {
    type Error = Error;

    fn try_from(raw: &RawInode) -> Result<Self> {
        if raw.link_count == 0 {
            return_errno_with_message!(Errno::ESTALE, "inode is not in use");
        }

        let type_ = InodeType::from_raw_mode(raw.mode)
            .map_err(|_| Error::with_message(Errno::EUCLEAN, "invalid inode mode"))?;
        let perm = FilePerm::from_bits_truncate(raw.mode);

        let uid = (raw.uid as u32) | ((raw.uid_high as u32) << 16);
        let gid = (raw.gid as u32) | ((raw.gid_high as u32) << 16);

        let mut size = raw.size_lo as u64;
        if type_ == InodeType::File {
            size |= (raw.size_high as u64) << 32;
        }
        if type_ == InodeType::SymLink && size >= BLOCK_SIZE as u64 {
            return_errno_with_message!(Errno::EUCLEAN, "symlink size too large");
        }
        if size > i64::MAX as u64 {
            return_errno_with_message!(Errno::EUCLEAN, "inode size too large");
        }

        let flags = FileFlags::from_bits_truncate(raw.flags);
        let sector_count = (raw.sector_count as u64) | ((raw.blocks_high as u64) << 32);
        let file_acl = (raw.file_acl_lo as u64) | ((raw.file_acl_high as u64) << 32);

        Ok(Self {
            type_,
            perm,
            uid,
            gid,
            size,
            atime: decode_time(raw.atime, raw.atime_extra),
            ctime: decode_time(raw.ctime, raw.ctime_extra),
            mtime: decode_time(raw.mtime, raw.mtime_extra),
            crtime: decode_time(raw.crtime, raw.crtime_extra),
            dtime: Duration::from_secs(raw.dtime as u64),
            link_count: raw.link_count,
            sector_count,
            flags,
            file_acl,
            generation: raw.generation,
            block: raw.block,
        })
    }
}

const_assert!(size_of::<RawInode>() == 256);

/// The on-disk ext4 inode (256 bytes for the default `s_inode_size`).
///
/// The first 128 bytes match ext2's layout; the trailing fields are ext4's
/// extra-size region (nanosecond timestamps, creation time) followed by space
/// reserved for inline extended attributes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(super) struct RawInode {
    pub mode: u16,
    pub uid: u16,
    pub size_lo: u32,
    pub atime: u32,
    pub ctime: u32,
    pub mtime: u32,
    pub dtime: u32,
    pub gid: u16,
    pub link_count: u16,
    /// `i_blocks` low 32 bits (512-byte sectors).
    pub sector_count: u32,
    pub flags: u32,
    pub osd1: u32,
    /// `i_block`: 60 bytes holding the inline extent-tree root.
    pub block: [u32; RAW_BLOCK_PTRS_LEN],
    pub generation: u32,
    pub file_acl_lo: u32,
    pub size_high: u32,
    pub obso_faddr: u32,
    // osd2 (Linux ext4 layout).
    pub blocks_high: u16,
    pub file_acl_high: u16,
    pub uid_high: u16,
    pub gid_high: u16,
    pub checksum_lo: u16,
    pub osd2_reserved: u16,
    // ext4 extra-size region (present when `s_inode_size` > 128).
    pub extra_isize: u16,
    pub checksum_hi: u16,
    pub ctime_extra: u32,
    pub mtime_extra: u32,
    pub atime_extra: u32,
    pub crtime: u32,
    pub crtime_extra: u32,
    pub version_hi: u32,
    pub projid: u32,
    /// Space reserved for inline extended attributes (unused in Phase 1).
    pub tail: InodeTail,
}

/// Padding from the end of the ext4 inode fields (offset 160) to the 256-byte
/// on-disk inode size; in ext4 this holds inline extended attributes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(super) struct InodeTail([u8; 96]);

impl Default for InodeTail {
    fn default() -> Self {
        Self([0u8; 96])
    }
}

/// A single ext4 inode: shared metadata plus type-specific payload.
pub struct Inode {
    ino: Ext4Ino,
    type_: InodeType,
    inner: RwMutex<InodeInner>,
    block_group_idx: usize,
    fs: Weak<Ext4>,
    /// The VFS extension slot (flock, POSIX locks, inotify); must exist from
    /// day one or the VFS layer panics on inodes that use these features.
    extension: Extension,
}

impl Inode {
    pub(super) fn new(
        ino: Ext4Ino,
        type_: InodeType,
        desc: Dirty<InodeDesc>,
        block_group_idx: usize,
        fs: Weak<Ext4>,
    ) -> Arc<Self> {
        let payload = InodePayload::new(&desc, fs.clone());
        Arc::new(Self {
            ino,
            type_,
            inner: RwMutex::new(InodeInner { desc, payload }),
            block_group_idx,
            fs,
            extension: Extension::new(),
        })
    }

    pub(super) fn ino(&self) -> Ext4Ino {
        self.ino
    }

    pub(super) fn inode_type(&self) -> InodeType {
        self.type_
    }

    pub(super) fn size(&self) -> usize {
        self.inner.read().file_size()
    }

    /// Reads file data at `offset` through the inode's page cache.
    pub(super) fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize> {
        self.inner.read().read_at(offset, writer)
    }

    pub(super) fn perm(&self) -> FilePerm {
        self.inner.read().desc.perm()
    }

    /// Returns the permission bits as a VFS `InodeMode`.
    pub(super) fn mode(&self) -> InodeMode {
        InodeMode::from_bits_truncate(self.perm().bits() as _)
    }

    pub(super) fn uid(&self) -> u32 {
        self.inner.read().desc.uid()
    }

    pub(super) fn gid(&self) -> u32 {
        self.inner.read().desc.gid()
    }

    pub(super) fn link_count(&self) -> u16 {
        self.inner.read().desc.link_count()
    }

    pub(super) fn sector_count(&self) -> u64 {
        self.inner.read().desc.sector_count()
    }

    pub(super) fn atime(&self) -> Duration {
        self.inner.read().desc.atime()
    }

    pub(super) fn mtime(&self) -> Duration {
        self.inner.read().desc.mtime()
    }

    pub(super) fn ctime(&self) -> Duration {
        self.inner.read().desc.ctime()
    }

    pub(super) fn crtime(&self) -> Duration {
        self.inner.read().desc.crtime()
    }

    #[expect(dead_code)]
    pub(super) fn block_group_idx(&self) -> usize {
        self.block_group_idx
    }

    /// Returns a clone of the inode's page cache, if it is data-backed.
    pub(super) fn page_cache(&self) -> Option<PageCache> {
        self.inner.read().page_cache().ok().cloned()
    }

    /// Returns the owning filesystem, or an error if it has been dropped.
    pub(super) fn fs(&self) -> Result<Arc<Ext4>> {
        self.fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem dropped"))
    }

    pub(super) fn extension(&self) -> &Extension {
        &self.extension
    }
}

struct InodeInner {
    desc: Dirty<InodeDesc>,
    payload: InodePayload,
}

impl InodeInner {
    fn file_size(&self) -> usize {
        self.desc.size() as usize
    }

    fn page_cache(&self) -> Result<&PageCache> {
        match &self.payload {
            InodePayload::DataBacked { page_cache, .. } => Ok(page_cache),
            _ => return_errno_with_message!(Errno::EINVAL, "inode has no page cache"),
        }
    }

    fn read_at(&self, offset: usize, writer: &mut VmWriter) -> Result<usize> {
        if writer.avail() == 0 {
            return Ok(0);
        }
        let file_size = self.file_size();
        if offset >= file_size {
            return Ok(0);
        }
        let read_len = writer.avail().min(file_size - offset);
        writer.limit(read_len);
        self.page_cache()?.read(offset, writer)?;
        Ok(read_len)
    }
}

/// Type-specific inode contents.
enum InodePayload {
    /// Regular files and directories: page-cached data mapped by extents.
    DataBacked {
        page_cache: PageCache,
        /// Kept alive so the page-cache backend's `Weak` stays valid.
        #[expect(dead_code)]
        block_manager: Arc<ExtentManager>,
    },
    /// Inline data (small files stored in the inode); unsupported in Phase 1.
    #[expect(dead_code)]
    Inline,
    /// Symlinks, devices, and special files (filled in by later tasks).
    NoPayload,
}

impl InodePayload {
    fn new(desc: &InodeDesc, fs: Weak<Ext4>) -> Self {
        match desc.type_() {
            InodeType::File | InodeType::Dir => {
                Self::new_data_backed(desc.size() as usize, *desc.raw_block(), fs)
            }
            // Symlinks, devices, and special files are handled by later tasks.
            _ => Self::NoPayload,
        }
    }

    fn new_data_backed(size: usize, root: [u32; RAW_BLOCK_PTRS_LEN], fs: Weak<Ext4>) -> Self {
        let page_cache_size = size.align_up(PAGE_SIZE);
        let page_count = page_cache_size / PAGE_SIZE;
        let extent_manager = Arc::new(ExtentManager::new(root, fs, page_count));
        let backend: Weak<dyn PageCacheBackend> = Arc::downgrade(&extent_manager) as _;
        let page_cache = PageCache::new_with_backend(page_cache_size, backend)
            .expect("ext4 inode page cache allocation failed");
        Self::DataBacked {
            page_cache,
            block_manager: extent_manager,
        }
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;

    /// Builds a raw root-directory inode: one data block via an (unparsed here)
    /// extent root, link count 2, `EXT4_EXTENTS_FL` set.
    fn raw_root_dir() -> RawInode {
        RawInode {
            mode: 0o040755, // S_IFDIR | 0755
            size_lo: BLOCK_SIZE as u32,
            link_count: 2,
            sector_count: (BLOCK_SIZE / SECTOR_SIZE) as u32,
            flags: EXTENTS_FL,
            extra_isize: 32,
            ..Default::default()
        }
    }

    #[ktest]
    fn decode_root_dir_inode() {
        let raw = raw_root_dir();
        let desc = InodeDesc::try_from(&raw).unwrap();
        assert_eq!(desc.type_(), InodeType::Dir);
        assert_eq!(desc.size(), BLOCK_SIZE as u64);
        assert_eq!(desc.link_count(), 2);
        assert!(desc.is_extent_based());
        assert_eq!(desc.sector_count(), (BLOCK_SIZE / SECTOR_SIZE) as u64);
    }

    #[ktest]
    fn decode_combines_uid_gid_high() {
        let mut raw = raw_root_dir();
        raw.uid = 0x1111;
        raw.uid_high = 0x2222;
        raw.gid = 0x3333;
        raw.gid_high = 0x4444;
        let desc = InodeDesc::try_from(&raw).unwrap();
        assert_eq!(desc.uid, 0x2222_1111);
        assert_eq!(desc.gid, 0x4444_3333);
    }

    #[ktest]
    fn decode_combines_size_high_for_file() {
        let mut raw = raw_root_dir();
        raw.mode = 0o100644; // S_IFREG | 0644
        raw.link_count = 1;
        raw.size_lo = 0x0000_1000;
        raw.size_high = 0x0000_0001; // 4 GiB + 4 KiB
        let desc = InodeDesc::try_from(&raw).unwrap();
        assert_eq!(desc.type_(), InodeType::File);
        assert_eq!(desc.size(), (1u64 << 32) | 0x1000);
    }

    #[ktest]
    fn decode_nanosecond_time() {
        let mut raw = raw_root_dir();
        raw.mtime = 1000;
        raw.mtime_extra = 500 << 2; // nsec=500, epoch=0
        let desc = InodeDesc::try_from(&raw).unwrap();
        assert_eq!(desc.mtime, Duration::new(1000, 500));
    }

    #[ktest]
    fn reject_unused_inode() {
        let mut raw = raw_root_dir();
        raw.link_count = 0;
        assert!(InodeDesc::try_from(&raw).is_err());
    }
}
