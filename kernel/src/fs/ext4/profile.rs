// SPDX-License-Identifier: MPL-2.0
//! Phase 8 move-only split: runtime profiling/diagnostic counters relocated verbatim from
//! `fs.rs` (the `*ProfileStats` structs + their impls + the GENERIC014 progress statics/consts +
//! `Jbd2DriverDebugStats` / `FsyncProfileStats` / `DeviceBlockCacheStats`). No behavior change.

use core::sync::atomic::{AtomicU64, Ordering};

use super::run::JournaledOp;

/// Phase 6 Task 2: zeroed stand-in for the ext4_rs `JournalRuntimeDebugStats` (debug-only counters
/// the core-backed driver does not track). Only consumed by the diagnostic `dump_perf_summary`
/// `warn!` line, so zeroed values keep the log shape without re-deriving instrumentation.
#[derive(Default)]
pub(super) struct Jbd2DriverDebugStats {
    pub(super) active_handle_samples: u64,
    pub(super) active_handle_sample_sum: u64,
    pub(super) started_handles: u64,
    pub(super) finished_handles: u64,
    pub(super) max_active_handles: u32,
    pub(super) max_running_handles: u32,
    pub(super) max_running_reserved_blocks: u32,
    pub(super) max_running_metadata_blocks: u32,
    pub(super) rotated_transactions: u64,
    pub(super) prepared_commits: u64,
    pub(super) finished_commits: u64,
    pub(super) finished_checkpoints: u64,
    pub(super) overlay_reads: u64,
    pub(super) overlay_hits: u64,
    pub(super) metadata_write_records: u64,
}

pub(super) const GENERIC014_PROGRESS_LOG_INTERVAL: u64 = 16;
pub(super) const GENERIC014_SLOW_OP_LOG_THRESHOLD_NS: u64 = 1_000_000_000;

pub(super) struct DirectReadProfileStats {
    pub(super) read_calls: AtomicU64,
    pub(super) read_bytes: AtomicU64,
    pub(super) total_mappings: AtomicU64,
    pub(super) mapped_bytes: AtomicU64,
    pub(super) zero_fill_bytes: AtomicU64,
    pub(super) max_mappings: AtomicU64,
    pub(super) max_mapped_bytes: AtomicU64,
    pub(super) cache_hits: AtomicU64,
    pub(super) cache_misses: AtomicU64,
    pub(super) plan_ns: AtomicU64,
    pub(super) alloc_ns: AtomicU64,
    pub(super) submit_ns: AtomicU64,
    pub(super) wait_ns: AtomicU64,
    pub(super) copy_ns: AtomicU64,
    // Phase 5 full-path probe: total wall time of read_direct_at (so we can see
    // how much per-read overhead is outside the measured stages), and the atime
    // bookkeeping time specifically.
    pub(super) total_ns: AtomicU64,
    pub(super) atime_ns: AtomicU64,
}

pub(super) struct DirectWriteProfileStats {
    pub(super) write_calls: AtomicU64,
    pub(super) write_bytes: AtomicU64,
    pub(super) total_mappings: AtomicU64,
    pub(super) total_bios: AtomicU64,
    pub(super) total_segments: AtomicU64,
    pub(super) total_blocks: AtomicU64,
    pub(super) merge_hits: AtomicU64,
    pub(super) user_buffer_pages: AtomicU64,
    pub(super) user_buffer_phys_runs: AtomicU64,
    pub(super) user_buffer_profile_failures: AtomicU64,
    pub(super) cache_hits: AtomicU64,
    pub(super) cache_misses: AtomicU64,
    pub(super) errors: AtomicU64,
    pub(super) plan_ns: AtomicU64,
    pub(super) prepare_ns: AtomicU64,
    pub(super) data_bio_ns: AtomicU64,
    pub(super) bio_alloc_ns: AtomicU64,
    pub(super) bio_copy_ns: AtomicU64,
    pub(super) bio_submit_ns: AtomicU64,
    pub(super) bio_wait_ns: AtomicU64,
    pub(super) bio_wait_return_after_complete_ns: AtomicU64,
    pub(super) touch_ns: AtomicU64,
    pub(super) total_ns: AtomicU64,
    pub(super) hit_data_bio_ns: AtomicU64,
    pub(super) hit_bio_copy_ns: AtomicU64,
    pub(super) hit_bio_wait_ns: AtomicU64,
    pub(super) hit_total_ns: AtomicU64,
    pub(super) miss_plan_ns: AtomicU64,
    pub(super) miss_prepare_ns: AtomicU64,
    pub(super) miss_data_bio_ns: AtomicU64,
    pub(super) miss_bio_copy_ns: AtomicU64,
    pub(super) miss_bio_wait_ns: AtomicU64,
    pub(super) miss_touch_ns: AtomicU64,
    pub(super) miss_total_ns: AtomicU64,
    pub(super) max_mappings: AtomicU64,
    pub(super) max_bios_per_call: AtomicU64,
    pub(super) max_segments_per_bio: AtomicU64,
    pub(super) max_blocks_per_bio: AtomicU64,
    pub(super) max_user_buffer_phys_runs: AtomicU64,
    pub(super) max_user_buffer_phys_run_pages: AtomicU64,
    pub(super) max_prepare_ns: AtomicU64,
    pub(super) max_data_bio_ns: AtomicU64,
    pub(super) max_bio_wait_return_after_complete_ns: AtomicU64,
    pub(super) max_touch_ns: AtomicU64,
    pub(super) max_total_ns: AtomicU64,
    pub(super) max_miss_prepare_ns: AtomicU64,
    pub(super) max_miss_data_bio_ns: AtomicU64,
    pub(super) max_miss_total_ns: AtomicU64,
}

/// Read-only Phase 6 probe for the buffered (page-cache) write path
/// (`write_at_page_cache`) — the path SQLite takes under `page_cache=1`. Splits
/// the per-`write()` cost into the overwrite fast path (no block allocation) vs
/// the append/sparse slow path (journaled allocation via `run_journaled_ext4` +
/// `ext4_prepare_write_at`), so Step 0 can attribute SQLite write time to
/// "new-allocation journaled prepare" vs "in-place overwrite", and confirm how
/// many disk blocks the slow path allocates. Gated by `ext4fs.phase2_profile`;
/// off by default so guard regressions see no extra work.
pub(super) struct BufferedWriteProfileStats {
    pub(super) calls: AtomicU64,
    pub(super) fast_calls: AtomicU64,
    pub(super) fast_bytes: AtomicU64,
    pub(super) fast_ns: AtomicU64,
    pub(super) slow_calls: AtomicU64,
    pub(super) slow_bytes: AtomicU64,
    pub(super) slow_blocks: AtomicU64,
    pub(super) slow_prepare_ns: AtomicU64,
    pub(super) slow_ns: AtomicU64,
    pub(super) max_slow_ns: AtomicU64,
    pub(super) max_slow_prepare_ns: AtomicU64,
    // Stage 0 OOM diagnosis: total bytes written back to disk by the page-cache
    // writeback path (`write_page_cache_data_at`). Outstanding dirty data ≈
    // (fast_bytes + slow_bytes) − writeback_bytes; if that grows unbounded toward
    // an OOM the cause is page-cache dirty pages, not journal memory.
    pub(super) writeback_bytes: AtomicU64,
}

#[derive(Default)]
pub(super) struct DirectWriteBioCallProfile {
    pub(super) mappings: u64,
    pub(super) bios: u64,
    pub(super) segments: u64,
    pub(super) blocks: u64,
    pub(super) merge_hits: u64,
    pub(super) user_buffer_pages: u64,
    pub(super) user_buffer_phys_runs: u64,
    pub(super) user_buffer_profile_failures: u64,
    pub(super) max_segments_per_bio: u64,
    pub(super) max_blocks_per_bio: u64,
    pub(super) max_user_buffer_phys_run_pages: u64,
    pub(super) wait_return_after_complete_ns: u64,
}

pub(super) struct Ext4RsRuntimeLockStats {
    pub(super) acquire_count: AtomicU64,
    pub(super) total_wait_ns: AtomicU64,
    pub(super) max_wait_ns: AtomicU64,
    pub(super) total_hold_ns: AtomicU64,
    pub(super) max_hold_ns: AtomicU64,
}

pub(super) struct JournaledOpProfileStats {
    pub(super) op_count: AtomicU64,
    pub(super) mkdir_count: AtomicU64,
    pub(super) rmdir_count: AtomicU64,
    pub(super) write_count: AtomicU64,
    pub(super) start_handle_ns: AtomicU64,
    pub(super) apply_ns: AtomicU64,
    pub(super) finish_handle_ns: AtomicU64,
    pub(super) finish_alloc_ns: AtomicU64,
    pub(super) finish_io_ns: AtomicU64,
    pub(super) total_ns: AtomicU64,
    pub(super) max_apply_ns: AtomicU64,
    pub(super) max_finish_handle_ns: AtomicU64,
    pub(super) max_total_ns: AtomicU64,
}

pub(super) static GENERIC014_WRITE_PROGRESS: AtomicU64 = AtomicU64::new(0);
pub(super) static GENERIC014_TRUNCATE_PROGRESS: AtomicU64 = AtomicU64::new(0);

impl DirectReadProfileStats {
    pub(super) const LOG_INTERVAL_READS: u64 = 8_192;

    pub(super) const fn new() -> Self {
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

    pub(super) fn record_cache_hit(&self) {
        self.cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_cache_miss(&self) {
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_read(
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

    pub(super) fn update_max_mappings(&self, mappings: u64) {
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

    pub(super) fn update_max_mapped_bytes(&self, mapped_bytes: u64) {
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
    pub(super) const fn new() -> Self {
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

    pub(super) fn record_fast(&self, bytes: usize, elapsed_ns: u64) {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.fast_calls.fetch_add(1, Ordering::Relaxed);
        self.fast_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.fast_ns.fetch_add(elapsed_ns, Ordering::Relaxed);
    }

    pub(super) fn record_slow(&self, bytes: usize, blocks: u64, prepare_ns: u64, elapsed_ns: u64) {
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

    pub(super) fn bump_max(field: &AtomicU64, value: u64) {
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
    pub(super) const LOG_INTERVAL_ACQUIRES: u64 = 4_096;

    pub(super) const fn new() -> Self {
        Self {
            acquire_count: AtomicU64::new(0),
            total_wait_ns: AtomicU64::new(0),
            max_wait_ns: AtomicU64::new(0),
            total_hold_ns: AtomicU64::new(0),
            max_hold_ns: AtomicU64::new(0),
        }
    }

    pub(super) fn record_wait(&self, wait_ns: u64) {
        self.acquire_count.fetch_add(1, Ordering::Relaxed);
        self.total_wait_ns.fetch_add(wait_ns, Ordering::Relaxed);
        Self::update_max(&self.max_wait_ns, wait_ns);
    }

    pub(super) fn record_hold(&self, hold_ns: u64) {
        self.total_hold_ns.fetch_add(hold_ns, Ordering::Relaxed);
        Self::update_max(&self.max_hold_ns, hold_ns);
    }

    pub(super) fn update_max(target: &AtomicU64, value: u64) {
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
    pub(super) const LOG_INTERVAL_WRITES: u64 = 4_096;

    pub(super) const fn new() -> Self {
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

    pub(super) fn record_write(
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
    pub(super) const fn new() -> Self {
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

    pub(super) fn record(
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

#[derive(Debug, Default)]
pub(super) struct FsyncProfileStats {
    pub(super) calls: AtomicU64,
    pub(super) writeback_ns: AtomicU64,
    pub(super) commit_ns: AtomicU64,
    pub(super) flush_ns: AtomicU64,
}

#[derive(Debug, Default)]
pub(super) struct DeviceBlockCacheStats {
    pub(super) hits: AtomicU64,
    pub(super) misses: AtomicU64,
    pub(super) unaligned_reads: AtomicU64,
    pub(super) evictions: AtomicU64,
    pub(super) invalidations: AtomicU64,
}
