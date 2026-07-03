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
    fs::Ext4,
    inode::{Inode, InodeDesc, RawInode},
    journal,
    prelude::*,
    super_block::SuperBlock,
};

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
        Self {
            block_bitmap_bid: (lo.block_bitmap_lo as Ext4Bid)
                | ((block_bitmap_hi as Ext4Bid) << 32),
            inode_bitmap_bid: (lo.inode_bitmap_lo as Ext4Bid)
                | ((inode_bitmap_hi as Ext4Bid) << 32),
            inode_table_bid: (lo.inode_table_lo as Ext4Bid) | ((inode_table_hi as Ext4Bid) << 32),
            free_blocks_count: lo.free_blocks_count_lo as u32,
            free_inodes_count: lo.free_inodes_count_lo as u32,
            used_dirs_count: lo.used_dirs_count_lo as u32,
        }
    }

    /// Patches this group's mutable descriptor counters into the after-image of the
    /// descriptor block, for op-time journaling.
    ///
    /// The descriptor block holds many group descriptors; this group's lives at
    /// `desc_offset % BLOCK_SIZE` within the block. It is a read-modify-write on the
    /// seeded buffer (mirroring [`BlockGroup::sync_metadata`]): only
    /// `free_blocks_count_lo` / `free_inodes_count_lo` / `used_dirs_count_lo` are
    /// overwritten, so every field the device held (flags, csum, itable_unused, …)
    /// and every *other* group's descriptor in the same block are preserved. For a
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
        let desc = Self::read_desc(device.as_ref(), desc_offset, desc_size)?;

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
            inode_size,
            block_size,
            desc_size,
            desc_offset,
            inode_cache: RwMutex::new(BTreeMap::new()),
        })
    }

    /// Reads and decodes this group's descriptor from `device` at `desc_offset`,
    /// sized by `desc_size`: the 64-byte `64BIT` layout — splicing the block-number
    /// high halves via [`BlockGroupDesc::from_raw`] — when `desc_size` is 64, else
    /// the classic 32-byte layout. The single decode path shared by [`Self::load`]
    /// and [`Self::reload_metadata`], so the high-half splice lives at one boundary.
    fn read_desc(
        device: &dyn BlockDevice,
        desc_offset: usize,
        desc_size: u16,
    ) -> Result<BlockGroupDesc> {
        if desc_size as usize >= size_of::<RawBlockGroup64>() {
            let raw = device
                .read_val::<RawBlockGroup64>(desc_offset)
                .map_err(|_| Error::with_message(Errno::EIO, "failed to read group descriptor"))?;
            Ok(BlockGroupDesc::from_raw(&raw.lo, Some(&raw.hi)))
        } else {
            let raw = device
                .read_val::<RawBlockGroup>(desc_offset)
                .map_err(|_| Error::with_message(Errno::EIO, "failed to read group descriptor"))?;
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
        let desc = Self::read_desc(device, self.desc_offset, self.desc_size)?;
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
    pub(super) fn lookup_inode(&self, ino: Ext4Ino, fs: Weak<Ext4>) -> Result<Arc<Inode>> {
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

        let desc = self.read_inode_desc(ino)?;
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

    /// Loads and decodes an inode's on-disk descriptor from the inode table.
    ///
    /// The inode-table read stays a direct device read (no page cache in
    /// Phase 2); the cache built on top is only for inode identity and
    /// enumeration.
    pub(super) fn read_inode_desc(&self, ino: Ext4Ino) -> Result<InodeDesc> {
        let idx_in_group = self.inode_idx_in_group(ino) as usize;
        let offset =
            self.inode_table_bid() as usize * self.block_size + idx_in_group * self.inode_size;
        let raw = self.block_device.read_val::<RawInode>(offset)?;
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

    /// Attempts to allocate up to `count` contiguous blocks within this group.
    ///
    /// Returns `Ok(range)` with filesystem-wide block numbers on success, or an
    /// empty range (`Ok(0..0)`) if the group has no allocatable blocks. Returns
    /// `Err(EIO)` on bitmap/counter corruption.
    pub(super) fn alloc_blocks(
        &self,
        count: u32,
        sb_free_blocks: u64,
        handle: Option<&journal::Handle>,
    ) -> Result<Range<Ext4Bid>> {
        let group_size = (self.last_block - self.first_block + 1) as u32;
        debug_assert!(group_size <= IdBitmap::capacity() as u32);

        let mut metadata = self.metadata.write();

        let mut requested_count = count
            .min(group_size)
            .min(metadata.desc.free_blocks_count())
            .min(sb_free_blocks.min(u32::MAX as u64) as u32)
            as u16;

        let block_bitmap_bid = metadata.desc.block_bitmap_bid();
        let bitmap_access =
            journal::get_write_access(handle, block_bitmap_bid, journal::TriggerType::BlockBitmap)?;

        // TODO(P9, allocator work): improve bitmap allocation to reduce
        // fragmentation (e.g. find the first free run directly instead of
        // retrying with halved counts).
        let mut allocated_range = None;
        while requested_count > 0 {
            let candidate_range = metadata.block_bitmap.alloc_consecutive(requested_count);
            if candidate_range.is_some() {
                allocated_range = candidate_range;
                break;
            }
            requested_count /= 2;
        }

        let Some(range) = allocated_range else {
            if metadata.desc.free_blocks_count() > 0 {
                return_errno_with_message!(Errno::EIO, "block bitmap corruption detected");
            }
            return Ok(0..0);
        };

        let range_start = range.start as Ext4Bid;
        let alloc_count = range.len() as u32;

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

        let desc_block_bid = (self.desc_offset / BLOCK_SIZE) as Ext4Bid;
        bitmap_access.patch(|buf| buf.copy_from_slice(metadata.block_bitmap.as_bytes()))?;
        journal::get_write_access(handle, desc_block_bid, journal::TriggerType::GroupDesc)?
            .patch(|buf| metadata.desc.patch_into(buf, self.desc_offset))?;

        let range_start_block = self.first_block + range.start as Ext4Bid;
        let range_end_block = self.first_block + range.end as Ext4Bid;
        Ok(range_start_block..range_end_block)
    }

    /// Frees a contiguous range of group-relative block bits.
    ///
    /// Returns the number of blocks actually freed (allocated-to-free
    /// transitions). Returns `Err(EIO)` when the range overlaps the group's
    /// system zone.
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
            return_errno_with_message!(Errno::EIO, "freeing blocks in system zone");
        }

        let block_bitmap_bid = metadata.desc.block_bitmap_bid();
        let bitmap_access =
            journal::get_write_access(handle, block_bitmap_bid, journal::TriggerType::BlockBitmap)?;

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
        journal::get_write_access(handle, desc_block_bid, journal::TriggerType::GroupDesc)?
            .patch(|buf| metadata.desc.patch_into(buf, self.desc_offset))?;

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
        if type_.is_directory() && metadata.desc.used_dirs_count() == u32::from(u16::MAX) {
            return_errno_with_message!(Errno::EIO, "group used directory counter overflow");
        }

        let inode_bitmap_bid = metadata.desc.inode_bitmap_bid();
        let bitmap_access =
            journal::get_write_access(handle, inode_bitmap_bid, journal::TriggerType::InodeBitmap)?;

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

        let desc_block_bid = (self.desc_offset / BLOCK_SIZE) as Ext4Bid;
        bitmap_access.patch(|buf| buf.copy_from_slice(metadata.inode_bitmap.as_bytes()))?;
        journal::get_write_access(handle, desc_block_bid, journal::TriggerType::GroupDesc)?
            .patch(|buf| metadata.desc.patch_into(buf, self.desc_offset))?;

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
        let bitmap_access =
            journal::get_write_access(handle, inode_bitmap_bid, journal::TriggerType::InodeBitmap)?;

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

        let desc_block_bid = (self.desc_offset / BLOCK_SIZE) as Ext4Bid;
        bitmap_access.patch(|buf| buf.copy_from_slice(metadata.inode_bitmap.as_bytes()))?;
        journal::get_write_access(handle, desc_block_bid, journal::TriggerType::GroupDesc)?
            .patch(|buf| metadata.desc.patch_into(buf, self.desc_offset))?;

        Ok(true)
    }

    /// Writes dirty metadata back to disk under a single lock.
    ///
    /// Both bitmaps are written in full. The group descriptor is updated via
    /// read-modify-write: the raw descriptor is read, only the mutated counters
    /// (`free_blocks_count_lo`, `free_inodes_count_lo`, `used_dirs_count_lo`) are
    /// patched, and the result is written back so every other on-disk field
    /// (flags, csum, exclude, itable_unused) is preserved. For a 64-byte (`64BIT`)
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
            let mut raw = self
                .block_device
                .read_val::<RawBlockGroup>(self.desc_offset)
                .map_err(|_| {
                    Error::with_message(Errno::EIO, "failed to read group descriptor for sync")
                })?;
            raw.free_blocks_count_lo = metadata.desc.free_blocks_count() as u16;
            raw.free_inodes_count_lo = metadata.desc.free_inodes_count() as u16;
            raw.used_dirs_count_lo = metadata.desc.used_dirs_count() as u16;
            self.block_device
                .write_val(self.desc_offset, &raw)
                .map_err(|_| Error::with_message(Errno::EIO, "failed to write group descriptor"))?;
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
        let bitmap_bid = desc.block_bitmap_bid();

        let mut buf = vec![0u8; BLOCK_SIZE];
        if block_device
            .read_bytes(Bid::new(bitmap_bid).to_offset(), &mut buf)
            .is_err()
        {
            return_errno_with_message!(Errno::EIO, "failed to read block bitmap");
        }

        let capacity = (last_block - first_block + 1) as u16;
        debug_assert!(capacity <= IdBitmap::capacity());
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
        let bitmap_bid = desc.inode_bitmap_bid();

        let mut buf = vec![0u8; BLOCK_SIZE];
        if block_device
            .read_bytes(Bid::new(bitmap_bid).to_offset(), &mut buf)
            .is_err()
        {
            return_errno_with_message!(Errno::EIO, "failed to read inode bitmap");
        }

        let capacity = nr_inodes_per_group.min(u32::from(IdBitmap::capacity())) as u16;
        Ok(IdBitmap::from_buf(buf.into_boxed_slice(), capacity))
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

        let d0 = BlockGroup::read_desc(&disk, off0, desc_size).unwrap();
        let d1 = BlockGroup::read_desc(&disk, off1, desc_size).unwrap();
        assert_eq!(d0.block_bitmap_bid(), (1u64 << 32) | 0x10);
        assert_eq!(d0.inode_table_bid(), (2u64 << 32) | 0x12);
        assert_eq!(d1.block_bitmap_bid(), (3u64 << 32) | 0x20);
        assert_eq!(d1.inode_bitmap_bid(), 0x21);
        assert_eq!(d1.inode_table_bid(), (4u64 << 32) | 0x22);
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
