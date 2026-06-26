// SPDX-License-Identifier: MPL-2.0

use alloc::{
    collections::{BTreeMap, BTreeSet},
    string::String,
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use aster_block::{
    BlockDevice, SECTOR_SIZE,
    bio::{
        BioDirection, BioSegment, BioStatus, BioWaiter, dump_read_bio_profile,
        dump_write_bio_profile, reset_read_bio_profile, reset_write_bio_profile,
        set_write_bio_profile_enabled,
    },
    id::Bid,
    request_queue::bio_request_merge_count,
};
use aster_cmdline::{KCMDLINE, ModuleArg};
use aster_time::read_monotonic_time;
// Phase 6 Task 5b: the integration-layer ext4 types (mode bits, root inode, block size, `Simple*`
// DTOs, metadata-writer seam trait) now come from the in-tree `super::types` module and `core/`
// instead of the deleted third-party `ext4_rs` crate. Byte-identical replacements throughout.
use super::core::alloc_guard::LocalOperationAllocGuard;
use super::core::file::SimpleBlockRange;
use super::types::{
    DeviceMetadataWriter as Ext4MetadataWriter, EXT4_BLOCK_SIZE, EXT4_ROOT_INODE, SimpleDirEntry,
    SimpleInodeMeta, mode,
};
// Phase 6 Task 2: the injected-crash replay hold now keys off the **core** commit-write stage enum
// (the production commit emitter is core's `write_commit_plan`). It is variant-for-variant identical
// to the ext4_rs stage it replaces.
use super::core::journal::commit::JournalCommitWriteStage;
use ostd::{
    Error as OstdError,
    mm::{
        HasPaddr, PAGE_SIZE, PageFlags, Vaddr, VmIo, VmWriter, io_util::HasVmReaderWriter,
        vm_space::VmQueriedItem,
    },
    sync::{RwMutex, WaitQueue},
    task::disable_preempt,
};

use crate::{
    fs::{
        path::PerMountFlags,
        registry::{FsProperties, FsType},
        utils::{
            FallocMode, FileSystem, FsEventSubscriberStats, FsFlags, Inode, NAME_MAX, StatusFlags,
            SuperBlock,
        },
    },
    prelude::*,
    vm::{
        vmar::{VMAR_CAP_ADDR, VMAR_LOWEST_ADDR},
        vmo::Vmo,
    },
};

// Phase 8 move-only split: items relocated to sibling modules (leaf structs/consts) are referenced
// back here by the `impl Ext4Fs` methods that still live in `fs.rs`. No behavior change.
use super::caches::{
    DirEntryCache, DirEntryCacheEntry, DirLookupCacheResult, DirectReadCache, ExtentMapCacheEntry,
    PendingDirectRead, PreparedDirectRead, WRITTEN_COVERAGE_MAX_INODES, WrittenCoverage,
};
use super::device_adapter::KernelBlockDeviceAdapter;
use super::journal_driver::JournalIoBridge;
use super::page_cache::Ext4PageCacheState;
use super::profile::{
    BufferedWriteProfileStats, DirectReadProfileStats, DirectWriteBioCallProfile,
    DirectWriteProfileStats, Ext4RsRuntimeLockStats, FsyncProfileStats,
    GENERIC014_PROGRESS_LOG_INTERVAL, GENERIC014_SLOW_OP_LOG_THRESHOLD_NS, GENERIC014_TRUNCATE_PROGRESS,
    GENERIC014_WRITE_PROGRESS, Jbd2DriverDebugStats, JournaledOpProfileStats,
};
use super::run::{EXT4_RS_RUNTIME_LOCK, JournaledOp, REGULAR_FILE_FSYNC_CHECKPOINT_DEPTH};

const EXT4_MAGIC: u64 = 0xEF53;

pub(super) const EXT4_SUPERBLOCK_OFFSET: usize = 1024;
const EXT4_SB_LOG_BLOCK_SIZE_OFFSET: usize = 24;
const EXT4_SB_BLOCKS_PER_GROUP_OFFSET: usize = 32;
const EXT4_SB_INODES_PER_GROUP_OFFSET: usize = 40;
const EXT4_SB_MAGIC_OFFSET: usize = 56;
const EXT4_SB_DESC_SIZE_OFFSET: usize = 254;
const JOURNALED_SMALL_WRITE_MAX_BYTES: usize = 192;

pub(super) struct Ext4Fs {
    // Phase 6 Task 5a: the ext4_rs `Ext4` handle has been removed. EVERY production path — mount,
    // read, journaled write, namespace, setattr, fsync, recovery, RECOVER-flag, and the last
    // vestige `load_dir_cache_if_needed_locked` (dir-entry cache seed) — is now driven by `core/`
    // over the overlay read seam + journal driver. ext4_rs is referenced only by the `#[cfg(ktest)]`
    // differential tests (slated for Task 5b deletion).
    pub(super) block_device: Arc<dyn BlockDevice>,
    pub(super) adapter: Arc<KernelBlockDeviceAdapter>,
    // Phase 6 Task 1: a mount-time snapshot of the on-disk superblock, parsed once via the core
    // read seam (`read_superblock` over the overlay bridge). The production READ path is now driven
    // by `core/` (stateless free fns over an injected `ReadCtx`), and every core read fn only
    // consumes immutable geometry/feature fields of the superblock (`block_size`, `inode_size`,
    // `inodes_per_group`, `uuid`, `features_read_only`, `blocks_count`, `group_desc_size`,
    // `inodes_count`, `first_data_block`) — never the mutable free counters. Those fields are fixed
    // by mkfs and never change after mount, so a single snapshot is correct for all reads and
    // avoids an extra 1024-byte device read per read op (the write path stays on ext4_rs, which
    // owns its own SB; this copy feeds reads only).
    pub(super) core_sb: super::core::superblock::RawSuperblock,
    // Phase 6 Task 4 (★SB ownership transition): the **running** authoritative superblock — holds the
    // live free-block / free-inode counts. Replaces ext4_rs's `inner.allocator_locks.superblock`
    // (`lock_superblock_counter()`) as the single source of truth for free counts. Seeded once at
    // mount from `core_sb` (which parsed the same on-disk SB ext4_rs would have); the journaled-write
    // / namespace chokepoints (`run_journaled_core` / `run_journaled_namespace`) read its free counts
    // to seed each per-op core allocator, then sink the post-op counts back. Core's `write_superblock`
    // persists the full 1024-byte SB to disk per op, so this is the in-memory mirror of disk. The C1
    // two-engine seed/sink is gone — this is the sole running-SB owner.
    //
    // NOTE on geometry: only the free counts ever change here; geometry stays at mount values. Read
    // paths + statfs (`sb()`) keep reading the FROZEN `core_sb` (statfs reports mount-time free counts,
    // byte-frozen behavior preserved — ext4_rs `inner.super_block` was likewise frozen).
    pub(super) running_sb: Mutex<super::core::superblock::RawSuperblock>,
    // Phase 6 Task 4: the in-memory `EXT4_FEATURE_INCOMPAT_RECOVER` (needs_recovery / dirty-log) flag.
    // Replaces the only mutable part of ext4_rs's frozen `inner.super_block` (its RECOVER bit). The
    // lazy first-commit set (`mark_needs_recovery_if_needed`), the shutdown set
    // (`mark_needs_recovery_for_shutdown`) and the mount-time clear (after recovery) toggle this and
    // persist a `core_sb`-based SB image (mount-time free counts, RECOVER bit set/clear, csum
    // recomputed) to disk — byte-frozen vs ext4_rs's `inner.super_block.sync_to_disk_with_csum`.
    pub(super) recover_flag: AtomicBool,
    pub(super) mount_flags_bits: AtomicU32,
    // Phase 6 Task 2: the integration-layer JBD2 commit/checkpoint driver, re-derived over the safe
    // `core/` journal. Holds the core in-memory runtime + the integration-owned checkpoint_list /
    // last_committed_tid / rotation / running ring geometry. Same `Arc<RwMutex<Option<..>>>` slot
    // and lock the ext4_rs `JournalRuntime` used (lock order unchanged).
    pub(super) jbd2_runtime: super::core_adapter::CoreJournalRuntimeHandle,
    pub(super) journal_io: Arc<JournalIoBridge>,
    pub(super) alloc_guard: Arc<LocalOperationAllocGuard>,
    pub(super) next_alloc_operation_id: AtomicU64,
    pub(super) jbd2_checkpoint_lock: Mutex<()>,
    pub(super) inode_correctness_locks: Mutex<BTreeMap<u32, Arc<RwMutex<()>>>>,
    pub(super) dir_correctness_locks: Mutex<BTreeMap<u32, Arc<RwMutex<()>>>>,
    /// Step 4a-2: per-ino "highest TID containing a metadata change for this
    /// inode" map.  Equivalent to Linux `EXT4_I(inode)->i_sync_tid`.
    /// Updated by `finish_jbd2_handle` after a Write/Truncate handle stops;
    /// queried by `fsync_regular_file` to find the target TID for force-commit.
    /// Entries with `tid <= last_committed_tid` are stale but harmless (the
    /// fast path in `force_commit_for_tid` filters them).  We do not actively
    /// evict to keep the lock granularity simple; eviction can be added later
    /// when memory pressure arises.
    pub(super) inode_tids: RwMutex<BTreeMap<u32, u32>>,
    /// Step 4a-2: WaitQueue for fsync force-commit waiters.  Woken after
    /// every successful `finish_commit` (i.e. `last_committed_tid` advances)
    /// and after every `stop_handle` (which may make a prev_running TX
    /// commit-ready).  Waiters re-check `last_committed_tid >= target_tid`.
    pub(super) commit_notifier: WaitQueue,
    /// Step 4b: shutdown state set by `EXT4_IOC_SHUTDOWN` ioctl.
    /// `0` = active, `1` = shutdown.  Once shutdown, `run_journaled_ext4`
    /// and `fsync_regular_file` return EIO.  Cleared automatically on
    /// remount + recovery (Phase 1 path).
    pub(super) shutdown_state: AtomicU32,
    pub(super) dir_entry_cache: Mutex<BTreeMap<u32, DirEntryCache>>,
    pub(super) inode_page_caches: Mutex<BTreeMap<u32, Arc<Ext4PageCacheState>>>,
    pub(super) open_file_handles: Mutex<BTreeMap<u32, usize>>,
    /// BUG-19 fix: inodes whose bitmap bit has already been reclaimed by the evict path
    /// (`cleanup_unlinked_file`). Guards against a double-free if both the last-ref drop
    /// (`cleanup_unlinked`) and the last-handle close (`on_close_file_handle`) race to reclaim the
    /// same inode. Cleared if the ino is reallocated (`create_at`/`mkdir_at` insert via the
    /// namespace path do not consult this set; the entry is dropped here on the next alloc that
    /// hands out the same number — see `note_inode_allocated`).
    pub(super) freed_inodes: Mutex<BTreeSet<u32>>,
    /// BUG-19 fix (atomic-context safety): inodes whose LAST open handle has closed while they were
    /// already unlinked (nlink==0), but whose actual reclaim (blocking journaled I/O) must be
    /// deferred out of the close path. `on_close_file_handle` can run inside `InodeHandle::drop` in
    /// an ATOMIC context (e.g. `dup2`/`dup3` drops the replaced fd while holding the file-table
    /// write lock with preempt disabled), where the blocking disk read of `cleanup_unlinked_file`
    /// would panic on a task switch. So the close path only RECORDS the ino here (cheap, no I/O,
    /// like the uncontended `open_file_handles` insert); the real free is drained lazily by
    /// `reclaim_pending_inode_frees` from the next SLEEPABLE fs op (namespace ops + `sync`). This
    /// mirrors ext2, which never frees in close/Drop — it frees lazily in the sleepable
    /// `sync_metadata` path. The `freed_inodes` guard keeps the eventual free idempotent.
    pub(super) pending_inode_free: Mutex<BTreeSet<u32>>,
    pub(super) inode_direct_read_cache: Mutex<BTreeMap<u32, DirectReadCache>>,
    // Phase 5: metadata-only extent mapping cache for O_DIRECT reads. Distinct
    // from `inode_direct_read_cache` above (which is the retired speculative
    // *data* read cache): this caches only the logical->physical extent mapping
    // (a few integers per extent) so sequential reads skip the per-read
    // `find_extent` walk. Holds no file data and does no speculative readahead.
    pub(super) inode_extent_map_cache: Mutex<BTreeMap<u32, ExtentMapCacheEntry>>,
    // P2 (Phase 6): per-inode written-extent coverage for the buffered
    // overwrite fast path. See `WrittenCoverage` for the ⊆-truth invariant
    // and the invalidation sites.
    pub(super) inode_written_coverage: Mutex<BTreeMap<u32, WrittenCoverage>>,
    pub(super) fsync_profile: FsyncProfileStats,
    // Phase 5: in-memory inode metadata (stat) cache. ext4_rs `get_inode_ref`
    // re-reads the inode block from the device on every stat, and the read path
    // stats several times per read (type check, size, atime), so small reads
    // paid ~25us per stat. ext2 keeps the inode in memory; this closes that gap.
    // Correctness: any journaled mutation bumps `meta_cache_generation` and
    // clears this cache (run_journaled_ext4 is the single chokepoint for all
    // create/write/truncate/setattr/dir ops); `stat` only inserts when the
    // generation did not advance across its disk read, closing the read-vs-write
    // TOCTOU.
    pub(super) inode_meta_cache: Mutex<BTreeMap<u32, SimpleInodeMeta>>,
    pub(super) meta_cache_generation: AtomicU64,
    pub(super) inode_atime_cache: Mutex<BTreeMap<u32, u32>>,
    pub(super) inode_ctime_cache: Mutex<BTreeMap<u32, u32>>,
    pub(super) inode_mtime_ctime_cache: Mutex<BTreeMap<u32, u32>>,
    pub(super) page_cache_enabled: bool,
    pub(super) direct_read_cache_enabled: bool,
    pub(super) extent_map_cache_enabled: bool,
    pub(super) phase2_profile_enabled: bool,
    pub(super) direct_read_profile_started: AtomicBool,
    pub(super) direct_write_profile_started: AtomicBool,
    pub(super) direct_read_profile: DirectReadProfileStats,
    pub(super) direct_write_profile: DirectWriteProfileStats,
    pub(super) buffered_write_profile: BufferedWriteProfileStats,
    pub(super) runtime_lock_stats: Ext4RsRuntimeLockStats,
    pub(super) journaled_op_profile: JournaledOpProfileStats,
    pub(super) fs_event_subscriber_stats: FsEventSubscriberStats,
    pub(super) self_ref: Weak<Self>,
}

impl Ext4Fs {
    pub fn open(block_device: Arc<dyn BlockDevice>) -> Arc<Self> {
        let adapter = Arc::new(KernelBlockDeviceAdapter::new(block_device.clone()));
        let jbd2_runtime = Arc::new(RwMutex::new(None));
        let alloc_guard = Arc::new(LocalOperationAllocGuard::new());
        let journal_io = Arc::new(JournalIoBridge::new(adapter.clone(), jbd2_runtime.clone()));
        // Phase 6 Task 1: parse the on-disk superblock once via the core read seam (over the same
        // overlay bridge the live read path uses) and hold a running snapshot for the core read
        // path. Reads only consume immutable geometry/feature fields (see the `core_sb` field doc).
        let core_sb = super::core_adapter::read_superblock(&super::core_adapter::CoreDeviceReader::new(
            journal_io.clone(),
        ));
        // Phase 6 Task 4: seed the running authoritative SB + in-memory RECOVER flag from the
        // mount-time on-disk SB snapshot. `running_sb` carries the live free counts (replacing the
        // ext4_rs `allocator_locks` mutex); `recover_flag` carries the RECOVER incompat bit (replacing
        // the mutable part of ext4_rs `inner.super_block`). Both start exactly at the on-disk values,
        // identical to ext4_rs's `Ext4::open` seeding both its SB copies from the same on-disk SB.
        let running_sb = Mutex::new(core_sb);
        let recover_flag = AtomicBool::new(core_sb.needs_recovery());
        let fs = Arc::new_cyclic(|weak_ref| Self {
            block_device,
            adapter,
            core_sb,
            running_sb,
            recover_flag,
            mount_flags_bits: AtomicU32::new(PerMountFlags::default().bits()),
            jbd2_runtime: jbd2_runtime.clone(),
            journal_io: journal_io.clone(),
            alloc_guard: alloc_guard.clone(),
            next_alloc_operation_id: AtomicU64::new(1),
            jbd2_checkpoint_lock: Mutex::new(()),
            inode_correctness_locks: Mutex::new(BTreeMap::new()),
            dir_correctness_locks: Mutex::new(BTreeMap::new()),
            inode_tids: RwMutex::new(BTreeMap::new()),
            commit_notifier: WaitQueue::new(),
            shutdown_state: AtomicU32::new(0),
            dir_entry_cache: Mutex::new(BTreeMap::new()),
            inode_page_caches: Mutex::new(BTreeMap::new()),
            open_file_handles: Mutex::new(BTreeMap::new()),
            freed_inodes: Mutex::new(BTreeSet::new()),
            pending_inode_free: Mutex::new(BTreeSet::new()),
            inode_direct_read_cache: Mutex::new(BTreeMap::new()),
            inode_extent_map_cache: Mutex::new(BTreeMap::new()),
            inode_written_coverage: Mutex::new(BTreeMap::new()),
            fsync_profile: FsyncProfileStats::default(),
            inode_meta_cache: Mutex::new(BTreeMap::new()),
            meta_cache_generation: AtomicU64::new(0),
            inode_atime_cache: Mutex::new(BTreeMap::new()),
            inode_ctime_cache: Mutex::new(BTreeMap::new()),
            inode_mtime_ctime_cache: Mutex::new(BTreeMap::new()),
            page_cache_enabled: Self::page_cache_enabled_from_kcmdline(),
            direct_read_cache_enabled: Self::direct_read_cache_enabled_from_kcmdline(),
            extent_map_cache_enabled: Self::extent_map_cache_enabled_from_kcmdline(),
            phase2_profile_enabled: Self::phase2_profile_enabled_from_kcmdline(),
            direct_read_profile_started: AtomicBool::new(false),
            direct_write_profile_started: AtomicBool::new(false),
            direct_read_profile: DirectReadProfileStats::new(),
            direct_write_profile: DirectWriteProfileStats::new(),
            buffered_write_profile: BufferedWriteProfileStats::new(),
            runtime_lock_stats: Ext4RsRuntimeLockStats::new(),
            journaled_op_profile: JournaledOpProfileStats::new(),
            fs_event_subscriber_stats: FsEventSubscriberStats::new(),
            self_ref: weak_ref.clone(),
        });

        set_write_bio_profile_enabled(fs.phase2_profile_enabled);
        fs.initialize_jbd2_journal();
        fs.replay_mount_jbd2_journal();
        fs
    }

    fn ext4fs_bool_arg_from_kcmdline(name: &[u8], default: bool) -> bool {
        let Some(kcmd) = KCMDLINE.get() else {
            return default;
        };
        let Some(args) = kcmd.get_module_args("ext4fs") else {
            return default;
        };

        for arg in args {
            match arg {
                ModuleArg::Arg(key) => {
                    if key.as_c_str().to_bytes() == name {
                        return true;
                    }
                }
                ModuleArg::KeyVal(key, value) => {
                    if key.as_c_str().to_bytes() != name {
                        continue;
                    }
                    return match value.as_c_str().to_bytes() {
                        b"1" | b"true" | b"yes" | b"on" => true,
                        b"0" | b"false" | b"no" | b"off" => false,
                        _ => default,
                    };
                }
            }
        }
        default
    }

    fn phase2_profile_enabled_from_kcmdline() -> bool {
        Self::ext4fs_bool_arg_from_kcmdline(b"phase2_profile", false)
    }

    fn page_cache_enabled_from_kcmdline() -> bool {
        Self::ext4fs_bool_arg_from_kcmdline(b"page_cache", false)
    }

    fn direct_read_cache_enabled_from_kcmdline() -> bool {
        Self::ext4fs_bool_arg_from_kcmdline(b"direct_read_cache", true)
    }

    /// Metadata-only O_DIRECT extent mapping cache. Default on: it is an honest
    /// filesystem optimization (the logical->physical mapping only, like Linux's
    /// extent_status cache) and is independent of the retired speculative data
    /// read cache (`direct_read_cache`), so it stays active in the cache-off
    /// benchmark guard. Disable with `ext4fs.extent_map_cache=0`.
    fn extent_map_cache_enabled_from_kcmdline() -> bool {
        Self::ext4fs_bool_arg_from_kcmdline(b"extent_map_cache", true)
    }

    pub(super) fn page_cache_enabled(&self) -> bool {
        self.page_cache_enabled
    }

    fn page_cache_state_for_inode(
        self: &Arc<Self>,
        ino: u32,
        capacity: usize,
    ) -> Result<Arc<Ext4PageCacheState>> {
        if let Some(state) = self.inode_page_caches.lock().get(&ino).cloned() {
            state.resize(capacity)?;
            return Ok(state);
        }

        let new_state = Arc::new(Ext4PageCacheState::new(
            Arc::downgrade(self),
            ino,
            capacity,
        )?);
        let state = self
            .inode_page_caches
            .lock()
            .entry(ino)
            .or_insert(new_state)
            .clone();
        state.resize(capacity)?;
        Ok(state)
    }

    pub(super) fn page_cache_for_inode(self: &Arc<Self>, ino: u32) -> Result<Arc<Vmo>> {
        let capacity = self.stat(ino)?.size as usize;
        Ok(self.page_cache_state_for_inode(ino, capacity)?.pages())
    }

    fn page_cache_state_if_present(&self, ino: u32) -> Option<Arc<Ext4PageCacheState>> {
        self.inode_page_caches.lock().get(&ino).cloned()
    }

    fn discard_page_cache_range(&self, ino: u32, start: usize, len: usize) {
        if let Some(state) = self.page_cache_state_if_present(ino) {
            state.discard_range(start, len);
        }
    }

    fn evict_page_cache_range(&self, ino: u32, start: usize, len: usize) -> Result<()> {
        if let Some(state) = self.page_cache_state_if_present(ino) {
            state.evict_range(start, len)?;
        }
        Ok(())
    }

    fn sync_page_cache_for_inode_locked(&self, ino: u32) -> Result<()> {
        let Some(state) = self.page_cache_state_if_present(ino) else {
            return Ok(());
        };
        let file_size = self.stat(ino)?.size as usize;
        // S3 (Phase 6): fsync writes dirty pages back but keeps them resident as
        // clean, instead of decommitting the whole file on every COMMIT. Keeps
        // the working set warm and removes the "clear-on-fsync -> per-4KB sync
        // refill" loop that made even in-place UPDATE 15-35x slower than ext2.
        state.flush_all(file_size)
    }

    pub(super) fn sync_page_cache_for_inode(&self, ino: u32) -> Result<()> {
        let inode_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
        let _inode_guard = inode_lock.write();
        self.sync_page_cache_for_inode_locked(ino)
    }

    /// fsync/fdatasync entry for regular files: writeback + journal
    /// force-commit + device flush, with a per-stage latency breakdown under
    /// `ext4fs.phase2_profile=1` so the fsync cost can be attributed
    /// (writeback scan+IO vs commit wait vs flush).
    pub(super) fn sync_regular_file_blocking(&self, ino: u32) -> Result<()> {
        let profile = self.phase2_profile_enabled;
        let t0 = if profile { Self::monotonic_nanos() } else { 0 };
        self.sync_page_cache_for_inode(ino)?;
        let t1 = if profile { Self::monotonic_nanos() } else { 0 };
        self.fsync_regular_file(ino)?;
        let t2 = if profile { Self::monotonic_nanos() } else { 0 };
        self.block_device
            .sync()
            .map_err(|_| Error::new(Errno::EIO))?;
        if profile {
            let t3 = Self::monotonic_nanos();
            let p = &self.fsync_profile;
            p.calls.fetch_add(1, Ordering::Relaxed);
            p.writeback_ns
                .fetch_add(t1.saturating_sub(t0), Ordering::Relaxed);
            p.commit_ns
                .fetch_add(t2.saturating_sub(t1), Ordering::Relaxed);
            p.flush_ns
                .fetch_add(t3.saturating_sub(t2), Ordering::Relaxed);
        }
        Ok(())
    }

    fn sync_all_page_caches(&self) -> Result<()> {
        let states: Vec<(u32, Arc<Ext4PageCacheState>)> = self
            .inode_page_caches
            .lock()
            .iter()
            .map(|(ino, state)| (*ino, state.clone()))
            .collect();

        for (ino, state) in states {
            let inode_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
            let _inode_guard = inode_lock.write();
            let file_size = self.stat(ino)?.size as usize;
            state.evict_all(file_size)?;
        }
        Ok(())
    }

    fn reset_page_cache_after_truncate(&self, ino: u32, new_size: usize) -> Result<()> {
        if let Some(state) = self.page_cache_state_if_present(ino) {
            state.discard_all();
            state.resize(new_size)?;
        }
        Ok(())
    }

    fn drop_page_cache_state(&self, ino: u32) {
        let Some(state) = self.inode_page_caches.lock().remove(&ino) else {
            return;
        };
        let file_size = self
            .stat(ino)
            .map(|meta| meta.size as usize)
            .unwrap_or_else(|_| state.cached_size());
        if let Err(err) = state.evict_all(file_size) {
            warn!(
                "ext4: failed to evict page cache while dropping inode state ino={} err={:?}",
                ino, err
            );
            state.discard_all();
        }
    }

    fn discard_page_cache_state(&self, ino: u32) {
        let Some(state) = self.inode_page_caches.lock().remove(&ino) else {
            return;
        };
        state.discard_all();
    }

    /// C1 (Phase 6): per-inode/dir correctness locks are RwMutex. Every
    /// metadata-mutating or page-cache-touching path takes `.write()`
    /// (semantics identical to the previous Mutex); the O_DIRECT
    /// overwrite-write and read paths under `page_cache=0` take `.read()` so
    /// concurrent dio on the same file is no longer serialized across the
    /// device wait (Linux ext4 shared-i_rwsem dio overwrite equivalent).
    fn correctness_lock_for(
        table: &Mutex<BTreeMap<u32, Arc<RwMutex<()>>>>,
        ino: u32,
    ) -> Arc<RwMutex<()>> {
        table
            .lock()
            .entry(ino)
            .or_insert_with(|| Arc::new(RwMutex::new(())))
            .clone()
    }

    fn sorted_unique_inos(inos: &[u32]) -> Vec<u32> {
        let mut sorted = Vec::new();
        for &ino in inos {
            if !sorted.contains(&ino) {
                sorted.push(ino);
            }
        }
        sorted.sort_unstable();
        sorted
    }

    fn with_inode_locks<T>(&self, inos: &[u32], f: impl FnOnce() -> Result<T>) -> Result<T> {
        let sorted = Self::sorted_unique_inos(inos);
        let locks: Vec<_> = sorted
            .iter()
            .map(|&ino| Self::correctness_lock_for(&self.inode_correctness_locks, ino))
            .collect();
        let mut guards = Vec::with_capacity(locks.len());
        for lock in &locks {
            guards.push(lock.write());
        }
        f()
    }

    fn with_inode_lock<T>(&self, ino: u32, f: impl FnOnce() -> Result<T>) -> Result<T> {
        self.with_inode_locks(&[ino], f)
    }

    fn with_dir_locks<T>(&self, inos: &[u32], f: impl FnOnce() -> Result<T>) -> Result<T> {
        let sorted = Self::sorted_unique_inos(inos);
        let locks: Vec<_> = sorted
            .iter()
            .map(|&ino| Self::correctness_lock_for(&self.dir_correctness_locks, ino))
            .collect();
        let mut guards = Vec::with_capacity(locks.len());
        for lock in &locks {
            guards.push(lock.write());
        }
        f()
    }

    #[inline]
    pub(super) fn now_unix_seconds_u32() -> u32 {
        let secs = crate::time::clocks::RealTimeClock::get()
            .read_time()
            .as_secs();
        u32::try_from(secs).unwrap_or(u32::MAX)
    }

    #[inline]
    pub(super) fn monotonic_nanos() -> u64 {
        let duration = read_monotonic_time();
        duration
            .as_secs()
            .saturating_mul(1_000_000_000)
            .saturating_add(u64::from(duration.subsec_nanos()))
    }

    pub(super) fn record_ext4_rs_runtime_lock_wait(&self, wait_ns: u64) {
        self.runtime_lock_stats.record_wait(wait_ns);
    }

    pub(super) fn record_ext4_rs_runtime_lock_hold(&self, hold_ns: u64) {
        self.runtime_lock_stats.record_hold(hold_ns);
        let acquire_count = self
            .runtime_lock_stats
            .acquire_count
            .load(Ordering::Relaxed);
        if self.phase2_profile_enabled
            && acquire_count % Ext4RsRuntimeLockStats::LOG_INTERVAL_ACQUIRES == 0
        {
            self.maybe_log_phase2_debug_stats(acquire_count);
        }
    }

    /// Force-emits one complete snapshot of all four profiling layers — FS
    /// direct read/write stages, JBD2 / runtime-lock, and block/virtio bio
    /// latency — regardless of the interval sampling gates. Called from
    /// `sync()` so a benchmark run ends with a full cumulative summary instead
    /// of relying on periodic interval logs. No-op unless
    /// `ext4fs.phase2_profile=1`.
    fn dump_perf_summary(&self) {
        if !self.phase2_profile_enabled {
            return;
        }
        let acquires = self.runtime_lock_stats.acquire_count.load(Ordering::Relaxed);
        if acquires > 0 {
            self.maybe_log_phase2_debug_stats(acquires);
        }
        let writes = self.direct_write_profile.write_calls.load(Ordering::Relaxed);
        self.maybe_log_direct_write_profile(writes, true);
        let reads = self.direct_read_profile.read_calls.load(Ordering::Relaxed);
        self.maybe_log_direct_read_profile(reads, true);
        self.dump_buffered_write_profile();
        self.dump_block_cache_profile();
        self.dump_fsync_profile();
        dump_write_bio_profile();
        dump_read_bio_profile();
    }

    /// Phase 6 buffered-write attribution: emits one snapshot of the
    /// page-cache write path split into overwrite fast path vs append/alloc slow
    /// path (the SQLite `page_cache=1` write profile). No-op unless
    /// `ext4fs.phase2_profile=1`.
    fn dump_buffered_write_profile(&self) {
        if !self.phase2_profile_enabled {
            return;
        }
        let p = &self.buffered_write_profile;
        let calls = p.calls.load(Ordering::Relaxed);
        if calls == 0 {
            return;
        }
        let fast_calls = p.fast_calls.load(Ordering::Relaxed);
        let fast_bytes = p.fast_bytes.load(Ordering::Relaxed);
        let fast_ns = p.fast_ns.load(Ordering::Relaxed);
        let slow_calls = p.slow_calls.load(Ordering::Relaxed);
        let slow_bytes = p.slow_bytes.load(Ordering::Relaxed);
        let slow_blocks = p.slow_blocks.load(Ordering::Relaxed);
        let slow_prepare_ns = p.slow_prepare_ns.load(Ordering::Relaxed);
        let slow_ns = p.slow_ns.load(Ordering::Relaxed);
        let avg_us = |sum: u64, n: u64| if n == 0 { 0 } else { sum / n / 1_000 };

        warn!(
            "[ext4-bufw] calls={} fast_calls={} fast_bytes={} avg_fast_us={} slow_calls={} slow_bytes={} slow_blocks={} avg_slow_prepare_us={} avg_slow_us={} total_slow_ms={} total_slow_prepare_ms={} total_fast_ms={} max_slow_prepare_us={} max_slow_us={}",
            calls,
            fast_calls,
            fast_bytes,
            avg_us(fast_ns, fast_calls),
            slow_calls,
            slow_bytes,
            slow_blocks,
            avg_us(slow_prepare_ns, slow_calls),
            avg_us(slow_ns, slow_calls),
            slow_ns / 1_000_000,
            slow_prepare_ns / 1_000_000,
            fast_ns / 1_000_000,
            p.max_slow_prepare_ns.load(Ordering::Relaxed) / 1_000,
            p.max_slow_ns.load(Ordering::Relaxed) / 1_000,
        );
    }

    /// P1 (Phase 6): one snapshot of the adapter device block cache counters.
    /// No-op unless `ext4fs.phase2_profile=1`.
    fn dump_block_cache_profile(&self) {
        if !self.phase2_profile_enabled {
            return;
        }
        let stats = &self.adapter.block_cache_stats;
        warn!(
            "[ext4-blkcache] hits={} misses={} unaligned_reads={} evictions={} invalidations={} resident={}",
            stats.hits.load(Ordering::Relaxed),
            stats.misses.load(Ordering::Relaxed),
            stats.unaligned_reads.load(Ordering::Relaxed),
            stats.evictions.load(Ordering::Relaxed),
            stats.invalidations.load(Ordering::Relaxed),
            self.adapter.block_cache.lock().blocks.len(),
        );
    }

    /// P4 precursor (Phase 6): per-stage fsync latency breakdown. No-op
    /// unless `ext4fs.phase2_profile=1`.
    fn dump_fsync_profile(&self) {
        if !self.phase2_profile_enabled {
            return;
        }
        let p = &self.fsync_profile;
        let calls = p.calls.load(Ordering::Relaxed);
        if calls == 0 {
            return;
        }
        let writeback = p.writeback_ns.load(Ordering::Relaxed);
        let commit = p.commit_ns.load(Ordering::Relaxed);
        let flush = p.flush_ns.load(Ordering::Relaxed);
        warn!(
            "[ext4-fsync] calls={} writeback_ms={} avg_writeback_us={} commit_ms={} avg_commit_us={} flush_ms={} avg_flush_us={}",
            calls,
            writeback / 1_000_000,
            writeback / calls / 1_000,
            commit / 1_000_000,
            commit / calls / 1_000,
            flush / 1_000_000,
            flush / calls / 1_000,
        );
    }

    fn maybe_log_phase2_debug_stats(&self, runtime_lock_acquires: u64) {
        if !self.phase2_profile_enabled || runtime_lock_acquires == 0 {
            return;
        }
        let total_wait_ns = self
            .runtime_lock_stats
            .total_wait_ns
            .load(Ordering::Relaxed);
        let total_hold_ns = self
            .runtime_lock_stats
            .total_hold_ns
            .load(Ordering::Relaxed);
        let max_wait_ns = self.runtime_lock_stats.max_wait_ns.load(Ordering::Relaxed);
        let max_hold_ns = self.runtime_lock_stats.max_hold_ns.load(Ordering::Relaxed);
        // Phase 6 Task 2: the core-backed driver does not track the ext4_rs `JournalRuntimeDebugStats`
        // counters (they were debug-only accounting). The perf-summary dump below is diagnostic; we
        // feed it zeroed counters so the log line shape is unchanged without re-deriving non-essential
        // instrumentation. (Re-adding them in the driver is a follow-up if profiling needs them.)
        let jbd2_stats = Jbd2DriverDebugStats::default();
        let alloc_guard_stats = self.alloc_guard.debug_stats();
        let journaled_ops = self.journaled_op_profile.op_count.load(Ordering::Relaxed);
        let avg_journaled_stage_us = |stage: &AtomicU64| {
            if journaled_ops == 0 {
                0
            } else {
                stage.load(Ordering::Relaxed) / journaled_ops / 1_000
            }
        };
        let avg_active_x100 = if jbd2_stats.active_handle_samples == 0 {
            0
        } else {
            jbd2_stats.active_handle_sample_sum.saturating_mul(100)
                / jbd2_stats.active_handle_samples
        };

        warn!(
            "[ext4-phase2] runtime_lock_acquires={} avg_wait_us={} max_wait_us={} avg_hold_us={} max_hold_us={} journaled_ops={} mkdir_ops={} rmdir_ops={} write_ops={} avg_start_handle_us={} avg_apply_us={} avg_finish_handle_us={} avg_finish_alloc_us={} avg_finish_io_us={} avg_total_us={} max_apply_ms={} max_finish_handle_ms={} max_total_ms={} jbd2_handles_started={} finished={} max_active={} avg_active_x100={} max_running_handles={} max_running_reserved={} max_running_metadata={} rotations={} commits_prepared={} commits_finished={} checkpoints={} overlay_reads={} overlay_hits={} metadata_writes={} alloc_clear_calls={} alloc_reserve_calls={} alloc_reserved_blocks={} alloc_contains_checks={} alloc_max_operation_blocks={} checkpoint_depth={} bufw_dirty_backlog_kb={}",
            runtime_lock_acquires,
            total_wait_ns / runtime_lock_acquires / 1_000,
            max_wait_ns / 1_000,
            total_hold_ns / runtime_lock_acquires / 1_000,
            max_hold_ns / 1_000,
            journaled_ops,
            self.journaled_op_profile
                .mkdir_count
                .load(Ordering::Relaxed),
            self.journaled_op_profile
                .rmdir_count
                .load(Ordering::Relaxed),
            self.journaled_op_profile
                .write_count
                .load(Ordering::Relaxed),
            avg_journaled_stage_us(&self.journaled_op_profile.start_handle_ns),
            avg_journaled_stage_us(&self.journaled_op_profile.apply_ns),
            avg_journaled_stage_us(&self.journaled_op_profile.finish_handle_ns),
            avg_journaled_stage_us(&self.journaled_op_profile.finish_alloc_ns),
            avg_journaled_stage_us(&self.journaled_op_profile.finish_io_ns),
            avg_journaled_stage_us(&self.journaled_op_profile.total_ns),
            self.journaled_op_profile
                .max_apply_ns
                .load(Ordering::Relaxed)
                / 1_000_000,
            self.journaled_op_profile
                .max_finish_handle_ns
                .load(Ordering::Relaxed)
                / 1_000_000,
            self.journaled_op_profile
                .max_total_ns
                .load(Ordering::Relaxed)
                / 1_000_000,
            jbd2_stats.started_handles,
            jbd2_stats.finished_handles,
            jbd2_stats.max_active_handles,
            avg_active_x100,
            jbd2_stats.max_running_handles,
            jbd2_stats.max_running_reserved_blocks,
            jbd2_stats.max_running_metadata_blocks,
            jbd2_stats.rotated_transactions,
            jbd2_stats.prepared_commits,
            jbd2_stats.finished_commits,
            jbd2_stats.finished_checkpoints,
            jbd2_stats.overlay_reads,
            jbd2_stats.overlay_hits,
            jbd2_stats.metadata_write_records,
            alloc_guard_stats.clear_calls,
            alloc_guard_stats.reserve_calls,
            alloc_guard_stats.reserved_blocks,
            alloc_guard_stats.contains_checks,
            alloc_guard_stats.max_operation_blocks,
            self.checkpoint_depth(),
            {
                let p = &self.buffered_write_profile;
                let dirtied = p
                    .fast_bytes
                    .load(Ordering::Relaxed)
                    .saturating_add(p.slow_bytes.load(Ordering::Relaxed));
                let written = p.writeback_bytes.load(Ordering::Relaxed);
                dirtied.saturating_sub(written) / 1024
            },
        );
    }

    fn maybe_log_direct_read_profile(&self, reads: u64, force: bool) {
        // Gated by `ext4fs.phase2_profile`; off by default so guard regressions
        // see no extra logging. `force` (from the end-of-run perf summary)
        // bypasses the interval so one complete snapshot is always emitted.
        if !self.phase2_profile_enabled || reads == 0 {
            return;
        }
        if !force && reads % DirectReadProfileStats::LOG_INTERVAL_READS != 0 {
            return;
        }

        let total_bytes = self.direct_read_profile.read_bytes.load(Ordering::Relaxed);
        let total_mappings = self
            .direct_read_profile
            .total_mappings
            .load(Ordering::Relaxed);
        let mapped_bytes = self
            .direct_read_profile
            .mapped_bytes
            .load(Ordering::Relaxed);
        let zero_fill_bytes = self
            .direct_read_profile
            .zero_fill_bytes
            .load(Ordering::Relaxed);
        let cache_hits = self.direct_read_profile.cache_hits.load(Ordering::Relaxed);
        let cache_misses = self
            .direct_read_profile
            .cache_misses
            .load(Ordering::Relaxed);
        let max_mappings = self
            .direct_read_profile
            .max_mappings
            .load(Ordering::Relaxed);
        let max_mapped_bytes = self
            .direct_read_profile
            .max_mapped_bytes
            .load(Ordering::Relaxed);
        let plan_ns = self.direct_read_profile.plan_ns.load(Ordering::Relaxed);
        let alloc_ns = self.direct_read_profile.alloc_ns.load(Ordering::Relaxed);
        let submit_ns = self.direct_read_profile.submit_ns.load(Ordering::Relaxed);
        let wait_ns = self.direct_read_profile.wait_ns.load(Ordering::Relaxed);
        let copy_ns = self.direct_read_profile.copy_ns.load(Ordering::Relaxed);
        let total_ns = self.direct_read_profile.total_ns.load(Ordering::Relaxed);
        let atime_ns = self.direct_read_profile.atime_ns.load(Ordering::Relaxed);
        // `other` = read_direct_at wall time minus the individually-measured
        // stages and atime. Captures the in-function overhead not otherwise
        // attributed (lock, evict, note, bookkeeping). Per-read time ABOVE
        // read_direct_at (syscall / VFS / framekernel) is fio_per_read - total.
        let measured_ns = plan_ns
            .saturating_add(alloc_ns)
            .saturating_add(submit_ns)
            .saturating_add(wait_ns)
            .saturating_add(copy_ns)
            .saturating_add(atime_ns);
        let other_ns = total_ns.saturating_sub(measured_ns);

        println!(
            "[ext4-profile] direct-read reads={} bytes={} avg_bytes={} avg_mapped_bytes={} avg_zero_fill_bytes={} max_mapped_bytes={} cache_hit={} cache_miss={} avg_mappings_x100={} max_mappings={} avg_plan_us={} avg_alloc_us={} avg_submit_us={} avg_wait_us={} avg_copy_us={} avg_atime_us={} avg_other_us={} avg_total_us={}",
            reads,
            total_bytes,
            total_bytes / reads,
            mapped_bytes / reads,
            zero_fill_bytes / reads,
            max_mapped_bytes,
            cache_hits,
            cache_misses,
            total_mappings.saturating_mul(100) / reads,
            max_mappings,
            plan_ns / reads / 1_000,
            alloc_ns / reads / 1_000,
            submit_ns / reads / 1_000,
            wait_ns / reads / 1_000,
            copy_ns / reads / 1_000,
            atime_ns / reads / 1_000,
            other_ns / reads / 1_000,
            total_ns / reads / 1_000,
        );
    }

    fn maybe_log_direct_write_profile(&self, writes: u64, force: bool) {
        if !self.phase2_profile_enabled || writes == 0 {
            return;
        }
        // `force` (end-of-run perf summary) bypasses the interval so one
        // complete snapshot is always emitted regardless of write count.
        if !force && writes != 1 && writes % DirectWriteProfileStats::LOG_INTERVAL_WRITES != 0 {
            return;
        }

        let total_bytes = self
            .direct_write_profile
            .write_bytes
            .load(Ordering::Relaxed);
        let total_mappings = self
            .direct_write_profile
            .total_mappings
            .load(Ordering::Relaxed);
        let total_bios = self.direct_write_profile.total_bios.load(Ordering::Relaxed);
        let total_segments = self
            .direct_write_profile
            .total_segments
            .load(Ordering::Relaxed);
        let total_blocks = self
            .direct_write_profile
            .total_blocks
            .load(Ordering::Relaxed);
        let merge_hits = self.direct_write_profile.merge_hits.load(Ordering::Relaxed);
        let user_buffer_pages = self
            .direct_write_profile
            .user_buffer_pages
            .load(Ordering::Relaxed);
        let user_buffer_phys_runs = self
            .direct_write_profile
            .user_buffer_phys_runs
            .load(Ordering::Relaxed);
        let user_buffer_profile_failures = self
            .direct_write_profile
            .user_buffer_profile_failures
            .load(Ordering::Relaxed);
        let cache_hits = self.direct_write_profile.cache_hits.load(Ordering::Relaxed);
        let cache_misses = self
            .direct_write_profile
            .cache_misses
            .load(Ordering::Relaxed);
        let errors = self.direct_write_profile.errors.load(Ordering::Relaxed);
        let plan_ns = self.direct_write_profile.plan_ns.load(Ordering::Relaxed);
        let prepare_ns = self.direct_write_profile.prepare_ns.load(Ordering::Relaxed);
        let data_bio_ns = self
            .direct_write_profile
            .data_bio_ns
            .load(Ordering::Relaxed);
        let bio_alloc_ns = self
            .direct_write_profile
            .bio_alloc_ns
            .load(Ordering::Relaxed);
        let bio_copy_ns = self
            .direct_write_profile
            .bio_copy_ns
            .load(Ordering::Relaxed);
        let bio_submit_ns = self
            .direct_write_profile
            .bio_submit_ns
            .load(Ordering::Relaxed);
        let bio_wait_ns = self
            .direct_write_profile
            .bio_wait_ns
            .load(Ordering::Relaxed);
        let bio_wait_return_after_complete_ns = self
            .direct_write_profile
            .bio_wait_return_after_complete_ns
            .load(Ordering::Relaxed);
        let touch_ns = self.direct_write_profile.touch_ns.load(Ordering::Relaxed);
        let total_ns = self.direct_write_profile.total_ns.load(Ordering::Relaxed);
        let hit_data_bio_ns = self
            .direct_write_profile
            .hit_data_bio_ns
            .load(Ordering::Relaxed);
        let hit_bio_copy_ns = self
            .direct_write_profile
            .hit_bio_copy_ns
            .load(Ordering::Relaxed);
        let hit_bio_wait_ns = self
            .direct_write_profile
            .hit_bio_wait_ns
            .load(Ordering::Relaxed);
        let hit_total_ns = self
            .direct_write_profile
            .hit_total_ns
            .load(Ordering::Relaxed);
        let miss_plan_ns = self
            .direct_write_profile
            .miss_plan_ns
            .load(Ordering::Relaxed);
        let miss_prepare_ns = self
            .direct_write_profile
            .miss_prepare_ns
            .load(Ordering::Relaxed);
        let miss_data_bio_ns = self
            .direct_write_profile
            .miss_data_bio_ns
            .load(Ordering::Relaxed);
        let miss_bio_copy_ns = self
            .direct_write_profile
            .miss_bio_copy_ns
            .load(Ordering::Relaxed);
        let miss_bio_wait_ns = self
            .direct_write_profile
            .miss_bio_wait_ns
            .load(Ordering::Relaxed);
        let miss_touch_ns = self
            .direct_write_profile
            .miss_touch_ns
            .load(Ordering::Relaxed);
        let miss_total_ns = self
            .direct_write_profile
            .miss_total_ns
            .load(Ordering::Relaxed);

        warn!(
            "[ext4-direct-write] writes={} bytes={} avg_bytes={} cache_hits={} cache_misses={} cache_hit_pct_x100={} errors={} avg_mappings_x100={} max_mappings={} avg_bios_x100={} max_bios_per_call={} avg_segments_per_bio_x100={} max_segments_per_bio={} avg_blocks_per_bio={} max_blocks_per_bio={} merge_hits={} avg_merge_hits_x100={} avg_user_pages_x100={} avg_user_phys_runs_x100={} max_user_phys_runs={} avg_user_phys_run_pages_x100={} max_user_phys_run_pages={} user_profile_failures={} avg_plan_us={} avg_prepare_us={} avg_data_bio_us={} avg_bio_alloc_us={} avg_bio_copy_us={} avg_bio_submit_us={} avg_bio_wait_us={} avg_bio_wait_return_after_complete_us={} avg_touch_us={} avg_total_us={} hit_avg_data_bio_us={} hit_avg_bio_copy_us={} hit_avg_bio_wait_us={} hit_avg_total_us={} miss_avg_plan_us={} miss_avg_prepare_us={} miss_avg_data_bio_us={} miss_avg_bio_copy_us={} miss_avg_bio_wait_us={} miss_avg_touch_us={} miss_avg_total_us={} max_prepare_ms={} max_data_bio_ms={} max_bio_wait_return_after_complete_us={} max_touch_ms={} max_total_ms={} max_miss_prepare_ms={} max_miss_data_bio_ms={} max_miss_total_ms={}",
            writes,
            total_bytes,
            total_bytes / writes,
            cache_hits,
            cache_misses,
            cache_hits.saturating_mul(10_000) / writes,
            errors,
            total_mappings.saturating_mul(100) / writes,
            self.direct_write_profile
                .max_mappings
                .load(Ordering::Relaxed),
            total_bios.saturating_mul(100) / writes,
            self.direct_write_profile
                .max_bios_per_call
                .load(Ordering::Relaxed),
            if total_bios == 0 {
                0
            } else {
                total_segments.saturating_mul(100) / total_bios
            },
            self.direct_write_profile
                .max_segments_per_bio
                .load(Ordering::Relaxed),
            if total_bios == 0 {
                0
            } else {
                total_blocks / total_bios
            },
            self.direct_write_profile
                .max_blocks_per_bio
                .load(Ordering::Relaxed),
            merge_hits,
            merge_hits.saturating_mul(100) / writes,
            user_buffer_pages.saturating_mul(100) / writes,
            user_buffer_phys_runs.saturating_mul(100) / writes,
            self.direct_write_profile
                .max_user_buffer_phys_runs
                .load(Ordering::Relaxed),
            if user_buffer_phys_runs == 0 {
                0
            } else {
                user_buffer_pages.saturating_mul(100) / user_buffer_phys_runs
            },
            self.direct_write_profile
                .max_user_buffer_phys_run_pages
                .load(Ordering::Relaxed),
            user_buffer_profile_failures,
            plan_ns / writes / 1_000,
            prepare_ns / writes / 1_000,
            data_bio_ns / writes / 1_000,
            bio_alloc_ns / writes / 1_000,
            bio_copy_ns / writes / 1_000,
            bio_submit_ns / writes / 1_000,
            bio_wait_ns / writes / 1_000,
            bio_wait_return_after_complete_ns / writes / 1_000,
            touch_ns / writes / 1_000,
            total_ns / writes / 1_000,
            if cache_hits == 0 {
                0
            } else {
                hit_data_bio_ns / cache_hits / 1_000
            },
            if cache_hits == 0 {
                0
            } else {
                hit_bio_copy_ns / cache_hits / 1_000
            },
            if cache_hits == 0 {
                0
            } else {
                hit_bio_wait_ns / cache_hits / 1_000
            },
            if cache_hits == 0 {
                0
            } else {
                hit_total_ns / cache_hits / 1_000
            },
            if cache_misses == 0 {
                0
            } else {
                miss_plan_ns / cache_misses / 1_000
            },
            if cache_misses == 0 {
                0
            } else {
                miss_prepare_ns / cache_misses / 1_000
            },
            if cache_misses == 0 {
                0
            } else {
                miss_data_bio_ns / cache_misses / 1_000
            },
            if cache_misses == 0 {
                0
            } else {
                miss_bio_copy_ns / cache_misses / 1_000
            },
            if cache_misses == 0 {
                0
            } else {
                miss_bio_wait_ns / cache_misses / 1_000
            },
            if cache_misses == 0 {
                0
            } else {
                miss_touch_ns / cache_misses / 1_000
            },
            if cache_misses == 0 {
                0
            } else {
                miss_total_ns / cache_misses / 1_000
            },
            self.direct_write_profile
                .max_prepare_ns
                .load(Ordering::Relaxed)
                / 1_000_000,
            self.direct_write_profile
                .max_data_bio_ns
                .load(Ordering::Relaxed)
                / 1_000_000,
            self.direct_write_profile
                .max_bio_wait_return_after_complete_ns
                .load(Ordering::Relaxed)
                / 1_000,
            self.direct_write_profile
                .max_touch_ns
                .load(Ordering::Relaxed)
                / 1_000_000,
            self.direct_write_profile
                .max_total_ns
                .load(Ordering::Relaxed)
                / 1_000_000,
            self.direct_write_profile
                .max_miss_prepare_ns
                .load(Ordering::Relaxed)
                / 1_000_000,
            self.direct_write_profile
                .max_miss_data_bio_ns
                .load(Ordering::Relaxed)
                / 1_000_000,
            self.direct_write_profile
                .max_miss_total_ns
                .load(Ordering::Relaxed)
                / 1_000_000,
        );
    }

    fn maybe_start_direct_read_profile(&self) {
        if self
            .direct_read_profile_started
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            reset_read_bio_profile();
        }
    }

    fn maybe_start_direct_write_profile(&self) {
        if self
            .direct_write_profile_started
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            reset_write_bio_profile();
        }
    }

    fn profile_direct_write_user_buffer(
        user_start: Vaddr,
        len: usize,
        bio_profile: &mut DirectWriteBioCallProfile,
    ) {
        if len == 0 {
            return;
        }
        if user_start < VMAR_LOWEST_ADDR
            || VMAR_CAP_ADDR
                .checked_sub(user_start)
                .is_none_or(|gap| gap < len)
        {
            bio_profile.user_buffer_profile_failures =
                bio_profile.user_buffer_profile_failures.saturating_add(1);
            return;
        }

        let aligned_start = user_start / PAGE_SIZE * PAGE_SIZE;
        let Some(user_end) = user_start.checked_add(len) else {
            bio_profile.user_buffer_profile_failures =
                bio_profile.user_buffer_profile_failures.saturating_add(1);
            return;
        };
        let aligned_end = user_end.saturating_add(PAGE_SIZE - 1) / PAGE_SIZE * PAGE_SIZE;
        let current_task = ostd::task::Task::current().unwrap();
        let thread_local =
            crate::process::posix_thread::AsThreadLocal::as_thread_local(&current_task).unwrap();
        let user_space = crate::context::CurrentUserSpace::new(thread_local);
        let vm_space = user_space.vmar().vm_space();

        let mut current = aligned_start;
        let mut previous_paddr = None;
        let mut current_run_pages = 0u64;
        while current < aligned_end {
            let paddr = {
                let preempt_guard = disable_preempt();
                let cursor_result =
                    vm_space.cursor(&preempt_guard, &(current..current + PAGE_SIZE));
                let Ok(mut cursor) = cursor_result else {
                    bio_profile.user_buffer_profile_failures =
                        bio_profile.user_buffer_profile_failures.saturating_add(1);
                    return;
                };
                match cursor.query() {
                    Ok((_, Some(VmQueriedItem::MappedRam { frame, prop })))
                        if prop.flags.contains(PageFlags::R) =>
                    {
                        frame.paddr()
                    }
                    _ => {
                        bio_profile.user_buffer_profile_failures =
                            bio_profile.user_buffer_profile_failures.saturating_add(1);
                        return;
                    }
                }
            };

            bio_profile.user_buffer_pages = bio_profile.user_buffer_pages.saturating_add(1);
            if previous_paddr.is_some_and(|prev| prev + PAGE_SIZE == paddr) {
                current_run_pages = current_run_pages.saturating_add(1);
            } else {
                bio_profile.user_buffer_phys_runs =
                    bio_profile.user_buffer_phys_runs.saturating_add(1);
                current_run_pages = 1;
            }
            bio_profile.max_user_buffer_phys_run_pages = bio_profile
                .max_user_buffer_phys_run_pages
                .max(current_run_pages);
            previous_paddr = Some(paddr);
            current = current.saturating_add(PAGE_SIZE);
        }
    }

    pub(super) fn set_inode_times(
        &self,
        ino: u32,
        atime: Option<u32>,
        mtime: Option<u32>,
        ctime: Option<u32>,
    ) -> Result<()> {
        // PARITY (ext4_rs `ext4_set_inode_times`): set each provided field, `None` leaves unchanged.
        self.run_inode_metadata_core(ino, |inode| {
            if let Some(v) = atime {
                inode.raw.atime = v;
            }
            if let Some(v) = mtime {
                inode.raw.mtime = v;
            }
            if let Some(v) = ctime {
                inode.raw.ctime = v;
            }
        })
    }

    pub(super) fn set_inode_mode(&self, ino: u32, mode: u16) -> Result<()> {
        // PARITY (ext4_rs `ext4_set_inode_mode`): keep the type bits (0xF000), replace only the
        // permission bits (0x0FFF) — `(current & 0xF000) | (mode & 0x0FFF)`.
        self.run_inode_metadata_core(ino, |inode| {
            let next = (inode.raw.mode & 0xF000) | (mode & 0x0FFF);
            inode.raw.mode = next;
        })?;
        self.touch_ctime(ino)
    }

    pub(super) fn set_inode_uid(&self, ino: u32, uid: u32) -> Result<()> {
        let uid = u16::try_from(uid)
            .map_err(|_| Error::with_message(Errno::EINVAL, "uid exceeds ext4 uid width"))?;
        // PARITY (ext4_rs `ext4_set_inode_uid`): direct set of the low-16 uid field.
        self.run_inode_metadata_core(ino, |inode| {
            inode.raw.uid = uid;
        })?;
        self.touch_ctime(ino)
    }

    pub(super) fn set_inode_gid(&self, ino: u32, gid: u32) -> Result<()> {
        let gid = u16::try_from(gid)
            .map_err(|_| Error::with_message(Errno::EINVAL, "gid exceeds ext4 gid width"))?;
        // PARITY (ext4_rs `ext4_set_inode_gid`): direct set of the low-16 gid field.
        self.run_inode_metadata_core(ino, |inode| {
            inode.raw.gid = gid;
        })?;
        self.touch_ctime(ino)
    }

    pub(super) fn set_inode_rdev(&self, ino: u32, rdev: u64) -> Result<()> {
        let rdev = u32::try_from(rdev)
            .map_err(|_| Error::with_message(Errno::EINVAL, "rdev exceeds ext4 rdev width"))?;
        // PARITY (ext4_rs `ext4_set_inode_rdev` → `set_faddr`): device id is stored in the
        // (deprecated) i_faddr field, not i_block.
        self.run_inode_metadata_core(ino, |inode| {
            inode.raw.faddr = rdev;
        })?;
        self.touch_ctime(ino)
    }

    pub(super) fn mknod_at(
        &self,
        parent: u32,
        name: &str,
        mode: u16,
        rdev: Option<u64>,
    ) -> Result<u32> {
        let ino = self.create_at(parent, name, mode)?;
        if let Some(rdev) = rdev {
            self.with_inode_lock(ino, || self.set_inode_rdev(ino, rdev))?;
        }
        Ok(ino)
    }

    fn mount_flags(&self) -> PerMountFlags {
        PerMountFlags::from_bits_truncate(self.mount_flags_bits.load(Ordering::Relaxed))
    }

    fn should_consider_atime_update(&self, status_flags: StatusFlags) -> Option<PerMountFlags> {
        if status_flags.contains(StatusFlags::O_NOATIME) {
            return None;
        }

        let mount_flags = self.mount_flags();
        if mount_flags.contains(PerMountFlags::RDONLY)
            || mount_flags.contains(PerMountFlags::NOATIME)
        {
            return None;
        }

        Some(mount_flags)
    }

    fn touch_atime(&self, ino: u32, status_flags: StatusFlags) -> Result<()> {
        let Some(mount_flags) = self.should_consider_atime_update(status_flags) else {
            return Ok(());
        };

        let now = Self::now_unix_seconds_u32();
        {
            if self.inode_atime_cache.lock().get(&ino).copied() == Some(now) {
                return Ok(());
            }
        }

        if !mount_flags.contains(PerMountFlags::STRICTATIME) {
            match self.stat(ino) {
                Ok(meta) if meta.atime > meta.mtime && meta.atime > meta.ctime => {
                    // Phase 5: cache the relatime "no atime update needed"
                    // decision for this second so subsequent reads skip the
                    // per-read `stat(ino)` (which re-reads the inode block,
                    // ~31us/read at small bs). A write removes this inode's
                    // atime-cache entry, so a later mtime/ctime bump re-stats.
                    self.inode_atime_cache.lock().insert(ino, now);
                    return Ok(());
                }
                Ok(_) => {}
                Err(err) => {
                    warn!(
                        "ext4: failed to stat inode {} for atime policy: {:?}",
                        ino, err
                    );
                    return Ok(());
                }
            }
        }

        {
            let mut cache = self.inode_atime_cache.lock();
            if cache.get(&ino).copied() == Some(now) {
                return Ok(());
            }
            cache.insert(ino, now);
        }

        if let Err(err) = self.set_inode_times(ino, Some(now), None, None) {
            self.inode_atime_cache.lock().remove(&ino);
            return Err(err);
        }
        Ok(())
    }

    fn touch_mtime_ctime(&self, ino: u32) -> Result<()> {
        let now = Self::now_unix_seconds_u32();
        {
            let mut cache = self.inode_mtime_ctime_cache.lock();
            if cache.get(&ino).copied() == Some(now) {
                return Ok(());
            }
            cache.insert(ino, now);
        }
        if let Err(err) = self.set_inode_times(ino, None, Some(now), Some(now)) {
            self.inode_mtime_ctime_cache.lock().remove(&ino);
            return Err(err);
        }
        Ok(())
    }

    fn touch_ctime(&self, ino: u32) -> Result<()> {
        let now = Self::now_unix_seconds_u32();
        {
            let mut cache = self.inode_ctime_cache.lock();
            if cache.get(&ino).copied() == Some(now) {
                return Ok(());
            }
            cache.insert(ino, now);
        }
        if let Err(err) = self.set_inode_times(ino, None, None, Some(now)) {
            self.inode_ctime_cache.lock().remove(&ino);
            return Err(err);
        }
        Ok(())
    }

    fn touch_birth_times(&self, ino: u32) -> Result<()> {
        let now = Self::now_unix_seconds_u32();
        self.set_inode_times(ino, Some(now), Some(now), Some(now))
    }

    pub(super) fn vm_io_error(err: OstdError) -> Error {
        let _ = err;
        Error::with_message(Errno::EFAULT, "vm I/O failed")
    }

    pub(super) fn write_zeros(writer: &mut VmWriter, len: usize) -> Result<()> {
        debug_assert!(len <= writer.avail());
        let zeroed = writer
            .fill_zeros(len)
            .map_err(|(err, _)| Error::from(err))?;
        debug_assert_eq!(zeroed, len);
        Ok(())
    }

    fn slice_mappings_for_range(
        offset: usize,
        len: usize,
        mappings: &[SimpleBlockRange],
    ) -> Result<Vec<SimpleBlockRange>> {
        if len == 0 {
            return Ok(Vec::new());
        }

        let block_size = EXT4_BLOCK_SIZE;
        let end = offset
            .checked_add(len)
            .ok_or_else(|| Error::with_message(Errno::EFBIG, "direct I/O range overflow"))?;
        let start_lblock = offset / block_size;
        let end_lblock = end / block_size;
        let mut sliced = Vec::new();
        let mut left = 0usize;
        let mut right = mappings.len();
        while left < right {
            let mid = left + (right - left) / 2;
            let mapping = &mappings[mid];
            let mapping_end = (mapping.lblock as usize)
                .checked_add(mapping.len as usize)
                .ok_or_else(|| Error::with_message(Errno::EFBIG, "mapped range overflow"))?;
            if mapping_end <= start_lblock {
                left = mid + 1;
            } else {
                right = mid;
            }
        }

        for mapping in mappings.iter().skip(left) {
            let mapping_start = mapping.lblock as usize;
            if mapping_start >= end_lblock {
                break;
            }
            let mapping_end = mapping_start
                .checked_add(mapping.len as usize)
                .ok_or_else(|| Error::with_message(Errno::EFBIG, "mapped range overflow"))?;
            let overlap_start = mapping_start.max(start_lblock);
            let overlap_end = mapping_end.min(end_lblock);
            if overlap_start >= overlap_end {
                continue;
            }

            sliced.push(SimpleBlockRange {
                lblock: overlap_start as u32,
                pblock: mapping.pblock + (overlap_start - mapping_start) as u64,
                len: (overlap_end - overlap_start) as u32,
            });
        }

        Ok(sliced)
    }

    fn plan_direct_read_cached(
        &self,
        ino: u32,
        offset: usize,
        requested_len: usize,
        cache_allowed: bool,
    ) -> Result<(usize, Vec<SimpleBlockRange>)> {
        const DIRECT_READ_PLAN_BASE_WINDOW_BYTES: usize = 128 * 1024 * 1024;
        const DIRECT_READ_PLAN_MAX_WINDOW_BYTES: usize = 512 * 1024 * 1024;

        if requested_len == 0 {
            return Ok((0, Vec::new()));
        }

        let requested_direct_len = requested_len / EXT4_BLOCK_SIZE * EXT4_BLOCK_SIZE;
        if requested_direct_len == 0 {
            return Ok((0, Vec::new()));
        }

        if !cache_allowed {
            self.direct_read_profile.record_cache_miss();
            return self.run_io_file_read_only(|ctx| {
                Self::core_plan_direct_read(ctx, ino, offset, requested_len)
            });
        }

        let mut next_plan_window = requested_len.max(DIRECT_READ_PLAN_BASE_WINDOW_BYTES);
        next_plan_window = next_plan_window.min(DIRECT_READ_PLAN_MAX_WINDOW_BYTES);

        {
            let cache = self.inode_direct_read_cache.lock();
            if let Some(entry) = cache.get(&ino) {
                let cache_end = entry.file_offset.saturating_add(entry.len);
                let request_end = offset.saturating_add(requested_direct_len);
                if offset >= entry.file_offset && request_end <= cache_end {
                    self.direct_read_profile.record_cache_hit();
                    let mappings = Self::slice_mappings_for_range(
                        offset,
                        requested_direct_len,
                        &entry.mappings,
                    )?;
                    return Ok((requested_direct_len, mappings));
                }

                let sequential_continuation =
                    offset >= entry.file_offset && offset <= cache_end && request_end > cache_end;
                let restart_after_eof =
                    offset == 0 && entry.file_offset > 0 && entry.len < entry.plan_window;

                if sequential_continuation {
                    next_plan_window = entry
                        .plan_window
                        .saturating_mul(2)
                        .min(DIRECT_READ_PLAN_MAX_WINDOW_BYTES)
                        .max(next_plan_window);
                } else if restart_after_eof {
                    next_plan_window = entry.plan_window.max(next_plan_window);
                }
            }
        }

        self.direct_read_profile.record_cache_miss();
        let (cached_len, cached_mappings) = self.run_io_file_read_only(|ctx| {
            Self::core_plan_direct_read(ctx, ino, offset, next_plan_window)
        })?;
        if cached_len == 0 {
            return Ok((0, Vec::new()));
        }

        let direct_len = cached_len.min(requested_direct_len);
        let mappings = Self::slice_mappings_for_range(offset, direct_len, &cached_mappings)?;
        self.inode_direct_read_cache.lock().insert(
            ino,
            DirectReadCache {
                file_offset: offset,
                len: cached_len,
                plan_window: next_plan_window,
                last_atime_sec: 0,
                last_read_end: 0,
                pending: None,
                mappings: cached_mappings,
            },
        );
        Ok((direct_len, mappings))
    }

    /// Phase 5: resolve the O_DIRECT read mapping for `[offset, requested_len)`
    /// through the metadata-only extent mapping cache.
    ///
    /// On a cache hit the cached extent mapping is sliced to the requested range
    /// and returned without touching the extent tree. On a miss the mapping is
    /// resolved once for a large window (mapping metadata only, no data and no
    /// speculative bio), cached, and sliced. Cache entries are dropped by
    /// `invalidate_direct_read_cache` on every block-changing operation (write /
    /// truncate / fallocate / unlink / rename), and reads and writes on the same
    /// inode are serialized by the inode correctness lock, so a cached mapping
    /// can never outlive the extents it describes.
    ///
    /// Step 3b: the window is wide enough to cover a whole typical file in one
    /// entry, so *random* reads also hit (the cached base offset monotonically
    /// drops toward 0 across misses until the whole file is covered) — the same
    /// effect Linux's extent_status cache provides. This caches only the
    /// resolved logical->physical mapping, so it eliminates both the extent-tree
    /// disk reads *and* the tree walk per read (a raw metadata-block cache would
    /// only remove the disk reads). Pathologically fragmented files are bounded
    /// by `MAX_CACHED_EXTENTS`.
    fn plan_direct_read_extent_map_cached(
        &self,
        ino: u32,
        offset: usize,
        requested_len: usize,
    ) -> Result<(usize, Vec<SimpleBlockRange>)> {
        // Wide mapping-resolution window so one cache entry covers a whole
        // typical file and random reads also hit. This only controls how much
        // *mapping metadata* is resolved per walk; no file data is read here.
        const EXTENT_MAP_PLAN_WINDOW_BYTES: usize = 1024 * 1024 * 1024;
        // Memory bound: skip caching a single inode's mapping past this many
        // extents (e.g. a maximally fragmented multi-GiB file). ~12 bytes each,
        // so the cap is ~192 KiB per inode.
        const MAX_CACHED_EXTENTS: usize = 16384;

        if requested_len == 0 {
            return Ok((0, Vec::new()));
        }
        let requested_direct_len = requested_len / EXT4_BLOCK_SIZE * EXT4_BLOCK_SIZE;
        if requested_direct_len == 0 {
            return Ok((0, Vec::new()));
        }

        {
            let cache = self.inode_extent_map_cache.lock();
            if let Some(entry) = cache.get(&ino) {
                let cache_end = entry.file_offset.saturating_add(entry.len);
                let request_end = offset.saturating_add(requested_direct_len);
                if offset >= entry.file_offset && request_end <= cache_end {
                    self.direct_read_profile.record_cache_hit();
                    let mappings = Self::slice_mappings_for_range(
                        offset,
                        requested_direct_len,
                        &entry.mappings,
                    )?;
                    return Ok((requested_direct_len, mappings));
                }
            }
        }

        self.direct_read_profile.record_cache_miss();
        let plan_window = requested_len.max(EXTENT_MAP_PLAN_WINDOW_BYTES);
        let (resolved_len, resolved_mappings) = self
            .run_io_file_read_only(|ctx| Self::core_plan_direct_read(ctx, ino, offset, plan_window))?;
        if resolved_len == 0 {
            return Ok((0, Vec::new()));
        }

        let direct_len = resolved_len.min(requested_direct_len);
        let mappings = Self::slice_mappings_for_range(offset, direct_len, &resolved_mappings)?;
        // Bound per-inode memory: only cache when the resolved mapping is small
        // enough. Fragmented files past the cap fall back to a per-read walk
        // (still correct, just unaccelerated).
        if resolved_mappings.len() <= MAX_CACHED_EXTENTS {
            self.inode_extent_map_cache.lock().insert(
                ino,
                ExtentMapCacheEntry {
                    file_offset: offset,
                    len: resolved_len,
                    mappings: resolved_mappings,
                },
            );
        }
        Ok((direct_len, mappings))
    }

    fn mappings_fully_cover_range(
        offset: usize,
        len: usize,
        mappings: &[SimpleBlockRange],
    ) -> Result<bool> {
        if len == 0 {
            return Ok(true);
        }

        let end = offset
            .checked_add(len)
            .ok_or_else(|| Error::with_message(Errno::EFBIG, "direct I/O range overflow"))?;
        let mut current_lblock = offset / EXT4_BLOCK_SIZE;
        let end_lblock = end / EXT4_BLOCK_SIZE;

        for mapping in mappings {
            let mapping_start = mapping.lblock as usize;
            if mapping_start != current_lblock {
                return Ok(false);
            }
            current_lblock = mapping_start
                .checked_add(mapping.len as usize)
                .ok_or_else(|| Error::with_message(Errno::EFBIG, "mapped range overflow"))?;
            if current_lblock > end_lblock {
                return Ok(false);
            }
        }

        Ok(current_lblock == end_lblock)
    }

    fn plan_direct_write_overwrite_cached(
        &self,
        ino: u32,
        offset: usize,
        len: usize,
    ) -> Result<Option<Vec<SimpleBlockRange>>> {
        if self.page_cache_enabled {
            return Ok(None);
        }

        let (direct_len, mappings) = self.plan_direct_read_cached(ino, offset, len, true)?;
        if direct_len != len {
            return Ok(None);
        }
        if !Self::mappings_fully_cover_range(offset, len, &mappings)? {
            return Ok(None);
        }
        Ok(Some(mappings))
    }

    fn submit_direct_write_mappings(
        &self,
        mappings: &[SimpleBlockRange],
        reader: &mut VmReader,
        profile_enabled: bool,
        bio_alloc_ns: &mut u64,
        bio_copy_ns: &mut u64,
        bio_submit_ns: &mut u64,
        bio_wait_ns: &mut u64,
        bio_profile: &mut DirectWriteBioCallProfile,
    ) -> Result<()> {
        let mut bio_waiter = BioWaiter::new();
        let merge_start = if profile_enabled {
            bio_profile.mappings = bio_profile
                .mappings
                .saturating_add(u64::try_from(mappings.len()).unwrap_or(u64::MAX));
            bio_request_merge_count()
        } else {
            0
        };
        for mapping in mappings {
            let alloc_start_ns = if profile_enabled {
                Self::monotonic_nanos()
            } else {
                0
            };
            let bio_segment = BioSegment::alloc(mapping.len as usize, BioDirection::ToDevice);
            if profile_enabled {
                bio_profile.bios = bio_profile.bios.saturating_add(1);
                bio_profile.segments = bio_profile.segments.saturating_add(1);
                bio_profile.blocks = bio_profile.blocks.saturating_add(u64::from(mapping.len));
                bio_profile.max_segments_per_bio = bio_profile.max_segments_per_bio.max(1);
                bio_profile.max_blocks_per_bio =
                    bio_profile.max_blocks_per_bio.max(u64::from(mapping.len));
            }
            if profile_enabled {
                *bio_alloc_ns = bio_alloc_ns
                    .saturating_add(Self::monotonic_nanos().saturating_sub(alloc_start_ns));
            }

            let copy_start_ns = if profile_enabled {
                Self::monotonic_nanos()
            } else {
                0
            };
            bio_segment
                .writer()
                .map_err(Self::vm_io_error)?
                .write_fallible(reader)
                .map_err(|(e, _)| Error::from(e))?;
            if profile_enabled {
                *bio_copy_ns = bio_copy_ns
                    .saturating_add(Self::monotonic_nanos().saturating_sub(copy_start_ns));
            }

            let submit_start_ns = if profile_enabled {
                Self::monotonic_nanos()
            } else {
                0
            };
            let waiter = self
                .block_device
                .write_blocks_async(Bid::new(mapping.pblock), bio_segment)?;
            if profile_enabled {
                *bio_submit_ns = bio_submit_ns
                    .saturating_add(Self::monotonic_nanos().saturating_sub(submit_start_ns));
            }
            bio_waiter.concat(waiter);
        }

        let wait_start_ns = if profile_enabled {
            Self::monotonic_nanos()
        } else {
            0
        };
        let status = bio_waiter.wait();
        if profile_enabled {
            let wait_return_ns = Self::monotonic_nanos();
            *bio_wait_ns = bio_wait_ns.saturating_add(wait_return_ns.saturating_sub(wait_start_ns));
            let max_complete_ns = bio_waiter.max_complete_ns();
            if max_complete_ns != 0 {
                bio_profile.wait_return_after_complete_ns = bio_profile
                    .wait_return_after_complete_ns
                    .saturating_add(wait_return_ns.saturating_sub(max_complete_ns));
            }
            bio_profile.merge_hits = bio_profile
                .merge_hits
                .saturating_add(bio_request_merge_count().saturating_sub(merge_start));
        }
        if Some(BioStatus::Complete) != status {
            return_errno!(Errno::EIO);
        }
        // P1 (Phase 6): these bios bypassed the adapter, so its device block
        // cache may hold stale copies of the overwritten blocks (e.g. from an
        // earlier buffered RMW read). Drop them.
        for mapping in mappings {
            self.adapter.invalidate_block_range(
                (mapping.pblock as usize).saturating_mul(EXT4_BLOCK_SIZE),
                (mapping.len as usize).saturating_mul(EXT4_BLOCK_SIZE),
            );
        }
        Ok(())
    }

    fn invalidate_direct_read_cache(&self, ino: u32) {
        self.inode_direct_read_cache.lock().remove(&ino);
        // Phase 5: the metadata-only extent mapping cache must be dropped on the
        // exact same block-changing events; sharing this entry point inherits
        // every existing invalidation call site (write/truncate/fallocate/
        // unlink/rename/shutdown).
        self.inode_extent_map_cache.lock().remove(&ino);
    }

    fn clear_pending_direct_read(&self, ino: u32) {
        if let Some(entry) = self.inode_direct_read_cache.lock().get_mut(&ino) {
            entry.pending = None;
        }
    }

    fn take_matching_pending_direct_read(
        &self,
        ino: u32,
        offset: usize,
        max_len: usize,
    ) -> Option<PendingDirectRead> {
        let mut cache = self.inode_direct_read_cache.lock();
        let entry = cache.get_mut(&ino)?;
        let pending = entry.pending.take()?;
        if pending.offset == offset && pending.len <= max_len {
            Some(pending)
        } else {
            None
        }
    }

    fn note_completed_direct_read(&self, ino: u32, offset: usize, direct_len: usize) {
        if let Some(entry) = self.inode_direct_read_cache.lock().get_mut(&ino) {
            entry.last_read_end = offset.saturating_add(direct_len);
        }
    }

    fn submit_direct_read_request_with_hint(
        &self,
        mappings: &[SimpleBlockRange],
        prefer_fast_submit: bool,
    ) -> Result<(BioWaiter, u64, u64)> {
        let mut bio_waiter = BioWaiter::new();
        let mut alloc_ns = 0u64;
        let mut submit_ns = 0u64;

        for mapping in mappings {
            let alloc_start = Self::monotonic_nanos();
            let bio_segment = BioSegment::alloc(mapping.len as usize, BioDirection::FromDevice);
            alloc_ns = alloc_ns.saturating_add(Self::monotonic_nanos().saturating_sub(alloc_start));
            let submit_start = Self::monotonic_nanos();
            let waiter = if prefer_fast_submit {
                self.block_device
                    .read_blocks_async_prefetch(Bid::new(mapping.pblock), bio_segment)?
            } else {
                self.block_device
                    .read_blocks_async(Bid::new(mapping.pblock), bio_segment)?
            };
            submit_ns =
                submit_ns.saturating_add(Self::monotonic_nanos().saturating_sub(submit_start));
            bio_waiter.concat(waiter);
        }

        Ok((bio_waiter, alloc_ns, submit_ns))
    }

    fn wait_direct_read(&self, bio_waiter: &BioWaiter) -> Result<u64> {
        let wait_start = Self::monotonic_nanos();
        if Some(BioStatus::Complete) != bio_waiter.wait() {
            return_errno!(Errno::EIO);
        }
        Ok(Self::monotonic_nanos().saturating_sub(wait_start))
    }

    fn copy_completed_direct_read(
        &self,
        offset: usize,
        direct_len: usize,
        mappings: &[SimpleBlockRange],
        bio_waiter: &BioWaiter,
        writer: &mut VmWriter,
    ) -> Result<(usize, u64)> {
        let mut current_offset = offset;
        let request_end = offset
            .checked_add(direct_len)
            .ok_or_else(|| Error::with_message(Errno::EFBIG, "direct I/O range overflow"))?;
        let copy_start = Self::monotonic_nanos();
        let mut mapped_bytes = 0usize;

        for (mapping, bio) in mappings.iter().zip(bio_waiter.reqs()) {
            let file_offset = (mapping.lblock as usize)
                .checked_mul(EXT4_BLOCK_SIZE)
                .ok_or_else(|| Error::with_message(Errno::EFBIG, "direct I/O offset overflow"))?;
            if current_offset < file_offset {
                Self::write_zeros(writer, file_offset - current_offset)?;
            }

            let segment = bio
                .segments()
                .first()
                .ok_or_else(|| Error::with_message(Errno::EIO, "missing direct read segment"))?;
            segment
                .reader()
                .map_err(Self::vm_io_error)?
                .read_fallible(writer)
                .map_err(|(e, _)| Error::from(e))?;
            mapped_bytes = mapped_bytes.saturating_add(mapping.len as usize * EXT4_BLOCK_SIZE);
            current_offset = file_offset + mapping.len as usize * EXT4_BLOCK_SIZE;
        }

        if current_offset < request_end {
            Self::write_zeros(writer, request_end - current_offset)?;
        }

        let copy_ns = Self::monotonic_nanos().saturating_sub(copy_start);
        Ok((mapped_bytes, copy_ns))
    }

    fn maybe_prepare_speculative_direct_read(
        &self,
        ino: u32,
        offset: usize,
        direct_len: usize,
    ) -> Result<(Option<PreparedDirectRead>, u64)> {
        const SPECULATIVE_DIRECT_READ_MIN_BYTES: usize = 512 * 1024;

        if self.page_cache_enabled {
            return Ok((None, 0));
        }
        if direct_len < SPECULATIVE_DIRECT_READ_MIN_BYTES {
            return Ok((None, 0));
        }
        if !self.direct_read_cache_enabled {
            return Ok((None, 0));
        }

        let next_offset = match offset.checked_add(direct_len) {
            Some(next_offset) => next_offset,
            None => return Ok((None, 0)),
        };

        {
            let cache = self.inode_direct_read_cache.lock();
            let Some(entry) = cache.get(&ino) else {
                return Ok((None, 0));
            };
            if entry.pending.is_some() {
                return Ok((None, 0));
            }
            if offset != 0 && entry.last_read_end != offset {
                return Ok((None, 0));
            }
        }

        let plan_start = Self::monotonic_nanos();
        let (next_len, next_mappings) =
            self.plan_direct_read_cached(ino, next_offset, direct_len, true)?;
        let plan_ns = Self::monotonic_nanos().saturating_sub(plan_start);
        if next_len < SPECULATIVE_DIRECT_READ_MIN_BYTES {
            return Ok((None, plan_ns));
        }
        if !Self::mappings_fully_cover_range(next_offset, next_len, &next_mappings)? {
            return Ok((None, plan_ns));
        }

        Ok((
            Some(PreparedDirectRead {
                offset: next_offset,
                len: next_len,
                mappings: next_mappings,
            }),
            plan_ns,
        ))
    }

    fn submit_prepared_speculative_direct_read(
        &self,
        ino: u32,
        prepared: Option<PreparedDirectRead>,
    ) -> Result<(u64, u64)> {
        let Some(prepared) = prepared else {
            return Ok((0, 0));
        };

        let (waiter, alloc_ns, submit_ns) =
            self.submit_direct_read_request_with_hint(&prepared.mappings, true)?;
        if let Some(entry) = self.inode_direct_read_cache.lock().get_mut(&ino) {
            entry.pending = Some(PendingDirectRead {
                offset: prepared.offset,
                len: prepared.len,
                mappings: prepared.mappings,
                waiter,
            });
        }

        Ok((alloc_ns, submit_ns))
    }

    fn touch_atime_after_direct_read(&self, ino: u32, status_flags: StatusFlags) -> Result<()> {
        let Some(_) = self.should_consider_atime_update(status_flags) else {
            return Ok(());
        };

        let now = Self::now_unix_seconds_u32();
        {
            let cache = self.inode_direct_read_cache.lock();
            if cache.get(&ino).map(|entry| entry.last_atime_sec) == Some(now) {
                return Ok(());
            }
        }

        self.touch_atime(ino, status_flags)?;

        if let Some(entry) = self.inode_direct_read_cache.lock().get_mut(&ino) {
            entry.last_atime_sec = now;
        }
        Ok(())
    }

    /// Phase 6 Task 3: setattr cutover. Drive an inode-field mutation through **core** instead of
    /// ext4_rs. `mutate` receives the loaded core [`Inode`]; the caller sets the fields it owns
    /// (mode/uid/gid/rdev/times) and this helper writes the inode back + invalidates the meta cache.
    ///
    /// PARITY with the retired `run_inode_metadata_update_with_op`: when a journal driver is present
    /// (`jbd2_runtime` installed at mount), the write goes through the journaled chokepoint
    /// ([`run_journaled_core`] — same lock order, JBD2 handle, `InodeMetadata` op, cache
    /// invalidation as the old `run_journaled_ext4` path). When journal-less, the inode is written
    /// straight to the device via [`CoreCommitMetadataWriter`] (mirroring ext4_rs's no-journal
    /// `write_back_inode` → `write_offset`), then the meta cache is invalidated with the same
    /// generation-bump + clear protocol.
    fn run_inode_metadata_core(
        &self,
        ino: u32,
        mutate: impl FnOnce(&mut super::core::inode::Inode),
    ) -> Result<()> {
        let journal_enabled = self.jbd2_runtime.read().as_ref().is_some();
        if journal_enabled {
            // Journaled: load → mutate → write_back inside the journaled-core chokepoint, which
            // records the inode block image into the active JBD2 transaction (the allocator is
            // unused by setattr but threaded for signature compatibility). The `InodeMetadata { ino }`
            // op drops just this inode from the meta cache (parity with the old path).
            let op = JournaledOp::InodeMetadata { ino };
            self.run_journaled_core(Some(op), ino, |ctx, _alloc, inode| {
                mutate(inode);
                super::core::inode::write_back_inode(ctx.writer, ctx.reader, ctx.sb, inode)
            })
        } else {
            // No-journal: write the inode straight to the device (CoreCommitMetadataWriter does the
            // block-number → device write, bypassing the overlay/defer logic, exactly like ext4_rs's
            // no-journal `write_back_inode`). Same IO-epoch + runtime-lock lifecycle as the old path.
            let io_epoch = self.prepare_ext4_io();
            let runtime_wait_start_ns = Self::monotonic_nanos();
            let runtime_guard = EXT4_RS_RUNTIME_LOCK.lock();
            self.record_ext4_rs_runtime_lock_wait(
                Self::monotonic_nanos().saturating_sub(runtime_wait_start_ns),
            );
            let runtime_hold_start_ns = Self::monotonic_nanos();
            let block_size = self.core_sb.block_size();
            let reader = super::core_adapter::CoreDeviceReader::new(self.journal_io.clone());
            let writer =
                super::core_adapter::CoreCommitMetadataWriter::new(self.adapter.clone(), block_size);
            let result = (|| -> Result<()> {
                let mut inode = super::core::inode::load_inode(&reader, &self.core_sb, ino)?;
                mutate(&mut inode);
                super::core::inode::write_back_inode(&writer, &reader, &self.core_sb, &mut inode)
            })();
            drop(runtime_guard);
            self.record_ext4_rs_runtime_lock_hold(
                Self::monotonic_nanos().saturating_sub(runtime_hold_start_ns),
            );
            let io_result = self.finish_ext4_io(io_epoch);
            // No-journal setattr bypasses run_journaled_*, so invalidate the inode meta cache here
            // too (same generation-bump + clear protocol as the retired path).
            self.meta_cache_generation.fetch_add(1, Ordering::Release);
            self.inode_meta_cache.lock().clear();
            match (result, io_result) {
                (Ok(()), Ok(())) => Ok(()),
                (Err(err), _) => Err(err),
                (Ok(()), Err(err)) => Err(err),
            }
        }
    }


    // =====================================================================================
    // Phase 6 Task 1: engine-agnostic read orchestration over `core/`.
    //
    // The READ path is cut from third-party `ext4_rs` to the in-tree safe `core/`. `core/` reads
    // are stateless free functions over an injected `ReadCtx { reader, sb, block_size }`. The
    // reader is `CoreDeviceReader`, wired to the SAME `JournalIoBridge` overlay the live read path
    // used, so core reads see read-your-writes against uncommitted journaled metadata while the
    // write path is still ext4_rs. The superblock is the mount-time snapshot `self.core_sb`
    // (immutable geometry; see the field doc).
    //
    // These `run_io_*_read_only*` wrappers replicate the IO-epoch + (where applicable) runtime-lock
    // lifecycle of their `run_ext4_*_read_only*` siblings one-for-one, so the integration's locking
    // structure is unchanged; only the engine driven inside the closure differs. The closure takes
    // a freshly built `ReadCtx` instead of `&Ext4`.
    // =====================================================================================

    /// Build a core read seam wired to the overlay bridge (read-your-writes).
    pub(super) fn core_reader(&self) -> super::core_adapter::CoreDeviceReader {
        super::core_adapter::CoreDeviceReader::new(self.journal_io.clone())
    }

    /// Map a logical block range to physical ranges via core, returning the integration DTO.
    ///
    /// `core::SimpleBlockRange` is a field-for-field copy of ext4_rs's `SimpleBlockRange`
    /// (`{ lblock, pblock, len }`), so the mapping-vector contract is preserved verbatim. The
    /// caller-supplied `ctx` is the core read context built by a `run_io_*` wrapper.
    pub(super) fn core_map_blocks(
        ctx: &super::core::file::ReadCtx,
        ino: u32,
        lblock_start: u32,
        lblock_count: u32,
    ) -> Result<Vec<SimpleBlockRange>> {
        let inode = super::core::inode::load_inode(ctx.reader, ctx.sb, ino)?;
        let ranges = super::core::file::map_blocks(ctx, &inode, lblock_start, lblock_count)?;
        Ok(ranges
            .into_iter()
            .map(|r| SimpleBlockRange {
                lblock: r.lblock,
                pblock: r.pblock,
                len: r.len,
            })
            .collect())
    }

    /// Build a direct-read plan via core, returning `(direct_len, mappings)` as integration DTOs.
    ///
    /// Byte-parity with ext4_rs `ext4_plan_direct_read`: `direct_len` is the byte count floored to
    /// block alignment; `mappings` are the resolved logical->physical ranges. `core::SimpleBlockRange`
    /// is a field-for-field copy of ext4_rs's, so the vector contract is preserved.
    fn core_plan_direct_read(
        ctx: &super::core::file::ReadCtx,
        ino: u32,
        offset: usize,
        len: usize,
    ) -> Result<(usize, Vec<SimpleBlockRange>)> {
        let inode = super::core::inode::load_inode(ctx.reader, ctx.sb, ino)?;
        let (direct_len, ranges) = super::core::file::plan_direct_read(ctx, &inode, offset, len)?;
        let mappings = ranges
            .into_iter()
            .map(|r| SimpleBlockRange {
                lblock: r.lblock,
                pblock: r.pblock,
                len: r.len,
            })
            .collect();
        Ok((direct_len, mappings))
    }

    /// Build the integration `SimpleInodeMeta` DTO from a core `Inode`, byte-identically to ext4_rs
    /// `ext4_stat`.
    ///
    /// Field-by-field mapping (each verified against ext4_rs `Ext4Inode` accessors):
    /// `ino`=`inode.num`; `mode`=`raw.mode()`; `file_type`=`raw.file_type()` (`mode & 0xF000`, same
    /// as ext4_rs `file_type().bits()`); `uid`/`gid`/`atime`/`mtime`/`ctime`/`faddr` are the raw
    /// fields; `nlink`=`raw.links_count()`; `size`=`raw.size()` (`size|size_hi<<32`);
    /// `blocks`=`raw.blocks()` (`blocks|l_i_blocks_high<<32`, equal to ext4_rs `blocks_count()`);
    /// `rdev`=`raw.faddr()`; `flags`=`raw.flags()`.
    fn core_inode_meta(inode: &super::core::inode::Inode) -> SimpleInodeMeta {
        let raw = &inode.raw;
        SimpleInodeMeta {
            ino: inode.num,
            mode: raw.mode(),
            file_type: raw.file_type(),
            uid: raw.uid,
            gid: raw.gid,
            nlink: raw.links_count(),
            size: raw.size(),
            blocks: raw.blocks(),
            atime: raw.atime,
            mtime: raw.mtime,
            ctime: raw.ctime,
            rdev: raw.faddr,
            flags: raw.flags(),
        }
    }

    /// Map core `file::SimpleBlockRange`s to the integration (ext4_rs) `SimpleBlockRange` DTO.
    /// Field-for-field copy (lblock / pblock / len) — the core type is a byte-parity reimplementation
    /// of ext4_rs's, so the vector contract (ascending by lblock, `len` in blocks) is preserved.
    fn core_to_integration_ranges(
        ranges: &[super::core::file::SimpleBlockRange],
    ) -> Vec<SimpleBlockRange> {
        ranges
            .iter()
            .map(|r| SimpleBlockRange {
                lblock: r.lblock,
                pblock: r.pblock,
                len: r.len,
            })
            .collect()
    }

    /// Set second-granularity atime/mtime/ctime on a loaded core inode and write it back.
    ///
    /// PARITY: ext4_rs `ext4_set_inode_times` (set the provided fields, `None` = leave unchanged,
    /// then `write_back_inode`). The write apply closures call this after the data/alloc write, just
    /// as the ext4_rs closures called `ext4_set_inode_times(ino, None, Some(now), Some(now))`.
    fn core_set_inode_times(
        ctx: &super::core::extents::WriteCtx,
        inode: &mut super::core::inode::Inode,
        atime: Option<u32>,
        mtime: Option<u32>,
        ctime: Option<u32>,
    ) -> Result<()> {
        if let Some(v) = atime {
            inode.raw.atime = v;
        }
        if let Some(v) = mtime {
            inode.raw.mtime = v;
        }
        if let Some(v) = ctime {
            inode.raw.ctime = v;
        }
        super::core::inode::write_back_inode(ctx.writer, ctx.reader, ctx.sb, inode)
    }

    pub(super) fn stat(&self, ino: u32) -> Result<SimpleInodeMeta> {
        // Fast path: serve from the in-memory metadata cache.
        let gen_before = self.meta_cache_generation.load(Ordering::Acquire);
        if let Some(meta) = self.inode_meta_cache.lock().get(&ino).copied() {
            return Ok(meta);
        }

        // Miss: read the inode from the device once via core and build the same DTO ext4_rs
        // `ext4_stat` returned. `load_inode` validates the inode number (ext4_rs `get_inode_ref`
        // does not), so a structurally-invalid inode now surfaces an error instead of garbage
        // metadata; valid inodes (all the VFS layer ever passes) are byte-identical.
        let meta = self.run_io_read_only(|ctx| {
            let inode = super::core::inode::load_inode(ctx.reader, ctx.sb, ino)?;
            Ok(Self::core_inode_meta(&inode))
        })?;

        // Only cache if no journaled mutation raced our read (generation
        // unchanged), so we never insert a value read across a mutation.
        if self.meta_cache_generation.load(Ordering::Acquire) == gen_before {
            self.inode_meta_cache.lock().insert(ino, meta);
        }
        Ok(meta)
    }

    fn lookup_cache(&self, parent: u32, name: &str) -> DirLookupCacheResult {
        let caches = self.dir_entry_cache.lock();
        let Some(cache) = caches.get(&parent) else {
            return DirLookupCacheResult::Unknown;
        };
        if let Some(entry) = cache.entries.get(name) {
            return DirLookupCacheResult::Hit(entry.ino, entry.offset, entry.de_type);
        }
        if cache.loaded {
            return DirLookupCacheResult::Miss;
        }
        DirLookupCacheResult::Unknown
    }

    fn load_dir_cache_if_needed_locked(&self, parent: u32) -> Result<()> {
        {
            let caches = self.dir_entry_cache.lock();
            if let Some(cache) = caches.get(&parent) {
                if cache.loaded {
                    return Ok(());
                }
            }
        }

        // Phase 6 Task 5a: seed the dir-entry cache via **core**, byte-identically to the retired
        // ext4_rs `ext4_stat` + `ext4_readdir_with_offsets` pair. ext4_rs `ext4_readdir_with_offsets`
        // returns an empty vec for a non-directory inode (`!is_dir`) — the old code gated on
        // `ext4_stat().file_type != S_IFDIR` and raised ENOTDIR; we replicate that guard with the
        // core inode's `is_dir()`. The captured offset is each entry's **own** start byte offset
        // (`iblock*bs + off`), which feeds O(1) rmdir via `dir_remove_entry_at_offset`; core's
        // `dir_get_entries_with_start_offset` computes the identical offset (it differs from the
        // readdir `next_offset` cookie — see that function's PARITY note). DTO mapping mirrors the
        // Task 1 `readdir_locked` cutover exactly:
        //   - name    = `String::from_utf8_lossy(name)` (== ext4_rs `get_name()`)
        //   - ino     = entry inode (unused/inode==0 slots skipped by core, matching ext4_rs)
        //   - offset  = entry-start abs byte offset (== ext4_rs `entry_offset`)
        //   - de_type = dirent file-type byte (core `OwnedDirEntry.file_type` == ext4_rs `get_de_type()`)
        let (is_dir, entries_with_offsets) = self.run_io_dir_read_only_noerr(|ctx| {
            let Ok(dir) = super::core::inode::load_inode(ctx.reader, ctx.sb, parent) else {
                return (false, Vec::new());
            };
            if !dir.is_dir() {
                return (false, Vec::new());
            }
            let entries = super::core::dir::dir_get_entries_with_start_offset(ctx, &dir)
                .into_iter()
                .map(|(entry, start_offset)| {
                    (
                        String::from_utf8_lossy(&entry.name).into_owned(),
                        entry.inode,
                        start_offset as u64,
                        entry.file_type,
                    )
                })
                .collect::<Vec<_>>();
            (true, entries)
        })?;
        if !is_dir {
            return_errno_with_message!(Errno::ENOTDIR, "parent inode is not a directory");
        }

        let mut entry_map = BTreeMap::new();
        for (name, ino, entry_offset, de_type) in entries_with_offsets {
            entry_map.insert(
                name,
                DirEntryCacheEntry {
                    ino,
                    offset: entry_offset,
                    de_type,
                },
            );
        }

        let mut caches = self.dir_entry_cache.lock();
        let cache = caches.entry(parent).or_default();
        if !cache.loaded {
            cache.entries = entry_map;
            cache.loaded = true;
        }
        Ok(())
    }

    /// Insert a cache entry with a known byte offset in the parent directory stream.
    fn cache_insert_entry_with_offset(
        &self,
        parent: u32,
        name: &str,
        child: u32,
        offset: u64,
        de_type: u8,
    ) {
        let mut caches = self.dir_entry_cache.lock();
        let cache = caches.entry(parent).or_default();
        cache.entries.insert(
            name.to_string(),
            DirEntryCacheEntry {
                ino: child,
                offset,
                de_type,
            },
        );
    }

    /// Insert a cache entry when the byte offset is unknown (fallback paths).
    fn cache_insert_entry(&self, parent: u32, name: &str, child: u32, de_type: u8) {
        self.cache_insert_entry_with_offset(parent, name, child, u64::MAX, de_type);
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

    fn clear_inode_touch_cache(&self, ino: u32) {
        self.drop_page_cache_state(ino);
        self.invalidate_direct_read_cache(ino);
        self.coverage_invalidate(ino);
        self.inode_atime_cache.lock().remove(&ino);
        self.inode_ctime_cache.lock().remove(&ino);
        self.inode_mtime_ctime_cache.lock().remove(&ino);
    }

    fn dirent_type_from_inode_mode(mode: u16) -> u8 {
        // NOTE: the `mode` parameter shadows the imported `mode` module here, so reference the
        // mode-bit constants through their full path `super::types::mode::*`.
        use super::types::mode as ext4_mode;
        let file_type = mode & 0xF000;
        if file_type == ext4_mode::S_IFREG {
            1
        } else if file_type == ext4_mode::S_IFDIR {
            2
        } else if file_type == ext4_mode::S_IFCHR {
            3
        } else if file_type == ext4_mode::S_IFBLK {
            4
        } else if file_type == ext4_mode::S_IFIFO {
            5
        } else if file_type == ext4_mode::S_IFSOCK {
            6
        } else if file_type == ext4_mode::S_IFLNK {
            7
        } else {
            0
        }
    }

    fn loaded_dir_cache_entries(&self, ino: u32) -> Option<Vec<SimpleDirEntry>> {
        let caches = self.dir_entry_cache.lock();
        let cache = caches.get(&ino)?;
        if !cache.loaded {
            return None;
        }
        if cache.entries.values().any(|entry| entry.offset == u64::MAX) {
            return None;
        }

        let mut entries: Vec<_> = cache
            .entries
            .iter()
            .map(|(name, entry)| (entry.offset, name.clone(), *entry))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

        Some(
            entries
                .into_iter()
                .enumerate()
                .map(|(index, (_, name, entry))| SimpleDirEntry {
                    inode: entry.ino,
                    de_type: entry.de_type,
                    name,
                    // The VFS layer only requires a stable, monotonically
                    // increasing cookie for repeated readdir_at calls.
                    next_offset: index.saturating_add(1),
                })
                .collect(),
        )
    }

    fn lookup_at_locked(&self, parent: u32, name: &str) -> Result<u32> {
        self.lookup_at_locked_with_type(parent, name)
            .map(|(ino, _)| ino)
    }

    fn lookup_at_locked_with_type(&self, parent: u32, name: &str) -> Result<(u32, u8)> {
        match self.lookup_cache(parent, name) {
            DirLookupCacheResult::Hit(ino, _, de_type) => return Ok((ino, de_type)),
            DirLookupCacheResult::Miss => {
                return_errno_with_message!(Errno::ENOENT, "entry not found in directory cache");
            }
            DirLookupCacheResult::Unknown => {}
        }

        if self.load_dir_cache_if_needed_locked(parent).is_ok() {
            match self.lookup_cache(parent, name) {
                DirLookupCacheResult::Hit(ino, _, de_type) => return Ok((ino, de_type)),
                DirLookupCacheResult::Miss => {
                    return_errno_with_message!(Errno::ENOENT, "entry not found in directory cache");
                }
                DirLookupCacheResult::Unknown => {}
            }
        }

        // Phase 6 Task 1: single-level directory lookup via core. `core::dir::lookup_at` is a
        // byte-parity reimplementation of ext4_rs `ext4_lookup_at` (cross-block linear scan,
        // ENOENT on miss).
        let ino = self.run_io_dir_read_only(|ctx| {
            let parent_inode = super::core::inode::load_inode(ctx.reader, ctx.sb, parent)?;
            super::core::dir::lookup_at(ctx, &parent_inode, name.as_bytes())
        })?;
        self.cache_insert_entry(parent, name, ino, 0);
        Ok((ino, 0))
    }

    pub(super) fn lookup_at(&self, parent: u32, name: &str) -> Result<u32> {
        let parent_lock = Self::correctness_lock_for(&self.dir_correctness_locks, parent);
        let _parent_guard = parent_lock.write();
        self.lookup_at_locked(parent, name)
    }

    pub(super) fn dir_open(&self, path: &str) -> Result<u32> {
        // Phase 6 Task 1: resolve a path to its inode via core, replicating ext4_rs
        // `ext4_dir_open` == `generic_open(path, root, create=false)`. `generic_open` walks the
        // path from the root inode component-by-component (skipping empty components from
        // consecutive '/'), failing the whole lookup with ENOENT on the first missing component
        // (create=false). We reproduce that exactly with a per-component `core::dir::lookup_at`,
        // re-loading the descended-into directory inode each step. `dir_open` is only called from
        // inode.rs's `..` branch with a well-formed relative parent path (no leading/trailing
        // slash, empty path handled before the call).
        self.run_io_read_only(|ctx| {
            let mut current = EXT4_ROOT_INODE;
            for component in path.split('/') {
                if component.is_empty() {
                    continue;
                }
                let dir = super::core::inode::load_inode(ctx.reader, ctx.sb, current)?;
                current = super::core::dir::lookup_at(ctx, &dir, component.as_bytes())?;
            }
            Ok(current)
        })
    }

    pub(super) fn create_at(&self, parent: u32, name: &str, mode: u16) -> Result<u32> {
        // Drain any inode frees deferred by an atomic-context close (BUG-19). Done before taking any
        // lock so the drain acquires only its own per-ino correctness lock.
        self.reclaim_pending_inode_frees();
        let parent_lock = Self::correctness_lock_for(&self.dir_correctness_locks, parent);
        let _parent_guard = parent_lock.write();
        let op = JournaledOp::Create;
        let ino = self.run_journaled_namespace(Some(op), |nctx| {
            super::core::dir::create_at(nctx, parent, name.as_bytes(), mode)
        })?;
        self.note_inode_allocated(ino);
        let child_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
        let _child_guard = child_lock.write();
        // A freshly allocated inode must not inherit stale VMO/PageCache state
        // from an earlier lifetime with the same inode number. Discard instead
        // of evicting: writing old cached pages through the new inode mapping
        // would corrupt the new file.
        self.discard_page_cache_state(ino);
        self.invalidate_direct_read_cache(ino);
        self.cache_insert_entry(parent, name, ino, Self::dirent_type_from_inode_mode(mode));
        self.cache_remove_dir(ino);
        self.touch_birth_times(ino)?;
        self.touch_mtime_ctime(parent)?;
        Ok(ino)
    }

    pub(super) fn mkdir_at(&self, parent: u32, name: &str, mode: u16) -> Result<u32> {
        self.reclaim_pending_inode_frees();
        let parent_lock = Self::correctness_lock_for(&self.dir_correctness_locks, parent);
        let _parent_guard = parent_lock.write();
        // Ensure the parent directory cache is fully loaded so subsequent existence
        // checks (lookup_cache → Miss) can bypass the O(n) dir_find_entry disk scan.
        // The first call reads the directory once; subsequent calls return immediately.
        let cache_loaded = self.load_dir_cache_if_needed_locked(parent).is_ok();

        if cache_loaded {
            match self.lookup_cache(parent, name) {
                DirLookupCacheResult::Hit(_, _, _) => return_errno!(Errno::EEXIST),
                DirLookupCacheResult::Miss => {
                    // Cache is complete and confirms the name is absent — skip disk scan.
                    let op = JournaledOp::Mkdir;
                    let (ino, dir_byte_offset) = self.run_journaled_namespace(Some(op), |nctx| {
                        super::core::dir::mkdir_unchecked_at(nctx, parent, name.as_bytes(), mode)
                    })?;
                    self.note_inode_allocated(ino);
                    let child_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
                    let _child_guard = child_lock.write();
                    self.cache_insert_entry_with_offset(parent, name, ino, dir_byte_offset, 2);
                    self.cache_remove_dir(ino);
                    self.touch_birth_times(ino)?;
                    self.touch_mtime_ctime(parent)?;
                    return Ok(ino);
                }
                DirLookupCacheResult::Unknown => {}
            }
        }

        // Fallback: cache unavailable — use disk-based existence check.
        let op = JournaledOp::Mkdir;
        let ino = self.run_journaled_namespace(Some(op), |nctx| {
            super::core::dir::mkdir_at(nctx, parent, name.as_bytes(), mode)
        })?;
        self.note_inode_allocated(ino);
        let child_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
        let _child_guard = child_lock.write();
        self.cache_insert_entry(parent, name, ino, 2);
        self.cache_remove_dir(ino);
        self.touch_birth_times(ino)?;
        self.touch_mtime_ctime(parent)?;
        Ok(ino)
    }

    pub(super) fn unlink_at(&self, parent: u32, name: &str) -> Result<()> {
        self.reclaim_pending_inode_frees();
        let parent_lock = Self::correctness_lock_for(&self.dir_correctness_locks, parent);
        let _parent_guard = parent_lock.write();
        let target_ino = self.lookup_at_locked(parent, name)?;
        let target_meta = self.stat(target_ino)?;
        if target_meta.file_type == mode::S_IFDIR {
            return_errno!(Errno::EISDIR);
        }
        let target_lock = Self::correctness_lock_for(&self.inode_correctness_locks, target_ino);
        let _target_guard = target_lock.write();

        let op = JournaledOp::Unlink;
        self.run_journaled_namespace(Some(op), |nctx| {
            super::core::dir::unlink_at(nctx, parent, name.as_bytes())
        })?;
        self.cache_remove_entry(parent, name);
        self.clear_inode_touch_cache(target_ino);
        self.touch_mtime_ctime(parent)?;
        Ok(())
    }

    pub(super) fn on_open_file_handle(&self, ino: u32) {
        let mut open_file_handles = self.open_file_handles.lock();
        *open_file_handles.entry(ino).or_insert(0) += 1;
    }

    pub(super) fn on_close_file_handle(&self, ino: u32) -> Result<()> {
        let last_handle_closed = {
            let mut open_file_handles = self.open_file_handles.lock();
            let Some(count) = open_file_handles.get_mut(&ino) else {
                return Ok(());
            };
            *count = count.saturating_sub(1);
            if *count == 0 {
                open_file_handles.remove(&ino);
                true
            } else {
                false
            }
        };
        // BUG-19 (ext4-spec-correct + atomic-context safe): open-then-unlink-then-close path. When
        // the LAST open handle closes and the file was already unlinked (nlink==0), this close IS the
        // last reference, so the inode + its data blocks must be reclaimed (POSIX: a file held open
        // across unlink is freed only at close).
        //
        // BUT `on_close_file_handle` runs inside `InodeHandle::drop`, which can fire in an ATOMIC
        // context: `dup2`/`dup3` (`do_dup3`) drops the replaced fd's handle while holding the
        // file-table write lock (preempt disabled). The reclaim needs blocking journaled disk I/O
        // (read the inode, truncate blocks, free the bitmap bit) whose task switch would panic there.
        // So we DEFER: only record the ino in `pending_inode_free` here (a cheap, non-blocking insert
        // — uncontended `Mutex::lock` does not sleep, same as the `open_file_handles` insert above),
        // and let `reclaim_pending_inode_frees` perform the real free from the next SLEEPABLE fs op
        // (namespace ops + `sync`). The actual free still re-checks nlink==0 / no-open-handles under
        // the inode correctness lock and is idempotent via the `freed_inodes` guard.
        if last_handle_closed {
            self.pending_inode_free.lock().insert(ino);
        }
        Ok(())
    }

    fn has_open_file_handles(&self, ino: u32) -> bool {
        self.open_file_handles
            .lock()
            .get(&ino)
            .is_some_and(|count| *count > 0)
    }

    /// Drop the "already-freed" marker for `ino` (called right after a successful inode allocation
    /// that may hand out a previously-freed number). Keeps the `freed_inodes` double-free guard from
    /// blocking reclaim of the inode's *next* lifetime. Also drops any stale `pending_inode_free`
    /// entry for the reused number so a deferred free from the inode's PREVIOUS lifetime can never
    /// target the freshly allocated owner (the `nlink != 0` re-check in `cleanup_unlinked_file`
    /// already protects this; clearing here just avoids a wasted reclaim attempt).
    fn note_inode_allocated(&self, ino: u32) {
        self.freed_inodes.lock().remove(&ino);
        self.pending_inode_free.lock().remove(&ino);
    }

    /// BUG-19 fix (ext4-spec-correct): reclaim a regular file whose last reference is gone.
    ///
    /// Mirrors ext2's evict model (`InodeImpl::sync_metadata` frees the inode when
    /// `hard_links()==0`, guarded against double-free): once a regular file is unlinked (nlink==0)
    /// and has no remaining open handles, this releases ALL of its data blocks AND clears its inode
    /// bitmap bit, bumping `s_free_inodes_count`/`s_free_blocks_count` so repeated create/delete no
    /// longer leaks and e2fsck reports zero orphan inodes.
    ///
    /// Both the data-block free (`truncate_inode(0)`) and the inode-bitmap free run inside a SINGLE
    /// `run_journaled_namespace` transaction (`free_inode_on_evict_at`), so the reclaim is
    /// crash-atomic and BOTH free counts are sunk back into the `running_sb` single source of truth
    /// (the namespace chokepoint harvests `post.free_blocks_count()` + `post.free_inodes_count()`).
    /// Lock order unchanged: inode correctness lock → RUNTIME → jbd2 → inner.
    pub(super) fn cleanup_unlinked_file(&self, ino: u32) -> Result<()> {
        let inode_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
        let _inode_guard = inode_lock.write();

        let meta = self.stat(ino)?;
        if meta.nlink != 0 || meta.file_type != mode::S_IFREG {
            return Ok(());
        }
        if self.has_open_file_handles(ino) {
            return Ok(());
        }
        // Double-free guard: if a prior evict (last-ref drop vs last-handle close race) already
        // reclaimed this inode's bitmap bit, the on-disk inode table still reads nlink==0, so we
        // must not free the bitmap bit a second time. The marker is dropped when the number is
        // reallocated (`note_inode_allocated`).
        if self.freed_inodes.lock().contains(&ino) {
            return Ok(());
        }

        self.reset_page_cache_after_truncate(ino, 0)?;
        let op = JournaledOp::Unlink;
        self.run_journaled_namespace(Some(op), |nctx| {
            super::core::dir::free_inode_on_evict_at(nctx, ino)
        })?;
        self.freed_inodes.lock().insert(ino);
        // Drop every cached scrap of the now-freed inode (page cache, direct-read, coverage, time
        // caches) so a later reallocation of this number starts clean.
        self.clear_inode_touch_cache(ino);
        Ok(())
    }

    /// Drain the `pending_inode_free` set, reclaiming each inode whose last open handle closed in an
    /// atomic context (see `on_close_file_handle`). MUST be called only from a SLEEPABLE context —
    /// the per-inode `cleanup_unlinked_file` does blocking journaled disk I/O. Invoked at the head of
    /// every sleepable namespace op (`create_at`/`unlink_at`/`rmdir_at`/`rename_at`/`mkdir_at`) and
    /// from `FileSystem::sync`, so the deferred frees are reclaimed promptly under continued fs
    /// activity — closing the steady-state create/delete leak (e2fsck sees zero orphans) without ever
    /// blocking in the close path.
    ///
    /// Re-entrancy: a pending ino that was already reclaimed (via `cleanup_unlinked`'s sleepable
    /// last-ref path, or a prior drain) is a no-op — `cleanup_unlinked_file` re-checks nlink/handles
    /// and the `freed_inodes` guard. A pending ino that was rescued (relinked, or reused after free)
    /// is likewise skipped by those checks. Errors from one inode do not abort the rest.
    fn reclaim_pending_inode_frees(&self) {
        // Snapshot + clear under the lock so the (blocking) frees run without holding it.
        let pending: Vec<u32> = {
            let mut set = self.pending_inode_free.lock();
            if set.is_empty() {
                return;
            }
            core::mem::take(&mut *set).into_iter().collect()
        };
        for ino in pending {
            if let Err(err) = self.cleanup_unlinked_file(ino) {
                warn!(
                    "ext4: deferred reclaim of unlinked inode {} failed: {:?}",
                    ino, err
                );
            }
        }
    }

    pub(super) fn rmdir_at(&self, parent: u32, name: &str) -> Result<()> {
        self.reclaim_pending_inode_frees();
        let parent_lock = Self::correctness_lock_for(&self.dir_correctness_locks, parent);
        let _parent_guard = parent_lock.write();
        let child_ino = self.lookup_at_locked(parent, name)?;
        let child_meta = self.stat(child_ino)?;
        if child_meta.file_type != mode::S_IFDIR {
            return_errno!(Errno::ENOTDIR);
        }
        let child_lock = Self::correctness_lock_for(&self.inode_correctness_locks, child_ino);
        let _child_guard = child_lock.write();

        let entries = self.readdir_locked(child_ino)?;
        let has_real_child = entries
            .iter()
            .any(|entry| entry.name != "." && entry.name != "..");
        if has_real_child {
            return_errno!(Errno::ENOTEMPTY);
        }

        // Retrieve cached byte offset for O(1) parent-dir entry removal.
        let dir_byte_offset = match self.lookup_cache(parent, name) {
            DirLookupCacheResult::Hit(_, offset, _) => offset,
            _ => u64::MAX,
        };

        let op = JournaledOp::Rmdir;
        if dir_byte_offset != u64::MAX {
            self.run_journaled_namespace(Some(op), |nctx| {
                super::core::dir::rmdir_at_fast(nctx, parent, child_ino, dir_byte_offset)
            })?;
        } else {
            self.run_journaled_namespace(Some(op), |nctx| {
                super::core::dir::rmdir_at(nctx, parent, name.as_bytes())
            })?;
        }
        self.cache_remove_entry(parent, name);
        self.cache_remove_dir(child_ino);
        self.clear_inode_touch_cache(child_ino);
        self.touch_mtime_ctime(parent)?;
        Ok(())
    }

    pub(super) fn rename_at(
        &self,
        old_parent: u32,
        old_name: &str,
        new_parent: u32,
        new_name: &str,
    ) -> Result<()> {
        self.reclaim_pending_inode_frees();
        self.with_dir_locks(&[old_parent, new_parent], || {
            if old_parent == new_parent && old_name == new_name {
                return Ok(());
            }

            let (old_ino, old_de_type) = self.lookup_at_locked_with_type(old_parent, old_name)?;
            let overwritten_ino = self.lookup_at_locked(new_parent, new_name).ok();
            let overwritten_is_dir = overwritten_ino
                .and_then(|ino| self.stat(ino).ok().map(|meta| (ino, meta)))
                .map(|(ino, meta)| {
                    (
                        ino,
                        meta.file_type == mode::S_IFDIR,
                    )
                });

            let mut affected_inodes = Vec::new();
            affected_inodes.push(old_ino);
            if let Some(ino) = overwritten_ino {
                affected_inodes.push(ino);
            }

            // The moved inode's own entry cache (if it is a directory) caches a `..` pointing at
            // `old_parent`; a cross-directory move reparents that `..` to `new_parent` inside
            // `core::dir::rename_at`, so its cached children become stale. Invalidate below.
            let old_is_dir = old_de_type == 2; // DE filetype DIR
            let cross_dir = new_parent != old_parent;

            self.with_inode_locks(&affected_inodes, || {
                let op = JournaledOp::Rename;
                self.run_journaled_namespace(Some(op), |nctx| {
                    super::core::dir::rename_at(
                        nctx,
                        old_parent,
                        old_name.as_bytes(),
                        new_parent,
                        new_name.as_bytes(),
                    )
                })?;

                // Cache invalidation: drop the old entry, insert under the new parent.
                self.cache_remove_entry(old_parent, old_name);
                self.cache_insert_entry(new_parent, new_name, old_ino, old_de_type);

                // A moved directory had its `..` rewritten (cross-dir) and changed parents; drop its
                // own cached child set so a later readdir re-reads the reparented `..`.
                if old_is_dir && cross_dir {
                    self.cache_remove_dir(old_ino);
                }

                // An overwritten target: drop its cached children (if a dir) and its name entry, then
                // restore the now-correct mapping (new_name -> old_ino) and scrub its time/page caches.
                if let Some((ino, true)) = overwritten_is_dir {
                    self.cache_remove_dir(ino);
                }
                if let Some(ino) = overwritten_ino {
                    if ino != old_ino {
                        self.cache_remove_entry(new_parent, new_name);
                        self.cache_insert_entry(new_parent, new_name, old_ino, old_de_type);
                        self.clear_inode_touch_cache(ino);
                    }
                }

                self.touch_mtime_ctime(old_parent)?;
                if cross_dir {
                    self.touch_mtime_ctime(new_parent)?;
                }
                self.touch_ctime(old_ino)?;

                Ok(())
            })?;

            // Overwrite reclamation.
            //
            // (1) REGULAR FILE target: `core::dir::rename_at`'s `unlink_file_branch` dropped the
            //     victim's nlink to 0 but freed nothing — the rename path has no VFS
            //     `cleanup_unlinked` hook for the overwritten target, so `mv a b` (b a regular file)
            //     would leak b's inode + data blocks. Reclaim it here via the same evict path used by
            //     unlink. Must be done AFTER `with_inode_locks` releases (its non-reentrant write
            //     guard on the victim ino would deadlock with `cleanup_unlinked_file`'s own inode
            //     correctness lock). The call is self-guarded: it re-checks
            //     `nlink==0 && S_IFREG && !has_open_file_handles` and consults the `freed_inodes`
            //     double-free set, so an open-across-rename target is freed only at its last close
            //     and there is no double-free.
            //
            // (2) EMPTY DIRECTORY target (`mv dir1 dir2`, dir2 an empty dir): NOW handled inside
            //     `core::dir::rename_at` — its directory-overwrite branch calls `finalize_freed_inode`
            //     (clears the inode bitmap bit + bumps free_inodes + decrements used_dirs), closing
            //     the BUG-19-deferred empty-dir-overwrite leak transactionally. No post-lock cleanup
            //     needed for the dir case; we only mark it in `freed_inodes` so a later realloc of the
            //     number is not blocked, and scrub its caches (done above via `cache_remove_dir` +
            //     `clear_inode_touch_cache`).
            if let Some((ino, false)) = overwritten_is_dir
                && ino != old_ino
            {
                self.cleanup_unlinked_file(ino)?;
            }
            if let Some((ino, true)) = overwritten_is_dir
                && ino != old_ino
            {
                // The empty-dir target was already freed in-core; record it in the double-free guard
                // and scrub its remaining caches (page/coverage/time) so a reallocated number starts
                // clean. `note_inode_allocated` drops the marker when the number is handed out again.
                self.freed_inodes.lock().insert(ino);
                self.clear_inode_touch_cache(ino);
            }

            Ok(())
        })
    }

    pub(super) fn read_at(
        &self,
        ino: u32,
        offset: usize,
        data: &mut [u8],
        status_flags: StatusFlags,
    ) -> Result<usize> {
        let inode_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
        let _inode_guard = inode_lock.write();
        // Phase 6 Task 1: drive the core read path. `core::file::read_at` is a byte-parity
        // reimplementation of ext4_rs `ext4_read_at` (clamp to size, holes/unwritten zero-filled),
        // returning the byte count read.
        let read_len = self.run_io_file_read_only(|ctx| {
            let inode = super::core::inode::load_inode(ctx.reader, ctx.sb, ino)?;
            super::core::file::read_at(ctx, &inode, offset, data)
        })?;
        if read_len > 0 {
            self.touch_atime(ino, status_flags)?;
        }
        Ok(read_len)
    }

    pub(super) fn read_at_page_cache(
        self: &Arc<Self>,
        ino: u32,
        offset: usize,
        writer: &mut VmWriter,
        status_flags: StatusFlags,
    ) -> Result<usize> {
        let inode_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
        let _inode_guard = inode_lock.write();
        let file_size = self.stat(ino)?.size as usize;
        let read_len = file_size.saturating_sub(offset).min(writer.avail());
        if read_len == 0 {
            return Ok(0);
        }

        let page_cache = self.page_cache_state_for_inode(ino, file_size)?.pages();
        let old_avail = writer.avail();
        writer.limit(read_len);
        page_cache.read(offset, writer)?;
        debug_assert_eq!(writer.avail(), old_avail - read_len);
        if read_len > 0 {
            self.touch_atime(ino, status_flags)?;
        }
        Ok(read_len)
    }

    pub(super) fn read_direct_at(
        &self,
        ino: u32,
        offset: usize,
        writer: &mut VmWriter,
        status_flags: StatusFlags,
    ) -> Result<usize> {
        // Phase 5 full-path probe: wall clock from the very entry (includes the
        // lock acquire, evict, atime, etc.) so we can see how much per-read
        // overhead lives outside the individually-measured stages.
        let rda_start = Self::monotonic_nanos();
        let inode_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
        // C1 (Phase 6): under page_cache=0 the read path mutates no inode
        // state (the evict below is a no-op without page-cache state, all
        // caches it touches have their own locks), so concurrent dio reads
        // of the same file take the lock shared. page_cache=1 keeps the
        // exclusive guard (it evicts page-cache ranges).
        let (_shared_guard, _excl_guard) = if self.page_cache_enabled {
            (None, Some(inode_lock.write()))
        } else {
            (Some(inode_lock.read()), None)
        };
        self.maybe_start_direct_read_profile();
        self.evict_page_cache_range(ino, offset, writer.avail())?;
        if self.page_cache_enabled {
            self.invalidate_direct_read_cache(ino);
        }

        let mut plan_ns = 0u64;
        let mut alloc_ns = 0u64;
        let mut submit_ns = 0u64;
        let (direct_len, mappings, bio_waiter) = if !self.page_cache_enabled
            && let Some(pending) =
                self.take_matching_pending_direct_read(ino, offset, writer.avail())
        {
            (pending.len, pending.mappings, pending.waiter)
        } else {
            let plan_start = Self::monotonic_nanos();
            let (direct_len, mappings) = if self.direct_read_cache_enabled
                && !self.page_cache_enabled
            {
                // Speculative data read cache (opt-in, off in the cache-off guard).
                self.plan_direct_read_cached(ino, offset, writer.avail(), true)?
            } else if self.extent_map_cache_enabled && !self.page_cache_enabled {
                // Phase 5: metadata-only extent mapping cache — skips the
                // per-read find_extent walk on sequential reads.
                self.plan_direct_read_extent_map_cached(ino, offset, writer.avail())?
            } else {
                self.plan_direct_read_cached(ino, offset, writer.avail(), false)?
            };
            plan_ns = Self::monotonic_nanos().saturating_sub(plan_start);
            if direct_len == 0 {
                return Ok(0);
            }

            let (bio_waiter, current_alloc_ns, current_submit_ns) =
                self.submit_direct_read_request_with_hint(&mappings, false)?;
            alloc_ns = alloc_ns.saturating_add(current_alloc_ns);
            submit_ns = submit_ns.saturating_add(current_submit_ns);
            (direct_len, mappings, bio_waiter)
        };

        let (prepared_next_read, next_plan_ns) =
            self.maybe_prepare_speculative_direct_read(ino, offset, direct_len)?;
        plan_ns = plan_ns.saturating_add(next_plan_ns);

        let wait_ns = self.wait_direct_read(&bio_waiter)?;
        let (next_alloc_ns, next_submit_ns) =
            self.submit_prepared_speculative_direct_read(ino, prepared_next_read)?;
        alloc_ns = alloc_ns.saturating_add(next_alloc_ns);
        submit_ns = submit_ns.saturating_add(next_submit_ns);

        let (mapped_bytes, copy_ns) =
            self.copy_completed_direct_read(offset, direct_len, &mappings, &bio_waiter, writer)?;
        let zero_fill_bytes = direct_len.saturating_sub(mapped_bytes);

        self.note_completed_direct_read(ino, offset, direct_len);
        let atime_start = Self::monotonic_nanos();
        self.touch_atime_after_direct_read(ino, status_flags)?;
        let atime_ns = Self::monotonic_nanos().saturating_sub(atime_start);
        let total_ns = Self::monotonic_nanos().saturating_sub(rda_start);
        let reads = self.direct_read_profile.record_read(
            direct_len,
            mappings.len(),
            mapped_bytes,
            zero_fill_bytes,
            plan_ns,
            alloc_ns,
            submit_ns,
            wait_ns,
            copy_ns,
            total_ns,
            atime_ns,
        );
        self.maybe_log_direct_read_profile(reads, false);
        Ok(direct_len)
    }

    pub(super) fn write_at(&self, ino: u32, offset: usize, data: &[u8]) -> Result<usize> {
        let inode_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
        let _inode_guard = inode_lock.write();
        let generic014_like_write = data.len() == 512;
        let mut generic014_write_seq = 0;
        let mut generic014_write_start_ns = 0;
        if generic014_like_write {
            generic014_write_seq = GENERIC014_WRITE_PROGRESS.fetch_add(1, Ordering::Relaxed) + 1;
            generic014_write_start_ns = Self::monotonic_nanos();
            if generic014_write_seq <= 8
                || generic014_write_seq % GENERIC014_PROGRESS_LOG_INTERVAL == 0
            {
                debug!(
                    "ext4: generic014-like write progress seq={} ino={} offset={} len={}",
                    generic014_write_seq,
                    ino,
                    offset,
                    data.len()
                );
            }
        }
        let now = Self::now_unix_seconds_u32();
        let op = JournaledOp::for_small_write(ino, offset, data);
        let mut ext4_write_elapsed_ns = 0u64;
        let mut inode_time_elapsed_ns = 0u64;
        let write_result = self
            .run_journaled_core(op, ino, |ctx, alloc, inode| {
                let ext4_write_start_ns = Self::monotonic_nanos();
                let written = super::core::file::write_at(ctx, alloc, inode, offset, data)?;
                ext4_write_elapsed_ns = Self::monotonic_nanos().saturating_sub(ext4_write_start_ns);
                if written > 0 {
                    let inode_time_start_ns = Self::monotonic_nanos();
                    Self::core_set_inode_times(ctx, inode, None, Some(now), Some(now))?;
                    inode_time_elapsed_ns =
                        Self::monotonic_nanos().saturating_sub(inode_time_start_ns);
                }
                Ok(written)
            })
            .map_err(|err| {
                if err.error() == Errno::ENOSPC {
                    debug!(
                        "ext4 write_at returned ENOSPC: ino={} offset={} len={}",
                        ino,
                        offset,
                        data.len()
                    );
                } else {
                    error!(
                        "ext4 write_at failed: ino={} offset={} len={} err={:?}",
                        ino,
                        offset,
                        data.len(),
                        err
                    );
                }
                err
            });
        self.invalidate_direct_read_cache(ino);
        let written = write_result?;
        if written > 0 {
            self.discard_page_cache_range(ino, offset, written);
            self.inode_mtime_ctime_cache.lock().insert(ino, now);
        }
        if generic014_like_write {
            let elapsed_ns = Self::monotonic_nanos().saturating_sub(generic014_write_start_ns);
            if generic014_write_seq <= 8
                || generic014_write_seq % GENERIC014_PROGRESS_LOG_INTERVAL == 0
                || elapsed_ns >= GENERIC014_SLOW_OP_LOG_THRESHOLD_NS
            {
                debug!(
                    "ext4: generic014-like write duration seq={} ino={} offset={} len={} written={} elapsed_ms={} ext4_write_ms={} inode_time_ms={}",
                    generic014_write_seq,
                    ino,
                    offset,
                    data.len(),
                    written,
                    elapsed_ns / 1_000_000,
                    ext4_write_elapsed_ns / 1_000_000,
                    inode_time_elapsed_ns / 1_000_000
                );
            }
        }
        Ok(written)
    }

    pub(super) fn write_at_page_cache(
        self: &Arc<Self>,
        ino: u32,
        offset: usize,
        reader: &mut VmReader,
    ) -> Result<usize> {
        let write_len = reader.remain();
        if write_len == 0 {
            return Ok(0);
        }

        // Phase 6 read-only probe: time the whole per-write() path so Step 0 can
        // split SQLite buffered-write cost into overwrite fast path vs
        // append/alloc slow path. No-op unless `ext4fs.phase2_profile=1`.
        let profile_enabled = self.phase2_profile_enabled;
        let call_start_ns = if profile_enabled {
            Self::monotonic_nanos()
        } else {
            0
        };

        let inode_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
        let _inode_guard = inode_lock.write();

        // Fast path: a pure overwrite of already-written blocks (no append, no
        // hole, no unwritten extent) needs no block allocation, so skip the
        // per-write() journaled prepare entirely. The data reaches disk via the
        // journaled writeback at fsync/sync; mtime/ctime is journaled at most
        // once per second by `touch_mtime_ctime` (ext4 inode timestamps are
        // seconds-granularity, so this loses no precision).
        //
        // P2 (Phase 6): the written check is answered by the in-memory
        // coverage cache (one BTreeMap lookup), and the user data is copied
        // straight from the `VmReader` into the page cache — the same shape
        // as ext2's write path (no intermediate buffer, no extent walk).
        let cur_size = self.stat(ino)?.size as usize;
        if let Some(write_end) = offset.checked_add(write_len)
            && write_end <= cur_size
            && self.write_range_covered_written(ino, offset, write_len)?
        {
            self.touch_mtime_ctime(ino)?;
            self.invalidate_direct_read_cache(ino);
            let page_cache = self.page_cache_state_for_inode(ino, cur_size)?.pages();
            page_cache.write(offset, reader)?;
            if profile_enabled {
                self.buffered_write_profile.record_fast(
                    write_len,
                    Self::monotonic_nanos().saturating_sub(call_start_ns),
                );
            }
            return Ok(write_len);
        }

        // Slow path needs the data in a kernel buffer: the journaled prepare
        // must run before the page-cache copy.
        let mut data = vec![0u8; write_len];
        reader.read_fallible(&mut VmWriter::from(data.as_mut_slice()).to_fallible())?;

        // Slow path: append / sparse / unmapped range — needs journaled allocation.
        let now = Self::now_unix_seconds_u32();
        let op = JournaledOp::for_small_write(ino, offset, data.as_slice());
        let prepare_start_ns = if profile_enabled {
            Self::monotonic_nanos()
        } else {
            0
        };
        let mappings = self
            .run_journaled_core(op, ino, |ctx, alloc, inode| {
                let (_lblock_start, ranges) =
                    super::core::file::prepare_write_at(ctx, alloc, inode, offset, write_len)?;
                Self::core_set_inode_times(ctx, inode, None, Some(now), Some(now))?;
                Ok(Self::core_to_integration_ranges(&ranges))
            })
            .map_err(|err| {
                if err.error() == Errno::ENOSPC {
                    debug!(
                        "ext4 page-cache prepare write returned ENOSPC: ino={} offset={} len={}",
                        ino, offset, write_len
                    );
                } else {
                    error!(
                        "ext4 page-cache prepare write failed: ino={} offset={} len={} err={:?}",
                        ino, offset, write_len, err
                    );
                }
                err
            })?;
        let prepare_ns = if profile_enabled {
            Self::monotonic_nanos().saturating_sub(prepare_start_ns)
        } else {
            0
        };
        // BUG-5 (Task 8): no per-written-block revoke here. These `mappings` are freshly
        // (re)mapped **data** blocks of a regular-file write; data blocks are not journaled, so
        // revoking them would be both a perf write-amplification (a revoke record per data block)
        // and architecturally wrong. The revoke is now driven on the metadata-block FREE path
        // (Linux `ext4_forget` model) inside core, not on the write/reuse path.
        // P2: the prepare made the whole write range written; extend the
        // coverage so subsequent overwrites of it take the fast path.
        self.coverage_insert_ranges(ino, &mappings);

        self.invalidate_direct_read_cache(ino);
        self.inode_mtime_ctime_cache.lock().insert(ino, now);
        let file_size = self.stat(ino)?.size as usize;
        let page_cache = self.page_cache_state_for_inode(ino, file_size)?.pages();
        page_cache.write(offset, &mut VmReader::from(data.as_slice()).to_fallible())?;
        if profile_enabled {
            let blocks: u64 = mappings.iter().map(|m| u64::from(m.len)).sum();
            self.buffered_write_profile.record_slow(
                write_len,
                blocks,
                prepare_ns,
                Self::monotonic_nanos().saturating_sub(call_start_ns),
            );
        }
        Ok(write_len)
    }

    /// P2 (Phase 6): answers "is `[offset, offset+len)` fully written?" for
    /// the buffered overwrite fast path, preferring the in-memory coverage
    /// cache over the extent-tree walk.
    ///
    /// - Coverage hit → true with one BTreeMap lookup.
    /// - No entry → populate it with one authoritative whole-file mapping
    ///   walk (read-semantics `map_blocks`, which skips unwritten extents),
    ///   then answer from it: post-populate the entry IS the truth.
    /// - TooFragmented → fall back to the per-write range walk.
    ///
    /// Caller must hold the inode correctness lock (all mutators of this
    /// inode's mappings do), which serializes coverage reads/populates with
    /// truncate/unlink invalidation.
    fn write_range_covered_written(&self, ino: u32, offset: usize, len: usize) -> Result<bool> {
        if len == 0 {
            return Ok(true);
        }
        let block_size = EXT4_BLOCK_SIZE;
        let end = offset
            .checked_add(len)
            .ok_or_else(|| Error::with_message(Errno::EFBIG, "write range overflow"))?;
        let lblock_start = u32::try_from(offset / block_size)
            .map_err(|_| Error::with_message(Errno::EFBIG, "write lblock overflow"))?;
        let lblock_count = u32::try_from(end.div_ceil(block_size) - offset / block_size)
            .map_err(|_| Error::with_message(Errno::EFBIG, "write lblock overflow"))?;

        {
            let coverage = self.inode_written_coverage.lock();
            match coverage.get(&ino) {
                Some(entry @ WrittenCoverage::Ranges(_)) => {
                    return Ok(entry.covers(lblock_start, lblock_count));
                }
                Some(WrittenCoverage::TooFragmented) => {
                    drop(coverage);
                    return self.write_range_fully_mapped(ino, offset, len);
                }
                None => {}
            }
        }

        self.coverage_populate(ino)?;
        Ok(self
            .inode_written_coverage
            .lock()
            .get(&ino)
            .is_some_and(|c| c.covers(lblock_start, lblock_count)))
    }

    /// Builds the written coverage for `ino` from one authoritative
    /// whole-file mapping walk. Caller holds the inode correctness lock.
    fn coverage_populate(&self, ino: u32) -> Result<()> {
        let file_size = self.stat(ino)?.size as usize;
        let lblock_count = u32::try_from(file_size.div_ceil(EXT4_BLOCK_SIZE))
            .map_err(|_| Error::with_message(Errno::EFBIG, "file lblock overflow"))?;
        let mappings = if lblock_count == 0 {
            Vec::new()
        } else {
            self.run_io_file_read_only(|ctx| Self::core_map_blocks(ctx, ino, 0, lblock_count))?
        };

        let mut entry = WrittenCoverage::Ranges(BTreeMap::new());
        for mapping in &mappings {
            entry.insert(mapping.lblock, mapping.len);
            if matches!(entry, WrittenCoverage::TooFragmented) {
                break;
            }
        }

        let mut coverage = self.inode_written_coverage.lock();
        if coverage.len() >= WRITTEN_COVERAGE_MAX_INODES && !coverage.contains_key(&ino) {
            coverage.clear();
        }
        coverage.insert(ino, entry);
        Ok(())
    }

    /// Extends `ino`'s coverage with ranges a successful journaled prepare
    /// just made written. No-op when the entry is absent (the next fast-path
    /// miss repopulates) or TooFragmented.
    fn coverage_insert_ranges(&self, ino: u32, mappings: &[SimpleBlockRange]) {
        let mut coverage = self.inode_written_coverage.lock();
        let Some(entry) = coverage.get_mut(&ino) else {
            return;
        };
        for mapping in mappings {
            entry.insert(mapping.lblock, mapping.len);
        }
    }

    pub(super) fn coverage_invalidate(&self, ino: u32) {
        self.inode_written_coverage.lock().remove(&ino);
    }

    /// Read-only check that every block backing `[offset, offset+len)` is already
    /// allocated (no holes). Used by the buffered-write fast path to decide
    /// whether a write is a pure overwrite that needs no journaled allocation.
    fn write_range_fully_mapped(&self, ino: u32, offset: usize, len: usize) -> Result<bool> {
        if len == 0 {
            return Ok(true);
        }
        let block_size = EXT4_BLOCK_SIZE;
        let end = offset
            .checked_add(len)
            .ok_or_else(|| Error::with_message(Errno::EFBIG, "write range overflow"))?;
        let lblock_start = offset / block_size;
        let lblock_count = end.div_ceil(block_size) - lblock_start;
        let lblock_start_u32 = u32::try_from(lblock_start)
            .map_err(|_| Error::with_message(Errno::EFBIG, "write lblock overflow"))?;
        let lblock_count_u32 = u32::try_from(lblock_count)
            .map_err(|_| Error::with_message(Errno::EFBIG, "write lblock overflow"))?;
        let mappings = self.run_io_file_read_only(|ctx| {
            Self::core_map_blocks(ctx, ino, lblock_start_u32, lblock_count_u32)
        })?;
        let mapped_blocks: u64 = mappings.iter().map(|m| m.len as u64).sum();
        Ok(mapped_blocks == lblock_count as u64)
    }

    pub(super) fn write_page_cache_data_at(&self, ino: u32, offset: usize, data: &[u8]) -> Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }

        let block_size = EXT4_BLOCK_SIZE;
        let end = offset
            .checked_add(data.len())
            .ok_or_else(|| Error::with_message(Errno::EFBIG, "page-cache write range overflow"))?;
        let lblock_start = offset / block_size;
        let lblock_end = end.div_ceil(block_size);
        let lblock_count = lblock_end
            .checked_sub(lblock_start)
            .ok_or_else(|| Error::with_message(Errno::EFBIG, "page-cache write range overflow"))?;
        let lblock_start_u32 = u32::try_from(lblock_start)
            .map_err(|_| Error::with_message(Errno::EFBIG, "page-cache write lblock overflow"))?;
        let lblock_count_u32 = u32::try_from(lblock_count)
            .map_err(|_| Error::with_message(Errno::EFBIG, "page-cache write lblock overflow"))?;
        let mappings = self.run_io_file_read_only(|ctx| {
            Self::core_map_blocks(ctx, ino, lblock_start_u32, lblock_count_u32)
        })?;
        // BUG-5 (Task 8): write-path revoke removed — see `write_page_cache_data_at_for_inode`.
        // The mapped blocks are regular-file data (not journaled); the revoke is now driven on the
        // metadata-block FREE path inside core.

        let op = JournaledOp::Write {
            ino,
            len: data.len(),
        };
        let written = self.run_journaled_core(Some(op), ino, |ctx, alloc, inode| {
            super::core::file::write_at(ctx, alloc, inode, offset, data)
        })?;
        if self.phase2_profile_enabled {
            self.buffered_write_profile
                .writeback_bytes
                .fetch_add(written as u64, Ordering::Relaxed);
        }
        self.invalidate_direct_read_cache(ino);
        Ok(written)
    }

    pub(super) fn read_page_cache_data_at(&self, ino: u32, offset: usize, data: &mut [u8]) -> Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }

        data.fill(0);
        let file_size = self.stat(ino)?.size as usize;
        let read_len = file_size.saturating_sub(offset).min(data.len());
        if read_len == 0 {
            return Ok(0);
        }

        let block_size = EXT4_BLOCK_SIZE;
        let end = offset
            .checked_add(read_len)
            .ok_or_else(|| Error::with_message(Errno::EFBIG, "page-cache read range overflow"))?;
        let lblock_start = offset / block_size;
        let lblock_end = end.div_ceil(block_size);
        let lblock_count = lblock_end
            .checked_sub(lblock_start)
            .ok_or_else(|| Error::with_message(Errno::EFBIG, "page-cache read range overflow"))?;
        let lblock_start_u32 = u32::try_from(lblock_start)
            .map_err(|_| Error::with_message(Errno::EFBIG, "page-cache read lblock overflow"))?;
        let lblock_count_u32 = u32::try_from(lblock_count)
            .map_err(|_| Error::with_message(Errno::EFBIG, "page-cache read lblock overflow"))?;

        let mappings = self.run_io_file_read_only(|ctx| {
            Self::core_map_blocks(ctx, ino, lblock_start_u32, lblock_count_u32)
        })?;

        for mapping in mappings {
            let mapping_start_lblock = mapping.lblock as usize;
            let mapping_end_lblock = mapping_start_lblock
                .checked_add(mapping.len as usize)
                .ok_or_else(|| Error::with_message(Errno::EFBIG, "mapped range overflow"))?;
            let overlap_start_lblock = mapping_start_lblock.max(lblock_start);
            let overlap_end_lblock = mapping_end_lblock.min(lblock_end);
            if overlap_start_lblock >= overlap_end_lblock {
                continue;
            }

            for lblock in overlap_start_lblock..overlap_end_lblock {
                let pblock = mapping
                    .pblock
                    .checked_add((lblock - mapping_start_lblock) as u64)
                    .ok_or_else(|| Error::with_message(Errno::EFBIG, "mapped pblock overflow"))?;
                let pblock = usize::try_from(pblock)
                    .map_err(|_| Error::with_message(Errno::EFBIG, "mapped pblock overflow"))?;
                let block_offset = pblock
                    .checked_mul(block_size)
                    .ok_or_else(|| Error::with_message(Errno::EFBIG, "mapped offset overflow"))?;
                let file_block_start = lblock
                    .checked_mul(block_size)
                    .ok_or_else(|| Error::with_message(Errno::EFBIG, "file offset overflow"))?;
                let copy_start = file_block_start.max(offset);
                let copy_end = file_block_start
                    .checked_add(block_size)
                    .ok_or_else(|| Error::with_message(Errno::EFBIG, "file offset overflow"))?
                    .min(end);
                if copy_start >= copy_end {
                    continue;
                }

                let mut block_data = vec![0u8; block_size];
                self.adapter
                    .read_offset_into(block_offset, block_data.as_mut_slice());
                let out_start = copy_start - offset;
                let out_end = copy_end - offset;
                let block_start = copy_start - file_block_start;
                data[out_start..out_end].copy_from_slice(
                    &block_data[block_start..block_start + (copy_end - copy_start)],
                );
            }
        }

        Ok(read_len)
    }

    pub(super) fn write_direct_at(
        &self,
        ino: u32,
        offset: usize,
        reader: &mut VmReader,
    ) -> Result<usize> {
        let write_len = reader.remain();
        if write_len == 0 {
            return Ok(0);
        }

        // C1 (Phase 6): dio overwrite concurrency. Under page_cache=0, try
        // the overwrite plan while holding the per-inode lock SHARED: the
        // verified mappings cannot change concurrently (every mapping
        // mutator — prepare/truncate/fallocate — takes the lock exclusive),
        // so concurrent same-file overwrite dio proceeds in parallel through
        // bio submission instead of serializing across the device wait
        // (Linux ext4 shared-i_rwsem dio overwrite equivalent). Overlapping
        // concurrent writes to the same bytes are the application's race,
        // exactly as in Linux; filesystem metadata is untouched on this
        // path. Anything that needs the journaled prepare falls back to the
        // exclusive guard.
        let inode_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
        let mut shared_overwrite_mappings: Option<Vec<SimpleBlockRange>> = None;
        let mut shared_guard = None;
        if !self.page_cache_enabled {
            let guard = inode_lock.read();
            if let Some(mappings) =
                self.plan_direct_write_overwrite_cached(ino, offset, write_len)?
            {
                shared_overwrite_mappings = Some(mappings);
                shared_guard = Some(guard);
            }
        }
        let _shared_guard = shared_guard;
        let _excl_guard = if _shared_guard.is_none() {
            Some(inode_lock.write())
        } else {
            None
        };
        self.evict_page_cache_range(ino, offset, write_len)?;

        let profile_enabled = self.phase2_profile_enabled;
        let profile_start_ns = if profile_enabled {
            Self::monotonic_nanos()
        } else {
            0
        };
        let mut plan_elapsed_ns = 0u64;
        let mut prepare_elapsed_ns = 0u64;
        let mut data_bio_elapsed_ns = 0u64;
        let mut bio_alloc_elapsed_ns = 0u64;
        let mut bio_copy_elapsed_ns = 0u64;
        let mut bio_submit_elapsed_ns = 0u64;
        let mut bio_wait_elapsed_ns = 0u64;
        let mut touch_elapsed_ns = 0u64;
        let mut bio_call_profile = DirectWriteBioCallProfile::default();
        let user_buffer_start = reader.cursor() as Vaddr;
        let mut reused_read_mapping_cache = false;
        let mut touched_inside_write_handle = false;
        let now = Self::now_unix_seconds_u32();
        if profile_enabled {
            self.maybe_start_direct_write_profile();
        }
        let write_result = (|| -> Result<usize> {
            let plan_start_ns = if profile_enabled {
                Self::monotonic_nanos()
            } else {
                0
            };
            let mappings = if let Some(cached_mappings) = shared_overwrite_mappings.take() {
                reused_read_mapping_cache = true;
                cached_mappings
            } else {
                if profile_enabled {
                    plan_elapsed_ns = Self::monotonic_nanos().saturating_sub(plan_start_ns);
                }
                self.run_journaled_core(
                    Some(JournaledOp::Write {
                        len: write_len,
                        ino,
                    }),
                    ino,
                    |ctx, alloc, inode| {
                        let prepare_start_ns = if profile_enabled {
                            Self::monotonic_nanos()
                        } else {
                            0
                        };
                        let (_lblock_start, ranges) = super::core::file::prepare_write_at(
                            ctx, alloc, inode, offset, write_len,
                        )?;
                        Self::core_set_inode_times(ctx, inode, None, Some(now), Some(now))?;
                        touched_inside_write_handle = true;
                        if profile_enabled {
                            prepare_elapsed_ns =
                                Self::monotonic_nanos().saturating_sub(prepare_start_ns);
                        }

                        Ok(Self::core_to_integration_ranges(&ranges))
                    },
                )?
            };
            if profile_enabled && reused_read_mapping_cache {
                plan_elapsed_ns = Self::monotonic_nanos().saturating_sub(plan_start_ns);
            }

            let data_bio_start_ns = if profile_enabled {
                Self::monotonic_nanos()
            } else {
                0
            };
            // BUG-5 (Task 8): write-path revoke removed (see the slow-path write above). These are
            // regular-file data blocks (not journaled); the revoke is driven on the metadata-block
            // FREE path inside core, not here on the write/reuse path.
            // P2: the prepare (or the verified overwrite plan) covers a fully
            // written range; extend coverage for later buffered overwrites.
            self.coverage_insert_ranges(ino, &mappings);
            self.submit_direct_write_mappings(
                &mappings,
                reader,
                profile_enabled,
                &mut bio_alloc_elapsed_ns,
                &mut bio_copy_elapsed_ns,
                &mut bio_submit_elapsed_ns,
                &mut bio_wait_elapsed_ns,
                &mut bio_call_profile,
            )?;
            if profile_enabled {
                data_bio_elapsed_ns = Self::monotonic_nanos().saturating_sub(data_bio_start_ns);
            }

            Ok(write_len)
        })();

        if profile_enabled && write_result.is_ok() {
            Self::profile_direct_write_user_buffer(
                user_buffer_start,
                write_len,
                &mut bio_call_profile,
            );
        }
        if reused_read_mapping_cache && write_result.is_ok() {
            self.clear_pending_direct_read(ino);
        } else {
            self.invalidate_direct_read_cache(ino);
        }
        let result = match write_result {
            Ok(written) => {
                let touch_start_ns = if profile_enabled {
                    Self::monotonic_nanos()
                } else {
                    0
                };
                let touch_result = if touched_inside_write_handle {
                    self.inode_mtime_ctime_cache.lock().insert(ino, now);
                    Ok(())
                } else {
                    self.touch_mtime_ctime(ino)
                };
                if profile_enabled {
                    touch_elapsed_ns = Self::monotonic_nanos().saturating_sub(touch_start_ns);
                }
                touch_result?;
                Ok(written)
            }
            Err(err) => Err(err),
        };
        if result.is_ok() {
            self.discard_page_cache_range(ino, offset, write_len);
        }
        if profile_enabled {
            let writes = self.direct_write_profile.record_write(
                write_len,
                reused_read_mapping_cache,
                result.is_ok(),
                &bio_call_profile,
                plan_elapsed_ns,
                prepare_elapsed_ns,
                data_bio_elapsed_ns,
                bio_alloc_elapsed_ns,
                bio_copy_elapsed_ns,
                bio_submit_elapsed_ns,
                bio_wait_elapsed_ns,
                touch_elapsed_ns,
                Self::monotonic_nanos().saturating_sub(profile_start_ns),
            );
            self.maybe_log_direct_write_profile(writes, false);
        }
        result
    }

    pub(super) fn truncate(&self, ino: u32, new_size: u64) -> Result<()> {
        let inode_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
        let _inode_guard = inode_lock.write();
        let seq = GENERIC014_TRUNCATE_PROGRESS.fetch_add(1, Ordering::Relaxed) + 1;
        if seq <= 8 || seq % GENERIC014_PROGRESS_LOG_INTERVAL == 0 {
            debug!(
                "ext4: generic014-like truncate progress seq={} ino={} new_size={}",
                seq, ino, new_size
            );
        }
        self.sync_page_cache_for_inode_locked(ino)?;
        let now = Self::now_unix_seconds_u32();
        let op = JournaledOp::Truncate { ino };
        let truncate_result = self
            .run_journaled_core(Some(op), ino, |ctx, alloc, inode| {
                super::core::file::truncate_inode(ctx, alloc, inode, new_size)?;
                Self::core_set_inode_times(ctx, inode, None, Some(now), Some(now))?;
                Ok(())
            })
            .map_err(|err| {
                error!(
                    "ext4 truncate failed: ino={} new_size={} err={:?}",
                    ino, new_size, err
                );
                err
            });
        self.invalidate_direct_read_cache(ino);
        truncate_result?;
        self.reset_page_cache_after_truncate(ino, new_size as usize)?;
        self.inode_mtime_ctime_cache.lock().insert(ino, now);
        Ok(())
    }

    pub(super) fn fallocate(
        &self,
        ino: u32,
        mode: FallocMode,
        offset: usize,
        len: usize,
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }

        let inode_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
        let _inode_guard = inode_lock.write();
        self.evict_page_cache_range(ino, offset, len)?;

        let now = Self::now_unix_seconds_u32();
        let op = JournaledOp::Write { ino, len };
        self.run_journaled_core(Some(op), ino, |ctx, alloc, inode| {
            match mode {
                FallocMode::Allocate => {
                    super::core::file::allocate_range(ctx, alloc, inode, offset, len, false)?;
                }
                FallocMode::AllocateKeepSize => {
                    super::core::file::allocate_range(ctx, alloc, inode, offset, len, true)?;
                }
                FallocMode::ZeroRange => {
                    super::core::file::zero_range(ctx, alloc, inode, offset, len, false)?;
                }
                FallocMode::ZeroRangeKeepSize => {
                    super::core::file::zero_range(ctx, alloc, inode, offset, len, true)?;
                }
                FallocMode::PunchHoleKeepSize => {
                    super::core::file::punch_hole_keep_size(ctx, alloc, inode, offset, len)?;
                }
                FallocMode::CollapseRange
                | FallocMode::InsertRange
                | FallocMode::AllocateUnshareRange => {
                    return_errno_with_message!(
                        Errno::EOPNOTSUPP,
                        "ext4 fallocate mode is not supported"
                    );
                }
            }
            Self::core_set_inode_times(ctx, inode, None, Some(now), Some(now))?;
            Ok(())
        })
        .map_err(|err| {
            if err.error() == Errno::EOPNOTSUPP {
                debug!(
                    "ext4 fallocate unsupported: ino={} mode={:?} offset={} len={}",
                    ino, mode, offset, len
                );
            } else {
                error!(
                    "ext4 fallocate failed: ino={} mode={:?} offset={} len={} err={:?}",
                    ino, mode, offset, len, err
                );
            }
            err
        })?;

        self.invalidate_direct_read_cache(ino);
        self.evict_page_cache_range(ino, offset, len)?;
        let file_size = self.stat(ino)?.size as usize;
        if let Some(state) = self.page_cache_state_if_present(ino) {
            state.resize(file_size)?;
        }
        self.inode_mtime_ctime_cache.lock().insert(ino, now);
        Ok(())
    }

    fn readdir_locked(&self, ino: u32) -> Result<Vec<SimpleDirEntry>> {
        if let Some(entries) = self.loaded_dir_cache_entries(ino) {
            return Ok(entries);
        }

        if self.load_dir_cache_if_needed_locked(ino).is_ok() {
            if let Some(entries) = self.loaded_dir_cache_entries(ino) {
                return Ok(entries);
            }
        }

        // Phase 6 Task 1: enumerate directory entries via core, byte-identically to ext4_rs
        // `ext4_readdir`. ext4_rs returns an empty vec for a non-directory inode (`!is_dir`); core's
        // `dir_get_entries_with_next_offset` does not gate on type, so we replicate the guard here.
        // Each `SimpleDirEntry` mirrors the ext4_rs build one-for-one:
        //   - `inode`     = entry inode (unused/inode==0 slots are skipped by core, matching ext4_rs)
        //   - `de_type`   = dirent file-type byte (core `OwnedDirEntry.file_type` == ext4_rs
        //                   `get_de_type()`, the same union byte)
        //   - `name`      = `String::from_utf8_lossy(name)` (exactly ext4_rs `get_name()`)
        //   - `next_offset` = `iblock*bs + off + rec_len` (core computes the identical cookie)
        self.run_io_dir_read_only_noerr(|ctx| {
            let Ok(dir) = super::core::inode::load_inode(ctx.reader, ctx.sb, ino) else {
                return Vec::new();
            };
            if !dir.is_dir() {
                return Vec::new();
            }
            super::core::dir::dir_get_entries_with_next_offset(ctx, &dir)
                .into_iter()
                .map(|(entry, next_offset)| SimpleDirEntry {
                    inode: entry.inode,
                    de_type: entry.file_type,
                    name: String::from_utf8_lossy(&entry.name).into_owned(),
                    next_offset,
                })
                .collect()
        })
    }

    pub(super) fn readdir(&self, ino: u32) -> Result<Vec<SimpleDirEntry>> {
        let dir_lock = Self::correctness_lock_for(&self.dir_correctness_locks, ino);
        let _dir_guard = dir_lock.write();
        self.readdir_locked(ino)
    }

    pub(super) fn dev_id(&self) -> u64 {
        self.block_device.id().as_encoded_u64()
    }

    /// Returns a reference to the underlying block device.
    ///
    /// Used by `Ext4Inode::sync_all` / `sync_data` to issue a final
    /// `BlockDevice::sync()` after `fsync_regular_file()`, mirroring the ext2
    /// `impl_for_vfs/inode.rs` pattern (Step 4a-1).
    pub(super) fn block_device(&self) -> &Arc<dyn BlockDevice> {
        &self.block_device
    }

    /// Step 4b: returns true if the filesystem has been shut down via
    /// `EXT4_IOC_SHUTDOWN`.  After shutdown, journaled operations and
    /// fsync return EIO.  Reads still work in v1 (Linux returns EIO on
    /// reads too, but we accept the slightly looser semantics for now;
    /// the xfstests use shutdown only as a "stop writing then unmount"
    /// barrier and do not read after shutdown).
    pub(super) fn is_shutdown(&self) -> bool {
        self.shutdown_state.load(Ordering::Acquire) != 0
    }

    pub(super) fn check_not_shutdown(&self) -> Result<()> {
        if self.is_shutdown() {
            return_errno_with_message!(Errno::EIO, "ext4: filesystem is shutdown");
        }
        Ok(())
    }

    /// Step 4b: implements the `EXT4_IOC_SHUTDOWN` ioctl.
    ///
    /// Three flag values follow Linux ext4 semantics:
    ///   - `EXT4_GOING_FLAGS_DEFAULT (0x0)`: best-effort sync of dirty
    ///     metadata, then forced shutdown.  Implemented as `Ext4Fs::sync()`
    ///     followed by setting the shutdown bit (slightly more conservative
    ///     than Linux, which does not actively force-commit on DEFAULT).
    ///   - `EXT4_GOING_FLAGS_LOGFLUSH (0x1)`: force-commit all pending
    ///     transactions, flush the journal and the device, then shutdown.
    ///     This is the "clean-ish" variant.
    ///   - `EXT4_GOING_FLAGS_NOLOGFLUSH (0x2)`: discard in-flight commits,
    ///     no flush, no journal sync.  This is the strongest crash
    ///     simulation — equivalent to a hard power-cut at the moment of
    ///     the ioctl.  After this, the on-disk state is whatever was
    ///     already persisted; remount must replay the journal.
    ///
    /// In all three cases, after this call returns, all subsequent
    /// journaled operations and fsync return EIO until remount.
    pub(super) fn shutdown(&self, flag: u32) -> Result<()> {
        const EXT4_GOING_FLAGS_DEFAULT: u32 = 0x0;
        const EXT4_GOING_FLAGS_LOGFLUSH: u32 = 0x1;
        const EXT4_GOING_FLAGS_NOLOGFLUSH: u32 = 0x2;

        // Idempotent: a second shutdown call is a no-op.
        if self.is_shutdown() {
            return Ok(());
        }

        match flag {
            EXT4_GOING_FLAGS_NOLOGFLUSH => {
                // Hard crash simulation: do NOT flush the journal.  Just
                // mark the FS as shutdown.  Any pending JBD2 transactions
                // on disk are left in their current state; remount will
                // see needs_recovery (s_start != 0) and replay.
                warn!("ext4: shutdown NOLOGFLUSH (hard crash simulation)");
            }
            EXT4_GOING_FLAGS_LOGFLUSH | EXT4_GOING_FLAGS_DEFAULT => {
                // Clean-ish shutdown: force-commit and flush journal +
                // device first.  The existing FileSystem::sync() path
                // already does flush_pending_jbd2_transactions +
                // block_device.sync().
                warn!(
                    "ext4: shutdown {} — force-commit + flush before mark",
                    if flag == EXT4_GOING_FLAGS_LOGFLUSH {
                        "LOGFLUSH"
                    } else {
                        "DEFAULT"
                    }
                );
                if let Err(err) = self.do_filesystem_sync_unchecked() {
                    warn!("ext4: shutdown sync failed (continuing anyway): {:?}", err);
                }
            }
            _ => {
                return_errno_with_message!(Errno::EINVAL, "ext4: unsupported shutdown flag");
            }
        }

        // Step 4b: ensure dumpe2fs / e2fsprogs see "needs_recovery" after a
        // forced shutdown.  This is what generic/052/054/055 (and Linux ext4
        // mount-time pessimistic flag) rely on.  We persist the flag here
        // unconditionally for ALL shutdown flags — even LOGFLUSH leaves
        // EXT4_FEATURE_INCOMPAT_RECOVER set in Linux because LOGFLUSH only
        // flushes the journal area, it does not perform a clean unmount.
        self.mark_needs_recovery_for_shutdown();

        self.shutdown_state.store(1, Ordering::Release);
        Ok(())
    }

    /// Step 4b: force-write `EXT4_FEATURE_INCOMPAT_RECOVER` to the on-disk
    /// superblock and flush.  Called from `shutdown()` so dumpe2fs reports
    /// "dirty log" after `EXT4_IOC_SHUTDOWN`, regardless of which flag was
    /// used.  Bypasses `JournalIoBridge` deferral by routing the write
    /// through a raw adapter wrapper so the value reaches the disk even
    /// on the NOLOGFLUSH (no journal commit) path.
    fn mark_needs_recovery_for_shutdown(&self) {
        // Lightweight metadata writer that bypasses the journal overlay
        // and writes directly via the underlying block adapter.
        struct RawAdapterWriter(Arc<KernelBlockDeviceAdapter>);
        impl Ext4MetadataWriter for RawAdapterWriter {
            fn write_metadata(&self, offset: usize, data: &[u8]) {
                self.0.write_offset(offset, data);
            }
            fn write_metadata_for_jbd2_handle(
                &self,
                _handle_id: Option<u64>,
                offset: usize,
                data: &[u8],
            ) {
                self.0.write_offset(offset, data);
            }
        }
        let raw_writer = RawAdapterWriter(self.adapter.clone());

        // Phase 6 Task 4: set the core-owned RECOVER flag + persist a `core_sb`-based SB image
        // (mount-time free counts, RECOVER bit set, csum recomputed) straight to the home superblock
        // via the raw adapter writer. PARITY: ext4_rs `inner.super_block.set_needs_recovery(true) +
        // sync_to_disk_with_csum(&RawAdapterWriter)`.
        self.persist_recover_flag(true, &raw_writer);
        // Flush so dumpe2fs / next mount sees the updated superblock.
        let _ = self.block_device.sync();
    }

    /// Internal helper: same as `FileSystem::sync()` but does not check
    /// the shutdown bit (used during shutdown itself).
    fn do_filesystem_sync_unchecked(&self) -> Result<()> {
        self.flush_pending_jbd2_transactions();
        self.block_device.sync()?;
        self.flush_pending_jbd2_transactions();
        Ok(())
    }

    /// Step 4b: lazily set the on-disk superblock's `needs_recovery` flag
    /// (`EXT4_FEATURE_INCOMPAT_RECOVER`) on first journal commit since the
    /// flag was last clean.  After this, dumpe2fs-style probes will report
    /// the FS as "dirty log" until the next clean shutdown / replay clears
    /// the flag.  No-op if the flag is already set (cheap fast path).
    pub(super) fn mark_needs_recovery_if_needed(&self) {
        // Phase 6 Task 4: fast path on the core-owned in-memory RECOVER flag. PARITY: ext4_rs
        // checked `!inner.super_block.needs_recovery()` (the in-memory bit) before persisting.
        if self.recover_flag.load(Ordering::Acquire) {
            return;
        }
        // Persist the SB with RECOVER set through the journal bridge metadata writer. This is called
        // post-commit with no active handle, so the bridge write goes straight to the home superblock
        // — byte-frozen vs ext4_rs `inner.super_block.sync_to_disk_with_csum(&inner.metadata_writer)`.
        let writer: Arc<dyn Ext4MetadataWriter> = self.journal_io.clone();
        self.persist_recover_flag(true, writer.as_ref());
    }

    pub(super) fn fsync_regular_file(&self, ino: u32) -> Result<()> {
        // Step 4b: post-shutdown fsync must return EIO so that callers
        // (e.g. xfstests after godown) know that the filesystem is dead.
        self.check_not_shutdown()?;

        let inode_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
        let _inode_guard = inode_lock.write();

        // Step 4a-2: look up the highest TID that contains a metadata change
        // for this inode (recorded by `finish_jbd2_handle` after Write/Truncate).
        // If `None`, the inode has no recorded metadata changes and fsync is
        // a no-op for the journal — the VFS-layer device flush in
        // `Ext4Inode::sync_all` (Step 4a-1) still runs to handle any
        // already-issued data writes from prior writers.
        let target_tid = self.lookup_inode_tid(ino);
        let committed = self.last_committed_tid();

        // Step 1 observation: log fsync entry state for diagnostic purposes.
        warn!(
            "ext4: fsync ino={} target_tid={:?} committed_tid={}",
            ino, target_tid, committed
        );

        if let Some(target_tid) = target_tid {
            // Step 4a-2: force-commit the target TID. Internally:
            //   - Fast path returns if already committed.
            //   - If target is the running TX, rotate it to prev_running.
            //   - Drive `try_commit_ready_jbd2_transaction()` and block on
            //     `commit_notifier` until last_committed_tid >= target_tid.
            // This replaces the previous best-effort
            // `commit_pending_jbd2_transactions()` calls, which silently
            // no-op'd when commit_ready=false (e.g. when other workers held
            // active handles) — a POSIX violation.
            self.force_commit_for_tid(target_tid);
        }

        // Lazy checkpoint: only when journal pressure is high. Checkpoint
        // writes home blocks back from the journal area, freeing journal
        // space; not strictly required for fsync correctness (replay handles
        // it on crash). Phase 2 batch_checkpoint policy preserved.
        if self.checkpoint_depth() >= REGULAR_FILE_FSYNC_CHECKPOINT_DEPTH {
            self.try_batch_checkpoint_all_jbd2_transactions();
        }
        Ok(())
    }

    pub(super) fn this(&self) -> Arc<Self> {
        self.self_ref.upgrade().unwrap()
    }

    pub(super) fn make_inode(self: &Arc<Self>, ino: u32, path: String) -> Arc<dyn Inode> {
        Arc::new(super::inode::Ext4Inode::new(
            Arc::downgrade(self),
            ino,
            path,
        ))
    }
}

impl FileSystem for Ext4Fs {
    fn name(&self) -> &'static str {
        "ext4"
    }

    fn sync(&self) -> Result<()> {
        // Step 4b: after `EXT4_IOC_SHUTDOWN`, sync() is a no-op.  Especially
        // important for NOLOGFLUSH (hard crash simulation) — we must NOT
        // sneak in commits on subsequent unmount/syncfs.  For LOGFLUSH /
        // DEFAULT, the sync was already done as part of the ioctl.
        if self.is_shutdown() {
            return Ok(());
        }
        // BUG-19: reclaim inodes whose last handle closed in an atomic context (deferred by
        // `on_close_file_handle`) before flushing, so their frees are journaled + committed by this
        // sync. `sync()` is sleepable, so the blocking reclaim is safe here.
        self.reclaim_pending_inode_frees();
        self.sync_all_page_caches()?;
        self.flush_pending_jbd2_transactions();
        self.block_device.sync()?;
        self.flush_pending_jbd2_transactions();
        // Phase 5: emit one complete latency-attribution snapshot at the end of
        // a benchmark run (syncfs / unmount). No-op unless ext4fs.phase2_profile=1.
        self.dump_perf_summary();
        Ok(())
    }

    fn root_inode(&self) -> Arc<dyn Inode> {
        self.this().make_inode(EXT4_ROOT_INODE, String::new())
    }

    fn sb(&self) -> SuperBlock {
        // Phase 6 Task 4: read the FROZEN mount-time snapshot `core_sb` (byte-frozen behavior — the
        // old `inner.super_block` was likewise the frozen mount snapshot, so statfs reports mount-time
        // free counts, NOT the live `running_sb` counts). All fields here are immutable geometry or
        // the frozen free counts; identical to what ext4_rs `inner.super_block` returned.
        let ext4_sb = &self.core_sb;
        let block_size = ext4_sb.block_size();
        let blocks = ext4_sb.blocks_count() as usize;
        let bfree = ext4_sb.free_blocks_count().min(usize::MAX as u64) as usize;
        let files = ext4_sb.inodes_count() as usize;
        let ffree = ext4_sb.free_inodes_count() as usize;
        let uuid = ext4_sb.uuid();
        let fsid = u64::from_le_bytes(uuid[..8].try_into().unwrap_or([0u8; 8]));

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

    fn set_mount_flags(&self, mount_flags_bits: u32) {
        self.mount_flags_bits
            .store(mount_flags_bits, Ordering::Relaxed);
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

    let desc_size_on_disk = u16::from_le_bytes([
        superblock_sector[EXT4_SB_DESC_SIZE_OFFSET],
        superblock_sector[EXT4_SB_DESC_SIZE_OFFSET + 1],
    ]);
    // Legacy ext4 may store s_desc_size as 0, which means 32-byte descriptors.
    let desc_size = if desc_size_on_disk == 0 {
        32usize
    } else {
        desc_size_on_disk as usize
    };
    if desc_size < 32 || desc_size > block_size || (block_size % desc_size) != 0 {
        return_errno_with_message!(Errno::EINVAL, "unsupported ext4 group descriptor size");
    }

    Ok(())
}
