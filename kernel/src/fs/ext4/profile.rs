// SPDX-License-Identifier: MPL-2.0
//! Phase 8 move-only split: runtime profiling/diagnostic counters relocated verbatim from
//! `fs.rs` (the `*ProfileStats` structs + their impls + the GENERIC014 progress statics/consts +
//! `Jbd2DriverDebugStats` / `FsyncProfileStats` / `DeviceBlockCacheStats`). No behavior change.

use core::sync::atomic::{AtomicU64, Ordering};

use super::run::JournaledOp;

use aster_block::bio::{
    dump_read_bio_profile, dump_write_bio_profile, reset_read_bio_profile, reset_write_bio_profile,
};
use ostd::{
    mm::{HasPaddr, PageFlags, vm_space::VmQueriedItem},
    task::disable_preempt,
};

use super::fs::Ext4Fs;
use crate::prelude::*;
use crate::vm::vmar::{VMAR_CAP_ADDR, VMAR_LOWEST_ADDR};

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

// Phase 8 move-only split: the profiling/diagnostic METHODS relocated verbatim from `fs.rs`
// (they operate on the `*ProfileStats` structs already defined above). No behavior change.
impl Ext4Fs {
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
    pub(super) fn dump_perf_summary(&self) {
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

    pub(super) fn maybe_log_direct_read_profile(&self, reads: u64, force: bool) {
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

    pub(super) fn maybe_log_direct_write_profile(&self, writes: u64, force: bool) {
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

    pub(super) fn maybe_start_direct_read_profile(&self) {
        if self
            .direct_read_profile_started
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            reset_read_bio_profile();
        }
    }

    pub(super) fn maybe_start_direct_write_profile(&self) {
        if self
            .direct_write_profile_started
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            reset_write_bio_profile();
        }
    }

    pub(super) fn profile_direct_write_user_buffer(
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

}
