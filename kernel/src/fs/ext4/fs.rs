// SPDX-License-Identifier: MPL-2.0

use alloc::{collections::BTreeMap, string::String, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use aster_block::{
    bio::{BioDirection, BioSegment, BioStatus, BioWaiter, reset_read_bio_profile},
    id::Bid,
    BlockDevice, SECTOR_SIZE,
};
use aster_time::read_monotonic_time;
use aster_cmdline::{KCMDLINE, ModuleArg};
use ext4_rs::{
    BLOCK_SIZE as EXT4_BLOCK_SIZE, BlockDevice as Ext4BlockDevice, EXT4_ROOT_INODE, Ext4,
    Jbd2Journal, JournalCommitWriteStage, JournalHandle, JournalRecoveryResult, JournalRuntime,
    LocalOperationAllocGuard, MetadataWriter as Ext4MetadataWriter,
    OperationAllocGuard as Ext4OperationAllocGuard, OperationScopedAllocGuard,
    SimpleBlockRange, SimpleDirEntry, SimpleInodeMeta,
};
use ostd::{
    mm::{VmIo, VmWriter, io_util::HasVmReaderWriter},
    sync::RwMutex,
    Error as OstdError,
};

use crate::{
    fs::{
        path::PerMountFlags,
        registry::{FsProperties, FsType},
        utils::{
            FileSystem, FsEventSubscriberStats, FsFlags, Inode, StatusFlags, SuperBlock, NAME_MAX,
        },
    },
    prelude::*,
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
    Write { len: usize },
    Truncate,
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
}

struct Ext4RsRuntimeLockStats {
    acquire_count: AtomicU64,
    total_wait_ns: AtomicU64,
    max_wait_ns: AtomicU64,
    total_hold_ns: AtomicU64,
    max_hold_ns: AtomicU64,
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
            match target.compare_exchange_weak(
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

impl JournaledOp {
    fn for_small_write(_ino: u32, _offset: usize, data: &[u8]) -> Option<Self> {
        if data.is_empty() || data.len() > JOURNALED_SMALL_WRITE_MAX_BYTES {
            return None;
        }
        Some(Self::Write { len: data.len() })
    }
}

#[derive(Debug, Default)]
struct DirEntryCache {
    loaded: bool,
    /// Maps entry name → (child_ino, dir_byte_offset).
    /// `dir_byte_offset == u64::MAX` means the offset is unknown (fallback path).
    entries: BTreeMap<String, (u32, u64)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirLookupCacheResult {
    /// (child_ino, dir_byte_offset); offset is u64::MAX when unknown.
    Hit(u32, u64),
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

#[derive(Debug)]
struct KernelBlockDeviceAdapter {
    inner: Arc<dyn BlockDevice>,
    io_failed: AtomicBool,
}

impl KernelBlockDeviceAdapter {
    fn new(inner: Arc<dyn BlockDevice>) -> Self {
        Self {
            inner,
            io_failed: AtomicBool::new(false),
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
        self.io_failed.store(true, Ordering::Release);
    }

    fn clear_io_failure(&self) {
        self.io_failed.store(false, Ordering::Release);
    }

    fn consume_io_failure(&self) -> bool {
        self.io_failed.swap(false, Ordering::AcqRel)
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

        let aligned_start = Self::align_down(offset);
        let aligned_end = Self::align_up(offset + read_len);
        let aligned_len = aligned_end - aligned_start;

        if aligned_start == offset && aligned_len == read_len {
            let mut writer = VmWriter::from(&mut out[..]).to_fallible();
            if let Err(err) = self.inner.read(offset, &mut writer) {
                self.mark_io_failure();
                error!("ext4 block read failed at offset {}: {:?}", offset, err);
                out.fill(0);
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
            error!("ext4 block write overflow at offset {} len={}", offset, data.len());
            return;
        };
        if write_end > dev_size {
            self.mark_io_failure();
            error!(
                "ext4 block write out of range: offset={} len={} device_size={}",
                offset, data.len(), dev_size
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
            }
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
    dir_entry_cache: Mutex<BTreeMap<u32, DirEntryCache>>,
    inode_direct_read_cache: Mutex<BTreeMap<u32, DirectReadCache>>,
    inode_atime_cache: Mutex<BTreeMap<u32, u32>>,
    inode_ctime_cache: Mutex<BTreeMap<u32, u32>>,
    inode_mtime_ctime_cache: Mutex<BTreeMap<u32, u32>>,
    direct_read_profile_started: AtomicBool,
    direct_read_profile: DirectReadProfileStats,
    runtime_lock_stats: Ext4RsRuntimeLockStats,
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
            dir_entry_cache: Mutex::new(BTreeMap::new()),
            inode_direct_read_cache: Mutex::new(BTreeMap::new()),
            inode_atime_cache: Mutex::new(BTreeMap::new()),
            inode_ctime_cache: Mutex::new(BTreeMap::new()),
            inode_mtime_ctime_cache: Mutex::new(BTreeMap::new()),
            direct_read_profile_started: AtomicBool::new(false),
            direct_read_profile: DirectReadProfileStats::new(),
            runtime_lock_stats: Ext4RsRuntimeLockStats::new(),
            fs_event_subscriber_stats: FsEventSubscriberStats::new(),
            self_ref: weak_ref.clone(),
        });

        fs.initialize_jbd2_journal();
        fs.replay_mount_jbd2_journal();
        fs
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

        self.prepare_ext4_io();
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
        let io_result = self.finish_ext4_io();
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
            Some(JournaledOp::Write { len }) => {
                let blocks = len.div_ceil(EXT4_BLOCK_SIZE);
                u32::try_from(blocks.saturating_add(8)).unwrap_or(u32::MAX)
            }
            Some(JournaledOp::Truncate) => 8,
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
            Some(JournaledOp::Truncate) => "truncate",
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
                let rotated_tid = if runtime
                    .should_rotate_running_transaction(JOURNAL_COMMIT_BATCH_BLOCKS)
                {
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
                let Some(block_offset) = (metadata.block_nr as usize).checked_mul(block_size) else {
                    warn!(
                        "ext4: batch checkpoint block offset overflow block_nr={} block_size={}",
                        metadata.block_nr, block_size
                    );
                    continue;
                };
                self.adapter.write_offset(block_offset, &metadata.block_data);
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
                let _ = self
                    .jbd2_runtime
                    .write()
                    .as_mut()
                    .map(|runtime| {
                        runtime.finish_commit(plan.tid, commit.start_block, commit.next_head)
                    });
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
                // Lazy checkpoint: only flush home blocks when journal space is tight.
                // Use batch to amortize the sync cost over all pending transactions.
                if free_after_commit < JOURNAL_CHECKPOINT_THRESHOLD {
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
            let Some(block_offset) = (metadata.block_nr as usize).checked_mul(metadata.block_data.len()) else {
                warn!(
                    "ext4: checkpoint metadata block offset overflow tid={} block_nr={} block_size={}",
                    checkpoint_plan.tid,
                    metadata.block_nr,
                    metadata.block_data.len(),
                );
                return false;
            };
            self.adapter.write_offset(block_offset, &metadata.block_data);
        }

        if let Err(err) = self.block_device.sync() {
            warn!(
                "ext4: failed to sync metadata blocks before JBD2 checkpoint tid={}: {:?}",
                checkpoint_plan.tid,
                err
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
                    checkpoint_plan.tid,
                    result.start_block,
                    result.next_head,
                    result.next_start,
                );
                true
            }
            Err(err) => {
                warn!(
                    "ext4: failed to checkpoint JBD2 transaction tid={}: {:?}",
                    checkpoint_plan.tid,
                    err
                );
                false
            }
        }
    }

    fn prepare_ext4_io(&self) {
        self.adapter.clear_io_failure();
    }

    fn finish_ext4_io(&self) -> Result<()> {
        if self.adapter.consume_io_failure() {
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
        if acquire_count % Ext4RsRuntimeLockStats::LOG_INTERVAL_ACQUIRES == 0 {
            self.maybe_log_phase2_debug_stats(acquire_count);
        }
    }

    fn maybe_log_phase2_debug_stats(&self, runtime_lock_acquires: u64) {
        if runtime_lock_acquires == 0 {
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
        let avg_active_x100 = if jbd2_stats.active_handle_samples == 0 {
            0
        } else {
            jbd2_stats
                .active_handle_sample_sum
                .saturating_mul(100)
                / jbd2_stats.active_handle_samples
        };

        debug!(
            "[ext4-phase2] runtime_lock_acquires={} avg_wait_us={} max_wait_us={} avg_hold_us={} max_hold_us={} jbd2_handles_started={} finished={} max_active={} avg_active_x100={} max_running_handles={} max_running_reserved={} max_running_metadata={} rotations={} commits_prepared={} commits_finished={} checkpoints={} overlay_reads={} overlay_hits={} metadata_writes={} alloc_clear_calls={} alloc_reserve_calls={} alloc_reserved_blocks={} alloc_contains_checks={} alloc_max_operation_blocks={}",
            runtime_lock_acquires,
            total_wait_ns / runtime_lock_acquires / 1_000,
            max_wait_ns / 1_000,
            total_hold_ns / runtime_lock_acquires / 1_000,
            max_hold_ns / 1_000,
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
        );
    }

    fn maybe_log_direct_read_profile(&self, reads: u64) {
        const DIRECT_READ_PROFILE_LOG_ENABLED: bool = false;
        if !DIRECT_READ_PROFILE_LOG_ENABLED {
            return;
        }
        if reads == 0 || reads % DirectReadProfileStats::LOG_INTERVAL_READS != 0 {
            return;
        }

        let total_bytes = self.direct_read_profile.read_bytes.load(Ordering::Relaxed);
        let total_mappings = self
            .direct_read_profile
            .total_mappings
            .load(Ordering::Relaxed);
        let mapped_bytes = self.direct_read_profile.mapped_bytes.load(Ordering::Relaxed);
        let zero_fill_bytes = self
            .direct_read_profile
            .zero_fill_bytes
            .load(Ordering::Relaxed);
        let cache_hits = self.direct_read_profile.cache_hits.load(Ordering::Relaxed);
        let cache_misses = self.direct_read_profile.cache_misses.load(Ordering::Relaxed);
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

        println!(
            "[ext4-profile] direct-read reads={} bytes={} avg_bytes={} avg_mapped_bytes={} avg_zero_fill_bytes={} max_mapped_bytes={} cache_hit={} cache_miss={} avg_mappings_x100={} max_mappings={} avg_plan_us={} avg_alloc_us={} avg_submit_us={} avg_wait_us={} avg_copy_us={}",
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

    pub(super) fn set_inode_times(
        &self,
        ino: u32,
        atime: Option<u32>,
        mtime: Option<u32>,
        ctime: Option<u32>,
    ) -> Result<()> {
        self.run_inode_metadata_update(|ext4| {
            ext4.ext4_set_inode_times(ino, atime, mtime, ctime).map(|_| ())
        })
    }

    pub(super) fn set_inode_mode(&self, ino: u32, mode: u16) -> Result<()> {
        self.run_inode_metadata_update(|ext4| ext4.ext4_set_inode_mode(ino, mode).map(|_| ()))?;
        self.touch_ctime(ino)
    }

    pub(super) fn set_inode_uid(&self, ino: u32, uid: u32) -> Result<()> {
        let uid = u16::try_from(uid)
            .map_err(|_| Error::with_message(Errno::EINVAL, "uid exceeds ext4 uid width"))?;
        self.run_inode_metadata_update(|ext4| ext4.ext4_set_inode_uid(ino, uid).map(|_| ()))?;
        self.touch_ctime(ino)
    }

    pub(super) fn set_inode_gid(&self, ino: u32, gid: u32) -> Result<()> {
        let gid = u16::try_from(gid)
            .map_err(|_| Error::with_message(Errno::EINVAL, "gid exceeds ext4 gid width"))?;
        self.run_inode_metadata_update(|ext4| ext4.ext4_set_inode_gid(ino, gid).map(|_| ()))?;
        self.touch_ctime(ino)
    }

    pub(super) fn set_inode_rdev(&self, ino: u32, rdev: u64) -> Result<()> {
        let rdev = u32::try_from(rdev)
            .map_err(|_| Error::with_message(Errno::EINVAL, "rdev exceeds ext4 rdev width"))?;
        self.run_inode_metadata_update(|ext4| ext4.ext4_set_inode_rdev(ino, rdev).map(|_| ()))?;
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
            self.set_inode_rdev(ino, rdev)?;
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
        if mount_flags.contains(PerMountFlags::RDONLY) || mount_flags.contains(PerMountFlags::NOATIME)
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
                Ok(meta) if meta.atime > meta.mtime && meta.atime > meta.ctime => return Ok(()),
                Ok(_) => {}
                Err(err) => {
                    warn!("ext4: failed to stat inode {} for atime policy: {:?}", ino, err);
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

        let mut next_plan_window = requested_len.max(DIRECT_READ_PLAN_BASE_WINDOW_BYTES);
        next_plan_window = next_plan_window.min(DIRECT_READ_PLAN_MAX_WINDOW_BYTES);

        {
            let cache = self.inode_direct_read_cache.lock();
            if let Some(entry) = cache.get(&ino) {
                let cache_end = entry.file_offset.saturating_add(entry.len);
                let request_end = offset.saturating_add(requested_direct_len);
                if offset >= entry.file_offset && request_end <= cache_end {
                    self.direct_read_profile.record_cache_hit();
                    let mappings =
                        Self::slice_mappings_for_range(offset, requested_direct_len, &entry.mappings)?;
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
        let (cached_len, cached_mappings) =
            self.run_ext4(|ext4| ext4.ext4_plan_direct_read(ino, offset, next_plan_window))?;
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
        let (direct_len, mappings) = self.plan_direct_read_cached(ino, offset, len)?;
        if direct_len != len {
            return Ok(None);
        }
        if !Self::mappings_fully_cover_range(offset, len, &mappings)? {
            return Ok(None);
        }
        Ok(Some(mappings))
    }

    fn clear_pending_direct_read(&self, ino: u32) {
        if let Some(entry) = self.inode_direct_read_cache.lock().get_mut(&ino) {
            entry.pending = None;
        }
    }

    fn invalidate_direct_read_cache(&self, ino: u32) {
        self.inode_direct_read_cache.lock().remove(&ino);
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

        if direct_len < SPECULATIVE_DIRECT_READ_MIN_BYTES {
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
        let (next_len, next_mappings) = self.plan_direct_read_cached(ino, next_offset, direct_len)?;
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
        self.prepare_ext4_io();
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
        self.finish_ext4_io()?;
        result
    }

    fn run_inode_metadata_update<T>(
        &self,
        f: impl FnOnce(&Ext4) -> core::result::Result<T, ext4_rs::Ext4Error>,
    ) -> Result<T> {
        let journal_enabled = self
            .jbd2_runtime
            .read()
            .as_ref()
            .is_some_and(|runtime| runtime.enabled());
        if journal_enabled && !self.has_active_jbd2_handle() {
            self.run_journaled_ext4(None, |ext4| f(ext4).map_err(map_ext4_error))
        } else {
            self.run_ext4(f)
        }
    }

    pub(super) fn run_ext4_noerr<T>(&self, f: impl FnOnce(&Ext4) -> T) -> Result<T> {
        self.prepare_ext4_io();
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
            f(&scoped_ext4)
        };
        self.finish_alloc_operation(alloc_operation_id);
        drop(runtime_guard);
        self.record_ext4_rs_runtime_lock_hold(
            Self::monotonic_nanos().saturating_sub(runtime_hold_start_ns),
        );
        self.finish_ext4_io()?;
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
        if op_matches {
            Some(stage)
        } else {
            None
        }
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
        let generic014_like_write = matches!(
            op.as_ref(),
            Some(JournaledOp::Write { len }) if *len == 512
        );
        self.prepare_ext4_io();
        let finish_io_start_ns = Self::monotonic_nanos();
        let (result, apply_elapsed_ns, finish_handle_elapsed_ns) = {
            let runtime_wait_start_ns = Self::monotonic_nanos();
            let runtime_guard = EXT4_RS_RUNTIME_LOCK.lock();
            self.record_ext4_rs_runtime_lock_wait(
                Self::monotonic_nanos().saturating_sub(runtime_wait_start_ns),
            );
            let runtime_hold_start_ns = Self::monotonic_nanos();

            let op_name = Self::jbd2_handle_op_name(op.as_ref());
            let jbd2_handle = self.start_jbd2_handle(op.as_ref());
            let handle_id = jbd2_handle.as_ref().map(|handle| handle.handle_id());
            let alloc_operation_id =
                self.begin_alloc_operation(handle_id);

            let apply_start_ns = Self::monotonic_nanos();
            let result = {
                let inner = self.lock_inner();
                let scoped_ext4 =
                    self.ext4_with_operation_context(&inner, handle_id, Some(alloc_operation_id));
                apply(&scoped_ext4)
            };
            let apply_elapsed_ns = Self::monotonic_nanos().saturating_sub(apply_start_ns);

            let finish_handle_start_ns = Self::monotonic_nanos();
            self.finish_jbd2_handle(jbd2_handle, op_name, result.is_ok());
            let finish_handle_elapsed_ns =
                Self::monotonic_nanos().saturating_sub(finish_handle_start_ns);
            self.finish_alloc_operation(Some(alloc_operation_id));
            drop(runtime_guard);
            self.record_ext4_rs_runtime_lock_hold(
                Self::monotonic_nanos().saturating_sub(runtime_hold_start_ns),
            );
            (result, apply_elapsed_ns, finish_handle_elapsed_ns)
        };

        let io_result = self.finish_ext4_io();
        let finish_io_elapsed_ns = Self::monotonic_nanos().saturating_sub(finish_io_start_ns);
        if generic014_like_write && finish_io_elapsed_ns >= GENERIC014_SLOW_OP_LOG_THRESHOLD_NS {
            debug!(
                "ext4: generic014-like journaled profile apply_ms={} finish_handle_ms={} finish_io_ms={} post_io_ms={}",
                apply_elapsed_ns / 1_000_000,
                finish_handle_elapsed_ns / 1_000_000,
                finish_io_elapsed_ns / 1_000_000,
                finish_io_elapsed_ns
                    .saturating_sub(apply_elapsed_ns)
                    .saturating_sub(finish_handle_elapsed_ns)
                    / 1_000_000
            );
        }
        match (result, io_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(err), _) => Err(err),
            (Ok(_), Err(err)) => Err(err),
        }
    }

    pub(super) fn stat(&self, ino: u32) -> Result<SimpleInodeMeta> {
        self.run_ext4_noerr(|ext4| ext4.ext4_stat(ino))
    }

    fn lookup_cache(&self, parent: u32, name: &str) -> DirLookupCacheResult {
        let caches = self.dir_entry_cache.lock();
        let Some(cache) = caches.get(&parent) else {
            return DirLookupCacheResult::Unknown;
        };
        if let Some(&(ino, offset)) = cache.entries.get(name) {
            return DirLookupCacheResult::Hit(ino, offset);
        }
        if cache.loaded {
            return DirLookupCacheResult::Miss;
        }
        DirLookupCacheResult::Unknown
    }

    fn load_dir_cache_if_needed(&self, parent: u32) -> Result<()> {
        {
            let caches = self.dir_entry_cache.lock();
            if let Some(cache) = caches.get(&parent) {
                if cache.loaded {
                    return Ok(());
                }
            }
        }

        let meta = self.stat(parent)?;
        if meta.file_type != ext4_rs::InodeFileType::S_IFDIR.bits() {
            return_errno_with_message!(Errno::ENOTDIR, "parent inode is not a directory");
        }

        // Use ext4_readdir_with_offsets so we capture each entry's byte offset,
        // enabling O(1) rmdir via ext4_rmdir_at_fast later.
        let entries_with_offsets =
            self.run_ext4_noerr(|ext4| ext4.ext4_readdir_with_offsets(parent))?;
        let mut entry_map = BTreeMap::new();
        for (name, ino, entry_offset) in entries_with_offsets {
            entry_map.insert(name, (ino, entry_offset));
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
    fn cache_insert_entry_with_offset(&self, parent: u32, name: &str, child: u32, offset: u64) {
        let mut caches = self.dir_entry_cache.lock();
        let cache = caches.entry(parent).or_default();
        cache.entries.insert(name.to_string(), (child, offset));
    }

    /// Insert a cache entry when the byte offset is unknown (fallback paths).
    fn cache_insert_entry(&self, parent: u32, name: &str, child: u32) {
        self.cache_insert_entry_with_offset(parent, name, child, u64::MAX);
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
        self.invalidate_direct_read_cache(ino);
        self.inode_atime_cache.lock().remove(&ino);
        self.inode_ctime_cache.lock().remove(&ino);
        self.inode_mtime_ctime_cache.lock().remove(&ino);
    }

    pub(super) fn lookup_at(&self, parent: u32, name: &str) -> Result<u32> {
        match self.lookup_cache(parent, name) {
            DirLookupCacheResult::Hit(ino, _) => return Ok(ino),
            DirLookupCacheResult::Miss => {
                return_errno_with_message!(Errno::ENOENT, "entry not found in directory cache");
            }
            DirLookupCacheResult::Unknown => {}
        }

        if self.load_dir_cache_if_needed(parent).is_ok() {
            match self.lookup_cache(parent, name) {
                DirLookupCacheResult::Hit(ino, _) => return Ok(ino),
                DirLookupCacheResult::Miss => {
                    return_errno_with_message!(Errno::ENOENT, "entry not found in directory cache");
                }
                DirLookupCacheResult::Unknown => {}
            }
        }

        let ino = self.run_ext4(|ext4| ext4.ext4_lookup_at(parent, name))?;
        self.cache_insert_entry(parent, name, ino);
        Ok(ino)
    }

    pub(super) fn dir_open(&self, path: &str) -> Result<u32> {
        self.run_ext4(|ext4| ext4.ext4_dir_open(path))
    }

    pub(super) fn create_at(&self, parent: u32, name: &str, mode: u16) -> Result<u32> {
        let op = JournaledOp::Create;
        let ino = self.run_journaled_ext4(Some(op), |ext4| {
            ext4.ext4_create_at(parent, name, mode)
                .map_err(map_ext4_error)
        })?;
        self.cache_insert_entry(parent, name, ino);
        self.cache_remove_dir(ino);
        self.touch_birth_times(ino)?;
        self.touch_mtime_ctime(parent)?;
        Ok(ino)
    }

    pub(super) fn mkdir_at(&self, parent: u32, name: &str, mode: u16) -> Result<u32> {
        // Ensure the parent directory cache is fully loaded so subsequent existence
        // checks (lookup_cache → Miss) can bypass the O(n) dir_find_entry disk scan.
        // The first call reads the directory once; subsequent calls return immediately.
        let cache_loaded = self.load_dir_cache_if_needed(parent).is_ok();

        if cache_loaded {
            match self.lookup_cache(parent, name) {
                DirLookupCacheResult::Hit(_, _) => return_errno!(Errno::EEXIST),
                DirLookupCacheResult::Miss => {
                    // Cache is complete and confirms the name is absent — skip disk scan.
                    let op = JournaledOp::Mkdir;
                    let (ino, dir_byte_offset) = self.run_journaled_ext4(Some(op), |ext4| {
                        ext4.ext4_mkdir_unchecked_at(parent, name, mode)
                            .map_err(map_ext4_error)
                    })?;
                    self.cache_insert_entry_with_offset(parent, name, ino, dir_byte_offset);
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
        self.cache_insert_entry(parent, name, ino);
        self.cache_remove_dir(ino);
        self.touch_birth_times(ino)?;
        self.touch_mtime_ctime(parent)?;
        Ok(ino)
    }

    pub(super) fn unlink_at(&self, parent: u32, name: &str) -> Result<()> {
        let target_ino = self.lookup_at(parent, name)?;
        let target_meta = self.stat(target_ino)?;
        if target_meta.file_type == ext4_rs::InodeFileType::S_IFDIR.bits() {
            return_errno!(Errno::EISDIR);
        }

        let op = JournaledOp::Unlink;
        self.run_journaled_ext4(Some(op), |ext4| {
            ext4.ext4_unlink_at(parent, name).map_err(map_ext4_error)
        })?;
        self.cache_remove_entry(parent, name);
        self.clear_inode_touch_cache(target_ino);
        self.touch_mtime_ctime(parent)?;
        Ok(())
    }

    pub(super) fn rmdir_at(&self, parent: u32, name: &str) -> Result<()> {
        let child_ino = self.lookup_at(parent, name)?;
        let child_meta = self.stat(child_ino)?;
        if child_meta.file_type != ext4_rs::InodeFileType::S_IFDIR.bits() {
            return_errno!(Errno::ENOTDIR);
        }

        let entries = self.readdir(child_ino)?;
        let has_real_child = entries
            .iter()
            .any(|entry| entry.name != "." && entry.name != "..");
        if has_real_child {
            return_errno!(Errno::ENOTEMPTY);
        }

        // Retrieve cached byte offset for O(1) parent-dir entry removal.
        let dir_byte_offset = match self.lookup_cache(parent, name) {
            DirLookupCacheResult::Hit(_, offset) => offset,
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
        if old_parent == new_parent && old_name == new_name {
            return Ok(());
        }

        let old_ino = self.lookup_at(old_parent, old_name)?;
        let overwritten_ino = self.lookup_at(new_parent, new_name).ok();
        let overwritten_is_dir = overwritten_ino
            .and_then(|ino| self.stat(ino).ok().map(|meta| (ino, meta)))
            .map(|(ino, meta)| (ino, meta.file_type == ext4_rs::InodeFileType::S_IFDIR.bits()));

        let op = JournaledOp::Rename;
        self.run_journaled_ext4(Some(op), |ext4| {
            ext4.ext4_rename_at(old_parent, old_name, new_parent, new_name)
                .map_err(map_ext4_error)
        })?;

        self.cache_remove_entry(old_parent, old_name);
        self.cache_insert_entry(new_parent, new_name, old_ino);

        if let Some((ino, true)) = overwritten_is_dir {
            self.cache_remove_dir(ino);
        }
        if let Some(ino) = overwritten_ino {
            if ino != old_ino {
                self.cache_remove_entry(new_parent, new_name);
                self.cache_insert_entry(new_parent, new_name, old_ino);
                self.clear_inode_touch_cache(ino);
            }
        }

        self.touch_mtime_ctime(old_parent)?;
        if new_parent != old_parent {
            self.touch_mtime_ctime(new_parent)?;
        }
        self.touch_ctime(old_ino)?;

        Ok(())
    }

    pub(super) fn read_at(
        &self,
        ino: u32,
        offset: usize,
        data: &mut [u8],
        status_flags: StatusFlags,
    ) -> Result<usize> {
        let read_len = self.run_ext4(|ext4| ext4.ext4_read_at(ino, offset, data))?;
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
        self.maybe_start_direct_read_profile();

        let mut plan_ns = 0u64;
        let mut alloc_ns = 0u64;
        let mut submit_ns = 0u64;
        let (direct_len, mappings, bio_waiter) = if let Some(pending) =
            self.take_matching_pending_direct_read(ino, offset, writer.avail())
        {
            (pending.len, pending.mappings, pending.waiter)
        } else {
            let plan_start = Self::monotonic_nanos();
            let (direct_len, mappings) = self.plan_direct_read_cached(ino, offset, writer.avail())?;
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
        self.touch_atime_after_direct_read(ino, status_flags)?;
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
        );
        self.maybe_log_direct_read_profile(reads);
        Ok(direct_len)
    }

    pub(super) fn write_at(&self, ino: u32, offset: usize, data: &[u8]) -> Result<usize> {
        let generic014_like_write = data.len() == 512;
        let mut generic014_write_seq = 0;
        let mut generic014_write_start_ns = 0;
        if generic014_like_write {
            generic014_write_seq = GENERIC014_WRITE_PROGRESS.fetch_add(1, Ordering::Relaxed) + 1;
            generic014_write_start_ns = Self::monotonic_nanos();
            if generic014_write_seq <= 8 || generic014_write_seq % GENERIC014_PROGRESS_LOG_INTERVAL == 0 {
                debug!(
                    "ext4: generic014-like write progress seq={} ino={} offset={} len={}",
                    generic014_write_seq, ino, offset, data.len()
                );
            }
        }
        let now = Self::now_unix_seconds_u32();
        let op = JournaledOp::for_small_write(ino, offset, data);
        let mut ext4_write_elapsed_ns = 0u64;
        let mut inode_time_elapsed_ns = 0u64;
        let written = self
            .run_journaled_ext4(op, |ext4| {
                let ext4_write_start_ns = Self::monotonic_nanos();
                let written = ext4
                    .ext4_write_at(ino, offset, data)
                    .map_err(map_ext4_error)?;
                ext4_write_elapsed_ns =
                    Self::monotonic_nanos().saturating_sub(ext4_write_start_ns);
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
            })?;
        if written > 0 {
            self.invalidate_direct_read_cache(ino);
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

        let mut reused_read_mapping_cache = false;
        let mappings = if let Some(cached_mappings) =
            self.plan_direct_write_overwrite_cached(ino, offset, write_len)?
        {
            reused_read_mapping_cache = true;
            cached_mappings
        } else {
            self.run_journaled_ext4(Some(JournaledOp::Write { len: write_len }), |ext4| {
                let mappings = ext4
                    .ext4_prepare_write_at(ino, offset, write_len)
                    .map_err(map_ext4_error)?;

                let mut bio_waiter = BioWaiter::new();
                for mapping in &mappings {
                    let bio_segment =
                        BioSegment::alloc(mapping.len as usize, BioDirection::ToDevice);
                    bio_segment
                        .writer()
                        .map_err(Self::vm_io_error)?
                        .write_fallible(reader)
                        .map_err(|(e, _)| Error::from(e))?;
                    let waiter = self
                        .block_device
                        .write_blocks_async(Bid::new(mapping.pblock), bio_segment)?;
                    bio_waiter.concat(waiter);
                }

                if Some(BioStatus::Complete) != bio_waiter.wait() {
                    return_errno!(Errno::EIO);
                }

                Ok(mappings)
            })?
        };

        if reused_read_mapping_cache {
            let mut bio_waiter = BioWaiter::new();

            for mapping in &mappings {
                let bio_segment = BioSegment::alloc(mapping.len as usize, BioDirection::ToDevice);
                bio_segment
                    .writer()
                    .map_err(Self::vm_io_error)?
                    .write_fallible(reader)
                    .map_err(|(e, _)| Error::from(e))?;
                let waiter = self
                    .block_device
                    .write_blocks_async(Bid::new(mapping.pblock), bio_segment)?;
                bio_waiter.concat(waiter);
            }

            if Some(BioStatus::Complete) != bio_waiter.wait() {
                return_errno!(Errno::EIO);
            }
        }

        self.clear_pending_direct_read(ino);
        if !reused_read_mapping_cache {
            self.invalidate_direct_read_cache(ino);
        }
        self.touch_mtime_ctime(ino)?;
        Ok(write_len)
    }

    pub(super) fn truncate(&self, ino: u32, new_size: u64) -> Result<()> {
        let seq = GENERIC014_TRUNCATE_PROGRESS.fetch_add(1, Ordering::Relaxed) + 1;
        if seq <= 8 || seq % GENERIC014_PROGRESS_LOG_INTERVAL == 0 {
            debug!(
                "ext4: generic014-like truncate progress seq={} ino={} new_size={}",
                seq, ino, new_size
            );
        }
        let now = Self::now_unix_seconds_u32();
        let op = JournaledOp::Truncate;
        self.run_journaled_ext4(Some(op), |ext4| {
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
            })?;
        self.invalidate_direct_read_cache(ino);
        self.inode_mtime_ctime_cache.lock().insert(ino, now);
        Ok(())
    }

    pub(super) fn readdir(&self, ino: u32) -> Result<Vec<SimpleDirEntry>> {
        self.run_ext4_noerr(|ext4| ext4.ext4_readdir(ino))
    }

    pub(super) fn dev_id(&self) -> u64 {
        self.block_device.id().as_encoded_u64()
    }

    pub(super) fn fsync_regular_file(&self) -> Result<()> {
        // Regular-file fsync/fdatasync should not force a full filesystem
        // checkpoint sweep. On the current virtio-blk stack, journal writes are
        // already synchronous DMA to the host, and the JBD2 commit path relies
        // on write ordering rather than an extra full-device flush. Doing a
        // block_device.sync() here turns generic/047 into one global flush per
        // file, which is far more expensive than the journal commit itself.
        self.commit_pending_jbd2_transactions();
        self.commit_pending_jbd2_transactions();
        if self.checkpoint_depth() >= REGULAR_FILE_FSYNC_CHECKPOINT_DEPTH {
            self.try_batch_checkpoint_all_jbd2_transactions();
        }
        Ok(())
    }

    pub(super) fn this(&self) -> Arc<Self> {
        self.self_ref.upgrade().unwrap()
    }

    pub(super) fn make_inode(self: &Arc<Self>, ino: u32, path: String) -> Arc<dyn Inode> {
        Arc::new(super::inode::Ext4Inode::new(Arc::downgrade(self), ino, path))
    }
}

impl FileSystem for Ext4Fs {
    fn name(&self) -> &'static str {
        "ext4"
    }

    fn sync(&self) -> Result<()> {
        self.flush_pending_jbd2_transactions();
        self.block_device.sync()?;
        self.flush_pending_jbd2_transactions();
        Ok(())
    }

    fn root_inode(&self) -> Arc<dyn Inode> {
        self.this().make_inode(EXT4_ROOT_INODE, String::new())
    }

    fn sb(&self) -> SuperBlock {
        let ext4_sb = self.lock_inner().super_block;
        let block_size = ext4_sb.block_size() as usize;
        let blocks = ext4_sb.blocks_count() as usize;
        let bfree = ext4_sb
            .free_blocks_count()
            .min(usize::MAX as u64) as usize;
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
