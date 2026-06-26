// SPDX-License-Identifier: MPL-2.0

use alloc::{
    collections::{BTreeMap, BTreeSet},
    string::String,
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use aster_block::{BlockDevice, SECTOR_SIZE, bio::set_write_bio_profile_enabled};
use aster_cmdline::{KCMDLINE, ModuleArg};
use aster_time::read_monotonic_time;
// Phase 6 Task 5b: the integration-layer ext4 types (mode bits, root inode, block size, `Simple*`
// DTOs, metadata-writer seam trait) now come from the in-tree `super::types` module and `core/`
// instead of the deleted third-party `ext4_rs` crate. Byte-identical replacements throughout.
use super::core::alloc_guard::LocalOperationAllocGuard;
use super::core::file::SimpleBlockRange;
use super::types::SimpleInodeMeta;
use ostd::{
    Error as OstdError,
    mm::{VmIo, VmWriter},
    sync::{RwMutex, WaitQueue},
};

use crate::{
    fs::{
        path::PerMountFlags,
        registry::{FsProperties, FsType},
        utils::{FileSystem, FsEventSubscriberStats, FsFlags, Inode},
    },
    prelude::*,
};

// Phase 8 move-only split: items relocated to sibling modules (leaf structs/consts) are referenced
// back here by the `impl Ext4Fs` methods that still live in `fs.rs`. No behavior change.
use super::caches::{DirEntryCache, DirectReadCache, ExtentMapCacheEntry, WrittenCoverage};
use super::device_adapter::KernelBlockDeviceAdapter;
use super::journal_driver::JournalIoBridge;
use super::page_cache::Ext4PageCacheState;
use super::profile::{
    BufferedWriteProfileStats, DirectReadProfileStats, DirectWriteProfileStats,
    Ext4RsRuntimeLockStats, FsyncProfileStats, JournaledOpProfileStats,
};

pub(super) const EXT4_MAGIC: u64 = 0xEF53;

pub(super) const EXT4_SUPERBLOCK_OFFSET: usize = 1024;
const EXT4_SB_LOG_BLOCK_SIZE_OFFSET: usize = 24;
const EXT4_SB_BLOCKS_PER_GROUP_OFFSET: usize = 32;
const EXT4_SB_INODES_PER_GROUP_OFFSET: usize = 40;
const EXT4_SB_MAGIC_OFFSET: usize = 56;
const EXT4_SB_DESC_SIZE_OFFSET: usize = 254;

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

    pub(super) fn this(&self) -> Arc<Self> {
        self.self_ref.upgrade().unwrap()
    }

    pub(super) fn make_inode(self: &Arc<Self>, ino: u32, path: String) -> Arc<dyn Inode> {
        Arc::new(super::impl_for_vfs::Ext4Inode::new(
            Arc::downgrade(self),
            ino,
            path,
        ))
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
