// SPDX-License-Identifier: MPL-2.0

//! The `Ext4` filesystem object: mount, geometry, block allocation, and inode
//! lookup.
//!
//! The superblock and block-group descriptors are mutable behind dirty
//! tracking, each group caches its block and inode bitmaps, and `Ext4` routes
//! block and inode allocation/free across groups, threads the journal, and
//! writes the mutated metadata back. `Ext4` also owns the per-group inode cache,
//! the orphan chain, and inode-descriptor writeback (`write_back_inode_desc`).

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use device_id::DeviceId;

use super::{
    block_group::BlockGroup,
    checksum::FsCsumSeed,
    feature::FeatureIncompatSet,
    inode,
    inode::{FilePerm, Inode, InodeDesc, InodeSeed, RawInode},
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

/// How `EXT4_IOC_SHUTDOWN` takes the filesystem down — the parse-once form of
/// the ioctl's `EXT4_GOING_FLAGS_*` argument (Linux `ext4_ioctl_shutdown`).
#[derive(Debug)]
pub(super) enum GoingDown {
    /// `EXT4_GOING_FLAGS_DEFAULT` (0): flush everything, then freeze. The
    /// mildest form — nothing is lost, the device just stops accepting work.
    Default,
    /// `EXT4_GOING_FLAGS_LOGFLUSH` (1): commit the running transaction, then
    /// kill the journal. Committed operations survive the "crash".
    LogFlush,
    /// `EXT4_GOING_FLAGS_NOLOGFLUSH` (2): kill the journal immediately; the
    /// running transaction vanishes, like a power cut mid-operation.
    NoLogFlush,
}

impl TryFrom<u32> for GoingDown {
    type Error = Error;

    fn try_from(flags: u32) -> Result<Self> {
        match flags {
            0 => Ok(Self::Default),
            1 => Ok(Self::LogFlush),
            2 => Ok(Self::NoLogFlush),
            _ => Err(Error::with_message(
                Errno::EINVAL,
                "unknown EXT4_GOING_FLAGS value",
            )),
        }
    }
}

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
    /// Total inode count, cached at mount like `nr_inodes_per_group` — the
    /// corrupt-ino bound check sits on every inode load and must not touch
    /// the `super_block` lock (callers may hold it).
    total_inodes: u32,
    /// Monotonic source for the `i_generation` stamped onto each newly created
    /// inode. Seeded from the mount time, like ext2.
    next_generation: AtomicU32,
    /// Raised by `EXT4_IOC_SHUTDOWN` (Linux `EXT4_FLAGS_SHUTDOWN`): the
    /// filesystem is "dead" — new operations fail `EIO` at `begin_op`, the
    /// sync/writeback paths refuse to touch the device, and unmount performs
    /// no clean-shutdown writes, freezing the on-disk state the way a power
    /// cut would.
    shutdown: AtomicBool,
    /// The orphan list: the guarded [`OrphanChain`] is the **in-memory mirror
    /// of the on-disk chain** (the position invariant lives on the type), and
    /// the lock is jbd2's `s_orphan_lock`.
    ///
    /// The mirror is the authoritative chain at runtime: a non-head removal
    /// finds the predecessor here (never by walking on-disk `i_dtime` pointers,
    /// which lag committed-but-un-checkpointed writes), and the journaled inode
    /// writeback pulls an on-list inode's successor from here (a splice cannot
    /// reach a chained neighbor's cached `InodeDesc` — taking that inode's
    /// `inner` under this lock would invert the lock order). Linux keeps the
    /// same structure as `sbi->s_orphan` + `s_orphan_lock`.
    ///
    /// Lock order: taken *after* the journal handle (②) and *before* the
    /// superblock (⑤), never while acquiring an inode `inner` (report §5.1).
    /// Empty on a non-journaled volume (the orphan machinery is journal-only,
    /// matching Linux).
    s_orphan_lock: Mutex<OrphanChain>,
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
        let total_inodes = super_block.total_inodes();

        let block_groups = Self::load_block_groups(device.clone(), &super_block)?;

        let ext4 = Arc::new_cyclic(|weak| Ext4 {
            block_device: device,
            super_block: RwMutex::new(Dirty::new(super_block)),
            block_groups,
            nr_inodes_per_group,
            total_inodes,
            next_generation: AtomicU32::new(utils::now().as_secs() as u32),
            shutdown: AtomicBool::new(false),
            s_orphan_lock: Mutex::new(OrphanChain::new()),
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
                // Replay rewrote the very blocks the in-memory superblock and
                // group metadata were parsed from (they were loaded above,
                // pre-replay): reload them, or the mount would run on stale
                // state — the orphan scan below would read a stale head, the
                // allocators would re-hand-out replayed blocks, and the next
                // sync would write stale counters back over the replayed values.
                ext4.reload_metadata_after_replay()?;
            }

            // D-4 journal feature upgrade: a metadata_csum filesystem over a
            // fresh featureless journal flips it to csum_v3 (+64bit iff the
            // fs is 64bit). Deliberately narrower than Linux's every-mount
            // clear-and-reset (`set_journal_csum_feature_set`) — a journal
            // already carrying features is honored verbatim; see
            // `journal::upgrade_journal_on_mount`. Placed exactly here —
            // after recovery left the log clean and empty (the flip is only
            // crash-safe then; see `journal::upgrade_journal_on_mount`) and
            // before the journal is published for new transactions. A
            // rewrite invalidates the parsed geometry (tag layout / csum
            // seed were derived from the pre-upgrade bytes), so the journal
            // is rebuilt from a reload.
            let journal = {
                let needs = {
                    let sb = ext4.super_block();
                    journal::JournalUpgradeNeeds {
                        fs_has_metadata_csum: sb.has_metadata_csum(),
                        fs_is_64bit: sb.feature_incompat().contains(FeatureIncompatSet::IS_64BIT),
                    }
                };
                if journal::upgrade_journal_on_mount(
                    journal.geometry(),
                    ext4.block_device().as_ref(),
                    needs,
                )?
                .upgraded
                {
                    let geometry = journal::load_geometry(&ext4)?.ok_or_else(|| {
                        Error::with_message(
                            Errno::EIO,
                            "the journal disappeared across the feature upgrade",
                        )
                    })?;
                    journal::Journal::new(geometry, ext4.block_device().clone())
                } else {
                    journal
                }
            };

            // Stamp `INCOMPAT_RECOVER` for the lifetime of this writable mount
            // (Linux sets it in `ext4_load_journal`, clears it at clean unmount
            // — see `Ext4::drop`): if THIS session crashes, the bit forces the
            // next mount to replay the dirty log. Without it, a crash while the
            // device `s_last_orphan` is 0 would skip recovery and the next
            // session's first commit would overwrite committed transactions —
            // silently discarding fsync-acknowledged metadata. Persisted and
            // barriered before the commit thread can dirty the log.
            ext4.super_block.write().set_recover();
            ext4.sync_metadata()?;
            if ext4.block_device.sync()? != BioStatus::Complete {
                return_errno_with_message!(Errno::EIO, "failed to flush the RECOVER flag");
            }

            // Start `kjournald` and publish the journal. Storing it only *after*
            // the thread is running means every error path above drops the `Ext4`
            // `Arc` while `journal` is still `None`, so `Ext4::drop` finds nothing
            // to stop — no half-started thread is ever leaked.
            journal.start_commit_thread();
            *ext4.journal.write() = Some(journal);

            // Finish any deletion a crash interrupted after the link count hit 0
            // but before the inode/blocks were freed (Linux
            // `ext4_orphan_cleanup`). Ordered strictly AFTER journal recovery +
            // metadata reload (the orphan head and chain pointers must be the
            // replayed values) and after the journal is published, so every
            // deletion runs as a normal journaled transaction — a crash *during*
            // the scan is itself recoverable (replay, then rescan the shorter
            // chain). Never fails the mount: scan problems degrade to warnings
            // (`e2fsck -p` territory), matching Linux.
            ext4.recover_orphan_list();
        }

        Ok(ext4)
    }

    /// Reloads the in-memory superblock and every block group's descriptor +
    /// bitmaps from the device — called once, right after journal replay has
    /// rewritten their final locations (see the call site in [`Ext4::open`]).
    fn reload_metadata_after_replay(&self) -> Result<()> {
        let raw_super_block = self
            .block_device
            .read_val::<RawSuperBlock>(SUPER_BLOCK_OFFSET)
            .map_err(|_| {
                Error::with_message(Errno::EIO, "failed to re-read superblock after replay")
            })?;
        let super_block = SuperBlock::try_from(raw_super_block)?;
        for group in &self.block_groups {
            group.reload_metadata(self.block_device.as_ref())?;
        }
        *self.super_block.write() = Dirty::new(super_block);
        Ok(())
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
        // The single gate every metadata operation passes (Linux
        // `ext4_journal_check_start`): a shut-down filesystem accepts no new
        // work, journaled or not.
        self.ensure_not_shutdown()?;
        match self.journal() {
            Some(journal) => journal::OpHandle::start(&journal, credits),
            None => Ok(journal::OpHandle::none()),
        }
    }

    /// Returns whether `EXT4_IOC_SHUTDOWN` has killed this filesystem.
    pub(super) fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    /// Errors `EIO` once the filesystem has been shut down (Linux
    /// `ext4_forced_shutdown` checks).
    pub(super) fn ensure_not_shutdown(&self) -> Result<()> {
        if self.is_shutdown() {
            return_errno_with_message!(Errno::EIO, "filesystem is shut down");
        }
        Ok(())
    }

    /// Shuts the filesystem down (`EXT4_IOC_SHUTDOWN`, Linux
    /// `ext4_force_shutdown`): after this, new operations fail `EIO` and the
    /// on-disk state is frozen as-is — the controlled "crash right here" the
    /// crash tests are built on. Idempotent: a second call is a no-op.
    pub(super) fn shutdown(&self, going: GoingDown) -> Result<()> {
        if self.is_shutdown() {
            return Ok(());
        }
        match going {
            GoingDown::Default => {
                // Flush everything (data, metadata, journal commit), then
                // raise the flag. Linux freezes the fs around the flag so no
                // write can slip in between; we have no freeze — the unfrozen
                // window is a recorded deviation, fine for a test hook.
                <Self as crate::fs::vfs::file_system::FileSystem>::sync(self)?;
                self.shutdown.store(true, Ordering::Release);
            }
            GoingDown::LogFlush => {
                self.shutdown.store(true, Ordering::Release);
                if let Some(journal) = self.journal() {
                    // Commit what is running, then kill the journal: committed
                    // operations survive the "crash", in-flight ones vanish.
                    journal.commit_and_wait_running()?;
                    journal.abort_for_shutdown();
                }
            }
            GoingDown::NoLogFlush => {
                self.shutdown.store(true, Ordering::Release);
                if let Some(journal) = self.journal() {
                    journal.abort_for_shutdown();
                }
            }
        }
        Ok(())
    }

    /// Returns the maximum byte size of a regular file.
    ///
    /// An extent maps a 32-bit logical block index, so a file spans at most
    /// `2^32 - 1` blocks; the result is also clamped to `i64::MAX` (the VFS
    /// size limit). This is the bound `write_at` rejects against.
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
                sb.journal_capture(handle, sb.last_orphan())?;
                return Ok(range);
            }
        }

        return_errno_with_message!(Errno::ENOSPC, "no free blocks available in any group");
    }

    /// Frees the physical block run `auth` covers, splitting across groups —
    /// the sole consumption funnel of the forget-before-free protocol.
    ///
    /// Consuming a [`journal::BlockFreeAuth`] instead of a bare
    /// `(start, count)` pair means a free that skips the revoke decision does
    /// not compile (rust_rules ⑤): the credential is minted either by
    /// [`journal::forget`] — journaled metadata and revoke-covered data
    /// (extent-tree nodes, directory blocks, slow-symlink targets), carrying
    /// the revoke duty — or duty-free by
    /// [`journal::BlockFreeAuth::without_revoke_duty`].
    ///
    /// The mint is pure; the forget **effects** (capture cancel, revoke
    /// record, retained-image eviction) run HERE, per contiguous per-group
    /// run, immediately before that run's bitmap clear, under the caller's
    /// handle and this funnel's lock scope — record-then-free, Linux's order
    /// within `ext4_free_blocks` (`ext4_forget` before the bitmap clear,
    /// fs/ext4/mballoc.c:6676/6719). An operation that errors *before* a
    /// run's effects (an earlier extent's free failing, a mint whose free is
    /// never reached) therefore leaves the journal state of the unfreed
    /// blocks untouched: no revoke stands for a still-referenced block, no
    /// capture is lost (`RevokeDuty::discharge` in the journal's revoke
    /// module documents the three effects).
    pub(super) fn free_blocks(
        &self,
        auth: journal::BlockFreeAuth,
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        let (start, count, duty) = auth.into_parts();
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
            // Discharge this run's revoke duty right before its bitmap clear
            // (record-then-free; see the function docs). Without a handle
            // there is no log whose replay could resurrect the blocks, so
            // the duty is inert — the pre-consumption semantics of a
            // non-journaled volume.
            if let (Some(handle), Some(duty)) = (handle, duty.as_ref()) {
                duty.discharge(handle, current_block, blocks_in_group)?;
            }
            let freed_count =
                group.free_blocks(group_start_bit..(group_start_bit + blocks_in_group), handle)?;
            if freed_count > 0 {
                sb.inc_free_blocks(freed_count as u64)?;
                sb.journal_capture(handle, sb.last_orphan())?;
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

            // Compute in u64: near-2^32 `inodes_count` is valid, so a last-group
            // `group * per_group + local` can exceed u32 — a wrapped number would
            // alias an early-group inode while still passing the range check.
            let ino64 = group_idx as u64 * nr_inodes_per_group as u64 + local_idx as u64 + 1;
            let ino = match u32::try_from(ino64) {
                Ok(ino) if ino >= sb.first_ino() && ino <= total_inodes => ino,
                _ => {
                    // Roll back the group-level allocation before erroring out.
                    let _ = group.free_inode(local_idx, type_, handle);
                    return_errno_with_message!(
                        Errno::EIO,
                        "allocated inode number out of valid range"
                    );
                }
            };
            sb.dec_free_inodes()?;
            sb.journal_capture(handle, sb.last_orphan())?;

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
            sb.journal_capture(handle, sb.last_orphan())?;
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
    pub(super) fn create_inode(
        &self,
        parent_ino: Ext4Ino,
        type_: InodeType,
        perm: FilePerm,
        seed: InodeSeed,
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
        let inode_desc = InodeDesc::new(type_, perm, uid, gid, link_count, generation, now, seed);

        if let Err(err) = self.write_new_inode_desc(ino, &inode_desc, handle) {
            // Roll back the inode allocation: clear the bitmap bit and restore
            // the superblock free-inode counter.
            if let Err(free_err) = self.free_inode(ino, type_, handle) {
                error!("create_inode: rollback free_inode failed: {:?}", free_err);
            }
            return Err(err);
        }

        let block_group_idx = ((ino - 1) / self.nr_inodes_per_group) as usize;
        let inode = match Inode::new(
            ino,
            inode_desc.type_(),
            Dirty::new(inode_desc),
            block_group_idx,
            self.self_ref.clone(),
        ) {
            Ok(inode) => inode,
            // Unreachable for a fresh descriptor (its root is
            // `ExtentTree::empty()`, which always parses), but roll the
            // allocation back symmetrically with the writeback failure above.
            Err(err) => {
                if let Err(free_err) = self.free_inode(ino, type_, handle) {
                    error!("create_inode: rollback free_inode failed: {:?}", free_err);
                }
                return Err(err);
            }
        };
        // The fresh descriptor was captured under the creating op's handle
        // (before this `Inode` existed): record the transaction so an fsync of
        // the just-created inode waits for its commit (see
        // `InodeInner::sync_tid` — the inode starts "clean", and clean does
        // not imply committed).
        inode.record_sync_tid(handle);
        Ok(inode)
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

    /// Links `ino` onto the head of the orphan list, returning the previous
    /// head (the successor `ino` now points at; `0` if the list was empty).
    ///
    /// The caller — a namespace op that has just dropped an inode's link count
    /// to 0 (Linux `ext4_orphan_add`) — must consume the returned
    /// [`OrphanLink`] through [`InodeInner::persist_as_orphan`], which records
    /// the previous head in the child's `i_dtime` and writes the child back
    /// **in the same transaction**: a crash after this transaction commits
    /// then leaves a well-formed chain the recovery scan can walk. The
    /// credential is `#[must_use]` and its field is private, so the add /
    /// record / write-back triple cannot be half-done at a call site. A no-op
    /// (empty link) without a handle: the orphan machinery is journal-only
    /// (Linux parity — without a journal there is no recovery pass to consume
    /// the list, and a stale `s_last_orphan` would just accrete).
    ///
    /// # Locking
    ///
    /// Acquires [`s_orphan_lock`](Self::s_orphan_lock), a leaf taken **after**
    /// the journal handle (②) and **before** the superblock (⑤); it never takes
    /// an inode `inner`, so it cannot invert against the caller's held child
    /// `inner` (①).
    pub(super) fn orphan_add(
        &self,
        ino: Ext4Ino,
        handle: Option<&journal::Handle>,
    ) -> Result<OrphanLink> {
        if handle.is_none() {
            return Ok(OrphanLink { old_head: None });
        }
        let mut chain = self.s_orphan_lock.lock();
        // Already listed (defensive — no current call site can re-add a listed
        // inode; Linux guards the same way): keep the existing successor.
        if let Some(next) = chain.successor_of(ino) {
            warn!("inode {ino} is already on the orphan list");
            return Ok(OrphanLink { old_head: next });
        }
        // One superblock WRITE guard across both the capture and the mutation:
        // the counters are guarded by the superblock lock (a concurrent
        // alloc/free holds it across its own capture), so snapshotting them
        // outside it could patch stale values over a newer capture. Capture
        // first (fallible), then mutate (infallible), so an error leaves head,
        // mirror, and transaction mutually consistent.
        let mut sb = self.super_block.write();
        let old_head = sb.last_orphan();
        sb.journal_capture(handle, Some(ino))?;
        sb.set_last_orphan(Some(ino));
        drop(sb);
        chain.push_head(ino);
        Ok(OrphanLink { old_head })
    }

    /// Unlinks `ino` from the orphan list, the mirror of
    /// [`orphan_add`](Self::orphan_add) (Linux `ext4_orphan_del`).
    ///
    /// The predecessor and successor come from the in-memory chain — never from
    /// walking on-disk `i_dtime` pointers, which lag every
    /// committed-but-un-checkpointed writeback. If `ino` is the head, the
    /// superblock head advances to the successor; otherwise the predecessor's
    /// on-disk `i_dtime` is spliced to the successor with a journaled sub-slot
    /// patch ([`patch_orphan_next_on_disk`](Self::patch_orphan_next_on_disk)) —
    /// the predecessor's *cached* descriptor is deliberately not touched
    /// (taking its `inner` here would invert the lock order); the journaled
    /// writeback compensates by pulling an on-list inode's successor from the
    /// chain (see [`write_back_inode_desc`](Self::write_back_inode_desc)).
    ///
    /// A no-op for an inode that is not on the list (a non-journaled volume, or
    /// an inode that was never added). Locking as in [`orphan_add`](Self::orphan_add).
    pub(super) fn orphan_del(&self, ino: Ext4Ino, handle: Option<&journal::Handle>) -> Result<()> {
        let mut chain = self.s_orphan_lock.lock();
        let Some(splice) = chain.splice_for(ino) else {
            return Ok(());
        };
        match splice {
            OrphanSplice::Head { successor } => {
                // One superblock write guard across capture + mutation, capture
                // first (see `orphan_add`).
                let mut sb = self.super_block.write();
                sb.journal_capture(handle, successor)?;
                sb.set_last_orphan(successor);
            }
            OrphanSplice::Middle {
                predecessor,
                successor,
            } => {
                self.patch_orphan_next_on_disk(predecessor, successor, handle)?;
            }
        }
        chain.commit_remove(ino);
        Ok(())
    }

    /// Splices a chained predecessor's on-disk `i_dtime` (its orphan-next
    /// pointer) to `next` with a journaled 4-byte patch of its inode-table
    /// slot.
    ///
    /// The patch is sound without decoding or locking the predecessor: the
    /// capture's seed is the block's newest committed image (see
    /// `UncheckpointedImage` in `journal/transaction.rs`), and only the 4
    /// `i_dtime` bytes are overwritten, so every other field — and every
    /// neighboring inode — keeps its committed content.
    fn patch_orphan_next_on_disk(
        &self,
        ino: Ext4Ino,
        next: Option<Ext4Ino>,
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        // On a metadata_csum volume the 4-byte i_dtime splice invalidates the
        // predecessor's inode checksum, so hand the slot what it needs to
        // re-stamp the whole inode.
        let csum = {
            let sb = self.super_block.read();
            sb.has_metadata_csum()
                .then(|| (ino, sb.metadata_csum_seed(), sb.inode_size()))
        };
        // `0 = end of chain` is the on-disk convention (encode boundary).
        self.inode_slot(ino)?
            .journal_patch_dtime(handle, next.unwrap_or(0), csum)
    }

    /// Finishes deletions interrupted by a crash, by walking the on-disk orphan
    /// list at mount time (Linux `ext4_orphan_cleanup`).
    ///
    /// Called from [`Ext4::open`] **after** journal recovery has replayed the
    /// log (the head and every `i_dtime` pointer are the replayed values — with
    /// no transaction in flight, the device is authoritative) and after the
    /// journal is published, so every deletion runs as an ordinary journaled
    /// reclaim transaction: a crash mid-scan replays and rescans the shorter
    /// chain, never observing a half-freed inode.
    ///
    /// The walk distrusts the disk (Linux `ext4_orphan_get`): an out-of-range
    /// head/pointer, a cycle, an unallocated inode (its deletion completed; its
    /// `i_dtime` is a deletion time, not a pointer), or an undecodable inode
    /// ends the walk, and whatever remains unprocessed is cleared with a
    /// warning — dropping garbage loses at most already-freed-or-leaked blocks,
    /// which `e2fsck -p` reclaims, and never frees a live inode. A chain member
    /// with a nonzero link count (a Linux crash-mid-truncate orphan — we never
    /// produce one, the truncate seam is inert) is skipped, never freed:
    /// freeing it would destroy a live file; the re-truncate recovery mode is a
    /// P7 follow-up.
    ///
    /// Never fails the mount: every problem degrades to a warning and, at
    /// worst, a cleared head (Linux logs and continues the same way).
    fn recover_orphan_list(self: &Arc<Self>) {
        if self.super_block.read().last_orphan().is_none() {
            return;
        }

        let (chain, to_free, mut suspect) = self.walk_orphan_chain();

        // Prime the in-memory mirror, then finish each interrupted deletion in
        // chain order. Every reclaim opens its own journaled transaction and its
        // `orphan_del` advances/splices the on-disk chain, so the state after
        // every step is a well-formed shorter chain.
        self.s_orphan_lock.lock().replace(chain);
        for &ino in &to_free {
            suspect |= !self.reclaim_scanned_orphan(ino);
        }

        // A truncated walk, a skipped (live or undecodable) member, or a failed
        // reclaim can leave the on-disk head referring to inodes the scan did
        // not free: clear it, journaled, so the next mount does not rewalk
        // garbage. Best-effort: on failure the head stays and the next mount
        // retries the scan.
        let head_after = self.super_block.read().last_orphan();
        if suspect || head_after.is_some() {
            if let Some(head) = head_after
                && !suspect
            {
                warn!("orphan cleanup left an unexpected nonzero head {head}");
            }
            self.clear_orphan_head();
        }
    }

    /// Walks the on-disk orphan chain into a defensive snapshot (the walk half
    /// of [`recover_orphan_list`](Self::recover_orphan_list)): returns the
    /// full mirrored chain, the subset the scan may delete, and whether
    /// anything looked suspect (truncated walk / live member).
    fn walk_orphan_chain(&self) -> (Vec<Ext4Ino>, Vec<Ext4Ino>, bool) {
        let (first_ino, total_inodes) = {
            let sb = self.super_block.read();
            (sb.first_ino(), sb.total_inodes())
        };
        let mut chain = Vec::new();
        let mut to_free = Vec::new();
        let mut cursor = self.super_block.read().last_orphan();
        let mut suspect = false;
        while let Some(cur) = cursor {
            // Reserved inodes (including the root) can never be orphans; a
            // revisit is a cycle; an unallocated inode's deletion completed and
            // its `i_dtime` is a deletion time, not a trustworthy pointer.
            let in_range = cur >= first_ino && cur <= total_inodes;
            if !in_range || chain.contains(&cur) || !self.is_inode_allocated(cur) {
                suspect = true;
                break;
            }
            let raw = match self.read_raw_inode(cur) {
                Ok(raw) => raw,
                Err(e) => {
                    warn!("orphan walk could not read inode {cur}: {e:?}");
                    suspect = true;
                    break;
                }
            };
            chain.push(cur);
            if raw.link_count == 0 {
                to_free.push(cur);
            } else {
                warn!(
                    "orphan inode {cur} has link count {}; skipping (crash-mid-truncate \
                     recovery is not supported yet)",
                    raw.link_count
                );
                suspect = true;
            }
            // On disk `0` terminates the chain (decode boundary).
            cursor = (raw.dtime != 0).then_some(raw.dtime);
        }
        (chain, to_free, suspect)
    }

    /// Finishes one scanned orphan's interrupted deletion (truncate + free +
    /// `orphan_del`, one journaled transaction). Returns `false` when anything
    /// degraded to a warning — the caller then clears the on-disk head.
    fn reclaim_scanned_orphan(self: &Arc<Self>, ino: Ext4Ino) -> bool {
        let raw = match self.read_raw_inode(ino) {
            Ok(raw) => raw,
            Err(e) => {
                warn!("orphan cleanup could not re-read inode {ino}: {e:?}");
                return false;
            }
        };
        let block_group_idx = ((ino - 1) / self.nr_inodes_per_group) as usize;
        match Inode::from_raw_for_recovery(ino, &raw, block_group_idx, self.self_ref.clone()) {
            // (`Drop` would run the same reclaim, but calling it directly
            // surfaces an error instead of logging it from a destructor.)
            Ok(orphan) => match orphan.try_reclaim_deleted_inode() {
                Ok(_) => true,
                Err(e) => {
                    warn!("orphan cleanup could not reclaim inode {ino}: {e:?}");
                    false
                }
            },
            Err(e) => {
                warn!("skipping undecodable orphan inode {ino}: {e:?}");
                if let Err(e) = self.orphan_del(ino, None) {
                    warn!("could not unlist orphan inode {ino}: {e:?}");
                }
                false
            }
        }
    }

    /// Clears the on-disk orphan head and empties the mirror, journaled (the
    /// close half of [`recover_orphan_list`](Self::recover_orphan_list)).
    /// Lock order: handle (②, `begin_op`) → `s_orphan_lock` → superblock (⑤),
    /// as everywhere. Best-effort: every failure degrades to a warning.
    fn clear_orphan_head(&self) {
        let op = match self.begin_op(Self::FSYNC_CREDITS) {
            Ok(op) => op,
            Err(e) => {
                warn!("could not clear the orphan head: {e:?}");
                return;
            }
        };
        let mut chain = self.s_orphan_lock.lock();
        // One superblock write guard across capture + mutation (see
        // `orphan_add`).
        let mut sb = self.super_block.write();
        if let Err(e) = sb.journal_capture(op.get(), None) {
            warn!("could not journal the cleared orphan head: {e:?}");
            return;
        }
        sb.set_last_orphan(None);
        drop(sb);
        chain.clear();
    }

    /// Reads an inode's raw on-disk bytes, bypassing the link-count-0 gate that
    /// [`read_inode_desc`](Self::read_inode_desc) enforces. Used by the
    /// mount-time orphan scan, whose inodes legitimately carry link count 0
    /// (and whose device bytes are authoritative — recovery has replayed the
    /// log and nothing is in flight).
    fn read_raw_inode(&self, ino: Ext4Ino) -> Result<RawInode> {
        self.inode_slot(ino)?.read_raw(self.block_device.as_ref())
    }

    /// Writes back the superblock and every dirty group descriptor/bitmap —
    /// on a **non-journaled** volume, or at the unmount quiesce point. While a
    /// journal is live this is a deliberate no-op (see the body).
    pub(super) fn sync_metadata(&self) -> Result<()> {
        // A shut-down filesystem writes nothing more (Linux: writeback paths
        // bail on `ext4_forced_shutdown`); fsync of a dirty inode reports the
        // death as `EIO` through this same gate.
        self.ensure_not_shutdown()?;
        // While the journal is live, these direct RMWs are pure WAL hazard:
        // every bitmap/GDT/superblock mutation is already captured at op time
        // (the write-access credentials + `journal_capture`) and reaches its
        // final location via checkpoint, whereas a direct write here can land
        // *ahead* of a still-uncommitted transaction — the crash harness
        // reconstructed exactly that (bitmap/counts ahead of the journal at a
        // sync-then-crash boundary). Durability for sync(2) comes from
        // waiting on the commit instead (`commit_and_wait_running`). Only the
        // unmount path, after `stop_commit_thread` + `flush_on_unmount`
        // emptied the log, may write directly again (nothing uncommitted
        // remains — that is where the cleared `RECOVER` flag goes out).
        if let Some(journal) = self.journal()
            && !journal.is_stopped()
        {
            return Ok(());
        }
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
            // Emit the free-block count's low half (and, under `64BIT`, its high
            // half) through the single write-side splice, mirroring the read.
            sb.write_free_blocks_count(&mut raw);
            raw.free_inodes_count = sb.free_inodes_count();
            raw.feature_incompat = sb.feature_incompat().bits();
            // Stamp the superblock checksum over the final image (a no-op field
            // when the feature is off).
            if sb.has_metadata_csum() {
                raw.checksum = SuperBlock::superblock_checksum(&raw);
            }
            // `last_orphan` is deliberately NOT patched from memory: the
            // in-memory head advances inside a still-running transaction
            // (`orphan_add`), so a direct write here would publish an
            // uncommitted head ahead of the log — a crash would then hand the
            // next mount's orphan scan a head whose unlink transaction never
            // committed, and the scan would free a LIVE inode. The device value
            // (last checkpointed, preserved by this RMW) is the consistent one;
            // the journaled superblock capture is the head's only writer.
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
        self.ensure_not_shutdown()?;
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
        // A corrupt dirent can carry any 32-bit ino; bound it by the
        // superblock's inode count, not just the group range (P1 review item,
        // batch-fixed at P5). Uses the mount-time cache — callers may already
        // hold the `super_block` lock.
        if ino > self.total_inodes {
            return_errno_with_message!(Errno::ENOENT, "inode number beyond s_inodes_count");
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

    /// Locates the on-disk `RawInode` slot for `ino`.
    fn inode_slot(&self, ino: Ext4Ino) -> Result<InodeSlot> {
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
        let byte =
            group.inode_table_bid() as usize * sb.block_size() + idx_in_group * sb.inode_size();
        Ok(InodeSlot {
            bid: (byte / BLOCK_SIZE) as Ext4Bid,
            offset_in_block: byte % BLOCK_SIZE,
        })
    }

    /// Returns the device byte offset of `ino`'s `RawInode` slot; tests peek
    /// and doctor raw slots through it.
    #[cfg(ktest)]
    pub(super) fn inode_table_offset(&self, ino: Ext4Ino) -> Result<usize> {
        Ok(self.inode_slot(ino)?.device_offset())
    }

    /// Read-modify-writes the on-disk `RawInode` for `ino`, patching only the
    /// fields that buffered writes can mutate (size, `i_blocks`, the extent
    /// root, timestamps, flags, and link count) and preserving everything else
    /// (`extra_isize`, checksums, generation, xattr tail, osd fields) losslessly.
    pub(super) fn write_back_inode_desc(
        &self,
        ino: Ext4Ino,
        desc: &InodeDesc,
        root: &[u32; inode::RAW_BLOCK_PTRS_LEN],
        handle: Option<&journal::Handle>,
    ) -> Result<()> {
        let slot = self.inode_slot(ino)?;

        // On a metadata_csum volume, stamp i_checksum_lo/hi over the final raw
        // inode at both writeback funnels (Linux `ext4_inode_csum_set`). `None`
        // when the feature is off keeps the raw inode byte-for-byte unchanged.
        let inode_csum = {
            let sb = self.super_block.read();
            sb.has_metadata_csum()
                .then(|| (sb.metadata_csum_seed(), sb.inode_size()))
        };

        // Journaled path: the on-disk inode may be **stale** — a prior write to it
        // was suppressed (WAL) and has not yet been checkpointed — so a
        // read-modify-write from the device would resurrect that block's zeroed
        // `i_mode` type bits / `extra_isize` / `generation` (exactly the
        // corruption the guest e2fsck caught after a rename touched a
        // not-yet-checkpointed directory). Encode the whole inode from the
        // in-memory descriptor instead and capture it; checkpoint applies it to
        // the final location after the transaction commits.
        if handle.is_some() {
            let mut raw = desc.to_raw_inode(root);
            // An on-orphan-list inode carries its chain successor in `i_dtime`,
            // and the authoritative successor is the in-memory chain — a
            // non-head splice repoints the on-disk chain without reaching this
            // (possibly stale) cached descriptor. Serializing the descriptor's
            // value here could resurrect a spliced-out pointer.
            //
            // `s_orphan_lock` is held across BOTH the lookup and the capture: a
            // concurrent `orphan_del` splice runs entirely under the lock, so
            // without this span it could land between the two and be
            // overwritten by this whole-slot patch carrying the pre-splice
            // successor. Lock order: inner ① (held by the caller) → handle ②
            // → `s_orphan_lock` → journal state (leaf), the same nesting as
            // `patch_orphan_next_on_disk`.
            let chain = self.s_orphan_lock.lock();
            if let Some(next) = chain.successor_of(ino) {
                // `0 = end of chain` is the on-disk convention (encode boundary).
                raw.dtime = next.unwrap_or(0);
            }
            if let Some((fs_seed, inode_size)) = inode_csum {
                let iseed = fs_seed.derive_inode(ino, raw.generation);
                InodeDesc::stamp_inode_checksum(&mut raw, iseed, inode_size);
            }
            return slot.journal_write(handle, &raw);
        }

        // Non-journaled (or the sync path): read-modify-write the on-disk inode,
        // patching only the fields buffered writes mutate (size, `i_blocks`, the
        // extent root, timestamps, flags, link count, dtime) and preserving
        // everything else (`extra_isize`, checksums, generation, xattr tail, osd
        // fields) losslessly, then write it through. The device is authoritative
        // here, so the RMW is safe.
        let mut raw = self
            .block_device
            .read_val::<RawInode>(slot.device_offset())
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
        let (mtime_secs, mtime_extra) = inode::encode_time(desc.mtime());
        raw.mtime = mtime_secs;
        raw.mtime_extra = mtime_extra;
        let (ctime_secs, ctime_extra) = inode::encode_time(desc.ctime());
        raw.ctime = ctime_secs;
        raw.ctime_extra = ctime_extra;

        // Mode (keep the on-disk type bits, update only the permission bits),
        // owners, and access time — so chmod/chown/chgrp/utimes persist too.
        raw.mode = (raw.mode & 0xF000) | (desc.perm().bits() & 0o7777);
        raw.uid = desc.uid() as u16;
        raw.uid_high = (desc.uid() >> 16) as u16;
        raw.gid = desc.gid() as u16;
        raw.gid_high = (desc.gid() >> 16) as u16;
        let (atime_secs, atime_extra) = inode::encode_time(desc.atime());
        raw.atime = atime_secs;
        raw.atime_extra = atime_extra;

        // The inline extent-tree root, flags, and link count.
        raw.block = *root;
        raw.flags = desc.flags().bits();
        raw.link_count = desc.link_count();

        // Deletion time (`i_dtime`, whole seconds). Zero for live inodes; the
        // reclaim path stamps it before the final writeback so a freed inode
        // carries a non-zero `i_dtime`, matching ext4 on-disk semantics.
        raw.dtime = desc.raw_dtime();

        if let Some((fs_seed, inode_size)) = inode_csum {
            let iseed = fs_seed.derive_inode(ino, raw.generation);
            InodeDesc::stamp_inode_checksum(&mut raw, iseed, inode_size);
        }

        self.block_device
            .write_val(slot.device_offset(), &raw)
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
        let slot = self.inode_slot(ino)?;

        let mut raw = desc.to_raw_inode(desc.raw_block());
        {
            let sb = self.super_block.read();
            if sb.has_metadata_csum() {
                let iseed = sb.metadata_csum_seed().derive_inode(ino, raw.generation);
                InodeDesc::stamp_inode_checksum(&mut raw, iseed, sb.inode_size());
            }
        }

        slot.journal_write(handle, &raw)?;
        // See `write_back_inode_desc`: suppress the direct write under a handle so
        // the inode reaches its final location only via checkpoint (WAL).
        if handle.is_none() {
            self.block_device
                .write_val(slot.device_offset(), &raw)
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
        // A shut-down filesystem unmounts without touching the device: no
        // journal flush, no RECOVER clear — the disk stays exactly as the
        // "crash" left it, and the next mount replays (Linux `ext4_put_super`
        // parity under forced shutdown).
        if self.is_shutdown() {
            if let Some(journal) = self.journal.get_mut().take() {
                journal.stop_commit_thread();
            }
            return;
        }
        // `get_mut` on the `RwMutex` is lock-free here — `&mut self` proves we are
        // the sole owner, so there is no contention to guard against.
        if let Some(journal) = self.journal.get_mut().take() {
            journal.stop_commit_thread();
            // With the commit thread stopped we are the sole committer: flush the
            // final running transaction and checkpoint so the on-disk journal is
            // left clean (`s_start == 0`) for the next mount. A failure here cannot
            // be propagated out of `drop`; log it (the un-checkpointed log stays
            // replay-safe — a later mount would recover it: `RECOVER` stays set).
            match journal.flush_on_unmount() {
                Ok(()) => {
                    // Clean unmount: the log is empty, so drop the session's
                    // `RECOVER` stamp (Linux parity) — the next mount skips
                    // replay. Best-effort: on any error the bit stays set and
                    // the next mount replays a clean journal (a no-op).
                    self.super_block.get_mut().clear_recover();
                    if let Err(e) = self.sync_metadata() {
                        error!(
                            "unmount could not persist the cleared RECOVER flag: {:?}",
                            e
                        );
                    } else if !matches!(self.block_device.sync(), Ok(BioStatus::Complete)) {
                        error!("unmount barrier failed");
                    }
                }
                Err(e) => error!("journal unmount flush failed: {:?}", e),
            }
        }
    }
}

/// The in-memory mirror of the on-disk orphan chain (jbd2 `sbi->s_orphan`).
///
/// Position IS the on-disk topology — entry 0 mirrors the superblock's
/// `s_last_orphan` head, and entry `i`'s on-disk successor (its `i_dtime`
/// pointer) is entry `i + 1`, with `0` marking the end — and this type owns
/// that invariant: callers ask for successors and splices instead of
/// re-deriving the `idx ± 1` arithmetic at every touch point (one wrong index
/// would silently corrupt the on-disk chain a crash recovery walks).
///
/// Removal is two-phase, mirroring its callers' capture-fallible-then-
/// mutate-infallible discipline: [`splice_for`](Self::splice_for) reports what
/// a removal must persist (journaled, fallible) without mutating, and only
/// after that succeeds does [`commit_remove`](Self::commit_remove) update the
/// mirror — an error leaves mirror, head, and transaction mutually consistent.
struct OrphanChain(Vec<Ext4Ino>);

/// Proof that an inode was linked onto the orphan chain, carrying the
/// previous head its `i_dtime` must record.
///
/// Minted only by [`Ext4::orphan_add`] and consumed only by
/// [`InodeInner::persist_as_orphan`](super::inode::Inode) — which records the
/// head into `i_dtime` and writes the inode back. Those two steps MUST land in
/// the same transaction as the add (a committed superblock head pointing at an
/// inode whose `i_dtime` is garbage breaks the recovery walk), so the
/// credential is `#[must_use]` and its field is private: the triple cannot be
/// half-done at a call site.
#[must_use = "the link must be persisted into the inode via persist_as_orphan"]
pub(super) struct OrphanLink {
    old_head: Option<Ext4Ino>,
}

impl OrphanLink {
    /// Consumes the credential, yielding the previous chain head the inode's
    /// `i_dtime` must record (`None` = it becomes the last member).
    pub(super) fn into_old_head(self) -> Option<Ext4Ino> {
        self.old_head
    }
}

/// What removing an inode from the [`OrphanChain`] must persist.
enum OrphanSplice {
    /// The inode is the head: the superblock's `s_last_orphan` must advance to
    /// `successor` (`None` = the list becomes empty).
    Head { successor: Option<Ext4Ino> },
    /// A middle/tail member: `predecessor`'s on-disk `i_dtime` must be
    /// repointed to `successor` (`None` = it becomes the last member).
    Middle {
        predecessor: Ext4Ino,
        successor: Option<Ext4Ino>,
    },
}

impl OrphanChain {
    const fn new() -> Self {
        Self(Vec::new())
    }

    /// Returns whether `ino` is on the chain and, if listed, its successor —
    /// `Some(None)` = listed as the last member, outer `None` = not listed.
    /// One query, so the two states cannot be conflated by a missed
    /// pre-check.
    fn successor_of(&self, ino: Ext4Ino) -> Option<Option<Ext4Ino>> {
        let idx = self.0.iter().position(|&i| i == ino)?;
        Some(self.0.get(idx + 1).copied())
    }

    /// Prepends a new head. The caller has already persisted it as
    /// `s_last_orphan` (journaled) — see the two-phase note on the type.
    fn push_head(&mut self, ino: Ext4Ino) {
        self.0.insert(0, ino);
    }

    /// Returns what removing `ino` must persist, or `None` when it is not
    /// listed.
    /// Read-only; pair with [`commit_remove`](Self::commit_remove) once the
    /// persist step succeeded.
    fn splice_for(&self, ino: Ext4Ino) -> Option<OrphanSplice> {
        let idx = self.0.iter().position(|&i| i == ino)?;
        let successor = self.0.get(idx + 1).copied();
        Some(if idx == 0 {
            OrphanSplice::Head { successor }
        } else {
            OrphanSplice::Middle {
                predecessor: self.0[idx - 1],
                successor,
            }
        })
    }

    /// Removes `ino` from the mirror after its splice was persisted.
    fn commit_remove(&mut self, ino: Ext4Ino) {
        let Some(idx) = self.0.iter().position(|&i| i == ino) else {
            debug_assert!(false, "commit_remove of an unlisted orphan inode");
            return;
        };
        self.0.remove(idx);
    }

    /// Replaces the whole mirror with a freshly walked on-disk chain (the
    /// mount-time orphan scan).
    fn replace(&mut self, chain: Vec<Ext4Ino>) {
        self.0 = chain;
    }

    /// Empties the mirror (the on-disk head was cleared).
    fn clear(&mut self) {
        self.0.clear();
    }
}

/// The device location of one on-disk `RawInode` slot: its inode-table block
/// plus the byte offset inside that block. Built by [`Ext4::inode_slot`]; the
/// journaled writers and the raw reader hang off it, so the block/offset
/// decomposition exists exactly once. (Inode-table blocks are inode-size
/// aligned, so a slot never straddles a block boundary.)
struct InodeSlot {
    bid: Ext4Bid,
    offset_in_block: usize,
}

impl InodeSlot {
    /// Returns the absolute device byte offset of the slot, for direct
    /// reads/writes.
    fn device_offset(&self) -> usize {
        Bid::new(self.bid).to_offset() + self.offset_in_block
    }

    /// Reads the slot's on-disk bytes. The caller owns the judgement that the
    /// device is authoritative here (e.g. the mount-time orphan scan, which
    /// runs after replay with nothing in flight).
    fn read_raw(&self, device: &dyn BlockDevice) -> Result<RawInode> {
        device
            .read_val::<RawInode>(self.device_offset())
            .map_err(|_| Error::with_message(Errno::EIO, "failed to read raw inode"))
    }

    /// Captures the slot's after-image into the operation's transaction, a
    /// sub-block RMW of its inode-table block, for op-time journaling. A no-op
    /// without a handle.
    ///
    /// Only the slot's `RawInode` bytes are patched, preserving the rest of
    /// the block — the other inodes sharing it and the slot bytes past the
    /// written `RawInode` prefix — from the seed. The partial patch is sound
    /// only because the seed is current: `get_write_access` seeds from the
    /// newest committed-but-un-checkpointed image of the block when one is
    /// retained, falling back to the device (see `UncheckpointedImage` in
    /// `journal/transaction.rs`; a raw device seed would lag pending
    /// checkpoints and silently clobber the neighboring inodes — the Task 8
    /// guest data loss).
    fn journal_write(&self, handle: Option<&journal::Handle>, raw: &RawInode) -> Result<()> {
        let off = self.offset_in_block;
        journal::get_write_access(handle, self.bid)?.patch(|buf| {
            buf[off..off + size_of::<RawInode>()].copy_from_slice(raw.as_bytes());
        })
    }

    /// Splices the slot's on-disk `i_dtime` (its orphan-next pointer) to
    /// `next` with a journaled patch. A no-op without a handle.
    ///
    /// The patch is sound without locking the inode: the capture's seed is the
    /// block's newest committed image (see `journal_write`), and only the 4
    /// `i_dtime` bytes (plus, on a `metadata_csum` volume, the inode's own
    /// checksum) are overwritten, so every other field — and every neighboring
    /// inode — keeps its committed content.
    ///
    /// `csum` carries `(ino, fs_seed, inode_size)` on a checksummed volume: the
    /// `i_dtime` change invalidates this inode's checksum, so the whole slot is
    /// decoded from the seeded image, re-stamped, and written back (Linux
    /// `ext4_orphan_del` rewrites the predecessor via `ext4_inode_csum_set`).
    /// Without it, the minimal 4-byte patch stands (Phases 1-5 verbatim).
    fn journal_patch_dtime(
        &self,
        handle: Option<&journal::Handle>,
        next: u32,
        csum: Option<(Ext4Ino, FsCsumSeed, usize)>,
    ) -> Result<()> {
        let off = self.offset_in_block;
        let dtime_off = off + core::mem::offset_of!(RawInode, dtime);
        journal::get_write_access(handle, self.bid)?.patch(|buf| {
            buf[dtime_off..dtime_off + size_of::<u32>()].copy_from_slice(&next.to_le_bytes());
            if let Some((ino, fs_seed, inode_size)) = csum {
                let mut raw = RawInode::from_bytes(&buf[off..off + size_of::<RawInode>()]);
                let iseed = fs_seed.derive_inode(ino, raw.generation);
                InodeDesc::stamp_inode_checksum(&mut raw, iseed, inode_size);
                buf[off..off + size_of::<RawInode>()].copy_from_slice(raw.as_bytes());
            }
        })
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::{
        super::{
            block_group::RawBlockGroup,
            test_utils::{
                Ext4FixtureBuilder, Ext4MemoryDisk, make_empty_file_inode, make_file_inode,
            },
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

        f.ext4
            .free_blocks(
                journal::BlockFreeAuth::without_revoke_duty(range.start, alloc_len),
                None,
            )
            .unwrap();
        assert_eq!(f.ext4.block_group(0).free_blocks_count(), before_group_free);
        assert_eq!(f.ext4.super_block().free_blocks_count(), before_sb_free);
    }

    /// MAJOR-B red→green: the forget effects (capture cancel, revoke record,
    /// retained-image eviction) run at the authorization's CONSUMPTION —
    /// inside `free_blocks`, with the bitmap clear — never at mint. An
    /// operation that errors between minting and freeing (a multi-extent
    /// truncate failing mid-loop, a failed tree reserialize) must leave the
    /// not-yet-freed blocks' journal state untouched: a standing revoke for a
    /// still-referenced block would suppress its legitimate journaled updates
    /// at checkpoint (silent content rollback), and the cancelled capture's
    /// update would be lost. Reverting to effects-at-mint fails every `b2`
    /// assertion below.
    #[ktest]
    fn forget_effects_run_at_free_not_at_mint() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_journal_inode(32)
            .build()
            .unwrap();
        let journal = f.ext4.journal().unwrap();
        journal.stop_commit_thread();

        // Two journaled "metadata" blocks (as two extents' tree/dir blocks),
        // committed but NOT checkpointed so both have retained images.
        let op1 = f.ext4.begin_op(8).unwrap();
        let b1 = f.ext4.alloc_blocks(1, 0, op1.get()).unwrap().start;
        let b2 = f.ext4.alloc_blocks(1, 0, op1.get()).unwrap().start;
        journal::get_create_access(op1.get(), b1)
            .unwrap()
            .patch(|b| b[..4].copy_from_slice(&[0x11; 4]))
            .unwrap();
        journal::get_create_access(op1.get(), b2)
            .unwrap()
            .patch(|b| b[..4].copy_from_slice(&[0x22; 4]))
            .unwrap();
        drop(op1);
        journal.commit_now_for_test();
        assert!(journal.uncheckpointed_blocks_for_test().contains(&b1));
        assert!(journal.uncheckpointed_blocks_for_test().contains(&b2));

        // A fresh transaction journals both blocks again (their next update).
        let op2 = f.ext4.begin_op(8).unwrap();
        journal::get_write_access(op2.get(), b1)
            .unwrap()
            .patch(|b| b[4..8].copy_from_slice(&[0x33; 4]))
            .unwrap();
        journal::get_write_access(op2.get(), b2)
            .unwrap()
            .patch(|b| b[4..8].copy_from_slice(&[0x44; 4]))
            .unwrap();
        let sb_free_before = f.ext4.super_block().free_blocks_count();

        // Extent #1: minted AND consumed — all three effects land, with the
        // bitmap clear.
        f.ext4
            .free_blocks(journal::forget(b1, 1), op2.get())
            .unwrap();
        assert!(journal.running_revoked_blocks_for_test().contains(&b1));
        assert!(!journal.running_captured_blocks_for_test().contains(&b1));
        assert!(!journal.uncheckpointed_blocks_for_test().contains(&b1));
        assert_eq!(f.ext4.super_block().free_blocks_count(), sb_free_before + 1);

        // Extent #2: minted, then DROPPED — the operation errored before this
        // extent's free. A true no-op: capture intact, NO revoke recorded,
        // retained image intact, bitmap untouched.
        let auth = journal::forget(b2, 1);
        drop(auth);
        assert!(!journal.running_revoked_blocks_for_test().contains(&b2));
        assert!(journal.running_captured_blocks_for_test().contains(&b2));
        assert!(journal.uncheckpointed_blocks_for_test().contains(&b2));
        assert_eq!(f.ext4.super_block().free_blocks_count(), sb_free_before + 1);

        drop(op2);
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
            .create_inode(ROOT_INO, InodeType::File, perm, InodeSeed::ExtentRoot, None)
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
            .create_inode(ROOT_INO, InodeType::Dir, perm, InodeSeed::ExtentRoot, None)
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
            .create_inode(ROOT_INO, InodeType::File, perm, InodeSeed::ExtentRoot, None)
            .unwrap();
        let b = f
            .ext4
            .create_inode(ROOT_INO, InodeType::File, perm, InodeSeed::ExtentRoot, None)
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
        let err =
            match f
                .ext4
                .create_inode(ROOT_INO, InodeType::File, perm, InodeSeed::ExtentRoot, None)
            {
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

    use super::super::test_utils::JOURNAL_START_BLOCK;

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
            committed_after.geq(committed_before.next()),
            "fsync must wait for its commit (before={committed_before:?}, after={committed_after:?})"
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
        // During the recovered session RECOVER stays STAMPED (the session's own
        // crash protection — cleared only at clean unmount, Linux parity).
        let raw_sb_during = disk
            .segment()
            .read_val::<RawSuperBlock>(SUPER_BLOCK_OFFSET)
            .unwrap();
        assert_ne!(raw_sb_during.feature_incompat & RECOVER_BIT, 0);

        // The journal is loaded on the recovered mount too.
        assert!(ext4.journal().is_some());

        // Clean unmount drops the stamp: the next mount skips recovery.
        drop(ext4);
        let raw_sb_after = disk
            .segment()
            .read_val::<RawSuperBlock>(SUPER_BLOCK_OFFSET)
            .unwrap();
        assert_eq!(raw_sb_after.feature_incompat & RECOVER_BIT, 0);
    }

    /// The session-crash story (the reason RECOVER is stamped at mount): a
    /// journaled op commits to the log but crashes before checkpoint — the
    /// next mount MUST replay it. Without the mount-time RECOVER stamp the
    /// device superblock shows no recovery need (`last_orphan == 0`) and the
    /// committed transaction would be silently discarded (the fsync-durability
    /// hole the adversarial review confirmed).
    #[ktest]
    fn crash_of_our_own_mount_replays_committed_log_on_next_mount() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_journal_inode(64)
            .build()
            .unwrap();
        let disk = f.disk.clone();
        let journal = f.ext4.journal().unwrap();
        journal.stop_commit_thread();

        // A journaled inode writeback, committed to the LOG but not
        // checkpointed — the state fsync acknowledges.
        let ino = ROOT_INO;
        let mut desc = f.ext4.read_inode_desc(ino).unwrap();
        let new_link = desc.link_count() + 5;
        desc.set_link_count(new_link);
        let root = *desc.raw_block();
        let offset = f.ext4.inode_table_offset(ino).unwrap();
        {
            let op = f.ext4.begin_op(4).unwrap();
            f.ext4
                .write_back_inode_desc(ino, &desc, &root, op.get())
                .unwrap();
        }
        journal.commit_now_for_test();
        let stale: RawInode = disk.segment().read_val(offset).unwrap();
        assert_ne!(stale.link_count, new_link, "not yet checkpointed");

        // Crash: the fs is never dropped (no unmount flush, no RECOVER clear).
        core::mem::forget(f);

        // The next mount must replay the log (RECOVER was stamped at mount).
        let ext4 = Ext4::open(disk.clone() as Arc<dyn BlockDevice>).unwrap();
        let replayed: RawInode = disk.segment().read_val(offset).unwrap();
        assert_eq!(
            replayed.link_count, new_link,
            "the committed transaction was replayed, not discarded"
        );
        assert_eq!(ext4.read_inode_desc(ino).unwrap().link_count(), new_link);
        drop(ext4);
    }

    /// Mount contract (report §4.5): a superblock whose `s_journal_inum` names
    /// anything but the reserved ino 8 must fail the mount — silently parsing
    /// some other inode as the journal would "replay" unrelated file content.
    #[ktest]
    fn read_inode_rejects_out_of_range_ino() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        // A corrupt dirent can carry any 32-bit ino; both the inode-count
        // bound and the group-range check must answer ENOENT, never panic.
        for ino in [u32::MAX, 1 << 20] {
            let Err(err) = f.ext4.read_inode(ino) else {
                panic!("out-of-range ino {ino} must not resolve");
            };
            assert_eq!(err.error(), Errno::ENOENT);
        }
    }

    #[ktest]
    fn shutdown_freezes_the_filesystem() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_journal_inode(64)
            .build()
            .unwrap();
        let journal = f.ext4.journal().unwrap();

        // Unknown EXT4_GOING_FLAGS values are rejected.
        assert_eq!(GoingDown::try_from(3).unwrap_err().error(), Errno::EINVAL);

        f.ext4.shutdown(GoingDown::NoLogFlush).unwrap();
        assert!(f.ext4.is_shutdown());
        assert!(journal.is_aborted());
        // New operations and writeback die with EIO...
        assert_eq!(
            f.ext4.begin_op(4).map(|_| ()).unwrap_err().error(),
            Errno::EIO
        );
        assert_eq!(f.ext4.sync_all().unwrap_err().error(), Errno::EIO);
        // ...while sync(2) succeeds as a no-op (Linux parity), and a repeat
        // shutdown is idempotent.
        crate::fs::vfs::file_system::FileSystem::sync(f.ext4.as_ref()).unwrap();
        f.ext4.shutdown(GoingDown::NoLogFlush).unwrap();
    }

    #[ktest]
    fn mount_rejects_nonstandard_journal_inum() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_journal_inode(16)
            .build()
            .unwrap();
        let disk = f.disk.clone();
        drop(f); // clean unmount; the doctored field survives the RMW writeback

        let mut raw: RawSuperBlock = disk.segment().read_val(SUPER_BLOCK_OFFSET).unwrap();
        raw.journal_ino = 12;
        disk.segment().write_val(SUPER_BLOCK_OFFSET, &raw).unwrap();

        let Err(err) = Ext4::open(disk as Arc<dyn BlockDevice>) else {
            panic!("mount must reject s_journal_inum != 8");
        };
        assert_eq!(err.error(), Errno::EINVAL);
    }

    /// Mount contract (report §4.5): an external journal (`s_journal_dev != 0`)
    /// is unsupported and must fail the mount, not be silently treated as an
    /// internal ino-8 journal.
    #[ktest]
    fn mount_rejects_external_journal_device() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_journal_inode(16)
            .build()
            .unwrap();
        let disk = f.disk.clone();
        drop(f);

        let mut raw: RawSuperBlock = disk.segment().read_val(SUPER_BLOCK_OFFSET).unwrap();
        raw.journal_dev = 0xff00;
        disk.segment().write_val(SUPER_BLOCK_OFFSET, &raw).unwrap();

        let Err(err) = Ext4::open(disk as Arc<dyn BlockDevice>) else {
            panic!("mount must reject an external journal device");
        };
        assert_eq!(err.error(), Errno::EINVAL);
    }

    /// The mount-time orphan scan must never free a chain member with a
    /// nonzero link count — that is a Linux crash-mid-truncate orphan (a LIVE
    /// file); it is skipped and the head is cleared instead.
    #[ktest]
    fn mount_scan_skips_live_linked_orphan() {
        crate::time::clocks::init_for_ktest();
        let first = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_inode_bitmap_metadata_marked()
            .with_journal_inode(64)
            .build()
            .unwrap();
        let disk = first.disk.clone();

        // A chained inode with link count 1 — the shape Linux leaves when it
        // crashes mid-truncate (the file is still referenced!).
        let live_ino: u32 = 15;
        let mut raw = make_empty_file_inode();
        raw.link_count = 1;
        first.write_raw_inode(live_ino, &raw);
        mark_inode_bit_allocated_on_disk(&disk, live_ino);
        let mut raw_sb: RawSuperBlock = disk.segment().read_val(SUPER_BLOCK_OFFSET).unwrap();
        raw_sb.last_orphan = live_ino;
        disk.segment()
            .write_val(SUPER_BLOCK_OFFSET, &raw_sb)
            .unwrap();
        drop(first);

        let ext4 = Ext4::open(disk.clone() as Arc<dyn BlockDevice>).unwrap();
        assert!(
            ext4.is_inode_allocated(live_ino),
            "a linked (live) chain member must not be freed"
        );
        assert_eq!(ext4.super_block().last_orphan(), None, "head cleared");
        drop(ext4);
    }

    // --- Int-B Task 8: journaled orphan list + mount-time crash recovery. ---

    /// Marks `ino`'s bit allocated directly in the on-disk inode bitmap (group
    /// 0's inode bitmap is block 3 in the fixture layout) — fabricating the
    /// "crashed mid-deletion" state for the mount-scan tests.
    fn mark_inode_bit_allocated_on_disk(disk: &Arc<Ext4MemoryDisk>, ino: u32) {
        const INODE_BITMAP_BLOCK: usize = 3;
        let bit = (ino - 1) as usize;
        let byte_off = INODE_BITMAP_BLOCK * BLOCK_SIZE + bit / 8;
        let mut byte = [0u8; 1];
        disk.segment().read_bytes(byte_off, &mut byte).unwrap();
        byte[0] |= 1 << (bit % 8);
        disk.segment().write_bytes(byte_off, &byte).unwrap();
    }

    /// Orphan chain bookkeeping: journaled adds push the superblock head and
    /// return the successor; a non-head removal splices the predecessor's
    /// on-disk `i_dtime` with a journaled 4-byte patch; head removals advance
    /// the head; a non-journaled add is a no-op (the machinery is journal-only).
    #[ktest]
    fn orphan_chain_journaled_add_del_and_disk_splice() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_inode_bitmap_metadata_marked()
            .with_journal_inode(64)
            .build()
            .unwrap();
        let journal = f.ext4.journal().unwrap();
        journal.stop_commit_thread();

        let a = f.ext4.alloc_ino(ROOT_INO, InodeType::File, None).unwrap();
        let b = f.ext4.alloc_ino(ROOT_INO, InodeType::File, None).unwrap();
        let c = f.ext4.alloc_ino(ROOT_INO, InodeType::File, None).unwrap();

        // Journal-only: without a handle the add is a no-op returning 0.
        assert_eq!(f.ext4.orphan_add(a, None).unwrap().into_old_head(), None);
        assert_eq!(f.ext4.super_block().last_orphan(), None);

        {
            let op = f.ext4.begin_op(8).unwrap();
            // Build the chain c → b → a; each add returns the previous head.
            assert_eq!(
                f.ext4.orphan_add(a, op.get()).unwrap().into_old_head(),
                None
            );
            assert_eq!(
                f.ext4.orphan_add(b, op.get()).unwrap().into_old_head(),
                Some(a)
            );
            assert_eq!(
                f.ext4.orphan_add(c, op.get()).unwrap().into_old_head(),
                Some(b)
            );
            assert_eq!(f.ext4.super_block().last_orphan(), Some(c));

            // Remove the middle b: the head is untouched and predecessor c is
            // spliced (on disk, journaled) to point at a.
            f.ext4.orphan_del(b, op.get()).unwrap();
            assert_eq!(
                f.ext4.super_block().last_orphan(),
                Some(c),
                "head unchanged"
            );

            // Remove the head c: the head advances past the spliced-out b to a.
            f.ext4.orphan_del(c, op.get()).unwrap();
            assert_eq!(f.ext4.super_block().last_orphan(), Some(a));

            // Drain.
            f.ext4.orphan_del(a, op.get()).unwrap();
            assert_eq!(f.ext4.super_block().last_orphan(), None);
        }
        journal.commit_now_for_test();
        journal.flush_on_unmount().unwrap();

        // The splice reached c's on-disk `i_dtime` (b's successor a), and the
        // drained head reached the on-disk superblock.
        let raw_c: RawInode = f
            .disk
            .segment()
            .read_val(f.ext4.inode_table_offset(c).unwrap())
            .unwrap();
        assert_eq!(raw_c.dtime, a, "predecessor spliced on disk to skip b");
        let sb_disk: RawSuperBlock = f.disk.segment().read_val(SUPER_BLOCK_OFFSET).unwrap();
        assert_eq!(sb_disk.last_orphan, 0);
    }

    /// The journaled inode writeback serializes an on-list inode's `i_dtime`
    /// from the in-memory chain, not from its (possibly stale) descriptor — a
    /// non-head splice can never be resurrected by a later writeback.
    #[ktest]
    fn journaled_writeback_pulls_orphan_next_from_chain() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_inode_bitmap_metadata_marked()
            .with_journal_inode(64)
            .build()
            .unwrap();
        let journal = f.ext4.journal().unwrap();
        journal.stop_commit_thread();

        // A decodable on-disk file inode for `b` (the one we write back).
        let a = f.ext4.alloc_ino(ROOT_INO, InodeType::File, None).unwrap();
        let b = f.ext4.alloc_ino(ROOT_INO, InodeType::File, None).unwrap();
        f.write_raw_inode(b, &make_empty_file_inode());

        {
            let op = f.ext4.begin_op(8).unwrap();
            let _ = f.ext4.orphan_add(a, op.get()).unwrap().into_old_head();
            let _ = f.ext4.orphan_add(b, op.get()).unwrap().into_old_head();

            // Write `b` back with a descriptor whose `i_dtime` is 0 (never told
            // about the chain): the writeback must serialize successor `a` from
            // the chain regardless.
            let desc_b = f.ext4.read_inode_desc(b).unwrap();
            assert_eq!(desc_b.raw_dtime(), 0);
            let root_b = *desc_b.raw_block();
            f.ext4
                .write_back_inode_desc(b, &desc_b, &root_b, op.get())
                .unwrap();
        }
        journal.commit_now_for_test();
        journal.flush_on_unmount().unwrap();

        let raw_b: RawInode = f
            .disk
            .segment()
            .read_val(f.ext4.inode_table_offset(b).unwrap())
            .unwrap();
        assert_eq!(
            raw_b.dtime, a,
            "writeback serialized the chain successor, not the stale descriptor"
        );
    }

    /// Mount-time orphan recovery (the Task 8 crash story): a volume whose
    /// superblock points at an allocated link-0 inode — the state a crash
    /// between the unlink transaction and the reclaim transaction leaves — gets
    /// that deletion finished at the next mount: inode freed, list drained, and
    /// the drained head persisted.
    #[ktest]
    fn mount_scan_frees_crashed_orphan() {
        crate::time::clocks::init_for_ktest();
        let first = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_inode_bitmap_metadata_marked()
            .with_journal_inode(64)
            .build()
            .unwrap();
        let disk = first.disk.clone();
        let free_inodes_before = first.ext4.super_block().free_inodes_count();

        // Fabricate the crashed orphan on disk: a link-0 empty file inode,
        // allocated in the bitmap, at the head of the on-disk orphan list. The
        // first mount's in-memory state never dirtied, so dropping it below
        // rewrites none of these bytes.
        let orphan_ino: u32 = 15;
        let mut raw = make_empty_file_inode();
        raw.link_count = 0;
        first.write_raw_inode(orphan_ino, &raw);
        mark_inode_bit_allocated_on_disk(&disk, orphan_ino);
        let mut raw_sb: RawSuperBlock = disk.segment().read_val(SUPER_BLOCK_OFFSET).unwrap();
        raw_sb.last_orphan = orphan_ino;
        disk.segment()
            .write_val(SUPER_BLOCK_OFFSET, &raw_sb)
            .unwrap();
        drop(first);

        // Re-mount: recovery must finish the interrupted deletion.
        let ext4 = Ext4::open(disk.clone() as Arc<dyn BlockDevice>).unwrap();
        assert!(
            !ext4.is_inode_allocated(orphan_ino),
            "the orphan inode was freed by the mount scan"
        );
        assert_eq!(ext4.super_block().last_orphan(), None, "list drained");
        assert_eq!(
            ext4.super_block().free_inodes_count(),
            free_inodes_before + 1,
            "the freed inode returned to the free count"
        );

        // The drained state reaches disk by the unmount flush.
        drop(ext4);
        let sb_after: RawSuperBlock = disk.segment().read_val(SUPER_BLOCK_OFFSET).unwrap();
        assert_eq!(sb_after.last_orphan, 0);
    }

    /// Mount-time orphan recovery distrusts the disk: a garbage `s_last_orphan`
    /// (out of range / unallocated) must not panic the mount or free anything —
    /// the head is cleared and the volume mounts normally.
    #[ktest]
    fn mount_scan_clears_garbage_orphan_head() {
        crate::time::clocks::init_for_ktest();
        let first = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_inode_bitmap_metadata_marked()
            .with_journal_inode(64)
            .build()
            .unwrap();
        let disk = first.disk.clone();
        drop(first);

        let mut raw_sb: RawSuperBlock = disk.segment().read_val(SUPER_BLOCK_OFFSET).unwrap();
        raw_sb.last_orphan = 60000; // way past `total_inodes`
        disk.segment()
            .write_val(SUPER_BLOCK_OFFSET, &raw_sb)
            .unwrap();

        let ext4 = Ext4::open(disk.clone() as Arc<dyn BlockDevice>).unwrap();
        assert_eq!(
            ext4.super_block().last_orphan(),
            None,
            "garbage head cleared"
        );
        drop(ext4);
        let sb_after: RawSuperBlock = disk.segment().read_val(SUPER_BLOCK_OFFSET).unwrap();
        assert_eq!(sb_after.last_orphan, 0, "cleared head persisted");
    }
}
