// SPDX-License-Identifier: MPL-2.0

//! Ext4 inodes: shared type aliases, the on-disk inode, its validated in-memory
//! form, and the buffered write path.
//!
//! An inode decodes its on-disk metadata (type, permissions, owners, size,
//! times, flags) into `InodeDesc` and maps its data through an extent tree
//! (`extent_manager`). Reads, buffered writes, truncation, and attribute changes
//! all go through `Inode`.
//!
//! # Locking
//!
//! Data-backed inodes nest the extent tree under `inner`:
//!
//! ```text
//! Inode::inner → ExtentManager::state
//! ```
//!
//! Operations that allocate or free blocks call filesystem-level methods
//! (`Ext4::alloc_blocks` / `Ext4::free_blocks`); the full cross-layer lock order
//! is:
//!
//! ```text
//! Inode::inner → ExtentManager::state → Ext4::super_block → BlockGroup::metadata
//! ```
//!
//! The journal handle (Phase 4) sits between `inner` and the extent tree; the
//! Phase-2 `journal` wrappers are no-ops and take no lock. `BlockGroup::inode_cache`
//! is independent: it is never held while acquiring `super_block`/`metadata`, nor
//! while syncing an inode (`sync_inodes` clones the `Arc`s out and drops the read
//! lock first).

use super::{fs::Ext4, journal, journal::Tid, prelude::*};

mod dir;
mod extent_manager;
mod symlink;

use self::{extent_manager::ExtentManager, symlink::FastSymlinkTarget};
use crate::fs::{file::InodeMode, vfs::inode::Extension};

/// Number of 32-bit slots in `i_block` (60 bytes total).
///
/// In ext4 these 60 bytes hold the inline extent-tree root rather than the
/// direct/indirect block pointers of ext2.
pub(super) const RAW_BLOCK_PTRS_LEN: usize = 15;

/// Byte capacity of the inline `i_block` area used to store a fast symlink
/// target (`RAW_BLOCK_PTRS_LEN * 4` = 60). A target strictly shorter than this
/// is stored inline (one byte is reserved for the Linux trailing NUL); a longer
/// one is stored in an extent-mapped data block (a slow symlink).
pub(super) const MAX_FAST_SYMLINK_LEN: usize = RAW_BLOCK_PTRS_LEN * 4;

/// Maximum hard-link count for an inode (Linux `EXT4_LINK_MAX`). `link` rejects
/// a request that would exceed this. Mirrors ext2 `MAX_LINK_COUNT`.
pub(super) const MAX_LINK_COUNT: u16 = 32000;

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
pub struct FilePerm(u16);

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
    dtime: Duration,
    link_count: u16,
    /// `i_blocks` in 512-byte sectors (48-bit: low 32 + high 16).
    sector_count: u64,
    flags: FileFlags,
    #[expect(dead_code)]
    file_acl: u64,
    generation: u32,
    /// Raw `i_block` (60 bytes) — the inline extent-tree root.
    block: [u32; RAW_BLOCK_PTRS_LEN],
}

/// `eh_magic` of an `ext4_extent_header` (Linux `EXT4_EXT_MAGIC`).
const EXTENT_MAGIC: u16 = 0xF30A;

/// Maximum extents the 60-byte inline root can hold past its 12-byte header
/// (`(60 - 12) / 12`).
const EXTENT_MAX_INLINE: u16 = 4;

/// Builds the inline extent-tree root for a freshly created inode: a valid empty
/// `ext4_extent_header` (magic `0xF30A`, 0 entries, max 4, depth 0) followed by
/// zeros. New regular files and directories carry this so the extent reader sees
/// a well-formed (empty) tree from the first byte — unlike ext2, whose new
/// inodes start with zeroed indirect-block pointers.
fn empty_extent_root() -> [u32; RAW_BLOCK_PTRS_LEN] {
    let mut block = [0u32; RAW_BLOCK_PTRS_LEN];
    // Each `i_block` word packs two 16-bit fields, little-endian: word 0 is
    // `eh_magic | eh_entries(=0)`, word 1 is `eh_max(=4) | eh_depth(=0)`.
    block[0] = EXTENT_MAGIC as u32;
    block[1] = EXTENT_MAX_INLINE as u32;
    block[2] = 0; // eh_generation
    block
}

impl InodeDesc {
    /// Builds a fresh inode descriptor for a newly created file or directory.
    ///
    /// Size and `i_blocks` start at zero; all timestamps are `now`; the inline
    /// `i_block` holds a valid empty extent root and the `EXTENTS` flag is set
    /// (ext4-specific — the data of every regular file/directory is extent
    /// mapped). Mirrors ext2 `InodeDesc::new`, diverging only in the extent
    /// root + flag (ext2 leaves zeroed indirect pointers and no flag).
    pub(super) fn new(
        type_: InodeType,
        perm: FilePerm,
        uid: u32,
        gid: u32,
        link_count: u16,
        generation: u32,
        now: Duration,
    ) -> Self {
        Self {
            type_,
            perm,
            uid,
            gid,
            size: 0,
            atime: now,
            ctime: now,
            mtime: now,
            crtime: now,
            dtime: Duration::ZERO,
            link_count,
            sector_count: 0,
            flags: FileFlags::EXTENTS,
            file_acl: 0,
            generation,
            block: empty_extent_root(),
        }
    }

    pub(super) const fn type_(&self) -> InodeType {
        self.type_
    }

    pub(super) const fn size(&self) -> u64 {
        self.size
    }

    pub(super) const fn link_count(&self) -> u16 {
        self.link_count
    }

    pub(super) const fn flags(&self) -> FileFlags {
        self.flags
    }

    pub(super) const fn sector_count(&self) -> u64 {
        self.sector_count
    }

    /// Sets the logical file size (in bytes). Mutates through `Dirty`.
    pub(super) fn set_size(&mut self, size: u64) {
        self.size = size;
    }

    /// Overwrites the link count outright. Mutates through `Dirty`.
    pub(super) fn set_link_count(&mut self, count: u16) {
        self.link_count = count;
    }

    /// Adds `delta` to the link count. Mutates through `Dirty`.
    pub(super) fn inc_link_count(&mut self, delta: u16) {
        debug_assert!(self.link_count <= u16::MAX - delta);
        self.link_count += delta;
    }

    /// Subtracts `delta` from the link count (saturating). Mutates through
    /// `Dirty`; used by the unlink/rmdir path to drop a name's reference.
    pub(super) fn dec_link_count(&mut self, delta: u16) {
        debug_assert!(self.link_count >= delta);
        self.link_count = self.link_count.saturating_sub(delta);
    }

    /// Sets the deletion time (`i_dtime`). Mutates through `Dirty`; stamped when
    /// a fully unlinked inode is reclaimed.
    pub(super) fn set_dtime(&mut self, time: Duration) {
        self.dtime = time;
    }

    /// Clears the given inode flags. Mutates through `Dirty`; used to drop the
    /// `EXTENTS` flag when an inode switches to inline (fast-symlink) storage.
    pub(super) fn remove_flags(&mut self, flags: FileFlags) {
        self.flags.remove(flags);
    }

    /// Overwrites the raw `i_block` words. Mutates through `Dirty`; used to store
    /// a fast-symlink target inline so writeback persists it (a fast symlink has
    /// no block manager to snapshot the `i_block` from).
    pub(super) fn set_raw_block(&mut self, block: [u32; RAW_BLOCK_PTRS_LEN]) {
        self.block = block;
    }

    /// Sets the last-modification time. Mutates through `Dirty`.
    pub(super) fn set_mtime(&mut self, time: Duration) {
        self.mtime = time;
    }

    /// Sets the last-metadata-change time. Mutates through `Dirty`.
    pub(super) fn set_ctime(&mut self, time: Duration) {
        self.ctime = time;
    }

    /// Sets the `i_blocks` accounting (512-byte sectors). Mutates through
    /// `Dirty`; used to mirror the block manager's authoritative count.
    pub(super) fn set_sector_count(&mut self, sectors: u64) {
        self.sector_count = sectors;
    }

    /// Sets the permission bits (chmod). Mutates through `Dirty`.
    pub(super) fn set_perm(&mut self, perm: FilePerm) {
        self.perm = perm;
    }

    /// Sets the owning user id (chown). Mutates through `Dirty`.
    pub(super) fn set_uid(&mut self, uid: u32) {
        self.uid = uid;
    }

    /// Sets the owning group id (chgrp). Mutates through `Dirty`.
    pub(super) fn set_gid(&mut self, gid: u32) {
        self.gid = gid;
    }

    /// Sets the last-access time. Mutates through `Dirty`.
    pub(super) fn set_atime(&mut self, time: Duration) {
        self.atime = time;
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

    /// Returns the deletion time (`i_dtime`).
    pub(super) const fn dtime(&self) -> Duration {
        self.dtime
    }

    /// Returns the inode generation (`i_generation`).
    pub(super) const fn generation(&self) -> u32 {
        self.generation
    }

    /// Returns the raw `i_block` bytes holding the extent-tree root.
    pub(super) const fn raw_block(&self) -> &[u32; RAW_BLOCK_PTRS_LEN] {
        &self.block
    }

    /// Returns whether this inode's data is mapped by an extent tree.
    pub(super) fn is_extent_based(&self) -> bool {
        self.flags.contains(FileFlags::EXTENTS)
    }
}

/// Resolves the physical device block backing each of the first `nblocks`
/// logical blocks of an extent-mapped inode.
///
/// This keeps the extent engine encapsulated in the `inode` module while handing
/// callers a plain, fully resolved block map. The journal uses it to build its
/// log block map from the journal inode (ino 8), whose data blocks hold the log;
/// every log block must be a real allocated, written block, so a hole or
/// unwritten block is an error rather than a zero-filled read.
pub(in crate::fs::fs_impls::ext4) fn map_all_blocks(
    fs: Weak<Ext4>,
    root: [u32; RAW_BLOCK_PTRS_LEN],
    sector_count: u64,
    nblocks: u32,
) -> Result<Vec<Ext4Bid>> {
    let em = ExtentManager::new(root, sector_count, fs, nblocks as usize);
    let mut map = Vec::with_capacity(nblocks as usize);
    let mut i: Iblock = 0;
    while i < nblocks {
        let m = em.map_blocks(i)?;
        if m.reads_as_zeros() {
            return_errno_with_message!(
                Errno::EUCLEAN,
                "journal inode has an unmapped (hole/unwritten) block"
            );
        }
        let run = m.len().min(nblocks - i);
        for k in 0..run {
            map.push(m.pblock() + k as Ext4Bid);
        }
        i += run;
    }
    Ok(map)
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
            inner: RwMutex::new(InodeInner {
                desc,
                payload,
                sync_tid: 0,
                datasync_tid: 0,
            }),
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

    /// Writes file data at `offset` through the inode's page cache.
    ///
    /// Allocates blocks for any holes the write covers, fills the page cache,
    /// and updates size and timestamps. Data and the inode become durable on a
    /// later `sync` / writeback.
    pub(super) fn write_at(&self, offset: usize, reader: &mut VmReader) -> Result<usize> {
        if self.type_ == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }
        if reader.remain() == 0 {
            return Ok(0);
        }
        let fs = self.fs()?;
        let mut inner = self.inner.write();
        // Journal handle after the inner lock (inner ① → handle ②): captures the
        // block-bitmap / group-descriptor / extent after-images this write's
        // allocations dirty. Dropped at return, closing the handle.
        //
        // P4 limitation (data=ordered not wired, → follow-up): this write's data
        // pages are NOT registered as ordered data of the transaction
        // (`Transaction::add_ordered_inode`), so they are not flushed before the
        // extent metadata commits. On a clean unmount the fs-level sync flushes
        // them; but a crash after this transaction commits (or checkpoints) yet
        // before the data pages reach the platter can leave the committed extents
        // covering stale blocks. File **metadata** is crash-safe; file **data**
        // ordering is a follow-up (registering ordered inodes needs the write path
        // to reach the `Arc<Inode>`).
        let op = fs.begin_op(Ext4::WRITE_CREDITS)?;
        inner.write_at(&fs, offset, reader, op.get())
    }

    /// Truncates or extends a regular file to `new_size` bytes.
    ///
    /// Shrinking frees the trailing data/metadata blocks and zeroes the kept
    /// partial last block; expanding is sparse (the gap is a hole that reads as
    /// zeros). Directories are rejected with `EISDIR` (P2 resizes only files).
    pub(super) fn resize(&self, new_size: usize) -> Result<()> {
        if self.type_ == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }
        let fs = self.fs()?;
        let mut inner = self.inner.write();
        // Journal handle after the inner lock (inner ① → handle ②): captures the
        // block-bitmap / group-descriptor / extent after-images a shrink frees.
        let op = fs.begin_op(Ext4::TRUNCATE_CREDITS)?;
        // Orphan-list seam for shrinking truncates (Phase-3 no-op; Task 8 will
        // journal a large truncate onto the orphan list so crash recovery can
        // finish freeing the trailing blocks if interrupted mid-shrink).
        let is_shrink = new_size < inner.file_size();
        if is_shrink {
            journal::orphan_add(op.get(), self.ino)?;
        }
        inner.resize(&fs, new_size, op.get())?;
        if is_shrink {
            journal::orphan_del(op.get(), self.ino)?;
        }
        Ok(())
    }

    /// Persists the inode's mutable metadata (size, `i_blocks`, extent root,
    /// timestamps) to disk if dirty. Data pages are flushed by
    /// [`sync_data_and_meta`](Self::sync_data_and_meta).
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn sync_metadata(&self) -> Result<()> {
        let fs = self.fs()?;
        let mut inner = self.inner.write();
        // Journaled: capture under a handle like every other metadata write (the
        // direct RMW below in `write_back_inode_desc` reads the on-disk slot,
        // which lags a suppressed-but-not-yet-checkpointed write — stale — and
        // writing the final location outside the journal breaks WAL ordering).
        // Non-journaled volumes get the no-op handle and the Phase-3 direct RMW.
        let op = fs.begin_op(Ext4::FSYNC_CREDITS)?;
        inner.write_back_inode_desc(&fs, self.ino, op.get())
    }

    /// Flushes dirty data pages, journals/writes the inode metadata, waits for
    /// the transaction to commit (journaled volumes), then issues a device sync
    /// — the `fsync` contract: data first (ordered-data semantics — the data is
    /// durable before the metadata referencing it can commit), metadata
    /// recoverable from the log on return.
    pub(super) fn sync_data_and_meta(&self) -> Result<()> {
        let fs = self.fs()?;
        let wait_tid = {
            let mut inner = self.inner.write();
            inner.sync_data_pages()?;
            // Journaled: capture instead of direct-writing (see
            // `sync_metadata`). Only wait below if this writeback actually
            // captured something: a clean inode contributes no metadata, and a
            // transaction with no captured blocks never becomes committable —
            // waiting on its tid would sleep forever.
            let was_dirty = inner.is_dirty();
            let op = fs.begin_op(Ext4::FSYNC_CREDITS)?;
            inner.write_back_inode_desc(&fs, self.ino, op.get())?;
            if was_dirty { op.tid() } else { None }
            // `op` closes here, then `inner` unlocks — the reverse of the
            // inner ① → handle ② acquisition order.
        };
        // Wait with NO filesystem locks held (the commit thread takes
        // `inode.inner` for the ordered-data flush).
        if let Some(target) = wait_tid
            && let Some(journal) = fs.journal()
        {
            journal.log_wait_commit(target)?;
        }
        if fs.block_device().sync()? != BioStatus::Complete {
            return_errno_with_message!(Errno::EIO, "failed to flush block device");
        }
        Ok(())
    }

    /// Flushes dirty data pages and then the inode metadata, *without* a device
    /// barrier or a commit wait. Used by the filesystem-level sync, which
    /// flushes every cached inode and the block-side metadata before issuing a
    /// single barrier — so a per-inode barrier here would be redundant. Mirrors
    /// ext2 `Inode::sync_all`, where the barrier lives at the `FileSystem::sync`
    /// boundary.
    ///
    /// On a journaled volume the writeback is captured under a handle (see
    /// [`sync_metadata`](Self::sync_metadata)); its commit is asynchronous — the
    /// filesystem-level sync's log durability is a documented P4 limitation
    /// (unmount reaches durability via `flush_on_unmount`).
    pub(super) fn sync_data_and_meta_no_barrier(&self) -> Result<()> {
        let fs = self.fs()?;
        let mut inner = self.inner.write();
        inner.sync_data_pages()?;
        let op = fs.begin_op(Ext4::FSYNC_CREDITS)?;
        inner.write_back_inode_desc(&fs, self.ino, op.get())?;
        Ok(())
    }

    /// Flushes this inode's dirty **data** pages to their final on-disk locations,
    /// for journaling's ordered-data mode (jbd2 `data=ordered`): a file's data
    /// must be durable before the metadata referencing it is committed to the log,
    /// so recovery never replays metadata (size/extents) that points at a block
    /// whose data never reached the platter. Does not touch inode/extent metadata
    /// (that goes through the journal). Called by the commit pipeline for each
    /// ordered inode.
    ///
    /// A `read()` guard suffices: `sync_data_pages` takes `&self` on
    /// [`InodeInner`] and only reads the page cache to flush its dirty pages
    /// through the extent-mapped backend to their final device blocks — it mutates
    /// no `InodeInner` field. In the global lock order this holds only
    /// `inner.read()` (a leaf here) and no journal state lock.
    pub(in crate::fs::fs_impls::ext4) fn flush_ordered_data(&self) -> Result<()> {
        self.inner.read().sync_data_pages()
    }

    /// Maps logical block `iblock` to its physical block, or `None` for a hole.
    /// Test-only inspection used to read a file's data straight off the device
    /// (e.g. to prove the ordered flush reached the final location).
    #[cfg(ktest)]
    pub(in crate::fs::fs_impls::ext4) fn data_block_of(&self, iblock: Iblock) -> Option<Ext4Bid> {
        use self::extent_manager::MapState;

        let inner = self.inner.read();
        let block_manager = inner.block_manager().ok()?;
        let mapping = block_manager.map_blocks(iblock).ok()?;
        match mapping.state() {
            MapState::Written | MapState::Unwritten => Some(mapping.pblock()),
            MapState::Hole => None,
        }
    }

    /// Reclaims a fully unlinked inode: frees its data blocks and inode bit.
    ///
    /// Runs from `Drop` when the last `Arc<Inode>` is released. A no-op (returns
    /// `Ok(false)`) unless the inode's link count is 0 *and* its bitmap bit is
    /// still allocated — the latter guards against double-freeing an inode an
    /// earlier reclaim already released. On reclaim it stamps `i_dtime`, drops
    /// the data (page cache + extent-mapped blocks, only for data-backed
    /// inodes — a fast symlink has no data block), persists the descriptor, and
    /// frees the inode. Mirrors ext2 `try_reclaim_deleted_inode`, minus the
    /// xattr-block deletion (ext4 has no xattr support yet; `file_acl` is unused).
    pub(super) fn try_reclaim_deleted_inode(&self) -> Result<bool> {
        if self.link_count() != 0 {
            return Ok(false);
        }

        let fs = self.fs()?;
        if !fs.is_inode_allocated(self.ino) {
            return Ok(false);
        }

        let mut inner = self.inner.write();
        // Journal handle after the inner lock (inner ① → handle ②): captures the
        // block-bitmap / group-descriptor / inode-bitmap after-images freeing the
        // inode's blocks and the inode itself dirty.
        let op = fs.begin_op(Ext4::RECLAIM_CREDITS)?;
        let old_size = inner.file_size();
        // Only data-backed inodes (files, directories, slow symlinks) own a page
        // cache and extent-mapped blocks. A fast symlink stores its target inline
        // in `i_block` with no data block, so it skips both the page-cache resize
        // and the block truncate below.
        let block_manager = inner.block_manager().ok().cloned();
        if block_manager.is_some() {
            inner.resize_page_cache(0, old_size)?;
        }
        inner.set_dtime(super::utils::now());
        inner.set_file_size(0);
        // Gate on the extent manager's live `sector_count`, not the descriptor's
        // copy (which ext2 uses): the extent manager is the authority and the
        // descriptor may be stale until writeback. This divergence from the ext2
        // template is intentional — do not "fix" it back to `inner.desc`.
        if let Some(block_manager) = block_manager
            && block_manager.sector_count() > 0
        {
            block_manager.truncate_to_byte_len(0, op.get())?;
        }
        inner.write_back_inode_desc(&fs, self.ino, op.get())?;

        fs.free_inode(self.ino, self.type_, op.get())?;
        // Orphan-list seam (Phase-3 no-op): Task 8 unlinks the inode from the
        // on-disk orphan chain now that its blocks and inode are freed.
        journal::orphan_del(op.get(), self.ino)?;
        Ok(true)
    }

    /// Updates the permission bits (chmod) and bumps ctime. Persists on fsync.
    pub(super) fn set_mode(&self, mode: InodeMode) {
        let mut inner = self.inner.write();
        inner
            .desc
            .set_perm(FilePerm::from_bits_truncate(mode.bits()));
        inner.desc.set_ctime(super::utils::now());
    }

    /// Updates the owning uid (chown) and bumps ctime. Persists on fsync.
    pub(super) fn set_owner(&self, uid: u32) {
        let mut inner = self.inner.write();
        inner.desc.set_uid(uid);
        inner.desc.set_ctime(super::utils::now());
    }

    /// Updates the owning gid (chgrp) and bumps ctime. Persists on fsync.
    pub(super) fn set_group(&self, gid: u32) {
        let mut inner = self.inner.write();
        inner.desc.set_gid(gid);
        inner.desc.set_ctime(super::utils::now());
    }

    /// Sets the last-access time. Persists on fsync.
    pub(super) fn set_atime(&self, time: Duration) {
        self.inner.write().desc.set_atime(time);
    }

    /// Sets the last-modification time. Persists on fsync.
    pub(super) fn set_mtime(&self, time: Duration) {
        self.inner.write().desc.set_mtime(time);
    }

    /// Sets the last-metadata-change time. Persists on fsync.
    pub(super) fn set_ctime(&self, time: Duration) {
        self.inner.write().desc.set_ctime(time);
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
        self.inner.read().sector_count()
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

impl Drop for Inode {
    fn drop(&mut self) {
        if let Err(err) = self.try_reclaim_deleted_inode() {
            debug!(
                "failed to reclaim deleted inode {} during drop: {:?}",
                self.ino, err
            );
        }
    }
}

struct InodeInner {
    desc: Dirty<InodeDesc>,
    payload: InodePayload,
    /// Last transaction that modified this inode (jbd2 `i_sync_tid`), and the
    /// subset needed for `fdatasync` (`i_datasync_tid`). Phase 2 has no journal
    /// and leaves these at 0; `fsync` degrades to a direct writeback. Phase 4
    /// sets them on commit and `fsync` waits on the recorded transaction.
    #[expect(dead_code)]
    sync_tid: Tid,
    #[expect(dead_code)]
    datasync_tid: Tid,
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

    fn block_manager(&self) -> Result<&Arc<ExtentManager>> {
        match &self.payload {
            InodePayload::DataBacked { block_manager, .. } => Ok(block_manager),
            _ => return_errno_with_message!(Errno::EINVAL, "inode has no block manager"),
        }
    }

    /// Returns `i_blocks` (512-byte sectors). For data-backed inodes the block
    /// manager owns the authoritative count; otherwise the descriptor's value.
    fn sector_count(&self) -> u64 {
        match self.block_manager() {
            Ok(bm) => bm.sector_count(),
            Err(_) => self.desc.sector_count(),
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

    fn set_file_size(&mut self, new_size: usize) {
        self.desc.set_size(new_size as u64);
    }

    fn set_mtime_ctime(&mut self, time: Duration) {
        self.desc.set_mtime(time);
        self.desc.set_ctime(time);
    }

    /// Sets the last-metadata-change time. Used by the unlink/rmdir path to bump
    /// the child's ctime when a link is dropped.
    fn set_ctime(&mut self, time: Duration) {
        self.desc.set_ctime(time);
    }

    /// Sets the deletion time (`i_dtime`). Used by the reclaim path.
    fn set_dtime(&mut self, time: Duration) {
        self.desc.set_dtime(time);
    }

    /// Clears the given inode flags. Used by rename to drop a moved directory's
    /// stale htree `INDEX` flag.
    fn remove_flags(&mut self, flags: FileFlags) {
        self.desc.remove_flags(flags);
    }

    /// Returns the inode's type.
    fn inode_type(&self) -> InodeType {
        self.desc.type_()
    }

    /// Returns the current link count.
    fn link_count(&self) -> u16 {
        self.desc.link_count()
    }

    /// Overwrites the link count. Used by the create error path to set it to 0
    /// so `Drop` (Task 4) reclaims the half-built inode.
    fn set_link_count(&mut self, count: u16) {
        self.desc.set_link_count(count);
    }

    /// Adds `delta` to the link count. Used by the create path to bump the
    /// parent directory's count for a new subdirectory's `..` reference.
    fn inc_link_count(&mut self, delta: u16) {
        self.desc.inc_link_count(delta);
    }

    /// Subtracts `delta` from the link count. Used by the unlink/rmdir path to
    /// drop a name's reference; reaching 0 triggers reclaim on the last `Drop`.
    fn dec_link_count(&mut self, delta: u16) {
        self.desc.dec_link_count(delta);
    }

    /// Rejects growth beyond the maximum representable file size.
    fn ensure_size_within_limit(&self, fs: &Ext4, new_size: usize) -> Result<()> {
        let max = match self.desc.type_() {
            InodeType::File => fs.max_file_size(),
            _ => u32::MAX as usize,
        };
        if new_size > max {
            return_errno_with_message!(Errno::EFBIG, "inode size exceeds ext4 maximum");
        }
        Ok(())
    }

    /// Resizes the page cache and keeps the backend's `npages` bound in sync.
    ///
    /// Ordering (report §5.2 rule 4): on grow the file size is published before
    /// the VMO grows; on shrink the VMO shrinks before the size drops. The page
    /// cache's `resize` takes `(new, old)`; mirroring ext2, the caller passes
    /// the captured sizes so this stays correct in both directions.
    fn resize_page_cache(&mut self, new_size: usize, old_size: usize) -> Result<()> {
        let InodePayload::DataBacked {
            page_cache,
            block_manager,
        } = &self.payload
        else {
            return_errno_with_message!(Errno::EINVAL, "inode has no data page cache");
        };
        page_cache.resize(new_size, old_size)?;
        block_manager.set_npages(new_size.div_ceil(PAGE_SIZE));
        Ok(())
    }

    /// Prepares the inode for a write spanning `[offset, end)`: grows the page
    /// cache if extending, then allocates data blocks for any holes covered.
    ///
    /// On failure the caller must invoke `rollback_write` to restore page-cache
    /// capacity and free the partially allocated blocks.
    fn prepare_write(
        &mut self,
        fs: &Ext4,
        offset: usize,
        end: usize,
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        let old_size = self.file_size();
        if end > old_size {
            self.ensure_size_within_limit(fs, end)?;
            self.resize_page_cache(end, old_size)?;
        }
        let start_block = (offset / BLOCK_SIZE) as Iblock;
        let end_block = end.div_ceil(BLOCK_SIZE) as Iblock;
        self.block_manager()?
            .ensure_allocated(start_block, end_block, handle)
    }

    /// Restores page-cache capacity and frees blocks allocated past `old_size`
    /// after a failed write.
    fn rollback_write(&mut self, old_size: usize, end: usize, handle: Option<&journal::Handle>) {
        if end <= old_size {
            return;
        }
        if let Err(err) = self.resize_page_cache(old_size, end) {
            error!(
                "write_at: cleanup page cache resize failed: old_size={}, err={:?}",
                old_size, err
            );
        }
        if let Ok(block_manager) = self.block_manager()
            && let Err(err) = block_manager.truncate_to_byte_len(old_size, handle)
        {
            error!("write_at: cleanup block truncate failed: {:?}", err);
        }
    }

    /// Truncates or extends the file to `new_size` bytes, updating the page
    /// cache, block mappings, and size. Mirrors ext2 `inode/file.rs:resize`.
    ///
    /// Shrinking zeroes the partial tail (in the page cache, via
    /// `resize_page_cache`) before freeing the trailing data/metadata blocks;
    /// expanding is sparse (no allocation — the gap stays a hole that reads as
    /// zeros). The caller updates timestamps and holds the `inner` write lock.
    fn resize(
        &mut self,
        fs: &Ext4,
        new_size: usize,
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        let old_size = self.file_size();
        if new_size == old_size {
            return Ok(());
        }
        if new_size < old_size {
            self.shrink(new_size, handle)?;
        } else {
            self.expand(fs, new_size)?;
        }
        self.set_mtime_ctime(super::utils::now());
        Ok(())
    }

    /// Shrinks the file: zeroes the kept partial last block in the page cache,
    /// frees every data/metadata block past `new_size`, then publishes the size.
    fn shrink(&mut self, new_size: usize, handle: Option<&journal::Handle>) -> Result<()> {
        let old_size = self.file_size();
        // Order (report §5.2 rule 4, shrink): zero + shrink the VMO before the
        // size drops. `PageCache::resize` zeroes `[new_size, block_end)` of the
        // kept partial block (BLOCK_SIZE == PAGE_SIZE), so stale tail bytes do
        // not reappear if the file is later extended.
        self.resize_page_cache(new_size, old_size)?;
        self.block_manager()?
            .truncate_to_byte_len(new_size, handle)?;
        self.set_file_size(new_size);
        Ok(())
    }

    /// Expands the file sparsely: grows the page cache and publishes the new
    /// size without allocating any data block — the gap stays a hole.
    fn expand(&mut self, fs: &Ext4, new_size: usize) -> Result<()> {
        let old_size = self.file_size();
        if new_size <= old_size {
            return Ok(());
        }
        self.ensure_size_within_limit(fs, new_size)?;
        // Order (report §5.2 rule 4, grow): publish the size before the VMO
        // grows; `resize_page_cache` keeps the backend's `npages` bound in sync.
        self.set_file_size(new_size);
        self.resize_page_cache(new_size, old_size)?;
        Ok(())
    }

    /// Writes file data at `offset` through the page cache.
    fn write_at(
        &mut self,
        fs: &Ext4,
        offset: usize,
        reader: &mut VmReader,
        handle: Option<&journal::Handle>,
    ) -> Result<usize> {
        let write_len = reader.remain();
        if write_len == 0 {
            return Ok(0);
        }
        let end = offset
            .checked_add(write_len)
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "write range overflow"))?;
        let old_size = self.file_size();

        if let Err(err) = self.prepare_write(fs, offset, end, handle) {
            self.rollback_write(old_size, end, handle);
            return Err(err);
        }
        if let Err(err) = self.page_cache()?.write(offset, reader) {
            self.rollback_write(old_size, end, handle);
            return Err(err.into());
        }

        self.set_mtime_ctime(super::utils::now());
        if end > old_size {
            self.set_file_size(end);
        }
        Ok(write_len)
    }

    fn is_dirty(&self) -> bool {
        self.desc.is_dirty()
            || self
                .block_manager()
                .is_ok_and(|block_manager| block_manager.is_dirty())
    }

    fn clear_dirty(&mut self) {
        self.desc.clear_dirty();
        if let Ok(block_manager) = self.block_manager() {
            block_manager.clear_dirty();
        }
    }

    /// Persists the inode's mutable metadata to its on-disk `RawInode` if dirty,
    /// pulling the extent root and `i_blocks` from the block manager, and clears
    /// the dirty flags.
    fn write_back_inode_desc(
        &mut self,
        fs: &Ext4,
        ino: Ext4Ino,
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        if !self.is_dirty() {
            return Ok(());
        }
        let (root, sector_count) = match self.block_manager() {
            Ok(bm) => (bm.root_snapshot(), bm.sector_count()),
            Err(_) => (*self.desc.raw_block(), self.desc.sector_count()),
        };
        // Mirror the authoritative `i_blocks` into the descriptor before writing.
        self.desc.set_sector_count(sector_count);
        fs.write_back_inode_desc(ino, &self.desc, &root, handle)?;
        self.clear_dirty();
        Ok(())
    }

    /// Flushes dirty data pages in `[0, file_size)`.
    fn sync_data_pages(&self) -> Result<()> {
        let file_size = self.file_size();
        if file_size == 0 {
            return Ok(());
        }
        match &self.payload {
            InodePayload::DataBacked { page_cache, .. } => page_cache.flush_range(0..file_size),
            _ => Ok(()),
        }
    }
}

/// Type-specific inode contents.
enum InodePayload {
    /// Regular files and directories: page-cached data mapped by extents. Also
    /// slow (block-backed) symlinks, whose target lives in a data block.
    DataBacked {
        page_cache: PageCache,
        /// The authoritative extent tree + `i_blocks`, and the page-cache
        /// backend (the page cache holds only a `Weak` to it).
        block_manager: Arc<ExtentManager>,
    },
    /// Fast (inline) symlinks: the target bytes sit in the 60-byte `i_block`
    /// area without any data block, and the `EXTENTS` flag is cleared.
    FastSymlink { target: FastSymlinkTarget },
    /// Inline data (small files stored in the inode); unsupported in Phase 1.
    #[expect(dead_code)]
    Inline,
    /// Devices and special files (filled in by later tasks).
    NoPayload,
}

impl InodePayload {
    fn new(desc: &InodeDesc, fs: Weak<Ext4>) -> Self {
        match desc.type_() {
            InodeType::File | InodeType::Dir => Self::new_data_backed(
                desc.size() as usize,
                *desc.raw_block(),
                desc.sector_count(),
                fs,
            ),
            // A symlink is fast (inline) when it is not extent-based and its
            // target fits in the `i_block` area; otherwise it is a slow,
            // extent-mapped data block. A freshly created symlink (before
            // `write_link`) starts extent-flagged and size 0, so it decodes as
            // `DataBacked` here and `write_link` later flips it to a fast
            // symlink if the target is short.
            InodeType::SymLink => {
                let size = desc.size() as usize;
                if !desc.is_extent_based() && size <= MAX_FAST_SYMLINK_LEN {
                    Self::FastSymlink {
                        target: FastSymlinkTarget::new(*desc.raw_block()),
                    }
                } else {
                    Self::new_data_backed(size, *desc.raw_block(), desc.sector_count(), fs)
                }
            }
            // Devices and special files are handled by later tasks.
            _ => Self::NoPayload,
        }
    }

    fn new_data_backed(
        size: usize,
        root: [u32; RAW_BLOCK_PTRS_LEN],
        sector_count: u64,
        fs: Weak<Ext4>,
    ) -> Self {
        let page_cache_size = size.align_up(PAGE_SIZE);
        let page_count = page_cache_size / PAGE_SIZE;
        let extent_manager = Arc::new(ExtentManager::new(root, sector_count, fs, page_count));
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

#[cfg(ktest)]
mod write_tests {
    use ostd::prelude::*;

    use super::{
        super::test_utils::{
            Ext4Fixture, Ext4FixtureBuilder, make_empty_file_inode, make_unwritten_file_inode,
        },
        extent_manager::MapState,
        *,
    };
    use crate::time::clocks;

    const FILE_INO: u32 = 11;
    const SECTORS_PER_BLOCK: u64 = (BLOCK_SIZE / SECTOR_SIZE) as u64;

    /// A fixture with a realistic bitmap and an empty regular file at `FILE_INO`.
    fn fixture_with_empty_file() -> Ext4Fixture {
        clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        f.write_raw_inode(FILE_INO, &make_empty_file_inode());
        f
    }

    fn write_all(inode: &Inode, offset: usize, data: &[u8]) -> usize {
        let mut reader = VmReader::from(data).to_fallible();
        inode.write_at(offset, &mut reader).unwrap()
    }

    fn read_back(inode: &Inode, offset: usize, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        let mut writer = VmWriter::from(buf.as_mut_slice()).to_fallible();
        let read = inode.read_at(offset, &mut writer).unwrap();
        buf.truncate(read);
        buf
    }

    #[ktest]
    fn write_fresh_file_all_holes_round_trip() {
        let f = fixture_with_empty_file();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        let content = b"hello ext4 buffered write path, this is task 3!";
        assert_eq!(write_all(&inode, 0, content), content.len());
        assert_eq!(inode.size(), content.len());
        assert_eq!(read_back(&inode, 0, content.len()), content);

        // One data block allocated => i_blocks grew by one block of sectors.
        assert_eq!(inode.sector_count(), SECTORS_PER_BLOCK);

        // The extent tree maps logical block 0 to a real written extent.
        let bm = inode.inner.read();
        let bm = bm.block_manager().unwrap();
        let mapping = bm.map_blocks(0).unwrap();
        assert_eq!(mapping.state(), MapState::Written);
    }

    #[ktest]
    fn append_past_eof_extends_file() {
        let f = fixture_with_empty_file();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        write_all(&inode, 0, &[0xAA; BLOCK_SIZE]);
        let sc_after_first = inode.sector_count();
        assert_eq!(sc_after_first, SECTORS_PER_BLOCK);

        // Append a second block past EOF.
        let appended = vec![0xBBu8; BLOCK_SIZE];
        write_all(&inode, BLOCK_SIZE, &appended);
        assert_eq!(inode.size(), 2 * BLOCK_SIZE);
        assert_eq!(inode.sector_count(), 2 * SECTORS_PER_BLOCK);
        assert_eq!(read_back(&inode, BLOCK_SIZE, BLOCK_SIZE), appended);
    }

    #[ktest]
    fn sparse_write_leaves_zero_gap() {
        let f = fixture_with_empty_file();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        // Write one block at a high offset, leaving a multi-block hole before it.
        let high_off = 5 * BLOCK_SIZE;
        let payload = vec![0xCDu8; BLOCK_SIZE];
        write_all(&inode, high_off, &payload);

        assert_eq!(inode.size(), high_off + BLOCK_SIZE);
        // Only the single written block is backed; the gap stays a hole.
        assert_eq!(inode.sector_count(), SECTORS_PER_BLOCK);

        // The gap reads as zeros; the written region reads back the payload.
        assert_eq!(read_back(&inode, 0, BLOCK_SIZE), vec![0u8; BLOCK_SIZE]);
        assert_eq!(read_back(&inode, high_off, BLOCK_SIZE), payload);
    }

    #[ktest]
    fn overwrite_existing_data_no_new_allocation() {
        let f = fixture_with_empty_file();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        write_all(&inode, 0, &[0x11; BLOCK_SIZE]);
        let sc_before = inode.sector_count();
        let free_before = f.ext4.super_block().free_blocks_count();

        // Overwrite the same block: no allocation, sector_count unchanged.
        let new_data = vec![0x22u8; BLOCK_SIZE];
        write_all(&inode, 0, &new_data);
        assert_eq!(inode.sector_count(), sc_before);
        assert_eq!(f.ext4.super_block().free_blocks_count(), free_before);
        assert_eq!(read_back(&inode, 0, BLOCK_SIZE), new_data);
    }

    #[ktest]
    fn scattered_writes_grow_tree_to_depth1() {
        let f = fixture_with_empty_file();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        // Five non-contiguous single blocks overflow the 4-entry inline root,
        // forcing a depth-1 tree with an external leaf block.
        for k in 0..5usize {
            let off = k * 2 * BLOCK_SIZE; // gaps keep extents non-mergeable
            write_all(&inode, off, &[(0x30 + k as u8); BLOCK_SIZE]);
        }

        // i_blocks counts 5 data blocks + 1 extent-tree leaf block.
        assert_eq!(inode.sector_count(), 6 * SECTORS_PER_BLOCK);

        // All five logical blocks read back correctly through the external leaf.
        for k in 0..5usize {
            let off = k * 2 * BLOCK_SIZE;
            assert_eq!(
                read_back(&inode, off, BLOCK_SIZE),
                vec![0x30 + k as u8; BLOCK_SIZE]
            );
        }

        // The root is now a depth-1 index tree.
        let depth = inode
            .inner
            .read()
            .block_manager()
            .unwrap()
            .root_depth()
            .unwrap();
        assert_eq!(depth, 1);
    }

    #[ktest]
    fn write_back_inode_desc_is_lossless() {
        let f = fixture_with_empty_file();

        // Seed a few distinctive immutable fields the RMW must preserve.
        let mut raw = make_empty_file_inode();
        raw.generation = 0xDEAD_BEEF;
        raw.checksum_lo = 0x1234;
        raw.extra_isize = 32;
        raw.uid = 0x1111;
        raw.uid_high = 0x2222;
        f.write_raw_inode(FILE_INO, &raw);
        let before = f.read_raw_inode(FILE_INO);

        let inode = f.ext4.read_inode(FILE_INO).unwrap();
        write_all(&inode, 0, b"persisted");
        inode.sync_metadata().unwrap();

        let after = f.read_raw_inode(FILE_INO);

        // Mutated fields changed.
        assert_eq!(after.size_lo, b"persisted".len() as u32);
        assert!(after.sector_count > 0);
        assert_ne!(after.block, before.block); // extent root rewritten

        // Untouched fields preserved.
        assert_eq!(after.generation, before.generation);
        assert_eq!(after.checksum_lo, before.checksum_lo);
        assert_eq!(after.extra_isize, before.extra_isize);
        assert_eq!(after.uid, before.uid);
        assert_eq!(after.uid_high, before.uid_high);
        assert_eq!(after.crtime, before.crtime);
        assert_eq!(after.crtime_extra, before.crtime_extra);
    }

    #[ktest]
    fn remount_persistence_round_trip() {
        let f = fixture_with_empty_file();
        let content = b"survives a remount of the same disk image";

        {
            let inode = f.ext4.read_inode(FILE_INO).unwrap();
            write_all(&inode, 0, content);
            // Full sync: data pages + inode metadata + block-side metadata.
            inode.sync_data_and_meta().unwrap();
            f.ext4.sync_metadata().unwrap();
        }

        // Re-open the filesystem from the same on-disk image and read it back.
        let ext4 = Ext4::open(f.disk.clone() as Arc<dyn BlockDevice>).unwrap();
        let inode = ext4.read_inode(FILE_INO).unwrap();
        assert_eq!(inode.size(), content.len());
        assert_eq!(inode.sector_count(), SECTORS_PER_BLOCK);
        let mut buf = vec![0u8; content.len()];
        let mut writer = VmWriter::from(buf.as_mut_slice()).to_fallible();
        inode.read_at(0, &mut writer).unwrap();
        assert_eq!(&buf[..], content);
    }

    /// Returns whether physical block `pblock` is marked allocated in group 0.
    fn block_is_allocated(f: &Ext4Fixture, pblock: Ext4Bid) -> bool {
        let group = f.ext4.block_group(0);
        group
            .metadata()
            .block_bitmap
            .is_allocated((pblock - group.first_block()) as u16)
    }

    #[ktest]
    fn shrink_frees_blocks_and_zeroes_partial_tail() {
        let f = fixture_with_empty_file();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        // A 3-block file (contiguous extent, blocks 0..3).
        let old_size = 3 * BLOCK_SIZE;
        write_all(&inode, 0, &vec![0xEE; old_size]);
        assert_eq!(inode.sector_count(), 3 * SECTORS_PER_BLOCK);

        // Record the physical blocks for block 2 (freed) and block 1 (the kept
        // partial block whose tail must be zeroed).
        let (b1, b2) = {
            let inner = inode.inner.read();
            let bm = inner.block_manager().unwrap();
            (
                bm.map_blocks(1).unwrap().pblock(),
                bm.map_blocks(2).unwrap().pblock(),
            )
        };
        assert!(block_is_allocated(&f, b1));
        assert!(block_is_allocated(&f, b2));
        let free_before = f.ext4.super_block().free_blocks_count();

        // Shrink to a non-block-aligned size landing inside block 1. Blocks 0 and
        // 1 are kept (1 is the partial last block); block 2 is freed.
        let new_size = BLOCK_SIZE + 100;
        inode.resize(new_size).unwrap();

        assert_eq!(inode.size(), new_size);
        // One data block freed: sector_count dropped by one block of sectors.
        assert_eq!(inode.sector_count(), 2 * SECTORS_PER_BLOCK);
        assert_eq!(f.ext4.super_block().free_blocks_count(), free_before + 1);
        assert!(block_is_allocated(&f, b1)); // kept partial block stays allocated
        assert!(!block_is_allocated(&f, b2)); // trailing block freed

        // The retained head bytes of block 1 are unchanged.
        assert_eq!(read_back(&inode, BLOCK_SIZE, 100), vec![0xEE; 100]);

        // Re-expand to expose the kept partial block's tail: it must read as
        // zeros (stale 0xEE bytes beyond `new_size` were zeroed before shrink),
        // proving the partial block was zeroed and stale data did not reappear.
        let tail_len = 2 * BLOCK_SIZE - new_size;
        inode.resize(2 * BLOCK_SIZE).unwrap();
        assert_eq!(read_back(&inode, new_size, tail_len), vec![0u8; tail_len]);
        // The head bytes are still intact after the round trip.
        assert_eq!(read_back(&inode, BLOCK_SIZE, 100), vec![0xEE; 100]);
    }

    #[ktest]
    fn shrink_to_zero_frees_everything() {
        let f = fixture_with_empty_file();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        // Five scattered single blocks force a depth-1 tree with an external leaf.
        for k in 0..5usize {
            write_all(&inode, k * 2 * BLOCK_SIZE, &[(0x40 + k as u8); BLOCK_SIZE]);
        }
        // 5 data blocks + 1 external leaf block.
        assert_eq!(inode.sector_count(), 6 * SECTORS_PER_BLOCK);
        let free_before = f.ext4.super_block().free_blocks_count();

        inode.resize(0).unwrap();

        assert_eq!(inode.size(), 0);
        assert_eq!(inode.sector_count(), 0);
        // All 5 data blocks + the leaf block returned to the allocator.
        assert_eq!(f.ext4.super_block().free_blocks_count(), free_before + 6);

        // The tree is back to an empty inline depth-0 root.
        let inner = inode.inner.read();
        let bm = inner.block_manager().unwrap();
        assert_eq!(bm.root_depth().unwrap(), 0);
        assert_eq!(bm.map_blocks(0).unwrap().state(), MapState::Hole);
    }

    #[ktest]
    fn expand_is_sparse_and_reads_zeros() {
        let f = fixture_with_empty_file();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        let payload = b"sparse expand";
        write_all(&inode, 0, payload);
        let sc_before = inode.sector_count();
        let free_before = f.ext4.super_block().free_blocks_count();

        // Expand far past EOF: no allocation, the gap is a hole.
        let new_size = 4 * BLOCK_SIZE;
        inode.resize(new_size).unwrap();

        assert_eq!(inode.size(), new_size);
        assert_eq!(inode.sector_count(), sc_before);
        assert_eq!(f.ext4.super_block().free_blocks_count(), free_before);

        // The original bytes survive; the rest of the file reads as zeros.
        assert_eq!(read_back(&inode, 0, payload.len()), payload);
        assert_eq!(
            read_back(&inode, payload.len(), new_size - payload.len()),
            vec![0u8; new_size - payload.len()]
        );
    }

    #[ktest]
    fn write_into_unwritten_extent_converts_and_reads_back() {
        clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        // A file with a single 4-block unwritten (preallocated) extent at pblock
        // 200, logical size 4 blocks. The blocks are already in i_blocks.
        let len = 4u16;
        f.write_raw_inode(
            FILE_INO,
            &make_unwritten_file_inode(200, len, (len as u32) * BLOCK_SIZE as u32),
        );
        let inode = f.ext4.read_inode(FILE_INO).unwrap();
        let sc_before = inode.sector_count();
        assert_eq!(sc_before, len as u64 * SECTORS_PER_BLOCK);
        let free_before = f.ext4.super_block().free_blocks_count();

        // The whole extent reads as zeros while unwritten.
        assert_eq!(
            read_back(&inode, 0, len as usize * BLOCK_SIZE),
            vec![0u8; len as usize * BLOCK_SIZE]
        );

        // Write into the middle block (logical block 1) only.
        let payload = vec![0x77u8; BLOCK_SIZE];
        write_all(&inode, BLOCK_SIZE, &payload);

        // The written block now reads back the real bytes...
        assert_eq!(read_back(&inode, BLOCK_SIZE, BLOCK_SIZE), payload);
        // ...while the still-unwritten blocks read as zeros.
        assert_eq!(read_back(&inode, 0, BLOCK_SIZE), vec![0u8; BLOCK_SIZE]);
        assert_eq!(
            read_back(&inode, 2 * BLOCK_SIZE, 2 * BLOCK_SIZE),
            vec![0u8; 2 * BLOCK_SIZE]
        );

        // The extent split: block 1 is Written at the preserved physical block
        // (200 + 1 = 201); blocks 0 and 2 remain Unwritten.
        let inner = inode.inner.read();
        let bm = inner.block_manager().unwrap();
        let m0 = bm.map_blocks(0).unwrap();
        let m1 = bm.map_blocks(1).unwrap();
        let m2 = bm.map_blocks(2).unwrap();
        assert_eq!(m0.state(), MapState::Unwritten);
        assert_eq!(m1.state(), MapState::Written);
        assert_eq!(m1.pblock(), 201);
        assert_eq!(m2.state(), MapState::Unwritten);

        // No data block allocated or freed: the inline split fits the root, so
        // i_blocks is unchanged (conversion is metadata-only, no extra leaf).
        assert_eq!(inode.sector_count(), sc_before);
        assert_eq!(f.ext4.super_block().free_blocks_count(), free_before);
    }

    #[ktest]
    fn resize_on_directory_is_eisdir() {
        let f = fixture_with_empty_file();
        // ROOT_INO (2) is a directory in the fixture image.
        let dir = f.ext4.read_inode(2).unwrap();
        assert_eq!(dir.inode_type(), InodeType::Dir);
        assert_eq!(dir.resize(0).unwrap_err().error(), Errno::EISDIR);
    }
}
