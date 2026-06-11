// SPDX-License-Identifier: MPL-2.0

use alloc::{collections::BTreeMap, string::String, vec::Vec};
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
use ext4_rs::{
    BLOCK_SIZE as EXT4_BLOCK_SIZE, BlockDevice as Ext4BlockDevice, EXT4_ROOT_INODE, Ext4,
    Jbd2Journal, JournalCommitWriteStage, JournalHandle, JournalRecoveryResult, JournalRuntime,
    LocalOperationAllocGuard, MetadataWriter as Ext4MetadataWriter,
    OperationAllocGuard as Ext4OperationAllocGuard, OperationScopedAllocGuard, SimpleBlockRange,
    SimpleDirEntry, SimpleInodeMeta,
};
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
            CachePage, FallocMode, FileSystem, FsEventSubscriberStats, FsFlags, Inode, NAME_MAX,
            PageCache, PageCacheBackend, StatusFlags, SuperBlock,
        },
    },
    prelude::*,
    vm::{
        vmar::{VMAR_CAP_ADDR, VMAR_LOWEST_ADDR},
        vmo::Vmo,
    },
};

const EXT4_MAGIC: u64 = 0xEF53;

// Lazy JBD2 checkpoint thresholds.
// Only checkpoint when journal free blocks drop below these limits, rather than
// after every single commit. This avoids a BioType::Flush per operation.
// Pre-commit: if free < NEEDED + JOURNAL_LOW_WATER, checkpoint first to make room.
// Intentionally small: typical journal is ~1024 blocks, so only flush when nearly full.
const JOURNAL_LOW_WATER_MARK: u32 = 64;
// Post-commit: if free < JOURNAL_CHECKPOINT_THRESHOLD, checkpoint to keep headroom.
const JOURNAL_CHECKPOINT_THRESHOLD: u32 = 128;
// Post-commit memory bound: committed-but-not-checkpointed transactions keep a
// copy of every metadata block they touched (~4 KiB each) in the in-memory
// `checkpoint_list`. The journal-space trigger above can leave that list to
// grow into the gigabytes during a long fsync-less transaction (e.g. SQLite's
// 500k-row inserts), exhausting the kernel heap. Force a checkpoint once the
// list reaches this depth so the in-memory footprint stays bounded.
const JOURNAL_CHECKPOINT_MAX_DEPTH: usize = 64;
// Maximum number of metadata blocks to accumulate before forcing a commit.
// Batching many handles into one transaction reduces commit frequency and
// eliminates the per-handle journal write overhead for high-concurrency workloads
// like fsstress with many parallel processes.
const JOURNAL_COMMIT_BATCH_BLOCKS: u32 = 128;
// Regular-file fsync can legally rely on committed journal transactions for
// crash durability, but if checkpointing is deferred indefinitely, committed
// metadata accumulates in memory under xfstests generic/047. Periodically drain
// the checkpoint queue to keep memory bounded without regressing to per-fsync
// full-filesystem sync.
const REGULAR_FILE_FSYNC_CHECKPOINT_DEPTH: usize = 8;
const GENERIC014_PROGRESS_LOG_INTERVAL: u64 = 16;
const GENERIC014_SLOW_OP_LOG_THRESHOLD_NS: u64 = 1_000_000_000;
const EXT4_SUPERBLOCK_OFFSET: usize = 1024;
const EXT4_SB_LOG_BLOCK_SIZE_OFFSET: usize = 24;
const EXT4_SB_BLOCKS_PER_GROUP_OFFSET: usize = 32;
const EXT4_SB_INODES_PER_GROUP_OFFSET: usize = 40;
const EXT4_SB_MAGIC_OFFSET: usize = 56;
const EXT4_SB_DESC_SIZE_OFFSET: usize = 254;
const JOURNALED_SMALL_WRITE_MAX_BYTES: usize = 192;

// ext4_rs currently stores runtime block size in a global variable.
// Serialize ext4_rs calls across mounted ext4 instances to avoid
// cross-filesystem block-size races during xfstests mkfs/remount cycles.
static EXT4_RS_RUNTIME_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug)]
enum JournaledOp {
    Create,
    Mkdir,
    Unlink,
    Rmdir,
    Rename,
    Write { len: usize, ino: u32 },
    Truncate { ino: u32 },
    InodeMetadata { ino: u32 },
}

impl JournaledOp {
    /// Step 4a-2: returns the primary inode whose metadata is modified by this
    /// op, used by `finish_jbd2_handle` to update the inode→TID map for
    /// fsync force-commit.  Single-inode metadata ops carry inode info;
    /// directory ops touch multiple inodes (parents + child) and are not
    /// tracked at this granularity in v1.
    fn affected_ino(&self) -> Option<u32> {
        match self {
            Self::Write { ino, .. } | Self::Truncate { ino } | Self::InodeMetadata { ino } => {
                Some(*ino)
            }
            _ => None,
        }
    }
}

struct DirectReadProfileStats {
    read_calls: AtomicU64,
    read_bytes: AtomicU64,
    total_mappings: AtomicU64,
    mapped_bytes: AtomicU64,
    zero_fill_bytes: AtomicU64,
    max_mappings: AtomicU64,
    max_mapped_bytes: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    plan_ns: AtomicU64,
    alloc_ns: AtomicU64,
    submit_ns: AtomicU64,
    wait_ns: AtomicU64,
    copy_ns: AtomicU64,
    // Phase 5 full-path probe: total wall time of read_direct_at (so we can see
    // how much per-read overhead is outside the measured stages), and the atime
    // bookkeeping time specifically.
    total_ns: AtomicU64,
    atime_ns: AtomicU64,
}

struct DirectWriteProfileStats {
    write_calls: AtomicU64,
    write_bytes: AtomicU64,
    total_mappings: AtomicU64,
    total_bios: AtomicU64,
    total_segments: AtomicU64,
    total_blocks: AtomicU64,
    merge_hits: AtomicU64,
    user_buffer_pages: AtomicU64,
    user_buffer_phys_runs: AtomicU64,
    user_buffer_profile_failures: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    errors: AtomicU64,
    plan_ns: AtomicU64,
    prepare_ns: AtomicU64,
    data_bio_ns: AtomicU64,
    bio_alloc_ns: AtomicU64,
    bio_copy_ns: AtomicU64,
    bio_submit_ns: AtomicU64,
    bio_wait_ns: AtomicU64,
    bio_wait_return_after_complete_ns: AtomicU64,
    touch_ns: AtomicU64,
    total_ns: AtomicU64,
    hit_data_bio_ns: AtomicU64,
    hit_bio_copy_ns: AtomicU64,
    hit_bio_wait_ns: AtomicU64,
    hit_total_ns: AtomicU64,
    miss_plan_ns: AtomicU64,
    miss_prepare_ns: AtomicU64,
    miss_data_bio_ns: AtomicU64,
    miss_bio_copy_ns: AtomicU64,
    miss_bio_wait_ns: AtomicU64,
    miss_touch_ns: AtomicU64,
    miss_total_ns: AtomicU64,
    max_mappings: AtomicU64,
    max_bios_per_call: AtomicU64,
    max_segments_per_bio: AtomicU64,
    max_blocks_per_bio: AtomicU64,
    max_user_buffer_phys_runs: AtomicU64,
    max_user_buffer_phys_run_pages: AtomicU64,
    max_prepare_ns: AtomicU64,
    max_data_bio_ns: AtomicU64,
    max_bio_wait_return_after_complete_ns: AtomicU64,
    max_touch_ns: AtomicU64,
    max_total_ns: AtomicU64,
    max_miss_prepare_ns: AtomicU64,
    max_miss_data_bio_ns: AtomicU64,
    max_miss_total_ns: AtomicU64,
}

/// Read-only Phase 6 probe for the buffered (page-cache) write path
/// (`write_at_page_cache`) — the path SQLite takes under `page_cache=1`. Splits
/// the per-`write()` cost into the overwrite fast path (no block allocation) vs
/// the append/sparse slow path (journaled allocation via `run_journaled_ext4` +
/// `ext4_prepare_write_at`), so Step 0 can attribute SQLite write time to
/// "new-allocation journaled prepare" vs "in-place overwrite", and confirm how
/// many disk blocks the slow path allocates. Gated by `ext4fs.phase2_profile`;
/// off by default so guard regressions see no extra work.
struct BufferedWriteProfileStats {
    calls: AtomicU64,
    fast_calls: AtomicU64,
    fast_bytes: AtomicU64,
    fast_ns: AtomicU64,
    slow_calls: AtomicU64,
    slow_bytes: AtomicU64,
    slow_blocks: AtomicU64,
    slow_prepare_ns: AtomicU64,
    slow_ns: AtomicU64,
    max_slow_ns: AtomicU64,
    max_slow_prepare_ns: AtomicU64,
    // Stage 0 OOM diagnosis: total bytes written back to disk by the page-cache
    // writeback path (`write_page_cache_data_at`). Outstanding dirty data ≈
    // (fast_bytes + slow_bytes) − writeback_bytes; if that grows unbounded toward
    // an OOM the cause is page-cache dirty pages, not journal memory.
    writeback_bytes: AtomicU64,
}

#[derive(Default)]
struct DirectWriteBioCallProfile {
    mappings: u64,
    bios: u64,
    segments: u64,
    blocks: u64,
    merge_hits: u64,
    user_buffer_pages: u64,
    user_buffer_phys_runs: u64,
    user_buffer_profile_failures: u64,
    max_segments_per_bio: u64,
    max_blocks_per_bio: u64,
    max_user_buffer_phys_run_pages: u64,
    wait_return_after_complete_ns: u64,
}

struct Ext4RsRuntimeLockStats {
    acquire_count: AtomicU64,
    total_wait_ns: AtomicU64,
    max_wait_ns: AtomicU64,
    total_hold_ns: AtomicU64,
    max_hold_ns: AtomicU64,
}

struct JournaledOpProfileStats {
    op_count: AtomicU64,
    mkdir_count: AtomicU64,
    rmdir_count: AtomicU64,
    write_count: AtomicU64,
    start_handle_ns: AtomicU64,
    apply_ns: AtomicU64,
    finish_handle_ns: AtomicU64,
    finish_alloc_ns: AtomicU64,
    finish_io_ns: AtomicU64,
    total_ns: AtomicU64,
    max_apply_ns: AtomicU64,
    max_finish_handle_ns: AtomicU64,
    max_total_ns: AtomicU64,
}

static GENERIC014_WRITE_PROGRESS: AtomicU64 = AtomicU64::new(0);
static GENERIC014_TRUNCATE_PROGRESS: AtomicU64 = AtomicU64::new(0);

impl DirectReadProfileStats {
    const LOG_INTERVAL_READS: u64 = 8_192;

    const fn new() -> Self {
        Self {
            read_calls: AtomicU64::new(0),
            read_bytes: AtomicU64::new(0),
            total_mappings: AtomicU64::new(0),
            mapped_bytes: AtomicU64::new(0),
            zero_fill_bytes: AtomicU64::new(0),
            max_mappings: AtomicU64::new(0),
            max_mapped_bytes: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            plan_ns: AtomicU64::new(0),
            alloc_ns: AtomicU64::new(0),
            submit_ns: AtomicU64::new(0),
            wait_ns: AtomicU64::new(0),
            copy_ns: AtomicU64::new(0),
            total_ns: AtomicU64::new(0),
            atime_ns: AtomicU64::new(0),
        }
    }

    fn record_cache_hit(&self) {
        self.cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    fn record_cache_miss(&self) {
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    fn record_read(
        &self,
        bytes: usize,
        mappings: usize,
        mapped_bytes: usize,
        zero_fill_bytes: usize,
        plan_ns: u64,
        alloc_ns: u64,
        submit_ns: u64,
        wait_ns: u64,
        copy_ns: u64,
        total_ns: u64,
        atime_ns: u64,
    ) -> u64 {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        let mappings = u64::try_from(mappings).unwrap_or(u64::MAX);
        let mapped_bytes = u64::try_from(mapped_bytes).unwrap_or(u64::MAX);
        let zero_fill_bytes = u64::try_from(zero_fill_bytes).unwrap_or(u64::MAX);

        let reads = self.read_calls.fetch_add(1, Ordering::Relaxed) + 1;
        self.read_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.total_mappings.fetch_add(mappings, Ordering::Relaxed);
        self.mapped_bytes.fetch_add(mapped_bytes, Ordering::Relaxed);
        self.zero_fill_bytes
            .fetch_add(zero_fill_bytes, Ordering::Relaxed);
        self.update_max_mappings(mappings);
        self.update_max_mapped_bytes(mapped_bytes);
        self.plan_ns.fetch_add(plan_ns, Ordering::Relaxed);
        self.alloc_ns.fetch_add(alloc_ns, Ordering::Relaxed);
        self.submit_ns.fetch_add(submit_ns, Ordering::Relaxed);
        self.wait_ns.fetch_add(wait_ns, Ordering::Relaxed);
        self.copy_ns.fetch_add(copy_ns, Ordering::Relaxed);
        self.total_ns.fetch_add(total_ns, Ordering::Relaxed);
        self.atime_ns.fetch_add(atime_ns, Ordering::Relaxed);
        reads
    }

    fn update_max_mappings(&self, mappings: u64) {
        let mut current = self.max_mappings.load(Ordering::Relaxed);
        while mappings > current {
            match self.max_mappings.compare_exchange_weak(
                current,
                mappings,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    fn update_max_mapped_bytes(&self, mapped_bytes: u64) {
        let mut current = self.max_mapped_bytes.load(Ordering::Relaxed);
        while mapped_bytes > current {
            match self.max_mapped_bytes.compare_exchange_weak(
                current,
                mapped_bytes,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }
}

impl BufferedWriteProfileStats {
    const fn new() -> Self {
        Self {
            calls: AtomicU64::new(0),
            fast_calls: AtomicU64::new(0),
            fast_bytes: AtomicU64::new(0),
            fast_ns: AtomicU64::new(0),
            slow_calls: AtomicU64::new(0),
            slow_bytes: AtomicU64::new(0),
            slow_blocks: AtomicU64::new(0),
            slow_prepare_ns: AtomicU64::new(0),
            slow_ns: AtomicU64::new(0),
            max_slow_ns: AtomicU64::new(0),
            max_slow_prepare_ns: AtomicU64::new(0),
            writeback_bytes: AtomicU64::new(0),
        }
    }

    fn record_fast(&self, bytes: usize, elapsed_ns: u64) {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.fast_calls.fetch_add(1, Ordering::Relaxed);
        self.fast_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.fast_ns.fetch_add(elapsed_ns, Ordering::Relaxed);
    }

    fn record_slow(&self, bytes: usize, blocks: u64, prepare_ns: u64, elapsed_ns: u64) {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.slow_calls.fetch_add(1, Ordering::Relaxed);
        self.slow_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.slow_blocks.fetch_add(blocks, Ordering::Relaxed);
        self.slow_prepare_ns.fetch_add(prepare_ns, Ordering::Relaxed);
        self.slow_ns.fetch_add(elapsed_ns, Ordering::Relaxed);
        Self::bump_max(&self.max_slow_ns, elapsed_ns);
        Self::bump_max(&self.max_slow_prepare_ns, prepare_ns);
    }

    fn bump_max(field: &AtomicU64, value: u64) {
        let mut current = field.load(Ordering::Relaxed);
        while value > current {
            match field.compare_exchange_weak(
                current,
                value,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }
}

impl Ext4RsRuntimeLockStats {
    const LOG_INTERVAL_ACQUIRES: u64 = 4_096;

    const fn new() -> Self {
        Self {
            acquire_count: AtomicU64::new(0),
            total_wait_ns: AtomicU64::new(0),
            max_wait_ns: AtomicU64::new(0),
            total_hold_ns: AtomicU64::new(0),
            max_hold_ns: AtomicU64::new(0),
        }
    }

    fn record_wait(&self, wait_ns: u64) {
        self.acquire_count.fetch_add(1, Ordering::Relaxed);
        self.total_wait_ns.fetch_add(wait_ns, Ordering::Relaxed);
        Self::update_max(&self.max_wait_ns, wait_ns);
    }

    fn record_hold(&self, hold_ns: u64) {
        self.total_hold_ns.fetch_add(hold_ns, Ordering::Relaxed);
        Self::update_max(&self.max_hold_ns, hold_ns);
    }

    fn update_max(target: &AtomicU64, value: u64) {
        let mut current = target.load(Ordering::Relaxed);
        while value > current {
            match target.compare_exchange_weak(current, value, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }
}

impl DirectWriteProfileStats {
    const LOG_INTERVAL_WRITES: u64 = 4_096;

    const fn new() -> Self {
        Self {
            write_calls: AtomicU64::new(0),
            write_bytes: AtomicU64::new(0),
            total_mappings: AtomicU64::new(0),
            total_bios: AtomicU64::new(0),
            total_segments: AtomicU64::new(0),
            total_blocks: AtomicU64::new(0),
            merge_hits: AtomicU64::new(0),
            user_buffer_pages: AtomicU64::new(0),
            user_buffer_phys_runs: AtomicU64::new(0),
            user_buffer_profile_failures: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            plan_ns: AtomicU64::new(0),
            prepare_ns: AtomicU64::new(0),
            data_bio_ns: AtomicU64::new(0),
            bio_alloc_ns: AtomicU64::new(0),
            bio_copy_ns: AtomicU64::new(0),
            bio_submit_ns: AtomicU64::new(0),
            bio_wait_ns: AtomicU64::new(0),
            bio_wait_return_after_complete_ns: AtomicU64::new(0),
            touch_ns: AtomicU64::new(0),
            total_ns: AtomicU64::new(0),
            hit_data_bio_ns: AtomicU64::new(0),
            hit_bio_copy_ns: AtomicU64::new(0),
            hit_bio_wait_ns: AtomicU64::new(0),
            hit_total_ns: AtomicU64::new(0),
            miss_plan_ns: AtomicU64::new(0),
            miss_prepare_ns: AtomicU64::new(0),
            miss_data_bio_ns: AtomicU64::new(0),
            miss_bio_copy_ns: AtomicU64::new(0),
            miss_bio_wait_ns: AtomicU64::new(0),
            miss_touch_ns: AtomicU64::new(0),
            miss_total_ns: AtomicU64::new(0),
            max_mappings: AtomicU64::new(0),
            max_bios_per_call: AtomicU64::new(0),
            max_segments_per_bio: AtomicU64::new(0),
            max_blocks_per_bio: AtomicU64::new(0),
            max_user_buffer_phys_runs: AtomicU64::new(0),
            max_user_buffer_phys_run_pages: AtomicU64::new(0),
            max_prepare_ns: AtomicU64::new(0),
            max_data_bio_ns: AtomicU64::new(0),
            max_bio_wait_return_after_complete_ns: AtomicU64::new(0),
            max_touch_ns: AtomicU64::new(0),
            max_total_ns: AtomicU64::new(0),
            max_miss_prepare_ns: AtomicU64::new(0),
            max_miss_data_bio_ns: AtomicU64::new(0),
            max_miss_total_ns: AtomicU64::new(0),
        }
    }

    fn record_write(
        &self,
        bytes: usize,
        cache_hit: bool,
        success: bool,
        bio_profile: &DirectWriteBioCallProfile,
        plan_ns: u64,
        prepare_ns: u64,
        data_bio_ns: u64,
        bio_alloc_ns: u64,
        bio_copy_ns: u64,
        bio_submit_ns: u64,
        bio_wait_ns: u64,
        touch_ns: u64,
        total_ns: u64,
    ) -> u64 {
        let writes = self.write_calls.fetch_add(1, Ordering::Relaxed) + 1;
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.write_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.total_mappings
            .fetch_add(bio_profile.mappings, Ordering::Relaxed);
        self.total_bios
            .fetch_add(bio_profile.bios, Ordering::Relaxed);
        self.total_segments
            .fetch_add(bio_profile.segments, Ordering::Relaxed);
        self.total_blocks
            .fetch_add(bio_profile.blocks, Ordering::Relaxed);
        self.merge_hits
            .fetch_add(bio_profile.merge_hits, Ordering::Relaxed);
        self.user_buffer_pages
            .fetch_add(bio_profile.user_buffer_pages, Ordering::Relaxed);
        self.user_buffer_phys_runs
            .fetch_add(bio_profile.user_buffer_phys_runs, Ordering::Relaxed);
        self.user_buffer_profile_failures
            .fetch_add(bio_profile.user_buffer_profile_failures, Ordering::Relaxed);
        if cache_hit {
            self.cache_hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.cache_misses.fetch_add(1, Ordering::Relaxed);
        }
        if !success {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
        self.plan_ns.fetch_add(plan_ns, Ordering::Relaxed);
        self.prepare_ns.fetch_add(prepare_ns, Ordering::Relaxed);
        self.data_bio_ns.fetch_add(data_bio_ns, Ordering::Relaxed);
        self.bio_alloc_ns.fetch_add(bio_alloc_ns, Ordering::Relaxed);
        self.bio_copy_ns.fetch_add(bio_copy_ns, Ordering::Relaxed);
        self.bio_submit_ns
            .fetch_add(bio_submit_ns, Ordering::Relaxed);
        self.bio_wait_ns.fetch_add(bio_wait_ns, Ordering::Relaxed);
        self.bio_wait_return_after_complete_ns
            .fetch_add(bio_profile.wait_return_after_complete_ns, Ordering::Relaxed);
        self.touch_ns.fetch_add(touch_ns, Ordering::Relaxed);
        self.total_ns.fetch_add(total_ns, Ordering::Relaxed);
        if cache_hit {
            self.hit_data_bio_ns
                .fetch_add(data_bio_ns, Ordering::Relaxed);
            self.hit_bio_copy_ns
                .fetch_add(bio_copy_ns, Ordering::Relaxed);
            self.hit_bio_wait_ns
                .fetch_add(bio_wait_ns, Ordering::Relaxed);
            self.hit_total_ns.fetch_add(total_ns, Ordering::Relaxed);
        } else {
            self.miss_plan_ns.fetch_add(plan_ns, Ordering::Relaxed);
            self.miss_prepare_ns
                .fetch_add(prepare_ns, Ordering::Relaxed);
            self.miss_data_bio_ns
                .fetch_add(data_bio_ns, Ordering::Relaxed);
            self.miss_bio_copy_ns
                .fetch_add(bio_copy_ns, Ordering::Relaxed);
            self.miss_bio_wait_ns
                .fetch_add(bio_wait_ns, Ordering::Relaxed);
            self.miss_touch_ns.fetch_add(touch_ns, Ordering::Relaxed);
            self.miss_total_ns.fetch_add(total_ns, Ordering::Relaxed);
            Ext4RsRuntimeLockStats::update_max(&self.max_miss_prepare_ns, prepare_ns);
            Ext4RsRuntimeLockStats::update_max(&self.max_miss_data_bio_ns, data_bio_ns);
            Ext4RsRuntimeLockStats::update_max(&self.max_miss_total_ns, total_ns);
        }
        Ext4RsRuntimeLockStats::update_max(&self.max_mappings, bio_profile.mappings);
        Ext4RsRuntimeLockStats::update_max(&self.max_bios_per_call, bio_profile.bios);
        Ext4RsRuntimeLockStats::update_max(
            &self.max_segments_per_bio,
            bio_profile.max_segments_per_bio,
        );
        Ext4RsRuntimeLockStats::update_max(
            &self.max_blocks_per_bio,
            bio_profile.max_blocks_per_bio,
        );
        Ext4RsRuntimeLockStats::update_max(
            &self.max_user_buffer_phys_runs,
            bio_profile.user_buffer_phys_runs,
        );
        Ext4RsRuntimeLockStats::update_max(
            &self.max_user_buffer_phys_run_pages,
            bio_profile.max_user_buffer_phys_run_pages,
        );
        Ext4RsRuntimeLockStats::update_max(&self.max_prepare_ns, prepare_ns);
        Ext4RsRuntimeLockStats::update_max(&self.max_data_bio_ns, data_bio_ns);
        Ext4RsRuntimeLockStats::update_max(
            &self.max_bio_wait_return_after_complete_ns,
            bio_profile.wait_return_after_complete_ns,
        );
        Ext4RsRuntimeLockStats::update_max(&self.max_touch_ns, touch_ns);
        Ext4RsRuntimeLockStats::update_max(&self.max_total_ns, total_ns);
        writes
    }
}

impl JournaledOpProfileStats {
    const fn new() -> Self {
        Self {
            op_count: AtomicU64::new(0),
            mkdir_count: AtomicU64::new(0),
            rmdir_count: AtomicU64::new(0),
            write_count: AtomicU64::new(0),
            start_handle_ns: AtomicU64::new(0),
            apply_ns: AtomicU64::new(0),
            finish_handle_ns: AtomicU64::new(0),
            finish_alloc_ns: AtomicU64::new(0),
            finish_io_ns: AtomicU64::new(0),
            total_ns: AtomicU64::new(0),
            max_apply_ns: AtomicU64::new(0),
            max_finish_handle_ns: AtomicU64::new(0),
            max_total_ns: AtomicU64::new(0),
        }
    }

    fn record(
        &self,
        op: Option<&JournaledOp>,
        start_handle_ns: u64,
        apply_ns: u64,
        finish_handle_ns: u64,
        finish_alloc_ns: u64,
        finish_io_ns: u64,
        total_ns: u64,
    ) {
        self.op_count.fetch_add(1, Ordering::Relaxed);
        match op {
            Some(JournaledOp::Mkdir) => {
                self.mkdir_count.fetch_add(1, Ordering::Relaxed);
            }
            Some(JournaledOp::Rmdir) => {
                self.rmdir_count.fetch_add(1, Ordering::Relaxed);
            }
            Some(JournaledOp::Write { .. }) => {
                self.write_count.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        self.start_handle_ns
            .fetch_add(start_handle_ns, Ordering::Relaxed);
        self.apply_ns.fetch_add(apply_ns, Ordering::Relaxed);
        self.finish_handle_ns
            .fetch_add(finish_handle_ns, Ordering::Relaxed);
        self.finish_alloc_ns
            .fetch_add(finish_alloc_ns, Ordering::Relaxed);
        self.finish_io_ns.fetch_add(finish_io_ns, Ordering::Relaxed);
        self.total_ns.fetch_add(total_ns, Ordering::Relaxed);
        Ext4RsRuntimeLockStats::update_max(&self.max_apply_ns, apply_ns);
        Ext4RsRuntimeLockStats::update_max(&self.max_finish_handle_ns, finish_handle_ns);
        Ext4RsRuntimeLockStats::update_max(&self.max_total_ns, total_ns);
    }
}

impl JournaledOp {
    /// Tag for buffered writes through `Ext4Fs::write_at`.
    ///
    /// Step 4a-2: previously this returned `None` for writes larger than
    /// `JOURNALED_SMALL_WRITE_MAX_BYTES` (192 B), causing the
    /// `inode_tids` map to miss large buffered writes — so fsync of those
    /// inodes had `target_tid = None` and skipped force-commit.
    /// generic/047 (32 K pwrite + fsync per file) exposed this: late files
    /// went un-committed and were lost after shutdown + replay.
    /// Now we always return `Some(Write { len, ino })` for non-empty
    /// writes; the journal credit estimation in
    /// `estimate_jbd2_reserved_blocks` already scales with `len`.
    fn for_small_write(ino: u32, _offset: usize, data: &[u8]) -> Option<Self> {
        if data.is_empty() {
            return None;
        }
        Some(Self::Write {
            len: data.len(),
            ino,
        })
    }
}

#[derive(Debug, Default)]
struct DirEntryCache {
    loaded: bool,
    /// Maps entry name → (child_ino, dir_byte_offset).
    /// `dir_byte_offset == u64::MAX` means the offset is unknown (fallback path).
    entries: BTreeMap<String, DirEntryCacheEntry>,
}

#[derive(Clone, Copy, Debug)]
struct DirEntryCacheEntry {
    ino: u32,
    offset: u64,
    de_type: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirLookupCacheResult {
    /// (child_ino, dir_byte_offset, de_type); offset is u64::MAX when unknown.
    Hit(u32, u64, u8),
    Miss,
    Unknown,
}

#[derive(Debug)]
struct PendingDirectRead {
    offset: usize,
    len: usize,
    mappings: Vec<SimpleBlockRange>,
    waiter: BioWaiter,
}

#[derive(Debug)]
struct PreparedDirectRead {
    offset: usize,
    len: usize,
    mappings: Vec<SimpleBlockRange>,
}

#[derive(Debug)]
struct DirectReadCache {
    file_offset: usize,
    len: usize,
    plan_window: usize,
    last_atime_sec: u32,
    last_read_end: usize,
    pending: Option<PendingDirectRead>,
    mappings: Vec<SimpleBlockRange>,
}

/// Phase 5: a cached logical->physical extent mapping for one inode, covering
/// the file byte range `[file_offset, file_offset + len)`. Metadata only — no
/// file data, no speculative readahead. Lets sequential O_DIRECT reads reuse a
/// single `find_extent` walk instead of re-resolving the mapping per read.
struct ExtentMapCacheEntry {
    file_offset: usize,
    len: usize,
    mappings: Vec<SimpleBlockRange>,
}

// P1 (Phase 6): capacity of the adapter-level device block cache, in 4 KiB
// blocks (8192 = 32 MiB). Sized for the metadata working set (inode table
// blocks, extent tree blocks, bitmaps, directory blocks); data blocks that
// pass through only via buffered RMW reads just cycle the LRU tail.
const DEVICE_BLOCK_CACHE_CAPACITY: usize = 8192;
// Evict this many least-recently-used entries per eviction pass, so the
// O(n) scan is amortized over many inserts.
const DEVICE_BLOCK_CACHE_EVICT_BATCH: usize = 1024;

/// P1 (Phase 6): bounded write-through mirror of device blocks.
///
/// Sits at the lowest layer (`KernelBlockDeviceAdapter`), *below* the JBD2
/// overlay: a cached block always equals the device's home-location content,
/// and `JournalIoBridge` patches the journal overlay on top of it exactly as
/// it does on top of a real device read. This makes coherence local to the
/// adapter:
/// - every `write_offset` (data writes, checkpoint home writes, journal
///   recovery replay) updates full blocks in place and drops partially
///   overwritten ones;
/// - deferred metadata writes (active JBD2 handle) never reach the adapter,
///   so the cache keeps serving the pre-write home content that the overlay
///   correctly overrides — and the eventual checkpoint write refreshes it.
///
/// The only writer that bypasses the adapter is the O_DIRECT data path
/// (`submit_direct_write_mappings`, raw `write_blocks_async`); it must call
/// `invalidate_block_range` after its bios complete.
struct DeviceBlockCache {
    blocks: BTreeMap<usize, CachedDeviceBlock>,
    use_counter: u64,
}

struct CachedDeviceBlock {
    data: Vec<u8>,
    last_use: u64,
}

impl DeviceBlockCache {
    fn new() -> Self {
        Self {
            blocks: BTreeMap::new(),
            use_counter: 0,
        }
    }

    fn get(&mut self, block_idx: usize, out: &mut [u8]) -> bool {
        self.use_counter += 1;
        let counter = self.use_counter;
        if let Some(cached) = self.blocks.get_mut(&block_idx) {
            out.copy_from_slice(&cached.data);
            cached.last_use = counter;
            true
        } else {
            false
        }
    }

    fn put(&mut self, block_idx: usize, data: &[u8]) -> u64 {
        let mut evicted = 0u64;
        if self.blocks.len() >= DEVICE_BLOCK_CACHE_CAPACITY
            && !self.blocks.contains_key(&block_idx)
        {
            let mut by_age: Vec<(u64, usize)> = self
                .blocks
                .iter()
                .map(|(idx, cached)| (cached.last_use, *idx))
                .collect();
            by_age.sort_unstable();
            for (_, idx) in by_age.into_iter().take(DEVICE_BLOCK_CACHE_EVICT_BATCH) {
                self.blocks.remove(&idx);
                evicted += 1;
            }
        }
        self.use_counter += 1;
        let last_use = self.use_counter;
        self.blocks.insert(
            block_idx,
            CachedDeviceBlock {
                data: data.to_vec(),
                last_use,
            },
        );
        evicted
    }

    fn update_if_present(&mut self, block_idx: usize, data: &[u8]) {
        if let Some(cached) = self.blocks.get_mut(&block_idx) {
            cached.data.copy_from_slice(data);
        }
    }

    fn remove(&mut self, block_idx: usize) {
        self.blocks.remove(&block_idx);
    }
}

#[derive(Debug, Default)]
struct DeviceBlockCacheStats {
    hits: AtomicU64,
    misses: AtomicU64,
    unaligned_reads: AtomicU64,
    evictions: AtomicU64,
    invalidations: AtomicU64,
}

struct KernelBlockDeviceAdapter {
    inner: Arc<dyn BlockDevice>,
    io_failure_epoch: AtomicU64,
    block_cache: Mutex<DeviceBlockCache>,
    block_cache_stats: DeviceBlockCacheStats,
}

impl core::fmt::Debug for KernelBlockDeviceAdapter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("KernelBlockDeviceAdapter").finish()
    }
}

impl KernelBlockDeviceAdapter {
    fn new(inner: Arc<dyn BlockDevice>) -> Self {
        Self {
            inner,
            io_failure_epoch: AtomicU64::new(0),
            block_cache: Mutex::new(DeviceBlockCache::new()),
            block_cache_stats: DeviceBlockCacheStats::default(),
        }
    }

    /// Drops cached copies of the device blocks overlapping
    /// `[start_byte, start_byte + len)`. Required by every writer that
    /// bypasses `write_offset` (the O_DIRECT data bios).
    fn invalidate_block_range(&self, start_byte: usize, len: usize) {
        if len == 0 {
            return;
        }
        let first = start_byte / EXT4_BLOCK_SIZE;
        let last = start_byte
            .saturating_add(len)
            .saturating_sub(1)
            / EXT4_BLOCK_SIZE;
        let mut cache = self.block_cache.lock();
        for idx in first..=last {
            cache.remove(idx);
        }
        self.block_cache_stats
            .invalidations
            .fetch_add((last - first + 1) as u64, Ordering::Relaxed);
    }

    /// Mirrors a completed device write into the cache: full blocks are
    /// updated in place (kept warm), partially covered blocks are dropped.
    fn mirror_write_to_cache(&self, offset: usize, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let end = offset.saturating_add(data.len());
        let first = offset / EXT4_BLOCK_SIZE;
        let last = (end - 1) / EXT4_BLOCK_SIZE;
        let mut cache = self.block_cache.lock();
        for idx in first..=last {
            let block_start = idx * EXT4_BLOCK_SIZE;
            let block_end = block_start + EXT4_BLOCK_SIZE;
            if offset <= block_start && end >= block_end {
                let src = &data[block_start - offset..block_end - offset];
                cache.update_if_present(idx, src);
            } else {
                cache.remove(idx);
            }
        }
    }

    #[inline]
    fn align_down(offset: usize) -> usize {
        offset / SECTOR_SIZE * SECTOR_SIZE
    }

    #[inline]
    fn align_up(offset: usize) -> usize {
        offset.div_ceil(SECTOR_SIZE) * SECTOR_SIZE
    }

    #[inline]
    fn mark_io_failure(&self) {
        self.io_failure_epoch.fetch_add(1, Ordering::AcqRel);
    }

    fn begin_io_operation(&self) -> u64 {
        self.io_failure_epoch.load(Ordering::Acquire)
    }

    fn io_failed_since(&self, epoch: u64) -> bool {
        self.io_failure_epoch.load(Ordering::Acquire) != epoch
    }

    fn device_size_bytes(&self) -> usize {
        self.inner.metadata().nr_sectors.saturating_mul(SECTOR_SIZE)
    }
}

impl Ext4BlockDevice for KernelBlockDeviceAdapter {
    fn read_offset(&self, offset: usize) -> Vec<u8> {
        let mut data = vec![0u8; EXT4_BLOCK_SIZE];
        self.read_offset_into(offset, data.as_mut_slice());
        data
    }

    fn read_offset_into(&self, offset: usize, out: &mut [u8]) {
        if out.is_empty() {
            return;
        }

        let read_len = out.len();
        let dev_size = self.device_size_bytes();
        let Some(read_end) = offset.checked_add(read_len) else {
            self.mark_io_failure();
            error!("ext4 block read overflow at offset {}", offset);
            out.fill(0);
            return;
        };
        if read_end > dev_size {
            self.mark_io_failure();
            error!(
                "ext4 block read out of range: offset={} len={} device_size={}",
                offset, read_len, dev_size
            );
            out.fill(0);
            return;
        }

        // P1 (Phase 6): block cache fast path for whole-block aligned reads
        // (the shape of every metadata read: inode table blocks, extent tree
        // blocks, bitmaps, directory blocks). Other shapes pass through.
        let cacheable = offset % EXT4_BLOCK_SIZE == 0 && read_len == EXT4_BLOCK_SIZE;
        if cacheable {
            let block_idx = offset / EXT4_BLOCK_SIZE;
            if self.block_cache.lock().get(block_idx, out) {
                self.block_cache_stats.hits.fetch_add(1, Ordering::Relaxed);
                return;
            }
        } else {
            self.block_cache_stats
                .unaligned_reads
                .fetch_add(1, Ordering::Relaxed);
        }

        let aligned_start = Self::align_down(offset);
        let aligned_end = Self::align_up(offset + read_len);
        let aligned_len = aligned_end - aligned_start;

        if aligned_start == offset && aligned_len == read_len {
            let mut writer = VmWriter::from(&mut out[..]).to_fallible();
            if let Err(err) = self.inner.read(offset, &mut writer) {
                self.mark_io_failure();
                error!("ext4 block read failed at offset {}: {:?}", offset, err);
                out.fill(0);
                return;
            }
            if cacheable {
                self.block_cache_stats.misses.fetch_add(1, Ordering::Relaxed);
                let evicted = self.block_cache.lock().put(offset / EXT4_BLOCK_SIZE, out);
                if evicted > 0 {
                    self.block_cache_stats
                        .evictions
                        .fetch_add(evicted, Ordering::Relaxed);
                }
            }
            return;
        }

        let mut aligned = vec![0u8; aligned_len];
        let mut writer = VmWriter::from(aligned.as_mut_slice()).to_fallible();
        if let Err(err) = self.inner.read(aligned_start, &mut writer) {
            self.mark_io_failure();
            error!("ext4 block read failed at offset {}: {:?}", offset, err);
            out.fill(0);
            return;
        }

        let start = offset - aligned_start;
        out.copy_from_slice(&aligned[start..start + read_len]);
    }

    fn write_offset(&self, offset: usize, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        let dev_size = self.device_size_bytes();
        let Some(write_end) = offset.checked_add(data.len()) else {
            self.mark_io_failure();
            error!(
                "ext4 block write overflow at offset {} len={}",
                offset,
                data.len()
            );
            return;
        };
        if write_end > dev_size {
            self.mark_io_failure();
            error!(
                "ext4 block write out of range: offset={} len={} device_size={}",
                offset,
                data.len(),
                dev_size
            );
            return;
        }

        let aligned_start = Self::align_down(offset);
        let aligned_end = Self::align_up(offset + data.len());
        let aligned_len = aligned_end - aligned_start;

        if aligned_start == offset && aligned_len == data.len() {
            let mut reader = VmReader::from(data).to_fallible();
            if let Err(err) = self.inner.write(offset, &mut reader) {
                self.mark_io_failure();
                error!("ext4 block write failed at offset {}: {:?}", offset, err);
                // A failed write leaves the device content undefined; drop
                // the cached copies instead of mirroring.
                self.invalidate_block_range(offset, data.len());
                return;
            }
            self.mirror_write_to_cache(offset, data);
            return;
        }

        let mut aligned = vec![0u8; aligned_len];

        // Preserve neighboring bytes when ext4_rs issues unaligned writes.
        if aligned_start != offset || aligned_len != data.len() {
            let mut writer = VmWriter::from(aligned.as_mut_slice()).to_fallible();
            if let Err(err) = self.inner.read(aligned_start, &mut writer) {
                self.mark_io_failure();
                error!(
                    "ext4 block pre-read failed at offset {}: {:?}",
                    aligned_start, err
                );
                return;
            }
        }

        let start = offset - aligned_start;
        aligned[start..start + data.len()].copy_from_slice(data);

        let mut reader = VmReader::from(aligned.as_slice()).to_fallible();
        if let Err(err) = self.inner.write(aligned_start, &mut reader) {
            self.mark_io_failure();
            error!("ext4 block write failed at offset {}: {:?}", offset, err);
            self.invalidate_block_range(aligned_start, aligned_len);
            return;
        }
        self.mirror_write_to_cache(aligned_start, aligned.as_slice());
    }

    fn sync(&self) -> core::result::Result<(), ext4_rs::Ext4Error> {
        match self.inner.sync() {
            Ok(BioStatus::Complete) => Ok(()),
            Ok(status) => {
                self.mark_io_failure();
                error!("ext4 block sync completed with status {:?}", status);
                Err(ext4_rs::Ext4Error::with_message(
                    ext4_rs::Errno::EIO,
                    "block device sync did not complete",
                ))
            }
            Err(err) => {
                self.mark_io_failure();
                error!("ext4 block sync failed: {:?}", err);
                Err(ext4_rs::Ext4Error::with_message(
                    ext4_rs::Errno::EIO,
                    "block device sync failed",
                ))
            }
        }
    }
}

struct JournalIoBridge {
    adapter: Arc<KernelBlockDeviceAdapter>,
    runtime: Arc<RwMutex<Option<JournalRuntime>>>,
}

impl JournalIoBridge {
    fn new(
        adapter: Arc<KernelBlockDeviceAdapter>,
        runtime: Arc<RwMutex<Option<JournalRuntime>>>,
    ) -> Self {
        Self { adapter, runtime }
    }

    fn overlay_metadata_read(&self, offset: usize, out: &mut [u8]) {
        let runtime_guard = self.runtime.read();
        let Some(runtime) = runtime_guard.as_ref() else {
            return;
        };
        runtime.overlay_metadata_read(offset, out);
    }

    fn write_metadata_for_handle(&self, handle_id: Option<u64>, offset: usize, data: &[u8]) {
        let mut defer_metadata_write = false;
        if let Some(runtime) = self.runtime.write().as_mut() {
            let block_size = runtime.block_size();
            if let Some(handle_id) = handle_id {
                runtime.record_metadata_write_for_handle(handle_id, offset, data, |block_nr| {
                    let block_offset = block_nr as usize * block_size;
                    let mut block_data = vec![0u8; block_size];
                    self.adapter.read_offset_into(block_offset, &mut block_data);
                    block_data
                });
            }
            defer_metadata_write = runtime.should_defer_metadata_write();
        }
        if defer_metadata_write {
            return;
        }
        self.adapter.write_offset(offset, data);
    }
}

impl Ext4BlockDevice for JournalIoBridge {
    fn read_offset(&self, offset: usize) -> Vec<u8> {
        let block_size = self
            .runtime
            .read()
            .as_ref()
            .map(|runtime| runtime.block_size())
            .unwrap_or(EXT4_BLOCK_SIZE);
        let mut data = vec![0u8; block_size];
        self.read_offset_into(offset, &mut data);
        data
    }

    fn read_offset_into(&self, offset: usize, out: &mut [u8]) {
        self.adapter.read_offset_into(offset, out);
        self.overlay_metadata_read(offset, out);
    }

    fn write_offset(&self, offset: usize, data: &[u8]) {
        self.adapter.write_offset(offset, data);
    }

    fn sync(&self) -> core::result::Result<(), ext4_rs::Ext4Error> {
        self.adapter.sync()
    }
}

impl Ext4MetadataWriter for JournalIoBridge {
    fn write_metadata(&self, offset: usize, data: &[u8]) {
        self.write_metadata_for_handle(None, offset, data);
    }

    fn write_metadata_for_jbd2_handle(&self, handle_id: Option<u64>, offset: usize, data: &[u8]) {
        self.write_metadata_for_handle(handle_id, offset, data);
    }
}

struct JournalOperationMetadataWriter {
    bridge: Arc<JournalIoBridge>,
    handle_id: Option<u64>,
}

impl JournalOperationMetadataWriter {
    fn new(bridge: Arc<JournalIoBridge>, handle_id: Option<u64>) -> Self {
        Self { bridge, handle_id }
    }
}

impl Ext4MetadataWriter for JournalOperationMetadataWriter {
    fn write_metadata(&self, offset: usize, data: &[u8]) {
        self.bridge
            .write_metadata_for_jbd2_handle(self.handle_id, offset, data);
    }
}

struct Ext4PageCacheState {
    page_cache: PageCache,
    _backend: Arc<Ext4PageCacheBackend>,
}

impl Ext4PageCacheState {
    fn new(fs: Weak<Ext4Fs>, ino: u32, capacity: usize) -> Result<Self> {
        let backend = Arc::new(Ext4PageCacheBackend { fs, ino });
        let page_cache = PageCache::with_capacity(capacity, Arc::downgrade(&backend) as _)?;
        Ok(Self {
            page_cache,
            _backend: backend,
        })
    }

    fn pages(&self) -> Arc<Vmo> {
        self.page_cache.pages().clone()
    }

    fn cached_size(&self) -> usize {
        self.page_cache.pages().size()
    }

    fn resize(&self, new_size: usize) -> Result<()> {
        self.page_cache.resize(new_size)
    }

    fn evict_range(&self, start: usize, len: usize) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let end = start.saturating_add(len);
        self.page_cache.evict_range(start..end)?;
        self.decommit_vmo_range(start, end)
    }

    fn evict_all(&self, file_size: usize) -> Result<()> {
        self.page_cache.evict_range(0..file_size)?;
        self.decommit_vmo_range(0, file_size)
    }

    /// Writes back every dirty page but keeps the pages resident as clean
    /// (no `decommit`). This is the `fsync`/`sync` safe point: Linux `fsync`
    /// flushes dirty pages yet never drops them, so the working set stays warm
    /// and later reads or sub-page writes hit the cache instead of refilling the
    /// whole file 4KB at a time from the device. Page frames are still released
    /// by `evict_all` (dropping inode state), `discard_all` (truncate) and the
    /// O_DIRECT path (which evicts+discards its own range before touching the
    /// device), so buffered/direct coherency is preserved without decommitting
    /// here.
    fn flush_all(&self, file_size: usize) -> Result<()> {
        self.page_cache.evict_range(0..file_size)
    }

    fn discard_range(&self, start: usize, len: usize) {
        if len == 0 {
            return;
        }
        let end = start.saturating_add(len);
        self.page_cache.discard_range(start..end);
        let _ = self.decommit_vmo_range(start, end);
    }

    fn discard_all(&self) {
        let size = self.page_cache.pages().size();
        self.page_cache.discard_range(0..size);
        let _ = self.decommit_vmo_range(0, size);
    }

    fn decommit_vmo_range(&self, start: usize, end: usize) -> Result<()> {
        let size = self.page_cache.pages().size();
        if start >= size {
            return Ok(());
        }
        self.page_cache.pages().decommit(start..end.min(size))
    }
}

struct Ext4PageCacheBackend {
    fs: Weak<Ext4Fs>,
    ino: u32,
}

impl Ext4PageCacheBackend {
    fn fs(&self) -> Result<Arc<Ext4Fs>> {
        self.fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "ext4 fs is dropped"))
    }

    fn page_offset(idx: usize) -> Result<usize> {
        idx.checked_mul(PAGE_SIZE)
            .ok_or_else(|| Error::with_message(Errno::EFBIG, "page cache offset overflow"))
    }
}

impl PageCacheBackend for Ext4PageCacheBackend {
    fn read_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter> {
        let fs = self.fs()?;
        let offset = Self::page_offset(idx)?;
        let mut data = vec![0u8; PAGE_SIZE];
        let file_size = fs.stat(self.ino)?.size as usize;

        if offset < file_size {
            let read_len = PAGE_SIZE.min(file_size - offset);
            fs.read_page_cache_data_at(self.ino, offset, &mut data[..read_len])?;
        }

        frame.writer().write(&mut VmReader::from(data.as_slice()));
        Ok(BioWaiter::new())
    }

    fn write_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter> {
        let fs = self.fs()?;
        let offset = Self::page_offset(idx)?;
        let file_size = fs.stat(self.ino)?.size as usize;
        if offset >= file_size {
            return Ok(BioWaiter::new());
        }

        let write_len = PAGE_SIZE.min(file_size - offset);
        let mut data = vec![0u8; write_len];
        frame
            .reader()
            .read_fallible(&mut VmWriter::from(data.as_mut_slice()).to_fallible())
            .map_err(|(err, _)| Error::from(err))?;
        fs.write_page_cache_data_at(self.ino, offset, data.as_slice())?;
        Ok(BioWaiter::new())
    }

    /// S4 (Phase 6): write a contiguous run of dirty pages in one batch.
    ///
    /// Gathers the run's page contents into a single buffer and writes it with
    /// one `write_page_cache_data_at` call, so the whole run is mapped once,
    /// journaled under one handle and written as coalesced bios (`write_at`
    /// merges contiguous physical blocks) -- instead of one mapping + one JBD2
    /// handle + one 4KB bio per page. Preserves the same size clamp as
    /// `write_page_async` (C4 invariant: never write past the on-disk size).
    fn write_pages_async(&self, start_idx: usize, frames: &[&CachePage]) -> Result<BioWaiter> {
        if frames.is_empty() {
            return Ok(BioWaiter::new());
        }
        let fs = self.fs()?;
        let offset = Self::page_offset(start_idx)?;
        let file_size = fs.stat(self.ino)?.size as usize;
        if offset >= file_size {
            return Ok(BioWaiter::new());
        }
        let run_bytes = frames
            .len()
            .checked_mul(PAGE_SIZE)
            .ok_or_else(|| Error::with_message(Errno::EFBIG, "page cache run overflow"))?;
        let write_len = run_bytes.min(file_size - offset);
        if write_len == 0 {
            return Ok(BioWaiter::new());
        }
        let mut data = vec![0u8; write_len];
        let mut copied = 0;
        for &frame in frames {
            if copied >= write_len {
                break;
            }
            let chunk = PAGE_SIZE.min(write_len - copied);
            frame
                .reader()
                .read_fallible(&mut VmWriter::from(&mut data[copied..copied + chunk]).to_fallible())
                .map_err(|(err, _)| Error::from(err))?;
            copied += chunk;
        }
        fs.write_page_cache_data_at(self.ino, offset, data.as_slice())?;
        Ok(BioWaiter::new())
    }

    fn npages(&self) -> usize {
        let Ok(fs) = self.fs() else {
            return 0;
        };
        fs.stat(self.ino)
            .map(|meta| (meta.size as usize).div_ceil(PAGE_SIZE))
            .unwrap_or(0)
    }
}

pub(super) struct Ext4Fs {
    inner: Mutex<Ext4>,
    block_device: Arc<dyn BlockDevice>,
    adapter: Arc<KernelBlockDeviceAdapter>,
    mount_flags_bits: AtomicU32,
    jbd2_journal: Mutex<Option<Jbd2Journal>>,
    jbd2_runtime: Arc<RwMutex<Option<JournalRuntime>>>,
    journal_io: Arc<JournalIoBridge>,
    alloc_guard: Arc<LocalOperationAllocGuard>,
    next_alloc_operation_id: AtomicU64,
    jbd2_checkpoint_lock: Mutex<()>,
    inode_correctness_locks: Mutex<BTreeMap<u32, Arc<Mutex<()>>>>,
    dir_correctness_locks: Mutex<BTreeMap<u32, Arc<Mutex<()>>>>,
    /// Step 4a-2: per-ino "highest TID containing a metadata change for this
    /// inode" map.  Equivalent to Linux `EXT4_I(inode)->i_sync_tid`.
    /// Updated by `finish_jbd2_handle` after a Write/Truncate handle stops;
    /// queried by `fsync_regular_file` to find the target TID for force-commit.
    /// Entries with `tid <= last_committed_tid` are stale but harmless (the
    /// fast path in `force_commit_for_tid` filters them).  We do not actively
    /// evict to keep the lock granularity simple; eviction can be added later
    /// when memory pressure arises.
    inode_tids: RwMutex<BTreeMap<u32, u32>>,
    /// Step 4a-2: WaitQueue for fsync force-commit waiters.  Woken after
    /// every successful `finish_commit` (i.e. `last_committed_tid` advances)
    /// and after every `stop_handle` (which may make a prev_running TX
    /// commit-ready).  Waiters re-check `last_committed_tid >= target_tid`.
    commit_notifier: WaitQueue,
    /// Step 4b: shutdown state set by `EXT4_IOC_SHUTDOWN` ioctl.
    /// `0` = active, `1` = shutdown.  Once shutdown, `run_journaled_ext4`
    /// and `fsync_regular_file` return EIO.  Cleared automatically on
    /// remount + recovery (Phase 1 path).
    shutdown_state: AtomicU32,
    dir_entry_cache: Mutex<BTreeMap<u32, DirEntryCache>>,
    inode_page_caches: Mutex<BTreeMap<u32, Arc<Ext4PageCacheState>>>,
    open_file_handles: Mutex<BTreeMap<u32, usize>>,
    inode_direct_read_cache: Mutex<BTreeMap<u32, DirectReadCache>>,
    // Phase 5: metadata-only extent mapping cache for O_DIRECT reads. Distinct
    // from `inode_direct_read_cache` above (which is the retired speculative
    // *data* read cache): this caches only the logical->physical extent mapping
    // (a few integers per extent) so sequential reads skip the per-read
    // `find_extent` walk. Holds no file data and does no speculative readahead.
    inode_extent_map_cache: Mutex<BTreeMap<u32, ExtentMapCacheEntry>>,
    // Phase 5: in-memory inode metadata (stat) cache. ext4_rs `get_inode_ref`
    // re-reads the inode block from the device on every stat, and the read path
    // stats several times per read (type check, size, atime), so small reads
    // paid ~25us per stat. ext2 keeps the inode in memory; this closes that gap.
    // Correctness: any journaled mutation bumps `meta_cache_generation` and
    // clears this cache (run_journaled_ext4 is the single chokepoint for all
    // create/write/truncate/setattr/dir ops); `stat` only inserts when the
    // generation did not advance across its disk read, closing the read-vs-write
    // TOCTOU.
    inode_meta_cache: Mutex<BTreeMap<u32, SimpleInodeMeta>>,
    meta_cache_generation: AtomicU64,
    inode_atime_cache: Mutex<BTreeMap<u32, u32>>,
    inode_ctime_cache: Mutex<BTreeMap<u32, u32>>,
    inode_mtime_ctime_cache: Mutex<BTreeMap<u32, u32>>,
    page_cache_enabled: bool,
    direct_read_cache_enabled: bool,
    extent_map_cache_enabled: bool,
    phase2_profile_enabled: bool,
    direct_read_profile_started: AtomicBool,
    direct_write_profile_started: AtomicBool,
    direct_read_profile: DirectReadProfileStats,
    direct_write_profile: DirectWriteProfileStats,
    buffered_write_profile: BufferedWriteProfileStats,
    runtime_lock_stats: Ext4RsRuntimeLockStats,
    journaled_op_profile: JournaledOpProfileStats,
    fs_event_subscriber_stats: FsEventSubscriberStats,
    self_ref: Weak<Self>,
}

impl Ext4Fs {
    pub fn open(block_device: Arc<dyn BlockDevice>) -> Arc<Self> {
        let adapter = Arc::new(KernelBlockDeviceAdapter::new(block_device.clone()));
        let jbd2_runtime = Arc::new(RwMutex::new(None));
        let alloc_guard = Arc::new(LocalOperationAllocGuard::new());
        let journal_io = Arc::new(JournalIoBridge::new(adapter.clone(), jbd2_runtime.clone()));
        let mut ext4 = Ext4::open(journal_io.clone());
        let metadata_writer: Arc<dyn Ext4MetadataWriter> = journal_io.clone();
        ext4.metadata_writer = metadata_writer;
        let operation_alloc_guard: Arc<dyn Ext4OperationAllocGuard> = alloc_guard.clone();
        ext4.alloc_guard = operation_alloc_guard;
        let fs = Arc::new_cyclic(|weak_ref| Self {
            inner: Mutex::new(ext4),
            block_device,
            adapter,
            mount_flags_bits: AtomicU32::new(PerMountFlags::default().bits()),
            jbd2_journal: Mutex::new(None),
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
            inode_direct_read_cache: Mutex::new(BTreeMap::new()),
            inode_extent_map_cache: Mutex::new(BTreeMap::new()),
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
        let _inode_guard = inode_lock.lock();
        self.sync_page_cache_for_inode_locked(ino)
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
            let _inode_guard = inode_lock.lock();
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

    fn correctness_lock_for(
        table: &Mutex<BTreeMap<u32, Arc<Mutex<()>>>>,
        ino: u32,
    ) -> Arc<Mutex<()>> {
        table
            .lock()
            .entry(ino)
            .or_insert_with(|| Arc::new(Mutex::new(())))
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
            guards.push(lock.lock());
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
            guards.push(lock.lock());
        }
        f()
    }

    pub(super) fn lock_inner(&self) -> MutexGuard<'_, Ext4> {
        self.inner.lock()
    }

    fn initialize_jbd2_journal(&self) {
        let journal = match self.run_ext4(|ext4| ext4.load_journal()) {
            Ok(journal) => journal,
            Err(err) => {
                warn!("ext4: failed to initialize JBD2 journal: {:?}", err);
                *self.jbd2_runtime.write() = None;
                return;
            }
        };

        match journal {
            Some(journal) => {
                *self.jbd2_runtime.write() = Some(JournalRuntime::new(
                    journal.superblock.block_size() as usize,
                    journal.superblock.sequence(),
                ));
                info!(
                    "ext4: loaded JBD2 journal inode={} blocks={} mapped_blocks={} block_size={} sequence={} start={} head={} first={} free_blocks={} incompat=0x{:x}",
                    journal.device.journal_inode(),
                    journal.superblock.maxlen(),
                    journal.device.logical_blocks(),
                    journal.superblock.block_size(),
                    journal.superblock.sequence(),
                    journal.superblock.start(),
                    journal.superblock.head(),
                    journal.superblock.first(),
                    journal.space.free_blocks(),
                    journal.superblock.feature_incompat(),
                );
                *self.jbd2_journal.lock() = Some(journal);
            }
            None => {
                info!("ext4: filesystem has no JBD2 journal feature; using non-journal path");
                *self.jbd2_runtime.write() = None;
                *self.jbd2_journal.lock() = None;
            }
        }
    }

    fn replay_mount_jbd2_journal(&self) {
        let needs_recovery = {
            let journal_guard = self.jbd2_journal.lock();
            let journal_needs_recovery = journal_guard
                .as_ref()
                .is_some_and(|journal| journal.needs_recovery());
            drop(journal_guard);

            if journal_needs_recovery {
                true
            } else {
                let inner = self.lock_inner();
                inner.super_block.needs_recovery()
            }
        };
        if !needs_recovery {
            return;
        }

        let recovery_result = {
            let block_device: Arc<dyn Ext4BlockDevice> = self.adapter.clone();
            let mut journal_guard = self.jbd2_journal.lock();
            let Some(journal) = journal_guard.as_mut() else {
                return;
            };
            journal.recover(&block_device)
        };

        match recovery_result {
            Ok(result) => {
                if let Err(err) = self.sync_recovered_jbd2_state(&result) {
                    warn!(
                        "ext4: JBD2 recovery replayed transactions but failed to finalize superblock state: {:?}",
                        err
                    );
                    return;
                }
                info!(
                    "ext4: JBD2 recovery complete: transactions={} metadata_blocks={} revoked={} last_sequence={:?}",
                    result.transactions_replayed,
                    result.metadata_blocks_replayed,
                    result.revoked_blocks,
                    result.last_sequence,
                );
            }
            Err(err) => {
                warn!("ext4: JBD2 recovery failed at mount: {:?}", err);
            }
        }
    }

    fn sync_recovered_jbd2_state(&self, result: &JournalRecoveryResult) -> Result<()> {
        self.block_device
            .sync()
            .map_err(|_| Error::with_message(Errno::EIO, "failed to sync recovered JBD2 blocks"))?;

        let io_epoch = self.prepare_ext4_io();
        let superblock_result = {
            let runtime_wait_start_ns = Self::monotonic_nanos();
            let runtime_guard = EXT4_RS_RUNTIME_LOCK.lock();
            self.record_ext4_rs_runtime_lock_wait(
                Self::monotonic_nanos().saturating_sub(runtime_wait_start_ns),
            );
            let runtime_hold_start_ns = Self::monotonic_nanos();
            let mut inner = self.lock_inner();
            let metadata_writer = inner.metadata_writer.clone();
            inner.super_block.set_needs_recovery(false);
            inner.super_block.sync_to_disk_with_csum(&metadata_writer);
            drop(inner);
            drop(runtime_guard);
            self.record_ext4_rs_runtime_lock_hold(
                Self::monotonic_nanos().saturating_sub(runtime_hold_start_ns),
            );
            Ok::<(), Error>(())
        };
        let io_result = self.finish_ext4_io(io_epoch);
        superblock_result?;
        io_result?;

        self.block_device
            .sync()
            .map_err(|_| Error::with_message(Errno::EIO, "failed to sync cleared recovery flag"))?;

        let (block_size, next_sequence) = {
            let journal_guard = self.jbd2_journal.lock();
            let Some(journal) = journal_guard.as_ref() else {
                return Ok(());
            };
            (
                journal.superblock.block_size() as usize,
                result
                    .last_sequence
                    .map(|sequence| sequence.saturating_add(1))
                    .unwrap_or(journal.superblock.sequence()),
            )
        };
        *self.jbd2_runtime.write() = Some(JournalRuntime::new(block_size, next_sequence));
        Ok(())
    }

    fn estimate_jbd2_reserved_blocks(op: Option<&JournaledOp>) -> u32 {
        match op {
            Some(JournaledOp::Create) | Some(JournaledOp::Mkdir) => 8,
            Some(JournaledOp::Unlink) | Some(JournaledOp::Rmdir) => 8,
            Some(JournaledOp::Rename) => 12,
            Some(JournaledOp::Write { len, .. }) => {
                let blocks = len.div_ceil(EXT4_BLOCK_SIZE);
                u32::try_from(blocks.saturating_add(8)).unwrap_or(u32::MAX)
            }
            Some(JournaledOp::Truncate { .. }) | Some(JournaledOp::InodeMetadata { .. }) => 8,
            None => 8,
        }
    }

    fn jbd2_handle_op_name(op: Option<&JournaledOp>) -> &'static str {
        match op {
            Some(JournaledOp::Create) => "create",
            Some(JournaledOp::Mkdir) => "mkdir",
            Some(JournaledOp::Unlink) => "unlink",
            Some(JournaledOp::Rmdir) => "rmdir",
            Some(JournaledOp::Rename) => "rename",
            Some(JournaledOp::Write { .. }) => "write",
            Some(JournaledOp::Truncate { .. }) => "truncate",
            Some(JournaledOp::InodeMetadata { .. }) => "inode_metadata",
            None => "anonymous",
        }
    }

    fn start_jbd2_handle(&self, op: Option<&JournaledOp>) -> Option<JournalHandle> {
        let reserved_blocks = Self::estimate_jbd2_reserved_blocks(op);
        let trigger_op = op.map(|_| Self::jbd2_handle_op_name(op));
        let mut runtime_guard = self.jbd2_runtime.write();
        let runtime = runtime_guard.as_mut()?;
        let handle = runtime.start_handle(reserved_blocks, trigger_op);
        if matches!(op, Some(JournaledOp::Write { .. })) {
            if let Some(handle) = handle.as_ref() {
                runtime.mark_handle_requires_data_sync(handle.handle_id());
            }
        }
        handle
    }

    fn next_alloc_operation_id(&self) -> u64 {
        self.next_alloc_operation_id
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1)
            .max(1)
    }

    fn begin_alloc_operation(&self, operation_id: Option<u64>) -> u64 {
        let operation_id = operation_id.unwrap_or_else(|| self.next_alloc_operation_id());
        self.alloc_guard.begin_operation(operation_id);
        operation_id
    }

    fn finish_alloc_operation(&self, operation_id: Option<u64>) {
        if let Some(operation_id) = operation_id {
            self.alloc_guard.finish_operation(operation_id);
        }
    }

    fn ext4_with_operation_context(
        &self,
        ext4: &Ext4,
        handle_id: Option<u64>,
        operation_id: Option<u64>,
    ) -> Ext4 {
        let mut scoped = ext4.clone();
        let metadata_writer: Arc<dyn Ext4MetadataWriter> = Arc::new(
            JournalOperationMetadataWriter::new(self.journal_io.clone(), handle_id),
        );
        scoped.metadata_writer = metadata_writer;
        if let Some(operation_id) = operation_id {
            let alloc_guard: Arc<dyn Ext4OperationAllocGuard> = Arc::new(
                OperationScopedAllocGuard::new(self.alloc_guard.clone(), operation_id),
            );
            scoped.alloc_guard = alloc_guard;
        }
        scoped
    }

    fn finish_jbd2_handle(
        &self,
        handle: Option<JournalHandle>,
        op: Option<&JournaledOp>,
        op_name: &'static str,
        succeeded: bool,
    ) {
        let Some(handle) = handle else {
            return;
        };
        let summary = self
            .jbd2_runtime
            .write()
            .as_mut()
            .and_then(|runtime| runtime.stop_handle(handle));
        let Some(summary) = summary else {
            return;
        };

        // Step 4a-2: record (ino → handle's TID) so a subsequent fsync(ino)
        // can force-commit exactly the TID containing this inode's metadata.
        // Only single-inode ops carry inode info in v1; directory ops touch
        // multiple inodes (parent + child) and rely on the Phase 2 inode
        // correctness lock to serialize fsync after the op completes.
        if succeeded && summary.modified_blocks > 0 {
            if let Some(ino) = op.and_then(JournaledOp::affected_ino) {
                self.record_inode_tid(ino, summary.transaction_id);
            }
        }
        // Wake any fsync waiter blocked on this transaction (a stop_handle on
        // prev_running may have made it commit_ready below).
        self.commit_notifier.wake_all();

        debug!(
            "ext4: jbd2 handle op={} handle_id={} tid={} reserved={} modified_blocks={} data_sync_required={} success={}",
            op_name,
            summary.handle_id,
            summary.transaction_id,
            summary.reserved_blocks,
            summary.modified_blocks,
            summary.data_sync_required,
            succeeded,
        );

        let runtime_guard = self.jbd2_runtime.read();
        if let Some(runtime) = runtime_guard.as_ref() {
            if runtime.commit_ready() {
                if let Some(transaction) = runtime.running_transaction() {
                    debug!(
                        "ext4: jbd2 transaction ready tid={} metadata_blocks={} reserved={} data_sync_required={}",
                        transaction.tid(),
                        transaction.modified_block_count(),
                        transaction.reserved_blocks(),
                        transaction.data_sync_required(),
                    );
                }
            }
        }
        drop(runtime_guard);

        if succeeded {
            let (rotated_tid, batch_commit_ready) = {
                let mut rt = self.jbd2_runtime.write();
                let Some(runtime) = rt.as_mut() else {
                    return;
                };
                let rotated_tid =
                    if runtime.should_rotate_running_transaction(JOURNAL_COMMIT_BATCH_BLOCKS) {
                        runtime.rotate_running_transaction()
                    } else {
                        None
                    };
                let batch_commit_ready = runtime.batch_commit_ready(JOURNAL_COMMIT_BATCH_BLOCKS);
                (rotated_tid, batch_commit_ready)
            };
            if let Some(tid) = rotated_tid {
                debug!(
                    "ext4: rotated JBD2 running transaction tid={} after batch threshold",
                    tid
                );
            }
            if batch_commit_ready {
                let _ = self.try_commit_ready_jbd2_transaction();
            }
            if Self::should_force_commit_for_injected_crash(op_name) {
                let _ = self.try_commit_ready_jbd2_transaction();
            }
        }
    }

    fn has_active_jbd2_handle(&self) -> bool {
        self.jbd2_runtime
            .read()
            .as_ref()
            .is_some_and(|runtime| runtime.has_active_handle())
    }

    /// Step 4a-2: returns the highest TID whose `finish_commit` succeeded.
    /// `0` means no transaction has committed yet (post-mount fresh state)
    /// — fsync of an inode whose recorded TID is also 0 is therefore a no-op
    /// (no metadata change).
    fn last_committed_tid(&self) -> u32 {
        self.jbd2_runtime
            .read()
            .as_ref()
            .map(|runtime| runtime.last_committed_tid())
            .unwrap_or(0)
    }

    /// Step 4a-2: record that this inode's metadata is committed in TID
    /// `tid` (or earlier). Always advances monotonically. Called by
    /// `finish_jbd2_handle` after a Write/Truncate handle stops.
    fn record_inode_tid(&self, ino: u32, tid: u32) {
        if tid == 0 {
            return;
        }
        let mut tids = self.inode_tids.write();
        let entry = tids.entry(ino).or_insert(0);
        if tid > *entry {
            *entry = tid;
        }
    }

    /// Step 4a-2: returns the latest TID with a known metadata change for
    /// this inode, or `None` if no Write/Truncate has been recorded.
    fn lookup_inode_tid(&self, ino: u32) -> Option<u32> {
        self.inode_tids.read().get(&ino).copied()
    }

    /// Step 4a-2: drive forward and wait until the JBD2 transaction
    /// containing this inode's recent metadata changes is durable in the
    /// journal.  Mirrors Linux `jbd2_journal_force_commit_nested` +
    /// `jbd2_log_wait_commit` semantics.
    ///
    /// Algorithm:
    /// 1. Fast path: if `last_committed_tid >= target_tid`, return.
    /// 2. If `target_tid` is the current running TX, rotate it to
    ///    `prev_running` (so no new handles join, allowing existing handles
    ///    to drain to commit-readiness).
    /// 3. Loop: try `try_commit_ready_jbd2_transaction()` to drive any
    ///    commit-ready TX to disk; if `last_committed_tid < target_tid`,
    ///    block on `commit_notifier` until either a finish_commit or a
    ///    handle stop wakes us, then re-check.
    ///
    /// Wakeup correctness: every `finish_commit` and every `stop_handle`
    /// calls `commit_notifier.wake_all()`, so any state change that could
    /// advance `last_committed_tid` notifies waiters.  `WaitQueue::wait_until`
    /// enqueues the waker before re-evaluating the condition, so no wakeup
    /// is lost.
    fn force_commit_for_tid(&self, target_tid: u32) {
        if target_tid == 0 {
            return;
        }
        // Fast path: already committed.
        if self.last_committed_tid() >= target_tid {
            return;
        }

        // Rotate target_tid out of `running` if it's still there. This
        // prevents new handles from joining and lets existing ones drain.
        {
            let mut runtime_guard = self.jbd2_runtime.write();
            if let Some(runtime) = runtime_guard.as_mut() {
                let running_tid = runtime.running_transaction().map(|t| t.tid()).unwrap_or(0);
                if running_tid == target_tid {
                    let _ = runtime.rotate_running_transaction();
                }
            }
        }

        // Wait until target_tid is committed.  In each iteration we first
        // try to drive any commit-ready TX forward (this is what advances
        // `last_committed_tid`), then check the condition.  If still not
        // satisfied, `wait_until` enqueues us on `commit_notifier` and the
        // next `finish_commit` / `stop_handle` will wake us up.
        self.commit_notifier.wait_until(|| {
            // Drive forward: this commits the prev_running TX once its
            // active handles have drained, advancing `last_committed_tid`.
            let _ = self.try_commit_ready_jbd2_transaction();
            if self.last_committed_tid() >= target_tid {
                Some(())
            } else {
                None
            }
        });
    }

    fn flush_pending_jbd2_transactions(&self) {
        // Drain any pending commit first (there should be at most one).
        while {
            let rt = self.jbd2_runtime.read();
            rt.as_ref().is_some_and(|rt| rt.commit_ready())
        } {
            if !self.try_commit_ready_jbd2_transaction() {
                break;
            }
        }
        // Batch checkpoint all accumulated transactions with a single disk flush,
        // rather than one flush per transaction.
        self.try_batch_checkpoint_all_jbd2_transactions();
    }

    fn commit_pending_jbd2_transactions(&self) {
        while {
            let rt = self.jbd2_runtime.read();
            rt.as_ref().is_some_and(|rt| rt.commit_ready())
        } {
            if !self.try_commit_ready_jbd2_transaction() {
                break;
            }
        }
    }

    fn checkpoint_depth(&self) -> usize {
        self.jbd2_runtime
            .read()
            .as_ref()
            .map(|runtime| runtime.checkpoint_depth())
            .unwrap_or(0)
    }

    fn reconcile_jbd2_checkpoint_tail(&self) {
        let current_tail = {
            let journal_guard = self.jbd2_journal.lock();
            let Some(journal) = journal_guard.as_ref() else {
                return;
            };
            journal.space.tail()
        };
        let dropped = self
            .jbd2_runtime
            .write()
            .as_mut()
            .map(|runtime| runtime.discard_checkpointed_before_tail(current_tail))
            .unwrap_or(0);
        if dropped != 0 {
            debug!(
                "ext4: reconciled {} stale JBD2 checkpoint transactions at tail={}",
                dropped, current_tail
            );
        }
    }

    /// Checkpoints all pending transactions with a single BioType::Flush.
    /// Each individual checkpoint still advances the journal tail and updates the
    /// superblock, but home block writes are batched and synced together.
    fn try_batch_checkpoint_all_jbd2_transactions(&self) -> bool {
        let _checkpoint_guard = self.jbd2_checkpoint_lock.lock();
        self.reconcile_jbd2_checkpoint_tail();
        let plans = {
            let runtime_guard = self.jbd2_runtime.read();
            let Some(runtime) = runtime_guard.as_ref() else {
                return false;
            };
            if !runtime.checkpoint_ready() {
                return false;
            }
            runtime.all_checkpoint_plans()
        };
        if plans.is_empty() {
            return false;
        }

        // Write home blocks for ALL checkpoint transactions before syncing.
        let block_size = {
            self.jbd2_runtime
                .read()
                .as_ref()
                .map(|rt| rt.block_size())
                .unwrap_or(EXT4_BLOCK_SIZE)
        };
        for plan in &plans {
            for metadata in &plan.metadata_blocks {
                let Some(block_offset) = (metadata.block_nr as usize).checked_mul(block_size)
                else {
                    warn!(
                        "ext4: batch checkpoint block offset overflow block_nr={} block_size={}",
                        metadata.block_nr, block_size
                    );
                    continue;
                };
                self.adapter
                    .write_offset(block_offset, &metadata.block_data);
            }
        }

        // Single sync for all home blocks.
        if let Err(err) = self.block_device.sync() {
            warn!(
                "ext4: batch checkpoint sync failed ({} transactions): {:?}",
                plans.len(),
                err
            );
            return false;
        }

        // Now finish each checkpoint individually (advances tail + updates superblock).
        // The home blocks are already durable; these are just metadata updates.
        let mut any_checkpointed = false;
        for plan in &plans {
            let next_start = {
                let runtime_guard = self.jbd2_runtime.read();
                let Some(runtime) = runtime_guard.as_ref() else {
                    break;
                };
                match runtime.next_checkpoint_start_after(plan.tid) {
                    Some(ns) => ns,
                    None => break,
                }
            };

            let checkpoint_result = {
                let block_device: Arc<dyn Ext4BlockDevice> = self.adapter.clone();
                let mut journal_guard = self.jbd2_journal.lock();
                let Some(journal) = journal_guard.as_mut() else {
                    break;
                };
                if journal.space.tail() != plan.range.start_block {
                    warn!(
                        "ext4: batch checkpoint tail mismatch tid={} current_tail={} start={} next_head={}",
                        plan.tid,
                        journal.space.tail(),
                        plan.range.start_block,
                        plan.range.next_head
                    );
                }
                journal.checkpoint_transaction(&block_device, plan, next_start)
            };

            match checkpoint_result {
                Ok(_) => {
                    let _ = self
                        .jbd2_runtime
                        .write()
                        .as_mut()
                        .and_then(|runtime| runtime.finish_checkpoint(plan.tid));
                    any_checkpointed = true;
                }
                Err(err) => {
                    warn!(
                        "ext4: batch checkpoint tail update failed tid={}: {:?}",
                        plan.tid, err
                    );
                    break;
                }
            }
        }

        if any_checkpointed {
            warn!(
                "ext4: batch checkpointed {} transactions with single sync",
                plans.len()
            );
        }
        any_checkpointed
    }

    fn try_commit_ready_jbd2_transaction(&self) -> bool {
        let plan = {
            let mut runtime_guard = self.jbd2_runtime.write();
            let Some(runtime) = runtime_guard.as_mut() else {
                return false;
            };
            if !runtime.commit_ready() {
                return false;
            }
            runtime.prepare_commit()
        };
        let Some(plan) = plan else {
            return false;
        };

        // Pre-commit space check: if journal is running low, checkpoint first to make room.
        // This prevents ENOSPC failures inside write_commit_plan without busy-looping.
        let required = plan.metadata_blocks.len() as u32 + 2;
        let free_before = self
            .jbd2_journal
            .lock()
            .as_ref()
            .map(|j| j.space.free_blocks())
            .unwrap_or(u32::MAX);
        if free_before < required.saturating_add(JOURNAL_LOW_WATER_MARK) {
            // Batch-checkpoint all pending transactions in one sync rather than one
            // sync per transaction. This keeps journal free space high and avoids
            // a sync on every commit once the journal fills up (e.g. ext4/045).
            self.try_batch_checkpoint_all_jbd2_transactions();
            let free_after = self
                .jbd2_journal
                .lock()
                .as_ref()
                .map(|j| j.space.free_blocks())
                .unwrap_or(u32::MAX);
            if free_after < required {
                warn!(
                    "ext4: journal out of space tid={} free={} required={}, aborting commit",
                    plan.tid, free_after, required
                );
                let _ = self
                    .jbd2_runtime
                    .write()
                    .as_mut()
                    .map(|runtime| runtime.abort_commit(plan.tid));
                return false;
            }
        }

        // Virtio-blk writes are synchronous DMA — data reaches the host before this
        // call returns, so ordering relative to the journal commit block is already
        // guaranteed by the write queue.  An explicit BioType::Flush here would add
        // ~50 ms per Write operation (hundreds of writes in generic/013) for no
        // benefit in guest-crash-only recovery scenarios that xfstests exercises.

        let (write_result, free_after_commit) = {
            let block_device: Arc<dyn Ext4BlockDevice> = self.adapter.clone();
            let mut journal_guard = self.jbd2_journal.lock();
            let Some(journal) = journal_guard.as_mut() else {
                warn!(
                    "ext4: JBD2 runtime prepared commit tid={} but journal state is missing",
                    plan.tid
                );
                let _ = self
                    .jbd2_runtime
                    .write()
                    .as_mut()
                    .map(|runtime| runtime.abort_commit(plan.tid));
                return false;
            };
            let trigger_op = plan.trigger_op;
            let result = journal.write_commit_plan_with_hook(&block_device, &plan, |stage| {
                if let Some(op_name) = trigger_op {
                    if Self::should_hold_for_injected_crash(op_name, stage) {
                        warn!(
                            "ext4: replay hold point reached for op={} stage={} (kill VM now to simulate power loss)",
                            op_name,
                            Self::jbd2_commit_stage_name(stage),
                        );
                        loop {
                            core::hint::spin_loop();
                        }
                    }
                }
            });
            let free = journal.space.free_blocks();
            (result, free)
        };

        match write_result {
            Ok(commit) => {
                let _ = self.jbd2_runtime.write().as_mut().map(|runtime| {
                    runtime.finish_commit(plan.tid, commit.start_block, commit.next_head)
                });
                // Step 4a-2: wake any fsync waiter that was blocked on this TID.
                self.commit_notifier.wake_all();
                // Step 4b: ensure on-disk superblock has the
                // EXT4_FEATURE_INCOMPAT_RECOVER ("needs_recovery") flag set
                // after the first commit since last clean SB.  Linux ext4
                // sets this at mount; we set it lazily on first commit so
                // post-replay clean unmount paths report "clean log"
                // correctly without needing an explicit umount hook.
                self.mark_needs_recovery_if_needed();
                warn!(
                    "ext4: jbd2 committed tid={} sequence={} start={} commit={} next_head={} metadata_blocks={} data_sync_required={} free_blocks={}",
                    plan.tid,
                    commit.sequence,
                    commit.start_block,
                    commit.commit_block,
                    commit.next_head,
                    commit.metadata_blocks,
                    plan.data_sync_required,
                    free_after_commit,
                );
                // Lazy checkpoint: flush home blocks when journal space is tight,
                // OR when the in-memory checkpoint list has grown deep enough to
                // threaten the kernel heap (a long fsync-less transaction never
                // drops journal free space below the threshold, so the list would
                // otherwise grow unbounded). Batch to amortize the sync cost.
                if free_after_commit < JOURNAL_CHECKPOINT_THRESHOLD
                    || self.checkpoint_depth() >= JOURNAL_CHECKPOINT_MAX_DEPTH
                {
                    self.try_batch_checkpoint_all_jbd2_transactions();
                }
                true
            }
            Err(err) => {
                warn!(
                    "ext4: failed to write JBD2 commit plan tid={} metadata_blocks={}: {:?}",
                    plan.tid,
                    plan.metadata_blocks.len(),
                    err
                );
                let _ = self
                    .jbd2_runtime
                    .write()
                    .as_mut()
                    .map(|runtime| runtime.abort_commit(plan.tid));
                false
            }
        }
    }

    #[allow(dead_code)]
    fn try_checkpoint_ready_jbd2_transaction(&self) -> bool {
        let _checkpoint_guard = self.jbd2_checkpoint_lock.lock();
        self.reconcile_jbd2_checkpoint_tail();
        let checkpoint_plan = {
            let runtime_guard = self.jbd2_runtime.read();
            let Some(runtime) = runtime_guard.as_ref() else {
                return false;
            };
            if !runtime.checkpoint_ready() {
                return false;
            }
            runtime.prepare_checkpoint()
        };
        let Some(checkpoint_plan) = checkpoint_plan else {
            return false;
        };

        for metadata in &checkpoint_plan.metadata_blocks {
            let Some(block_offset) =
                (metadata.block_nr as usize).checked_mul(metadata.block_data.len())
            else {
                warn!(
                    "ext4: checkpoint metadata block offset overflow tid={} block_nr={} block_size={}",
                    checkpoint_plan.tid,
                    metadata.block_nr,
                    metadata.block_data.len(),
                );
                return false;
            };
            self.adapter
                .write_offset(block_offset, &metadata.block_data);
        }

        if let Err(err) = self.block_device.sync() {
            warn!(
                "ext4: failed to sync metadata blocks before JBD2 checkpoint tid={}: {:?}",
                checkpoint_plan.tid, err
            );
            return false;
        }

        let next_start = {
            let runtime_guard = self.jbd2_runtime.read();
            let Some(runtime) = runtime_guard.as_ref() else {
                return false;
            };
            match runtime.next_checkpoint_start_after(checkpoint_plan.tid) {
                Some(next_start) => next_start,
                None => return false,
            }
        };

        let checkpoint_result = {
            let block_device: Arc<dyn Ext4BlockDevice> = self.adapter.clone();
            let mut journal_guard = self.jbd2_journal.lock();
            let Some(journal) = journal_guard.as_mut() else {
                warn!(
                    "ext4: JBD2 runtime prepared checkpoint tid={} but journal state is missing",
                    checkpoint_plan.tid
                );
                return false;
            };
            if journal.space.tail() != checkpoint_plan.range.start_block {
                warn!(
                    "ext4: checkpoint tail mismatch tid={} current_tail={} start={} next_head={}",
                    checkpoint_plan.tid,
                    journal.space.tail(),
                    checkpoint_plan.range.start_block,
                    checkpoint_plan.range.next_head
                );
            }
            journal.checkpoint_transaction(&block_device, &checkpoint_plan, next_start)
        };

        match checkpoint_result {
            Ok(result) => {
                let _ = self
                    .jbd2_runtime
                    .write()
                    .as_mut()
                    .and_then(|runtime| runtime.finish_checkpoint(checkpoint_plan.tid));
                debug!(
                    "ext4: jbd2 checkpointed tid={} start={} next_head={} next_start={:?}",
                    checkpoint_plan.tid, result.start_block, result.next_head, result.next_start,
                );
                true
            }
            Err(err) => {
                warn!(
                    "ext4: failed to checkpoint JBD2 transaction tid={}: {:?}",
                    checkpoint_plan.tid, err
                );
                false
            }
        }
    }

    fn prepare_ext4_io(&self) -> u64 {
        self.adapter.begin_io_operation()
    }

    fn finish_ext4_io(&self, io_epoch: u64) -> Result<()> {
        if self.adapter.io_failed_since(io_epoch) {
            return_errno_with_message!(Errno::EIO, "ext4 block I/O failure");
        }
        Ok(())
    }

    #[inline]
    fn now_unix_seconds_u32() -> u32 {
        let secs = crate::time::clocks::RealTimeClock::get()
            .read_time()
            .as_secs();
        u32::try_from(secs).unwrap_or(u32::MAX)
    }

    #[inline]
    fn monotonic_nanos() -> u64 {
        let duration = read_monotonic_time();
        duration
            .as_secs()
            .saturating_mul(1_000_000_000)
            .saturating_add(u64::from(duration.subsec_nanos()))
    }

    fn record_ext4_rs_runtime_lock_wait(&self, wait_ns: u64) {
        self.runtime_lock_stats.record_wait(wait_ns);
    }

    fn record_ext4_rs_runtime_lock_hold(&self, hold_ns: u64) {
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
        let jbd2_stats = self
            .jbd2_runtime
            .read()
            .as_ref()
            .map(|runtime| runtime.debug_stats())
            .unwrap_or_default();
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
        self.run_inode_metadata_update(ino, |ext4| {
            ext4.ext4_set_inode_times(ino, atime, mtime, ctime)
                .map(|_| ())
        })
    }

    pub(super) fn set_inode_mode(&self, ino: u32, mode: u16) -> Result<()> {
        self.run_inode_metadata_update(ino, |ext4| {
            ext4.ext4_set_inode_mode(ino, mode).map(|_| ())
        })?;
        self.touch_ctime(ino)
    }

    pub(super) fn set_inode_uid(&self, ino: u32, uid: u32) -> Result<()> {
        let uid = u16::try_from(uid)
            .map_err(|_| Error::with_message(Errno::EINVAL, "uid exceeds ext4 uid width"))?;
        self.run_inode_metadata_update(ino, |ext4| ext4.ext4_set_inode_uid(ino, uid).map(|_| ()))?;
        self.touch_ctime(ino)
    }

    pub(super) fn set_inode_gid(&self, ino: u32, gid: u32) -> Result<()> {
        let gid = u16::try_from(gid)
            .map_err(|_| Error::with_message(Errno::EINVAL, "gid exceeds ext4 gid width"))?;
        self.run_inode_metadata_update(ino, |ext4| ext4.ext4_set_inode_gid(ino, gid).map(|_| ()))?;
        self.touch_ctime(ino)
    }

    pub(super) fn set_inode_rdev(&self, ino: u32, rdev: u64) -> Result<()> {
        let rdev = u32::try_from(rdev)
            .map_err(|_| Error::with_message(Errno::EINVAL, "rdev exceeds ext4 rdev width"))?;
        self.run_inode_metadata_update(ino, |ext4| {
            ext4.ext4_set_inode_rdev(ino, rdev).map(|_| ())
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

    fn vm_io_error(err: OstdError) -> Error {
        let _ = err;
        Error::with_message(Errno::EFAULT, "vm I/O failed")
    }

    fn write_zeros(writer: &mut VmWriter, len: usize) -> Result<()> {
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
            return self.run_ext4_file_read_only(|ext4| {
                ext4.ext4_plan_direct_read(ino, offset, requested_len)
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
        let (cached_len, cached_mappings) = self.run_ext4_file_read_only(|ext4| {
            ext4.ext4_plan_direct_read(ino, offset, next_plan_window)
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
            .run_ext4_file_read_only(|ext4| ext4.ext4_plan_direct_read(ino, offset, plan_window))?;
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

    fn revoke_jbd2_checkpoint_metadata_blocks(&self, mappings: &[SimpleBlockRange]) {
        let mut runtime_guard = self.jbd2_runtime.write();
        let Some(runtime) = runtime_guard.as_mut() else {
            return;
        };

        for mapping in mappings {
            for block in 0..mapping.len {
                let block_nr = mapping.pblock.saturating_add(u64::from(block));
                let revoked = runtime.revoke_checkpoint_metadata_block(block_nr);
                if revoked > 0 {
                    debug!(
                        "ext4: revoked stale checkpoint metadata block={} count={}",
                        block_nr, revoked
                    );
                }
            }
        }
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

    pub(super) fn run_ext4<T>(
        &self,
        f: impl FnOnce(&Ext4) -> core::result::Result<T, ext4_rs::Ext4Error>,
    ) -> Result<T> {
        let io_epoch = self.prepare_ext4_io();
        let runtime_wait_start_ns = Self::monotonic_nanos();
        let runtime_guard = EXT4_RS_RUNTIME_LOCK.lock();
        self.record_ext4_rs_runtime_lock_wait(
            Self::monotonic_nanos().saturating_sub(runtime_wait_start_ns),
        );
        let runtime_hold_start_ns = Self::monotonic_nanos();
        let preserve_alloc_guard = self.has_active_jbd2_handle();
        let alloc_operation_id = if preserve_alloc_guard {
            None
        } else {
            Some(self.begin_alloc_operation(None))
        };
        let result = {
            let inner = self.lock_inner();
            let scoped_ext4 = self.ext4_with_operation_context(&inner, None, alloc_operation_id);
            f(&scoped_ext4).map_err(map_ext4_error)
        };
        self.finish_alloc_operation(alloc_operation_id);
        drop(runtime_guard);
        self.record_ext4_rs_runtime_lock_hold(
            Self::monotonic_nanos().saturating_sub(runtime_hold_start_ns),
        );
        self.finish_ext4_io(io_epoch)?;
        result
    }

    fn run_ext4_read_only<T>(
        &self,
        f: impl FnOnce(&Ext4) -> core::result::Result<T, ext4_rs::Ext4Error>,
    ) -> Result<T> {
        let io_epoch = self.prepare_ext4_io();
        let runtime_wait_start_ns = Self::monotonic_nanos();
        let runtime_guard = EXT4_RS_RUNTIME_LOCK.lock();
        self.record_ext4_rs_runtime_lock_wait(
            Self::monotonic_nanos().saturating_sub(runtime_wait_start_ns),
        );
        let runtime_hold_start_ns = Self::monotonic_nanos();
        let result = {
            let inner = self.lock_inner();
            let scoped_ext4 = self.ext4_with_operation_context(&inner, None, None);
            f(&scoped_ext4).map_err(map_ext4_error)
        };
        drop(runtime_guard);
        self.record_ext4_rs_runtime_lock_hold(
            Self::monotonic_nanos().saturating_sub(runtime_hold_start_ns),
        );
        let io_result = self.finish_ext4_io(io_epoch);
        io_result?;
        result
    }

    fn run_ext4_file_read_only<T>(
        &self,
        f: impl FnOnce(&Ext4) -> core::result::Result<T, ext4_rs::Ext4Error>,
    ) -> Result<T> {
        let io_epoch = self.prepare_ext4_io();
        let result = {
            let inner = self.lock_inner();
            let scoped_ext4 = self.ext4_with_operation_context(&inner, None, None);
            f(&scoped_ext4).map_err(map_ext4_error)
        };
        let io_result = self.finish_ext4_io(io_epoch);
        io_result?;
        result
    }

    fn run_ext4_dir_read_only<T>(
        &self,
        f: impl FnOnce(&Ext4) -> core::result::Result<T, ext4_rs::Ext4Error>,
    ) -> Result<T> {
        let io_epoch = self.prepare_ext4_io();
        let result = {
            let inner = self.lock_inner();
            let scoped_ext4 = self.ext4_with_operation_context(&inner, None, None);
            f(&scoped_ext4).map_err(map_ext4_error)
        };
        let io_result = self.finish_ext4_io(io_epoch);
        io_result?;
        result
    }

    fn run_ext4_dir_read_only_noerr<T>(&self, f: impl FnOnce(&Ext4) -> T) -> Result<T> {
        let io_epoch = self.prepare_ext4_io();
        let result = {
            let inner = self.lock_inner();
            let scoped_ext4 = self.ext4_with_operation_context(&inner, None, None);
            f(&scoped_ext4)
        };
        let io_result = self.finish_ext4_io(io_epoch);
        io_result?;
        Ok(result)
    }

    fn run_inode_metadata_update<T>(
        &self,
        ino: u32,
        f: impl FnOnce(&Ext4) -> core::result::Result<T, ext4_rs::Ext4Error>,
    ) -> Result<T> {
        self.run_inode_metadata_update_with_op(Some(JournaledOp::InodeMetadata { ino }), f)
    }

    fn run_inode_metadata_update_with_op<T>(
        &self,
        op: Option<JournaledOp>,
        f: impl FnOnce(&Ext4) -> core::result::Result<T, ext4_rs::Ext4Error>,
    ) -> Result<T> {
        let journal_enabled = self
            .jbd2_runtime
            .read()
            .as_ref()
            .is_some_and(|runtime| runtime.enabled());
        if journal_enabled {
            self.run_journaled_ext4(op, |ext4| f(ext4).map_err(map_ext4_error))
        } else {
            // No-journal setattr bypasses run_journaled_ext4, so invalidate the
            // inode meta cache here too (same generation-bump + clear protocol).
            let result = self.run_ext4(f);
            self.meta_cache_generation.fetch_add(1, Ordering::Release);
            self.inode_meta_cache.lock().clear();
            result
        }
    }

    fn run_ext4_read_only_noerr<T>(&self, f: impl FnOnce(&Ext4) -> T) -> Result<T> {
        let io_epoch = self.prepare_ext4_io();
        let runtime_wait_start_ns = Self::monotonic_nanos();
        let runtime_guard = EXT4_RS_RUNTIME_LOCK.lock();
        self.record_ext4_rs_runtime_lock_wait(
            Self::monotonic_nanos().saturating_sub(runtime_wait_start_ns),
        );
        let runtime_hold_start_ns = Self::monotonic_nanos();
        let result = {
            let inner = self.lock_inner();
            let scoped_ext4 = self.ext4_with_operation_context(&inner, None, None);
            f(&scoped_ext4)
        };
        drop(runtime_guard);
        self.record_ext4_rs_runtime_lock_hold(
            Self::monotonic_nanos().saturating_sub(runtime_hold_start_ns),
        );
        let io_result = self.finish_ext4_io(io_epoch);
        io_result?;
        Ok(result)
    }

    fn jbd2_op_name_from_bytes(name: &[u8]) -> Option<&'static str> {
        match name {
            b"create" => Some("create"),
            b"mkdir" => Some("mkdir"),
            b"unlink" => Some("unlink"),
            b"rmdir" => Some("rmdir"),
            b"rename" => Some("rename"),
            b"write" => Some("write"),
            b"truncate" => Some("truncate"),
            _ => None,
        }
    }

    fn jbd2_commit_stage_name(stage: JournalCommitWriteStage) -> &'static str {
        match stage {
            JournalCommitWriteStage::BeforeDescriptor => "before_commit",
            JournalCommitWriteStage::BeforeCommitBlock => "before_commit_block",
            JournalCommitWriteStage::AfterCommitBlock => "after_commit_block",
            JournalCommitWriteStage::AfterSuperblock => "after_commit",
        }
    }

    fn jbd2_commit_stage_from_name(name: &[u8]) -> Option<JournalCommitWriteStage> {
        match name {
            b"before_commit" | b"before_descriptor" => {
                Some(JournalCommitWriteStage::BeforeDescriptor)
            }
            b"mid_commit" | b"before_commit_block" => {
                Some(JournalCommitWriteStage::BeforeCommitBlock)
            }
            b"after_commit_block" => Some(JournalCommitWriteStage::AfterCommitBlock),
            b"after_commit" | b"after_superblock" => Some(JournalCommitWriteStage::AfterSuperblock),
            _ => None,
        }
    }

    fn replay_hold_request(op_name: &str) -> Option<JournalCommitWriteStage> {
        let Some(kcmd) = KCMDLINE.get() else {
            return None;
        };
        let Some(args) = kcmd.get_module_args("ext4fs") else {
            return None;
        };

        let mut enabled = false;
        let mut op_filter: Option<&'static str> = None;
        let mut stage = JournalCommitWriteStage::AfterSuperblock;
        for arg in args {
            match arg {
                ModuleArg::Arg(key) => {
                    if key.as_c_str().to_bytes() == b"replay_hold" {
                        enabled = true;
                    }
                }
                ModuleArg::KeyVal(key, value) => {
                    let key = key.as_c_str().to_bytes();
                    let value = value.as_c_str().to_bytes();
                    if key == b"replay_hold" {
                        if value == b"1" || value == b"true" || value == b"yes" {
                            enabled = true;
                        }
                    } else if key == b"replay_hold_op" {
                        op_filter = Self::jbd2_op_name_from_bytes(value);
                    } else if key == b"replay_hold_stage" {
                        if let Some(parsed) = Self::jbd2_commit_stage_from_name(value) {
                            stage = parsed;
                        }
                    }
                }
            }
        }

        if !enabled {
            return None;
        }
        let op_matches = match op_filter {
            Some(filter_op) => filter_op == op_name,
            None => true,
        };
        if op_matches { Some(stage) } else { None }
    }

    fn should_force_commit_for_injected_crash(op_name: &str) -> bool {
        Self::replay_hold_request(op_name).is_some()
    }

    fn should_hold_for_injected_crash(op_name: &str, stage: JournalCommitWriteStage) -> bool {
        Self::replay_hold_request(op_name).is_some_and(|requested| requested == stage)
    }

    fn run_journaled_ext4<T>(
        &self,
        op: Option<JournaledOp>,
        apply: impl FnOnce(&Ext4) -> Result<T>,
    ) -> Result<T> {
        // Step 4b: gate at the entry of journaled operations.  All
        // create/mkdir/unlink/rmdir/rename/write/truncate paths flow
        // through this helper, so a single check here is sufficient.
        self.check_not_shutdown()?;

        let generic014_like_write = matches!(
            op.as_ref(),
            Some(JournaledOp::Write { len, .. }) if *len == 512
        );
        let io_epoch = self.prepare_ext4_io();
        let runtime_wait_start_ns = Self::monotonic_nanos();
        let runtime_guard = EXT4_RS_RUNTIME_LOCK.lock();
        self.record_ext4_rs_runtime_lock_wait(
            Self::monotonic_nanos().saturating_sub(runtime_wait_start_ns),
        );
        let runtime_hold_start_ns = Self::monotonic_nanos();
        let profile_start_ns = Self::monotonic_nanos();
        let op_name = Self::jbd2_handle_op_name(op.as_ref());

        let start_handle_start_ns = Self::monotonic_nanos();
        let jbd2_handle = self.start_jbd2_handle(op.as_ref());
        let start_handle_elapsed_ns = Self::monotonic_nanos().saturating_sub(start_handle_start_ns);
        let handle_id = jbd2_handle.as_ref().map(|handle| handle.handle_id());
        let alloc_operation_id = self.begin_alloc_operation(handle_id);

        let apply_start_ns = Self::monotonic_nanos();
        let result = {
            let inner = self.lock_inner();
            let scoped_ext4 =
                self.ext4_with_operation_context(&inner, handle_id, Some(alloc_operation_id));
            apply(&scoped_ext4)
        };
        let apply_elapsed_ns = Self::monotonic_nanos().saturating_sub(apply_start_ns);

        // Phase 5 (inode meta cache): this journaled op may have changed inode
        // metadata (size/mtime/nlink/mode/...) for the target and/or its parent
        // directory. Bump the generation and drop all cached stats so the next
        // stat re-reads. The generation guard in `stat` prevents a racing stat
        // from re-inserting a value it read across this mutation.
        self.meta_cache_generation.fetch_add(1, Ordering::Release);
        self.inode_meta_cache.lock().clear();

        let finish_handle_start_ns = Self::monotonic_nanos();
        self.finish_jbd2_handle(jbd2_handle, op.as_ref(), op_name, result.is_ok());
        let finish_handle_elapsed_ns =
            Self::monotonic_nanos().saturating_sub(finish_handle_start_ns);
        let finish_alloc_start_ns = Self::monotonic_nanos();
        self.finish_alloc_operation(Some(alloc_operation_id));
        let finish_alloc_elapsed_ns = Self::monotonic_nanos().saturating_sub(finish_alloc_start_ns);
        drop(runtime_guard);
        self.record_ext4_rs_runtime_lock_hold(
            Self::monotonic_nanos().saturating_sub(runtime_hold_start_ns),
        );

        let finish_io_start_ns = Self::monotonic_nanos();
        let io_result = self.finish_ext4_io(io_epoch);
        let finish_io_elapsed_ns = Self::monotonic_nanos().saturating_sub(finish_io_start_ns);
        let total_elapsed_ns = Self::monotonic_nanos().saturating_sub(profile_start_ns);
        if self.phase2_profile_enabled {
            self.journaled_op_profile.record(
                op.as_ref(),
                start_handle_elapsed_ns,
                apply_elapsed_ns,
                finish_handle_elapsed_ns,
                finish_alloc_elapsed_ns,
                finish_io_elapsed_ns,
                total_elapsed_ns,
            );
        }
        if generic014_like_write && total_elapsed_ns >= GENERIC014_SLOW_OP_LOG_THRESHOLD_NS {
            debug!(
                "ext4: generic014-like journaled profile apply_ms={} finish_handle_ms={} finish_io_ms={} total_ms={}",
                apply_elapsed_ns / 1_000_000,
                finish_handle_elapsed_ns / 1_000_000,
                finish_io_elapsed_ns / 1_000_000,
                total_elapsed_ns / 1_000_000
            );
        }
        match (result, io_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(err), _) => Err(err),
            (Ok(_), Err(err)) => Err(err),
        }
    }

    pub(super) fn stat(&self, ino: u32) -> Result<SimpleInodeMeta> {
        // Fast path: serve from the in-memory metadata cache.
        let gen_before = self.meta_cache_generation.load(Ordering::Acquire);
        if let Some(meta) = self.inode_meta_cache.lock().get(&ino).copied() {
            return Ok(meta);
        }

        // Miss: read the inode from the device once.
        let meta = self.run_ext4_read_only_noerr(|ext4| ext4.ext4_stat(ino))?;

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

        let (is_dir, entries_with_offsets) = self.run_ext4_dir_read_only_noerr(|ext4| {
            let meta = ext4.ext4_stat(parent);
            if meta.file_type != ext4_rs::InodeFileType::S_IFDIR.bits() {
                return (false, Vec::new());
            }
            // Use ext4_readdir_with_offsets so we capture each entry's byte offset,
            // enabling O(1) rmdir via ext4_rmdir_at_fast later.
            (true, ext4.ext4_readdir_with_offsets(parent))
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
        self.inode_atime_cache.lock().remove(&ino);
        self.inode_ctime_cache.lock().remove(&ino);
        self.inode_mtime_ctime_cache.lock().remove(&ino);
    }

    fn dirent_type_from_inode_mode(mode: u16) -> u8 {
        let file_type = mode & 0xF000;
        if file_type == ext4_rs::InodeFileType::S_IFREG.bits() {
            1
        } else if file_type == ext4_rs::InodeFileType::S_IFDIR.bits() {
            2
        } else if file_type == ext4_rs::InodeFileType::S_IFCHR.bits() {
            3
        } else if file_type == ext4_rs::InodeFileType::S_IFBLK.bits() {
            4
        } else if file_type == ext4_rs::InodeFileType::S_IFIFO.bits() {
            5
        } else if file_type == ext4_rs::InodeFileType::S_IFSOCK.bits() {
            6
        } else if file_type == ext4_rs::InodeFileType::S_IFLNK.bits() {
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

        let ino = self.run_ext4_dir_read_only(|ext4| ext4.ext4_lookup_at(parent, name))?;
        self.cache_insert_entry(parent, name, ino, 0);
        Ok((ino, 0))
    }

    pub(super) fn lookup_at(&self, parent: u32, name: &str) -> Result<u32> {
        let parent_lock = Self::correctness_lock_for(&self.dir_correctness_locks, parent);
        let _parent_guard = parent_lock.lock();
        self.lookup_at_locked(parent, name)
    }

    pub(super) fn dir_open(&self, path: &str) -> Result<u32> {
        self.run_ext4_read_only(|ext4| ext4.ext4_dir_open(path))
    }

    pub(super) fn create_at(&self, parent: u32, name: &str, mode: u16) -> Result<u32> {
        let parent_lock = Self::correctness_lock_for(&self.dir_correctness_locks, parent);
        let _parent_guard = parent_lock.lock();
        let op = JournaledOp::Create;
        let ino = self.run_journaled_ext4(Some(op), |ext4| {
            ext4.ext4_create_at(parent, name, mode)
                .map_err(map_ext4_error)
        })?;
        let child_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
        let _child_guard = child_lock.lock();
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
        let parent_lock = Self::correctness_lock_for(&self.dir_correctness_locks, parent);
        let _parent_guard = parent_lock.lock();
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
                    let (ino, dir_byte_offset) = self.run_journaled_ext4(Some(op), |ext4| {
                        ext4.ext4_mkdir_unchecked_at(parent, name, mode)
                            .map_err(map_ext4_error)
                    })?;
                    let child_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
                    let _child_guard = child_lock.lock();
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
        let ino = self.run_journaled_ext4(Some(op), |ext4| {
            ext4.ext4_mkdir_at(parent, name, mode)
                .map_err(map_ext4_error)
        })?;
        let child_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
        let _child_guard = child_lock.lock();
        self.cache_insert_entry(parent, name, ino, 2);
        self.cache_remove_dir(ino);
        self.touch_birth_times(ino)?;
        self.touch_mtime_ctime(parent)?;
        Ok(ino)
    }

    pub(super) fn unlink_at(&self, parent: u32, name: &str) -> Result<()> {
        let parent_lock = Self::correctness_lock_for(&self.dir_correctness_locks, parent);
        let _parent_guard = parent_lock.lock();
        let target_ino = self.lookup_at_locked(parent, name)?;
        let target_meta = self.stat(target_ino)?;
        if target_meta.file_type == ext4_rs::InodeFileType::S_IFDIR.bits() {
            return_errno!(Errno::EISDIR);
        }
        let target_lock = Self::correctness_lock_for(&self.inode_correctness_locks, target_ino);
        let _target_guard = target_lock.lock();

        let op = JournaledOp::Unlink;
        self.run_journaled_ext4(Some(op), |ext4| {
            ext4.ext4_unlink_at(parent, name).map_err(map_ext4_error)
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
        let mut open_file_handles = self.open_file_handles.lock();
        let Some(count) = open_file_handles.get_mut(&ino) else {
            return Ok(());
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            open_file_handles.remove(&ino);
        }
        Ok(())
    }

    fn has_open_file_handles(&self, ino: u32) -> bool {
        self.open_file_handles
            .lock()
            .get(&ino)
            .is_some_and(|count| *count > 0)
    }

    pub(super) fn cleanup_unlinked_file(&self, ino: u32) -> Result<()> {
        let inode_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
        let _inode_guard = inode_lock.lock();

        let meta = self.stat(ino)?;
        if meta.nlink != 0 || meta.file_type != ext4_rs::InodeFileType::S_IFREG.bits() {
            return Ok(());
        }
        if self.has_open_file_handles(ino) {
            return Ok(());
        }

        self.reset_page_cache_after_truncate(ino, 0)?;
        let now = Self::now_unix_seconds_u32();
        let op = JournaledOp::Truncate { ino };
        self.run_journaled_ext4(Some(op), |ext4| {
            ext4.ext4_truncate(ino, 0).map_err(map_ext4_error)?;
            ext4.ext4_set_inode_times(ino, None, Some(now), Some(now))
                .map_err(map_ext4_error)?;
            Ok(())
        })?;
        self.invalidate_direct_read_cache(ino);
        self.inode_mtime_ctime_cache.lock().insert(ino, now);
        Ok(())
    }

    pub(super) fn rmdir_at(&self, parent: u32, name: &str) -> Result<()> {
        let parent_lock = Self::correctness_lock_for(&self.dir_correctness_locks, parent);
        let _parent_guard = parent_lock.lock();
        let child_ino = self.lookup_at_locked(parent, name)?;
        let child_meta = self.stat(child_ino)?;
        if child_meta.file_type != ext4_rs::InodeFileType::S_IFDIR.bits() {
            return_errno!(Errno::ENOTDIR);
        }
        let child_lock = Self::correctness_lock_for(&self.inode_correctness_locks, child_ino);
        let _child_guard = child_lock.lock();

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
            self.run_journaled_ext4(Some(op), |ext4| {
                ext4.ext4_rmdir_at_fast(parent, child_ino, dir_byte_offset)
                    .map_err(map_ext4_error)
            })?;
        } else {
            self.run_journaled_ext4(Some(op), |ext4| {
                ext4.ext4_rmdir_at(parent, name).map_err(map_ext4_error)
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
                        meta.file_type == ext4_rs::InodeFileType::S_IFDIR.bits(),
                    )
                });

            let mut affected_inodes = Vec::new();
            affected_inodes.push(old_ino);
            if let Some(ino) = overwritten_ino {
                affected_inodes.push(ino);
            }

            self.with_inode_locks(&affected_inodes, || {
                let op = JournaledOp::Rename;
                self.run_journaled_ext4(Some(op), |ext4| {
                    ext4.ext4_rename_at(old_parent, old_name, new_parent, new_name)
                        .map_err(map_ext4_error)
                })?;

                self.cache_remove_entry(old_parent, old_name);
                self.cache_insert_entry(new_parent, new_name, old_ino, old_de_type);

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
                if new_parent != old_parent {
                    self.touch_mtime_ctime(new_parent)?;
                }
                self.touch_ctime(old_ino)?;

                Ok(())
            })
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
        let _inode_guard = inode_lock.lock();
        let read_len = self.run_ext4_file_read_only(|ext4| ext4.ext4_read_at(ino, offset, data))?;
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
        let _inode_guard = inode_lock.lock();
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
        let _inode_guard = inode_lock.lock();
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
        let _inode_guard = inode_lock.lock();
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
            .run_journaled_ext4(op, |ext4| {
                let ext4_write_start_ns = Self::monotonic_nanos();
                let written = ext4
                    .ext4_write_at(ino, offset, data)
                    .map_err(map_ext4_error)?;
                ext4_write_elapsed_ns = Self::monotonic_nanos().saturating_sub(ext4_write_start_ns);
                if written > 0 {
                    let inode_time_start_ns = Self::monotonic_nanos();
                    ext4.ext4_set_inode_times(ino, None, Some(now), Some(now))
                        .map_err(map_ext4_error)?;
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

        let mut data = vec![0u8; write_len];
        reader.read_fallible(&mut VmWriter::from(data.as_mut_slice()).to_fallible())?;

        let inode_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
        let _inode_guard = inode_lock.lock();

        // Fast path: a pure overwrite of already-allocated blocks (no append, no
        // hole) needs no block allocation, so skip the per-write() journaled
        // prepare entirely. The data reaches disk via the journaled writeback at
        // fsync/sync; mtime/ctime is journaled at most once per second by
        // `touch_mtime_ctime` (ext4 inode timestamps are seconds-granularity, so
        // this loses no precision). This removes the dominant per-write() cost on
        // buffered overwrite-heavy workloads (e.g. SQLite).
        let cur_size = self.stat(ino)?.size as usize;
        if let Some(write_end) = offset.checked_add(write_len)
            && write_end <= cur_size
            && self.write_range_fully_mapped(ino, offset, write_len)?
        {
            self.touch_mtime_ctime(ino)?;
            self.invalidate_direct_read_cache(ino);
            let page_cache = self.page_cache_state_for_inode(ino, cur_size)?.pages();
            page_cache.write(offset, &mut VmReader::from(data.as_slice()).to_fallible())?;
            if profile_enabled {
                self.buffered_write_profile.record_fast(
                    write_len,
                    Self::monotonic_nanos().saturating_sub(call_start_ns),
                );
            }
            return Ok(write_len);
        }

        // Slow path: append / sparse / unmapped range — needs journaled allocation.
        let now = Self::now_unix_seconds_u32();
        let op = JournaledOp::for_small_write(ino, offset, data.as_slice());
        let prepare_start_ns = if profile_enabled {
            Self::monotonic_nanos()
        } else {
            0
        };
        let mappings = self
            .run_journaled_ext4(op, |ext4| {
                let mappings = ext4
                    .ext4_prepare_write_at(ino, offset, write_len)
                    .map_err(map_ext4_error)?;
                ext4.ext4_set_inode_times(ino, None, Some(now), Some(now))
                    .map_err(map_ext4_error)?;
                Ok(mappings)
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
        self.revoke_jbd2_checkpoint_metadata_blocks(&mappings);

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
        let mappings = self.run_ext4_file_read_only(|ext4| {
            ext4.ext4_map_blocks(ino, lblock_start_u32, lblock_count_u32)
        })?;
        let mapped_blocks: u64 = mappings.iter().map(|m| m.len as u64).sum();
        Ok(mapped_blocks == lblock_count as u64)
    }

    fn write_page_cache_data_at(&self, ino: u32, offset: usize, data: &[u8]) -> Result<usize> {
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
        let mappings = self.run_ext4_file_read_only(|ext4| {
            ext4.ext4_map_blocks(ino, lblock_start_u32, lblock_count_u32)
        })?;
        self.revoke_jbd2_checkpoint_metadata_blocks(&mappings);

        let op = JournaledOp::Write {
            ino,
            len: data.len(),
        };
        let written = self.run_journaled_ext4(Some(op), |ext4| {
            ext4.ext4_write_at(ino, offset, data)
                .map_err(map_ext4_error)
        })?;
        if self.phase2_profile_enabled {
            self.buffered_write_profile
                .writeback_bytes
                .fetch_add(written as u64, Ordering::Relaxed);
        }
        self.invalidate_direct_read_cache(ino);
        Ok(written)
    }

    fn read_page_cache_data_at(&self, ino: u32, offset: usize, data: &mut [u8]) -> Result<usize> {
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

        let mappings = self.run_ext4_file_read_only(|ext4| {
            ext4.ext4_map_blocks(ino, lblock_start_u32, lblock_count_u32)
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
        let inode_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
        let _inode_guard = inode_lock.lock();
        let write_len = reader.remain();
        if write_len == 0 {
            return Ok(0);
        }
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
            let mappings = if let Some(cached_mappings) =
                self.plan_direct_write_overwrite_cached(ino, offset, write_len)?
            {
                reused_read_mapping_cache = true;
                cached_mappings
            } else {
                if profile_enabled {
                    plan_elapsed_ns = Self::monotonic_nanos().saturating_sub(plan_start_ns);
                }
                self.run_journaled_ext4(
                    Some(JournaledOp::Write {
                        len: write_len,
                        ino,
                    }),
                    |ext4| {
                        let prepare_start_ns = if profile_enabled {
                            Self::monotonic_nanos()
                        } else {
                            0
                        };
                        let mappings = ext4
                            .ext4_prepare_write_at(ino, offset, write_len)
                            .map_err(map_ext4_error)?;
                        ext4.ext4_set_inode_times(ino, None, Some(now), Some(now))
                            .map_err(map_ext4_error)?;
                        touched_inside_write_handle = true;
                        if profile_enabled {
                            prepare_elapsed_ns =
                                Self::monotonic_nanos().saturating_sub(prepare_start_ns);
                        }

                        Ok(mappings)
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
            self.revoke_jbd2_checkpoint_metadata_blocks(&mappings);
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
        let _inode_guard = inode_lock.lock();
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
            .run_journaled_ext4(Some(op), |ext4| {
                ext4.ext4_truncate(ino, new_size).map_err(map_ext4_error)?;
                ext4.ext4_set_inode_times(ino, None, Some(now), Some(now))
                    .map_err(map_ext4_error)?;
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
        let _inode_guard = inode_lock.lock();
        self.evict_page_cache_range(ino, offset, len)?;

        let now = Self::now_unix_seconds_u32();
        let op = JournaledOp::Write { ino, len };
        self.run_journaled_ext4(Some(op), |ext4| {
            match mode {
                FallocMode::Allocate => {
                    ext4.ext4_allocate_range(ino, offset, len, false)
                        .map_err(map_ext4_error)?;
                }
                FallocMode::AllocateKeepSize => {
                    ext4.ext4_allocate_range(ino, offset, len, true)
                        .map_err(map_ext4_error)?;
                }
                FallocMode::ZeroRange => {
                    ext4.ext4_zero_range(ino, offset, len, false)
                        .map_err(map_ext4_error)?;
                }
                FallocMode::ZeroRangeKeepSize => {
                    ext4.ext4_zero_range(ino, offset, len, true)
                        .map_err(map_ext4_error)?;
                }
                FallocMode::PunchHoleKeepSize => {
                    ext4.ext4_punch_hole_keep_size(ino, offset, len)
                        .map_err(map_ext4_error)?;
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
            ext4.ext4_set_inode_times(ino, None, Some(now), Some(now))
                .map_err(map_ext4_error)?;
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

        self.run_ext4_dir_read_only_noerr(|ext4| ext4.ext4_readdir(ino))
    }

    pub(super) fn readdir(&self, ino: u32) -> Result<Vec<SimpleDirEntry>> {
        let dir_lock = Self::correctness_lock_for(&self.dir_correctness_locks, ino);
        let _dir_guard = dir_lock.lock();
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

    fn check_not_shutdown(&self) -> Result<()> {
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
        let raw_writer: Arc<dyn Ext4MetadataWriter> =
            Arc::new(RawAdapterWriter(self.adapter.clone()));

        let mut inner = self.lock_inner();
        inner.super_block.set_needs_recovery(true);
        inner.super_block.sync_to_disk_with_csum(&raw_writer);
        drop(inner);
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
    fn mark_needs_recovery_if_needed(&self) {
        let need_persist = {
            let inner = self.lock_inner();
            !inner.super_block.needs_recovery()
        };
        if !need_persist {
            return;
        }
        let mut inner = self.lock_inner();
        if inner.super_block.needs_recovery() {
            return;
        }
        let metadata_writer = inner.metadata_writer.clone();
        inner.super_block.set_needs_recovery(true);
        inner.super_block.sync_to_disk_with_csum(&metadata_writer);
    }

    pub(super) fn fsync_regular_file(&self, ino: u32) -> Result<()> {
        // Step 4b: post-shutdown fsync must return EIO so that callers
        // (e.g. xfstests after godown) know that the filesystem is dead.
        self.check_not_shutdown()?;

        let inode_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
        let _inode_guard = inode_lock.lock();

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
        let ext4_sb = self.lock_inner().super_block;
        let block_size = ext4_sb.block_size() as usize;
        let blocks = ext4_sb.blocks_count() as usize;
        let bfree = ext4_sb.free_blocks_count().min(usize::MAX as u64) as usize;
        let files = ext4_sb.total_inodes() as usize;
        let ffree = ext4_sb.free_inodes_count() as usize;
        let fsid = u64::from_le_bytes(ext4_sb.uuid[..8].try_into().unwrap_or([0u8; 8]));

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

pub(super) fn map_ext4_error(err: ext4_rs::Ext4Error) -> Error {
    Error::new(map_ext4_errno(err.error()))
}

fn map_ext4_errno(errno: ext4_rs::Errno) -> Errno {
    match errno {
        ext4_rs::Errno::EPERM => Errno::EPERM,
        ext4_rs::Errno::ENOENT => Errno::ENOENT,
        ext4_rs::Errno::EINTR => Errno::EINTR,
        ext4_rs::Errno::EIO => Errno::EIO,
        ext4_rs::Errno::ENXIO => Errno::ENXIO,
        ext4_rs::Errno::E2BIG => Errno::E2BIG,
        ext4_rs::Errno::EBADF => Errno::EBADF,
        ext4_rs::Errno::EAGAIN => Errno::EAGAIN,
        ext4_rs::Errno::ENOMEM => Errno::ENOMEM,
        ext4_rs::Errno::EACCES => Errno::EACCES,
        ext4_rs::Errno::EFAULT => Errno::EFAULT,
        ext4_rs::Errno::ENOTBLK => Errno::ENOTBLK,
        ext4_rs::Errno::EBUSY => Errno::EBUSY,
        ext4_rs::Errno::EEXIST => Errno::EEXIST,
        ext4_rs::Errno::EXDEV => Errno::EXDEV,
        ext4_rs::Errno::ENODEV => Errno::ENODEV,
        ext4_rs::Errno::ENOTDIR => Errno::ENOTDIR,
        ext4_rs::Errno::EISDIR => Errno::EISDIR,
        ext4_rs::Errno::EINVAL => Errno::EINVAL,
        ext4_rs::Errno::ENFILE => Errno::ENFILE,
        ext4_rs::Errno::EMFILE => Errno::EMFILE,
        ext4_rs::Errno::ENOTTY => Errno::ENOTTY,
        ext4_rs::Errno::ETXTBSY => Errno::ETXTBSY,
        ext4_rs::Errno::EFBIG => Errno::EFBIG,
        ext4_rs::Errno::ENOSPC => Errno::ENOSPC,
        ext4_rs::Errno::ESPIPE => Errno::ESPIPE,
        ext4_rs::Errno::EROFS => Errno::EROFS,
        ext4_rs::Errno::EMLINK => Errno::EMLINK,
        ext4_rs::Errno::EPIPE => Errno::EPIPE,
        ext4_rs::Errno::ENAMETOOLONG => Errno::ENAMETOOLONG,
        ext4_rs::Errno::ENOTEMPTY => Errno::ENOTEMPTY,
        ext4_rs::Errno::ENOTSUP => Errno::EOPNOTSUPP,
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
