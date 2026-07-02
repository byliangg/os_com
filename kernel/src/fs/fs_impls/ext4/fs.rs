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
    journal,
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
    /// Guards the on-disk orphan list (jbd2 `s_orphan_lock`). **Phase 3
    /// scaffolding:** the orphan-list operations are no-ops
    /// ([`journal::orphan_add`](super::journal)/`orphan_del`), so this lock is
    /// declared but never taken. Phase 4 acquires it around the orphan-chain
    /// update — ordered as a leaf taken *after* the journal handle and *before*
    /// the superblock (report §5.1) — when it fills the no-op bodies.
    #[expect(dead_code)] // Phase 4 acquires this; see `journal::orphan_add`.
    s_orphan_lock: Mutex<()>,
    /// The JBD2 journal, present iff the volume carries the `HAS_JOURNAL` compat
    /// feature (jbd2 `journal_t`).
    ///
    /// A settable cell, not a plain field, because the journal can only be built
    /// *after* the `Ext4` `Arc` exists: [`journal::load_geometry`] reads the
    /// journal inode (ino 8) *through* the filesystem, so the fs must already be
    /// live. [`Ext4::open`] therefore constructs `Ext4` with `journal: None`, then
    /// loads/recovers/starts the journal and stores it here exactly once. A
    /// non-journaled volume leaves this `None` forever — the Phase 1–3 behavior is
    /// preserved byte-for-byte. It is set once and only read thereafter, so a plain
    /// `RwMutex<Option<_>>` (no interior invariants to uphold) suffices.
    journal: RwMutex<Option<Arc<journal::Journal>>>,
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

        let ext4 = Arc::new_cyclic(|weak| Ext4 {
            block_device: device,
            super_block: RwMutex::new(Dirty::new(super_block)),
            block_groups,
            nr_inodes_per_group,
            next_generation: AtomicU32::new(utils::now().as_secs() as u32),
            s_orphan_lock: Mutex::new(()),
            journal: RwMutex::new(None),
            fs_event_subscriber_stats: FsEventSubscriberStats::new(),
            self_ref: weak.clone(),
        });

        // Journal mount lifecycle (jbd2 `jbd2_journal_load` + recovery + start of
        // `kjournald`). Done here, after the `Ext4` `Arc` exists, because
        // `load_geometry` resolves the journal inode's blocks *through* the fs.
        //
        // For a non-journaled volume `load_geometry` returns `None` and this whole
        // block is a pure no-op — the Phase 1–3 mount path is unchanged.
        if let Some(geometry) = journal::load_geometry(&ext4)? {
            let journal = journal::Journal::new(geometry, ext4.block_device().clone());

            // Recover a dirty journal (crashed mount) BEFORE any normal operation,
            // so the filesystem is consistent before the commit thread or any op
            // touches it. `needs_recovery()` is true when the on-disk `RECOVER`
            // incompat bit is set (or an orphan chain is pending). `recover` is a
            // no-op if the journal superblock is already clean (`s_start == 0`).
            if ext4.super_block().needs_recovery() {
                journal::recover(&journal, ext4.block_device().as_ref())?;
                // jbd2 clears the on-disk `INCOMPAT_RECOVER` bit once the log has
                // been replayed, so a subsequent clean mount does not re-recover.
                ext4.super_block.write().clear_recover();
                ext4.sync_metadata()?;
            }

            // Start `kjournald` and publish the journal. Storing it only *after*
            // the thread is running means every error path above drops the `Ext4`
            // `Arc` while `journal` is still `None`, so `Ext4::drop` finds nothing
            // to stop — no half-started thread is ever leaked.
            journal.start_commit_thread();
            *ext4.journal.write() = Some(journal);
        }

        Ok(ext4)
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

    /// Returns a clone of the loaded journal, or `None` on a non-journaled volume.
    pub(super) fn journal(&self) -> Option<Arc<journal::Journal>> {
        self.journal.read().clone()
    }

    /// Journal credits (an upper bound on the distinct metadata blocks the
    /// operation may dirty) reserved per operation type.
    ///
    /// Phase 4 uses generous fixed estimates; a real `mke2fs` journal admits
    /// hundreds of credits, so these never bind (and the tiny ktest journals
    /// never run operations). Precise per-op credit accounting and `extend`/
    /// `restart` for unbounded writes are a P7 refinement.
    pub(super) const CREATE_CREDITS: usize = 16;
    pub(super) const WRITE_CREDITS: usize = 16;
    pub(super) const TRUNCATE_CREDITS: usize = 16;
    pub(super) const RECLAIM_CREDITS: usize = 16;
    pub(super) const UNLINK_CREDITS: usize = 16;
    pub(super) const LINK_CREDITS: usize = 16;
    pub(super) const RENAME_CREDITS: usize = 24;
    /// An `fsync`/sync inode writeback captures exactly one inode-table block;
    /// kept small so it also fits the deliberately tiny ktest journals.
    pub(super) const FSYNC_CREDITS: usize = 4;

    /// Opens a journal handle for a metadata operation, reserving `credits`
    /// metadata blocks (jbd2 `jbd2_journal_start`), or a no-op handle on a
    /// non-journaled volume.
    ///
    /// The operation opens this **after** taking its inode `inner` lock (lock
    /// order: `inner` ① → handle ②), threads [`OpHandle::get`](journal::OpHandle::get)
    /// into the metadata funnels, and lets the returned [`OpHandle`](journal::OpHandle)
    /// drop at the end of the operation to close the handle (and, when it is the
    /// transaction's last, signal a commit — asynchronously).
    pub(super) fn begin_op(&self, credits: usize) -> Result<journal::OpHandle> {
        match self.journal() {
            Some(journal) => journal::OpHandle::start(&journal, credits),
            None => Ok(journal::OpHandle::none()),
        }
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

    pub(super) fn this(&self) -> Weak<Ext4> {
        self.self_ref.clone()
    }

    /// Allocates up to `count` contiguous blocks, preferring the group that owns
    /// `goal`.
    ///
    /// Searches groups in a ring starting from the goal group. Returns
    /// `Err(ENOSPC)` if no group can satisfy the request, `Err(EINVAL)` if
    /// `count` is zero.
    pub(super) fn alloc_blocks(
        &self,
        count: u32,
        goal: Ext4Bid,
        handle: Option<&journal::Handle>,
    ) -> Result<Range<Ext4Bid>> {
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

            let range = group.alloc_blocks(count, sb_free_blocks, handle)?;
            if !range.is_empty() {
                let allocated_count = range.end - range.start;
                sb.dec_free_blocks(allocated_count)?;
                journal_superblock(
                    handle,
                    sb.free_blocks_count(),
                    sb.free_inodes_count(),
                    sb.last_orphan(),
                )?;
                return Ok(range);
            }
        }

        return_errno_with_message!(Errno::ENOSPC, "no free blocks available in any group");
    }

    /// Frees `count` blocks starting at `start`, splitting across groups.
    pub(super) fn free_blocks(
        &self,
        start: Ext4Bid,
        count: u32,
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
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
                group.free_blocks(group_start_bit..(group_start_bit + blocks_in_group), handle)?;
            if freed_count > 0 {
                sb.inc_free_blocks(freed_count as u64)?;
                journal_superblock(
                    handle,
                    sb.free_blocks_count(),
                    sb.free_inodes_count(),
                    sb.last_orphan(),
                )?;
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
    pub(super) fn alloc_ino(
        &self,
        parent_ino: Ext4Ino,
        type_: InodeType,
        handle: Option<&journal::Handle>,
    ) -> Result<Ext4Ino> {
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

            let Some(local_idx) = group.alloc_ino(type_, handle)? else {
                continue;
            };

            let ino = (group_idx as u32) * nr_inodes_per_group + local_idx + 1;
            if ino < sb.first_ino() || ino > total_inodes {
                // Roll back the group-level allocation before erroring out.
                let _ = group.free_inode(local_idx, type_, handle);
                return_errno_with_message!(Errno::EIO, "allocated inode number out of valid range");
            }
            sb.dec_free_inodes()?;
            journal_superblock(
                handle,
                sb.free_blocks_count(),
                sb.free_inodes_count(),
                sb.last_orphan(),
            )?;

            return Ok(ino);
        }

        return_errno_with_message!(Errno::ENOSPC, "no free inodes available in any group");
    }

    /// Frees an inode by number, mirroring [`free_blocks`] on the block side.
    pub(super) fn free_inode(
        &self,
        ino: Ext4Ino,
        type_: InodeType,
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        let mut sb = self.super_block.write();
        let group = self.find_group(ino)?;
        let local_idx = (ino - 1) % self.nr_inodes_per_group;

        let was_allocated = group.free_inode(local_idx, type_, handle)?;
        if was_allocated {
            sb.inc_free_inodes()?;
            journal_superblock(
                handle,
                sb.free_blocks_count(),
                sb.free_inodes_count(),
                sb.last_orphan(),
            )?;
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
    //
    // Reached from `Inode::create` (the namespace entry point not yet wired into
    // the VFS); that root's `expect(dead_code)` marker keeps this helper and
    // everything below it (`alloc_ino`/`free_inode`/`write_new_inode_desc`/
    // `InodeDesc::new`) reachable, so none of those need their own marker.
    pub(super) fn create_inode(
        &self,
        parent_ino: Ext4Ino,
        type_: InodeType,
        perm: FilePerm,
        handle: Option<&journal::Handle>,
    ) -> Result<Arc<Inode>> {
        let ino = self.alloc_ino(parent_ino, type_, handle)?;

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

        if let Err(err) = self.write_new_inode_desc(ino, &inode_desc, handle) {
            // Roll back the inode allocation: clear the bitmap bit and restore
            // the superblock free-inode counter.
            if let Err(free_err) = self.free_inode(ino, type_, handle) {
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

    /// Inserts a newly created inode into the live block-group cache, routing it
    /// to its owning group. Mirrors ext2 `Ext2::insert_inode`.
    pub(super) fn insert_inode(&self, inode: Arc<Inode>) {
        if let Ok(group) = self.find_group(inode.ino()) {
            group.insert_inode(inode);
        }
    }

    /// Removes one inode from the live block-group cache. Mirrors ext2
    /// `Ext2::remove_inode`. Called by the unlink/rmdir path once a child's link
    /// count reaches 0, dropping the cache's reference so the last surviving
    /// `Arc` reclaims the inode on `Drop`.
    pub(super) fn remove_inode(&self, ino: Ext4Ino) -> Option<Arc<Inode>> {
        self.find_group(ino)
            .ok()
            .and_then(|group| group.remove_inode(ino))
    }

    /// Returns whether `ino` is marked allocated in its owning group's inode
    /// bitmap. Used by the reclaim path to skip an already-freed inode. Mirrors
    /// ext2 routing through the owning block group.
    pub(super) fn is_inode_allocated(&self, ino: Ext4Ino) -> bool {
        self.find_group(ino)
            .map(|group| group.is_inode_allocated(ino))
            .unwrap_or(false)
    }

    /// Writes back the superblock and every dirty group descriptor/bitmap.
    pub(super) fn sync_metadata(&self) -> Result<()> {
        for group in &self.block_groups {
            group.sync_metadata()?;
        }

        let mut sb = self.super_block.write();
        if sb.is_dirty() {
            // RMW the on-disk superblock: patch only the free-block and
            // free-inode counters and the incompatible-feature bits so every
            // other on-disk field is preserved losslessly. `feature_incompat` is
            // lossless to write back — in memory it only ever changes via
            // `SuperBlock::clear_recover` (jbd2 clearing `INCOMPAT_RECOVER` after
            // a mount-time recovery), so persisting it here is what makes a
            // recovered volume mount clean next time.
            let mut raw = self
                .block_device
                .read_val::<RawSuperBlock>(SUPER_BLOCK_OFFSET)
                .map_err(|_| {
                    Error::with_message(Errno::EIO, "failed to read superblock for sync")
                })?;
            raw.free_blocks_count = sb.free_blocks_count() as u32;
            raw.free_inodes_count = sb.free_inodes_count();
            raw.feature_incompat = sb.feature_incompat().bits();
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
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        let offset = self.inode_table_offset(ino)?;

        // Journaled path: the on-disk inode may be **stale** — a prior write to it
        // was suppressed (WAL) and has not yet been checkpointed — so a
        // read-modify-write from the device would resurrect that block's zeroed
        // `i_mode` type bits / `extra_isize` / `generation` (exactly the
        // corruption the guest e2fsck caught after a rename touched a
        // not-yet-checkpointed directory). Encode the whole inode from the
        // in-memory descriptor instead and capture it; checkpoint applies it to
        // the final location after the transaction commits.
        if handle.is_some() {
            let raw = build_raw_inode(desc, root);
            return journal_inode_block(handle, offset, raw.as_bytes());
        }

        // Non-journaled (or the sync path): read-modify-write the on-disk inode,
        // patching only the fields buffered writes mutate (size, `i_blocks`, the
        // extent root, timestamps, flags, link count, dtime) and preserving
        // everything else (`extra_isize`, checksums, generation, xattr tail, osd
        // fields) losslessly, then write it through. The device is authoritative
        // here, so the RMW is safe.
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

        // Deletion time (`i_dtime`, whole seconds). Zero for live inodes; the
        // reclaim path stamps it before the final writeback so a freed inode
        // carries a non-zero `i_dtime`, matching ext4 on-disk semantics.
        raw.dtime = desc.dtime().as_secs() as u32;

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
    fn write_new_inode_desc(
        &self,
        ino: Ext4Ino,
        desc: &InodeDesc,
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        let offset = self.inode_table_offset(ino)?;

        let raw = build_raw_inode(desc, desc.raw_block());

        journal_inode_block(handle, offset, raw.as_bytes())?;
        // See `write_back_inode_desc`: suppress the direct write under a handle so
        // the inode reaches its final location only via checkpoint (WAL).
        if handle.is_none() {
            self.block_device
                .write_val(offset, &raw)
                .map_err(|_| Error::with_message(Errno::EIO, "failed to write new inode"))?;
        }
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

/// Stops the journal's commit thread on unmount (jbd2 journal teardown, the
/// `jbd2_journal_destroy` step that halts `kjournald`).
///
/// This MUST run here, on the unmounting thread — never on the commit thread
/// itself, or [`Journal::stop_commit_thread`](journal::Journal::stop_commit_thread)
/// would join on itself and spin forever. It is structurally impossible for the
/// commit thread to reach this: that thread holds only a `Weak<Journal>` (see the
/// commit-thread model in [`journal`]), so it never contributes to the `Ext4`
/// strong count and can never be the last owner whose drop runs this. `Ext4::drop`
/// therefore always runs on a real owner's thread. For a non-journaled volume the
/// cell is `None` and this is a no-op.
impl Drop for Ext4 {
    fn drop(&mut self) {
        // `get_mut` on the `RwMutex` is lock-free here — `&mut self` proves we are
        // the sole owner, so there is no contention to guard against.
        if let Some(journal) = self.journal.get_mut().take() {
            journal.stop_commit_thread();
            // With the commit thread stopped we are the sole committer: flush the
            // final running transaction and checkpoint so the on-disk journal is
            // left clean (`s_start == 0`) for the next mount. A failure here cannot
            // be propagated out of `drop`; log it (the un-checkpointed log stays
            // replay-safe — a later mount would recover it).
            if let Err(e) = journal.flush_on_unmount() {
                error!("ext4 journal unmount flush failed: {:?}", e);
            }
        }
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

/// Builds the complete on-disk [`RawInode`] for `desc` with `root` as its inline
/// extent-tree root — every field from the in-memory descriptor, never from the
/// device.
///
/// This is the authoritative encoding used by both the new-inode write and the
/// *journaled* writeback. Under a handle the on-disk inode may be **stale** (its
/// previous write was suppressed and not yet checkpointed), so a read-modify-write
/// from the device would resurrect zeroed `i_mode` type bits, `extra_isize`,
/// `generation`, and nanosecond timestamps — the corruption the guest e2fsck
/// caught after a rename touched a directory whose creation had not yet
/// checkpointed. Encoding straight from `desc` (which `InodeDesc::try_from` loads
/// in full: type, generation, crtime, …) sidesteps that entirely.
fn build_raw_inode(desc: &InodeDesc, root: &[u32; super::inode::RAW_BLOCK_PTRS_LEN]) -> RawInode {
    let (mtime_secs, mtime_extra) = encode_time(desc.mtime());
    let (ctime_secs, ctime_extra) = encode_time(desc.ctime());
    let (atime_secs, atime_extra) = encode_time(desc.atime());
    let (crtime_secs, crtime_extra) = encode_time(desc.crtime());
    RawInode {
        // The full mode comes from `desc.type_()`, not the (possibly stale) device
        // — this is what preserves the `S_IFMT` type bits under journaling.
        mode: (desc.type_() as u16) | (desc.perm().bits() & 0o7777),
        uid: desc.uid() as u16,
        size_lo: desc.size() as u32,
        atime: atime_secs,
        ctime: ctime_secs,
        mtime: mtime_secs,
        dtime: desc.dtime().as_secs() as u32,
        gid: desc.gid() as u16,
        link_count: desc.link_count(),
        sector_count: desc.sector_count() as u32,
        flags: desc.flags().bits(),
        block: *root,
        generation: desc.generation(),
        size_high: if desc.type_() == InodeType::File {
            (desc.size() >> 32) as u32
        } else {
            0
        },
        blocks_high: (desc.sector_count() >> 32) as u16,
        uid_high: (desc.uid() >> 16) as u16,
        gid_high: (desc.gid() >> 16) as u16,
        // The `extra_isize` a 256-byte inode carries (32 bytes past the 128-byte
        // base), so the nanosecond timestamps are honored on read.
        extra_isize: 32,
        ctime_extra,
        mtime_extra,
        atime_extra,
        crtime: crtime_secs,
        crtime_extra,
        ..Default::default()
    }
}

/// The device block that holds the primary superblock. With 4 KiB blocks the
/// superblock lives at byte [`SUPER_BLOCK_OFFSET`] (1024) inside block 0, so
/// journaling it means capturing block 0.
const SUPERBLOCK_BID: Ext4Bid = (SUPER_BLOCK_OFFSET / BLOCK_SIZE) as Ext4Bid;

/// Captures the superblock's after-image into the operation's transaction
/// (block 0, RMW at [`SUPER_BLOCK_OFFSET`]), for op-time journaling. A no-op
/// without a handle.
///
/// Patches **every field the filesystem mutates after mount** —
/// `free_blocks_count`, `free_inodes_count`, and `s_last_orphan` — from the
/// caller's in-memory values, every capture. This is a single-writer rule, not a
/// convenience: a capture that patched only "its own" field would leave the
/// others at the seed value, so two captures patching disjoint fields in
/// different transactions would clobber each other's committed writes (the B-1
/// stale-seed class; the Task 8 first attempt hit exactly this with a
/// counts-only vs. orphan-only pair). Every untracked field (label, feature
/// words, mount counters — changed only at mount time, never under an
/// operation) survives from the seed. The values are absolute, so repeated
/// captures converge on the final state.
fn journal_superblock(
    handle: Option<&journal::Handle>,
    free_blocks: u64,
    free_inodes: u32,
    last_orphan: u32,
) -> Result<()> {
    journal::get_write_access(handle, SUPERBLOCK_BID, journal::TriggerType::Superblock)?;
    journal::dirty_metadata(
        handle,
        SUPERBLOCK_BID,
        journal::TriggerType::Superblock,
        |buf| {
            let off = SUPER_BLOCK_OFFSET;
            let mut raw = RawSuperBlock::from_bytes(&buf[off..off + size_of::<RawSuperBlock>()]);
            raw.free_blocks_count = free_blocks as u32;
            raw.free_inodes_count = free_inodes;
            raw.last_orphan = last_orphan;
            buf[off..off + size_of::<RawSuperBlock>()].copy_from_slice(raw.as_bytes());
        },
    )
}

/// Captures an inode's after-image into the operation's transaction, a sub-block
/// RMW of its inode-table block (the inode lives at byte `offset`, i.e. at
/// `offset % BLOCK_SIZE` within block `offset / BLOCK_SIZE`), for op-time
/// journaling. A no-op without a handle.
///
/// `inode_bytes` is exactly the `size_of::<RawInode>()` bytes the direct write
/// persists; patching only those preserves the rest of the block — the other
/// inodes sharing it and the slot bytes past the written `RawInode` prefix —
/// from the seed. The partial patch is sound only because the seed is current:
/// `get_write_access` seeds from the newest committed-but-un-checkpointed image
/// of the block when one is retained, falling back to the device (see
/// `UncheckpointedImage` in `journal/transaction.rs`; a raw device seed would
/// lag pending checkpoints and silently clobber the neighboring inodes — the
/// Task 8 guest data loss). (Inode-table blocks are inode-size aligned, so an
/// inode never straddles a block boundary.)
fn journal_inode_block(
    handle: Option<&journal::Handle>,
    offset: usize,
    inode_bytes: &[u8],
) -> Result<()> {
    let inode_block = (offset / BLOCK_SIZE) as Ext4Bid;
    let in_block_off = offset % BLOCK_SIZE;
    journal::get_write_access(handle, inode_block, journal::TriggerType::InodeTable)?;
    journal::dirty_metadata(
        handle,
        inode_block,
        journal::TriggerType::InodeTable,
        |buf| {
            buf[in_block_off..in_block_off + inode_bytes.len()].copy_from_slice(inode_bytes);
        },
    )
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
            if let Err(err) = self.fs.free_blocks(range.start, count, None) {
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
        let range = f.ext4.alloc_blocks(8, goal, None).unwrap();
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

        f.ext4.free_blocks(range.start, alloc_len, None).unwrap();
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

        let range = f.ext4.alloc_blocks(4, group1_first, None).unwrap();
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
                .alloc_blocks(1, f_full.ext4.block_group(0).first_block(), None)
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
                .alloc_blocks(0, f.ext4.block_group(0).first_block(), None)
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
            .alloc_blocks(4, f.ext4.block_group(0).first_block(), None)
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
            .alloc_blocks(4, f.ext4.block_group(0).first_block(), None)
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
            .alloc_blocks(4, f.ext4.block_group(0).first_block(), None)
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

        let ino = f.ext4.alloc_ino(ROOT_INO, InodeType::File, None).unwrap();
        assert_eq!(ino, 11); // first free inode after the 10 reserved ones
        assert_eq!(f.ext4.super_block().free_inodes_count(), before_sb - 1);
        assert_eq!(f.ext4.block_group(0).free_inodes_count(), before_group - 1);

        f.ext4.free_inode(ino, InodeType::File, None).unwrap();
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
            let ino = f.ext4.alloc_ino(ROOT_INO, InodeType::File, None).unwrap();
            assert!(ino <= 32, "ino {} should land in group 0", ino);
        }
        assert_eq!(f.ext4.block_group(0).free_inodes_count(), 0);

        // The 23rd allocation, with parent in group 0, rings to group 1.
        let ino = f.ext4.alloc_ino(ROOT_INO, InodeType::File, None).unwrap();
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
        let ino = f.ext4.alloc_ino(ROOT_INO, InodeType::Dir, None).unwrap();
        assert_eq!(f.ext4.block_group(0).used_dirs_count(), before + 1);

        f.ext4.free_inode(ino, InodeType::Dir, None).unwrap();
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
        let ino = f.ext4.alloc_ino(ROOT_INO, InodeType::File, None).unwrap();
        assert_eq!(f.ext4.block_group(0).used_dirs_count(), before);
        f.ext4.free_inode(ino, InodeType::File, None).unwrap();
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
                .alloc_ino(ROOT_INO, InodeType::File, None)
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
            .create_inode(ROOT_INO, InodeType::File, perm, None)
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
        let inode = f
            .ext4
            .create_inode(ROOT_INO, InodeType::Dir, perm, None)
            .unwrap();
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
            .create_inode(ROOT_INO, InodeType::File, perm, None)
            .unwrap();
        let b = f
            .ext4
            .create_inode(ROOT_INO, InodeType::File, perm, None)
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
        let err = match f.ext4.create_inode(ROOT_INO, InodeType::File, perm, None) {
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
        let ino = f.ext4.alloc_ino(ROOT_INO, InodeType::Dir, None).unwrap();
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

    // --- Int-A: journal mount lifecycle (load / recover / start / stop). ---

    use super::super::{feature::FeatureIncompatSet, test_utils::JOURNAL_START_BLOCK};

    /// The on-disk `RECOVER` incompatible feature bit.
    const RECOVER_BIT: u32 = FeatureIncompatSet::RECOVER.bits();

    /// A journaled volume (clean journal) loads the journal at mount time and the
    /// commit thread starts; dropping the `Ext4` stops it cleanly (no panic/hang).
    #[ktest]
    fn journaled_mount_loads_journal_and_drops_cleanly() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_journal_inode(16)
            .build()
            .unwrap();

        // The journal was loaded and its commit thread started.
        assert!(f.ext4.journal().is_some());

        // Drop the `Ext4` explicitly: `stop_commit_thread` must run and join the
        // idle commit thread without hanging. Only the fixture's `disk` Arc lingers.
        drop(f);
    }

    /// Int-B B2.3: an operation on a journaled volume opens a handle and captures
    /// the metadata it dirties. Allocating a block captures the block-bitmap and
    /// group-descriptor after-images into the running transaction.
    #[ktest]
    fn journaled_op_captures_allocation_metadata() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_journal_inode(16)
            .build()
            .unwrap();
        let journal = f.ext4.journal().unwrap();
        // Stop the background committer so the running transaction stays
        // inspectable — no async commit races the assertion.
        journal.stop_commit_thread();

        {
            // `begin_op` reserves a few credits (< the 16-block log's capacity);
            // `alloc_blocks` then dirties the block bitmap + group descriptor.
            let op = f.ext4.begin_op(4).unwrap();
            let range = f.ext4.alloc_blocks(1, 0, op.get()).unwrap();
            assert_eq!(range.end - range.start, 1);

            let captured = journal.running_nr_metadata_blocks();
            assert!(
                captured >= 2,
                "block bitmap + group descriptor captured, got {captured}"
            );
            // `op` drops here: journal_stop (its request_commit wakes the stopped
            // thread, a no-op).
        }

        // Drop the fs: Ext4::drop stops (idempotent) then flush_on_unmount commits
        // and checkpoints the captured transaction, leaving the journal clean.
        drop(f);
    }

    /// Int-B B3: under a handle, an inode writeback is *suppressed* (the direct
    /// write to its final location does not happen), the after-image is captured,
    /// and only checkpoint writes it to the final location — write-ahead logging.
    /// This is the correctness B2's clean-unmount could not show, since B2's
    /// direct writes masked whether the captured bytes were right.
    #[ktest]
    fn journaled_inode_writeback_is_suppressed_until_checkpoint() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_journal_inode(16)
            .build()
            .unwrap();
        let journal = f.ext4.journal().unwrap();
        journal.stop_commit_thread();

        let ino = ROOT_INO;
        let mut desc = f.ext4.read_inode_desc(ino).unwrap();
        let new_link = desc.link_count() + 7;
        desc.set_link_count(new_link);
        let root = *desc.raw_block();
        let offset = f.ext4.inode_table_offset(ino).unwrap();
        let before: RawInode = f.disk.segment().read_val(offset).unwrap();

        let op = f.ext4.begin_op(4).unwrap();
        f.ext4
            .write_back_inode_desc(ino, &desc, &root, op.get())
            .unwrap();

        // Suppressed: the on-disk inode is UNCHANGED — the write went only to the
        // running transaction, not to its final location.
        let after_op: RawInode = f.disk.segment().read_val(offset).unwrap();
        assert_eq!(
            after_op.link_count, before.link_count,
            "the direct write is suppressed under a handle"
        );
        assert!(
            journal.running_nr_metadata_blocks() >= 1,
            "the inode block was captured"
        );
        drop(op);

        // Checkpoint (via the unmount flush) applies the captured after-image to
        // the final location — so the captured bytes were correct.
        journal.flush_on_unmount().unwrap();
        let after_ckpt: RawInode = f.disk.segment().read_val(offset).unwrap();
        assert_eq!(
            after_ckpt.link_count, new_link,
            "checkpoint wrote the captured inode to its final location"
        );
    }

    /// Int-B B3 regression (the bug the guest e2fsck caught): a journaled inode
    /// writeback must rebuild the inode from the in-memory descriptor, NOT
    /// read-modify-write the on-disk block — which under WAL can be stale (a prior
    /// write suppressed, not yet checkpointed). Here the on-disk slot is zeroed to
    /// stand in for that stale block; the writeback must still preserve the type
    /// bits, `extra_isize`, and generation from the descriptor.
    #[ktest]
    fn journaled_inode_writeback_rebuilds_from_desc_not_stale_disk() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_journal_inode(16)
            .build()
            .unwrap();
        let journal = f.ext4.journal().unwrap();
        journal.stop_commit_thread();

        let ino = ROOT_INO;
        let desc = f.ext4.read_inode_desc(ino).unwrap();
        let type_bits = (desc.type_() as u16) & 0xF000;
        assert_ne!(type_bits, 0, "root is a directory (S_IFDIR)");
        let generation = desc.generation();
        let root = *desc.raw_block();
        let offset = f.ext4.inode_table_offset(ino).unwrap();

        // Simulate the stale/suppressed on-disk inode: zero its slot.
        f.disk
            .segment()
            .write_val(offset, &RawInode::default())
            .unwrap();

        // Journaled writeback + checkpoint.
        let op = f.ext4.begin_op(4).unwrap();
        f.ext4
            .write_back_inode_desc(ino, &desc, &root, op.get())
            .unwrap();
        drop(op);
        journal.flush_on_unmount().unwrap();

        // Despite the zeroed on-disk block, the inode was rebuilt from the
        // descriptor: type bits, extra_isize and generation are intact (a
        // RMW-from-the-zeroed-disk would have lost all three — the guest e2fsck
        // "unknown file type" / "i_blocks wrong" corruption).
        let after: RawInode = f.disk.segment().read_val(offset).unwrap();
        assert_eq!(after.mode & 0xF000, type_bits, "S_IFMT type bits preserved");
        assert_eq!(after.extra_isize, 32, "extra_isize preserved");
        assert_eq!(after.generation, generation, "generation preserved");
        assert_eq!(after.link_count, desc.link_count(), "link count written");
    }

    /// T1 regression (the Task-8 guest silent data loss, B-1 class): an inode
    /// written back in transaction 1 must survive a *neighbor* inode's writeback
    /// in transaction 2 while txn 1 is committed but not yet checkpointed.
    ///
    /// Inodes 2 (root) and 8 (journal) share one inode-table block (16 × 256 B
    /// per 4 KiB block). Txn 2's capture of that block must seed from txn 1's
    /// retained after-image — seeding from the device (which lags until
    /// checkpoint) resurrects inode 2's old bytes, and checkpoint (tid order,
    /// newest wins) clobbers txn 1's write: exactly how the guest's `rm f2`
    /// emptied the unrelated f1.
    #[ktest]
    fn journaled_neighbor_inode_survives_cross_transaction_capture() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_journal_inode(16)
            .build()
            .unwrap();
        let journal = f.ext4.journal().unwrap();
        journal.stop_commit_thread();

        // Txn 1: inode A (root) gets a new link count.
        let ino_a = ROOT_INO;
        let mut desc_a = f.ext4.read_inode_desc(ino_a).unwrap();
        let new_link_a = desc_a.link_count() + 7;
        desc_a.set_link_count(new_link_a);
        let root_a = *desc_a.raw_block();
        {
            let op = f.ext4.begin_op(4).unwrap();
            f.ext4
                .write_back_inode_desc(ino_a, &desc_a, &root_a, op.get())
                .unwrap();
        }
        // Commit txn 1 but do NOT checkpoint: the on-disk inode table still holds
        // A's OLD link count — the async-commit window between two ops.
        journal.commit_now_for_test();

        // Txn 2: neighbor inode B (the journal inode, same table block).
        let ino_b = JOURNAL_INO;
        let mut desc_b = f.ext4.read_inode_desc(ino_b).unwrap();
        let new_link_b = desc_b.link_count() + 3;
        desc_b.set_link_count(new_link_b);
        let root_b = *desc_b.raw_block();
        {
            let op = f.ext4.begin_op(4).unwrap();
            f.ext4
                .write_back_inode_desc(ino_b, &desc_b, &root_b, op.get())
                .unwrap();
        }
        journal.commit_now_for_test();

        // Checkpoint everything (txn 1 then txn 2; the newest applies last).
        journal.flush_on_unmount().unwrap();

        // BOTH inodes carry their writes: txn 2's capture did not resurrect A's
        // pre-txn-1 bytes from the lagging device.
        let raw_a: RawInode = f
            .disk
            .segment()
            .read_val(f.ext4.inode_table_offset(ino_a).unwrap())
            .unwrap();
        let raw_b: RawInode = f
            .disk
            .segment()
            .read_val(f.ext4.inode_table_offset(ino_b).unwrap())
            .unwrap();
        assert_eq!(
            raw_a.link_count, new_link_a,
            "neighbor capture must not clobber inode A (B-1 stale seed)"
        );
        assert_eq!(raw_b.link_count, new_link_b, "inode B's own write applied");
    }

    /// T1 (superblock capture hygiene, B-1 class): the superblock capture must
    /// patch EVERY mutable field from memory — a capture that patches only "its
    /// own" field leaves the others at the seed value, so two captures patching
    /// disjoint fields across transactions clobber each other. The on-disk
    /// `last_orphan` here stands in for any stale seed byte: after a journaled
    /// op captures the superblock and checkpoint applies it, the field must hold
    /// the in-memory value, not the doctored device value.
    #[ktest]
    fn journaled_superblock_capture_patches_all_mutable_fields() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_journal_inode(16)
            .build()
            .unwrap();
        let journal = f.ext4.journal().unwrap();
        journal.stop_commit_thread();

        // Doctor the DEVICE superblock's `last_orphan` (the in-memory superblock
        // still holds 0) — a stand-in for any device byte lagging memory.
        let mut raw_sb: RawSuperBlock = f.disk.segment().read_val(SUPER_BLOCK_OFFSET).unwrap();
        raw_sb.last_orphan = 99;
        f.disk
            .segment()
            .write_val(SUPER_BLOCK_OFFSET, &raw_sb)
            .unwrap();

        // A journaled op that allocates a block captures the superblock counters.
        {
            let op = f.ext4.begin_op(4).unwrap();
            let range = f.ext4.alloc_blocks(1, 0, op.get()).unwrap();
            assert_eq!(range.end - range.start, 1);
        }
        journal.commit_now_for_test();
        journal.flush_on_unmount().unwrap();

        // The checkpointed superblock carries the IN-MEMORY state for every
        // mutable field: the counters (changed by the alloc) AND `last_orphan`
        // (unchanged in memory, so 0 — not the doctored 99).
        let after: RawSuperBlock = f.disk.segment().read_val(SUPER_BLOCK_OFFSET).unwrap();
        let sb = f.ext4.super_block.read();
        assert_eq!(
            u64::from(after.free_blocks_count),
            sb.free_blocks_count(),
            "free block count patched from memory"
        );
        assert_eq!(
            after.free_inodes_count,
            sb.free_inodes_count(),
            "free inode count patched from memory"
        );
        assert_eq!(
            after.last_orphan, 0,
            "last_orphan patched from memory, not left at the (doctored) seed"
        );
    }

    /// T2 (fsync WAL): on a journaled volume, `fsync` journals the inode
    /// writeback (no direct stale-RMW of the on-disk slot) and does not return
    /// until the transaction is committed to the log — and an `fsync` of a
    /// clean inode returns immediately instead of waiting on a transaction
    /// that will never become committable.
    #[ktest]
    fn fsync_on_journaled_volume_commits_before_returning() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_journal_inode(16)
            .build()
            .unwrap();
        let journal = f.ext4.journal().unwrap();
        // The commit thread stays RUNNING: `log_wait_commit` sleeps on it.

        let inode = f.ext4.read_inode(ROOT_INO).unwrap();
        inode.set_atime(Duration::from_secs(12345));

        let committed_before = journal.committed_tid();
        inode.sync_data_and_meta().unwrap();
        // `fsync` returned only after its transaction committed.
        let committed_after = journal.committed_tid();
        assert!(
            journal::tid_geq(committed_after, committed_before.wrapping_add(1)),
            "fsync must wait for its commit (before={committed_before}, after={committed_after})"
        );

        // A second fsync with nothing dirty must not hang (nothing to commit).
        inode.sync_data_and_meta().unwrap();
        assert_eq!(journal.committed_tid(), committed_after);
        drop(inode);
    }

    /// A non-journaled volume has no journal — the Phase 1–3 mount path is
    /// unchanged (the `load_geometry -> None` no-op branch).
    #[ktest]
    fn non_journaled_mount_has_no_journal() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        assert!(f.ext4.journal().is_none());
    }

    /// The mount-recovery path in miniature: a fixture whose on-disk journal holds
    /// a committed-but-un-checkpointed transaction (and whose ext4 superblock has
    /// the `RECOVER` bit set) is *recovered by `Ext4::open`* — the after-image
    /// reaches its final location and the on-disk `RECOVER` bit is cleared.
    #[ktest]
    fn dirty_journaled_mount_recovers_and_clears_recover_bit() {
        crate::time::clocks::init_for_ktest();

        // Build a fixture with a clean journal already loaded (this first mount
        // starts a commit thread we tear down by dropping `first` below).
        let first = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_journal_inode(16)
            .build()
            .unwrap();
        let disk = first.disk.clone();

        // Commit a single-block transaction into the on-disk log via the loaded
        // journal. This writes the after-image to the log and flips the on-disk
        // journal superblock to dirty (`s_start != 0`); it does NOT checkpoint, so
        // the destination block still holds the fixture's zeroed disk.
        let dest = 500u64;
        let mut after = [0u8; BLOCK_SIZE];
        after[..8].copy_from_slice(b"RECOVER!");
        let journal = first.ext4.journal().unwrap();
        journal::commit_single_block_for_test(journal.as_ref(), disk.as_ref(), dest, after)
            .unwrap();
        // The dirty journal is on disk; the final location is still zeroed.
        let mut before = [0u8; BLOCK_SIZE];
        disk.segment()
            .read_bytes(dest as usize * BLOCK_SIZE, &mut before)
            .unwrap();
        assert_eq!(before, [0u8; BLOCK_SIZE]);
        let raw_jsb: [u8; 24] = {
            let mut b = [0u8; 24];
            disk.segment()
                .read_bytes(JOURNAL_START_BLOCK as usize * BLOCK_SIZE, &mut b)
                .unwrap();
            b
        };
        // s_start is bytes [20..24] of the journal superblock (big-endian), nonzero.
        assert_ne!(
            u32::from_be_bytes([raw_jsb[20], raw_jsb[21], raw_jsb[22], raw_jsb[23]]),
            0
        );

        // Set the ext4 superblock's RECOVER incompat bit on disk so the next mount
        // treats the volume as needing recovery.
        let mut raw_sb = disk
            .segment()
            .read_val::<RawSuperBlock>(SUPER_BLOCK_OFFSET)
            .unwrap();
        raw_sb.feature_incompat |= RECOVER_BIT;
        disk.segment()
            .write_val(SUPER_BLOCK_OFFSET, &raw_sb)
            .unwrap();

        // Tear down the first mount (stops its commit thread) before re-mounting.
        drop(journal);
        drop(first);

        // Re-mount: `Ext4::open` must recover the dirty journal.
        let ext4 = Ext4::open(disk.clone() as Arc<dyn BlockDevice>).unwrap();

        // Recovery replayed the after-image to its final location.
        let mut recovered = [0u8; BLOCK_SIZE];
        disk.segment()
            .read_bytes(dest as usize * BLOCK_SIZE, &mut recovered)
            .unwrap();
        assert_eq!(recovered, after);

        // The on-disk RECOVER bit was cleared (persisted by `sync_metadata`).
        let raw_sb_after = disk
            .segment()
            .read_val::<RawSuperBlock>(SUPER_BLOCK_OFFSET)
            .unwrap();
        assert_eq!(raw_sb_after.feature_incompat & RECOVER_BIT, 0);

        // The journal is loaded on the recovered mount too.
        assert!(ext4.journal().is_some());
        drop(ext4);
    }
}
