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
    checksum::{self, InodeCsumSeed},
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
use crate::fs::{
    file::InodeMode,
    vfs::inode::{Extension, FallocMode},
};

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
/// The raw `i_block` bytes are retained in `block`; they are parsed into an
/// `ExtentTree` when the inode's payload is built (`InodePayload::new`).
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

    /// Sets the given inode flags. Mutates through `Dirty`; used to restore the
    /// `EXTENTS` flag when an inode switches back to extent-mapped storage.
    pub(super) fn insert_flags(&mut self, flags: FileFlags) {
        self.flags.insert(flags);
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
        // The journal inode is read-only here (no external node is ever
        // written, no block is ever freed), so no checksum seed and no
        // forget policy apply.
        let em = ExtentManager::try_new(
            *self.raw_block(),
            self.sector_count(),
            fs,
            nblocks as usize,
            None,
            journal::DataForgetPolicy::PlainData,
        )?;
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
    /// Computes the full 32-bit crc32c of `raw` over the whole `inode_size`, with
    /// both checksum fields treated as zero (Linux `ext4_inode_csum`). The caller
    /// splits it into `i_checksum_lo` (low 16 bits) and, when the inode has the
    /// extra region, `i_checksum_hi` (high 16 bits). `seed` is this inode's
    /// per-inode seed (ino and generation already folded in).
    fn inode_checksum(raw: &RawInode, seed: InodeCsumSeed, inode_size: usize) -> u32 {
        let bytes = raw.as_bytes();
        let mut crc = checksum::crc32c(seed.get(), &bytes[..I_CHECKSUM_LO_OFFSET]);
        crc = checksum::crc32c(crc, &[0u8, 0u8]); // i_checksum_lo
        if inode_size > 128 {
            // The extra region carries i_checksum_hi: checksum the gap between
            // the two fields, then the zeroed hi, then the remainder.
            crc = checksum::crc32c(crc, &bytes[I_CHECKSUM_LO_OFFSET + 2..I_CHECKSUM_HI_OFFSET]);
            crc = checksum::crc32c(crc, &[0u8, 0u8]); // i_checksum_hi
            crc = checksum::crc32c(crc, &bytes[I_CHECKSUM_HI_OFFSET + 2..inode_size]);
        } else {
            crc = checksum::crc32c(crc, &bytes[I_CHECKSUM_LO_OFFSET + 2..inode_size]);
        }
        crc
    }

    /// Verifies `raw`'s stored `i_checksum_lo` (and `i_checksum_hi` when the
    /// inode has the extra region) for a `metadata_csum` volume, at the inode
    /// read boundary. `seed` is this inode's per-inode seed.
    pub(super) fn verify_inode_checksum(
        raw: &RawInode,
        seed: InodeCsumSeed,
        inode_size: usize,
    ) -> Result<()> {
        let crc = Self::inode_checksum(raw, seed, inode_size);
        if raw.checksum_lo != (crc & 0xFFFF) as u16 {
            return_errno_with_message!(Errno::EUCLEAN, "bad inode checksum (lo)");
        }
        if inode_size > 128 && raw.checksum_hi != ((crc >> 16) & 0xFFFF) as u16 {
            return_errno_with_message!(Errno::EUCLEAN, "bad inode checksum (hi)");
        }
        Ok(())
    }

    /// Stamps `raw`'s `i_checksum_lo` (and `i_checksum_hi` when the inode has the
    /// extra region) for a `metadata_csum` volume, at an inode writeback funnel
    /// (Linux `ext4_inode_csum_set`). `seed` is this inode's per-inode seed; the
    /// caller stamps only when the feature is on.
    pub(super) fn stamp_inode_checksum(raw: &mut RawInode, seed: InodeCsumSeed, inode_size: usize) {
        let crc = Self::inode_checksum(raw, seed, inode_size);
        raw.checksum_lo = (crc & 0xFFFF) as u16;
        if inode_size > 128 {
            raw.checksum_hi = ((crc >> 16) & 0xFFFF) as u16;
        }
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

/// How much of an inode's pending metadata a sync must wait on — the caller's
/// `fsync`-vs-`fdatasync` intent named at the call site rather than a bare bool.
/// Selects which recorded transaction [`sync_data_and_meta`](Inode::sync_data_and_meta)
/// waits for (jbd2's `i_sync_tid` vs `i_datasync_tid`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SyncScope {
    /// `fsync`: wait for the full `sync_tid`, so a pending pure-attribute change
    /// (chmod/chown/utimens) is forced too.
    Full,
    /// `fdatasync`: wait only for the data-relevant `datasync_tid`, so a pending
    /// pure-attribute change is not forced (POSIX "does not flush modified
    /// metadata not needed to read the data").
    DataOnly,
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
        // With `metadata_csum`, external extent-tree nodes carry a tail checksum
        // seeded per inode (ino + generation folded into the fs seed). Compute
        // it once here and thread it into the payload's extent manager; `None`
        // when the feature is off keeps external nodes byte-identical.
        let csum_seed = fs.upgrade().and_then(|f| {
            let sb = f.super_block();
            sb.has_metadata_csum()
                .then(|| sb.metadata_csum_seed().derive_inode(ino, desc.generation()))
        });
        let payload = InodePayload::new(&desc, fs.clone(), csum_seed)?;
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
                csum_seed,
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

    /// Builds a live inode (link count > 0) for a crash-recovery *re-truncate*
    /// from its raw on-disk inode — a truncate interrupted mid-flight, whose
    /// deletion was never intended. Unlike [`from_raw_for_recovery`](Self::from_raw_for_recovery)
    /// it keeps the true (nonzero) link count, so `Drop` never reclaims it; the
    /// caller drives [`retruncate_to`](Self::retruncate_to) to finish freeing the
    /// tail and unlist. The on-disk `i_dtime` holds the orphan-chain successor
    /// (decoded here as a bogus timestamp); `retruncate_to` resets it to live.
    pub(super) fn from_raw_live(
        ino: Ext4Ino,
        raw: &RawInode,
        block_group_idx: usize,
        fs: Weak<Ext4>,
    ) -> Result<Arc<Self>> {
        let desc = InodeDesc::try_from(raw)?;
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

    /// The transaction `fsync` waits on (the full `sync_tid`), for tests.
    #[cfg(ktest)]
    pub(super) fn recorded_sync_tid_for_test(&self) -> Option<Tid> {
        self.inner.read().sync_tid
    }

    /// The transaction `fdatasync` waits on (the `datasync_tid` subset), for
    /// tests. Distinct from `sync_tid` after a pure-attribute change.
    #[cfg(ktest)]
    pub(super) fn recorded_datasync_tid_for_test(&self) -> Option<Tid> {
        self.inner.read().datasync_tid
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
    ///
    /// A large journaled **append** (`offset >= old_size`) is split into
    /// credit-bounded chunks, each committed in its own transaction across a
    /// [`journal_restart`](journal) boundary, so a write that maps more metadata
    /// than one transaction holds (a fragmented run, a deep tree) never overflows
    /// — jbd2's `ext4_alloc_file_blocks` chunk loop shape. Other writes (overwrites
    /// and sparse fills within the existing file, and non-journaled volumes) take
    /// the single-transaction path, whose `charge_fresh_capture` `ENOSPC` backstop
    /// is unchanged; chunking them needs a re-windowable data source the page-cache
    /// write API does not offer for an in-place bounded read (a P9 refinement,
    /// ledger: `within-file-write-restart`).
    pub(super) fn write_at(&self, offset: usize, reader: &mut VmReader) -> Result<usize> {
        if self.type_ == InodeType::Dir {
            return_errno!(Errno::EISDIR);
        }
        if reader.remain() == 0 {
            return Ok(0);
        }
        let write_len = reader.remain();
        let end = offset
            .checked_add(write_len)
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "write range overflow"))?;
        let fs = self.fs()?;
        let mut inner = self.inner.write();
        // Journal handle after the inner lock (inner ① → handle ②): captures the
        // block-bitmap / group-descriptor / extent after-images this write's
        // allocations dirty. Dropped at return, closing the handle. The estimate
        // is the per-chunk cost at the file's live extent depth; a chunk that
        // fragments into several extents grows the reservation via journal_extend,
        // and a whole chunk that fills grows the reservation until a restart
        // rejoins a fresh transaction (see `Ext4::write_credits`).
        let depth = inner
            .extent_manager()
            .map(|em| em.root_depth())
            .unwrap_or(0);
        let mut op = fs.begin_op(fs.write_credits(depth))?;

        // A journaled append restarts across chunks; every other write is one
        // transaction (see the function docs). The append is where an unbounded
        // sequential write (log files, `cp`/`dd`) fragments a deep tree past one
        // transaction, and its per-chunk data write is bounded by growing the
        // page cache to the chunk end — the bound the in-place overwrite lacks.
        if op.get().is_some() && offset >= inner.file_size() {
            return self.write_append_chunked(&fs, offset, end, reader, &mut op, &mut inner);
        }

        // `get_mut`: the write spine holds the handle by `&mut` so the
        // `journal_restart` seam is reachable; the per-handle descriptor capture
        // below reverts to the shared `get`.
        let len = inner.write_at(&fs, offset, reader, op.get_mut())?;
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
            // Data-relevant (this write may map blocks and/or grow `i_size`):
            // `fdatasync` must commit this capture to retrieve the data.
            inner.stamp_datasync_tid(op.get());
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

    /// The credit-bounded append loop (jbd2/ext4 `ext4_alloc_file_blocks`): map,
    /// write, and convert one credit-bounded chunk per transaction, restarting
    /// onto a fresh transaction at each boundary, until `[offset, end)` is
    /// written. Assumes `offset >= old_size` (a pure append), so each chunk's
    /// page-cache write is bounded by growing the page cache to the chunk end,
    /// and a live journal handle (`op.get().is_some()`).
    ///
    /// Crash safety (report §5.1-5.2): each chunk's alloc(bitmap)+extent+inode
    /// descriptor ride ONE transaction (`write_back_inode_desc` here), and the
    /// restart boundary falls AFTER that writeback — never between a bitmap set
    /// and the extent/inode capture — so a crash leaves chunks `0..K` committed
    /// (a valid short write) and the rest untouched, with no leaked blocks and,
    /// by Unwritten-first, no stale data.
    fn write_append_chunked(
        &self,
        fs: &Ext4,
        offset: usize,
        end: usize,
        reader: &mut VmReader,
        op: &mut journal::OpHandle,
        inner: &mut InodeInner,
    ) -> Result<usize> {
        let write_len = reader.remain();
        let start_block = Iblock::try_from(offset / BLOCK_SIZE)
            .map_err(|_| Error::with_message(Errno::EFBIG, "block index exceeds 32 bits"))?;
        let end_block = Iblock::try_from(end.div_ceil(BLOCK_SIZE))
            .map_err(|_| Error::with_message(Errno::EFBIG, "block index exceeds 32 bits"))?;

        // The per-transaction credit ceiling: a single insert needing more than
        // this can never fit any transaction — the EFBIG floor. Present on the
        // journaled volume this path requires (`op.get().is_some()`).
        let max_credits = fs.journal().map(|j| j.max_credits());

        let mut cursor = start_block;
        // Bytes durably written so far (committed chunks only): the short-write
        // count a later chunk error reports, distinct from reader consumption —
        // a failed chunk is rolled back but may have advanced the reader.
        let mut written = 0usize;
        while cursor < end_block {
            let depth = inner
                .extent_manager()
                .map(|em| em.root_depth())
                .unwrap_or(0);
            let need = fs.write_credits(depth);
            if let Some(handle) = op.get_mut() {
                journal::ensure_chunk_credits(handle, need)?;
            }

            let outcome = match inner.write_bounded_chunk(fs, offset, end, cursor, op.get(), reader)
            {
                Ok(outcome) => outcome,
                // A chunk error after earlier chunks committed durably (each
                // advanced on-disk `i_size` in its own transaction) is a POSIX
                // short write, not a failed write — report the bytes that
                // landed. Only a failure before ANY chunk committed propagates
                // the raw error.
                Err(err) => {
                    if written > 0 {
                        return Ok(written);
                    }
                    return Err(err);
                }
            };

            let reached = match outcome {
                ChunkWrite::Wrote(reached) => reached,
                ChunkWrite::Stalled { need: need2 } => {
                    // One insert needs `need2` credits the current transaction
                    // cannot grant. If `need2` exceeds a whole transaction's
                    // capacity, no restart can ever fit it — the honest EFBIG
                    // floor (P9's in-place B-tree surgery lifts it); otherwise
                    // restart onto a fresh transaction reserving exactly `need2`,
                    // whose first insert then fits, and retry the chunk (no
                    // cursor advance).
                    if max_credits.is_some_and(|max| need2 > max) {
                        if written > 0 {
                            return Ok(written);
                        }
                        return_errno_with_message!(
                            Errno::EFBIG,
                            "one extent insert's whole-tree reserialize exceeds a journal transaction"
                        );
                    }
                    if let Some(handle) = op.get_mut() {
                        journal::journal_restart(handle, need2)?;
                    }
                    continue;
                }
            };

            // This chunk's inode descriptor (size, i_blocks, extent root) must
            // ride the SAME transaction as its bitmap/GDT/extent captures.
            if op.get().is_some() {
                inner.write_back_inode_desc(fs, self.ino, op.get())?;
            }
            // data=ordered per chunk: flush this chunk's pages (up to the newly
            // published size) before its commit block.
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
            cursor = reached;
            // Committed bytes: the write start to this chunk's block-aligned end,
            // clamped to the write end. The final chunk lands `write_len`.
            written = (reached as usize * BLOCK_SIZE).min(end) - offset;
        }
        // Data-relevant: `op` holds the final chunk's transaction (each restart
        // rejoins `op` onto a fresh tid), which carries this append's newest
        // extent/`i_size` capture — the tid `fdatasync` must commit.
        inner.stamp_datasync_tid(op.get());
        Ok(write_len)
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
        let old_size = inner.file_size();

        // Fast/slow gate (DECISION §E, G-3): a shrink whose worst-case
        // whole-truncate estimate overruns one journal transaction takes the
        // chunked, orphan-protected spine — a crash mid-truncate then leaves the
        // inode on the orphan list with `i_size = new_size`, and recovery
        // re-truncates to that size. The gate never predicts single-txn for a
        // genuinely multi-txn truncate (it upper-bounds the free), so the fast
        // path is correctness-equivalent to Linux's always-orphan while adding
        // NO orphan_add/del and NO restart to the common case (grow, no-op, and
        // a bounded shrink).
        let chunked = if new_size < old_size {
            match (
                fs.journal().map(|j| j.max_credits()),
                inner.extent_manager(),
            ) {
                (Some(max), Ok(em)) => {
                    let plan = em.plan_shrink(new_size, max)?;
                    let chunked = plan.whole_estimate > max;
                    // A genuinely un-splittable shrink (the chunked route's EFBIG
                    // floor) is rejected HERE, before `shrink_restartable` runs
                    // `prepare_shrink` (which lowers `i_size` and re-zeros the
                    // partial block) or `orphan_add`, so the in-memory inode is
                    // left exactly as it was — the honest "nothing changed"
                    // contract, symmetric to the write path's EFBIG.
                    if chunked && plan.floor_efbig {
                        return_errno_with_message!(
                            Errno::EFBIG,
                            "the extent tree cannot be shrunk within one journal transaction"
                        );
                    }
                    chunked
                }
                _ => false,
            }
        } else {
            false
        };
        if chunked {
            return self.shrink_restartable(&fs, &mut inner, old_size, new_size);
        }

        // Journal handle after the inner lock (inner ① → handle ②): captures the
        // block-bitmap / group-descriptor / extent after-images a shrink frees.
        // Per-chunk estimate at the file's live extent depth; a shrink freeing
        // across a few groups grows in place via `charge_fresh_capture` (the
        // gate proved the whole truncate fits one transaction).
        let depth = inner
            .extent_manager()
            .map(|em| em.root_depth())
            .unwrap_or(0);
        let mut op = fs.begin_op(fs.truncate_credits(depth))?;
        // `get_mut`: the resize spine holds the handle by `&mut` so the
        // single-transaction shrink can grow its reservation in place; the
        // descriptor capture below reverts to the shared `get`.
        inner.resize(&fs, new_size, op.get_mut())?;
        // Same per-handle descriptor capture as `write_at`: the new size and
        // truncated extent root must commit with the bitmap/GDT changes.
        if op.get().is_some() {
            inner.write_back_inode_desc(&fs, self.ino, op.get())?;
            // Data-relevant (`i_size` and the extent tree changed): `fdatasync`
            // must commit the new size to read the file's data correctly.
            inner.stamp_datasync_tid(op.get());
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

    /// The chunked, orphan-protected shrink spine — the multi-transaction
    /// truncate (DECISION §A/§C). Up front, ONCE: flush the doomed tail pages,
    /// zero+shrink the page cache, and publish `i_size = new_size` (the KEY
    /// INSIGHT — the target IS `i_size`, published now and never advanced, so
    /// recovery reads it and re-truncates to it). Then, in the first
    /// transaction, link the inode onto the orphan list and persist that size;
    /// loop freeing tail extents one credit-bounded chunk per transaction (a
    /// `journal_restart` at each boundary, ③ released) until the tree references
    /// only `[0, keep_blocks)`; in the LAST chunk unlink from the orphan list.
    ///
    /// Crash safety (report §5.1-5.2, DECISION §F): each chunk commits `i_size =
    /// new_size`, a tree referencing `[0, reached)`, bitmaps freed for `[reached,
    /// old)`, and matching `i_blocks` — atomically, with the inode listed. A
    /// crash after chunk k leaves exactly that; recovery re-truncates to the
    /// persisted `i_size` and unlists. No leak (freed ⟺ dropped-from-tree, one
    /// txn), no double-free (a re-truncate frees only `[keep_blocks, reached)`,
    /// the already-freed extents gone from the tree), idempotent recovery (a
    /// crash mid-recovery re-scans the shorter frontier and resumes).
    fn shrink_restartable(
        &self,
        fs: &Ext4,
        inner: &mut InodeInner,
        old_size: usize,
        new_size: usize,
    ) -> Result<()> {
        // Up front, ONCE (never re-run across restarts).
        inner.prepare_shrink(new_size, old_size)?;

        let keep_blocks = Iblock::try_from(new_size.div_ceil(BLOCK_SIZE))
            .map_err(|_| Error::with_message(Errno::EFBIG, "block index exceeds 32 bits"))?;
        let em = inner.extent_manager()?.clone();

        // First transaction: link onto the orphan list and persist `i_size =
        // new_size`, its on-disk `i_dtime` stamped to the chain successor by the
        // existing override in `write_back_inode_desc` (the inode stays live —
        // link count untouched — and that override is link-count-agnostic).
        // DECISION G-2: do NOT `persist_as_orphan` — its pointer written into the
        // cached descriptor's `i_dtime` would go stale after `orphan_del`; the
        // descriptor's `dtime` stays live (0), the chain override supplies the
        // on-disk successor while listed.
        let mut op = fs.begin_op(fs.truncate_credits(em.root_depth()))?;
        // The `#[must_use]` link is deliberately dropped, not fed to
        // `persist_as_orphan` (see G-2 above): the chain override already
        // stamps `i_dtime` on every writeback while the inode is listed.
        let _orphan_link = fs.orphan_add(self.ino, op.get())?;
        inner.write_back_inode_desc(fs, self.ino, op.get())?;
        // data=ordered: the kept partial block's re-zeroing (in `prepare_shrink`)
        // must reach disk before this first transaction — which already
        // publishes `i_size = new_size` — commits, or a later sparse extend over
        // the tail could expose pre-truncate bytes after a replay. Registered
        // once, in the first transaction; later chunks free blocks only.
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

        loop {
            // The per-transaction ceiling — the honest EFBIG floor inside
            // `truncate_chunk`. Present on the journaled volume this path
            // requires (the fast/slow gate only routes here with a journal).
            let Some(max) = fs.journal().map(|j| j.max_credits()) else {
                em.truncate_to_byte_len(new_size, op.get())?;
                fs.orphan_del(self.ino, op.get())?;
                inner.write_back_inode_desc(fs, self.ino, op.get())?;
                break;
            };
            let chunk = em.truncate_chunk(new_size, op.get(), max)?;
            if chunk.reached <= keep_blocks {
                // Last chunk: unlink from the orphan list BEFORE the final
                // writeback, so that writeback (dirtied by this chunk's
                // reserialize) sees the inode off the chain and persists
                // `i_dtime = 0` — a live truncated file's deletion time. The
                // cached descriptor's `dtime` was never repointed (G-2), so it
                // is still live.
                fs.orphan_del(self.ino, op.get())?;
                inner.write_back_inode_desc(fs, self.ino, op.get())?;
                debug_assert!(inner.desc_dtime_is_live());
                break;
            }
            // Not done: persist this chunk's frontier + `i_blocks` (SAME txn as
            // its frees + reserialize), then restart onto a fresh transaction —
            // `truncate_chunk` has returned, so the ExtentTree lock ③ is dropped
            // (iron law 1: the restart's re-admission may wait, legal only under
            // the inode lock alone).
            inner.write_back_inode_desc(fs, self.ino, op.get())?;
            if let Some(handle) = op.get_mut() {
                journal::journal_restart(handle, chunk.next_bound)?;
            }
        }
        // Data-relevant: the loop published `i_size = new_size` and rewrote the
        // extent tree; `op` holds the final chunk's transaction. `fdatasync`
        // must commit it to read the truncated file at its new size.
        inner.stamp_datasync_tid(op.get());
        Ok(())
    }

    /// Finishes a crash-interrupted truncate on a LIVE (link-count > 0) orphan
    /// found at mount time: re-truncates the extent tree to the persisted
    /// `i_size` (`target`) — the original truncate published that size and never
    /// advanced it, so the survivor frontier is exactly where to resume — then
    /// unlinks from the orphan list (DECISION §D). Reuses the chunked spine; the
    /// inode is ALREADY listed (the in-memory chain was primed by
    /// `recover_orphan_list`), so it never re-adds. Purely an extent-tree
    /// operation — the page cache is untouched (the partial-block zeroing already
    /// reached disk in the crashed truncate's first, committed transaction).
    pub(super) fn retruncate_to(&self, target: usize) -> Result<()> {
        let fs = self.fs()?;
        let mut inner = self.inner.write();
        // A live inode's `i_dtime` is 0; the crashed truncate left the on-disk
        // field holding the chain successor, which `InodeDesc::try_from` decoded
        // as a bogus timestamp. Reset it now: while listed the chain override
        // re-stamps the successor on each writeback, and the final writeback
        // (after `orphan_del`) persists this live 0.
        inner.set_dtime(Duration::ZERO);

        let keep_blocks = Iblock::try_from(target.div_ceil(BLOCK_SIZE))
            .map_err(|_| Error::with_message(Errno::EFBIG, "block index exceeds 32 bits"))?;
        let em = inner.extent_manager()?.clone();
        let mut op = fs.begin_op(fs.truncate_credits(em.root_depth()))?;
        loop {
            let Some(max) = fs.journal().map(|j| j.max_credits()) else {
                em.truncate_to_byte_len(target, op.get())?;
                fs.orphan_del(self.ino, op.get())?;
                inner.write_back_inode_desc(&fs, self.ino, op.get())?;
                break;
            };
            let chunk = em.truncate_chunk(target, op.get(), max)?;
            if chunk.reached <= keep_blocks {
                // Unlink BEFORE the writeback so it (dirtied by this chunk's
                // reserialize) persists the live `i_dtime = 0` rather than the
                // stale successor pointer the crash left on disk.
                fs.orphan_del(self.ino, op.get())?;
                inner.write_back_inode_desc(&fs, self.ino, op.get())?;
                debug_assert!(inner.desc_dtime_is_live());
                break;
            }
            inner.write_back_inode_desc(&fs, self.ino, op.get())?;
            if let Some(handle) = op.get_mut() {
                journal::journal_restart(handle, chunk.next_bound)?;
            }
        }
        // Data-relevant (extent tree resumed to the persisted `i_size`): the
        // recovered truncate is a data-relevant change like the live one.
        inner.stamp_datasync_tid(op.get());
        Ok(())
    }

    /// Preallocates or punches disk space over `[offset, offset + len)`
    /// (`fallocate(2)`), dispatching on `mode`.
    ///
    /// Supported (the xfstests punch group this task targets):
    /// - [`Allocate`](FallocMode::Allocate) / [`AllocateKeepSize`](FallocMode::AllocateKeepSize):
    ///   reserve UNWRITTEN blocks over the range (the blocks read zero until a
    ///   real write converts them), extending `i_size` only for `Allocate`.
    /// - [`PunchHoleKeepSize`](FallocMode::PunchHoleKeepSize): free the mapped
    ///   blocks in the range, leaving a hole (`i_size` unchanged).
    ///
    /// Deferred beyond P7 (ledger `a2-fallocate`): `ZeroRange`/`ZeroRangeKeepSize`
    /// and the extent-shifting `CollapseRange`/`InsertRange`/`AllocateUnshareRange`
    /// all return `EOPNOTSUPP`. `fallocate` is a regular-file operation; a
    /// directory or special inode is rejected the same way (Linux ext4 gates on
    /// `S_ISREG`).
    pub(super) fn fallocate(&self, mode: FallocMode, offset: usize, len: usize) -> Result<()> {
        if self.type_ != InodeType::File {
            return_errno_with_message!(
                Errno::EOPNOTSUPP,
                "fallocate is only supported on regular files"
            );
        }
        if len == 0 {
            return Ok(());
        }
        match mode {
            FallocMode::Allocate => self.preallocate(offset, len, true),
            FallocMode::AllocateKeepSize => self.preallocate(offset, len, false),
            FallocMode::PunchHoleKeepSize => self.punch_hole(offset, len),
            FallocMode::ZeroRange
            | FallocMode::ZeroRangeKeepSize
            | FallocMode::CollapseRange
            | FallocMode::InsertRange
            | FallocMode::AllocateUnshareRange => {
                return_errno_with_message!(Errno::EOPNOTSUPP, "unsupported fallocate mode")
            }
        }
    }

    /// Reserves UNWRITTEN blocks over `[offset, offset + len)`: any hole in the
    /// range is allocated as an unwritten extent (reads zero until a real write
    /// converts it — the Unwritten-first protocol), pre-existing written and
    /// unwritten extents are left as-is. `grow_size` extends `i_size` to the range
    /// end (`Allocate`); otherwise `i_size` is unchanged and the blocks are
    /// reserved past EOF (`KEEP_SIZE`).
    ///
    /// Chunked + restarted like the append write path: a large preallocation maps
    /// more metadata than one transaction holds (a fragmented run, a deep tree),
    /// so it splits into credit-bounded chunks, each committing its extent-tree
    /// changes + `i_blocks` in one transaction across a `journal_restart`
    /// boundary. For `Allocate`, `i_size` is advanced ONCE at the end on success
    /// (committed with the final descriptor writeback), not per chunk: a failed
    /// allocation must leave `i_size` at its original value. Crash safety mirrors
    /// a sparse extend: a crash mid-preallocation leaves some blocks unwritten and
    /// the rest holes — both read zero, and `i_size` is still the pre-op value or
    /// the final one, never a torn intermediate — so no orphan protection is
    /// needed.
    fn preallocate(&self, offset: usize, len: usize, grow_size: bool) -> Result<()> {
        let end = offset
            .checked_add(len)
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "fallocate range overflow"))?;
        let fs = self.fs()?;
        let mut inner = self.inner.write();
        let old_size = inner.file_size();
        inner.ensure_size_within_limit(&fs, end)?;

        let start_block = Iblock::try_from(offset / BLOCK_SIZE)
            .map_err(|_| Error::with_message(Errno::EFBIG, "block index exceeds 32 bits"))?;
        let end_block = Iblock::try_from(end.div_ceil(BLOCK_SIZE))
            .map_err(|_| Error::with_message(Errno::EFBIG, "block index exceeds 32 bits"))?;

        // `Allocate` grows the page cache sparsely FIRST — while the range is
        // still holes the grow-resize skips the boundary zero-fill (which over a
        // hole plants a backing-block-less dirty page), and the blocks about to be
        // allocated are UNWRITTEN, so a read of the extended range returns zeros
        // either way. `i_size` is NOT advanced here: the on-disk/reported size may
        // only move on SUCCESS (Linux extends it at the end of
        // `ext4_alloc_file_blocks`), so an allocation failure below (`ENOSPC` /
        // `EFBIG`) leaves it at `old_size`. The grown page cache past `old_size`
        // is harmless residue — `read_at` clamps reads to `i_size`, and no page
        // there was dirtied.
        if grow_size && end > old_size {
            inner.resize_page_cache(end, old_size)?;
        }

        let depth = inner
            .extent_manager()
            .map(|em| em.root_depth())
            .unwrap_or(0);
        let mut op = fs.begin_op(fs.write_credits(depth))?;
        self.preallocate_chunked(&fs, &mut inner, start_block, end_block, &mut op)?;
        // Allocation succeeded: NOW advance `i_size` to the range end (`Allocate`),
        // committed in the same transaction as the final descriptor writeback
        // below. Never reached on the failure path above, so a failed `Allocate`
        // reports `old_size` unchanged.
        if grow_size && end > old_size {
            inner.set_file_size(end);
        }
        // A pure metadata operation: `fallocate` bumps ctime/mtime like Linux.
        // The final descriptor capture also covers the sub-block and non-journaled
        // paths where the chunk loop did no writeback.
        inner.set_mtime_ctime(super::utils::now());
        inner.write_back_inode_desc(&fs, self.ino, op.get())?;
        // Data-relevant (blocks mapped and, for `Allocate`, `i_size` grew):
        // `fdatasync` must commit this to observe the reservation.
        inner.stamp_datasync_tid(op.get());
        Ok(())
    }

    /// The credit-bounded preallocation loop (jbd2/ext4 `ext4_alloc_file_blocks`):
    /// allocate one credit-bounded chunk of UNWRITTEN blocks per transaction,
    /// restarting onto a fresh transaction at each boundary, until `[start_block,
    /// end_block)` is fully mapped. On a non-journaled volume `ensure_allocated_chunk`
    /// never early-stops, so this runs once.
    fn preallocate_chunked(
        &self,
        fs: &Ext4,
        inner: &mut InodeInner,
        start_block: Iblock,
        end_block: Iblock,
        op: &mut journal::OpHandle,
    ) -> Result<()> {
        if start_block >= end_block {
            return Ok(());
        }
        let em = inner.extent_manager()?.clone();
        let max_credits = fs.journal().map(|j| j.max_credits());
        let mut cursor = start_block;
        while cursor < end_block {
            let need = fs.write_credits(em.root_depth());
            if let Some(handle) = op.get_mut() {
                journal::ensure_chunk_credits(handle, need)?;
            }
            match em.ensure_allocated_chunk(cursor, end_block, op.get())? {
                extent_manager::HoleFill::Filled => {
                    // The rest of the range is mapped; persist this chunk's tree +
                    // `i_blocks` (+ the already-published `i_size`) and finish.
                    inner.write_back_inode_desc(fs, self.ino, op.get())?;
                    cursor = end_block;
                }
                extent_manager::HoleFill::Stopped { reached, need } => {
                    if reached > cursor {
                        // Progress: `[cursor, reached)` is now mapped. Persist it,
                        // then restart onto a fresh transaction — never under the
                        // ExtentTree lock (iron law 1: it is released because
                        // `ensure_allocated_chunk` returned).
                        inner.write_back_inode_desc(fs, self.ino, op.get())?;
                        cursor = reached;
                        if let Some(handle) = op.get_mut() {
                            journal::journal_restart(handle, need)?;
                        }
                    } else {
                        // Zero progress: one insert needs more than this transaction
                        // can grant. Above a whole transaction no restart ever fits
                        // it — the EFBIG floor; otherwise restart reserving exactly
                        // `need` and retry the same chunk.
                        if max_credits.is_some_and(|max| need > max) {
                            return_errno_with_message!(
                                Errno::EFBIG,
                                "one fallocate extent insert's reserialize exceeds a journal transaction"
                            );
                        }
                        if let Some(handle) = op.get_mut() {
                            journal::journal_restart(handle, need)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Frees the mapped blocks in `[offset, offset + len)`, leaving a hole
    /// (`fallocate(2)` `FALLOC_FL_PUNCH_HOLE`, always with `KEEP_SIZE`): reads of
    /// the range return zeros, the surrounding data is intact, and `i_size` is
    /// unchanged. Mirrors Linux `ext4_punch_hole`.
    ///
    /// Partial-block edges are ZEROED in the page cache, not freed: a block only
    /// partially covered by the range is kept (its uncovered bytes survive), so
    /// the covered sub-range is zeroed through the page cache and only the
    /// fully-covered blocks are freed via the truncate free machinery (per-block
    /// forget/revoke + pin, chunked + restarted so a large punch spans
    /// transactions).
    ///
    /// Crash safety (red-line ①): a crash mid-punch leaves some blocks freed and
    /// some not — a partial hole, a VALID file state, because the size never
    /// changes (no orphan protection needed, unlike truncate). The freed blocks
    /// still go through revoke (so a replay does not resurrect stale data into a
    /// reused block) and pin (so a freed block is not reallocated before its
    /// freeing transaction commits) — the SAME funnels as the truncate free path.
    fn punch_hole(&self, offset: usize, len: usize) -> Result<()> {
        let fs = self.fs()?;
        let mut inner = self.inner.write();
        let old_size = inner.file_size();
        // No hole beyond i_size (KEEP_SIZE never grows the file) — that range
        // already reads zero (Linux ext4_punch_hole).
        if offset >= old_size {
            return Ok(());
        }
        // Clamp to i_size: a punch is always KEEP_SIZE, so blocks past EOF stay
        // holes rather than being reserved.
        let end = offset
            .checked_add(len)
            .map(|e| e.min(old_size))
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "punch range overflow"))?;
        if end <= offset {
            return Ok(());
        }

        // Block-aligned fully-covered range `[aligned_start, aligned_end)`; the
        // partial edges `[offset, aligned_start)` and `[aligned_end, end)` fall in
        // KEPT blocks that are zeroed, not freed.
        let aligned_start = offset.align_up(BLOCK_SIZE);
        let aligned_end = (end / BLOCK_SIZE) * BLOCK_SIZE;

        // Zero the partial edges in the page cache (Linux ext4_zero_partial_blocks).
        // The single-block fast path is guarded by SAME-BLOCK (`offset` and the
        // last punched byte `end - 1` in one block), NOT `aligned_start >=
        // aligned_end`: the latter also holds for two ADJACENT partial blocks with
        // no full block between (e.g. `offset = BLOCK_SIZE + 100`, `end = 2 *
        // BLOCK_SIZE + 100`), where one merged `zero_partial_block(offset, end)`
        // would span TWO blocks yet check only the first block's mapping — leaving
        // the tail unzeroed when the head is a hole (stale data), or dirtying a
        // page over a hole when the tail is a hole (a journaled-writeback abort).
        // Each partial is zeroed against ITS OWN block's mapping.
        if offset / BLOCK_SIZE == (end - 1) / BLOCK_SIZE {
            // `offset` and `end - 1` share one block. Zero `[offset, end)` only
            // when that block is partially covered; a fully-covered single block
            // (`offset` and `end` both block-aligned) is freed below, not zeroed.
            if offset < aligned_start || aligned_end < end {
                inner.zero_partial_block(offset, end)?;
            }
        } else {
            // `offset` and `end - 1` lie in DIFFERENT blocks. Zero the head-partial
            // (`[offset, aligned_start)`, within `offset`'s block) and the
            // tail-partial (`[aligned_end, end)`, within `end - 1`'s block) as
            // SEPARATE single-block calls — whether or not a full block sits
            // between them — each checking its own block's mapping.
            if offset < aligned_start {
                inner.zero_partial_block(offset, aligned_start)?;
            }
            if aligned_end < end {
                inner.zero_partial_block(aligned_end, end)?;
            }
        }

        let has_full_blocks = aligned_start < aligned_end;
        let (first_block, stop_block) = if has_full_blocks {
            (
                Iblock::try_from(aligned_start / BLOCK_SIZE).map_err(|_| {
                    Error::with_message(Errno::EFBIG, "block index exceeds 32 bits")
                })?,
                Iblock::try_from(aligned_end / BLOCK_SIZE).map_err(|_| {
                    Error::with_message(Errno::EFBIG, "block index exceeds 32 bits")
                })?,
            )
        } else {
            (0, 0)
        };

        // Flush then evict the fully-covered page range so post-punch reads see
        // the new hole as zeros (Linux truncate_pagecache_range); flush-first
        // keeps an earlier committing transaction's ordered obligation from being
        // orphaned (the `prepare_shrink` invariant).
        if has_full_blocks && let Ok(pages) = inner.page_cache() {
            pages.invalidate_range(aligned_start..aligned_end)?;
        }

        inner.set_mtime_ctime(super::utils::now());

        let em = inner.extent_manager()?.clone();
        let mut op = fs.begin_op(fs.truncate_credits(em.root_depth()))?;
        // data=ordered: the partial-edge zeroing (kept blocks) must reach disk
        // before this operation's freeing transaction commits, or a crash could
        // leave an edge block holding its pre-punch bytes. Registered once.
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

        if has_full_blocks {
            loop {
                // The per-transaction ceiling — the EFBIG floor inside
                // `punch_chunk`. Present on the journaled volume this loop needs.
                let Some(max) = fs.journal().map(|j| j.max_credits()) else {
                    em.punch_range(first_block, stop_block, op.get())?;
                    break;
                };
                let chunk = em.punch_chunk(first_block, stop_block, op.get(), max)?;
                // This chunk's frees + reserialize + `i_blocks` ride ONE transaction.
                inner.write_back_inode_desc(&fs, self.ino, op.get())?;
                if !chunk.more {
                    break;
                }
                // Not done: restart onto a fresh transaction — `punch_chunk` has
                // returned, so the ExtentTree lock is dropped (iron law 1).
                if let Some(handle) = op.get_mut() {
                    journal::journal_restart(handle, chunk.next_bound)?;
                }
            }
        }
        // Journal the ctime/mtime bump (and, on the non-journaled path, the freed
        // tree); idempotent after the loop's last per-chunk writeback.
        inner.write_back_inode_desc(&fs, self.ino, op.get())?;
        // Data-relevant (the extent tree changed): `fdatasync` must commit it to
        // observe the hole.
        inner.stamp_datasync_tid(op.get());
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
    ///
    /// [`SyncScope::DataOnly`] narrows the metadata wait to `fdatasync`
    /// semantics: wait only for the last **data-relevant** capture
    /// (`datasync_tid` — extent/`i_size`), not the full `sync_tid`. A
    /// chmod/chown/utimens bumps `sync_tid` but not `datasync_tid`, so an
    /// `fdatasync` after one does NOT force its commit — POSIX's "does not flush
    /// modified metadata not needed to read the data."
    pub(super) fn sync_data_and_meta(&self, scope: SyncScope) -> Result<()> {
        let fs = self.fs()?;
        let wait_tid = {
            let mut inner = self.inner.write();
            inner.sync_data_pages(&fs)?;
            // Journaled: capture instead of direct-writing (see
            // `sync_metadata`). Wait below on whichever transaction carries
            // this inode's newest capture: the one this writeback just made
            // (dirty inode), else the recorded tid of an earlier journaled
            // capture — `datasync_tid` for `fdatasync`, the full `sync_tid` for
            // `fsync`. The dirty flag clears at capture time while the commit is
            // asynchronous, so a "clean" inode may still sit in an uncommitted
            // transaction. A clean inode with no recorded tid has nothing
            // pending, and its fresh op captured nothing — a transaction with no
            // captured blocks never becomes committable, so waiting on it would
            // sleep forever. A dirty inode's fresh capture is waited on in full
            // by both (conservative: `fdatasync` may over-wait if the dirty
            // change was attribute-only, never under-wait).
            let recorded = match scope {
                SyncScope::DataOnly => inner.datasync_tid,
                SyncScope::Full => inner.sync_tid,
            };
            let was_dirty = inner.is_dirty();
            let op = fs.begin_op(Ext4::FSYNC_CREDITS)?;
            inner.write_back_inode_desc(&fs, self.ino, op.get())?;
            if was_dirty { op.tid() } else { recorded }
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
    /// [`sync_metadata`](Self::sync_metadata)); the capture alone is not
    /// durable yet — log durability lives at the `FileSystem::sync` boundary,
    /// which waits on the commit via `commit_and_wait_running` before issuing
    /// the single barrier (wired in P5), not per inode.
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

    /// Reclaims a fully unlinked inode: frees its data blocks and inode bit,
    /// chunked and restartable so a large file's reclaim never overruns one
    /// transaction (DECISION §D).
    ///
    /// Runs from `Drop` when the last `Arc<Inode>` is released. A no-op (returns
    /// `Ok(false)`) unless the inode's link count is 0 *and* its bitmap bit is
    /// still allocated — the latter guards against double-freeing an inode an
    /// earlier reclaim already released. It keeps the inode on the orphan list
    /// across the whole multi-transaction free and splices it off + frees the
    /// inode bit in the LAST chunk's transaction, so a crash mid-free re-scans
    /// (the inode still listed and allocated) and resumes, and a crash after
    /// leaves it fully freed and off the chain. Mirrors ext2
    /// `try_reclaim_deleted_inode` (plus ext4's orphan splice), minus the
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
        // inode's blocks and the inode itself dirty. Per-chunk estimate at the
        // file's live extent depth; freeing a large file's blocks across many
        // groups chunks across transactions (see `Ext4::reclaim_credits`).
        let depth = inner
            .extent_manager()
            .map(|em| em.root_depth())
            .unwrap_or(0);
        let mut op = fs.begin_op(fs.reclaim_credits(depth))?;

        // Keep the inode listed across the multi-transaction free (defensive):
        // the normal delete and recovery paths already listed it (via `unlink`
        // or the primed recovery chain), but the create-error path reaches Drop
        // unlisted — a mid-free crash then could not resume. A no-op when
        // already listed. Journal-only.
        fs.orphan_add_if_absent(self.ino, op.get())?;

        let old_size = inner.file_size();
        // Only data-backed inodes (files, directories, slow symlinks) own a page
        // cache and extent-mapped blocks. A fast symlink stores its target inline
        // in `i_block` with no data block, so it skips both the page-cache resize
        // and the block truncate below.
        let extent_manager = inner.extent_manager().ok().cloned();
        if extent_manager.is_some() {
            inner.resize_page_cache(0, old_size)?;
        }
        // The real deletion time: while the inode stays listed the chain override
        // in `write_back_inode_desc` re-stamps the on-disk `i_dtime` with the
        // successor; only the final writeback (after `orphan_del` below) persists
        // this deletion time.
        inner.set_dtime(super::utils::now());
        inner.set_file_size(0);

        // Free the data blocks in credit-bounded chunks, each committed with its
        // own tree reserialize + inode writeback (crash red-line: freed ⟺
        // dropped-from-tree, one txn). Gate on the extent manager's live
        // `sector_count`, not the descriptor's copy (which ext2 uses): the extent
        // manager is the authority and the descriptor may be stale until
        // writeback. This divergence from the ext2 template is intentional.
        if let Some(extent_manager) = extent_manager
            && extent_manager.sector_count() > 0
        {
            loop {
                let Some(max) = fs.journal().map(|j| j.max_credits()) else {
                    extent_manager.truncate_to_byte_len(0, op.get())?;
                    break;
                };
                let chunk = extent_manager.truncate_chunk(0, op.get(), max)?;
                if chunk.reached == 0 {
                    // The final chunk: its writeback is deferred past `orphan_del`
                    // below so it persists the deletion time (unlisted), not the
                    // successor pointer the chain override stamps while listed.
                    break;
                }
                // Non-terminal: persist this chunk's frontier (SAME txn as its
                // frees), then restart. While listed, the chain override stamps
                // the on-disk `i_dtime` with the successor.
                inner.write_back_inode_desc(&fs, self.ino, op.get())?;
                if let Some(handle) = op.get_mut() {
                    journal::journal_restart(handle, chunk.next_bound)?;
                }
            }
        }

        // Last chunk: splice off the orphan list and free the inode bit, both in
        // this final transaction (with the deletion-time writeback, dirtied by
        // the last chunk's reserialize / the `set_dtime` above), so the unlist +
        // free are atomic against a crash.
        fs.orphan_del(self.ino, op.get())?;
        inner.write_back_inode_desc(&fs, self.ino, op.get())?;
        fs.free_inode(self.ino, self.type_, op.get())?;
        Ok(true)
    }

    /// Updates the permission bits (chmod) and bumps ctime, journaling the
    /// change on a journaled volume ([`journal_attr_change`](InodeInner::journal_attr_change))
    /// so it survives a crash before the deferred writeback.
    pub(super) fn set_mode(&self, mode: InodeMode) -> Result<()> {
        let fs = self.fs()?;
        let mut inner = self.inner.write();
        inner
            .desc
            .set_perm(FilePerm::from_bits_truncate(mode.bits()));
        inner.desc.set_ctime(super::utils::now());
        inner.journal_attr_change(&fs, self.ino)
    }

    /// Updates the owning uid (chown) and bumps ctime, journaled (see
    /// [`set_mode`](Self::set_mode)).
    pub(super) fn set_owner(&self, uid: u32) -> Result<()> {
        let fs = self.fs()?;
        let mut inner = self.inner.write();
        inner.desc.set_uid(uid);
        inner.desc.set_ctime(super::utils::now());
        inner.journal_attr_change(&fs, self.ino)
    }

    /// Updates the owning gid (chgrp) and bumps ctime, journaled (see
    /// [`set_mode`](Self::set_mode)).
    pub(super) fn set_group(&self, gid: u32) -> Result<()> {
        let fs = self.fs()?;
        let mut inner = self.inner.write();
        inner.desc.set_gid(gid);
        inner.desc.set_ctime(super::utils::now());
        inner.journal_attr_change(&fs, self.ino)
    }

    /// Sets the last-access time, journaled (see [`set_mode`](Self::set_mode)).
    /// The VFS time setters return `()`, so a journal failure is best-effort
    /// (logged, not propagated) — Linux `ext4_dirty_inode` likewise drops the
    /// update when the handle cannot start.
    pub(super) fn set_atime(&self, time: Duration) {
        self.journal_time_change(|desc| desc.set_atime(time), "atime");
    }

    /// Sets the last-modification time, journaled best-effort (see
    /// [`set_atime`](Self::set_atime)).
    pub(super) fn set_mtime(&self, time: Duration) {
        self.journal_time_change(|desc| desc.set_mtime(time), "mtime");
    }

    /// Sets the last-metadata-change time, journaled best-effort (see
    /// [`set_atime`](Self::set_atime)).
    pub(super) fn set_ctime(&self, time: Duration) {
        self.journal_time_change(|desc| desc.set_ctime(time), "ctime");
    }

    /// Applies a timestamp mutation to the descriptor and journals it on a
    /// journaled volume, swallowing (only logging) a journal failure — the void
    /// VFS time setters cannot propagate one. Non-journaled volumes keep the
    /// buffered-writeback behavior unchanged (`journal_attr_change` no-ops).
    fn journal_time_change(&self, mutate: impl FnOnce(&mut InodeDesc), what: &str) {
        let Ok(fs) = self.fs() else {
            return;
        };
        let mut inner = self.inner.write();
        // The `&mut Dirty<InodeDesc>` derefs to `&mut InodeDesc` for the closure,
        // marking the descriptor dirty (same as the direct `desc.set_*` setters).
        mutate(&mut inner.desc);
        if let Err(e) = inner.journal_attr_change(&fs, self.ino) {
            warn!(
                "could not journal {what} update for inode {}: {e:?}",
                self.ino
            );
        }
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

/// The outcome of one [`write_bounded_chunk`](InodeInner::write_bounded_chunk):
/// the chunk mapped, wrote, and converted a prefix of the remaining range, or it
/// could not fit even the first insert into the current transaction.
enum ChunkWrite {
    /// The chunk covered `[cursor, reached)` (`reached > cursor`); the spine
    /// advances the cursor to `reached`.
    Wrote(Iblock),
    /// No block fit this transaction: the first insert needs `need` credits the
    /// running transaction cannot grant even after growing in place. The spine
    /// restarts onto a fresh transaction reserving `need`, or — when `need`
    /// exceeds a whole transaction's capacity — reports the `EFBIG` floor.
    Stalled { need: usize },
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
    /// `Option`, not a `0` sentinel: tids wrap (see `Tid::geq`), so `0` is a
    /// legal transaction id a wrapped journal could hand out.
    sync_tid: Option<Tid>,
    /// The `fdatasync` subset (jbd2 `i_datasync_tid`): the transaction that
    /// captured this inode's most recent **data-relevant** metadata change —
    /// extent mapping or `i_size`, the metadata `fdatasync` must commit to
    /// retrieve the data. Only the write/truncate paths stamp it (see
    /// [`stamp_datasync_tid`](Self::stamp_datasync_tid)); a pure-attribute
    /// change (chmod/chown/utimens) bumps `sync_tid` but not this, so
    /// `fdatasync` does not force mode/owner/timestamp updates the data does
    /// not depend on. A subset of `sync_tid` — `fdatasync` waits on it,
    /// `fsync` on the full `sync_tid`. `None` (like `sync_tid`) on
    /// non-journaled volumes and before any data-relevant capture.
    datasync_tid: Option<Tid>,
    /// This inode's `metadata_csum` seed `crc32c(crc32c(fs_seed, ino),
    /// generation)`, or `None` when the feature is off. Seeds the directory-block
    /// and inode checksums this inner computes on writeback; the same value
    /// threads into the extent manager for the extent-node tail checksums.
    csum_seed: Option<InodeCsumSeed>,
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

    /// Zeroes the byte sub-range `[start, end)` (within one block) of a KEPT
    /// block in the page cache — the punch-hole partial-edge zeroing (Linux
    /// `ext4_zero_partial_blocks`).
    ///
    /// Only zeroes when the block is MAPPED: a hole already reads zero, and
    /// dirtying a page over a hole plants a backing-block-less dirty page the
    /// journaled writeback cannot honor (the same guard [`resize_page_cache`](Self::resize_page_cache)
    /// applies). Assumes `[start, end)` lies within one block (the caller splits
    /// on block boundaries), so a single mapping lookup covers it.
    fn zero_partial_block(&self, start: usize, end: usize) -> Result<()> {
        if start >= end {
            return Ok(());
        }
        let iblock = Iblock::try_from(start / BLOCK_SIZE).map_err(|_| {
            Error::with_message(Errno::EFBIG, "punch edge beyond 32-bit block space")
        })?;
        let mapped = matches!(
            self.extent_manager()?.map_blocks(iblock)?,
            extent_manager::Mapping::Mapped { .. }
        );
        if mapped {
            self.page_cache()?.fill_zeros(start..end)?;
        }
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
        let start_block = Iblock::try_from(offset / BLOCK_SIZE)
            .map_err(|_| Error::with_message(Errno::EFBIG, "block index exceeds 32 bits"))?;
        let end_block = Iblock::try_from(end.div_ceil(BLOCK_SIZE))
            .map_err(|_| Error::with_message(Errno::EFBIG, "block index exceeds 32 bits"))?;
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
        handle: Option<&mut journal::Handle>,
    ) -> Result<()> {
        let old_size = self.file_size();
        if new_size == old_size {
            return Ok(());
        }
        if new_size < old_size {
            // The single-transaction fast-path shrink; the multi-transaction
            // spine (`Inode::shrink_restartable`) is a separate path selected by
            // `Inode::resize`'s fast/slow gate.
            self.shrink(new_size, handle)?;
        } else {
            self.expand(fs, new_size)?;
        }
        self.set_mtime_ctime(super::utils::now());
        Ok(())
    }

    /// Shrinks the file in ONE transaction (the fast path): zeroes the kept
    /// partial last block in the page cache, frees every data/metadata block
    /// past `new_size`, and publishes the size. The chunked, orphan-protected
    /// multi-transaction shrink lives in [`Inode::shrink_restartable`], reached
    /// only when the whole truncate overruns one transaction.
    fn shrink(&mut self, new_size: usize, handle: Option<&mut journal::Handle>) -> Result<()> {
        let old_size = self.file_size();
        self.prepare_shrink(new_size, old_size)?;
        self.extent_manager()?
            .truncate_to_byte_len(new_size, handle.as_deref())?;
        Ok(())
    }

    /// The up-front, once-per-truncate work shared by the fast [`shrink`](Self::shrink)
    /// and the chunked [`Inode::shrink_restartable`]: flush the doomed tail
    /// pages, zero + shrink the page cache, and publish `i_size = new_size`.
    ///
    /// Flush first (report §5.2, shrink): discarding the doomed tail pages would
    /// orphan the flush obligation of an *earlier committing* transaction whose
    /// extents still reference them (its ordered flush only writes pages still
    /// dirty — a discarded page is silently gone, and replaying that transaction
    /// would then expose whatever the device holds). Then zero + shrink the VMO
    /// before the size drops (rule 4): `PageCache::resize` zeroes `[new_size,
    /// block_end)` of the kept partial block when that block is mapped, so stale
    /// tail bytes do not reappear on a later extend; a hole tail is left
    /// untouched. Publishing `i_size = new_size` here is the chunked spine's KEY
    /// INSIGHT — the target is committed once and never advanced, so recovery
    /// reads it and re-truncates to it.
    fn prepare_shrink(&mut self, new_size: usize, old_size: usize) -> Result<()> {
        if let Ok(page_cache) = self.page_cache() {
            let doomed_start = (new_size / BLOCK_SIZE) * BLOCK_SIZE;
            page_cache.flush_range(doomed_start..old_size)?;
        }
        self.resize_page_cache(new_size, old_size)?;
        self.set_file_size(new_size);
        Ok(())
    }

    /// Whether the cached descriptor's `i_dtime` still encodes a live inode
    /// (on-disk 0). The chunked-truncate G-2 invariant: a truncate never repoints
    /// the descriptor's `dtime` (it relies on the orphan chain override for the
    /// on-disk successor while listed), so a truncated file's final writeback
    /// persists `i_dtime = 0`.
    fn desc_dtime_is_live(&self) -> bool {
        self.desc.dtime.to_raw() == 0
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
    ///
    /// Holds the handle by `&mut` (not the shared `Option<&Handle>` the bounded
    /// metadata ops thread) so a future commit-boundary `journal_restart`
    /// (P7d-2b) is reachable at the safe seams between the allocate / page-write
    /// / convert steps below — where the extent-tree lock is released and no
    /// capture credential is live. The leaf spine (`prepare_write`,
    /// `rollback_write`, `mark_range_written`) is shared with the bounded
    /// dir/symlink paths and stays `Option<&Handle>`; each call reborrows
    /// `handle.as_deref()`.
    fn write_at(
        &mut self,
        fs: &Ext4,
        offset: usize,
        reader: &mut VmReader,
        handle: Option<&mut journal::Handle>,
    ) -> Result<usize> {
        let write_len = reader.remain();
        if write_len == 0 {
            return Ok(0);
        }
        let end = offset
            .checked_add(write_len)
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "write range overflow"))?;
        let old_size = self.file_size();

        if let Err(err) = self.prepare_write(fs, offset, end, handle.as_deref()) {
            self.rollback_write(old_size, end, handle.as_deref());
            return Err(err);
        }
        if let Err(err) = self.page_cache()?.write(offset, reader) {
            self.rollback_write(old_size, end, handle.as_deref());
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
        let start_block = Iblock::try_from(offset / BLOCK_SIZE)
            .map_err(|_| Error::with_message(Errno::EFBIG, "block index exceeds 32 bits"))?;
        let end_block = Iblock::try_from(end.div_ceil(BLOCK_SIZE))
            .map_err(|_| Error::with_message(Errno::EFBIG, "block index exceeds 32 bits"))?;
        if let Err(err) = self
            .extent_manager()
            .and_then(|em| em.mark_range_written(start_block, end_block, handle.as_deref()))
        {
            self.rollback_write(old_size, end, handle.as_deref());
            return Err(err);
        }

        self.set_mtime_ctime(super::utils::now());
        if end > old_size {
            self.set_file_size(end);
        }
        Ok(write_len)
    }

    /// Maps, writes, and converts ONE credit-bounded append chunk starting at
    /// `cursor_block`, in the caller's current transaction. The whole-write range
    /// is `[write_offset, write_end)`; a [`ChunkWrite::Wrote`] covers
    /// `[cursor_block, reached)`, and a [`ChunkWrite::Stalled`] means even the
    /// first insert would overflow the transaction and reports the reservation it
    /// needs so the spine can restart (or declare EFBIG).
    ///
    /// The chunk's blocks are allocated UNWRITTEN, then the page-cache data is
    /// written (a partial boundary block reads back as zeros — Unwritten-first),
    /// then the range is converted to written IN THIS TRANSACTION, so the extent
    /// metadata that makes the blocks readable-as-data commits no earlier than
    /// the caller's ordered-data flush. Growing the page cache to the chunk end
    /// bounds `PageCache::write` to exactly this chunk's bytes (the append
    /// invariant: `chunk_end > file_size`). On error the chunk's partial
    /// allocation and page-cache growth are rolled back to the pre-chunk size —
    /// earlier chunks are already committed and untouched.
    fn write_bounded_chunk(
        &mut self,
        fs: &Ext4,
        write_offset: usize,
        write_end: usize,
        cursor_block: Iblock,
        handle: Option<&journal::Handle>,
        reader: &mut VmReader,
    ) -> Result<ChunkWrite> {
        let size_before = self.file_size();
        match self.try_write_chunk(fs, write_offset, write_end, cursor_block, handle, reader) {
            Ok(outcome) => Ok(outcome),
            Err(err) => {
                // Free this chunk's just-allocated blocks and restore the page
                // cache; the earlier chunks committed under prior transactions
                // are durable and left alone.
                self.rollback_write(size_before, write_end, handle);
                Err(err)
            }
        }
    }

    /// The fallible body of [`write_bounded_chunk`](Self::write_bounded_chunk),
    /// split out so the caller can roll back the chunk on any error.
    fn try_write_chunk(
        &mut self,
        fs: &Ext4,
        write_offset: usize,
        write_end: usize,
        cursor_block: Iblock,
        handle: Option<&journal::Handle>,
        reader: &mut VmReader,
    ) -> Result<ChunkWrite> {
        let end_block = Iblock::try_from(write_end.div_ceil(BLOCK_SIZE))
            .map_err(|_| Error::with_message(Errno::EFBIG, "block index exceeds 32 bits"))?;
        let reached =
            match self
                .extent_manager()?
                .ensure_allocated_chunk(cursor_block, end_block, handle)?
            {
                extent_manager::HoleFill::Filled => end_block,
                // No block allocated: even the first insert would overflow the
                // transaction. Report the reservation it needs so the spine restarts
                // (or declares EFBIG); nothing was written, so leave the reader and
                // size untouched.
                extent_manager::HoleFill::Stopped { reached, need } if reached == cursor_block => {
                    return Ok(ChunkWrite::Stalled { need });
                }
                // Partial progress: `[cursor_block, reached)` was allocated before
                // the early stop. Map/write/convert what was allocated; the outer
                // spine's per-chunk `ensure_chunk_credits` starts the next chunk on a
                // fresh transaction.
                extent_manager::HoleFill::Stopped { reached, .. } => reached,
            };
        // This chunk's byte range: the write start for the first chunk, the
        // block-aligned cursor otherwise, up to the reached block (clamped to
        // the write end for the final chunk).
        let chunk_start = write_offset.max(cursor_block as usize * BLOCK_SIZE);
        let chunk_end = write_end.min(reached as usize * BLOCK_SIZE);
        let size_before = self.file_size();
        // Grow the page cache to the chunk end. This bounds `PageCache::write`
        // below to exactly this chunk's bytes (append invariant: the write
        // starts at or beyond EOF, so `chunk_end > size_before`).
        self.ensure_size_within_limit(fs, chunk_end)?;
        self.resize_page_cache(chunk_end, size_before)?;
        self.page_cache()?.write(chunk_start, reader)?;
        // Convert this chunk's blocks to written, in this transaction. The write
        // covers `[cursor_block, reached)` (the final block partially, its tail
        // read back as zeros); converting the whole run makes them readable.
        self.extent_manager()?
            .mark_range_written(cursor_block, reached, handle)?;
        self.set_mtime_ctime(super::utils::now());
        // Publish this committed chunk's on-disk size (per-chunk, DECISION D-1).
        self.set_file_size(chunk_end);
        Ok(ChunkWrite::Wrote(reached))
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

    /// Stamps `datasync_tid` (jbd2 `i_datasync_tid`) with `handle`'s transaction
    /// — the `fdatasync` wait target. Call it only from the write/truncate paths,
    /// right after their [`write_back_inode_desc`](Self::write_back_inode_desc)
    /// capture: those change the data-relevant metadata (extent mapping /
    /// `i_size`) `fdatasync` must commit. Pure-attribute changes must NOT call it,
    /// so `fdatasync` stays narrower than `fsync`. Mirrors Linux
    /// `ext4_update_inode_fsync_trans(handle, inode, /* need_datasync */ 1)`.
    ///
    /// A no-op without a handle (non-journaled volume leaves `datasync_tid`
    /// `None`, matching `sync_tid`). We stamp on every write/truncate capture
    /// rather than only on block-allocating ones — broader than Linux's
    /// allocation-precise gate (which needs `ensure_allocated` to report whether
    /// it allocated, a P7/P9 refinement, the same precision the ordered-data
    /// registration defers), but only ever conservative: `fdatasync` may commit
    /// a hair more than the strict minimum, never less.
    fn stamp_datasync_tid(&mut self, handle: Option<&journal::Handle>) {
        if let Some(handle) = handle {
            self.datasync_tid = Some(handle.tid());
        }
    }

    /// On a journaled volume, captures this inode's descriptor into its own
    /// single-block transaction so a pure-attribute change (chmod/chown/utimens)
    /// survives a crash before the deferred writeback — Linux
    /// `ext4_mark_inode_dirty` under the setattr handle. Stamps `sync_tid` (via
    /// the capture, so `fsync` waits) but never `datasync_tid`: attributes are
    /// not needed to retrieve the data, so `fdatasync` must not force them.
    ///
    /// Non-journaled volumes are unchanged: no handle, no shutdown gate — the
    /// mutated descriptor stays dirty for the buffered fsync writeback, exactly
    /// as before P7. The caller holds the inner lock; the handle opens under it
    /// (① inner → ② handle).
    fn journal_attr_change(&mut self, fs: &Ext4, ino: Ext4Ino) -> Result<()> {
        if fs.journal().is_none() {
            return Ok(());
        }
        let op = fs.begin_op(Ext4::SETATTR_CREDITS)?;
        self.write_back_inode_desc(fs, ino, op.get())
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
    /// Inline data (small files stored in the inode); not supported (volumes are
    /// mounted `^inline_data`).
    #[expect(dead_code)]
    Inline,
    /// Devices, FIFOs, and sockets: no in-memory payload — a device node's id is
    /// decoded from the descriptor's `i_block` on demand (`InodeDesc::device_id`).
    NoPayload,
}

impl InodePayload {
    /// Builds the payload for `desc`; fails if a data-backed inode's extent
    /// root does not parse (`ExtentTree::try_new` — the parse-once boundary).
    fn new(desc: &InodeDesc, fs: Weak<Ext4>, csum_seed: Option<InodeCsumSeed>) -> Result<Self> {
        Ok(match desc.type_() {
            // The freed-data revoke policy keys off the inode type, exactly
            // Linux `get_default_free_blocks_flags` (fs/ext4/extents.c:
            // 2405-2420): directory blocks are journaled metadata and
            // symlink targets are revoked conservatively, so both forget
            // their freed data blocks; regular-file data (ordered mode,
            // never journaled) does not.
            InodeType::File => Self::new_data_backed(
                desc.size() as usize,
                *desc.raw_block(),
                desc.sector_count(),
                fs,
                csum_seed,
                journal::DataForgetPolicy::PlainData,
            )?,
            InodeType::Dir => Self::new_data_backed(
                desc.size() as usize,
                *desc.raw_block(),
                desc.sector_count(),
                fs,
                csum_seed,
                journal::DataForgetPolicy::Forget,
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
                    Self::new_data_backed(
                        size,
                        *desc.raw_block(),
                        desc.sector_count(),
                        fs,
                        csum_seed,
                        journal::DataForgetPolicy::Forget,
                    )?
                }
            }
            // Devices, FIFOs, and sockets carry no payload (a device id lives in
            // the descriptor's `i_block`, decoded on demand).
            _ => Self::NoPayload,
        })
    }

    fn new_data_backed(
        size: usize,
        root: [u32; RAW_BLOCK_PTRS_LEN],
        sector_count: u64,
        fs: Weak<Ext4>,
        csum_seed: Option<InodeCsumSeed>,
        data_forget_policy: journal::DataForgetPolicy,
    ) -> Result<Self> {
        let page_cache_size = size.align_up(PAGE_SIZE);
        let page_count = page_cache_size / PAGE_SIZE;
        let extent_manager = Arc::new(ExtentManager::try_new(
            root,
            sector_count,
            fs,
            page_count,
            csum_seed,
            data_forget_policy,
        )?);
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
        let iseed = checksum::FsCsumSeed::new(fs_seed).derive_inode(ino, raw.generation);

        let crc = InodeDesc::inode_checksum(&raw, iseed, INODE_SIZE);
        raw.checksum_lo = (crc & 0xFFFF) as u16;
        raw.checksum_hi = ((crc >> 16) & 0xFFFF) as u16;
        InodeDesc::verify_inode_checksum(&raw, iseed, INODE_SIZE).unwrap();

        // Wrong inode number / generation are folded into the seed.
        let wrong_ino_seed =
            checksum::FsCsumSeed::new(fs_seed).derive_inode(ino + 1, raw.generation);
        assert_eq!(
            InodeDesc::verify_inode_checksum(&raw, wrong_ino_seed, INODE_SIZE)
                .unwrap_err()
                .error(),
            Errno::EUCLEAN
        );
        // A regenerated inode changes the covered body (its `generation` field),
        // so it no longer verifies against the original seed.
        let mut regen = raw;
        regen.generation = 0x55AB;
        assert!(InodeDesc::verify_inode_checksum(&regen, iseed, INODE_SIZE).is_err());

        // Corrupted body (the covered range excludes the checksum fields).
        let mut bad = raw;
        bad.size_lo += 1;
        assert!(InodeDesc::verify_inode_checksum(&bad, iseed, INODE_SIZE).is_err());

        // Storing the checksum back does not change the covered result.
        let recheck = InodeDesc::inode_checksum(&raw, iseed, INODE_SIZE);
        assert_eq!(recheck, crc);
    }

    /// The write-side `stamp_inode_checksum` produces exactly what the read-side
    /// `verify_inode_checksum` accepts — the compute/verify round-trip at the
    /// inode writeback funnel.
    #[ktest]
    fn stamp_inode_checksum_round_trips_with_verify() {
        const INODE_SIZE: usize = 256;
        let fs_seed = 0x0102_0304;
        let ino: Ext4Ino = 27;
        let mut raw = raw_root_dir();
        raw.generation = 0xC0FFEE;
        let iseed = checksum::FsCsumSeed::new(fs_seed).derive_inode(ino, raw.generation);

        InodeDesc::stamp_inode_checksum(&mut raw, iseed, INODE_SIZE);
        InodeDesc::verify_inode_checksum(&raw, iseed, INODE_SIZE).unwrap();

        // A post-stamp body change is then caught by verify.
        raw.link_count += 1;
        assert!(InodeDesc::verify_inode_checksum(&raw, iseed, INODE_SIZE).is_err());
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
            inode.sync_data_and_meta(SyncScope::Full).unwrap();
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

    /// RED-LINE ①: if the ordered-data registration fails AFTER the write's
    /// extent/`i_size` metadata is captured (an `ENOMEM` growing the map), the
    /// transaction must not stay committable — committing metadata that points
    /// at blocks with no flush obligation would expose stale bytes on a
    /// post-commit crash. The failure aborts the journal, discarding the
    /// running transaction's captures (nothing can ever commit), and the write
    /// fails; the filesystem is left read-only.
    #[ktest]
    fn ordered_data_enomem_aborts_the_transaction() {
        let f = journaled_fixture_with_empty_file();
        let journal = f.ext4.journal().unwrap();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        // Arm the one-shot fault, then drive an allocating write: it maps and
        // writes the data, captures the extent tree + i_size, then trips the
        // fault at ordered-data registration.
        journal.arm_ordered_data_enomem();
        let data = [0x5au8; 2 * BLOCK_SIZE];
        let mut reader = VmReader::from(&data[..]).to_fallible();
        let err = inode.write_at(0, &mut reader).unwrap_err();
        assert_eq!(err.error(), Errno::ENOMEM);

        // The journal aborted and recorded no ordered-data obligation; the
        // captured metadata can never be made durable (the abort discards it),
        // and every further write op is refused with EROFS.
        assert!(journal.is_aborted());
        assert_eq!(journal.running_nr_ordered_data_for_test(), 0);
        assert_eq!(
            journal.commit_and_wait_running().unwrap_err().error(),
            Errno::EIO
        );
        assert_eq!(
            f.ext4.begin_op(4).map(|_| ()).unwrap_err().error(),
            Errno::EROFS
        );
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

    /// Builds a journaled fixture whose data area spans many small block groups,
    /// with the commit thread left RUNNING. A single-group image collapses every
    /// allocating capture onto the same handful of metadata blocks (one bitmap,
    /// one GDT, one superblock), so no append can overflow a transaction; this
    /// image's 16-block groups give each group an 11-block free run, so a
    /// contiguous append fragments into one extent per group and captures a
    /// DISTINCT block bitmap per group — the only shape that fills a transaction
    /// and forces a real mid-write `journal_restart`. The restart's re-admission
    /// waits for room, so the commit thread must be live to drain it (else it
    /// hangs); `Ext4::drop` stops the thread at teardown.
    ///
    /// The journal sits at physical block 200 (groups 12-13). `maxlen` sizes the
    /// per-transaction credit ceiling (`max_credits`); the callers keep every
    /// data allocation under ~11 groups (block < 192), clear of the journal.
    fn journaled_multigroup_fixture(maxlen: u32) -> Ext4Fixture {
        clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(16, 16, 512)
            .with_block_bitmap_metadata_marked()
            .with_journal_inode(maxlen)
            .build()
            .unwrap();
        f.write_raw_inode(FILE_INO, &make_empty_file_inode());
        f
    }

    /// P7d-2b (Fix C) — the real mid-write restart the single-group fixtures
    /// cannot reach. A contiguous append over a many-small-group image fragments
    /// into one extent per group; each extent captures a distinct block bitmap,
    /// so the running transaction fills before the whole append is mapped and the
    /// spine must `journal_restart` onto a fresh transaction mid-write — proving
    /// the credit-threaded restart (Fix A) drains and completes rather than
    /// spuriously failing. The commit thread is live so the restart's
    /// re-admission drains; the file must read back byte-exact with every block
    /// mapped written and `i_blocks`/size consistent.
    #[ktest]
    fn append_spanning_many_groups_forces_journal_restart() {
        // `max_credits` = 12 here (journal usable = 15): equal to
        // `write_credits(depth 1)`, so once the append's tree reaches depth 1 a
        // chunk fits only a fresh transaction — a captured predecessor forces a
        // restart. The 120-block append fragments across ~11 groups (≈ 14
        // distinct captures), comfortably past the ceiling.
        let f = journaled_multigroup_fixture(16);
        let journal = f.ext4.journal().unwrap();
        assert_eq!(journal.max_credits(), 12);
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        const N_BLOCKS: usize = 120;
        let payload: Vec<u8> = (0..N_BLOCKS * BLOCK_SIZE)
            .map(|k| (k * 31 + 7) as u8)
            .collect();
        assert_eq!(write_all(&inode, 0, &payload), payload.len());

        // At least one genuine mid-write restart occurred (the hook the fast
        // path leaves at zero).
        assert!(
            journal.restart_count_for_test() >= 1,
            "a fragmenting append across many groups must restart at least once, got {}",
            journal.restart_count_for_test()
        );

        // The whole append landed durably and reads back byte-exact.
        assert_eq!(inode.size(), payload.len());
        assert_eq!(read_back(&inode, 0, payload.len()), payload);

        // Every logical block maps to a WRITTEN extent (the convert ran on each
        // chunk, across the restart boundary).
        {
            let inner = inode.inner.read();
            let bm = inner.extent_manager().unwrap();
            for i in 0..u32::try_from(N_BLOCKS).unwrap() {
                assert_eq!(
                    bm.map_blocks(i).unwrap().state(),
                    MapState::Written,
                    "block {i} not converted to written",
                );
            }
        }

        // `i_blocks`: the 120 data blocks plus the single depth-1 leaf node the
        // tree needs (≤ 340 extents fit one leaf), consistent after the restart.
        assert_eq!(
            inode.sector_count(),
            (N_BLOCKS as u64 + 1) * SECTORS_PER_BLOCK
        );
    }

    /// P7 §A/§C/§E — the multi-transaction truncate. A 120-block file fragmented
    /// across ~11 groups (one distinct block bitmap per group) has a whole-free
    /// that overruns one transaction, so the shrink takes the chunked,
    /// orphan-protected spine: it lists the inode, frees the tail one
    /// credit-bounded chunk per transaction (a real `journal_restart` at each
    /// boundary), and unlists in the last chunk. The commit thread is live so the
    /// restarts' re-admissions drain. Asserts a genuine truncate restart, a
    /// drained orphan list, and a consistent tree / `i_blocks` / `i_size`.
    #[ktest]
    fn truncate_spans_multiple_transactions_and_restarts() {
        let f = journaled_multigroup_fixture(16);
        let journal = f.ext4.journal().unwrap();
        assert_eq!(journal.max_credits(), 12);
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        const N_BLOCKS: usize = 120;
        let payload: Vec<u8> = (0..N_BLOCKS * BLOCK_SIZE)
            .map(|k| (k * 31 + 7) as u8)
            .collect();
        assert_eq!(write_all(&inode, 0, &payload), payload.len());

        // The whole-truncate estimate overruns one transaction → the chunked
        // orphan path (the write above already restarted, so measure the delta).
        let keep = 5usize;
        let est = {
            let inner = inode.inner.read();
            inner
                .extent_manager()
                .unwrap()
                .plan_shrink(keep * BLOCK_SIZE, journal.max_credits())
                .unwrap()
                .whole_estimate
        };
        assert!(
            est > journal.max_credits(),
            "a many-group truncate must exceed one transaction: est={est} max={}",
            journal.max_credits()
        );

        let restarts_before = journal.restart_count_for_test();
        inode.resize(keep * BLOCK_SIZE).unwrap();

        // A genuine mid-truncate restart occurred.
        assert!(
            journal.restart_count_for_test() > restarts_before,
            "a many-group truncate must restart: {restarts_before} -> {}",
            journal.restart_count_for_test()
        );
        // The orphan list drained: the last chunk unlisted the inode.
        assert_eq!(
            f.ext4.super_block().last_orphan(),
            None,
            "the truncate must drain the orphan list"
        );

        // Size, tree, and `i_blocks` are consistent, and the kept prefix reads
        // back byte-exact.
        assert_eq!(inode.size(), keep * BLOCK_SIZE);
        assert_eq!(
            read_back(&inode, 0, keep * BLOCK_SIZE),
            payload[..keep * BLOCK_SIZE].to_vec()
        );
        {
            let inner = inode.inner.read();
            let bm = inner.extent_manager().unwrap();
            for i in 0..u32::try_from(keep).unwrap() {
                assert_eq!(
                    bm.map_blocks(i).unwrap().state(),
                    MapState::Written,
                    "kept block {i} must survive"
                );
            }
            assert_eq!(
                bm.map_blocks(u32::try_from(keep).unwrap()).unwrap().state(),
                MapState::Hole,
                "the freed tail must be a hole"
            );
        }
        // The survivor `[0, 5)` collapses to an inline tree (no external leaf): 5
        // data blocks, zero metadata.
        assert_eq!(inode.sector_count(), keep as u64 * SECTORS_PER_BLOCK);
    }

    /// P7d-2cd (BLOCKING 1) — an INTERMEDIATE truncate chunk must serialize a
    /// SORTED on-disk extent tree. `truncate_chunk` builds the survivor `kept`
    /// out of logical order (ascending prefix ++ descending un-freed doomed ++
    /// straddler); a non-terminal chunk that does not sort it before serializing
    /// stamps a valid `metadata_csum` over an out-of-order leaf with a
    /// non-monotonic index key, so a crash between chunks replays a tree e2fsck
    /// reports dirty. The final state self-heals (the next chunk re-flattens and
    /// sorts), so the multi-restart tests miss it — this drives exactly ONE
    /// credit-bounded chunk, commits it (the crash boundary), and inspects the
    /// committed tree: every logical block the survivor still references must
    /// map. An unsorted survivor makes `search_entries` break early and report a
    /// covered block as a false hole.
    #[ktest]
    fn intermediate_truncate_chunk_serializes_sorted_ondisk_tree() {
        let f = journaled_multigroup_fixture(16);
        let journal = f.ext4.journal().unwrap();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        // A 120-block file fragmented one extent per group → a multi-leaf tree
        // whose whole free overruns one transaction, so a single credit-bounded
        // chunk stops mid-truncate leaving un-freed doomed extents in the
        // survivor (the only place the out-of-order-survivor bug can show).
        const N_BLOCKS: usize = 120;
        let payload: Vec<u8> = (0..N_BLOCKS * BLOCK_SIZE)
            .map(|k| (k * 31 + 7) as u8)
            .collect();
        assert_eq!(write_all(&inode, 0, &payload), payload.len());

        let keep_blocks = 5u32;
        let max = journal.max_credits();

        // Drive exactly ONE chunk of a truncate to `keep_blocks`, as the spine's
        // first iteration does, then STOP before the outer loop restarts and its
        // next chunk re-flattens+re-sorts (which self-heals a bad intermediate).
        let em = inode.inner.read().extent_manager().unwrap().clone();
        let reached = {
            let op = f
                .ext4
                .begin_op(f.ext4.truncate_credits(em.root_depth()))
                .unwrap();
            let chunk = em
                .truncate_chunk(keep_blocks as usize * BLOCK_SIZE, op.get(), max)
                .unwrap();
            chunk.reached
        };
        // Commit this chunk to the log — the exact crash boundary between chunks,
        // so the read below sees what a replay would reconstruct.
        journal.commit_and_wait_running().unwrap();

        // The chunk stopped SHORT of `keep_blocks`: a genuine intermediate (a
        // terminal chunk's survivor `[0, keep_blocks)` is already sorted).
        assert!(
            reached > keep_blocks,
            "the chunk must stop mid-truncate to exercise an intermediate tree: reached={reached}"
        );

        // The file was written contiguously, so the survivor `[0, reached)` is a
        // contiguous mapped prefix: every logical block in it MUST map. A false
        // hole here is the unsorted-survivor corruption (a non-monotonic tree
        // makes `search_entries` break early past a covered block).
        for lb in 0..reached {
            assert_ne!(
                em.map_blocks(lb).unwrap().state(),
                MapState::Hole,
                "intermediate chunk left covered logical block {lb} a false hole (unsorted survivor)"
            );
        }
    }

    /// P7 §E (G-3) — the single-transaction fast path. A small shrink whose whole
    /// truncate fits one transaction takes the atomic path: NO orphan_add/del, NO
    /// restart, byte-identical to before P7. The regression guard for the common
    /// case.
    #[ktest]
    fn single_txn_truncate_takes_no_orphan() {
        let f = journaled_fixture_with_empty_file();
        let journal = f.ext4.journal().unwrap();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        let payload: Vec<u8> = (0..4 * BLOCK_SIZE).map(|k| (k * 7 + 3) as u8).collect();
        assert_eq!(write_all(&inode, 0, &payload), payload.len());

        let target = BLOCK_SIZE + 10;
        // The gate routes this to the atomic fast path.
        let est = {
            let inner = inode.inner.read();
            inner
                .extent_manager()
                .unwrap()
                .plan_shrink(target, journal.max_credits())
                .unwrap()
                .whole_estimate
        };
        assert!(
            est <= journal.max_credits(),
            "a small truncate must fit one transaction: est={est} max={}",
            journal.max_credits()
        );

        let restarts_before = journal.restart_count_for_test();
        inode.resize(target).unwrap();

        // Fast path: never listed on the orphan chain, never restarted.
        assert_eq!(
            f.ext4.super_block().last_orphan(),
            None,
            "the fast path must not touch the orphan list"
        );
        assert_eq!(
            journal.restart_count_for_test(),
            restarts_before,
            "the fast path must not restart"
        );

        assert_eq!(inode.size(), target);
        assert_eq!(
            read_back(&inode, 0, BLOCK_SIZE + 10),
            payload[..BLOCK_SIZE + 10].to_vec()
        );
        // Blocks 0 and 1 kept (1 is the partial last block).
        assert_eq!(inode.sector_count(), 2 * SECTORS_PER_BLOCK);
    }

    /// A byte pattern whose values are all NONZERO, so a read can tell original
    /// data (nonzero) apart from a punched/preallocated hole (zero).
    fn nonzero_pattern(len: usize) -> Vec<u8> {
        (0..len).map(|k| (k % 250 + 1) as u8).collect()
    }

    /// P7e-3 — `fallocate(Allocate)` reserves UNWRITTEN blocks and extends
    /// `i_size`: the range reads zero (nothing written yet), and a later write
    /// converts just the written block to written while its neighbours stay
    /// reserved.
    #[ktest]
    fn fallocate_allocate_reserves_unwritten_and_extends_size() {
        let f = fixture_with_empty_file();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        let len = 3 * BLOCK_SIZE;
        inode.fallocate(FallocMode::Allocate, 0, len).unwrap();

        // i_size grew to the range end; three blocks are reserved.
        assert_eq!(inode.size(), len);
        assert_eq!(inode.sector_count(), 3 * SECTORS_PER_BLOCK);
        {
            let inner = inode.inner.read();
            let bm = inner.extent_manager().unwrap();
            for i in 0..3 {
                let m = bm.map_blocks(i).unwrap();
                assert_eq!(
                    m.state(),
                    MapState::Unwritten,
                    "block {i} must be reserved-unwritten"
                );
                assert!(m.reads_as_zeros());
            }
        }
        // The reserved range reads zero.
        assert_eq!(read_back(&inode, 0, len), vec![0u8; len]);

        // A real write into the reserved range converts THAT block to written and
        // reads back the data; the neighbours stay unwritten.
        let payload = nonzero_pattern(BLOCK_SIZE);
        write_all(&inode, BLOCK_SIZE, &payload);
        assert_eq!(read_back(&inode, BLOCK_SIZE, BLOCK_SIZE), payload);
        {
            let inner = inode.inner.read();
            let bm = inner.extent_manager().unwrap();
            assert_eq!(bm.map_blocks(0).unwrap().state(), MapState::Unwritten);
            assert_eq!(bm.map_blocks(1).unwrap().state(), MapState::Written);
            assert_eq!(bm.map_blocks(2).unwrap().state(), MapState::Unwritten);
        }
    }

    /// P7e-3 — `fallocate(AllocateKeepSize)` reserves UNWRITTEN blocks WITHOUT
    /// changing `i_size` (the KEEP_SIZE flag): the blocks are reserved past EOF.
    #[ktest]
    fn fallocate_allocate_keep_size_reserves_without_growing() {
        let f = fixture_with_empty_file();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        // Seed one written block so old_size = BLOCK_SIZE, then reserve past EOF.
        write_all(&inode, 0, &nonzero_pattern(BLOCK_SIZE));
        assert_eq!(inode.size(), BLOCK_SIZE);

        inode
            .fallocate(FallocMode::AllocateKeepSize, BLOCK_SIZE, 2 * BLOCK_SIZE)
            .unwrap();

        // KEEP_SIZE: i_size is unchanged, but the two blocks past EOF are reserved.
        assert_eq!(inode.size(), BLOCK_SIZE);
        assert_eq!(inode.sector_count(), 3 * SECTORS_PER_BLOCK);
        {
            let inner = inode.inner.read();
            let bm = inner.extent_manager().unwrap();
            assert_eq!(bm.map_blocks(1).unwrap().state(), MapState::Unwritten);
            assert_eq!(bm.map_blocks(2).unwrap().state(), MapState::Unwritten);
        }
    }

    /// P7e-3 — `fallocate(PunchHoleKeepSize)` over a BLOCK-ALIGNED middle range
    /// frees the mapped blocks (leaving a hole that reads zero), keeps the
    /// surrounding data, and does not change `i_size`. The freed blocks travel the
    /// same forget/revoke + pin free funnel as truncate (data via `PlainData`,
    /// tree nodes via `free_meta_block`).
    #[ktest]
    fn fallocate_punch_hole_middle_reads_zero_surrounding_intact() {
        let f = fixture_with_empty_file();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        let payload = nonzero_pattern(4 * BLOCK_SIZE);
        write_all(&inode, 0, &payload);
        assert_eq!(inode.sector_count(), 4 * SECTORS_PER_BLOCK);

        // Punch the middle two blocks [1, 3).
        inode
            .fallocate(FallocMode::PunchHoleKeepSize, BLOCK_SIZE, 2 * BLOCK_SIZE)
            .unwrap();

        // i_size unchanged; the punched blocks are freed (a hole), the rest kept.
        assert_eq!(inode.size(), 4 * BLOCK_SIZE);
        assert_eq!(inode.sector_count(), 2 * SECTORS_PER_BLOCK);
        {
            let inner = inode.inner.read();
            let bm = inner.extent_manager().unwrap();
            assert_eq!(bm.map_blocks(0).unwrap().state(), MapState::Written);
            assert_eq!(bm.map_blocks(1).unwrap().state(), MapState::Hole);
            assert_eq!(bm.map_blocks(2).unwrap().state(), MapState::Hole);
            assert_eq!(bm.map_blocks(3).unwrap().state(), MapState::Written);
        }
        // Block 0 and block 3 intact; the punched range reads zero.
        assert_eq!(read_back(&inode, 0, BLOCK_SIZE), payload[0..BLOCK_SIZE]);
        assert_eq!(
            read_back(&inode, BLOCK_SIZE, 2 * BLOCK_SIZE),
            vec![0u8; 2 * BLOCK_SIZE]
        );
        assert_eq!(
            read_back(&inode, 3 * BLOCK_SIZE, BLOCK_SIZE),
            payload[3 * BLOCK_SIZE..4 * BLOCK_SIZE]
        );
    }

    /// P7e-3 — a punch whose edges are NOT block-aligned ZEROES the partial edge
    /// blocks (they are kept, their uncovered bytes survive) and FREES only the
    /// fully-covered block in between (Linux `ext4_zero_partial_blocks` +
    /// `ext4_ext_remove_space`).
    #[ktest]
    fn fallocate_punch_partial_edges_zero_not_free() {
        let f = fixture_with_empty_file();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        let payload = nonzero_pattern(4 * BLOCK_SIZE);
        write_all(&inode, 0, &payload);

        // Punch [BLOCK+100, 3*BLOCK+100): head partial in block 1, one fully
        // covered block 2, tail partial in block 3.
        let off = BLOCK_SIZE + 100;
        let len = 2 * BLOCK_SIZE;
        inode
            .fallocate(FallocMode::PunchHoleKeepSize, off, len)
            .unwrap();

        assert_eq!(inode.size(), 4 * BLOCK_SIZE);
        // Only the fully-covered block 2 is freed; the partial-edge blocks 1 and 3
        // are kept.
        assert_eq!(inode.sector_count(), 3 * SECTORS_PER_BLOCK);
        {
            let inner = inode.inner.read();
            let bm = inner.extent_manager().unwrap();
            assert_eq!(bm.map_blocks(1).unwrap().state(), MapState::Written);
            assert_eq!(bm.map_blocks(2).unwrap().state(), MapState::Hole);
            assert_eq!(bm.map_blocks(3).unwrap().state(), MapState::Written);
        }

        // Block 0 whole intact.
        assert_eq!(read_back(&inode, 0, BLOCK_SIZE), payload[0..BLOCK_SIZE]);
        // Block 1: [0,100) original, [100, BLOCK) zeroed.
        assert_eq!(
            read_back(&inode, BLOCK_SIZE, 100),
            payload[BLOCK_SIZE..BLOCK_SIZE + 100]
        );
        assert_eq!(
            read_back(&inode, BLOCK_SIZE + 100, BLOCK_SIZE - 100),
            vec![0u8; BLOCK_SIZE - 100]
        );
        // Block 2: whole hole.
        assert_eq!(
            read_back(&inode, 2 * BLOCK_SIZE, BLOCK_SIZE),
            vec![0u8; BLOCK_SIZE]
        );
        // Block 3: [0,100) zeroed, [100, BLOCK) original.
        assert_eq!(read_back(&inode, 3 * BLOCK_SIZE, 100), vec![0u8; 100]);
        assert_eq!(
            read_back(&inode, 3 * BLOCK_SIZE + 100, BLOCK_SIZE - 100),
            payload[3 * BLOCK_SIZE + 100..4 * BLOCK_SIZE]
        );
    }

    /// P7e-3 (red-line ①) — a punch straddling TWO ADJACENT blocks with a partial
    /// edge on EACH side and NO fully-covered block between them: each partial is
    /// zeroed against its OWN block's mapping. Both blocks are Written here, so
    /// both covered sub-ranges are zeroed and NOTHING is freed (no full block).
    /// The single-block fast path must be keyed on same-block, not `aligned_start
    /// >= aligned_end` (which also holds for this adjacent-partial geometry).
    #[ktest]
    fn fallocate_punch_two_adjacent_partials_both_mapped() {
        let f = fixture_with_empty_file();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        // Blocks 0..3 Written with nonzero data; the punch touches only 1 and 2.
        let payload = nonzero_pattern(3 * BLOCK_SIZE);
        write_all(&inode, 0, &payload);

        // Punch [BLOCK+100, 2*BLOCK+100): block 1's tail + block 2's head, no
        // fully-covered block between.
        inode
            .fallocate(FallocMode::PunchHoleKeepSize, BLOCK_SIZE + 100, BLOCK_SIZE)
            .unwrap();

        // No fully-covered block → nothing freed; both blocks stay Written.
        assert_eq!(inode.size(), 3 * BLOCK_SIZE);
        assert_eq!(inode.sector_count(), 3 * SECTORS_PER_BLOCK);
        {
            let inner = inode.inner.read();
            let bm = inner.extent_manager().unwrap();
            assert_eq!(bm.map_blocks(1).unwrap().state(), MapState::Written);
            assert_eq!(bm.map_blocks(2).unwrap().state(), MapState::Written);
        }
        // Block 1: [0,100) intact, [100, BLOCK) zeroed.
        assert_eq!(
            read_back(&inode, BLOCK_SIZE, 100),
            payload[BLOCK_SIZE..BLOCK_SIZE + 100]
        );
        assert_eq!(
            read_back(&inode, BLOCK_SIZE + 100, BLOCK_SIZE - 100),
            vec![0u8; BLOCK_SIZE - 100]
        );
        // Block 2: [0,100) zeroed, [100, BLOCK) intact.
        assert_eq!(read_back(&inode, 2 * BLOCK_SIZE, 100), vec![0u8; 100]);
        assert_eq!(
            read_back(&inode, 2 * BLOCK_SIZE + 100, BLOCK_SIZE - 100),
            payload[2 * BLOCK_SIZE + 100..3 * BLOCK_SIZE]
        );
    }

    /// P7e-3 (red-line ①, silent-stale) — two adjacent partials where the HEAD
    /// block is a hole and the TAIL block is Written: the tail-partial must be
    /// zeroed against ITS OWN mapping. The pre-fix merged call checked only the
    /// head block (a hole) and returned WITHOUT zeroing, leaving the tail block's
    /// covered head holding stale bytes (a read of a punched range returns data).
    #[ktest]
    fn fallocate_punch_two_adjacent_partials_head_hole() {
        let f = fixture_with_empty_file();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        // block 1 = hole, block 2 = Written nonzero (write only [2*BLOCK,3*BLOCK);
        // blocks 0 and 1 stay holes, i_size = 3*BLOCK).
        let payload = nonzero_pattern(BLOCK_SIZE);
        write_all(&inode, 2 * BLOCK_SIZE, &payload);
        assert_eq!(inode.size(), 3 * BLOCK_SIZE);
        {
            let inner = inode.inner.read();
            let bm = inner.extent_manager().unwrap();
            assert_eq!(bm.map_blocks(1).unwrap().state(), MapState::Hole);
            assert_eq!(bm.map_blocks(2).unwrap().state(), MapState::Written);
        }

        // Punch [BLOCK+100, 2*BLOCK+100): head partial in the hole block 1, tail
        // partial in the Written block 2.
        inode
            .fallocate(FallocMode::PunchHoleKeepSize, BLOCK_SIZE + 100, BLOCK_SIZE)
            .unwrap();

        // Block 2's covered head [2*BLOCK, 2*BLOCK+100) is now zeroed (the fix);
        // its tail survives. The pre-fix code leaves the head at its stale bytes.
        assert_eq!(read_back(&inode, 2 * BLOCK_SIZE, 100), vec![0u8; 100]);
        assert_eq!(
            read_back(&inode, 2 * BLOCK_SIZE + 100, BLOCK_SIZE - 100),
            payload[100..BLOCK_SIZE]
        );
    }

    /// P7e-3 (red-line ①, journal-abort) — two adjacent partials where the HEAD
    /// block is Written and the TAIL block is a hole: the head-partial is zeroed
    /// but NO dirty page is planted over the tail hole. The pre-fix merged call
    /// checked only the (mapped) head block and then `fill_zeros` dirtied BOTH
    /// pages, including the hole's; the punch transaction's ordered-data flush
    /// then hit that backing-block-less page — aborting the journal (fs read-only)
    /// or spuriously allocating the tail block — all triggered by a punch that
    /// already returned success. Drives a real commit to exercise the flush.
    #[ktest]
    fn fallocate_punch_two_adjacent_partials_tail_hole() {
        let f = journaled_multigroup_fixture(64);
        let journal = f.ext4.journal().unwrap();
        journal.stop_commit_thread();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        // block 1 = Written nonzero, block 2 = hole, block 3 = Written (so i_size
        // reaches 4*BLOCK while block 2 stays a hole). Commit the setup writes so
        // the punch commits alone below.
        let payload = nonzero_pattern(BLOCK_SIZE);
        write_all(&inode, BLOCK_SIZE, &payload);
        write_all(&inode, 3 * BLOCK_SIZE, &payload);
        journal.commit_now_for_test();
        {
            let inner = inode.inner.read();
            let bm = inner.extent_manager().unwrap();
            assert_eq!(bm.map_blocks(1).unwrap().state(), MapState::Written);
            assert_eq!(bm.map_blocks(2).unwrap().state(), MapState::Hole);
        }

        // Punch [BLOCK+100, 2*BLOCK+100): head partial in Written block 1, tail
        // partial in the hole block 2.
        inode
            .fallocate(FallocMode::PunchHoleKeepSize, BLOCK_SIZE + 100, BLOCK_SIZE)
            .unwrap();
        // Block 1's tail is zeroed; its head survives.
        assert_eq!(read_back(&inode, BLOCK_SIZE, 100), payload[..100]);
        assert_eq!(
            read_back(&inode, BLOCK_SIZE + 100, BLOCK_SIZE - 100),
            vec![0u8; BLOCK_SIZE - 100]
        );

        // Force the punch transaction to commit and run its ordered-data flush.
        // The fix left NO dirty page over block 2, so the flush touches nothing
        // there: the journal stays healthy and block 2 stays a hole. The pre-fix
        // dirty page over the hole would abort the commit or plant a block.
        journal.commit_now_for_test();
        assert!(
            !journal.is_aborted(),
            "a legitimate punch must not abort the journal"
        );
        {
            let inner = inode.inner.read();
            let bm = inner.extent_manager().unwrap();
            assert_eq!(
                bm.map_blocks(2).unwrap().state(),
                MapState::Hole,
                "the tail hole must not be planted with a block"
            );
        }
    }

    /// P7e-3 (MINOR) — a failed `fallocate(Allocate)` must NOT advance `i_size`:
    /// Linux extends the size only at the end of `ext4_alloc_file_blocks`, on
    /// success. Here the allocator runs out of space partway; `i_size` must stay
    /// at its original value. The pre-fix code grew `i_size` up front and left it
    /// at the range end after the failed allocation.
    #[ktest]
    fn fallocate_allocate_failure_leaves_size_unchanged() {
        clocks::init_for_ktest();
        // A single-group fixture capped to a few free blocks: an `Allocate` over
        // more blocks than that runs the allocator out of space partway.
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_free_blocks(4)
            .build()
            .unwrap();
        f.write_raw_inode(FILE_INO, &make_empty_file_inode());
        let inode = f.ext4.read_inode(FILE_INO).unwrap();
        assert_eq!(inode.size(), 0);

        // Reserve 32 blocks with only 4 free → `ENOSPC` partway.
        let err = inode
            .fallocate(FallocMode::Allocate, 0, 32 * BLOCK_SIZE)
            .unwrap_err();
        assert_eq!(err.error(), Errno::ENOSPC);

        // i_size unchanged after the failed allocation.
        assert_eq!(inode.size(), 0, "a failed Allocate must not advance i_size");
    }

    /// P7e-3 (red-line ①) — a large punch over a fragmented file spans multiple
    /// journal transactions: the free reuses the truncate chunk+restart spine, so
    /// a real mid-punch `journal_restart` fires. A crash between chunks leaves a
    /// valid partial hole, so — unlike truncate — the punch NEVER touches the
    /// orphan list.
    #[ktest]
    fn fallocate_large_punch_spans_transactions() {
        let f = journaled_multigroup_fixture(16);
        let journal = f.ext4.journal().unwrap();
        assert_eq!(journal.max_credits(), 12);
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        const N_BLOCKS: usize = 120;
        let payload: Vec<u8> = (0..N_BLOCKS * BLOCK_SIZE)
            .map(|k| (k % 250 + 1) as u8)
            .collect();
        assert_eq!(write_all(&inode, 0, &payload), payload.len());

        // Punch a large aligned middle range [10, 110): ~100 blocks fragmented
        // across many groups (one distinct block bitmap per group), whose free
        // overruns one transaction.
        let restarts_before = journal.restart_count_for_test();
        inode
            .fallocate(
                FallocMode::PunchHoleKeepSize,
                10 * BLOCK_SIZE,
                100 * BLOCK_SIZE,
            )
            .unwrap();

        // A genuine mid-punch restart occurred (reusing the d2-cd machinery).
        assert!(
            journal.restart_count_for_test() > restarts_before,
            "a large fragmented punch must restart: {restarts_before} -> {}",
            journal.restart_count_for_test()
        );
        // Punch never lists the inode on the orphan chain (no i_size change).
        assert_eq!(
            f.ext4.super_block().last_orphan(),
            None,
            "a punch must not touch the orphan list"
        );

        // i_size unchanged; the surrounding data intact, the punched range zero.
        assert_eq!(inode.size(), N_BLOCKS * BLOCK_SIZE);
        assert_eq!(
            read_back(&inode, 0, 10 * BLOCK_SIZE),
            payload[0..10 * BLOCK_SIZE]
        );
        assert_eq!(
            read_back(&inode, 10 * BLOCK_SIZE, 100 * BLOCK_SIZE),
            vec![0u8; 100 * BLOCK_SIZE]
        );
        assert_eq!(
            read_back(&inode, 110 * BLOCK_SIZE, 10 * BLOCK_SIZE),
            payload[110 * BLOCK_SIZE..120 * BLOCK_SIZE]
        );
        // The punched-out blocks all map as holes.
        {
            let inner = inode.inner.read();
            let bm = inner.extent_manager().unwrap();
            for i in 10u32..110 {
                assert_eq!(
                    bm.map_blocks(i).unwrap().state(),
                    MapState::Hole,
                    "punched block {i} must be a hole"
                );
            }
            assert_eq!(bm.map_blocks(9).unwrap().state(), MapState::Written);
            assert_eq!(bm.map_blocks(110).unwrap().state(), MapState::Written);
        }
    }

    /// Serializes one depth-0 extent leaf node into a full block: a 12-byte
    /// header (`eh_max` = `count`, depth 0) then `count` written extents mapping
    /// `first_logical + i` (length 1) to a dummy physical block. Plants a
    /// pre-built deep tree on disk; the physical targets are never read (the
    /// EFBIG stop precedes any allocation or data read).
    fn build_extent_leaf(first_logical: u32, count: u16) -> Vec<u8> {
        const ENTRY_BYTES: usize = 12;
        let mut block = vec![0u8; BLOCK_SIZE];
        block[0..2].copy_from_slice(&0xF30Au16.to_le_bytes()); // eh_magic
        block[2..4].copy_from_slice(&count.to_le_bytes()); // eh_entries
        block[4..6].copy_from_slice(&count.to_le_bytes()); // eh_max
        // eh_depth (0) and eh_generation are already zero.
        for i in 0..count {
            let off = ENTRY_BYTES * (1 + i as usize);
            let logical = first_logical + u32::from(i);
            block[off..off + 4].copy_from_slice(&logical.to_le_bytes()); // ee_block
            block[off + 4..off + 6].copy_from_slice(&1u16.to_le_bytes()); // ee_len (written)
            // ee_start_hi (0) already zero; ee_start_lo is a dummy valid block.
            block[off + 8..off + 12].copy_from_slice(&5u32.to_le_bytes()); // ee_start_lo
        }
        block
    }

    /// P7d-2b (Fix C) — the EFBIG floor. When one extent insert's whole-tree
    /// reserialize needs more credits than a whole transaction holds, no restart
    /// can ever fit it, so the append fails `EFBIG` — but WITHOUT leaking blocks,
    /// because the credit check precedes any allocation. A depth-1 tree with 1360
    /// extents (4 full leaves) sits one insert below a fifth leaf: appending one
    /// more block projects to 6 external nodes, whose reserialize (`need2` = 10)
    /// exceeds this journal's `max_credits` (9) while the tree's own
    /// `write_credits` (8) still admits the handle — the exact window the floor
    /// guards. This tree is too large to build through the allocator (each depth
    /// increase would itself EFBIG once the journal is this small), so it is laid
    /// down directly on disk.
    #[ktest]
    fn append_insert_exceeding_one_transaction_is_efbig_with_no_leak() {
        clocks::init_for_ktest();
        // Single group, journal maxlen 13 → max_credits 9 (usable 12: 508*10/509).
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_journal_inode(13)
            .build()
            .unwrap();
        let journal = f.ext4.journal().unwrap();
        journal.stop_commit_thread();
        assert_eq!(journal.max_credits(), 9);

        // Plant a depth-1 tree of 1360 extents: 4 leaf blocks at physical 300..304
        // (clear of the journal at 200), each holding 340 length-1 extents, and an
        // inline index root pointing at them.
        const LEAF_MAX_ENTRIES: u16 = 340;
        const NR_LEAVES: u32 = 4;
        const NR_EXTENTS: u32 = LEAF_MAX_ENTRIES as u32 * NR_LEAVES; // 1360
        const LEAF0_BID: u32 = 300;
        for g in 0..NR_LEAVES {
            let leaf = build_extent_leaf(g * u32::from(LEAF_MAX_ENTRIES), LEAF_MAX_ENTRIES);
            f.disk
                .segment()
                .write_bytes((LEAF0_BID + g) as usize * BLOCK_SIZE, &leaf)
                .unwrap();
        }

        let mut raw = make_empty_file_inode();
        raw.size_lo = u32::try_from(NR_EXTENTS as usize * BLOCK_SIZE).unwrap();
        raw.sector_count =
            u32::try_from((NR_EXTENTS as u64 + NR_LEAVES as u64) * SECTORS_PER_BLOCK).unwrap();
        // Inline index root: eh_magic | eh_entries=4; eh_max=4 | eh_depth=1.
        raw.block[0] = 0xF30A | (NR_LEAVES << 16);
        raw.block[1] = NR_LEAVES | (1 << 16);
        raw.block[2] = 0; // eh_generation
        for g in 0..NR_LEAVES {
            let w = 3 + (g as usize) * 3;
            raw.block[w] = g * u32::from(LEAF_MAX_ENTRIES); // ei_block
            raw.block[w + 1] = LEAF0_BID + g; // ei_leaf_lo
            raw.block[w + 2] = 0; // ei_leaf_hi | ei_unused
        }
        f.write_raw_inode(FILE_INO, &raw);

        let inode = f.ext4.read_inode(FILE_INO).unwrap();
        let free_before = f.ext4.super_block().free_blocks_count();
        let sectors_before = inode.sector_count();

        // Append one block past the 1360-block EOF: the single insert into the
        // 1361-extent (fifth-leaf) tree cannot fit one transaction → EFBIG.
        let mut reader = VmReader::from(&[0xABu8; BLOCK_SIZE][..]).to_fallible();
        let err = inode
            .write_at(NR_EXTENTS as usize * BLOCK_SIZE, &mut reader)
            .unwrap_err();
        assert_eq!(err.error(), Errno::EFBIG);

        // Zero leaked blocks: the credit check preceded any allocation, so the
        // free-block counter and `i_blocks` are untouched.
        assert_eq!(f.ext4.super_block().free_blocks_count(), free_before);
        assert_eq!(inode.sector_count(), sectors_before);
    }

    /// P7d-2b — a journaled contiguous append fits one transaction: the chunked
    /// append spine takes the fast path (`remaining >= need`) with ZERO
    /// restarts, identical behavior to before d2-b. The regression guard for the
    /// zero-overhead common case.
    #[ktest]
    fn contiguous_append_takes_the_no_restart_fast_path() {
        let f = journaled_fixture_with_empty_file();
        let journal = f.ext4.journal().unwrap();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        let payload: Vec<u8> = (0..5 * BLOCK_SIZE).map(|k| (k * 7 + 1) as u8).collect();
        assert_eq!(write_all(&inode, 0, &payload), payload.len());

        // A single contiguous extent — one insert, one chunk, no restart.
        assert_eq!(
            journal.restart_count_for_test(),
            0,
            "a contiguous append must not restart"
        );
        assert_eq!(inode.size(), payload.len());
        assert_eq!(read_back(&inode, 0, payload.len()), payload);
        let inner = inode.inner.read();
        let bm = inner.extent_manager().unwrap();
        for i in 0..5u32 {
            assert_eq!(bm.map_blocks(i).unwrap().state(), MapState::Written);
        }
    }

    /// P7d-2b — a multi-block append that spans several logical blocks maps,
    /// writes, and converts every block through the credit-bounded append spine
    /// (`ensure_allocated_chunk` → per-chunk page write → `mark_range_written`).
    /// A contiguous run over a single-group image stays one transaction; the
    /// mid-write `journal_restart` boundary is exercised end to end by
    /// `append_spanning_many_groups_forces_journal_restart` (a many-small-group
    /// image) and at the primitive level by
    /// `journal_restart_from_locking_seat_readmits_after_staging`.
    #[ktest]
    fn multi_block_append_maps_writes_and_converts_every_block() {
        let f = journaled_fixture_with_empty_file();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        // A first append, then a second contiguous append past EOF — two passes
        // through the append spine, distinct byte patterns.
        let first: Vec<u8> = (0..3 * BLOCK_SIZE).map(|k| (k * 5 + 1) as u8).collect();
        let second: Vec<u8> = (0..2 * BLOCK_SIZE).map(|k| (k * 9 + 2) as u8).collect();
        assert_eq!(write_all(&inode, 0, &first), first.len());
        assert_eq!(write_all(&inode, first.len(), &second), second.len());

        assert_eq!(inode.size(), first.len() + second.len());
        assert_eq!(read_back(&inode, 0, first.len()), first);
        assert_eq!(read_back(&inode, first.len(), second.len()), second);
        // i_blocks counts the 5 data blocks, consistent after the writes.
        assert_eq!(inode.sector_count(), 5 * SECTORS_PER_BLOCK);
        let inner = inode.inner.read();
        let bm = inner.extent_manager().unwrap();
        for i in 0..5u32 {
            assert_eq!(bm.map_blocks(i).unwrap().state(), MapState::Written);
        }
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
        // The freeing transaction must commit before the block can recycle:
        // freed blocks are pinned out of the allocator until then (P7b
        // freed-block pinning). Production reaches the recycle the same way —
        // after the free's commit — and the stale bytes are still on the
        // device (nothing zeroes them).
        f.ext4.journal().unwrap().commit_now_for_test();

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

    // FILE_INO (11) lives in group 0's first inode-table block: base
    // `INODE_TABLE_BID` (4) + (11 - 1) / (BLOCK_SIZE / INODE_SIZE = 16) = 4.
    const FILE_INODE_TABLE_BID: Ext4Bid = 4;

    /// P7d-4 Task 2: chmod/chown/utimens on a journaled volume open a handle and
    /// capture the inode-table block, so the change survives a crash before the
    /// deferred writeback. Every attribute folds into the one inode-table block,
    /// bumping `sync_tid` (fsync waits) but not `datasync_tid` (fdatasync does
    /// not force non-data metadata). Once committed, the after-image is durable
    /// in the journal (recovery replays it).
    #[ktest]
    fn setattr_journals_inode_desc_and_survives_commit() {
        let f = journaled_fixture_with_empty_file();
        let journal = f.ext4.journal().unwrap();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        let before = journal.running_nr_metadata_blocks();
        inode
            .set_mode(InodeMode::from_bits_truncate(0o600))
            .unwrap();
        assert_eq!(
            journal.running_nr_metadata_blocks(),
            before + 1,
            "chmod captures exactly the inode-table block"
        );
        assert!(
            journal
                .running_captured_blocks_for_test()
                .contains(&FILE_INODE_TABLE_BID),
            "the captured block is the inode's inode-table block"
        );

        // chown and utimens fold into the SAME inode-table block.
        inode.set_owner(4242).unwrap();
        inode.set_atime(Duration::from_secs(111));
        inode.set_mtime(Duration::from_secs(222));
        assert_eq!(
            journal.running_nr_metadata_blocks(),
            before + 1,
            "further attribute changes reuse the one inode-table block"
        );

        // fsync waits on the bumped sync_tid; fdatasync's datasync_tid is
        // untouched — a pure-attribute change is not data-relevant.
        assert!(inode.recorded_sync_tid_for_test().is_some());
        assert!(inode.recorded_datasync_tid_for_test().is_none());

        // Crash-durability: once the transaction commits, the inode-desc
        // after-image is durable in the journal (committed, awaiting
        // checkpoint) — recovery would replay the attribute change.
        journal.commit_now_for_test();
        assert!(
            journal
                .uncheckpointed_blocks_for_test()
                .contains(&FILE_INODE_TABLE_BID),
            "the committed attribute change is durable in the journal"
        );
    }

    /// P7d-4 Task 2: a non-journaled volume keeps the pre-P7 behavior — the
    /// attribute change lives only in the dirty in-memory descriptor (no handle,
    /// no recorded tid) and persists via the buffered metadata writeback.
    #[ktest]
    fn setattr_on_non_journaled_volume_stays_buffered() {
        let f = fixture_with_empty_file();
        assert!(f.ext4.journal().is_none());
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        inode
            .set_mode(InodeMode::from_bits_truncate(0o600))
            .unwrap();
        inode.set_owner(4242).unwrap();
        inode.set_atime(Duration::from_secs(111));

        // No handle opened: the change is only in the dirty descriptor, and no
        // sync/datasync tid was recorded.
        assert!(inode.inner.read().is_dirty());
        assert!(inode.recorded_sync_tid_for_test().is_none());
        assert!(inode.recorded_datasync_tid_for_test().is_none());

        // It persists via the buffered metadata writeback, as before P7.
        inode.sync_metadata().unwrap();
        let reloaded = f.ext4.read_inode(FILE_INO).unwrap();
        assert_eq!(reloaded.mode().bits() & 0o777, 0o600);
        assert_eq!(reloaded.uid(), 4242);
    }

    /// P7d-4 Task 1: `fdatasync` narrows to `datasync_tid`. A data write stamps
    /// both tids; a following chmod (committed into a strictly later
    /// transaction) bumps only `sync_tid`. `fdatasync`'s target stays the
    /// write's already-committed tid — it must NOT be dragged forward to force
    /// the chmod's commit, which `fsync` would still wait for.
    #[ktest]
    fn fdatasync_narrows_to_datasync_tid_excluding_chmod() {
        let f = journaled_fixture_with_empty_file();
        let journal = f.ext4.journal().unwrap();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        // A data write stamps BOTH tids with its transaction (T_w).
        write_all(&inode, 0, &[0xAB; BLOCK_SIZE]);
        let t_w = inode.recorded_datasync_tid_for_test().unwrap();
        assert_eq!(inode.recorded_sync_tid_for_test(), Some(t_w));

        // Commit the write, then chmod into a strictly later transaction (T_c).
        journal.commit_now_for_test();
        assert!(journal.committed_tid().geq(t_w));
        inode
            .set_mode(InodeMode::from_bits_truncate(0o600))
            .unwrap();

        let sync = inode.recorded_sync_tid_for_test().unwrap();
        let dsync = inode.recorded_datasync_tid_for_test().unwrap();
        assert_eq!(dsync, t_w, "datasync_tid stays at the write's transaction");
        assert!(
            sync.geq(t_w.next()),
            "sync_tid advanced to the chmod's later transaction"
        );
        assert_ne!(sync, dsync);

        // fdatasync's target (datasync_tid = T_w) is already durable, so it
        // returns without forcing the chmod; fsync's target (sync_tid = T_c) is
        // not yet committed.
        assert!(journal.committed_tid().geq(dsync));
        assert!(!journal.committed_tid().geq(sync));
    }

    /// P7d-4 Task 1: a truncate that changes `i_size` IS data-relevant (the size
    /// is needed to read the data), so it advances `datasync_tid` together with
    /// `sync_tid` — unlike a chmod. `fdatasync` after such a truncate waits for
    /// it.
    #[ktest]
    fn fdatasync_follows_size_changing_truncate() {
        let f = journaled_fixture_with_empty_file();
        let journal = f.ext4.journal().unwrap();
        let inode = f.ext4.read_inode(FILE_INO).unwrap();

        write_all(&inode, 0, &[0xAB; 2 * BLOCK_SIZE]);
        let t_w = inode.recorded_datasync_tid_for_test().unwrap();
        journal.commit_now_for_test();

        inode.resize(BLOCK_SIZE).unwrap();
        let dsync = inode.recorded_datasync_tid_for_test().unwrap();
        let sync = inode.recorded_sync_tid_for_test().unwrap();
        assert!(
            dsync.geq(t_w.next()),
            "the size-changing truncate advanced datasync_tid past the write"
        );
        assert_eq!(dsync, sync, "a size change bumps both tids together");
    }
}
