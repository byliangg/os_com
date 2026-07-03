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
//! Inode::inner → journal handle → ExtentManager::state
//!     → Ext4::s_orphan_lock → Ext4::super_block → BlockGroup::metadata
//! ```
//!
//! The journal handle (`Ext4::begin_op` → `journal_start`, position ②) is taken
//! right after `inner`; `s_orphan_lock` (the orphan chain) sits after the handle
//! and before the superblock; the journal *state* lock is a leaf the metadata
//! funnels take last (never across a wait). `BlockGroup::inode_cache`
//! is independent: it is never held while acquiring `super_block`/`metadata`, nor
//! while syncing an inode (`sync_inodes` clones the `Arc`s out and drops the read
//! lock first).

use super::{
    checksum::crc32c,
    fs::{Ext4, OrphanLink},
    journal,
    journal::Tid,
    prelude::*,
};

mod dir;
mod extent_manager;
mod symlink;

use self::{
    extent_manager::{ExtentManager, ExtentTree},
    symlink::FastSymlinkTarget,
};
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
    dtime: Dtime,
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

/// The in-memory state of `i_dtime`, whose 32 on-disk bits ext4 overloads:
/// normally the deletion timestamp, but the orphan-chain "next" pointer while
/// the inode is linked on the list. The two meanings are disjoint in time (an
/// inode carries a pointer only while listed and a real deletion time only
/// once freed) — this enum keeps them apart in memory instead of reproducing
/// the pun in a `Duration`; they collapse to the shared u32 only at
/// [`to_raw`](Self::to_raw).
#[derive(Clone, Copy, Debug)]
pub(super) enum Dtime {
    /// The deletion timestamp — ext4's normal meaning (zero = live).
    Time(Duration),
    /// On the orphan chain: `i_dtime` carries the successor (`None` = end of
    /// chain). The authoritative successor of a *cached* on-list inode is the
    /// fs-level in-memory chain; this records the value known at add time.
    OrphanNext(Option<Ext4Ino>),
}

impl Dtime {
    /// Returns the on-disk `i_dtime` encoding (`0` = live / end-of-chain —
    /// the on-disk convention; the sentinel exists only past this boundary).
    pub(super) const fn to_raw(self) -> u32 {
        match self {
            // Clamp rather than wrap a post-2106 timestamp: 2^32 seconds
            // would even encode as 0 = "live" (same rationale as
            // `encode_time`).
            Dtime::Time(time) => {
                let secs = time.as_secs();
                if secs > u32::MAX as u64 {
                    u32::MAX
                } else {
                    secs as u32
                }
            }
            Dtime::OrphanNext(next) => match next {
                Some(ino) => ino,
                None => 0,
            },
        }
    }
}

/// What a fresh inode's type-specific `i_block` area holds.
pub(super) enum InodeSeed {
    /// Files, directories, symlinks: a valid empty extent root, `EXTENTS` set.
    ExtentRoot,
    /// Character/block devices: the Linux special-file device-number encoding
    /// (the glibc-encoded id from `mknod(2)`), no `EXTENTS`.
    Device(u64),
    /// FIFOs and sockets: an all-zero `i_block`, no `EXTENTS`.
    Nothing,
}

/// Encodes a device id into the ext4 special-file `i_block` layout (Linux
/// `ext4_iget`/`ext4_do_update_inode`): 8-bit major/minor pairs use the old
/// `(major << 8) | minor` form in word 0, anything wider the `new_encode_dev`
/// form in word 1.
fn encode_device_block(device_id: u64) -> [u32; RAW_BLOCK_PTRS_LEN] {
    let (major, minor) = device_id::decode_device_numbers(device_id);
    let mut block = [0u32; RAW_BLOCK_PTRS_LEN];
    if major < 256 && minor < 256 {
        block[0] = (major << 8) | minor;
    } else {
        block[1] = (minor & 0xFF) | (major << 8) | ((minor & !0xFF) << 12);
    }
    block
}

/// Decodes the ext4 special-file device encoding stored in `i_block` (the
/// inverse of [`encode_device_block`]).
fn decode_device_block(block: &[u32; RAW_BLOCK_PTRS_LEN]) -> u64 {
    let (major, minor) = if block[0] != 0 {
        ((block[0] >> 8) & 0xFF, block[0] & 0xFF)
    } else {
        let dev = block[1];
        ((dev & 0xFFF00) >> 8, (dev & 0xFF) | ((dev >> 12) & 0xFFF00))
    };
    device_id::encode_device_numbers(major, minor)
}

impl InodeDesc {
    /// Builds a fresh inode descriptor for a newly created inode.
    ///
    /// Size and `i_blocks` start at zero and all timestamps are `now`; `seed`
    /// decides the type-specific `i_block` area — a valid empty extent root
    /// with `EXTENTS` set for files/directories/symlinks, the device-number
    /// encoding for device nodes, nothing for FIFOs/sockets. Mirrors ext2
    /// `InodeDesc::new`, diverging only in the extent root + flag (ext2
    /// leaves zeroed indirect pointers and no flag).
    #[expect(clippy::too_many_arguments)]
    pub(super) fn new(
        type_: InodeType,
        perm: FilePerm,
        uid: u32,
        gid: u32,
        link_count: u16,
        generation: u32,
        now: Duration,
        seed: InodeSeed,
    ) -> Self {
        let (flags, block) = match seed {
            // A valid empty extent root, so the extent reader sees a
            // well-formed tree from the first byte — unlike ext2, whose new
            // inodes start with zeroed indirect-block pointers.
            InodeSeed::ExtentRoot => (FileFlags::EXTENTS, *ExtentTree::empty().root_bytes()),
            // Special files carry no extent tree: devices hold the Linux
            // device-number encoding in `i_block`, FIFOs/sockets hold nothing.
            // Neither sets `EXTENTS`, so the extent reader never parses them.
            InodeSeed::Device(device_id) => (FileFlags::empty(), encode_device_block(device_id)),
            InodeSeed::Nothing => (FileFlags::empty(), [0u32; RAW_BLOCK_PTRS_LEN]),
        };
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
            dtime: Dtime::Time(Duration::ZERO),
            link_count,
            sector_count: 0,
            flags,
            file_acl: 0,
            generation,
            block,
        }
    }

    /// Returns the device id encoded in `i_block`, for character/block device
    /// inodes (`None` for every other type — their `i_block` is not a device
    /// encoding).
    pub(super) fn device_id(&self) -> Option<u64> {
        match self.type_ {
            InodeType::CharDevice | InodeType::BlockDevice => {
                Some(decode_device_block(&self.block))
            }
            _ => None,
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
        self.dtime = Dtime::Time(time);
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

    /// Returns the on-disk `i_dtime` encoding of the current state — the one
    /// place both meanings collapse to the shared u32 (encode boundary).
    pub(super) const fn raw_dtime(&self) -> u32 {
        self.dtime.to_raw()
    }

    /// Sets `i_dtime` to encode this inode's successor on the orphan list (ext4
    /// reuses `i_dtime` as the orphan "next" pointer while an inode is linked
    /// onto `s_last_orphan`). `0` marks the list tail.
    ///
    /// This is the same on-disk field as [`set_dtime`](Self::set_dtime); the two
    /// uses are disjoint in time — an inode carries a next-pointer only while it
    /// is on the orphan list, and a real deletion time only once it is freed.
    /// The authoritative successor of a *cached* on-list inode lives in the
    /// filesystem's in-memory orphan chain (`Ext4::s_orphan_lock`) — a non-head
    /// removal splices the on-disk pointer without reaching this descriptor —
    /// so the journaled writeback overrides `i_dtime` from that chain; this
    /// setter records the value known at add time.
    pub(super) fn set_orphan_next(&mut self, next: Option<Ext4Ino>) {
        self.dtime = Dtime::OrphanNext(next);
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

    /// Resolves the physical device block backing each of this descriptor's
    /// first `nblocks` logical blocks.
    ///
    /// This keeps the extent engine encapsulated in the `inode` module while
    /// handing callers a plain, fully resolved block map. The journal uses it
    /// to build its log block map from the journal inode (ino 8), whose data
    /// blocks hold the log; every log block must be a real allocated, written
    /// block, so a hole or unwritten block is an error rather than a
    /// zero-filled read.
    pub(in crate::fs::fs_impls::ext4) fn map_all_blocks(
        &self,
        fs: Weak<Ext4>,
        nblocks: u32,
    ) -> Result<Vec<Ext4Bid>> {
        let em =
            ExtentManager::try_new(*self.raw_block(), self.sector_count(), fs, nblocks as usize)?;
        let mut map = Vec::with_capacity(nblocks as usize);
        let mut i: Iblock = 0;
        while i < nblocks {
            let extent_manager::Mapping::Mapped {
                pblock,
                len,
                written: true,
            } = em.map_blocks(i)?
            else {
                return_errno_with_message!(
                    Errno::EUCLEAN,
                    "journal inode has an unmapped (hole/unwritten) block"
                );
            };
            let run = len.min(nblocks - i);
            for k in 0..run {
                map.push(pblock + k as Ext4Bid);
            }
            i += run;
        }
        Ok(map)
    }

    /// Builds the complete on-disk [`RawInode`] for this descriptor with `root`
    /// as its inline extent-tree root — every field from the in-memory
    /// descriptor, never from the device. The encode counterpart of
    /// [`InodeDesc::try_from`], kept beside it so the lossless-writeback
    /// invariant is reviewable in one place.
    ///
    /// This is the authoritative encoding used by both the new-inode write and
    /// the *journaled* writeback. Under a handle the on-disk inode may be
    /// **stale** (its previous write was suppressed and not yet checkpointed),
    /// so a read-modify-write from the device would resurrect zeroed `i_mode`
    /// type bits, `extra_isize`, `generation`, and nanosecond timestamps — the
    /// corruption the guest e2fsck caught after a rename touched a directory
    /// whose creation had not yet checkpointed. Encoding straight from `self`
    /// (which `try_from` loads in full: type, generation, crtime, …) sidesteps
    /// that entirely.
    pub(super) fn to_raw_inode(&self, root: &[u32; RAW_BLOCK_PTRS_LEN]) -> RawInode {
        let (mtime_secs, mtime_extra) = encode_time(self.mtime());
        let (ctime_secs, ctime_extra) = encode_time(self.ctime());
        let (atime_secs, atime_extra) = encode_time(self.atime());
        let (crtime_secs, crtime_extra) = encode_time(self.crtime());
        RawInode {
            // The full mode comes from `self.type_()`, not the (possibly stale)
            // device — this is what preserves the `S_IFMT` type bits under
            // journaling.
            mode: (self.type_() as u16) | (self.perm().bits() & 0o7777),
            uid: self.uid() as u16,
            size_lo: self.size() as u32,
            atime: atime_secs,
            ctime: ctime_secs,
            mtime: mtime_secs,
            dtime: self.raw_dtime(),
            gid: self.gid() as u16,
            link_count: self.link_count(),
            sector_count: self.sector_count() as u32,
            flags: self.flags().bits(),
            block: *root,
            generation: self.generation(),
            size_high: if self.type_() == InodeType::File {
                (self.size() >> 32) as u32
            } else {
                0
            },
            blocks_high: (self.sector_count() >> 32) as u16,
            uid_high: (self.uid() >> 16) as u16,
            gid_high: (self.gid() >> 16) as u16,
            // The `extra_isize` a 256-byte inode carries (32 bytes past the
            // 128-byte base), so the nanosecond timestamps are honored on read.
            extra_isize: 32,
            ctime_extra,
            mtime_extra,
            atime_extra,
            crtime: crtime_secs,
            crtime_extra,
            ..Default::default()
        }
    }
}

/// Decodes an ext4 timestamp from its seconds field and the `*_extra` field.
///
/// The extra field packs a 2-bit epoch (extending seconds past 2038) in its low
/// bits and nanoseconds in the upper bits (report §4.3).
fn decode_time(secs: u32, extra: u32) -> Duration {
    let epoch = (extra & 0x3) as i64;
    let nsec = extra >> 2;
    // Linux `ext4_decode_extra_time`: the base seconds are SIGNED and the
    // 2-bit epoch extends them upward, so epoch 0 spans 1901..2038 and epoch 1
    // continues seamlessly at 2^31. Decoding the base as unsigned misread
    // foreign images' pre-1970 timestamps as far-future (P1 review item).
    // `Duration` cannot express pre-1970 at all; clamp those to the epoch.
    let secs = (secs as i32) as i64 + (epoch << 32);
    Duration::new(u64::try_from(secs).unwrap_or(0), nsec)
}

/// Encodes a timestamp into its on-disk `(seconds, *_extra)` pair — the
/// reverse of [`decode_time`]. Also used by the sync path's raw-inode RMW in
/// `fs.rs`.
///
/// The 2-bit epoch encodes seconds only up to 2^34 - 1 (~year 2514). Clamp
/// rather than wrap: `secs` can come straight from `utimensat`, and a wrapped
/// value would read back as an unrelated timestamp (Linux truncates to the
/// filesystem's range at the VFS layer, `timestamp_truncate`).
pub(super) fn encode_time(time: Duration) -> (u32, u32) {
    let secs = time.as_secs().min((1 << 34) - 1);
    let nsec = time.subsec_nanos();
    let epoch = (secs >> 32) & 0x3;
    let secs_lo = secs as u32;
    let extra = (epoch as u32) | (nsec << 2);
    (secs_lo, extra)
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
            // Decode boundary: a normal read always carries a deletion time
            // (an on-list inode has link count 0, which `try_from` rejects;
            // the recovery scan reads the raw pointer directly).
            dtime: Dtime::Time(Duration::from_secs(raw.dtime as u64)),
            link_count: raw.link_count,
            sector_count,
            flags,
            file_acl,
            generation: raw.generation,
            block: raw.block,
        })
    }
}

/// Byte offset of `i_checksum_lo` in [`RawInode`] (0x7C, inside osd2); the low
/// half of the inode checksum, present in every inode.
const I_CHECKSUM_LO_OFFSET: usize = 0x7C;

/// Byte offset of `i_checksum_hi` in [`RawInode`] (0x82, just past
/// `i_extra_isize`); the high half, present only when the inode carries the
/// extra-size region (`s_inode_size > 128`).
const I_CHECKSUM_HI_OFFSET: usize = 0x82;

impl InodeDesc {
    /// The per-inode checksum seed: `crc32c(crc32c(fs_seed, ino), generation)`
    /// (Linux `ext4_inode_csum` / `ei->i_csum_seed`). Folds the inode number and
    /// generation into the filesystem seed so an inode's checksum does not match
    /// after it is reused elsewhere.
    fn inode_csum_seed(fs_seed: u32, ino: Ext4Ino, generation: u32) -> u32 {
        let seed = crc32c(fs_seed, &ino.to_le_bytes());
        crc32c(seed, &generation.to_le_bytes())
    }

    /// The full 32-bit crc32c of `raw` over the whole `inode_size`, with both
    /// checksum fields treated as zero (Linux `ext4_inode_csum`). The caller
    /// splits it into `i_checksum_lo` (low 16 bits) and, when the inode has the
    /// extra region, `i_checksum_hi` (high 16 bits).
    fn inode_checksum(raw: &RawInode, ino: Ext4Ino, fs_seed: u32, inode_size: usize) -> u32 {
        let seed = Self::inode_csum_seed(fs_seed, ino, raw.generation);
        let bytes = raw.as_bytes();
        let mut crc = crc32c(seed, &bytes[..I_CHECKSUM_LO_OFFSET]);
        crc = crc32c(crc, &[0u8, 0u8]); // i_checksum_lo
        if inode_size > 128 {
            // The extra region carries i_checksum_hi: checksum the gap between
            // the two fields, then the zeroed hi, then the remainder.
            crc = crc32c(crc, &bytes[I_CHECKSUM_LO_OFFSET + 2..I_CHECKSUM_HI_OFFSET]);
            crc = crc32c(crc, &[0u8, 0u8]); // i_checksum_hi
            crc = crc32c(crc, &bytes[I_CHECKSUM_HI_OFFSET + 2..inode_size]);
        } else {
            crc = crc32c(crc, &bytes[I_CHECKSUM_LO_OFFSET + 2..inode_size]);
        }
        crc
    }

    /// Verifies `raw`'s stored `i_checksum_lo` (and `i_checksum_hi` when the
    /// inode has the extra region) for a `metadata_csum` volume, at the inode
    /// read boundary. `fs_seed` is the per-filesystem seed.
    pub(super) fn verify_inode_checksum(
        raw: &RawInode,
        ino: Ext4Ino,
        fs_seed: u32,
        inode_size: usize,
    ) -> Result<()> {
        let crc = Self::inode_checksum(raw, ino, fs_seed, inode_size);
        if raw.checksum_lo != (crc & 0xFFFF) as u16 {
            return_errno_with_message!(Errno::EUCLEAN, "bad inode checksum (lo)");
        }
        if inode_size > 128 && raw.checksum_hi != ((crc >> 16) & 0xFFFF) as u16 {
            return_errno_with_message!(Errno::EUCLEAN, "bad inode checksum (hi)");
        }
        Ok(())
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
    /// This inode's own `Weak` (minted by `Arc::new_cyclic`): the write paths
    /// pass it as the ordered-data registrations' liveness gate (jbd2
    /// `data=ordered`), and `self_arc` upgrades it for the per-open layer.
    /// Never used on the `Drop`/reclaim path, where upgrading would fail.
    self_weak: Weak<Inode>,
    /// The in-memory pipe object backing a named-pipe (FIFO) inode; `None`
    /// for every other type. Created with the inode, like ext2's.
    pipe: Option<crate::fs::pipe::Pipe>,
    /// The VFS extension slot (flock, POSIX locks, inotify); must exist from
    /// day one or the VFS layer panics on inodes that use these features.
    extension: Extension,
}

impl Inode {
    /// Builds a live inode; fails if a data-backed extent root does not parse
    /// (see [`InodePayload::new`]).
    pub(super) fn new(
        ino: Ext4Ino,
        type_: InodeType,
        desc: Dirty<InodeDesc>,
        block_group_idx: usize,
        fs: Weak<Ext4>,
    ) -> Result<Arc<Self>> {
        let payload = InodePayload::new(&desc, fs.clone())?;
        let pipe = match type_ {
            InodeType::NamedPipe => Some(crate::fs::pipe::Pipe::new()),
            _ => None,
        };
        // `new_cyclic` so the write paths can hand `self_weak` to the journal's
        // ordered-inode registration; the only fallible step (payload parsing)
        // runs before the closure, which just assembles.
        Ok(Arc::new_cyclic(|self_weak| Self {
            ino,
            type_,
            inner: RwMutex::new(InodeInner {
                desc,
                payload,
                sync_tid: None,
                datasync_tid: None,
            }),
            block_group_idx,
            fs,
            self_weak: self_weak.clone(),
            pipe,
            extension: Extension::new(),
        }))
    }

    /// Builds a live inode for a crash-recovery orphan reclaim from its raw
    /// on-disk inode, whose link count is 0 (the orphan state that
    /// `InodeDesc::try_from` rejects for normal reads).
    ///
    /// Decodes the descriptor with the link count temporarily forced to 1 so the
    /// extent tree / payload build succeeds, then resets the in-memory link
    /// count to 0 so [`try_reclaim_deleted_inode`](Self::try_reclaim_deleted_inode)
    /// — run by the caller by dropping the returned `Arc` — frees it. Used only
    /// by the mount-time orphan scan (`Ext4::recover_orphan_list`).
    pub(super) fn from_raw_for_recovery(
        ino: Ext4Ino,
        raw: &RawInode,
        block_group_idx: usize,
        fs: Weak<Ext4>,
    ) -> Result<Arc<Self>> {
        let mut probe = *raw;
        probe.link_count = 1;
        let mut desc = InodeDesc::try_from(&probe)?;
        // Restore the true (0) link count so the reclaim on drop fires.
        desc.set_link_count(0);
        let type_ = desc.type_();
        Self::new(ino, type_, Dirty::new(desc), block_group_idx, fs)
    }

    pub(super) fn ino(&self) -> Ext4Ino {
        self.ino
    }

    /// Records the journal transaction that captured this inode's descriptor
    /// (see `InodeInner::sync_tid`). Used by `Ext4::create_inode`, whose fresh
    /// descriptor was written under the creating op's handle before this
    /// `Inode` existed. A no-op without a handle.
    ///
    /// Taking `inner` here while the caller holds the op handle formally
    /// reverses the inner ① → handle ② order, but cannot deadlock: the inode
    /// is not yet published (no cache entry, no second reference), so this
    /// write lock is uncontended and participates in no cycle.
    pub(super) fn record_sync_tid(&self, handle: Option<&journal::Handle>) {
        if let Some(handle) = handle {
            self.inner.write().sync_tid = Some(handle.tid());
        }
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
        let op = fs.begin_op(Ext4::WRITE_CREDITS)?;
        let len = inner.write_at(&fs, offset, reader, op.get())?;
        // Journaled: the descriptor this write mutated (size, mtime, i_blocks,
        // and — for an inline root — the extent mapping itself) must ride the
        // SAME transaction as the bitmap/GDT captures above, or a crash
        // between them persists allocated-but-unreferenced blocks (the crash
        // matrix reconstructed exactly that: a bitmap with the write's 8
        // blocks set and no extent pointing at them). Linux journals the
        // inode under every handle (ext4_mark_inode_dirty); this is our
        // equivalent.
        if op.get().is_some() {
            inner.write_back_inode_desc(&fs, self.ino, op.get())?;
        }
        // data=ordered: this write's dirty pages must reach their final blocks
        // before the transaction's commit block, or recovery could replay
        // extents that point at blocks whose data never hit the platter.
        // Registered unconditionally rather than only for allocating writes —
        // a pure overwrite's extra registration costs one idempotent flush of
        // pages that are usually clean by commit time (Linux registers only
        // newly-mapped ranges; that precision needs `ensure_allocated` to
        // report whether it allocated, a P7/P9 refinement).
        if let Some(handle) = op.get()
            && let Ok(pages) = inner.page_cache()
        {
            handle.register_ordered_data(
                self.ino,
                self.self_weak.clone(),
                pages.clone(),
                inner.file_size(),
            )?;
        }
        Ok(len)
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
        // Truncate-orphan protection is deliberately absent (owner: P7
        // `journal_restart`). It must NOT reuse the delete path's fs-level
        // `orphan_add`/`orphan_del`: a truncated inode stays live (link count
        // > 0) while the mount-time orphan scan *frees* everything on the list
        // — recovery must *re-truncate* instead, a distinct mode. Under
        // commit-per-op a whole truncate is one transaction — atomic across a
        // crash — so nothing is needed yet; P7's multi-transaction truncate
        // brings the fs-level re-truncate orphan machinery with it.
        let old_size = inner.file_size();
        inner.resize(&fs, new_size, op.get())?;
        // Same per-handle descriptor capture as `write_at`: the new size and
        // truncated extent root must commit with the bitmap/GDT changes.
        if op.get().is_some() {
            inner.write_back_inode_desc(&fs, self.ino, op.get())?;
        }
        // data=ordered on shrink: the kept partial block is re-zeroed in the
        // page cache, and that zeroing must reach the device before this
        // transaction's commit — otherwise a later sparse extend over the tail
        // could expose the pre-truncate bytes after a replay (Linux registers
        // the same case in __ext4_block_zero_page_range).
        if new_size < old_size
            && let Some(handle) = op.get()
            && let Ok(pages) = inner.page_cache()
        {
            handle.register_ordered_data(
                self.ino,
                self.self_weak.clone(),
                pages.clone(),
                inner.file_size(),
            )?;
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
            inner.sync_data_pages(&fs)?;
            // Journaled: capture instead of direct-writing (see
            // `sync_metadata`). Wait below on whichever transaction carries
            // this inode's newest capture: the one this writeback just made
            // (dirty inode), else the recorded `sync_tid` of an earlier
            // journaled capture — the dirty flag clears at capture time while
            // the commit is asynchronous, so a "clean" inode may still sit in
            // an uncommitted transaction. A clean inode with no recorded tid
            // has nothing pending, and its fresh op captured nothing — a
            // transaction with no captured blocks never becomes committable,
            // so waiting on it would sleep forever.
            let was_dirty = inner.is_dirty();
            let op = fs.begin_op(Ext4::FSYNC_CREDITS)?;
            inner.write_back_inode_desc(&fs, self.ino, op.get())?;
            if was_dirty { op.tid() } else { inner.sync_tid }
            // `op` closes here, then `inner` unlocks — the reverse of the
            // inner ① → handle ② acquisition order.
        };
        // Wait with NO filesystem locks held (jbd2 discipline; the commit
        // thread itself takes no inode lock).
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
    /// (unmount reaches durability via `flush_on_unmount`; owner: P7, a
    /// `log_wait_commit` at the `FileSystem::sync` boundary).
    pub(super) fn sync_data_and_meta_no_barrier(&self) -> Result<()> {
        let fs = self.fs()?;
        let mut inner = self.inner.write();
        inner.sync_data_pages(&fs)?;
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
    /// Maps logical block `iblock` to its physical block, or `None` for a hole.
    /// Test-only inspection used to read a file's data straight off the device
    /// (e.g. to prove the ordered flush reached the final location).
    #[cfg(ktest)]
    pub(in crate::fs::fs_impls::ext4) fn data_block_of(&self, iblock: Iblock) -> Option<Ext4Bid> {
        let inner = self.inner.read();
        let extent_manager = inner.extent_manager().ok()?;
        let mapping = extent_manager.map_blocks(iblock).ok()?;
        mapping.mapped_pblock()
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

        // Unlink this inode from the on-disk orphan list BEFORE stamping the
        // real deletion time below — while on the list, `i_dtime` doubles as the
        // orphan-next pointer, and this reclaim transaction must atomically both
        // splice the chain and free the inode (crash before its commit leaves
        // the inode chained and allocated, so recovery finishes the deletion;
        // crash after leaves it fully freed and off the chain). The successor
        // comes from the filesystem's in-memory chain; `orphan_del` of an inode
        // that was never added (a non-journaled volume) is a no-op.
        fs.orphan_del(self.ino, op.get())?;

        let old_size = inner.file_size();
        // Only data-backed inodes (files, directories, slow symlinks) own a page
        // cache and extent-mapped blocks. A fast symlink stores its target inline
        // in `i_block` with no data block, so it skips both the page-cache resize
        // and the block truncate below.
        let extent_manager = inner.extent_manager().ok().cloned();
        if extent_manager.is_some() {
            inner.resize_page_cache(0, old_size)?;
        }
        inner.set_dtime(super::utils::now());
        inner.set_file_size(0);
        // Gate on the extent manager's live `sector_count`, not the descriptor's
        // copy (which ext2 uses): the extent manager is the authority and the
        // descriptor may be stale until writeback. This divergence from the ext2
        // template is intentional — do not "fix" it back to `inner.desc`.
        if let Some(extent_manager) = extent_manager
            && extent_manager.sector_count() > 0
        {
            extent_manager.truncate_to_byte_len(0, op.get())?;
        }
        inner.write_back_inode_desc(&fs, self.ino, op.get())?;

        fs.free_inode(self.ino, self.type_, op.get())?;
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

    /// Returns the pipe object backing a named-pipe (FIFO) inode.
    pub(super) fn pipe(&self) -> Option<&crate::fs::pipe::Pipe> {
        self.pipe.as_ref()
    }

    /// Upgrades `self_weak` back to an `Arc` (for handing the inode to a
    /// per-open object). `None` only while the last `Arc` is mid-drop.
    pub(super) fn self_arc(&self) -> Option<Arc<Inode>> {
        self.self_weak.upgrade()
    }

    /// Returns the device id of a character/block device inode (`None`
    /// otherwise).
    pub(super) fn device_id(&self) -> Option<u64> {
        self.inner.read().desc.device_id()
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
    /// The transaction that captured this inode's most recent journaled
    /// writeback (jbd2 `i_sync_tid`), `None` if none. `fsync` waits on it even
    /// when the inode looks clean: the dirty flag clears at *capture* time
    /// while the commit is asynchronous, so "clean" does not imply "committed"
    /// — an earlier op or fs-level sync may have captured this inode into a
    /// transaction that is still only in memory. Non-journaled volumes leave
    /// it `None` (`fsync` degrades to the direct writeback + barrier). An
    /// `Option`, not a `0` sentinel: tids wrap (see `tid_geq`), so `0` is a
    /// legal transaction id a wrapped journal could hand out.
    sync_tid: Option<Tid>,
    /// The `fdatasync` subset (jbd2 `i_datasync_tid`). Phase 4 routes
    /// `fdatasync` through the same full-sync path, so this stays unused.
    #[expect(dead_code)]
    datasync_tid: Option<Tid>,
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

    fn extent_manager(&self) -> Result<&Arc<ExtentManager>> {
        match &self.payload {
            InodePayload::DataBacked { extent_manager, .. } => Ok(extent_manager),
            _ => return_errno_with_message!(Errno::EINVAL, "inode has no block manager"),
        }
    }

    /// Returns `i_blocks` (512-byte sectors). For data-backed inodes the block
    /// manager owns the authoritative count; otherwise the descriptor's value.
    fn sector_count(&self) -> u64 {
        match self.extent_manager() {
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

    /// Persists this inode as a freshly linked orphan, consuming the
    /// [`OrphanLink`] its `Ext4::orphan_add` minted: records the previous
    /// chain head into `i_dtime` (ext4's dual use of that field — the
    /// authoritative successor lives in `Ext4::s_orphan_lock`'s in-memory
    /// chain) and writes the inode back. Both steps MUST land in the same
    /// transaction as the add, which is exactly why they only exist fused
    /// here (see [`OrphanLink`]).
    pub(super) fn persist_as_orphan(
        &mut self,
        fs: &Ext4,
        ino: Ext4Ino,
        link: OrphanLink,
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        self.desc.set_orphan_next(link.into_old_head());
        self.write_back_inode_desc(fs, ino, handle)
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
    ///
    /// `PageCache::resize` zero-fills the partial page at the resize boundary
    /// (the kept tail on shrink, the old EOF tail on grow), and that fill marks
    /// the page dirty. Over a HOLE that dirty page has no backing block, and
    /// journaled writeback later refuses to allocate one — an ordered flush
    /// aborts the journal, an unmount flush fails with EIO. A hole already reads
    /// as zeros, so the fill only has real work when the boundary block is
    /// mapped (Linux `ext4_block_truncate_page` skips unmapped blocks the same
    /// way); over a hole we resize with page-aligned sizes, which never triggers
    /// the fill and changes nothing else (identical capacity end state and
    /// decommit range). The write path relies on Unwritten-first — not a
    /// boundary fill — to keep a freshly allocated block's stale contents out of
    /// the file (see [`write_at`](Self::write_at)).
    fn resize_page_cache(&mut self, new_size: usize, old_size: usize) -> Result<()> {
        let InodePayload::DataBacked {
            page_cache,
            extent_manager,
        } = &self.payload
        else {
            return_errno_with_message!(Errno::EINVAL, "inode has no data page cache");
        };
        let boundary = new_size.min(old_size);
        let boundary_block_is_mapped = if boundary.is_multiple_of(PAGE_SIZE) {
            // No partial page at the boundary: the cache has nothing to fill
            // either way, so skip the mapping lookup.
            false
        } else {
            let Ok(iblock) = Iblock::try_from(boundary / BLOCK_SIZE) else {
                return_errno_with_message!(
                    Errno::EFBIG,
                    "resize boundary beyond the 32-bit logical block space"
                );
            };
            matches!(
                extent_manager.map_blocks(iblock)?,
                extent_manager::Mapping::Mapped { .. }
            )
        };
        if boundary_block_is_mapped {
            page_cache.resize(new_size, old_size)?;
        } else {
            page_cache.resize(new_size.align_up(PAGE_SIZE), old_size.align_up(PAGE_SIZE))?;
        }
        extent_manager.set_npages(new_size.div_ceil(PAGE_SIZE));
        Ok(())
    }

    /// Prepares the inode for a write spanning `[offset, end)`: grows the page
    /// cache if extending, then allocates data blocks (as unwritten) for any
    /// holes covered.
    ///
    /// The grow-resize skips the boundary zero-fill over a hole (it would plant
    /// a dirty page with no backing block — a journaled-writeback bomb).
    /// Unwritten-first makes that safe: `ensure_allocated` allocates the write's
    /// hole blocks as UNWRITTEN, so the write's own sub-page page-cache commit
    /// reads them back as zeros (not the freshly recycled block's stale
    /// contents), and `write_at` converts the range to written after the data
    /// lands. A mapped partial boundary block still has its tail zeroed; a hole
    /// boundary is left untouched (`resize_page_cache`).
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
        let start_block = (offset / BLOCK_SIZE) as Iblock;
        let end_block = end.div_ceil(BLOCK_SIZE) as Iblock;
        if end > old_size {
            self.ensure_size_within_limit(fs, end)?;
            self.resize_page_cache(end, old_size)?;
        }
        self.extent_manager()?
            .ensure_allocated(start_block, end_block, handle)
    }

    /// Restores page-cache capacity and frees blocks allocated past `old_size`
    /// after a failed write.
    ///
    /// The shrink-resize skips the boundary fill over a hole: a boundary block
    /// that is still a hole here was never reached by the failed write (its
    /// page holds no write splatter to clean, and filling it would plant a
    /// dirty page over a hole), while a mapped one either predates the write
    /// (a legitimate device read) or was committed as zeros by
    /// `prepare_write`'s covered-boundary fill before allocation.
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
        if let Ok(extent_manager) = self.extent_manager()
            && let Err(err) = extent_manager.truncate_to_byte_len(old_size, handle)
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
        // Ordered-data vs. truncate: discarding the doomed tail pages would
        // orphan the flush obligation of an *earlier committing* transaction
        // whose extents still reference them (its ordered flush only writes
        // pages that are still dirty — a discarded page is silently gone, and
        // replaying that transaction would then expose whatever the device
        // holds). Flush the affected span to its final blocks first; rare and
        // bounded (shrinks only), where Linux instead orders the truncate
        // against the committing transaction (jbd2_journal_begin_ordered_truncate).
        if let Ok(page_cache) = self.page_cache() {
            let doomed_start = (new_size / BLOCK_SIZE) * BLOCK_SIZE;
            page_cache.flush_range(doomed_start..old_size)?;
        }
        // Order (report §5.2 rule 4, shrink): zero + shrink the VMO before the
        // size drops. `PageCache::resize` zeroes `[new_size, block_end)` of the
        // kept partial block (BLOCK_SIZE == PAGE_SIZE) when that block is
        // mapped, so stale tail bytes do not reappear if the file is later
        // extended; a hole tail is left untouched by `resize_page_cache`.
        self.resize_page_cache(new_size, old_size)?;
        self.extent_manager()?
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
        // Unwritten-first: the blocks this write covers were allocated as
        // unwritten (so the sub-page commit above read them back as zeros, not
        // stale recycled contents). Now that the data is in the page cache,
        // convert the range to written IN THIS TRANSACTION — the extent
        // metadata that makes the blocks readable-as-data thus commits together
        // with (never before) the ordered-data flush the caller registers. A
        // crash before this transaction commits leaves the blocks unwritten:
        // read-as-zeros, never another file's freed data.
        let start_block = (offset / BLOCK_SIZE) as Iblock;
        let end_block = end.div_ceil(BLOCK_SIZE) as Iblock;
        if let Err(err) = self
            .extent_manager()
            .and_then(|em| em.mark_range_written(start_block, end_block, handle))
        {
            self.rollback_write(old_size, end, handle);
            return Err(err);
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
                .extent_manager()
                .is_ok_and(|extent_manager| extent_manager.is_dirty())
    }

    fn clear_dirty(&mut self) {
        self.desc.clear_dirty();
        if let Ok(extent_manager) = self.extent_manager() {
            extent_manager.clear_dirty();
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
        let (root, sector_count) = match self.extent_manager() {
            Ok(bm) => (bm.root_snapshot(), bm.sector_count()),
            Err(_) => (*self.desc.raw_block(), self.desc.sector_count()),
        };
        // Mirror the authoritative `i_blocks` into the descriptor before writing.
        self.desc.set_sector_count(sector_count);
        fs.write_back_inode_desc(ino, &self.desc, &root, handle)?;
        if let Some(handle) = handle {
            // Record the transaction carrying this capture BEFORE clearing the
            // dirty flag: the flag clears now but the commit is asynchronous,
            // so `fsync` needs this tid to wait on (clean != committed).
            self.sync_tid = Some(handle.tid());
        }
        self.clear_dirty();
        Ok(())
    }

    /// Flushes dirty data pages in `[0, file_size)`.
    fn sync_data_pages(&self, fs: &Ext4) -> Result<()> {
        // Journaled DIRECTORY blocks are metadata: every edit is captured
        // into the journal (`journal_dir_block`) and reaches its final
        // location only via checkpoint. Flushing the page-cache copy directly
        // races WAL — the crash harness reconstructed a state where a
        // concurrent sync flushed a dir page carrying a still-uncommitted
        // unlink (dirent gone on disk, the inode update never committed →
        // unattached inode after replay). The journal owns directory
        // persistence end to end on journaled volumes; the page cache is a
        // read/edit cache only.
        if self.desc.type_() == InodeType::Dir && fs.journal().is_some() {
            return Ok(());
        }
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
        extent_manager: Arc<ExtentManager>,
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
    /// Builds the payload for `desc`; fails if a data-backed inode's extent
    /// root does not parse (`ExtentTree::try_new` — the parse-once boundary).
    fn new(desc: &InodeDesc, fs: Weak<Ext4>) -> Result<Self> {
        Ok(match desc.type_() {
            InodeType::File | InodeType::Dir => Self::new_data_backed(
                desc.size() as usize,
                *desc.raw_block(),
                desc.sector_count(),
                fs,
            )?,
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
                    Self::new_data_backed(size, *desc.raw_block(), desc.sector_count(), fs)?
                }
            }
            // Devices and special files are handled by later tasks.
            _ => Self::NoPayload,
        })
    }

    fn new_data_backed(
        size: usize,
        root: [u32; RAW_BLOCK_PTRS_LEN],
        sector_count: u64,
        fs: Weak<Ext4>,
    ) -> Result<Self> {
        let page_cache_size = size.align_up(PAGE_SIZE);
        let page_count = page_cache_size / PAGE_SIZE;
        let extent_manager = Arc::new(ExtentManager::try_new(root, sector_count, fs, page_count)?);
        let backend: Weak<dyn PageCacheBackend> = Arc::downgrade(&extent_manager) as _;
        let page_cache = PageCache::new_with_backend(page_cache_size, backend)
            .expect("ext4 inode page cache allocation failed");
        Ok(Self::DataBacked {
            page_cache,
            extent_manager,
        })
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
            flags: FileFlags::EXTENTS.bits(),
            extra_isize: 32,
            ..Default::default()
        }
    }

    /// An inode stamped with its crc32c `i_checksum_lo`/`i_checksum_hi` verifies;
    /// a body change, a wrong inode number, or a wrong generation each fail with
    /// `EUCLEAN`. The seed folds in the inode number and generation, so identical
    /// bytes at a different inode number do not verify.
    #[ktest]
    fn inode_checksum_round_trip() {
        const INODE_SIZE: usize = 256;
        let fs_seed = 0xFEED_BEEF;
        let ino: Ext4Ino = 12;
        let mut raw = raw_root_dir();
        raw.generation = 0x55AA;

        let crc = InodeDesc::inode_checksum(&raw, ino, fs_seed, INODE_SIZE);
        raw.checksum_lo = (crc & 0xFFFF) as u16;
        raw.checksum_hi = ((crc >> 16) & 0xFFFF) as u16;
        InodeDesc::verify_inode_checksum(&raw, ino, fs_seed, INODE_SIZE).unwrap();

        // Wrong inode number / generation are folded into the seed.
        assert_eq!(
            InodeDesc::verify_inode_checksum(&raw, ino + 1, fs_seed, INODE_SIZE)
                .unwrap_err()
                .error(),
            Errno::EUCLEAN
        );
        let mut regen = raw;
        regen.generation = 0x55AB;
        assert!(InodeDesc::verify_inode_checksum(&regen, ino, fs_seed, INODE_SIZE).is_err());

        // Corrupted body (the covered range excludes the checksum fields).
        let mut bad = raw;
        bad.size_lo += 1;
        assert!(InodeDesc::verify_inode_checksum(&bad, ino, fs_seed, INODE_SIZE).is_err());

        // Storing the checksum back does not change the covered result.
        let recheck = InodeDesc::inode_checksum(&raw, ino, fs_seed, INODE_SIZE);
        assert_eq!(recheck, crc);
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
        let bm = bm.extent_manager().unwrap();
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
        let depth = inode.inner.read().extent_manager().unwrap().root_depth();
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
            let bm = inner.extent_manager().unwrap();
            (
                bm.map_blocks(1).unwrap().mapped_pblock().unwrap(),
                bm.map_blocks(2).unwrap().mapped_pblock().unwrap(),
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
        let bm = inner.extent_manager().unwrap();
        assert_eq!(bm.root_depth(), 0);
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
        let bm = inner.extent_manager().unwrap();
        let m0 = bm.map_blocks(0).unwrap();
        let m1 = bm.map_blocks(1).unwrap();
        let m2 = bm.map_blocks(2).unwrap();
        assert_eq!(m0.state(), MapState::Unwritten);
        assert_eq!(m1.state(), MapState::Written);
        assert_eq!(m1.mapped_pblock(), Some(201));
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

    /// `decode_time` follows Linux's signed-base + epoch-extension layout:
    /// epoch 1 continues seamlessly at 2^31, and pre-1970 values (negative
    /// base, epoch 0) clamp to the epoch since `Duration` cannot express them.
    #[ktest]
    fn decode_time_signed_epoch_semantics() {
        // 0x8000_0000 as i32 = -2^31; epoch 1 adds 2^32 → exactly 2^31.
        assert_eq!(decode_time(0x8000_0000, 0b01), Duration::new(1 << 31, 0));
        // -1s (1969-12-31T23:59:59) is unrepresentable: clamps to 0.
        assert_eq!(decode_time(u32::MAX, 0), Duration::new(0, 0));
        // Plain positive seconds, nanoseconds in the upper extra bits.
        assert_eq!(decode_time(1000, 7 << 2), Duration::new(1000, 7));
    }

    /// The special-file device encoding round-trips through `i_block` in both
    /// the old (8-bit major/minor, word 0) and wide (`new_encode_dev`, word 1)
    /// layouts.
    #[ktest]
    fn device_block_encoding_roundtrip() {
        for (major, minor) in [(8, 1), (255, 255), (300, 7), (1, 70000), (4095, 1048575)] {
            let id = device_id::encode_device_numbers(major, minor);
            let block = encode_device_block(id);
            assert_eq!(
                decode_device_block(&block),
                id,
                "major={major} minor={minor}"
            );
        }
    }

    /// P5-T0: allocating writes and shrinking truncates must register the
    /// inode as ordered data of the running transaction (jbd2 `data=ordered`),
    /// so the commit pipeline flushes its pages before the commit block.
    #[ktest]
    fn journaled_write_and_shrink_register_ordered_data() {
        // The stopped commit thread keeps the running transaction inspectable:
        // nothing consumes it between the operation and the assertion.
        let f = journaled_fixture_with_empty_file();
        let journal = f.ext4.journal().unwrap();

        let inode = f.ext4.read_inode(FILE_INO).unwrap();
        assert_eq!(journal.running_nr_ordered_data_for_test(), 0);

        // An allocating write registers the inode (keyed by ino — repeats stay
        // one entry).
        write_all(&inode, 0, &[0x5au8; 2 * BLOCK_SIZE]);
        assert_eq!(journal.running_nr_ordered_data_for_test(), 1);
        write_all(&inode, 2 * BLOCK_SIZE, &[0xa5u8; BLOCK_SIZE]);
        assert_eq!(journal.running_nr_ordered_data_for_test(), 1);

        // A shrink to a non-block-aligned size re-zeroes the kept tail in the
        // page cache and must (re-)register too.
        inode.resize(BLOCK_SIZE / 2).unwrap();
        assert_eq!(journal.running_nr_ordered_data_for_test(), 1);
    }

    /// Builds a journaled fixture with an empty regular file and a stopped
    /// commit thread (keeps every state transition inspectable).
    fn journaled_fixture_with_empty_file() -> Ext4Fixture {
        clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_journal_inode(64)
            .build()
            .unwrap();
        f.write_raw_inode(FILE_INO, &make_empty_file_inode());
        f.ext4.journal().unwrap().stop_commit_thread();
        f
    }

    /// Unwritten-first (`hole-alloc-stale-exposure`): a partial-block write into
    /// a hole must not expose the freshly allocated block's stale contents —
    /// another file's freed data still on the device. The block is allocated
    /// UNWRITTEN, so the write's own sub-page page-cache read-fill returns zeros
    /// (not the recycled block), and only the written bytes land; the convert to
    /// written follows in the same transaction. Before the fix (fresh holes
    /// inserted as written) the sub-page commit read the recycled block's bytes
    /// into the uncovered range — a silent cross-file leak. Reproduces the fsx
    /// (generic/127) and posix_fallocate (generic/345) failures.
    #[ktest]
    fn partial_write_into_recycled_hole_reads_zeros_not_stale() {
        let f = journaled_fixture_with_empty_file();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        // Fill block 0 with a recognizable pattern, record its physical block,
        // then free it. `free` does not zero the device, so the pattern stays.
        write_all(&inode, 0, &[0xAB; BLOCK_SIZE]);
        let recycled = inode
            .inner
            .read()
            .extent_manager()
            .unwrap()
            .map_blocks(0)
            .unwrap()
            .mapped_pblock()
            .unwrap();
        inode.resize(0).unwrap();

        // Partial write inside block 0 at a non-zero offset: [200, 300). The
        // allocator hands block 0 back the freed physical block (first-fit),
        // now allocated unwritten.
        write_all(&inode, 200, &[0xCD; 100]);
        let reused = inode
            .inner
            .read()
            .extent_manager()
            .unwrap()
            .map_blocks(0)
            .unwrap()
            .mapped_pblock()
            .unwrap();
        // Non-vacuous: the hazard only exists when the stale block is recycled.
        assert_eq!(
            reused, recycled,
            "the freed block must be reallocated to exercise the stale-exposure hazard"
        );

        // The written bytes are the new data; the UNCOVERED head [0, 200) must
        // read zeros, NOT the 0xAB stale contents of the recycled block.
        assert_eq!(read_back(&inode, 0, 200), vec![0u8; 200]);
        assert_eq!(read_back(&inode, 200, 100), vec![0xCD; 100]);
        // The block is now written (the convert ran), so re-reads are stable.
        assert_eq!(
            inode
                .inner
                .read()
                .extent_manager()
                .unwrap()
                .map_blocks(0)
                .unwrap()
                .state(),
            MapState::Written
        );
    }

    /// A pure overwrite of already-written blocks must convert nothing: on a
    /// large/fragmented file the extent-tree rewrite that a needless
    /// `convert_unwritten` would trigger re-journals every external node and can
    /// abort a legal write. Here we pin the invariant on a small file — the
    /// mapping stays written and `i_blocks` is unchanged.
    #[ktest]
    fn overwrite_of_written_block_converts_nothing() {
        let f = journaled_fixture_with_empty_file();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();
        write_all(&inode, 0, &[0x11; BLOCK_SIZE]);
        let sc_before = inode.sector_count();

        // Overwrite the same, already-written block.
        write_all(&inode, 0, &[0x22; BLOCK_SIZE]);

        assert_eq!(inode.sector_count(), sc_before);
        assert_eq!(read_back(&inode, 0, BLOCK_SIZE), vec![0x22; BLOCK_SIZE]);
        assert_eq!(
            inode
                .inner
                .read()
                .extent_manager()
                .unwrap()
                .map_blocks(0)
                .unwrap()
                .state(),
            MapState::Written
        );
    }

    /// Shrinking a sparse file so the kept partial tail lands inside a hole
    /// must not leave a dirty page there: the tail zero-fill would dirty a
    /// page with no backing block, and journaled writeback later fails on it
    /// (the loud "writeback hit an unallocated block" EIO — as an ordered
    /// flush it aborts the journal). Reproduces xfstests generic/014
    /// (truncfile) and the generic/127 fsx poisoning.
    #[ktest]
    fn shrink_into_hole_leaves_no_dirty_tail_page() {
        let f = journaled_fixture_with_empty_file();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        // Block 0 mapped; every later block stays a hole.
        write_all(&inode, 0, &[0xEE; BLOCK_SIZE]);
        inode.resize(3 * BLOCK_SIZE).unwrap();
        // Materialize the hole page in the cache the way a reader would
        // (clean, all zeros) — the fsx shape.
        assert_eq!(
            read_back(&inode, BLOCK_SIZE, BLOCK_SIZE),
            vec![0u8; BLOCK_SIZE]
        );

        // Shrink so the kept partial tail is inside hole block 1.
        inode.resize(BLOCK_SIZE + 100).unwrap();

        let inner = inode.inner.read();
        // Writeback of the whole cache must succeed: nothing dirty may point
        // at an unallocated block.
        inner
            .page_cache()
            .unwrap()
            .flush_range(0..3 * BLOCK_SIZE)
            .unwrap();
        // The tail stayed a hole (neither the resize nor the flush allocated),
        // and the retained bytes are intact.
        let bm = inner.extent_manager().unwrap();
        assert_eq!(bm.map_blocks(1).unwrap().state(), MapState::Hole);
        drop(inner);
        assert_eq!(read_back(&inode, 0, 100), vec![0xEE; 100]);
        assert_eq!(read_back(&inode, BLOCK_SIZE, 100), vec![0u8; 100]);
    }

    /// A write leaving a gap past an unaligned hole EOF must not dirty the
    /// old-EOF boundary page: the write never allocates that block, so the
    /// grow-fill would plant a dirty page over a hole (the generic/014
    /// truncfile journal abort came through this flow).
    #[ktest]
    fn gap_write_past_unaligned_hole_eof_leaves_no_dirty_boundary_page() {
        let f = journaled_fixture_with_empty_file();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        // Block 0 mapped; an unaligned sparse EOF inside hole block 1.
        write_all(&inode, 0, &[0xEE; BLOCK_SIZE]);
        inode.resize(BLOCK_SIZE + 100).unwrap();

        // Write far past EOF: blocks 1..4 stay holes, block 4 is allocated.
        write_all(&inode, 4 * BLOCK_SIZE, &[0xBB; 100]);

        let inner = inode.inner.read();
        inner
            .page_cache()
            .unwrap()
            .flush_range(0..5 * BLOCK_SIZE)
            .unwrap();
        let bm = inner.extent_manager().unwrap();
        assert_eq!(bm.map_blocks(1).unwrap().state(), MapState::Hole);
        drop(inner);
        // The gap reads as zeros; head and tail data are intact.
        assert_eq!(read_back(&inode, BLOCK_SIZE, 100), vec![0u8; 100]);
        assert_eq!(read_back(&inode, 4 * BLOCK_SIZE, 100), vec![0xBB; 100]);
        assert_eq!(read_back(&inode, 0, 100), vec![0xEE; 100]);
    }

    /// A write that extends across an unaligned hole EOF *and allocates the
    /// boundary block* must keep the pre-allocation zero materialization: the
    /// bytes between the old EOF and the write start must read as zeros, and
    /// writeback of the now-backed page must succeed.
    #[ktest]
    fn covered_extend_write_across_unaligned_hole_eof_reads_zeros() {
        let f = journaled_fixture_with_empty_file();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        write_all(&inode, 0, &[0xEE; BLOCK_SIZE]);
        inode.resize(BLOCK_SIZE + 100).unwrap();

        // Extend within the boundary block: block 1 gets allocated.
        write_all(&inode, BLOCK_SIZE + 200, &[0xBB; 100]);

        let inner = inode.inner.read();
        inner
            .page_cache()
            .unwrap()
            .flush_range(0..2 * BLOCK_SIZE)
            .unwrap();
        drop(inner);
        // POSIX: the old tail and the gap below the write read as zeros.
        assert_eq!(read_back(&inode, BLOCK_SIZE, 200), vec![0u8; 200]);
        assert_eq!(read_back(&inode, BLOCK_SIZE + 200, 100), vec![0xBB; 100]);
    }

    /// Growing a file whose EOF sits unaligned inside a hole must not dirty
    /// the old tail page either — the grow-direction twin of the shrink fill
    /// (both directions of `PageCache::resize` zero-fill the boundary page).
    #[ktest]
    fn expand_from_unaligned_hole_eof_leaves_no_dirty_page() {
        let f = journaled_fixture_with_empty_file();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        // Block 0 mapped; an unaligned sparse EOF inside hole block 1.
        write_all(&inode, 0, &[0xEE; BLOCK_SIZE]);
        inode.resize(BLOCK_SIZE + 100).unwrap();

        // Grow across the unaligned hole EOF.
        inode.resize(3 * BLOCK_SIZE).unwrap();

        let inner = inode.inner.read();
        inner
            .page_cache()
            .unwrap()
            .flush_range(0..3 * BLOCK_SIZE)
            .unwrap();
        let bm = inner.extent_manager().unwrap();
        assert_eq!(bm.map_blocks(1).unwrap().state(), MapState::Hole);
        drop(inner);
        assert_eq!(
            read_back(&inode, BLOCK_SIZE, BLOCK_SIZE),
            vec![0u8; BLOCK_SIZE]
        );
    }
}
