// SPDX-License-Identifier: MPL-2.0
//! Phase 8 move-only split: the O_DIRECT read/write plan-submit-copy path relocated verbatim from
//! `fs.rs` — direct-read planning/caching, speculative readahead, direct-write mapping submission,
//! and the `read_at`/`read_direct_at`/`write_direct_at` entry points. No behavior change.

use aster_block::{
    bio::{BioDirection, BioSegment, BioStatus, BioWaiter},
    id::Bid,
    request_queue::bio_request_merge_count,
};
use ostd::mm::io_util::HasVmReaderWriter;

use super::caches::{DirectReadCache, ExtentMapCacheEntry, PendingDirectRead, PreparedDirectRead};
use super::core::file::SimpleBlockRange;
use super::fs::Ext4Fs;
use super::profile::DirectWriteBioCallProfile;
use super::run::JournaledOp;
use super::types::EXT4_BLOCK_SIZE;
use crate::fs::utils::StatusFlags;
use crate::prelude::*;

impl Ext4Fs {

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

    pub(super) fn invalidate_direct_read_cache(&self, ino: u32) {
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

    /// Map core `file::SimpleBlockRange`s to the integration (ext4_rs) `SimpleBlockRange` DTO.
    /// Field-for-field copy (lblock / pblock / len) — the core type is a byte-parity reimplementation
    /// of ext4_rs's, so the vector contract (ascending by lblock, `len` in blocks) is preserved.
    pub(super) fn core_to_integration_ranges(
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
}
