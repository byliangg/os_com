// SPDX-License-Identifier: MPL-2.0
//! Phase 8 move-only split: in-memory caches relocated verbatim from `fs.rs` — the directory
//! entry cache, the O_DIRECT read planning caches, the extent-map cache, and the per-inode
//! `WrittenCoverage` overwrite fast-path coverage (with its bound consts). No behavior change.

use aster_block::bio::BioWaiter;

use super::core::file::SimpleBlockRange;
use crate::prelude::*;

// P2 (Phase 6): bounds for the per-inode written-extent coverage cache.
// A populated entry holds the file's merged written ranges (~16 bytes each);
// files whose extent tree exceeds the range cap are marked TooFragmented and
// fall back to the per-write mapping walk.
const WRITTEN_COVERAGE_MAX_RANGES: usize = 4096;
pub(super) const WRITTEN_COVERAGE_MAX_INODES: usize = 64;

#[derive(Debug, Default)]
pub(super) struct DirEntryCache {
    pub(super) loaded: bool,
    /// Maps entry name → (child_ino, dir_byte_offset).
    /// `dir_byte_offset == u64::MAX` means the offset is unknown (fallback path).
    pub(super) entries: BTreeMap<String, DirEntryCacheEntry>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct DirEntryCacheEntry {
    pub(super) ino: u32,
    pub(super) offset: u64,
    pub(super) de_type: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DirLookupCacheResult {
    /// (child_ino, dir_byte_offset, de_type); offset is u64::MAX when unknown.
    Hit(u32, u64, u8),
    Miss,
    Unknown,
}

#[derive(Debug)]
pub(super) struct PendingDirectRead {
    pub(super) offset: usize,
    pub(super) len: usize,
    pub(super) mappings: Vec<SimpleBlockRange>,
    pub(super) waiter: BioWaiter,
}

#[derive(Debug)]
pub(super) struct PreparedDirectRead {
    pub(super) offset: usize,
    pub(super) len: usize,
    pub(super) mappings: Vec<SimpleBlockRange>,
}

#[derive(Debug)]
pub(super) struct DirectReadCache {
    pub(super) file_offset: usize,
    pub(super) len: usize,
    pub(super) plan_window: usize,
    pub(super) last_atime_sec: u32,
    pub(super) last_read_end: usize,
    pub(super) pending: Option<PendingDirectRead>,
    pub(super) mappings: Vec<SimpleBlockRange>,
}

/// Phase 5: a cached logical->physical extent mapping for one inode, covering
/// the file byte range `[file_offset, file_offset + len)`. Metadata only — no
/// file data, no speculative readahead. Lets sequential O_DIRECT reads reuse a
/// single `find_extent` walk instead of re-resolving the mapping per read.
pub(super) struct ExtentMapCacheEntry {
    pub(super) file_offset: usize,
    pub(super) len: usize,
    pub(super) mappings: Vec<SimpleBlockRange>,
}

/// P2 (Phase 6): per-inode in-memory coverage of *written* extents, so the
/// buffered overwrite fast path can answer "is this range fully written?"
/// with one BTreeMap lookup instead of an extent-tree walk per write().
///
/// Correctness invariant: **coverage ⊆ truth**. Entries are created only by
/// `coverage_populate` (an authoritative whole-file mapping walk under the
/// inode correctness lock) and extended only with post-success prepare
/// mappings (ranges the journaled prepare just made written). Paths that can
/// *remove* written mappings — truncate (`JournaledOp::Truncate` chokepoint),
/// unlink / rmdir / rename-overwrite (`clear_inode_touch_cache`) — drop the
/// whole entry. Untracked additions (e.g. mmap-hole allocation during
/// writeback) only make coverage pessimistic, never wrong: a miss falls back
/// to the real mapping walk.
pub(super) enum WrittenCoverage {
    /// Merged, maximal, non-adjacent written ranges: start lblock -> len.
    /// Because ranges are maximal, a query window is fully covered iff the
    /// single predecessor range covers it.
    Ranges(BTreeMap<u32, u32>),
    /// The file's written extent set exceeded `WRITTEN_COVERAGE_MAX_RANGES`;
    /// skip caching until the next invalidation.
    TooFragmented,
}

impl WrittenCoverage {
    pub(super) fn covers(&self, start: u32, count: u32) -> bool {
        let WrittenCoverage::Ranges(ranges) = self else {
            return false;
        };
        let Some(end) = start.checked_add(count) else {
            return false;
        };
        let Some((&range_start, &range_len)) = ranges.range(..=start).next_back() else {
            return false;
        };
        range_start.saturating_add(range_len) >= end
    }

    /// Merge-inserts a written range, keeping ranges maximal and
    /// non-adjacent.
    pub(super) fn insert(&mut self, start: u32, len: u32) {
        let WrittenCoverage::Ranges(ranges) = self else {
            return;
        };
        if len == 0 {
            return;
        }
        let Some(end) = start.checked_add(len) else {
            return;
        };
        let mut merged_start = start;
        let mut merged_end = end;
        // Absorb the predecessor if it overlaps or touches the new range.
        if let Some((&prev_start, &prev_len)) = ranges.range(..=start).next_back() {
            let prev_end = prev_start.saturating_add(prev_len);
            if prev_end >= merged_start {
                merged_start = prev_start;
                merged_end = merged_end.max(prev_end);
            }
        }
        // Absorb successors that overlap or touch.
        let mut absorbed = Vec::new();
        for (&next_start, &next_len) in ranges.range(merged_start..) {
            if next_start > merged_end {
                break;
            }
            absorbed.push(next_start);
            merged_end = merged_end.max(next_start.saturating_add(next_len));
        }
        for key in absorbed {
            ranges.remove(&key);
        }
        ranges.insert(merged_start, merged_end - merged_start);
        if ranges.len() > WRITTEN_COVERAGE_MAX_RANGES {
            *self = WrittenCoverage::TooFragmented;
        }
    }
}
