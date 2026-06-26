// SPDX-License-Identifier: MPL-2.0
//! Phase 8 move-only split: the buffered (page-cache) read/write path + written-coverage
//! overwrite fast path relocated verbatim from `fs.rs`, together with the `truncate`/`fallocate`
//! file-size ops (which own the page-cache + coverage invalidation). No behavior change.

use core::sync::atomic::Ordering;

use super::caches::{WRITTEN_COVERAGE_MAX_INODES, WrittenCoverage};
use super::core::file::SimpleBlockRange;
use super::fs::Ext4Fs;
use super::profile::{
    GENERIC014_PROGRESS_LOG_INTERVAL, GENERIC014_SLOW_OP_LOG_THRESHOLD_NS,
    GENERIC014_TRUNCATE_PROGRESS, GENERIC014_WRITE_PROGRESS,
};
use super::run::JournaledOp;
use super::types::{EXT4_BLOCK_SIZE, mode};
use crate::fs::utils::{FallocMode, StatusFlags};
use crate::prelude::*;

const JOURNALED_SMALL_WRITE_MAX_BYTES: usize = 192;

impl Ext4Fs {

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
    pub(super) fn coverage_insert_ranges(&self, ino: u32, mappings: &[SimpleBlockRange]) {
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
}
