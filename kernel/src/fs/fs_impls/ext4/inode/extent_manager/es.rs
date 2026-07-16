// SPDX-License-Identifier: MPL-2.0

//! [`EsCache`] — the per-inode extent-status cache: a set of proven facts
//! about the extent tree's logical→physical mapping, so the write path can
//! answer "is this range already all written / all mapped?" from one ordered
//! map scan instead of a tree walk (P10-T1; Linux `extents_status.c` is the
//! same-shaped cache).
//!
//! # The per-block-truth invariant (load-bearing)
//!
//! For every cached entry `E = (start, len, pblock, kind)` and every logical
//! block `b` in `[start, start + len)`, the tree currently maps `b` to
//! `pblock + (b - start)` with kind `kind`. An entry is a bundle of per-block
//! facts, NOT a claim that the tree holds an extent with these exact
//! boundaries: the tree's merges and kind-preserving splits move extent
//! boundaries without changing any block's mapping or kind, so they owe this
//! cache nothing — which is what lets facts survive across operations (the
//! structural tree helpers have zero invalidation duty; only the logical
//! mutators do).
//!
//! Consistency is a three-layer contract — never a stale hit, while any miss
//! is legal ([`EsCoverage::Unknown`] just falls back to today's tree walk):
//! 1. **Invalidation**: every logical tree mutator drops its span on entry,
//!    before touching the tree, and the leaf-entry editors demand the
//!    [`EsInvalidated`] credential only invalidation mints — "edited the tree
//!    but skipped the cache decision" does not compile.
//! 2. **Population**: only facts a walk/edit just proved under the ③ lock are
//!    recorded, and only from data already in hand (zero extra descent).
//! 3. **Debug net**: debug builds (every ktest) re-walk the tree behind any
//!    non-`Unknown` verdict
//!    ([`ExtentTree::debug_assert_es_coverage`](super::tree::ExtentTree)).
//!
//! Holes are NOT stored: an unrecorded block is simply `Unknown`. A false
//! "hole" fact is the most dangerous staleness class (a read path would
//! return zeros over live data), and no authorized query consumes hole facts
//! — the three-state view lives at the answer layer ([`EsCoverage`]) instead.
//!
//! # Locking
//!
//! The cache is a field of [`ExtentTree`](super::tree::ExtentTree) — content
//! of the position-③ lock — and every access happens with ③ held (read or
//! write); a fact's truth at probe time is guaranteed by ③ (the tree cannot
//! change under a holder). The inner `SpinLock` is a leaf lock exactly like
//! [`NodeCache`](super::tree::NodeCache)'s: it only arbitrates concurrent
//! ③-readers populating, is taken and dropped inside a single probe (record /
//! invalidate / query), and is never held across device I/O, a journal
//! funnel, or any other lock — in particular the two leaf locks are never
//! held together (population points record only after `write_back` returned
//! or between walk steps, when the node-cache probe is over) — so it adds no
//! edge to the lock order at position ③.

use super::{super::super::prelude::*, node::Extent};

/// Per-inode extent-status cache: the tree's per-block-truth fact set. See
/// the module docs for the invariant, the consistency contract, and the
/// locking discipline.
pub(super) struct EsCache {
    inner: SpinLock<EsInner>,
}

struct EsInner {
    /// Facts keyed by their logical start block (`key == extent.block()`).
    /// Entries are pairwise disjoint — [`EsCache::record`] removes every
    /// overlapping entry before inserting — so a range scan is unambiguous.
    map: BTreeMap<Iblock, Extent>,
}

/// The answer of a range-coverage query: the STRONGEST statement about
/// `[start, end)` the cached facts support. The three-state view lives at
/// this answer layer (`AllWritten ⊃ AllMapped`); a hole or an unrecorded
/// block is `Unknown` — holes are never stored as facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EsCoverage {
    /// Every block in the range is covered by a WRITTEN fact — the overwrite
    /// fast path's condition (Q1/Q2).
    AllWritten,
    /// Every block is mapped (written or unwritten, at least one unwritten)
    /// — enough to prove a hole-fill is a no-op (Q3).
    AllMapped,
    /// At least one block has no cached fact: maybe a hole, maybe just never
    /// recorded. The querier falls back to walking the tree.
    Unknown,
}

/// Invalidation credential (rule 5, typestate-protocol-guard): mintable ONLY
/// by [`EsCache::invalidate_range`] / [`EsCache::clear_all`] — no constructor
/// exists outside this module. Every leaf-entry editor demands
/// `&EsInvalidated`, so any code path that rewrites a leaf entry must have
/// made an es-cache invalidation decision first, at compile time. Non-`Copy`;
/// one invalidation backs all the edits of the same mutator via `&`-borrows.
///
/// Known residual weaknesses (deliberate, backstopped by the debug net and
/// the mutator-sweep ktest): the credential carries no span — a staged split
/// legitimately edits the unchanged part of an extent extending past the
/// invalidated range, so a per-edit span check would misfire — and it is not
/// bound to one inode's cache, since a mutator's scope only ever sees one
/// tree.
pub(super) struct EsInvalidated {
    _priv: (),
}

impl EsCache {
    /// Capacity cap per inode (≈30 KiB worst case with `BTreeMap` overhead —
    /// the `NodeCache`'s order of magnitude). A full cache is cleared whole
    /// and re-seeded rather than LRU-evicted: one line, no policy state, and
    /// correctness-free (a cleared fact is just a miss). Only a pathological
    /// wide scan over a heavily fragmented file ever trips it.
    const CAPACITY: usize = 512;

    /// An empty cache. `const` so
    /// [`ExtentTree::empty`](super::tree::ExtentTree) stays `const`.
    pub(super) const fn new() -> Self {
        Self {
            inner: SpinLock::new(EsInner {
                map: BTreeMap::new(),
            }),
        }
    }

    /// Records one fact a walk/edit just proved under the ③ lock. Every
    /// overlapping older entry is removed first — whole, including one
    /// straddling either boundary — keeping entries pairwise disjoint; a full
    /// cache is cleared before the insert (see [`CAPACITY`](Self::CAPACITY)).
    pub(super) fn record(&self, e: &Extent) {
        debug_assert!(e.len() > 0);
        let start = e.block();
        let end = start as u64 + e.len() as u64;
        let mut inner = self.inner.lock();
        if let Some(k) = inner.straddler_key(start as u64) {
            inner.map.remove(&k);
        }
        while let Some(k) = inner.first_key_in(start, end) {
            inner.map.remove(&k);
        }
        if inner.map.len() >= Self::CAPACITY {
            inner.map.clear();
        }
        inner.map.insert(start, *e);
    }

    /// Removes every fact overlapping `span` — INCLUDING an entry straddling
    /// `span.start`, probed via the predecessor (the classic missed case) —
    /// and mints the invalidation credential. Entries are removed whole,
    /// never trimmed: over-invalidation only costs a miss, and the red line
    /// runs the other way (a fact must never outlive a change to its blocks).
    ///
    /// The span is `u64`, [`walk_range`](super::tree::ExtentTree::walk_range)
    /// style: tail spans run to `u64::MAX` (truncate and the collapse/insert
    /// shifts) and a length-derived end may exceed the 32-bit logical space.
    pub(super) fn invalidate_range(&self, span: Range<u64>) -> EsInvalidated {
        if span.start < span.end {
            let mut inner = self.inner.lock();
            if let Some(k) = inner.straddler_key(span.start) {
                inner.map.remove(&k);
            }
            // Entries STARTING inside the span. A span starting above the
            // 32-bit logical space has no such entries (keys are `Iblock`);
            // it can only be straddled into, handled above.
            if let Ok(first) = Iblock::try_from(span.start) {
                while let Some(k) = inner.first_key_in(first, span.end) {
                    inner.map.remove(&k);
                }
            }
        }
        EsInvalidated { _priv: () }
    }

    /// Drops every fact and mints the credential — the whole-tree-rebuild
    /// invalidation (`reserialize`, beside its `node_cache.clear()`).
    pub(super) fn clear_all(&self) -> EsInvalidated {
        self.inner.lock().map.clear();
        EsInvalidated { _priv: () }
    }

    /// The coverage verdict for `[start, end)`: one ordered scan, starting at
    /// the entry covering `start` (found via the predecessor when it
    /// straddles) and advancing a cursor across abutting facts; any coverage
    /// gap is `Unknown`. O(overlapping entries).
    pub(super) fn range_state(&self, start: Iblock, end: Iblock) -> EsCoverage {
        debug_assert!(start < end);
        if start >= end {
            // Defensive: a vacuous claim over an empty/backwards range must
            // never fuel a fast path.
            return EsCoverage::Unknown;
        }
        let inner = self.inner.lock();
        // Begin at the predecessor when it covers `start`, else at `start`
        // itself (an entry keyed exactly there is picked up by the scan).
        let scan_from = match inner.map.range(..start).next_back() {
            Some((&k, e)) if k as u64 + e.len() as u64 > start as u64 => k,
            _ => start,
        };
        let mut cursor = start as u64;
        let mut all_written = true;
        for (&k, e) in inner.map.range(scan_from..) {
            if k as u64 > cursor {
                // A gap: entries are disjoint and sorted, so nothing later
                // can cover it either.
                return EsCoverage::Unknown;
            }
            if e.is_unwritten() {
                all_written = false;
            }
            cursor = k as u64 + e.len() as u64;
            if cursor >= end as u64 {
                return if all_written {
                    EsCoverage::AllWritten
                } else {
                    EsCoverage::AllMapped
                };
            }
        }
        EsCoverage::Unknown
    }
}

impl EsInner {
    /// Returns the key of the entry that starts BELOW `boundary` but extends
    /// across it, if any — the straddler both `record` and `invalidate_range`
    /// must remove and `range_state` must scan from.
    fn straddler_key(&self, boundary: u64) -> Option<Iblock> {
        let candidate = match Iblock::try_from(boundary) {
            Ok(b) => self.map.range(..b).next_back(),
            // Every key sits below a boundary past the 32-bit space; only
            // the last entry could reach across it.
            Err(_) => self.map.iter().next_back(),
        };
        match candidate {
            Some((&k, e)) if k as u64 + e.len() as u64 > boundary => Some(k),
            _ => None,
        }
    }

    /// Returns the first key in `[from, end)` (`end` in the u64 logical
    /// space), for the remove-overlaps loops.
    fn first_key_in(&self, from: Iblock, end: u64) -> Option<Iblock> {
        match self.map.range(from..).next() {
            Some((&k, _)) if (k as u64) < end => Some(k),
            _ => None,
        }
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::{super::node::ExtentKind, *};

    /// A written fact, the tests' shorthand.
    fn w(block: Iblock, len: u16, start: Ext4Bid) -> Extent {
        Extent::new(block, len, start, ExtentKind::Written)
    }

    /// An entry straddling `span.start` must be removed WHOLE (the classic
    /// missed invalidation case, probed via the predecessor), while disjoint
    /// facts survive; a span ending exactly at an entry's start spares it.
    #[ktest]
    fn straddler_across_span_start_is_invalidated_whole() {
        let es = EsCache::new();
        es.record(&w(0, 8, 100));
        es.record(&w(10, 4, 200));
        assert_eq!(es.range_state(0, 8), EsCoverage::AllWritten);

        // [4,6) begins inside [0,8): the straddler goes whole, not trimmed.
        es.invalidate_range(4..6);
        assert_eq!(es.range_state(0, 4), EsCoverage::Unknown);
        assert_eq!(es.range_state(4, 6), EsCoverage::Unknown);
        assert_eq!(es.range_state(10, 14), EsCoverage::AllWritten);

        // End-exclusive: a span ending at 10 does not touch [10,14).
        es.invalidate_range(8..10);
        assert_eq!(es.range_state(10, 14), EsCoverage::AllWritten);
    }

    /// A record into a full cache clears it whole and seeds the new fact
    /// (the one-line eviction policy: old facts miss, correctness unharmed).
    #[ktest]
    fn record_on_full_cache_clears_then_seeds() {
        let es = EsCache::new();
        for i in 0..EsCache::CAPACITY as u32 {
            es.record(&w(i * 2, 1, 1000 + i as Ext4Bid));
        }
        assert_eq!(es.range_state(0, 1), EsCoverage::AllWritten);

        es.record(&w(5000, 1, 9000));
        assert_eq!(es.range_state(0, 1), EsCoverage::Unknown);
        assert_eq!(es.range_state(5000, 5001), EsCoverage::AllWritten);
    }

    /// A new fact removes every overlapping older entry first (disjointness),
    /// and the answer layer reflects the recorded kind.
    #[ktest]
    fn record_removes_overlapping_facts_first() {
        let es = EsCache::new();
        es.record(&w(0, 8, 100));
        es.record(&Extent::new(4, 8, 200, ExtentKind::Unwritten));
        assert_eq!(es.range_state(4, 12), EsCoverage::AllMapped);
        // The old [0,8) went whole; its head is no longer claimed.
        assert_eq!(es.range_state(0, 4), EsCoverage::Unknown);
    }

    /// `range_state` returns the strongest TRUE tier: `AllWritten` over pure
    /// written coverage, `AllMapped` once an unwritten fact joins, `Unknown`
    /// on any gap — including a straddler-covered start on either side.
    #[ktest]
    fn range_state_returns_strongest_true_tier() {
        let es = EsCache::new();
        es.record(&w(0, 4, 100));
        es.record(&Extent::new(4, 4, 104, ExtentKind::Unwritten));
        es.record(&w(10, 2, 300));
        assert_eq!(es.range_state(0, 4), EsCoverage::AllWritten);
        assert_eq!(es.range_state(0, 8), EsCoverage::AllMapped);
        // A straddler covers the query start; the unwritten member caps the
        // tier at AllMapped.
        assert_eq!(es.range_state(2, 6), EsCoverage::AllMapped);
        assert_eq!(es.range_state(0, 12), EsCoverage::Unknown); // gap [8,10)
        assert_eq!(es.range_state(8, 10), EsCoverage::Unknown);
        assert_eq!(es.range_state(11, 12), EsCoverage::AllWritten);
    }

    /// The shift mutators (truncate / collapse / insert-range) drop the whole
    /// tail with one `[start, u64::MAX)` span; facts below the span survive.
    #[ktest]
    fn tail_span_invalidation_clears_tail_and_spares_head() {
        let es = EsCache::new();
        es.record(&w(0, 4, 100));
        es.record(&w(100, 4, 200));
        es.record(&w(200, 4, 300));
        es.invalidate_range(100..u64::MAX);
        assert_eq!(es.range_state(0, 4), EsCoverage::AllWritten);
        assert_eq!(es.range_state(100, 104), EsCoverage::Unknown);
        assert_eq!(es.range_state(200, 204), EsCoverage::Unknown);
    }

    /// `clear_all` (the whole-tree-rebuild invalidation) drops every fact.
    #[ktest]
    fn clear_all_drops_every_fact() {
        let es = EsCache::new();
        es.record(&w(0, 4, 100));
        es.record(&w(100, 4, 200));
        es.clear_all();
        assert_eq!(es.range_state(0, 4), EsCoverage::Unknown);
        assert_eq!(es.range_state(100, 104), EsCoverage::Unknown);
    }
}
