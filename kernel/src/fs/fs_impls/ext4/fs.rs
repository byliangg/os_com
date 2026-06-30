// SPDX-License-Identifier: MPL-2.0

//! The `Ext4` filesystem object: mount, geometry, block allocation, and inode
//! lookup.
//!
//! Phase 1 mounted a volume read-only. Phase 2 makes the block side writable:
//! the superblock and block-group descriptors become mutable behind dirty
//! tracking, each group caches its block bitmap, and `Ext4` routes block
//! allocation/free across groups and writes the mutated metadata back.
//!
//! The inode side is unchanged: inodes are still read directly from the device
//! by [`Ext4::read_inode_desc`]. The inode bitmap, inode-table page cache, and
//! inode allocation arrive in Phase 3.

use core::sync::atomic::{AtomicU32, Ordering};

use device_id::DeviceId;

use super::{
    block_group::BlockGroup,
    inode::{FilePerm, Inode, InodeDesc, RawInode},
    prelude::*,
    super_block::{RawSuperBlock, SUPER_BLOCK_OFFSET, SuperBlock},
    utils,
};
use crate::{
    fs::vfs::file_system::FsEventSubscriberStats, process::posix_thread::AsPosixThread,
    thread::Thread,
};

/// Root directory inode number.
pub(super) const ROOT_INO: Ext4Ino = 2;

/// Reserved inode holding the journal.
#[expect(dead_code)]
pub(super) const JOURNAL_INO: Ext4Ino = 8;

/// An ext4 filesystem instance.
pub struct Ext4 {
    block_device: Arc<dyn BlockDevice>,
    /// Superblock with dirty tracking.
    super_block: RwMutex<Dirty<SuperBlock>>,
    /// Per-group block-side metadata (descriptor + block bitmap).
    block_groups: Vec<BlockGroup>,
    /// Inodes per group, cached once at mount to avoid locking `super_block` on
    /// the inode read path.
    nr_inodes_per_group: u32,
    /// Monotonic source for the `i_generation` stamped onto each newly created
    /// inode. Seeded from the mount time, like ext2.
    next_generation: AtomicU32,
    fs_event_subscriber_stats: FsEventSubscriberStats,
    self_ref: Weak<Ext4>,
}

impl Ext4 {
    /// Mounts an ext4 volume from a block device.
    pub(super) fn open(device: Arc<dyn BlockDevice>) -> Result<Arc<Self>> {
        let raw_super_block = device.read_val::<RawSuperBlock>(SUPER_BLOCK_OFFSET)?;
        let super_block = SuperBlock::try_from(raw_super_block)?;
        let nr_inodes_per_group = super_block.nr_inodes_per_group();

        let block_groups = Self::load_block_groups(device.clone(), &super_block)?;

        Ok(Arc::new_cyclic(|weak| Ext4 {
            block_device: device,
            super_block: RwMutex::new(Dirty::new(super_block)),
            block_groups,
            nr_inodes_per_group,
            next_generation: AtomicU32::new(utils::now().as_secs() as u32),
            fs_event_subscriber_stats: FsEventSubscriberStats::new(),
            self_ref: weak.clone(),
        }))
    }

    pub(super) fn fs_event_subscriber_stats(&self) -> &FsEventSubscriberStats {
        &self.fs_event_subscriber_stats
    }

    /// Returns the device ID of the backing block device.
    pub(super) fn container_device_id(&self) -> DeviceId {
        self.block_device.id()
    }

    /// Loads every block group from the descriptor table, which immediately
    /// follows the block holding the superblock.
    fn load_block_groups(
        device: Arc<dyn BlockDevice>,
        super_block: &SuperBlock,
    ) -> Result<Vec<BlockGroup>> {
        let nr_groups = super_block.nr_block_groups() as usize;
        let gdt_base_offset =
            (super_block.first_data_block() as usize + 1) * super_block.block_size();

        let mut block_groups = Vec::with_capacity(nr_groups);
        for group_idx in 0..nr_groups {
            let group = BlockGroup::load(device.clone(), group_idx, super_block, gdt_base_offset)?;
            block_groups.push(group);
        }
        Ok(block_groups)
    }

    /// Returns a read guard of the superblock.
    pub(super) fn super_block(&self) -> RwMutexReadGuard<'_, Dirty<SuperBlock>> {
        self.super_block.read()
    }

    /// Returns a reference to the block group at `group_idx`.
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn block_group(&self, group_idx: usize) -> &BlockGroup {
        &self.block_groups[group_idx]
    }

    pub(super) fn block_device(&self) -> &Arc<dyn BlockDevice> {
        &self.block_device
    }

    /// Returns the maximum byte size of a regular file.
    ///
    /// An extent maps a 32-bit logical block index, so a file spans at most
    /// `2^32 - 1` blocks; the result is also clamped to `i64::MAX` (the VFS
    /// size limit). Phase 2's flatten-and-rebuild tree caps depth at 1, so real
    /// files are far smaller, but this bound is what `write_at` rejects against.
    pub(super) fn max_file_size(&self) -> usize {
        let by_blocks = (u32::MAX as u64) * BLOCK_SIZE as u64;
        by_blocks.min(i64::MAX as u64) as usize
    }

    /// Submits an asynchronous read of one or more blocks starting at `bid`.
    pub(super) fn read_blocks_async(
        &self,
        bid: Ext4Bid,
        bio_segment: BioSegment,
        complete_fn: Option<BioCompleteFn>,
        io_batch: &mut IoBatch,
    ) -> Result<()> {
        self.block_device
            .read_blocks_async(Bid::new(bid), bio_segment, complete_fn, io_batch)?;
        Ok(())
    }

    /// Submits an asynchronous write of one or more blocks starting at `bid`.
    pub(super) fn write_blocks_async(
        &self,
        bid: Ext4Bid,
        bio_segment: BioSegment,
        complete_fn: Option<BioCompleteFn>,
        io_batch: &mut IoBatch,
    ) -> Result<()> {
        self.block_device
            .write_blocks_async(Bid::new(bid), bio_segment, complete_fn, io_batch)?;
        Ok(())
    }

    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn this(&self) -> Weak<Ext4> {
        self.self_ref.clone()
    }

    /// Allocates up to `count` contiguous blocks, preferring the group that owns
    /// `goal`.
    ///
    /// Searches groups in a ring starting from the goal group. Returns
    /// `Err(ENOSPC)` if no group can satisfy the request, `Err(EINVAL)` if
    /// `count` is zero.
    pub(super) fn alloc_blocks(&self, count: u32, goal: Ext4Bid) -> Result<Range<Ext4Bid>> {
        if count == 0 {
            return_errno_with_message!(Errno::EINVAL, "zero block allocation requested");
        }

        let mut sb = self.super_block.write();
        let nr_block_groups = sb.nr_block_groups() as usize;
        let sb_free_blocks = sb.free_blocks_count();
        let first_data_block = sb.first_data_block();
        let nr_blocks_per_group = sb.nr_blocks_per_group() as Ext4Bid;
        if sb_free_blocks == 0 {
            return_errno_with_message!(Errno::ENOSPC, "no free blocks on device");
        }

        let goal_group = if goal > first_data_block {
            ((goal - first_data_block) / nr_blocks_per_group) as usize
        } else {
            0
        }
        .min(nr_block_groups - 1);

        for group_search_offset in 0..nr_block_groups {
            let group_idx = (goal_group + group_search_offset) % nr_block_groups;
            let group = &self.block_groups[group_idx];

            let range = group.alloc_blocks(count, sb_free_blocks)?;
            if !range.is_empty() {
                let allocated_count = range.end - range.start;
                sb.dec_free_blocks(allocated_count)?;
                return Ok(range);
            }
        }

        return_errno_with_message!(Errno::ENOSPC, "no free blocks available in any group");
    }

    /// Frees `count` blocks starting at `start`, splitting across groups.
    pub(super) fn free_blocks(&self, start: Ext4Bid, count: u32) -> Result<()> {
        if count == 0 {
            return Ok(());
        }

        let mut sb = self.super_block.write();
        let nr_blocks_per_group = sb.nr_blocks_per_group() as Ext4Bid;
        let first_data_block = sb.first_data_block();

        let mut current_block = start;
        let mut remaining_blocks = count;

        while remaining_blocks > 0 {
            let group_idx = ((current_block - first_data_block) / nr_blocks_per_group) as usize;
            let group = &self.block_groups[group_idx];

            let group_first_block = group.first_block();
            let group_last_block = group.last_block();
            let group_size = (group_last_block - group_first_block + 1) as u32;
            let group_start_bit = (current_block - group_first_block) as u32;
            let blocks_in_group = remaining_blocks.min(group_size - group_start_bit);
            let freed_count =
                group.free_blocks(group_start_bit..(group_start_bit + blocks_in_group))?;
            if freed_count > 0 {
                sb.inc_free_blocks(freed_count as u64)?;
            }
            current_block += blocks_in_group as Ext4Bid;
            remaining_blocks -= blocks_in_group;
        }

        Ok(())
    }

    /// Allocates one inode, preferring the group that owns `parent_ino`.
    ///
    /// Searches groups in a ring starting from the parent's group (ext2-style;
    /// the Orlov spreading policy is deferred to Phase 9). Returns the global
    /// inode number on success, `Err(ENOSPC)` if no group has a free inode.
    /// Mirrors [`alloc_blocks`] on the block side.
    pub(super) fn alloc_ino(&self, parent_ino: Ext4Ino, type_: InodeType) -> Result<Ext4Ino> {
        if type_ == InodeType::Unknown {
            return_errno_with_message!(Errno::EINVAL, "cannot allocate inode with unknown type");
        }

        let mut sb = self.super_block.write();
        let nr_block_groups = sb.nr_block_groups() as usize;
        let nr_inodes_per_group = sb.nr_inodes_per_group();
        let total_inodes = sb.total_inodes();
        if parent_ino < ROOT_INO || parent_ino > total_inodes {
            return_errno_with_message!(Errno::EIO, "parent inode number out of range");
        }
        if sb.free_inodes_count() == 0 {
            return_errno_with_message!(Errno::ENOSPC, "no free inodes on device");
        }

        let parent_group = ((parent_ino - 1) / nr_inodes_per_group) as usize;
        for group_search_offset in 0..nr_block_groups {
            let group_idx = (parent_group + group_search_offset) % nr_block_groups;
            let group = &self.block_groups[group_idx];

            let Some(local_idx) = group.alloc_ino(type_)? else {
                continue;
            };

            let ino = (group_idx as u32) * nr_inodes_per_group + local_idx + 1;
            if ino < sb.first_ino() || ino > total_inodes {
                // Roll back the group-level allocation before erroring out.
                let _ = group.free_inode(local_idx, type_);
                return_errno_with_message!(Errno::EIO, "allocated inode number out of valid range");
            }
            sb.dec_free_inodes()?;

            return Ok(ino);
        }

        return_errno_with_message!(Errno::ENOSPC, "no free inodes available in any group");
    }

    /// Frees an inode by number, mirroring [`free_blocks`] on the block side.
    pub(super) fn free_inode(&self, ino: Ext4Ino, type_: InodeType) -> Result<()> {
        let mut sb = self.super_block.write();
        let group = self.find_group(ino)?;
        let local_idx = (ino - 1) % self.nr_inodes_per_group;

        let was_allocated = group.free_inode(local_idx, type_)?;
        if was_allocated {
            sb.inc_free_inodes()?;
        }

        Ok(())
    }

    /// Allocates and initializes a new inode, returning the live `Arc<Inode>`.
    ///
    /// Allocates an inode number, builds a fresh [`InodeDesc`] (an empty extent
    /// root with the `EXTENTS` flag set, size/`i_blocks` 0, owners from the
    /// caller's fsuid/fsgid, `now` timestamps, and a monotonic generation), and
    /// writes the full on-disk inode. On a writeback failure the inode bit is
    /// freed and the superblock counter restored. Mirrors ext2 `create_inode`.
    // The sole inode-allocation entry point without a production caller yet; the
    // namespace operations (create/mkdir/symlink) that drive it land in Phase 3
    // Task 3. Marking this root reachable keeps every helper below it
    // (`alloc_ino`/`free_inode`/`write_new_inode_desc`/`InodeDesc::new` and their
    // callees) reachable too, so none of those need their own marker. Exercised
    // today by ktests.
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn create_inode(
        &self,
        parent_ino: Ext4Ino,
        type_: InodeType,
        perm: FilePerm,
    ) -> Result<Arc<Inode>> {
        let ino = self.alloc_ino(parent_ino, type_)?;

        let link_count = if type_.is_directory() { 2 } else { 1 };
        let (uid, gid) = Thread::current()
            .and_then(|thread| {
                thread
                    .as_posix_thread()
                    .map(|posix_thread| posix_thread.credentials())
            })
            .map(|credentials| {
                (
                    u32::from(credentials.fsuid()),
                    u32::from(credentials.fsgid()),
                )
            })
            .unwrap_or((0, 0));
        let now = utils::now();
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let inode_desc = InodeDesc::new(type_, perm, uid, gid, link_count, generation, now);

        if let Err(err) = self.write_new_inode_desc(ino, &inode_desc) {
            // Roll back the inode allocation: clear the bitmap bit and restore
            // the superblock free-inode counter.
            if let Err(free_err) = self.free_inode(ino, type_) {
                error!("create_inode: rollback free_inode failed: {:?}", free_err);
            }
            return Err(err);
        }

        let block_group_idx = ((ino - 1) / self.nr_inodes_per_group) as usize;
        Ok(Inode::new(
            ino,
            inode_desc.type_(),
            Dirty::new(inode_desc),
            block_group_idx,
            self.self_ref.clone(),
        ))
    }

    /// Writes back the superblock and every dirty group descriptor/bitmap.
    pub(super) fn sync_metadata(&self) -> Result<()> {
        for group in &self.block_groups {
            group.sync_metadata()?;
        }

        let mut sb = self.super_block.write();
        if sb.is_dirty() {
            // RMW the on-disk superblock: patch only the free-block and
            // free-inode counters so every other on-disk field is preserved
            // losslessly.
            let mut raw = self
                .block_device
                .read_val::<RawSuperBlock>(SUPER_BLOCK_OFFSET)
                .map_err(|_| {
                    Error::with_message(Errno::EIO, "failed to read superblock for sync")
                })?;
            raw.free_blocks_count = sb.free_blocks_count() as u32;
            raw.free_inodes_count = sb.free_inodes_count();
            self.block_device
                .write_val(SUPER_BLOCK_OFFSET, &raw)
                .map_err(|_| Error::with_message(Errno::EIO, "failed to write superblock"))?;
            sb.clear_dirty();
        }

        Ok(())
    }

    /// Flushes every cached inode together with the block-side metadata.
    ///
    /// Order matters for on-disk consistency: each group's cached inodes (their
    /// data pages + inode-table descriptors) are flushed first, then the dirty
    /// bitmaps/GDT/superblock. Flushing inodes before the bitmap keeps the
    /// on-disk extents and the block bitmap mutually consistent — otherwise a
    /// truncate that freed blocks in the bitmap could be persisted while the
    /// inode still on disk references those (now free) blocks, which `e2fsck`
    /// reports as corruption.
    ///
    /// Inode flushing never holds a group's `inode_cache` lock across the sync
    /// (see [`BlockGroup::sync_inodes`]), so the only locks held in sequence are
    /// `inode.inner.write()` then, later, `super_block.write()` + per-group
    /// `metadata.write()` — no inversion.
    pub(super) fn sync_all(&self) -> Result<()> {
        for group in &self.block_groups {
            group.sync_inodes()?;
        }
        self.sync_metadata()
    }

    /// Locates and decodes an inode's metadata directly from the inode table.
    ///
    /// The owning group performs the on-disk load; this method only routes the
    /// inode number to its group.
    pub(super) fn read_inode_desc(&self, ino: Ext4Ino) -> Result<InodeDesc> {
        self.find_group(ino)?.read_inode_desc(ino)
    }

    /// Returns the block group that owns `ino`.
    fn find_group(&self, ino: Ext4Ino) -> Result<&BlockGroup> {
        if ino == 0 {
            return_errno_with_message!(Errno::ENOENT, "invalid inode number 0");
        }
        let group_idx = ((ino - 1) / self.nr_inodes_per_group) as usize;
        self.block_groups
            .get(group_idx)
            .ok_or_else(|| Error::with_message(Errno::ENOENT, "inode block group out of range"))
    }

    /// Reads the root directory's inode metadata.
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn root_inode_desc(&self) -> Result<InodeDesc> {
        self.read_inode_desc(ROOT_INO)
    }

    /// Computes the device byte offset of the on-disk `RawInode` for `ino`.
    fn inode_table_offset(&self, ino: Ext4Ino) -> Result<usize> {
        if ino == 0 {
            return_errno_with_message!(Errno::ENOENT, "invalid inode number 0");
        }
        let group_idx = ((ino - 1) / self.nr_inodes_per_group) as usize;
        let idx_in_group = ((ino - 1) % self.nr_inodes_per_group) as usize;
        let group = self
            .block_groups
            .get(group_idx)
            .ok_or_else(|| Error::with_message(Errno::ENOENT, "inode block group out of range"))?;
        let sb = self.super_block.read();
        Ok(group.inode_table_bid() as usize * sb.block_size() + idx_in_group * sb.inode_size())
    }

    /// Read-modify-writes the on-disk `RawInode` for `ino`, patching only the
    /// fields that buffered writes can mutate (size, `i_blocks`, the extent
    /// root, timestamps, flags, and link count) and preserving everything else
    /// (`extra_isize`, checksums, generation, xattr tail, osd fields) losslessly.
    pub(super) fn write_back_inode_desc(
        &self,
        ino: Ext4Ino,
        desc: &InodeDesc,
        root: &[u32; super::inode::RAW_BLOCK_PTRS_LEN],
    ) -> Result<()> {
        let offset = self.inode_table_offset(ino)?;
        let mut raw = self
            .block_device
            .read_val::<RawInode>(offset)
            .map_err(|_| Error::with_message(Errno::EIO, "failed to read inode for writeback"))?;

        // Size (size_high only carries the high 32 bits for regular files).
        raw.size_lo = desc.size() as u32;
        if desc.type_() == InodeType::File {
            raw.size_high = (desc.size() >> 32) as u32;
        }

        // 48-bit `i_blocks` (low 32 + high 16).
        let sectors = desc.sector_count();
        raw.sector_count = sectors as u32;
        raw.blocks_high = (sectors >> 32) as u16;

        // Timestamps (epoch + nanoseconds) — reverse of `decode_time`.
        let (mtime_secs, mtime_extra) = encode_time(desc.mtime());
        raw.mtime = mtime_secs;
        raw.mtime_extra = mtime_extra;
        let (ctime_secs, ctime_extra) = encode_time(desc.ctime());
        raw.ctime = ctime_secs;
        raw.ctime_extra = ctime_extra;

        // Mode (keep the on-disk type bits, update only the permission bits),
        // owners, and access time — so chmod/chown/chgrp/utimes persist too.
        raw.mode = (raw.mode & 0xF000) | (desc.perm().bits() & 0o7777);
        raw.uid = desc.uid() as u16;
        raw.uid_high = (desc.uid() >> 16) as u16;
        raw.gid = desc.gid() as u16;
        raw.gid_high = (desc.gid() >> 16) as u16;
        let (atime_secs, atime_extra) = encode_time(desc.atime());
        raw.atime = atime_secs;
        raw.atime_extra = atime_extra;

        // The inline extent-tree root, flags, and link count.
        raw.block = *root;
        raw.flags = desc.flags().bits();
        raw.link_count = desc.link_count();

        self.block_device
            .write_val(offset, &raw)
            .map_err(|_| Error::with_message(Errno::EIO, "failed to write inode"))?;
        Ok(())
    }

    /// Writes the complete on-disk `RawInode` for a freshly created inode.
    ///
    /// Unlike [`write_back_inode_desc`](Self::write_back_inode_desc), which is a
    /// read-modify-write tuned for the buffered-write path (and therefore
    /// preserves the on-disk type bits and generation), this writes every field
    /// of a brand-new inode from scratch: the type/permission mode, owners,
    /// timestamps, generation, the inline extent root, flags, link count, and
    /// `extra_isize`. The previous slot contents (a deleted inode or zeros) are
    /// fully overwritten.
    fn write_new_inode_desc(&self, ino: Ext4Ino, desc: &InodeDesc) -> Result<()> {
        let offset = self.inode_table_offset(ino)?;

        let (mtime_secs, mtime_extra) = encode_time(desc.mtime());
        let (ctime_secs, ctime_extra) = encode_time(desc.ctime());
        let (atime_secs, atime_extra) = encode_time(desc.atime());
        let (crtime_secs, crtime_extra) = encode_time(desc.crtime());

        let raw = RawInode {
            mode: (desc.type_() as u16) | (desc.perm().bits() & 0o7777),
            uid: desc.uid() as u16,
            size_lo: desc.size() as u32,
            atime: atime_secs,
            ctime: ctime_secs,
            mtime: mtime_secs,
            gid: desc.gid() as u16,
            link_count: desc.link_count(),
            sector_count: desc.sector_count() as u32,
            flags: desc.flags().bits(),
            block: *desc.raw_block(),
            generation: desc.generation(),
            size_high: if desc.type_() == InodeType::File {
                (desc.size() >> 32) as u32
            } else {
                0
            },
            blocks_high: (desc.sector_count() >> 32) as u16,
            uid_high: (desc.uid() >> 16) as u16,
            gid_high: (desc.gid() >> 16) as u16,
            // Match the `extra_isize` the fixtures and `mke2fs` write for a
            // 256-byte inode (32 bytes of ext4 extra area past the 128-byte
            // base) so the nanosecond timestamps above are honored on read.
            extra_isize: 32,
            ctime_extra,
            mtime_extra,
            atime_extra,
            crtime: crtime_secs,
            crtime_extra,
            ..Default::default()
        };

        self.block_device
            .write_val(offset, &raw)
            .map_err(|_| Error::with_message(Errno::EIO, "failed to write new inode"))?;
        Ok(())
    }

    /// Reads an inode, returning the cached `Arc<Inode>` if one already exists.
    ///
    /// Routing through the owning group's inode cache gives every reader of one
    /// inode number the same in-memory inode (identity), which the
    /// filesystem-level sync relies on to enumerate and flush all dirty inodes.
    pub(super) fn read_inode(&self, ino: Ext4Ino) -> Result<Arc<Inode>> {
        self.find_group(ino)?
            .lookup_inode(ino, self.self_ref.clone())
    }

    /// Reads the root directory inode.
    pub(super) fn root_inode(&self) -> Result<Arc<Inode>> {
        self.read_inode(ROOT_INO)
    }
}

/// Encodes a timestamp into its on-disk `(seconds, *_extra)` pair: the extra
/// field packs a 2-bit epoch (the seconds bits past 2038) in its low bits and
/// nanoseconds in the upper bits. Reverse of `decode_time` in `inode`.
fn encode_time(time: Duration) -> (u32, u32) {
    let secs = time.as_secs();
    let nsec = time.subsec_nanos();
    let epoch = (secs >> 32) & 0x3;
    let secs_lo = secs as u32;
    let extra = (epoch as u32) | (nsec << 2);
    (secs_lo, extra)
}

/// Rollback guard for blocks allocated through [`Ext4::alloc_blocks`].
///
/// Tracks every allocated range and, unless [`commit`](Self::commit) is called,
/// frees them all on drop. This is the allocation-rollback primitive that later
/// tasks (e.g. extent insertion) use to undo block allocations when a multi-step
/// operation fails partway through.
pub(super) struct BlockAllocGuard<'a> {
    fs: &'a Ext4,
    ranges: Vec<Range<Ext4Bid>>,
    committed: bool,
}

#[cfg_attr(not(ktest), expect(dead_code))]
impl<'a> BlockAllocGuard<'a> {
    /// Creates a guard tracking no ranges yet.
    pub(super) fn new(fs: &'a Ext4) -> Self {
        Self {
            fs,
            ranges: Vec::new(),
            committed: false,
        }
    }

    /// Records an allocated range to be rolled back on drop.
    pub(super) fn extend(&mut self, range: Range<Ext4Bid>) {
        if !range.is_empty() {
            self.ranges.push(range);
        }
    }

    /// Commits the allocation; the tracked ranges are kept on drop.
    pub(super) fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for BlockAllocGuard<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for range in self.ranges.iter() {
            let count = (range.end - range.start) as u32;
            if let Err(err) = self.fs.free_blocks(range.start, count) {
                error!(
                    "BlockAllocGuard: failed to free range {:?} in rollback: {:?}",
                    range, err
                );
            }
        }
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::{
        super::{
            block_group::RawBlockGroup,
            test_utils::{Ext4FixtureBuilder, make_file_inode},
        },
        *,
    };

    #[ktest]
    fn mount_and_read_root() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let root = f.ext4.root_inode_desc().unwrap();
        assert_eq!(root.type_(), InodeType::Dir);
        assert_eq!(root.link_count(), 2);
        assert!(root.is_extent_based());
        assert_eq!(f.ext4.super_block().nr_block_groups(), 1);
    }

    #[ktest]
    fn read_inode_zero_fails() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        assert!(f.ext4.read_inode_desc(0).is_err());
    }

    #[ktest]
    fn read_small_file_end_to_end() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let data_block = 100u32;
        let content = b"hello ext4 phase 1 read path!";
        f.write_data_block(data_block, content);
        f.write_raw_inode(11, &make_file_inode(data_block, content.len() as u32));

        let inode = f.ext4.read_inode(11).unwrap();
        assert_eq!(inode.inode_type(), InodeType::File);
        assert_eq!(inode.size(), content.len());

        let mut buf = vec![0u8; content.len()];
        let mut writer = VmWriter::from(buf.as_mut_slice()).to_fallible();
        let read = inode.read_at(0, &mut writer).unwrap();
        assert_eq!(read, content.len());
        assert_eq!(&buf[..], content);
    }

    #[ktest]
    fn alloc_and_free_single_group_ok() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();

        let before_sb_free = f.ext4.super_block().free_blocks_count();
        let before_group_free = f.ext4.block_group(0).free_blocks_count();

        let goal = f.ext4.block_group(0).first_block();
        let range = f.ext4.alloc_blocks(8, goal).unwrap();
        let alloc_len = (range.end - range.start) as u32;
        assert!((1..=8).contains(&alloc_len));

        // The whole run lives in a single group.
        let group = f.ext4.block_group(0);
        assert!(range.start >= group.first_block());
        assert!(range.end - 1 <= group.last_block());

        assert_eq!(
            f.ext4.block_group(0).free_blocks_count(),
            before_group_free - alloc_len
        );
        assert_eq!(
            f.ext4.super_block().free_blocks_count(),
            before_sb_free - alloc_len as u64
        );

        f.ext4.free_blocks(range.start, alloc_len).unwrap();
        assert_eq!(f.ext4.block_group(0).free_blocks_count(), before_group_free);
        assert_eq!(f.ext4.super_block().free_blocks_count(), before_sb_free);
    }

    #[ktest]
    fn alloc_goal_lands_in_later_group() {
        // Two groups; goal points into group 1.
        let f = Ext4FixtureBuilder::new(2048, 256, 2 * 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let group1_first = f.ext4.block_group(1).first_block();

        let range = f.ext4.alloc_blocks(4, group1_first).unwrap();
        assert!(!range.is_empty());
        // Allocation lands in group 1 (at or after its first block).
        assert!(range.start >= group1_first);
    }

    #[ktest]
    fn alloc_enospc_and_einval() {
        // free == 0 -> ENOSPC.
        let f_full = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_no_free_blocks()
            .build()
            .unwrap();
        assert_eq!(
            f_full
                .ext4
                .alloc_blocks(1, f_full.ext4.block_group(0).first_block())
                .unwrap_err()
                .error(),
            Errno::ENOSPC
        );

        // count == 0 -> EINVAL.
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        assert_eq!(
            f.ext4
                .alloc_blocks(0, f.ext4.block_group(0).first_block())
                .unwrap_err()
                .error(),
            Errno::EINVAL
        );
    }

    #[ktest]
    fn block_alloc_guard_rolls_back_on_drop() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let before_sb_free = f.ext4.super_block().free_blocks_count();
        let before_group_free = f.ext4.block_group(0).free_blocks_count();

        let range = f
            .ext4
            .alloc_blocks(4, f.ext4.block_group(0).first_block())
            .unwrap();
        let alloc_len = (range.end - range.start) as u32;
        assert!(alloc_len > 0);

        {
            let mut guard = BlockAllocGuard::new(&f.ext4);
            guard.extend(range.clone());
            // Drop without commit -> rollback.
        }

        // Counts restored and bitmap bits cleared.
        assert_eq!(f.ext4.super_block().free_blocks_count(), before_sb_free);
        assert_eq!(f.ext4.block_group(0).free_blocks_count(), before_group_free);
        let group = f.ext4.block_group(0);
        let metadata = group.metadata();
        for bid in range.clone() {
            let bit = (bid - group.first_block()) as u16;
            assert!(!metadata.block_bitmap.is_allocated(bit));
        }
    }

    #[ktest]
    fn block_alloc_guard_commit_keeps_blocks() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let before_sb_free = f.ext4.super_block().free_blocks_count();

        let range = f
            .ext4
            .alloc_blocks(4, f.ext4.block_group(0).first_block())
            .unwrap();
        let alloc_len = (range.end - range.start) as u32;

        {
            let mut guard = BlockAllocGuard::new(&f.ext4);
            guard.extend(range.clone());
            guard.commit();
        }

        // Allocation persists.
        assert_eq!(
            f.ext4.super_block().free_blocks_count(),
            before_sb_free - alloc_len as u64
        );
        let group = f.ext4.block_group(0);
        let metadata = group.metadata();
        for bid in range {
            let bit = (bid - group.first_block()) as u16;
            assert!(metadata.block_bitmap.is_allocated(bit));
        }
    }

    #[ktest]
    fn sync_metadata_round_trip_lossless() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();

        // Snapshot the raw group descriptor before any mutation.
        let gdt_offset = (f.ext4.super_block().first_data_block() as usize + 1) * BLOCK_SIZE;
        let raw_before = f
            .disk
            .segment()
            .read_val::<RawBlockGroup>(gdt_offset)
            .unwrap();
        let raw_sb_before = f
            .disk
            .segment()
            .read_val::<RawSuperBlock>(SUPER_BLOCK_OFFSET)
            .unwrap();

        let range = f
            .ext4
            .alloc_blocks(4, f.ext4.block_group(0).first_block())
            .unwrap();
        let alloc_len = (range.end - range.start) as u32;
        f.ext4.sync_metadata().unwrap();

        let raw_after = f
            .disk
            .segment()
            .read_val::<RawBlockGroup>(gdt_offset)
            .unwrap();
        let raw_sb_after = f
            .disk
            .segment()
            .read_val::<RawSuperBlock>(SUPER_BLOCK_OFFSET)
            .unwrap();

        // Only free-block counters changed; all other fields are preserved.
        assert_eq!(
            raw_after.free_blocks_count_lo,
            raw_before.free_blocks_count_lo - alloc_len as u16
        );
        assert_eq!(raw_after.inode_table_lo, raw_before.inode_table_lo);
        assert_eq!(raw_after.block_bitmap_lo, raw_before.block_bitmap_lo);
        assert_eq!(raw_after.inode_bitmap_lo, raw_before.inode_bitmap_lo);
        assert_eq!(raw_after.flags, raw_before.flags);
        assert_eq!(raw_after.checksum, raw_before.checksum);
        assert_eq!(raw_after.itable_unused_lo, raw_before.itable_unused_lo);
        assert_eq!(
            raw_after.free_inodes_count_lo,
            raw_before.free_inodes_count_lo
        );

        assert_eq!(
            raw_sb_after.free_blocks_count,
            raw_sb_before.free_blocks_count - alloc_len
        );
        assert_eq!(raw_sb_after.inodes_count, raw_sb_before.inodes_count);
        assert_eq!(raw_sb_after.blocks_count, raw_sb_before.blocks_count);
        assert_eq!(raw_sb_after.magic, raw_sb_before.magic);
    }

    use super::super::test_utils::make_empty_file_inode;

    const SECTORS_PER_BLOCK: u64 = (BLOCK_SIZE / SECTOR_SIZE) as u64;

    fn write_all(inode: &Inode, offset: usize, data: &[u8]) {
        let mut reader = VmReader::from(data).to_fallible();
        let n = inode.write_at(offset, &mut reader).unwrap();
        assert_eq!(n, data.len());
    }

    /// Returns whether physical block `pblock` is marked allocated in group 0.
    fn block_is_allocated(f: &super::super::test_utils::Ext4Fixture, pblock: Ext4Bid) -> bool {
        let group = f.ext4.block_group(0);
        group
            .metadata()
            .block_bitmap
            .is_allocated((pblock - group.first_block()) as u16)
    }

    /// Parses the inline depth-0 extent root and returns the physical block that
    /// logical block `lblock` maps to, if any. Only handles the single-contiguous
    /// extent shape these tests build.
    fn ondisk_pblock_of(raw: &RawInode, lblock: u32) -> Option<Ext4Bid> {
        let entries = (raw.block[0] >> 16) & 0xFFFF;
        let depth = (raw.block[1] >> 16) & 0xFFFF;
        if depth != 0 {
            return None;
        }
        for i in 0..entries as usize {
            let ee_block = raw.block[3 + i * 3];
            let raw_len = raw.block[4 + i * 3] & 0xFFFF;
            // An unwritten extent biases its length by 32768; mask it off.
            let len = if raw_len > 32768 {
                raw_len - 32768
            } else {
                raw_len
            };
            let ee_start = raw.block[5 + i * 3] as Ext4Bid;
            if lblock >= ee_block && lblock < ee_block + len {
                return Some(ee_start + (lblock - ee_block) as Ext4Bid);
            }
        }
        None
    }

    /// `read_inode` returns the same `Arc<Inode>` for repeated reads of one ino:
    /// the per-group inode cache gives the inode a stable identity.
    #[ktest]
    fn read_inode_returns_same_arc_identity() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        f.write_raw_inode(11, &make_empty_file_inode());

        let a = f.ext4.read_inode(11).unwrap();
        let b = f.ext4.read_inode(11).unwrap();
        assert!(Arc::ptr_eq(&a, &b));

        // A different inode is a different identity.
        f.write_raw_inode(12, &make_empty_file_inode());
        let c = f.ext4.read_inode(12).unwrap();
        assert!(!Arc::ptr_eq(&a, &c));
    }

    /// `fs.sync_all()` flushes a dirty inode's metadata to the on-disk inode
    /// table: after a write + `sync_all`, the raw inode read straight from the
    /// device segment carries the new size and `i_blocks`.
    #[ktest]
    fn sync_all_flushes_dirty_inode() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        f.write_raw_inode(11, &make_empty_file_inode());

        let inode = f.ext4.read_inode(11).unwrap();
        write_all(&inode, 0, &[0xAB; BLOCK_SIZE]);
        assert_eq!(inode.size(), BLOCK_SIZE);

        // No fsync on this inode; the only flush is the filesystem-level sync.
        f.ext4.sync_all().unwrap();

        let raw = f.read_raw_inode(11);
        assert_eq!(raw.size_lo, BLOCK_SIZE as u32);
        assert_eq!(raw.sector_count as u64, SECTORS_PER_BLOCK);
    }

    /// The corruption fix: a truncate on inode B that frees blocks must leave the
    /// on-disk inode and the block bitmap mutually consistent after `sync_all`.
    ///
    /// File A is written and fsync'd (its blocks persist). Then a *separate*
    /// inode B is truncated, freeing trailing blocks — dirtying the global bitmap
    /// and B's in-memory inode but persisting neither yet. `fs.sync_all()` (the
    /// clean-unmount path) must flush B's trimmed inode together with the bitmap:
    /// on disk, B's size reflects the truncate, every block B's extents still
    /// reference is allocated, and the freed trailing block is marked free.
    #[ktest]
    fn cross_inode_truncate_consistent_after_sync_all() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        f.write_raw_inode(11, &make_empty_file_inode()); // file A
        f.write_raw_inode(12, &make_empty_file_inode()); // file B

        // File A: write one block and fsync it (allocations persist to disk).
        let a = f.ext4.read_inode(11).unwrap();
        write_all(&a, 0, &[0xAA; BLOCK_SIZE]);
        a.sync_data_and_meta().unwrap();

        // File B: a 3-block file, then truncate to 1 block, freeing 2 trailing
        // blocks. The truncate updates the in-memory inode + the global bitmap
        // but does not, on its own, write B's inode back to disk.
        let b = f.ext4.read_inode(12).unwrap();
        write_all(&b, 0, &[0xBB; 3 * BLOCK_SIZE]);
        b.sync_data_and_meta().unwrap(); // B's 3 blocks are on disk and allocated

        let b2_pblock = ondisk_pblock_of(&f.read_raw_inode(12), 2).unwrap();
        assert!(block_is_allocated(&f, b2_pblock));

        b.resize(BLOCK_SIZE).unwrap();
        assert_eq!(b.size(), BLOCK_SIZE);

        // The clean-unmount path: flush all inodes + the block-side metadata.
        f.ext4.sync_all().unwrap();

        // B's on-disk inode reflects the truncate.
        let raw_b = f.read_raw_inode(12);
        assert_eq!(raw_b.size_lo, BLOCK_SIZE as u32);
        assert_eq!(raw_b.sector_count as u64, SECTORS_PER_BLOCK);

        // Consistency: every block B's on-disk extents still reference is marked
        // allocated, and the freed trailing block is now free in the bitmap.
        let b0_pblock = ondisk_pblock_of(&raw_b, 0).unwrap();
        assert!(block_is_allocated(&f, b0_pblock));
        assert!(ondisk_pblock_of(&raw_b, 2).is_none());
        assert!(!block_is_allocated(&f, b2_pblock));

        // A is untouched: its single block is still mapped and allocated.
        let a0_pblock = ondisk_pblock_of(&f.read_raw_inode(11), 0).unwrap();
        assert!(block_is_allocated(&f, a0_pblock));
    }

    /// Parses an inline depth-0 extent header and returns `(magic, entries)`.
    fn ondisk_extent_header(raw: &RawInode) -> (u16, u16) {
        let magic = (raw.block[0] & 0xFFFF) as u16;
        let entries = ((raw.block[0] >> 16) & 0xFFFF) as u16;
        (magic, entries)
    }

    /// alloc_ino then free_inode restores the group and superblock free-inode
    /// counters exactly.
    #[ktest]
    fn alloc_ino_free_inode_round_trip() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_inode_bitmap_metadata_marked()
            .build()
            .unwrap();

        let before_sb = f.ext4.super_block().free_inodes_count();
        let before_group = f.ext4.block_group(0).free_inodes_count();

        let ino = f.ext4.alloc_ino(ROOT_INO, InodeType::File).unwrap();
        assert_eq!(ino, 11); // first free inode after the 10 reserved ones
        assert_eq!(f.ext4.super_block().free_inodes_count(), before_sb - 1);
        assert_eq!(f.ext4.block_group(0).free_inodes_count(), before_group - 1);

        f.ext4.free_inode(ino, InodeType::File).unwrap();
        assert_eq!(f.ext4.super_block().free_inodes_count(), before_sb);
        assert_eq!(f.ext4.block_group(0).free_inodes_count(), before_group);
    }

    /// A full first group rings the allocation into the next group.
    #[ktest]
    fn alloc_ino_rings_to_next_group() {
        // A 2-group image with the normal reserved layout. Group 0 has 32 inodes
        // (10 reserved -> 22 free); exhaust them so the next allocation, with its
        // parent in group 0, must ring into group 1.
        let f = Ext4FixtureBuilder::new(256, 32, 2 * 256)
            .with_inode_bitmap_metadata_marked()
            .build()
            .unwrap();

        // Group 0 has 32 inodes, 10 reserved -> 22 free. Allocate all 22 so the
        // next allocation must ring into group 1.
        for _ in 0..22 {
            let ino = f.ext4.alloc_ino(ROOT_INO, InodeType::File).unwrap();
            assert!(ino <= 32, "ino {} should land in group 0", ino);
        }
        assert_eq!(f.ext4.block_group(0).free_inodes_count(), 0);

        // The 23rd allocation, with parent in group 0, rings to group 1.
        let ino = f.ext4.alloc_ino(ROOT_INO, InodeType::File).unwrap();
        assert!(ino > 32, "ino {} should ring into group 1", ino);
        assert_eq!(((ino - 1) / 32) as usize, 1);
    }

    /// A directory allocation bumps `used_dirs_count`; freeing it drops it back.
    #[ktest]
    fn alloc_dir_tracks_used_dirs_count() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_inode_bitmap_metadata_marked()
            .build()
            .unwrap();

        let before = f.ext4.block_group(0).used_dirs_count();
        let ino = f.ext4.alloc_ino(ROOT_INO, InodeType::Dir).unwrap();
        assert_eq!(f.ext4.block_group(0).used_dirs_count(), before + 1);

        f.ext4.free_inode(ino, InodeType::Dir).unwrap();
        assert_eq!(f.ext4.block_group(0).used_dirs_count(), before);
    }

    /// A non-directory allocation leaves `used_dirs_count` untouched.
    #[ktest]
    fn alloc_file_does_not_touch_used_dirs_count() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_inode_bitmap_metadata_marked()
            .build()
            .unwrap();

        let before = f.ext4.block_group(0).used_dirs_count();
        let ino = f.ext4.alloc_ino(ROOT_INO, InodeType::File).unwrap();
        assert_eq!(f.ext4.block_group(0).used_dirs_count(), before);
        f.ext4.free_inode(ino, InodeType::File).unwrap();
        assert_eq!(f.ext4.block_group(0).used_dirs_count(), before);
    }

    /// alloc_ino with no free inodes returns ENOSPC.
    #[ktest]
    fn alloc_ino_enospc() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_no_free_inodes()
            .build()
            .unwrap();
        assert_eq!(
            f.ext4
                .alloc_ino(ROOT_INO, InodeType::File)
                .unwrap_err()
                .error(),
            Errno::ENOSPC
        );
    }

    /// create_inode of a regular file produces a valid empty extent root with the
    /// EXTENTS flag, size 0, blocks 0, and link count 1.
    #[ktest]
    fn create_file_inode_has_empty_extent_root() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_inode_bitmap_metadata_marked()
            .build()
            .unwrap();

        let perm = FilePerm::from_bits_truncate(0o644);
        let inode = f
            .ext4
            .create_inode(ROOT_INO, InodeType::File, perm)
            .unwrap();
        assert_eq!(inode.inode_type(), InodeType::File);
        assert_eq!(inode.size(), 0);
        assert_eq!(inode.sector_count(), 0);
        assert_eq!(inode.link_count(), 1);

        // The on-disk inode carries S_IFREG, the EXTENTS flag, and a valid empty
        // extent root (magic 0xF30A, 0 entries).
        let raw = f.read_raw_inode(inode.ino());
        assert_eq!(raw.mode & 0o170000, 0o100000); // S_IFREG
        assert_eq!(raw.mode & 0o7777, 0o644);
        assert_ne!(raw.flags & 0x0008_0000, 0); // EXT4_EXTENTS_FL
        let (magic, entries) = ondisk_extent_header(&raw);
        assert_eq!(magic, 0xF30A);
        assert_eq!(entries, 0);
        assert_eq!(raw.size_lo, 0);
        assert_eq!(raw.sector_count, 0);
        assert_eq!(raw.link_count, 1);
    }

    /// create_inode of a directory sets link count 2 (for the `.` self-link) and
    /// the directory type bits.
    #[ktest]
    fn create_dir_inode_has_link_count_two() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_inode_bitmap_metadata_marked()
            .build()
            .unwrap();

        let perm = FilePerm::from_bits_truncate(0o755);
        let inode = f.ext4.create_inode(ROOT_INO, InodeType::Dir, perm).unwrap();
        assert_eq!(inode.inode_type(), InodeType::Dir);
        assert_eq!(inode.link_count(), 2);

        let raw = f.read_raw_inode(inode.ino());
        assert_eq!(raw.mode & 0o170000, 0o040000); // S_IFDIR
        assert_eq!(raw.link_count, 2);
        let (magic, entries) = ondisk_extent_header(&raw);
        assert_eq!(magic, 0xF30A);
        assert_eq!(entries, 0);
    }

    /// The generation stamped onto a created inode increments across calls.
    #[ktest]
    fn create_inode_generation_increments() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_inode_bitmap_metadata_marked()
            .build()
            .unwrap();

        let perm = FilePerm::from_bits_truncate(0o644);
        let a = f
            .ext4
            .create_inode(ROOT_INO, InodeType::File, perm)
            .unwrap();
        let b = f
            .ext4
            .create_inode(ROOT_INO, InodeType::File, perm)
            .unwrap();

        let gen_a = f.read_raw_inode(a.ino()).generation;
        let gen_b = f.read_raw_inode(b.ino()).generation;
        assert_eq!(gen_b, gen_a.wrapping_add(1));
    }

    /// create_inode rolls back the inode allocation when the on-disk writeback
    /// fails: the bitmap bit is cleared and the superblock counter restored.
    #[ktest]
    fn create_inode_rolls_back_on_write_failure() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_inode_bitmap_metadata_marked()
            .build()
            .unwrap();

        let before_sb = f.ext4.super_block().free_inodes_count();
        let before_group = f.ext4.block_group(0).free_inodes_count();

        // Force the inode writeback to fail, so create_inode must roll back.
        f.disk.set_fail_writes(true);
        let perm = FilePerm::from_bits_truncate(0o644);
        // `Arc<Inode>` is not `Debug`, so match instead of `unwrap_err`.
        let err = match f.ext4.create_inode(ROOT_INO, InodeType::File, perm) {
            Ok(_) => panic!("create_inode unexpectedly succeeded despite write failure"),
            Err(err) => err,
        };
        assert_eq!(err.error(), Errno::EIO);
        f.disk.set_fail_writes(false);

        // Allocation fully rolled back: counters restored and the freshly taken
        // bit (group-local index 10, i.e. ino 11) is clear again.
        assert_eq!(f.ext4.super_block().free_inodes_count(), before_sb);
        assert_eq!(f.ext4.block_group(0).free_inodes_count(), before_group);
        let group = f.ext4.block_group(0);
        assert!(!group.metadata().inode_bitmap.is_allocated(10));
    }

    /// sync_metadata writes the inode bitmap and the inode-count descriptor
    /// fields back losslessly: after an inode alloc + sync, the on-disk group
    /// descriptor reflects the new free-inode and used-dirs counts.
    #[ktest]
    fn sync_metadata_persists_inode_counts() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_inode_bitmap_metadata_marked()
            .build()
            .unwrap();

        let gdt_offset = (f.ext4.super_block().first_data_block() as usize + 1) * BLOCK_SIZE;
        let raw_before = f
            .disk
            .segment()
            .read_val::<RawBlockGroup>(gdt_offset)
            .unwrap();

        // Allocate a directory inode (touches both free_inodes and used_dirs).
        let ino = f.ext4.alloc_ino(ROOT_INO, InodeType::Dir).unwrap();
        f.ext4.sync_metadata().unwrap();

        let raw_after = f
            .disk
            .segment()
            .read_val::<RawBlockGroup>(gdt_offset)
            .unwrap();
        assert_eq!(
            raw_after.free_inodes_count_lo,
            raw_before.free_inodes_count_lo - 1
        );
        assert_eq!(
            raw_after.used_dirs_count_lo,
            raw_before.used_dirs_count_lo + 1
        );
        // The inode bitmap bit for the allocated inode is set on disk.
        let local_idx = (ino - 1) as usize; // group 0
        let inode_bitmap_off = (f.ext4.block_group(0).first_block() as usize + 3) * BLOCK_SIZE;
        let mut bitmap = vec![0u8; BLOCK_SIZE];
        f.disk
            .segment()
            .read_bytes(inode_bitmap_off, &mut bitmap)
            .unwrap();
        assert_ne!(bitmap[local_idx / 8] & (1 << (local_idx % 8)), 0);

        // The superblock free-inode counter is patched too.
        let raw_sb = f
            .disk
            .segment()
            .read_val::<RawSuperBlock>(SUPER_BLOCK_OFFSET)
            .unwrap();
        assert_eq!(
            raw_sb.free_inodes_count,
            f.ext4.super_block().free_inodes_count()
        );
    }
}
