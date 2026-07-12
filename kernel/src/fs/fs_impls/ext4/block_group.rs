// SPDX-License-Identifier: MPL-2.0

//! Ext4 block-group descriptors and the block-side allocation domain.
//!
//! Each block group is an independent allocation domain owning its own block
//! bitmap. Phase 2 brings the block side to life: the per-group block bitmap is
//! loaded at mount, blocks are allocated/freed against it, and dirty metadata is
//! written back via read-modify-write (RMW) so the many on-disk fields the
//! decoded descriptor drops are preserved losslessly.
//!
//! Phase 2 also adds a per-group **inode cache** giving live inodes a stable
//! identity (the same `Arc<Inode>` for a given inode number) so the filesystem
//! can enumerate and flush every dirty inode together with the block-side
//! metadata.
//!
//! Phase 3 brings the inode side to life: the per-group inode bitmap is loaded
//! at mount alongside the block bitmap, inodes are allocated/freed against it,
//! and the descriptor's free-inode and used-dirs counters are written back via
//! the same read-modify-write. The inode-table page cache still arrives later;
//! inode-table writeback stays a direct RMW via `Ext4::write_back_inode_desc`.
//!
//! # Width invariant
//!
//! Group-relative bit indices and per-group counters are narrowed to `u16`
//! throughout this file. That leans on one parse-time invariant: a group holds
//! at most `block_size * 8 = 32768 < u16::MAX` blocks/inodes
//! (`SuperBlock::try_from` rejects larger `s_{blocks,inodes}_per_group`), and
//! counters never exceed the group capacity.
//!
//! # Locking
//!
//! `BlockGroup` uses two independent locks:
//!
//! - `metadata` — protects the group descriptor, the block bitmap, and the
//!   inode bitmap. Held briefly during alloc/free operations; the inode bitmap
//!   lives under this same lock, so the inode allocator introduces no new lock
//!   acquisition order.
//! - `inode_cache` — protects the per-group live inode map. Uses double-checked
//!   locking (read then promote to write on miss). Never held while syncing an
//!   inode (see [`BlockGroup::sync_inodes`]).

use core::fmt;

use super::{
    checksum::{self, FsCsumSeed},
    fs::Ext4,
    inode::{Inode, InodeDesc, RawInode},
    journal,
    prelude::*,
    super_block::SuperBlock,
};

/// `bg_flags` bit: the group's inode bitmap and inode table are uninitialized
/// (`EXT4_BG_INODE_UNINIT`). No inode has ever been allocated here; the on-disk
/// inode bitmap is not maintained and the inode table is not zeroed.
const BG_INODE_UNINIT: u16 = 0x0001;
/// `bg_flags` bit: the group's block bitmap is uninitialized
/// (`EXT4_BG_BLOCK_UNINIT`). No data block has ever been allocated here; the
/// on-disk block bitmap is not maintained and must be reconstructed from the
/// group layout (only the group's fixed metadata/backup overhead is in use).
const BG_BLOCK_UNINIT: u16 = 0x0002;

const_assert!(size_of::<RawBlockGroup>() == 32);

/// On-disk block-group descriptor, 32 bytes (without the `64BIT` high halves).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(super) struct RawBlockGroup {
    pub block_bitmap_lo: u32,
    pub inode_bitmap_lo: u32,
    pub inode_table_lo: u32,
    pub free_blocks_count_lo: u16,
    pub free_inodes_count_lo: u16,
    pub used_dirs_count_lo: u16,
    /// `bg_flags` (e.g. `INODE_UNINIT`, `BLOCK_UNINIT`, `INODE_ZEROED`).
    pub flags: u16,
    pub exclude_bitmap_lo: u32,
    pub block_bitmap_csum_lo: u16,
    pub inode_bitmap_csum_lo: u16,
    pub itable_unused_lo: u16,
    pub checksum: u16,
}

const_assert!(size_of::<RawBlockGroupHi>() == 32);

/// The 32-byte high-half tail of a `64BIT` group descriptor (`ext4_group_desc`
/// bytes 32..64). Present only when `s_desc_size == 64`; carries the block-number
/// high halves plus the per-group checksums the `metadata_csum` feature adds.
///
/// The per-group counter high halves (`*_count_hi`, `itable_unused_hi`) are
/// structurally zero in our geometry: a group holds at most `block_size * 8 =
/// 32768 < u16::MAX` blocks/inodes, so the counters never overflow their low
/// half. Only the block-number high halves can be non-zero (on volumes past
/// `2^32` blocks), and they are the only tail fields the decoder splices.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(super) struct RawBlockGroupHi {
    pub block_bitmap_hi: u32,
    pub inode_bitmap_hi: u32,
    pub inode_table_hi: u32,
    pub free_blocks_count_hi: u16,
    pub free_inodes_count_hi: u16,
    pub used_dirs_count_hi: u16,
    pub itable_unused_hi: u16,
    pub exclude_bitmap_hi: u32,
    pub block_bitmap_csum_hi: u16,
    pub inode_bitmap_csum_hi: u16,
    pub reserved: u32,
}

const_assert!(size_of::<RawBlockGroup64>() == 64);

/// On-disk 64-byte `64BIT` group descriptor: the classic 32-byte low half
/// ([`RawBlockGroup`]) followed by the 32-byte high-half tail
/// ([`RawBlockGroupHi`]). Read whole when `s_desc_size == 64`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(super) struct RawBlockGroup64 {
    pub lo: RawBlockGroup,
    pub hi: RawBlockGroupHi,
}

/// Validated, Rust-typed block-group descriptor.
///
/// Block numbers are `Ext4Bid` (`u64`) so the `64BIT` high halves slot in later
/// without widening; in Phase 2 they are the 32-bit low halves.
#[derive(Clone, Copy, Debug)]
pub(super) struct BlockGroupDesc {
    block_bitmap_bid: Ext4Bid,
    inode_bitmap_bid: Ext4Bid,
    inode_table_bid: Ext4Bid,
    free_blocks_count: u32,
    free_inodes_count: u32,
    used_dirs_count: u32,
    /// `bg_flags` — carries the `BLOCK_UNINIT`/`INODE_UNINIT` lazy-init bits. Kept
    /// so the block side can reconstruct an uninitialized bitmap on load and clear
    /// the bit (persisting it through `patch_into`/`sync_metadata`) on first use.
    flags: u16,
    /// `bg_itable_unused` — inodes at the tail of this group's inode table that
    /// have never been used. Recomputed from the inode bitmap on every alloc/free
    /// (see [`Self::itable_unused_from_bitmap`]) so it stays consistent with the
    /// e2fsck check on a `metadata_csum` volume; this port does not yet lazily
    /// initialize (zero) inode tables, so `INODE_UNINIT` groups are skipped by
    /// the allocator instead.
    itable_unused: u32,
}

impl BlockGroupDesc {
    /// Decodes a group descriptor from its raw low half and, for `64BIT` volumes,
    /// its high-half tail. This is the single parse-once boundary where the block
    /// numbers combine `lo | (hi << 32)` (rust_rules #3); no caller splices high
    /// halves itself, so a decode bug lives in exactly one place.
    ///
    /// The `(lo as u64) | ((hi as u64) << 32)` assembly is lossless: `lo` is
    /// `u32`, `hi` is `u32`, and their union fits `u64` with no bits dropped. The
    /// per-group counters are taken from the low half only — their high halves
    /// are structurally zero (see [`RawBlockGroupHi`]).
    fn from_raw(lo: &RawBlockGroup, hi: Option<&RawBlockGroupHi>) -> Self {
        let (block_bitmap_hi, inode_bitmap_hi, inode_table_hi) = match hi {
            Some(hi) => (hi.block_bitmap_hi, hi.inode_bitmap_hi, hi.inode_table_hi),
            None => (0, 0, 0),
        };
        // `itable_unused` has a high half in the 64BIT tail; splice it like the
        // block numbers (in our geometry it fits the low half, but decode both so
        // the value round-trips losslessly).
        let itable_unused_hi = hi.map_or(0, |hi| hi.itable_unused_hi);
        Self {
            block_bitmap_bid: (lo.block_bitmap_lo as Ext4Bid)
                | ((block_bitmap_hi as Ext4Bid) << 32),
            inode_bitmap_bid: (lo.inode_bitmap_lo as Ext4Bid)
                | ((inode_bitmap_hi as Ext4Bid) << 32),
            inode_table_bid: (lo.inode_table_lo as Ext4Bid) | ((inode_table_hi as Ext4Bid) << 32),
            free_blocks_count: lo.free_blocks_count_lo as u32,
            free_inodes_count: lo.free_inodes_count_lo as u32,
            used_dirs_count: lo.used_dirs_count_lo as u32,
            flags: lo.flags,
            itable_unused: (lo.itable_unused_lo as u32) | ((itable_unused_hi as u32) << 16),
        }
    }

    /// Computes the crc32c group-descriptor checksum (`metadata_csum`), low 16
    /// bits (Linux `ext4_group_desc_csum`). Seeded with the per-filesystem `seed`,
    /// then folded over the 0-based `group` number, the descriptor bytes up to
    /// `bg_checksum`, two zero bytes standing in for `bg_checksum` itself, and —
    /// for a 64-byte (`64BIT`) descriptor — the 32-byte high-half tail.
    fn group_desc_checksum(
        lo: &RawBlockGroup,
        hi: Option<&RawBlockGroupHi>,
        group: u32,
        seed: FsCsumSeed,
    ) -> u16 {
        // Byte offset of `bg_checksum` within the 32-byte low half.
        const BG_CHECKSUM_OFFSET: usize = 30;
        let mut crc = checksum::crc32c(seed.get(), &group.to_le_bytes());
        crc = checksum::crc32c(crc, &lo.as_bytes()[..BG_CHECKSUM_OFFSET]);
        crc = checksum::crc32c(crc, &[0u8, 0u8]); // bg_checksum, excluded from its own cover
        if let Some(hi) = hi {
            crc = checksum::crc32c(crc, hi.as_bytes());
        }
        (crc & 0xFFFF) as u16
    }

    /// Computes the fixed crc32c checksum of a bitmap block (Linux
    /// `ext4_block/inode_bitmap_csum`): crc32c of its first `len` bytes
    /// (`blocks_per_group / 8` or `ceil(inodes_per_group / 8)`).
    fn bitmap_checksum(seed: FsCsumSeed, bitmap: &[u8], len: usize) -> u32 {
        checksum::crc32c(seed.get(), &bitmap[..len])
    }

    /// Stamps the `metadata_csum` fields into decoded descriptor halves: the two
    /// precomputed bitmap checksums (low 16 in `*_csum_lo`, high 16 in
    /// `*_csum_hi` for a 64-byte descriptor), then `bg_checksum` over the result.
    fn stamp_checksums(
        lo: &mut RawBlockGroup,
        mut hi: Option<&mut RawBlockGroupHi>,
        group: u32,
        seed: FsCsumSeed,
        block_bitmap_csum: u32,
        inode_bitmap_csum: u32,
    ) {
        lo.block_bitmap_csum_lo = block_bitmap_csum as u16;
        lo.inode_bitmap_csum_lo = inode_bitmap_csum as u16;
        if let Some(hi) = hi.as_deref_mut() {
            hi.block_bitmap_csum_hi = (block_bitmap_csum >> 16) as u16;
            hi.inode_bitmap_csum_hi = (inode_bitmap_csum >> 16) as u16;
        }
        lo.checksum = Self::group_desc_checksum(lo, hi.as_deref(), group, seed);
    }

    /// Verifies a raw descriptor's stored `bg_checksum` for a `metadata_csum`
    /// volume, at the descriptor read boundary.
    fn verify_group_desc_checksum(
        lo: &RawBlockGroup,
        hi: Option<&RawBlockGroupHi>,
        group: u32,
        seed: FsCsumSeed,
    ) -> Result<()> {
        if lo.checksum != Self::group_desc_checksum(lo, hi, group, seed) {
            return_errno_with_message!(Errno::EUCLEAN, "bad group descriptor checksum");
        }
        Ok(())
    }

    /// Patches this group's mutable descriptor counters into the after-image of the
    /// descriptor block, for op-time journaling.
    ///
    /// The descriptor block holds many group descriptors; this group's lives at
    /// `desc_offset % BLOCK_SIZE` within the block. It is a read-modify-write on the
    /// seeded buffer (mirroring [`BlockGroup::sync_metadata`]): the mutated
    /// low-half fields — the three counters plus `flags` and `itable_unused_lo` —
    /// are overwritten from the in-memory descriptor (and the caller restamps
    /// `bg_checksum`), while `exclude`/reserved fields and every *other* group's
    /// descriptor in the same block are preserved. For a
    /// 64-byte (`64BIT`) descriptor this rewrites only the 32-byte low half at
    /// `desc_offset % BLOCK_SIZE`; the high-half tail — the block-number high halves
    /// (unchanged by counter updates) and the structurally-zero counter high halves
    /// — is left intact, and the descriptor never straddles a block (`4096 % 64 ==
    /// 0`). Because
    /// the after-image carries the absolute in-memory counters, repeated captures of
    /// the same block *within one transaction* converge on the final value.
    ///
    /// # Cross-transaction seed correctness (B-1)
    ///
    /// A partial patch like this is only sound because the capture's seed is
    /// guaranteed current: `get_write_access` seeds from the newest
    /// committed-but-un-checkpointed after-image of the block when the journal
    /// retains one, and from the device only once checkpoint has made it
    /// authoritative again (see `UncheckpointedImage` in `journal/transaction.rs`).
    /// Seeding straight from the device — which lags until checkpoint, and the
    /// commit thread runs asynchronously even under a single-threaded workload —
    /// would resurrect the *other* groups' stale counters in this block whenever
    /// two operations landed in separate transactions, and checkpoint (tid order,
    /// newest wins) would clobber the first transaction's committed counts. The
    /// same guarantee covers the other shared sub-block captures: inode-table
    /// blocks (where the clobber is silent neighbor-inode data loss) and the
    /// superblock.
    fn patch_into(&self, buf: &mut [u8], desc_offset: usize) {
        let off = desc_offset % BLOCK_SIZE;
        let raw_bytes = &mut buf[off..off + size_of::<RawBlockGroup>()];
        let mut raw = RawBlockGroup::from_bytes(raw_bytes);
        raw.free_blocks_count_lo = self.free_blocks_count() as u16;
        raw.free_inodes_count_lo = self.free_inodes_count() as u16;
        raw.used_dirs_count_lo = self.used_dirs_count() as u16;
        // Persist `bg_flags` too: clearing `BLOCK_UNINIT` on first allocation into
        // a lazy group must reach disk, or a later mount would re-reconstruct the
        // bitmap over blocks we have since handed out. `itable_unused` is likewise
        // recomputed on alloc/free and written back here.
        raw.flags = self.flags;
        raw.itable_unused_lo = self.itable_unused as u16;
        raw_bytes.copy_from_slice(raw.as_bytes());
    }

    /// Returns the starting block of this group's inode table.
    pub(super) const fn inode_table_bid(&self) -> Ext4Bid {
        self.inode_table_bid
    }

    pub(super) const fn block_bitmap_bid(&self) -> Ext4Bid {
        self.block_bitmap_bid
    }

    pub(super) const fn inode_bitmap_bid(&self) -> Ext4Bid {
        self.inode_bitmap_bid
    }

    pub(super) const fn free_blocks_count(&self) -> u32 {
        self.free_blocks_count
    }

    pub(super) const fn free_inodes_count(&self) -> u32 {
        self.free_inodes_count
    }

    pub(super) const fn used_dirs_count(&self) -> u32 {
        self.used_dirs_count
    }

    /// Whether this group's on-disk block bitmap is uninitialized and must be
    /// reconstructed from the group layout (`EXT4_BG_BLOCK_UNINIT`).
    pub(super) const fn is_block_uninit(&self) -> bool {
        self.flags & BG_BLOCK_UNINIT != 0
    }

    /// Whether this group's inode bitmap and table are uninitialized
    /// (`EXT4_BG_INODE_UNINIT`).
    pub(super) const fn is_inode_uninit(&self) -> bool {
        self.flags & BG_INODE_UNINIT != 0
    }
}

/// One block group's metadata: the descriptor, the block bitmap, and the inode
/// bitmap.
///
/// All three members carry dirty tracking; writeback is deferred to
/// [`BlockGroup::sync_metadata`].
pub(super) struct BlockGroupMetadata {
    /// Group descriptor with dirty tracking.
    pub desc: Dirty<BlockGroupDesc>,
    /// Block bitmap cached in memory.
    pub block_bitmap: Dirty<IdBitmap>,
    /// Inode bitmap cached in memory.
    pub inode_bitmap: Dirty<IdBitmap>,
}

impl Debug for BlockGroupMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlockGroupMetadata")
            .field("desc", &self.desc)
            .field("block_bitmap_dirty", &self.block_bitmap.is_dirty())
            .field("inode_bitmap_dirty", &self.inode_bitmap.is_dirty())
            .finish()
    }
}

/// The per-group allocation policy for one ring pass (P9b-b2): the halved
/// first-fit rescans this replaces gave a nearly-full goal group a 1-block
/// fragment instead of moving on — the 045 verdict's 1360 length-1 extents.
pub(super) enum AllocPolicy {
    /// First pass: the whole (group-clamped) run or nothing. `goal_offset`
    /// (the goal's group-local bit when the goal lands here) seeds a
    /// goal-directed first fit before the group-head scan — Linux
    /// `ext4_mb_find_by_goal`'s shape without the buddy machinery.
    FullRunOnly {
        /// Group-local bit of the caller's goal, `None` off-goal-group.
        goal_offset: Option<u16>,
    },
    /// Second pass: the longest free run available (early-stop at the
    /// request) — the fallback that takes the best piece instead of
    /// hammering one group with halved rescans.
    BestEffort,
}

/// The outcome of one group's block-allocation attempt
/// ([`BlockGroup::alloc_blocks`]).
pub(super) enum GroupBlockAlloc {
    /// The allocated run, in filesystem-wide block numbers.
    Allocated(Range<Ext4Bid>),
    /// Nothing fit. `pinned_in_group` reports whether freed-but-uncommitted
    /// pins covered free bits here — the caller's evidence for the transient
    /// "everything free awaits a commit" `ENOSPC` leg, kept apart from truly
    /// out of space.
    NoFit {
        /// Whether pinned freed runs overlapped this group.
        pinned_in_group: bool,
    },
}

/// A block group's allocation domain.
///
/// Owns the cached block bitmap, inode bitmap, and group descriptor behind a
/// single lock, plus the geometry needed to allocate/free blocks and inodes and
/// write metadata back to disk.
pub(super) struct BlockGroup {
    /// Block group index (0-based).
    group_idx: usize,
    /// Group descriptor, block bitmap, and inode bitmap, protected by a single
    /// lock.
    metadata: RwMutex<BlockGroupMetadata>,
    /// Backing block device (shared with `Ext4` and other groups).
    block_device: Arc<dyn BlockDevice>,
    /// Cached geometry: first filesystem-wide block number of this group.
    first_block: Ext4Bid,
    /// Cached geometry: last filesystem-wide block number of this group.
    last_block: Ext4Bid,
    /// Cached geometry: inode table blocks per group.
    nr_inode_table_blocks_per_group: u32,
    /// Cached geometry: inodes per group.
    nr_inodes_per_group: u32,
    /// Cached geometry: blocks per group (the nominal `s_blocks_per_group`, used
    /// as the fixed block-bitmap checksum length, `blocks_per_group / 8`).
    nr_blocks_per_group: u32,
    /// Cached geometry: inode size in bytes.
    inode_size: usize,
    /// Cached geometry: filesystem block size in bytes.
    block_size: usize,
    /// Cached geometry: on-disk group-descriptor size in bytes (32 or 64). Drives
    /// the GDT stride and selects the 64-byte high-half decode on `reload`.
    desc_size: u16,
    /// Absolute byte offset of this group's descriptor in the GDT (strided by
    /// [`Self::desc_size`], so it points at the 32-byte low half of a 64-byte
    /// descriptor).
    desc_offset: usize,
    /// The per-filesystem crc32c seed when `metadata_csum` is on, else `None`
    /// (checksums are a no-op). Cached from the superblock at load so `reload`
    /// and the inode read path can verify without holding a `SuperBlock`.
    csum_seed: Option<FsCsumSeed>,
    /// Per-group live inode cache keyed by group-local inode index.
    ///
    /// Ext4 keeps this cache locally because the VFS layer does not provide a
    /// shared inode cache for filesystem implementations. It gives inodes a
    /// stable identity and lets the filesystem enumerate every dirty inode for a
    /// consistent flush at sync/unmount time.
    inode_cache: RwMutex<BTreeMap<u16, Arc<Inode>>>,
}

impl Debug for BlockGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlockGroup")
            .field("group_idx", &self.group_idx)
            .finish()
    }
}

impl BlockGroup {
    /// Loads a block group from the descriptor table.
    ///
    /// Reads and decodes the group's descriptor at `gdt_base_offset + group_idx *
    /// sb.desc_size()` (32 or 64 bytes wide per the `64BIT` feature), caches the
    /// group's geometry from `sb`, and loads the block bitmap.
    ///
    /// Loading is lenient: strict validation that the system-metadata blocks are
    /// marked allocated in the bitmap is deferred (the read-only fixtures carry
    /// an all-zero bitmap and must still mount).
    pub(super) fn load(
        device: Arc<dyn BlockDevice>,
        group_idx: usize,
        sb: &SuperBlock,
        gdt_base_offset: usize,
    ) -> Result<Self> {
        let desc_size = sb.desc_size();
        let desc_offset = gdt_base_offset + group_idx * desc_size as usize;
        let csum_seed = sb.has_metadata_csum().then(|| sb.metadata_csum_seed());
        let desc = Self::read_desc(
            device.as_ref(),
            desc_offset,
            desc_size,
            group_idx as u32,
            csum_seed,
        )?;

        // Cache geometry from `SuperBlock` at load time.
        let nr_blocks_per_group = sb.nr_blocks_per_group() as Ext4Bid;
        let first_block = sb.first_data_block() + (group_idx as Ext4Bid) * nr_blocks_per_group;
        let nr_block_groups = sb.nr_block_groups() as usize;
        let last_block = if group_idx == nr_block_groups - 1 {
            sb.total_blocks() - 1
        } else {
            first_block + nr_blocks_per_group - 1
        };
        let nr_inode_table_blocks_per_group = sb.nr_inode_table_blocks_per_group();
        let nr_inodes_per_group = sb.nr_inodes_per_group();
        let nr_blocks_per_group = sb.nr_blocks_per_group();
        let inode_size = sb.inode_size();
        let block_size = sb.block_size();

        // Load the block bitmap and the inode bitmap.
        let block_bitmap =
            Self::load_block_bitmap(device.as_ref(), first_block, last_block, &desc)?;
        let inode_bitmap = Self::load_inode_bitmap(device.as_ref(), nr_inodes_per_group, &desc)?;

        Ok(Self {
            group_idx,
            metadata: RwMutex::new(BlockGroupMetadata {
                desc: Dirty::new(desc),
                block_bitmap: Dirty::new(block_bitmap),
                inode_bitmap: Dirty::new(inode_bitmap),
            }),
            block_device: device,
            first_block,
            last_block,
            nr_inode_table_blocks_per_group,
            nr_inodes_per_group,
            nr_blocks_per_group,
            inode_size,
            block_size,
            desc_size,
            desc_offset,
            csum_seed,
            inode_cache: RwMutex::new(BTreeMap::new()),
        })
    }

    /// Reads and decodes this group's descriptor from `device` at `desc_offset`,
    /// sized by `desc_size`: the 64-byte `64BIT` layout — splicing the block-number
    /// high halves via [`BlockGroupDesc::from_raw`] — when `desc_size` is 64, else
    /// the classic 32-byte layout. The single decode path shared by [`Self::load`]
    /// and [`Self::reload_metadata`], so the high-half splice lives at one boundary.
    /// `group` (0-based index) and `csum_seed` drive the `metadata_csum`
    /// verify-on-read: when `csum_seed` is `Some`, the stored `bg_checksum` is
    /// checked against a recomputation over this group's descriptor before the
    /// decode is trusted (`EUCLEAN` on mismatch); when `None` the descriptor is
    /// decoded as in Phases 1–5.
    fn read_desc(
        device: &dyn BlockDevice,
        desc_offset: usize,
        desc_size: u16,
        group: u32,
        csum_seed: Option<FsCsumSeed>,
    ) -> Result<BlockGroupDesc> {
        if desc_size as usize >= size_of::<RawBlockGroup64>() {
            let raw = device
                .read_val::<RawBlockGroup64>(desc_offset)
                .map_err(|_| Error::with_message(Errno::EIO, "failed to read group descriptor"))?;
            if let Some(seed) = csum_seed {
                BlockGroupDesc::verify_group_desc_checksum(&raw.lo, Some(&raw.hi), group, seed)?;
            }
            Ok(BlockGroupDesc::from_raw(&raw.lo, Some(&raw.hi)))
        } else {
            let raw = device
                .read_val::<RawBlockGroup>(desc_offset)
                .map_err(|_| Error::with_message(Errno::EIO, "failed to read group descriptor"))?;
            if let Some(seed) = csum_seed {
                BlockGroupDesc::verify_group_desc_checksum(&raw, None, group, seed)?;
            }
            Ok(BlockGroupDesc::from_raw(&raw, None))
        }
    }

    /// Re-reads this group's descriptor and both bitmaps from the device,
    /// replacing the cached copies.
    ///
    /// Used once per mount, right after journal replay: the cached copies were
    /// parsed from the pre-replay device, and replay rewrote their on-disk
    /// locations. Without the reload the allocators would work off stale
    /// bitmaps (re-handing out blocks/inodes the replayed transactions
    /// allocated — cross-links) and the next sync would write stale counters
    /// back over the replayed values. The group's cached geometry is immutable
    /// and untouched; the inode cache is empty this early in the mount.
    pub(super) fn reload_metadata(&self, device: &dyn BlockDevice) -> Result<()> {
        let desc = Self::read_desc(
            device,
            self.desc_offset,
            self.desc_size,
            self.group_idx as u32,
            self.csum_seed,
        )?;
        let block_bitmap =
            Self::load_block_bitmap(device, self.first_block, self.last_block, &desc)?;
        let inode_bitmap = Self::load_inode_bitmap(device, self.nr_inodes_per_group, &desc)?;

        let mut metadata = self.metadata.write();
        metadata.desc = Dirty::new(desc);
        metadata.block_bitmap = Dirty::new(block_bitmap);
        metadata.inode_bitmap = Dirty::new(inode_bitmap);
        Ok(())
    }

    /// Returns the starting block of this group's inode table.
    pub(super) fn inode_table_bid(&self) -> Ext4Bid {
        self.metadata.read().desc.inode_table_bid()
    }

    /// Looks up an inode by inode number through this group's inode cache.
    ///
    /// Returns the same `Arc<Inode>` for repeated lookups of one inode number,
    /// so concurrent users share one in-memory inode (and one set of dirty
    /// state). The fast path hits the cache under the read lock; the slow path
    /// promotes to the write lock, re-checks (another thread may have inserted
    /// the inode in the gap), then loads the descriptor from disk and inserts it.
    pub(super) fn lookup_inode(
        &self,
        ino: Ext4Ino,
        fs: Weak<Ext4>,
        journal: Option<&journal::Journal>,
    ) -> Result<Arc<Inode>> {
        let inode_idx = self.inode_idx_in_group(ino);

        // Fast path: cache hit under the read lock.
        if let Some(inode) = self.inode_cache.read().get(&inode_idx) {
            return Ok(inode.clone());
        }

        // Slow path: revalidate under the write lock, since another thread may
        // have inserted the inode between the read and write lock acquisition.
        let mut inode_cache = self.inode_cache.write();
        if let Some(inode) = inode_cache.get(&inode_idx) {
            return Ok(inode.clone());
        }

        let desc = self.read_inode_desc(ino, journal)?;
        let type_ = desc.type_();
        let inode = Inode::new(ino, type_, Dirty::new(desc), self.group_idx, fs)?;
        inode_cache.insert(inode_idx, inode.clone());
        Ok(inode)
    }

    /// Inserts a newly created inode into this group's live cache.
    pub(super) fn insert_inode(&self, inode: Arc<Inode>) {
        let inode_idx = self.inode_idx_in_group(inode.ino());
        self.inode_cache.write().insert(inode_idx, inode);
    }

    /// Removes one inode from this group's live cache.
    pub(super) fn remove_inode(&self, ino: Ext4Ino) -> Option<Arc<Inode>> {
        let inode_idx = self.inode_idx_in_group(ino);
        self.inode_cache.write().remove(&inode_idx)
    }

    /// Flushes every cached inode's data pages and metadata back to disk.
    ///
    /// The `Arc<Inode>` handles are cloned out under the read lock, which is then
    /// dropped *before* any inode is synced. This drop-before-sync ordering is
    /// required: `Inode::sync_data_and_meta` acquires `inner.write()`, so holding
    /// `inode_cache.read()` across the sync would invert the lock order against
    /// create/unlink paths that take `inner.write()` first and `inode_cache`
    /// after.
    pub(super) fn sync_inodes(&self) -> Result<()> {
        let inodes: Vec<Arc<Inode>> = self.inode_cache.read().values().cloned().collect();
        for inode in inodes {
            inode.sync_data_and_meta_no_barrier()?;
        }
        Ok(())
    }

    /// Recomputes this group's `metadata_csum` fields — the two bitmap
    /// checksums (from the final in-memory bitmaps) and `bg_checksum` — into a
    /// decoded descriptor. A no-op when the feature is off.
    ///
    /// This is the compute half of `metadata_csum` on the descriptor: it runs at
    /// every descriptor patch/writeback funnel (the journaled `patch_into` path
    /// and the direct `sync_metadata` path), so whichever operation last touches
    /// this group's descriptor in a transaction leaves it self-consistent. The
    /// bitmap checksums are folded from the in-memory bitmaps, which are the
    /// exact bytes written to the bitmap blocks in the same transaction, so the
    /// stored `*_bitmap_csum` always matches the on-disk bitmap; because both are
    /// captured in that one transaction, the cross-block dependency is atomic
    /// (P6b red line 2).
    fn stamp_desc_csum(
        &self,
        lo: &mut RawBlockGroup,
        hi: Option<&mut RawBlockGroupHi>,
        metadata: &BlockGroupMetadata,
    ) {
        let Some(seed) = self.csum_seed else {
            return;
        };
        // Linux uses a fixed checksum length: `blocks_per_group / 8` for the
        // block bitmap and `ceil(inodes_per_group / 8)` for the inode bitmap.
        let bb_len = (self.nr_blocks_per_group / 8) as usize;
        let ib_len = (self.nr_inodes_per_group as usize).div_ceil(8);
        let bb_csum =
            BlockGroupDesc::bitmap_checksum(seed, metadata.block_bitmap.as_bytes(), bb_len);
        let ib_csum =
            BlockGroupDesc::bitmap_checksum(seed, metadata.inode_bitmap.as_bytes(), ib_len);
        BlockGroupDesc::stamp_checksums(lo, hi, self.group_idx as u32, seed, bb_csum, ib_csum);
    }

    /// Stamps this group's `metadata_csum` fields into the descriptor block image
    /// `block` (the journaled after-image), at this group's `desc_offset` within
    /// it. Decodes the low half (and, for a 64-byte descriptor, the high tail),
    /// stamps via [`Self::stamp_desc_csum`], and writes them back. A no-op when
    /// the feature is off.
    fn stamp_desc_csum_in_block(&self, block: &mut [u8], metadata: &BlockGroupMetadata) {
        if self.csum_seed.is_none() {
            return;
        }
        let off = self.desc_offset % BLOCK_SIZE;
        let mut lo = RawBlockGroup::from_bytes(&block[off..off + size_of::<RawBlockGroup>()]);
        if self.desc_size as usize >= size_of::<RawBlockGroup64>() {
            let hi_start = off + size_of::<RawBlockGroup>();
            let mut hi = RawBlockGroupHi::from_bytes(
                &block[hi_start..hi_start + size_of::<RawBlockGroupHi>()],
            );
            self.stamp_desc_csum(&mut lo, Some(&mut hi), metadata);
            block[off..off + size_of::<RawBlockGroup>()].copy_from_slice(lo.as_bytes());
            block[hi_start..hi_start + size_of::<RawBlockGroupHi>()].copy_from_slice(hi.as_bytes());
        } else {
            self.stamp_desc_csum(&mut lo, None, metadata);
            block[off..off + size_of::<RawBlockGroup>()].copy_from_slice(lo.as_bytes());
        }
    }

    /// Loads and decodes an inode's on-disk descriptor from the inode table.
    ///
    /// The inode-table block is read through [`journal::read_metadata_block`],
    /// the WAL-suppressing funnel (newest wins: a running capture, then the
    /// committed-but-un-checkpointed retained image, then the device). On a
    /// journaled volume the device lags the log between an operation's commit
    /// and its checkpoint; a bare device read of this block in that window would
    /// decode a neighbor inode's stale slot. Routing through the funnel makes a
    /// cache-miss reload see the committed image structurally — the residual
    /// `extent-leaf-stale-read-window` for the inode table (the live `Arc` a
    /// pinned inode holds in the cache was the only prior guard; this closes the
    /// read path itself). `journal` is `None` on a non-journaled volume, where
    /// the device is authoritative and the funnel reads it directly.
    ///
    /// The fixed `size_of::<RawInode>()` (256-byte) read window never runs past
    /// the block: mount admission requires a power-of-two `s_inode_size` that both
    /// divides the 4 KiB block and is at least `size_of::<RawInode>()`
    /// (`SuperBlock::try_from`), so each inode's slot is block-aligned and no
    /// smaller than the window — `off_in_block + size_of::<RawInode>()` stays
    /// within the one metadata block that holds the descriptor.
    pub(super) fn read_inode_desc(
        &self,
        ino: Ext4Ino,
        journal: Option<&journal::Journal>,
    ) -> Result<InodeDesc> {
        let idx_in_group = self.inode_idx_in_group(ino) as usize;
        let byte_off = idx_in_group * self.inode_size;
        let block_bid = self.inode_table_bid() + (byte_off / self.block_size) as Ext4Bid;
        let off_in_block = byte_off % self.block_size;
        let block = journal::read_metadata_block(journal, self.block_device.as_ref(), block_bid)?;
        let raw = RawInode::from_bytes(&block[off_in_block..off_in_block + size_of::<RawInode>()]);
        if let Some(seed) = self.csum_seed {
            InodeDesc::verify_inode_checksum(
                &raw,
                seed.derive_inode(ino, raw.generation),
                self.inode_size,
            )?;
        }
        InodeDesc::try_from(&raw)
    }

    /// Returns whether `ino` is marked allocated in this group's inode bitmap.
    ///
    /// Used by the reclaim path (`Inode::try_reclaim_deleted_inode`) to avoid
    /// double-freeing an inode whose bitmap bit is already clear. Mirrors ext2
    /// `BlockGroup::is_inode_allocated`.
    pub(super) fn is_inode_allocated(&self, ino: Ext4Ino) -> bool {
        let inode_idx = self.inode_idx_in_group(ino);
        self.metadata.read().inode_bitmap.is_allocated(inode_idx)
    }

    /// Returns the 0-based group-local inode index for `ino`.
    fn inode_idx_in_group(&self, ino: Ext4Ino) -> u16 {
        debug_assert!(ino > 0);
        debug_assert_eq!(
            ((ino - 1) / self.nr_inodes_per_group) as usize,
            self.group_idx
        );
        ((ino - 1) % self.nr_inodes_per_group) as u16
    }

    /// Returns the first filesystem-wide block number of this group.
    pub(super) fn first_block(&self) -> Ext4Bid {
        self.first_block
    }

    /// Returns the last filesystem-wide block number of this group.
    pub(super) fn last_block(&self) -> Ext4Bid {
        self.last_block
    }

    /// Returns the number of free blocks in this group.
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn free_blocks_count(&self) -> u32 {
        self.metadata.read().desc.free_blocks_count()
    }

    /// The device block holding this group's block bitmap. Test-only inspection
    /// for the free-count WAL-ordering assertion.
    #[cfg(ktest)]
    pub(super) fn block_bitmap_bid_for_test(&self) -> Ext4Bid {
        self.metadata.read().desc.block_bitmap_bid()
    }

    /// The device block holding this group's inode bitmap. Test-only inspection
    /// for the free-count WAL-ordering assertion.
    #[cfg(ktest)]
    pub(super) fn inode_bitmap_bid_for_test(&self) -> Ext4Bid {
        self.metadata.read().desc.inode_bitmap_bid()
    }

    /// The device block holding this group's on-disk descriptor — the block a
    /// per-op alloc/free journals its `bg_free_*` count word (and `bg_checksum`)
    /// into, alongside the bitmap. Test-only inspection for the free-count
    /// WAL-ordering assertion.
    #[cfg(ktest)]
    pub(super) fn desc_block_bid_for_test(&self) -> Ext4Bid {
        Ext4Bid::try_from(self.desc_offset / BLOCK_SIZE)
            .expect("descriptor block index fits Ext4Bid")
    }

    /// Returns the number of free inodes in this group.
    #[cfg(ktest)]
    pub(super) fn free_inodes_count(&self) -> u32 {
        self.metadata.read().desc.free_inodes_count()
    }

    /// Returns the number of in-use directory inodes in this group.
    #[cfg(ktest)]
    pub(super) fn used_dirs_count(&self) -> u32 {
        self.metadata.read().desc.used_dirs_count()
    }

    /// Returns a read guard over the combined group metadata.
    #[cfg(ktest)]
    pub(super) fn metadata(&self) -> RwMutexReadGuard<'_, BlockGroupMetadata> {
        self.metadata.read()
    }

    /// Overwrites the in-memory free-block counter — the corrupted-counter
    /// fixture that makes [`free_blocks`](Self::free_blocks)' count-overflow
    /// arm deterministically reachable (a post-discharge failure arm).
    #[cfg(ktest)]
    pub(super) fn corrupt_free_blocks_count_for_test(&self, count: u32) {
        self.metadata.write().desc.free_blocks_count = count;
    }

    /// Attempts to allocate up to `count` contiguous blocks within this group,
    /// skipping the pinned freed runs (`pinned_frees`, filesystem-wide
    /// `(start, count)` pairs from
    /// [`Journal::pinned_frees_snapshot`](super::journal): blocks freed under
    /// a transaction that has not committed yet, clear in the bitmap but not
    /// yet allocatable).
    ///
    /// Returns [`GroupBlockAlloc::Allocated`] with filesystem-wide block
    /// numbers on success, [`GroupBlockAlloc::NoFit`] if the group has no
    /// allocatable blocks (reporting whether pins covered free bits here).
    /// Returns `Err(EIO)` on bitmap/counter corruption.
    pub(super) fn alloc_blocks(
        &self,
        count: u32,
        sb_free_blocks: u64,
        pinned_frees: &[(Ext4Bid, u32)],
        policy: AllocPolicy,
        handle: Option<&journal::Handle>,
    ) -> Result<GroupBlockAlloc> {
        let group_size = (self.last_block - self.first_block + 1) as u32;
        debug_assert!(group_size <= IdBitmap::capacity() as u32);

        // This group's slice of the pinned freed runs, as group-local bit
        // ranges. Pinned blocks are free in the bitmap (their free already
        // cleared the bits); only the allocator must pretend otherwise, and
        // only until the freeing transaction commits. The `as u16` casts are
        // bounded by the clamp: both offsets are at most `group_size`, which
        // the assert above bounds by the bitmap capacity (32768).
        let mut pinned_bits: Vec<Range<u16>> = Vec::new();
        for &(run_start, run_len) in pinned_frees {
            let lo = run_start.max(self.first_block);
            let hi = (run_start + run_len as Ext4Bid).min(self.last_block + 1);
            if lo < hi {
                pinned_bits.push((lo - self.first_block) as u16..(hi - self.first_block) as u16);
            }
        }

        let mut metadata = self.metadata.write();

        // Cheap prechecks BEFORE the journal capture: a ring pass visits many
        // groups, and `get_write_access` charges the transaction per capture —
        // a group that cannot possibly fit must answer `NoFit` without
        // touching the journal.
        let precheck_fits = match policy {
            AllocPolicy::FullRunOnly { .. } => {
                let want = count.min(group_size) as u64;
                (metadata.desc.free_blocks_count() as u64) >= want && sb_free_blocks >= want
            }
            AllocPolicy::BestEffort => metadata.desc.free_blocks_count() > 0 && sb_free_blocks > 0,
        };
        if !precheck_fits {
            return Ok(GroupBlockAlloc::NoFit {
                pinned_in_group: !pinned_bits.is_empty(),
            });
        }

        // Hold (mark allocated) every pinned bit BEFORE the scan, so the
        // policy scans below run exactly once over a bitmap where "free"
        // means "allocatable" — Linux's "mark the pending frees used when
        // generating the buddy" (mballoc.c `ext4_mb_generate_from_freelist`).
        // Every hold is released below, before the bitmap is read or
        // serialized. The retired shape — scan, test the candidate against
        // the pin list, hold the overlap, rescan — restarted the scan from
        // its hint after every hold: O(pinned²) per call under generic/371's
        // rm churn (tens of thousands of pinned bits), which stretched even
        // a single-block allocation to tens of milliseconds UNDER THE
        // SUPERBLOCK WRITE LOCK and starved the pwrite side's
        // pinned-`ENOSPC` retries into a surfaced `ENOSPC` (the freed-space
        // windows its commit-and-retry protocol opened were always consumed
        // by the concurrent fallocate loop first). Pre-holding is O(pinned)
        // once, and each policy scan is one linear pass.
        let mut pinned_held: Vec<Range<u16>> = Vec::new();
        for pin in &pinned_bits {
            if let Some(held) = metadata
                .block_bitmap
                .alloc_exact_at(pin.start, pin.end - pin.start)
            {
                pinned_held.push(held);
            } else {
                // A pin names free bits (its free cleared them, and the
                // allocator refuses pinned bits), but pin entries can
                // overlap each other transiently; hold each still-free bit
                // individually so coverage stays exact without double-holds.
                for bit in pin.clone() {
                    if let Some(held) = metadata.block_bitmap.alloc_exact_at(bit, 1) {
                        pinned_held.push(held);
                    }
                }
            }
        }
        let allocated_range = match policy {
            AllocPolicy::FullRunOnly { goal_offset } => {
                // The whole (group-clamped) run or nothing: no halving, no
                // silent shrink to the free count (the precheck above already
                // excluded a group that cannot hold it). Goal-directed first
                // (runs at or after the goal bit), then the group head — the
                // head scan also covers the below-goal runs the first pass
                // skipped.
                let want = count.min(group_size) as u16;
                let mut hints = goal_offset.into_iter().chain(core::iter::once(0));
                hints.find_map(|hint| metadata.block_bitmap.alloc_consecutive_from(hint, want))
            }
            AllocPolicy::BestEffort => {
                // The longest run available, early-stopping at the (fully
                // clamped) request.
                let cap = count
                    .min(group_size)
                    .min(metadata.desc.free_blocks_count())
                    .min(sb_free_blocks.min(u32::MAX as u64) as u32)
                    as u16;
                metadata.block_bitmap.alloc_longest_run(cap)
            }
        };
        // The pinned bits must stay clear everywhere but inside this scan: the
        // capture patch below serializes the whole bitmap, so a hold leaking
        // past this point would journal freed blocks as allocated.
        for held in pinned_held {
            metadata.block_bitmap.free_consecutive(held);
        }

        let Some(range) = allocated_range else {
            let pinned_in_group = !pinned_bits.is_empty();
            // The corruption heuristic ("free count > 0 yet not one block
            // allocatable") belongs to the exhaustive pass only: a
            // `FullRunOnly` NoFit is the normal ring-forward answer, and with
            // pins in the group the mismatch is the expected transient.
            if matches!(policy, AllocPolicy::BestEffort)
                && metadata.desc.free_blocks_count() > 0
                && !pinned_in_group
            {
                return_errno_with_message!(Errno::EIO, "block bitmap corruption detected");
            }
            return Ok(GroupBlockAlloc::NoFit { pinned_in_group });
        };

        let range_start = range.start as Ext4Bid;
        let alloc_count = range.len() as u32;

        // The journal capture happens only for a group that actually
        // allocates: a ring pass visits many groups, and capturing every
        // scanned-but-empty-handed bitmap would both waste credits (each
        // fresh capture charges the transaction — a fragmented disk could
        // trip the capacity backstop with space still available) and write
        // untouched bitmaps into the log. On a capture failure the in-memory
        // allocation rolls back; nothing was journaled.
        let block_bitmap_bid = metadata.desc.block_bitmap_bid();
        let bitmap_access = match journal::get_write_access(handle, block_bitmap_bid) {
            Ok(access) => access,
            Err(err) => {
                metadata.block_bitmap.free_consecutive(range);
                return Err(err);
            }
        };

        let abs_range = (self.first_block + range_start)
            ..(self.first_block + range_start + alloc_count as Ext4Bid);
        if self.overlaps_system_zone_with(&metadata.desc, abs_range)
            || metadata.desc.free_blocks_count() < alloc_count
            || sb_free_blocks < alloc_count as u64
        {
            metadata.block_bitmap.free_consecutive(range);
            return_errno_with_message!(Errno::EIO, "block bitmap corruption detected");
        }

        let new_free = metadata.desc.free_blocks_count() - alloc_count;
        metadata.desc.free_blocks_count = new_free;
        // First data allocation into a lazily-initialized group: the reconstructed
        // in-memory bitmap is now authoritative, so drop BLOCK_UNINIT (persisted by
        // `patch_into` below). A later mount must then read the real bitmap rather
        // than re-reconstruct a prefix over blocks we have already handed out.
        if metadata.desc.is_block_uninit() {
            metadata.desc.flags &= !BG_BLOCK_UNINIT;
        }

        let desc_block_bid = (self.desc_offset / BLOCK_SIZE) as Ext4Bid;
        bitmap_access.patch(|buf| buf.copy_from_slice(metadata.block_bitmap.as_bytes()))?;
        journal::get_write_access(handle, desc_block_bid)?.patch(|buf| {
            metadata.desc.patch_into(buf, self.desc_offset);
            self.stamp_desc_csum_in_block(buf, &metadata);
        })?;

        let range_start_block = self.first_block + range.start as Ext4Bid;
        let range_end_block = self.first_block + range.end as Ext4Bid;
        Ok(GroupBlockAlloc::Allocated(
            range_start_block..range_end_block,
        ))
    }

    /// Frees a contiguous range of group-relative block bits.
    ///
    /// Returns the number of blocks actually freed (allocated-to-free
    /// transitions). Returns `Err(EIO)` when the range overlaps the group's
    /// system zone — **after aborting the journal** (under a live handle):
    /// see the comment at the refusal. Every `Err` out of this function is
    /// equally fatal under a handle: the caller (`Ext4::free_blocks`) has
    /// already discharged the run's revoke duty and aborts the journal on
    /// ANY post-discharge failure (discharge and the bitmap free are
    /// atomic-or-dead), so a new failure arm added here needs no local
    /// abort.
    pub(super) fn free_blocks(
        &self,
        bit_range: Range<u32>,
        handle: Option<&journal::Handle>,
    ) -> Result<u32> {
        let start_bit = bit_range.start;
        let group_count = bit_range.len() as u32;
        // Validate system zone overlap using filesystem-wide coordinates.
        let abs_range = (self.first_block + start_bit as Ext4Bid)
            ..(self.first_block + bit_range.end as Ext4Bid);

        let mut metadata = self.metadata.write();

        if self.overlaps_system_zone_with(&metadata.desc, abs_range) {
            // A free targeting the system zone is filesystem corruption
            // (Linux refuses it through `ext4_error`, whose errors=journal
            // shape aborts the journal). A plain error return is NOT enough
            // under a handle (P7b-2 re-verification finding): the caller
            // (`Ext4::free_blocks`) discharged this run's revoke duty BEFORE
            // calling here, so the journal already holds a revoke record —
            // and a cancelled capture — for blocks that were never freed and
            // are still referenced; left standing, that revoke would
            // suppress the blocks' legitimate log images at every later
            // checkpoint/replay (silent rollback). The effects cannot be
            // unwound (the revoke may have evicted a retained image), so
            // abort loudly: an aborted journal accepts no further work, and
            // the poisoned records can never act. The caller's atomic-or-dead
            // wrap aborts on this `Err` too; this local abort stays as the
            // corruption site's own response (double abort is idempotent —
            // it stores a flag and wakes sleepers).
            if let Some(handle) = handle {
                handle.abort_journal_on_fs_error();
            }
            return_errno_with_message!(Errno::EIO, "freeing blocks in system zone");
        }

        let block_bitmap_bid = metadata.desc.block_bitmap_bid();
        let bitmap_access = journal::get_write_access(handle, block_bitmap_bid)?;

        // Clear bits one by one and count only allocated-to-free transitions.
        let range_start = start_bit as u16;
        let range_end = (start_bit + group_count) as u16;
        let mut actually_freed: u32 = 0;
        for block_bit in range_start..range_end {
            if !metadata.block_bitmap.is_allocated(block_bit) {
                warn!(
                    "free_blocks: bit already cleared for block {}",
                    self.first_block + start_bit as Ext4Bid + (block_bit - range_start) as Ext4Bid
                );
            } else {
                metadata.block_bitmap.free(block_bit);
                actually_freed += 1;
            }
        }

        let new_free = metadata
            .desc
            .free_blocks_count()
            .checked_add(actually_freed)
            .ok_or_else(|| Error::with_message(Errno::EIO, "free block count overflow in group"))?;
        metadata.desc.free_blocks_count = new_free;

        let desc_block_bid = (self.desc_offset / BLOCK_SIZE) as Ext4Bid;
        bitmap_access.patch(|buf| buf.copy_from_slice(metadata.block_bitmap.as_bytes()))?;
        journal::get_write_access(handle, desc_block_bid)?.patch(|buf| {
            metadata.desc.patch_into(buf, self.desc_offset);
            self.stamp_desc_csum_in_block(buf, &metadata);
        })?;

        Ok(actually_freed)
    }

    /// Attempts to allocate one inode within this group.
    ///
    /// Allocates a single free bit in the inode bitmap, decrements the group's
    /// free-inode counter, and (for directories) increments `used_dirs_count`.
    /// Returns `Some(inode_idx)` with the 0-based group-local inode index, or
    /// `None` if this group has no free inode. Mirrors [`alloc_blocks`] on the
    /// block side, threading the same journal seam.
    pub(super) fn alloc_ino(
        &self,
        type_: InodeType,
        handle: Option<&journal::Handle>,
    ) -> Result<Option<u32>> {
        let mut metadata = self.metadata.write();

        if metadata.desc.free_inodes_count() == 0 {
            return Ok(None);
        }
        // Skip a group whose inode table is uninitialized. Lazy inode-table init
        // (zeroing the table + maintaining `itable_unused`) is not yet
        // implemented; clearing INODE_UNINIT without it would expose an
        // unzeroed table of garbage inodes to e2fsck. Treating the group as
        // "no free inode here" confines allocation to initialized groups (on a
        // fresh image, effectively group 0's flex until it fills) — a capacity
        // limit, never corruption. Full support is a later phase.
        if metadata.desc.is_inode_uninit() {
            return Ok(None);
        }
        if type_.is_directory() && metadata.desc.used_dirs_count() == u32::from(u16::MAX) {
            return_errno_with_message!(Errno::EIO, "group used directory counter overflow");
        }

        let inode_bitmap_bid = metadata.desc.inode_bitmap_bid();
        let bitmap_access = journal::get_write_access(handle, inode_bitmap_bid)?;

        // Allocate exactly one free inode bit.
        let Some(range) = metadata.inode_bitmap.alloc_consecutive(1) else {
            // The counter said there was a free inode but the bitmap had none.
            return_errno_with_message!(Errno::EIO, "inode bitmap corruption detected");
        };
        let inode_idx = range.start as u32;

        metadata.desc.free_inodes_count = metadata.desc.free_inodes_count() - 1;
        if type_.is_directory() {
            metadata.desc.used_dirs_count = metadata.desc.used_dirs_count() + 1;
        }
        // Keep `bg_itable_unused` in step with the bitmap so e2fsck does not
        // flag a stale count on a metadata_csum volume. Feature-gated: without
        // `metadata_csum`/`gdt_csum` the field must stay 0 — e2fsck reads a
        // nonzero count on such a volume as "group descriptor marked
        // uninitialized without feature set" (ext4/045 caught exactly this on
        // a featureless guest-mkfs scratch).
        if self.csum_seed.is_some() {
            metadata.desc.itable_unused =
                Self::itable_unused_from_bitmap(&metadata.inode_bitmap, self.nr_inodes_per_group);
        }

        let desc_block_bid = (self.desc_offset / BLOCK_SIZE) as Ext4Bid;
        bitmap_access.patch(|buf| buf.copy_from_slice(metadata.inode_bitmap.as_bytes()))?;
        journal::get_write_access(handle, desc_block_bid)?.patch(|buf| {
            metadata.desc.patch_into(buf, self.desc_offset);
            self.stamp_desc_csum_in_block(buf, &metadata);
        })?;

        Ok(Some(inode_idx))
    }

    /// Frees one inode within this group, by its group-local index.
    ///
    /// Clears the inode bitmap bit, increments the group's free-inode counter,
    /// and (for directories) decrements `used_dirs_count`. Returns `true` if the
    /// bit transitioned allocated-to-free, `false` if it was already clear (logs
    /// a warning, mirroring [`free_blocks`]).
    pub(super) fn free_inode(
        &self,
        group_local_idx: u32,
        type_: InodeType,
        handle: Option<&journal::Handle>,
    ) -> Result<bool> {
        let mut metadata = self.metadata.write();

        let inode_bit = group_local_idx as u16;
        if !metadata.inode_bitmap.is_allocated(inode_bit) {
            warn!(
                "free_inode: inode bit {} already cleared in group {}",
                group_local_idx, self.group_idx
            );
            return Ok(false);
        }

        let inode_bitmap_bid = metadata.desc.inode_bitmap_bid();
        let bitmap_access = journal::get_write_access(handle, inode_bitmap_bid)?;

        let new_free = metadata
            .desc
            .free_inodes_count()
            .checked_add(1)
            .ok_or_else(|| Error::with_message(Errno::EIO, "free inode count overflow in group"))?;
        let new_used_dirs = if type_.is_directory() {
            Some(
                metadata
                    .desc
                    .used_dirs_count()
                    .checked_sub(1)
                    .ok_or_else(|| {
                        Error::with_message(Errno::EIO, "used directory counter underflow in group")
                    })?,
            )
        } else {
            None
        };

        metadata.inode_bitmap.free(inode_bit);
        metadata.desc.free_inodes_count = new_free;
        if let Some(new_used_dirs) = new_used_dirs {
            metadata.desc.used_dirs_count = new_used_dirs;
        }
        // Feature-gated like the allocation side: stays 0 without the
        // checksum feature (see `try_alloc_inode`).
        if self.csum_seed.is_some() {
            metadata.desc.itable_unused =
                Self::itable_unused_from_bitmap(&metadata.inode_bitmap, self.nr_inodes_per_group);
        }

        let desc_block_bid = (self.desc_offset / BLOCK_SIZE) as Ext4Bid;
        bitmap_access.patch(|buf| buf.copy_from_slice(metadata.inode_bitmap.as_bytes()))?;
        journal::get_write_access(handle, desc_block_bid)?.patch(|buf| {
            metadata.desc.patch_into(buf, self.desc_offset);
            self.stamp_desc_csum_in_block(buf, &metadata);
        })?;

        Ok(true)
    }

    /// Writes dirty metadata back to disk under a single lock.
    ///
    /// Both bitmaps are written in full. The group descriptor is updated via
    /// read-modify-write: the raw descriptor is read, the mutated fields (the
    /// three counters plus `flags` and `itable_unused_lo`) are patched and
    /// `bg_checksum` restamped, and the result is written back so the `exclude`/
    /// reserved fields are preserved. For a 64-byte (`64BIT`)
    /// descriptor this reads and writes only the 32-byte low half at `desc_offset`,
    /// leaving the high-half tail intact (mirroring [`BlockGroupDesc::patch_into`]);
    /// the counters' high halves are structurally zero, so the low-half write is
    /// complete.
    pub(super) fn sync_metadata(&self) -> Result<()> {
        let mut metadata = self.metadata.write();

        if metadata.block_bitmap.is_dirty() {
            let block_bitmap_bid = metadata.desc.block_bitmap_bid();
            if self
                .block_device
                .write_bytes(
                    Bid::new(block_bitmap_bid).to_offset(),
                    metadata.block_bitmap.as_bytes(),
                )
                .is_err()
            {
                // Keep the dirty bit set on writeback failure for retry.
                return_errno_with_message!(Errno::EIO, "failed to write block bitmap");
            }
            metadata.block_bitmap.clear_dirty();
        }

        if metadata.inode_bitmap.is_dirty() {
            let inode_bitmap_bid = metadata.desc.inode_bitmap_bid();
            if self
                .block_device
                .write_bytes(
                    Bid::new(inode_bitmap_bid).to_offset(),
                    metadata.inode_bitmap.as_bytes(),
                )
                .is_err()
            {
                // Keep the dirty bit set on writeback failure for retry.
                return_errno_with_message!(Errno::EIO, "failed to write inode bitmap");
            }
            metadata.inode_bitmap.clear_dirty();
        }

        if metadata.desc.is_dirty() {
            // A 64-byte descriptor with checksums needs its high-tail csum fields
            // rewritten too, so read/write the whole 64 bytes; otherwise the
            // classic 32-byte low-half RMW (the high tail's other fields — block
            // number highs — are unchanged by a counter update).
            let wide =
                self.desc_size as usize >= size_of::<RawBlockGroup64>() && self.csum_seed.is_some();
            if wide {
                let mut raw = self
                    .block_device
                    .read_val::<RawBlockGroup64>(self.desc_offset)
                    .map_err(|_| {
                        Error::with_message(Errno::EIO, "failed to read group descriptor for sync")
                    })?;
                raw.lo.free_blocks_count_lo = metadata.desc.free_blocks_count() as u16;
                raw.lo.free_inodes_count_lo = metadata.desc.free_inodes_count() as u16;
                raw.lo.used_dirs_count_lo = metadata.desc.used_dirs_count() as u16;
                raw.lo.flags = metadata.desc.flags;
                raw.lo.itable_unused_lo = metadata.desc.itable_unused as u16;
                self.stamp_desc_csum(&mut raw.lo, Some(&mut raw.hi), &metadata);
                self.block_device
                    .write_val(self.desc_offset, &raw)
                    .map_err(|_| {
                        Error::with_message(Errno::EIO, "failed to write group descriptor")
                    })?;
            } else {
                let mut raw = self
                    .block_device
                    .read_val::<RawBlockGroup>(self.desc_offset)
                    .map_err(|_| {
                        Error::with_message(Errno::EIO, "failed to read group descriptor for sync")
                    })?;
                raw.free_blocks_count_lo = metadata.desc.free_blocks_count() as u16;
                raw.free_inodes_count_lo = metadata.desc.free_inodes_count() as u16;
                raw.used_dirs_count_lo = metadata.desc.used_dirs_count() as u16;
                raw.flags = metadata.desc.flags;
                raw.itable_unused_lo = metadata.desc.itable_unused as u16;
                self.stamp_desc_csum(&mut raw, None, &metadata);
                self.block_device
                    .write_val(self.desc_offset, &raw)
                    .map_err(|_| {
                        Error::with_message(Errno::EIO, "failed to write group descriptor")
                    })?;
            }
            metadata.desc.clear_dirty();
        }

        Ok(())
    }

    /// Loads the block bitmap for this group.
    fn load_block_bitmap(
        block_device: &dyn BlockDevice,
        first_block: Ext4Bid,
        last_block: Ext4Bid,
        desc: &BlockGroupDesc,
    ) -> Result<IdBitmap> {
        let group_size = (last_block - first_block + 1) as u32;
        let capacity = group_size as u16;
        debug_assert!(capacity as u32 == group_size && capacity <= IdBitmap::capacity());

        // A `BLOCK_UNINIT` group's on-disk block bitmap is not maintained, so the
        // raw block is meaningless (often all-zero) — trusting it would hand out
        // the group's backup superblock/GDT blocks as "free". Reconstruct the
        // bitmap from the group layout instead: such a group holds no data, so its
        // only used blocks are the fixed metadata/backup overhead at the group
        // start — a contiguous prefix whose length the descriptor's authoritative
        // free-block count gives us directly (Linux `ext4_init_block_bitmap`).
        if desc.is_block_uninit() {
            let overhead = group_size
                .checked_sub(desc.free_blocks_count())
                .ok_or_else(|| {
                    Error::with_message(
                        Errno::EUCLEAN,
                        "uninit group free count exceeds group size",
                    )
                })?;
            let mut bitmap = IdBitmap::from_buf(vec![0u8; BLOCK_SIZE].into_boxed_slice(), capacity);
            if overhead > 0 {
                // `capacity == group_size >= overhead`, so this cannot fail.
                bitmap.alloc_consecutive(overhead as u16);
            }
            // Safety net for the prefix assumption: any of this group's own
            // metadata blocks that fall within the group must lie inside the
            // reconstructed prefix. If one sits beyond it the layout is not the
            // prefix we assumed — refuse rather than risk handing that block out.
            for bid in [
                desc.block_bitmap_bid(),
                desc.inode_bitmap_bid(),
                desc.inode_table_bid(),
            ] {
                if (first_block..=last_block).contains(&bid)
                    && bid - first_block >= overhead as Ext4Bid
                {
                    return_errno_with_message!(
                        Errno::EUCLEAN,
                        "uninit group metadata block outside reconstructed prefix"
                    );
                }
            }
            return Ok(bitmap);
        }

        let bitmap_bid = desc.block_bitmap_bid();
        let mut buf = vec![0u8; BLOCK_SIZE];
        if block_device
            .read_bytes(Bid::new(bitmap_bid).to_offset(), &mut buf)
            .is_err()
        {
            return_errno_with_message!(Errno::EIO, "failed to read block bitmap");
        }

        Ok(IdBitmap::from_buf(buf.into_boxed_slice(), capacity))
    }

    /// Loads the inode bitmap for this group.
    ///
    /// The bitmap's logical capacity is the number of inodes per group, capped
    /// at the bitmap's physical capacity (a single block always holds at least
    /// as many bits as inodes a group can have).
    fn load_inode_bitmap(
        block_device: &dyn BlockDevice,
        nr_inodes_per_group: u32,
        desc: &BlockGroupDesc,
    ) -> Result<IdBitmap> {
        let capacity = nr_inodes_per_group.min(u32::from(IdBitmap::capacity())) as u16;

        // An `INODE_UNINIT` group has never had an inode allocated: its on-disk
        // inode bitmap is not maintained (raw content is meaningless). Present an
        // all-free bitmap rather than trusting the garbage. The inode allocator
        // additionally skips such groups (see `alloc_ino`), so no inode is ever
        // placed here until lazy inode-table init lands, but generating the
        // correct bitmap keeps any incidental read (e.g. a bogus inode number)
        // from observing spurious allocations.
        if desc.is_inode_uninit() {
            return Ok(IdBitmap::from_buf(
                vec![0u8; BLOCK_SIZE].into_boxed_slice(),
                capacity,
            ));
        }

        let bitmap_bid = desc.inode_bitmap_bid();
        let mut buf = vec![0u8; BLOCK_SIZE];
        if block_device
            .read_bytes(Bid::new(bitmap_bid).to_offset(), &mut buf)
            .is_err()
        {
            return_errno_with_message!(Errno::EIO, "failed to read inode bitmap");
        }

        Ok(IdBitmap::from_buf(buf.into_boxed_slice(), capacity))
    }

    /// Recomputes `bg_itable_unused` — the number of never-used inodes at the
    /// tail of the group's inode table — from the in-memory inode bitmap. On a
    /// `metadata_csum`/`gdt_csum` volume e2fsck verifies this equals
    /// `nr_inodes_per_group` minus one past the highest allocated inode, so it
    /// must track every allocation and free rather than stay at the stale mke2fs
    /// value (Linux `ext4_bg_itable_unused` / `ext4_free_inodes_count`).
    fn itable_unused_from_bitmap(bitmap: &IdBitmap, nr_inodes_per_group: u32) -> u32 {
        let nbytes = (nr_inodes_per_group as usize).div_ceil(8);
        let high_water = bitmap.as_bytes()[..nbytes]
            .iter()
            .rposition(|&b| b != 0)
            .map_or(0, |byte_idx| {
                // Highest set bit within the last non-zero byte, one-based: bit
                // index `7 - leading_zeros`, plus one.
                byte_idx * 8 + (8 - bitmap.as_bytes()[byte_idx].leading_zeros() as usize)
            })
            .min(nr_inodes_per_group as usize);
        nr_inodes_per_group - high_water as u32
    }

    /// Checks whether `range` (filesystem-wide block numbers) overlaps any
    /// system-metadata block of this group: the block bitmap, the inode bitmap,
    /// or the inode-table blocks.
    fn overlaps_system_zone_with(&self, desc: &BlockGroupDesc, range: Range<Ext4Bid>) -> bool {
        if range.is_empty() {
            return false;
        }

        let block_bitmap = desc.block_bitmap_bid()..(desc.block_bitmap_bid() + 1);
        let inode_bitmap = desc.inode_bitmap_bid()..(desc.inode_bitmap_bid() + 1);
        let inode_table = desc.inode_table_bid()
            ..(desc.inode_table_bid() + self.nr_inode_table_blocks_per_group as Ext4Bid);

        Self::ranges_overlap(&range, &block_bitmap)
            || Self::ranges_overlap(&range, &inode_bitmap)
            || Self::ranges_overlap(&range, &inode_table)
    }

    fn ranges_overlap(a: &Range<Ext4Bid>, b: &Range<Ext4Bid>) -> bool {
        !a.is_empty() && !b.is_empty() && a.start < b.end && b.start < a.end
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::{super::test_utils::Ext4MemoryDisk, *};

    /// A `BLOCK_UNINIT` group reconstructs its block bitmap from the layout — the
    /// leading `group_size - free_blocks_count` overhead blocks marked used, the
    /// rest free — and does NOT trust the (garbage) on-disk bitmap block. This is
    /// the fix for handing out a lazily-initialized group's backup metadata as
    /// "free" space.
    #[ktest]
    fn block_uninit_reconstructs_prefix_bitmap() {
        const GROUP_SIZE: u32 = 100;
        const OVERHEAD: u32 = 10;

        // Poison the on-disk block-bitmap block (bid 0) with all-ones: if the code
        // trusted it, every block would read as used and the asserts below fail.
        let disk = Ext4MemoryDisk::new(2);
        disk.segment()
            .write_bytes(0, &[0xFFu8; BLOCK_SIZE])
            .unwrap();

        let desc = BlockGroupDesc {
            block_bitmap_bid: 0,
            inode_bitmap_bid: 0,
            inode_table_bid: 0,
            free_blocks_count: GROUP_SIZE - OVERHEAD,
            free_inodes_count: 0,
            used_dirs_count: 0,
            flags: BG_BLOCK_UNINIT,
            itable_unused: 0,
        };

        let bitmap =
            BlockGroup::load_block_bitmap(&disk, 0, (GROUP_SIZE - 1) as Ext4Bid, &desc).unwrap();

        for bit in 0..OVERHEAD as u16 {
            assert!(
                bitmap.is_allocated(bit),
                "overhead block {bit} must be used"
            );
        }
        for bit in OVERHEAD as u16..GROUP_SIZE as u16 {
            assert!(!bitmap.is_allocated(bit), "data block {bit} must be free");
        }
    }

    /// A metadata block sitting beyond the reconstructed prefix breaks the
    /// prefix assumption, so reconstruction refuses (fail-closed) rather than
    /// risk handing that block out.
    #[ktest]
    fn block_uninit_rejects_metadata_past_prefix() {
        let disk = Ext4MemoryDisk::new(2);
        let desc = BlockGroupDesc {
            block_bitmap_bid: 50, // in-range but past the 10-block prefix
            inode_bitmap_bid: 0,
            inode_table_bid: 0,
            free_blocks_count: 90,
            free_inodes_count: 0,
            used_dirs_count: 0,
            flags: BG_BLOCK_UNINIT,
            itable_unused: 0,
        };
        assert!(BlockGroup::load_block_bitmap(&disk, 0, 99, &desc).is_err());
    }

    /// A 64-byte descriptor whose block-number high halves are non-zero decodes
    /// to the correct `> 2^32` `Ext4Bid` — the red-line splice. The per-group
    /// counters come from the low half untouched.
    #[ktest]
    fn decode_64byte_descriptor_high_halves() {
        let raw = RawBlockGroup64 {
            lo: RawBlockGroup {
                block_bitmap_lo: 0x1111_2222,
                inode_bitmap_lo: 0x3333_4444,
                inode_table_lo: 0x5555_6666,
                free_blocks_count_lo: 7,
                free_inodes_count_lo: 9,
                used_dirs_count_lo: 3,
                ..Default::default()
            },
            hi: RawBlockGroupHi {
                block_bitmap_hi: 0xA,
                inode_bitmap_hi: 0xB,
                inode_table_hi: 0xC,
                ..Default::default()
            },
        };

        let desc = BlockGroupDesc::from_raw(&raw.lo, Some(&raw.hi));
        assert_eq!(desc.block_bitmap_bid(), 0x0000_000A_1111_2222);
        assert_eq!(desc.inode_bitmap_bid(), 0x0000_000B_3333_4444);
        assert_eq!(desc.inode_table_bid(), 0x0000_000C_5555_6666);
        assert_eq!(desc.free_blocks_count(), 7);
        assert_eq!(desc.free_inodes_count(), 9);
        assert_eq!(desc.used_dirs_count(), 3);

        // The 32-byte (no-hi) path leaves the block numbers at their low halves.
        let desc32 = BlockGroupDesc::from_raw(&raw.lo, None);
        assert_eq!(desc32.block_bitmap_bid(), 0x1111_2222);
        assert_eq!(desc32.inode_bitmap_bid(), 0x3333_4444);
        assert_eq!(desc32.inode_table_bid(), 0x5555_6666);
    }

    /// Two adjacent 64-byte descriptors in a GDT are read at their 64-byte-apart
    /// offsets and each decodes to its own distinct (wide) block numbers — the
    /// desc_size stride plus the wide read.
    #[ktest]
    fn read_desc_64byte_stride() {
        let disk = Ext4MemoryDisk::new(4);
        let gdt_base = BLOCK_SIZE; // GDT at block 1, as the fixture lays it out.
        let desc_size: u16 = 64;

        let g0 = RawBlockGroup64 {
            lo: RawBlockGroup {
                block_bitmap_lo: 0x10,
                inode_bitmap_lo: 0x11,
                inode_table_lo: 0x12,
                ..Default::default()
            },
            hi: RawBlockGroupHi {
                block_bitmap_hi: 1,
                inode_table_hi: 2,
                ..Default::default()
            },
        };
        let g1 = RawBlockGroup64 {
            lo: RawBlockGroup {
                block_bitmap_lo: 0x20,
                inode_bitmap_lo: 0x21,
                inode_table_lo: 0x22,
                ..Default::default()
            },
            hi: RawBlockGroupHi {
                block_bitmap_hi: 3,
                inode_table_hi: 4,
                ..Default::default()
            },
        };

        let off0 = gdt_base;
        let off1 = gdt_base + desc_size as usize;
        disk.segment().write_val(off0, &g0).unwrap();
        disk.segment().write_val(off1, &g1).unwrap();

        let d0 = BlockGroup::read_desc(&disk, off0, desc_size, 0, None).unwrap();
        let d1 = BlockGroup::read_desc(&disk, off1, desc_size, 1, None).unwrap();
        assert_eq!(d0.block_bitmap_bid(), (1u64 << 32) | 0x10);
        assert_eq!(d0.inode_table_bid(), (2u64 << 32) | 0x12);
        assert_eq!(d1.block_bitmap_bid(), (3u64 << 32) | 0x20);
        assert_eq!(d1.inode_bitmap_bid(), 0x21);
        assert_eq!(d1.inode_table_bid(), (4u64 << 32) | 0x22);
    }

    /// A 32-byte descriptor stamped with its crc32c `bg_checksum` verifies; a
    /// corrupted field, a wrong group number, or a wrong seed each fail with
    /// `EUCLEAN`. The checksum depends on the group number, so the same bytes at
    /// a different group index do not verify.
    #[ktest]
    fn group_desc_checksum_round_trip() {
        let seed = FsCsumSeed::new(0x1234_5678);
        let mut lo = RawBlockGroup {
            block_bitmap_lo: 0x10,
            inode_bitmap_lo: 0x11,
            inode_table_lo: 0x12,
            free_blocks_count_lo: 100,
            free_inodes_count_lo: 50,
            used_dirs_count_lo: 3,
            ..Default::default()
        };
        lo.checksum = BlockGroupDesc::group_desc_checksum(&lo, None, 7, seed);
        BlockGroupDesc::verify_group_desc_checksum(&lo, None, 7, seed).unwrap();

        // Wrong group number: the checksum folds it in.
        assert_eq!(
            BlockGroupDesc::verify_group_desc_checksum(&lo, None, 8, seed)
                .unwrap_err()
                .error(),
            Errno::EUCLEAN
        );
        // Wrong seed.
        assert!(
            BlockGroupDesc::verify_group_desc_checksum(&lo, None, 7, FsCsumSeed::new(0x1234_5679))
                .is_err()
        );
        // Corrupted body.
        let mut bad = lo;
        bad.free_blocks_count_lo = 101;
        assert!(BlockGroupDesc::verify_group_desc_checksum(&bad, None, 7, seed).is_err());
    }

    /// The 64-byte descriptor folds its high-half tail (including the bitmap
    /// checksum high halves) into `bg_checksum`, so a change there is caught.
    #[ktest]
    fn group_desc_checksum_covers_high_tail() {
        let seed = FsCsumSeed::new(0xABCD);
        let lo = RawBlockGroup {
            block_bitmap_lo: 0x20,
            ..Default::default()
        };
        let mut hi = RawBlockGroupHi {
            block_bitmap_hi: 1,
            block_bitmap_csum_hi: 0x9999,
            ..Default::default()
        };
        let csum = BlockGroupDesc::group_desc_checksum(&lo, Some(&hi), 2, seed);
        let mut lo_stamped = lo;
        lo_stamped.checksum = csum;
        BlockGroupDesc::verify_group_desc_checksum(&lo_stamped, Some(&hi), 2, seed).unwrap();

        // A change in the high tail changes the checksum.
        hi.block_bitmap_csum_hi = 0x8888;
        assert_ne!(
            BlockGroupDesc::group_desc_checksum(&lo, Some(&hi), 2, seed),
            csum
        );
    }

    /// The write-side stamp (bitmap checksums into the descriptor, then
    /// bg_checksum) round-trips through the read-side verify, and the stored
    /// bitmap checksum equals a fresh crc32c of the bitmap. A change to the
    /// bitmap after stamping is then detectable. 32-byte descriptor.
    #[ktest]
    fn stamp_checksums_round_trip_32() {
        let seed = FsCsumSeed::new(0x0BAD_F00D);
        let group = 4;
        let mut block_bitmap = vec![0u8; BLOCK_SIZE];
        block_bitmap[..8].copy_from_slice(&[0xFF, 0x0F, 0, 0, 0, 0, 0, 0]);
        let mut inode_bitmap = vec![0u8; BLOCK_SIZE];
        inode_bitmap[0] = 0x07;
        let (bb_len, ib_len) = (BLOCK_SIZE, 1024);

        let mut lo = RawBlockGroup {
            free_blocks_count_lo: 42,
            ..Default::default()
        };
        let bb_csum = BlockGroupDesc::bitmap_checksum(seed, &block_bitmap, bb_len);
        let ib_csum = BlockGroupDesc::bitmap_checksum(seed, &inode_bitmap, ib_len);
        BlockGroupDesc::stamp_checksums(&mut lo, None, group, seed, bb_csum, ib_csum);

        // The descriptor now verifies, and its bitmap checksum matches.
        BlockGroupDesc::verify_group_desc_checksum(&lo, None, group, seed).unwrap();
        assert_eq!(
            lo.block_bitmap_csum_lo,
            checksum::crc32c(seed.get(), &block_bitmap[..bb_len]) as u16
        );
        assert_eq!(
            lo.inode_bitmap_csum_lo,
            checksum::crc32c(seed.get(), &inode_bitmap[..ib_len]) as u16
        );

        // Flipping a bitmap bit changes what a fresh stamp would store.
        block_bitmap[2] = 0x01;
        let new_bb_csum = BlockGroupDesc::bitmap_checksum(seed, &block_bitmap, bb_len);
        assert_ne!(new_bb_csum as u16, lo.block_bitmap_csum_lo);
    }

    /// `stamp_desc_csum_in_block` decodes this group's descriptor from a block
    /// image, stamps it, and writes it back so a later read verifies. Exercises
    /// the journaled `patch_into` funnel's csum step at a non-zero descriptor
    /// offset within the block.
    #[ktest]
    fn stamp_desc_in_block_at_offset() {
        let seed = FsCsumSeed::new(0x1357_9BDF);
        let block_device: Arc<dyn BlockDevice> = Arc::new(Ext4MemoryDisk::new(4));
        let group_idx = 3usize;
        let desc_offset = BLOCK_SIZE + group_idx * size_of::<RawBlockGroup>();

        let block_bitmap = IdBitmap::from_buf(vec![0xFFu8; BLOCK_SIZE].into_boxed_slice(), 32768);
        let inode_bitmap = IdBitmap::from_buf(vec![0x00u8; BLOCK_SIZE].into_boxed_slice(), 8192);
        let desc = BlockGroupDesc::from_raw(&RawBlockGroup::default(), None);
        let metadata = BlockGroupMetadata {
            desc: Dirty::new(desc),
            block_bitmap: Dirty::new(block_bitmap),
            inode_bitmap: Dirty::new(inode_bitmap),
        };

        let bg = BlockGroup {
            group_idx,
            metadata: RwMutex::new(metadata),
            block_device,
            first_block: 0,
            last_block: 32767,
            nr_inode_table_blocks_per_group: 512,
            nr_inodes_per_group: 8192,
            nr_blocks_per_group: 32768,
            inode_size: 256,
            block_size: BLOCK_SIZE,
            desc_size: 32,
            desc_offset,
            csum_seed: Some(seed),
            inode_cache: RwMutex::new(BTreeMap::new()),
        };

        let mut block = vec![0u8; BLOCK_SIZE];
        let md = bg.metadata.read();
        bg.stamp_desc_csum_in_block(&mut block, &md);

        // The stamped descriptor at its offset verifies against the group index.
        let off = desc_offset % BLOCK_SIZE;
        let lo = RawBlockGroup::from_bytes(&block[off..off + size_of::<RawBlockGroup>()]);
        BlockGroupDesc::verify_group_desc_checksum(&lo, None, group_idx as u32, seed).unwrap();
        assert_eq!(
            lo.block_bitmap_csum_lo,
            checksum::crc32c(seed.get(), &vec![0xFFu8; BLOCK_SIZE]) as u16
        );
    }

    /// `patch_into` on a 64-byte descriptor rewrites only the 32-byte low half:
    /// the counters take new values while the high tail (block-number high halves
    /// and the zero counter high halves) is byte-for-byte preserved.
    #[ktest]
    fn patch_into_preserves_high_tail() {
        let raw = RawBlockGroup64 {
            lo: RawBlockGroup {
                block_bitmap_lo: 0x100,
                inode_bitmap_lo: 0x101,
                inode_table_lo: 0x102,
                free_blocks_count_lo: 50,
                free_inodes_count_lo: 60,
                used_dirs_count_lo: 2,
                ..Default::default()
            },
            hi: RawBlockGroupHi {
                block_bitmap_hi: 7,
                inode_bitmap_hi: 8,
                inode_table_hi: 9,
                ..Default::default()
            },
        };

        // Place the descriptor at group index 3's offset within the GDT block.
        let mut block = vec![0u8; BLOCK_SIZE];
        let desc_offset = 3 * 64;
        block[desc_offset..desc_offset + 64].copy_from_slice(raw.as_bytes());
        let hi_before = block[desc_offset + 32..desc_offset + 64].to_vec();

        let mut desc = BlockGroupDesc::from_raw(&raw.lo, Some(&raw.hi));
        desc.free_blocks_count = 11;
        desc.free_inodes_count = 22;
        desc.used_dirs_count = 4;
        desc.patch_into(&mut block, desc_offset);

        // The 32-byte high tail is untouched.
        assert_eq!(&block[desc_offset + 32..desc_offset + 64], &hi_before[..]);

        // The low-half counters took the new values; the (wide) block numbers,
        // whose high halves live in the tail, are unchanged.
        let after = RawBlockGroup64::from_bytes(&block[desc_offset..desc_offset + 64]);
        let desc_after = BlockGroupDesc::from_raw(&after.lo, Some(&after.hi));
        assert_eq!(desc_after.free_blocks_count(), 11);
        assert_eq!(desc_after.free_inodes_count(), 22);
        assert_eq!(desc_after.used_dirs_count(), 4);
        assert_eq!(desc_after.block_bitmap_bid(), (7u64 << 32) | 0x100);
        assert_eq!(desc_after.inode_bitmap_bid(), (8u64 << 32) | 0x101);
        assert_eq!(desc_after.inode_table_bid(), (9u64 << 32) | 0x102);
    }

    /// Round-trip: decode a 64-byte descriptor, patch its (unchanged) counters
    /// back into the same block, re-decode, and assert every field survives — and
    /// the full 64-byte on-disk image is byte-for-byte identical.
    #[ktest]
    fn descriptor_round_trip_64byte() {
        let raw = RawBlockGroup64 {
            lo: RawBlockGroup {
                block_bitmap_lo: 0xABCD,
                inode_bitmap_lo: 0xBCDE,
                inode_table_lo: 0xCDEF,
                free_blocks_count_lo: 100,
                free_inodes_count_lo: 200,
                used_dirs_count_lo: 5,
                ..Default::default()
            },
            hi: RawBlockGroupHi {
                block_bitmap_hi: 0x1,
                inode_bitmap_hi: 0x2,
                inode_table_hi: 0x3,
                ..Default::default()
            },
        };

        let mut block = vec![0u8; BLOCK_SIZE];
        let off = 5 * 64;
        block[off..off + 64].copy_from_slice(raw.as_bytes());

        let desc = BlockGroupDesc::from_raw(&raw.lo, Some(&raw.hi));
        desc.patch_into(&mut block, off);

        let raw2 = RawBlockGroup64::from_bytes(&block[off..off + 64]);
        let desc2 = BlockGroupDesc::from_raw(&raw2.lo, Some(&raw2.hi));
        assert_eq!(desc2.block_bitmap_bid(), desc.block_bitmap_bid());
        assert_eq!(desc2.inode_bitmap_bid(), desc.inode_bitmap_bid());
        assert_eq!(desc2.inode_table_bid(), desc.inode_table_bid());
        assert_eq!(desc2.free_blocks_count(), desc.free_blocks_count());
        assert_eq!(desc2.free_inodes_count(), desc.free_inodes_count());
        assert_eq!(desc2.used_dirs_count(), desc.used_dirs_count());
        // Patch wrote back the same counters it decoded, so nothing moved.
        assert_eq!(&block[off..off + 64], raw.as_bytes());
    }
}
