// SPDX-License-Identifier: MPL-2.0
//! Phase 8 move-only split: the block-device adapter relocated verbatim from `fs.rs` —
//! `KernelBlockDeviceAdapter` (the lowest device read/write seam) plus its bounded write-through
//! `DeviceBlockCache` / `CachedDeviceBlock` and the cache-sizing consts. No behavior change.

use core::sync::atomic::{AtomicU64, Ordering};

use aster_block::{BlockDevice, SECTOR_SIZE, bio::BioStatus};
use ostd::mm::VmIo;

use super::profile::DeviceBlockCacheStats;
use super::types::EXT4_BLOCK_SIZE;
use crate::prelude::*;

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
pub(super) struct DeviceBlockCache {
    pub(super) blocks: BTreeMap<usize, CachedDeviceBlock>,
    use_counter: u64,
}

pub(super) struct CachedDeviceBlock {
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

// Phase 6 Task 0: widened to `pub(super)` so the sibling `core_adapter` module can wrap the
// real-device home-read/write path (`read_offset_into` / `write_offset`) behind core's
// `BlockReader` / `BlockWriter`. No call site changed; only the type is nameable now.
pub(super) struct KernelBlockDeviceAdapter {
    inner: Arc<dyn BlockDevice>,
    io_failure_epoch: AtomicU64,
    pub(super) block_cache: Mutex<DeviceBlockCache>,
    pub(super) block_cache_stats: DeviceBlockCacheStats,
}

impl core::fmt::Debug for KernelBlockDeviceAdapter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("KernelBlockDeviceAdapter").finish()
    }
}

impl KernelBlockDeviceAdapter {
    pub(super) fn new(inner: Arc<dyn BlockDevice>) -> Self {
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
    pub(super) fn invalidate_block_range(&self, start_byte: usize, len: usize) {
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

    pub(super) fn begin_io_operation(&self) -> u64 {
        self.io_failure_epoch.load(Ordering::Acquire)
    }

    pub(super) fn io_failed_since(&self, epoch: u64) -> bool {
        self.io_failure_epoch.load(Ordering::Acquire) != epoch
    }

    fn device_size_bytes(&self) -> usize {
        self.inner.metadata().nr_sectors.saturating_mul(SECTOR_SIZE)
    }
}

// Phase 6 Task 5b: the device-read/write seam (formerly `impl ext4_rs::BlockDevice`) is now an
// inherent impl — `ext4_rs` is deleted and the only callers are in-tree (`JournalIoBridge`,
// `core_adapter`), which call these methods by name. Byte-for-byte the same bodies.
impl KernelBlockDeviceAdapter {
    fn read_offset(&self, offset: usize) -> Vec<u8> {
        let mut data = vec![0u8; EXT4_BLOCK_SIZE];
        self.read_offset_into(offset, data.as_mut_slice());
        data
    }

    pub(super) fn read_offset_into(&self, offset: usize, out: &mut [u8]) {
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

    pub(super) fn write_offset(&self, offset: usize, data: &[u8]) {
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

    pub(super) fn sync(&self) -> Result<()> {
        match self.inner.sync() {
            Ok(BioStatus::Complete) => Ok(()),
            Ok(status) => {
                self.mark_io_failure();
                error!("ext4 block sync completed with status {:?}", status);
                Err(Error::with_message(
                    Errno::EIO,
                    "block device sync did not complete",
                ))
            }
            Err(err) => {
                self.mark_io_failure();
                error!("ext4 block sync failed: {:?}", err);
                Err(Error::with_message(
                    Errno::EIO,
                    "block device sync failed",
                ))
            }
        }
    }
}
