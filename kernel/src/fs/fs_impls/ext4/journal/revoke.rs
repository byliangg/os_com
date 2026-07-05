// SPDX-License-Identifier: MPL-2.0

//! Journal revoke (jbd2 `fs/jbd2/revoke.c`).
//!
//! Revoke is the mechanism that stops old log records for freed metadata from
//! being applied on top of newer content in the same, reused blocks. Quoting
//! the spec (revoke.c's header comment, Linux 6.6): *"Revoke is the mechanism
//! used to prevent old log records for deleted metadata from being replayed
//! on top of newer data using the same blocks."* Without it, a metadata block
//! that was journaled, then freed, then reused would be clobbered by whichever
//! consumer re-applies the old log image — for jbd2 that is only mount-time
//! recovery, but our checkpoint is a replay implementation too
//! ([`apply_log_transaction`](super::checkpoint::apply_log_transaction)
//! re-reads the LOG and writes final locations), so the clobber needs no
//! crash: it happens at runtime, on the first checkpoint pass that reaches
//! the old transaction (audit S1).
//!
//! # The two tables (revoke.c's locking scheme, collapsed)
//!
//! jbd2 keeps two hash tables and swaps them at commit
//! (`journal_switch_revoke_table`). Here the running table lives **on the
//! [`Transaction`](super::transaction::Transaction)** itself (its `revoked`
//! set), so `running.take()` at commit *is* the table switch: the committing
//! transaction's revokes ride with it into the commit pipeline, needing no
//! extra lock (single committer). The journal-level [`RevokeTable`] below
//! holds only **committed** revokes — the memory that suppresses checkpoint
//! replay until the covering transactions retire.
//!
//! # The three interaction cases (revoke.c header, verbatim semantics)
//!
//! - **Revoked and then journaled**: the new image must win — the capture
//!   funnels cancel the running transaction's revoke record
//!   (`jbd2_journal_cancel_revoke`; see
//!   [`Transaction::capture_write`](super::transaction::Transaction::capture_write)).
//! - **Journaled and then revoked**: the revoke must win. jbd2 chooses to
//!   write the revoke later in the log; we take the header's other stated
//!   option — *"either to cancel the journal entry or to write the revoke
//!   later"* — and cancel the capture outright
//!   ([`Transaction::forget_block`](super::transaction::Transaction::forget_block)),
//!   which our per-transaction capture map makes exact.
//! - **Revoked and then written as data**: data writes never pass the capture
//!   funnels, so the revoke is *not* cancelled — old log records still cannot
//!   overwrite the new data. This is the reuse case the suppression exists for.
//!
//! # Publication and retention
//!
//! A transaction's revokes are published into the journal's [`RevokeTable`]
//! only once its commit block is durable (commit step 6), **not** at
//! `running.take()`: `commit_or_drain_tail`'s inline drain checkpoint runs
//! mid-commit, and suppressing an older transaction's after-image on the
//! authority of a not-yet-durable revoke would — after a crash that erases
//! this commit — leave the device missing committed (fsync-acknowledged)
//! metadata whose log blocks the drain just retired. jbd2 agrees in spirit:
//! a freed buffer stays on the older transaction's checkpoint list until the
//! freeing transaction commits.
//!
//! An **unpublished** revoke (the running transaction's, a force-locked
//! (draining) transaction's in the locking seat, or the committing slot's on
//! the drain path — all three read out of the journal state in the pass's one
//! snapshot window) is unsound to act on in *either* direction,
//! which is why the checkpoint pass collects the unpublished sets separately
//! and **defers** in front of them
//! ([`checkpoint`](super::checkpoint::checkpoint)'s defer-prefix rule):
//!
//! - It must not suppress-and-retire an older image: a crash may still erase
//!   the revoking transaction, and the suppressed (fsync-acknowledged) image
//!   would then be missing from both the device and the retired log.
//! - The older image must not be *applied* either: the forget's free already
//!   took effect in memory — the bitmap freed the block, and a new owner's
//!   bytes may already sit at the final location — so applying the old image
//!   is the S1 runtime clobber, no crash required.
//!
//! Deferral is the only remaining move: the pass stops (applies nothing,
//! retires nothing) at the first transaction touching such a block, keeping
//! its prefix progress. The revoke publishes with its commit, and the next
//! pass proceeds under ordinary suppression-with-retention. The *reuse* half
//! of the hazard is closed independently by freed-block pinning
//! ([`pin_freed_run`](super::pin_freed_run)): a forgotten block cannot
//! return to the allocator before its freeing transaction commits, so while
//! a revoke is unpublished no new owner's bytes can be standing at the final
//! location.
//!
//! A record `(B, tid_r)` matters only while some transaction with tid ≤
//! `tid_r` can still be applied from the log. Checkpoint therefore retires
//! records at the same tid boundary that retires the un-checkpointed images
//! ([`RevokeTable::retire_through`]): once the pass has applied (or
//! suppressed) everything up to its `committed_tid` snapshot and advanced the
//! tail past it, no runtime consumer can ever apply those transactions again.
//! Mount-time recovery never reads this table — it rebuilds its own,
//! recovery-local instance from the on-disk revoke blocks (PASS_REVOKE,
//! P7b-4) and drops it with the recovered — hence emptied — log (see
//! [`recovery`](super::recovery)'s module docs for why the live table
//! correctly starts empty).
//!
//! # The forget-before-free protocol (effects at consumption)
//!
//! [`forget`] is the sole minting point of [`BlockFreeAuth`] for
//! revoke-covered blocks, and [`Ext4::free_blocks`](super::super::fs::Ext4)
//! consumes the credential — freeing journaled metadata without the revoke
//! decision does not compile (rust_rules ⑤). Freeing with **no** revoke duty
//! takes the other constructor, whose name states the claim it makes
//! ([`BlockFreeAuth::without_revoke_duty`]).
//!
//! Minting is **pure**: every journal effect of a forget — capture cancel,
//! revoke record, retained-image eviction ([`RevokeDuty::discharge`]) — runs
//! when `Ext4::free_blocks` consumes the credential, immediately before (and
//! in the same lock scope as) the bitmap clear it authorizes,
//! record-then-free, mirroring Linux's order within `ext4_free_blocks`
//! (`ext4_forget` before the bitmap clear, fs/ext4/mballoc.c:6676/6719).
//! Effects at mint would let an operation that errors *between* mint and
//! free (a multi-extent truncate failing mid-loop, a failed tree
//! reserialize) silently roll back live metadata: a revoke standing for a
//! still-referenced block suppresses that block's legitimate journaled
//! updates at checkpoint/replay, and the cancelled capture's update is lost.
//! With consumption-time effects, an authorization dropped un-consumed is a
//! true no-op.

use super::{super::prelude::*, Handle, JournalState, Tid};

/// The committed-revoke memory: `block → tid of the newest committed
/// transaction that revoked it` (jbd2's revoke hash table on the recovery
/// side, `jbd2_revoke_record_s { blocknr, sequence }`).
///
/// Max-wins on insert — *"if there are multiple revoke records in the log for
/// a single block, only the last one counts"* (revoke.c) — and consulted by
/// the checkpoint/replay applier through [`suppresses`](Self::suppresses).
/// Held in [`JournalState`](super::JournalState) under the journal state
/// lock; checkpoint snapshots it by clone so its device I/O runs without the
/// lock. PASS_REVOKE (P7b-4) builds a second, recovery-local instance of
/// this same type from the on-disk revoke blocks.
//
// Visible at the `ext4` level only because it is a field of the
// equally-visible `JournalState`; every method stays `pub(super)`, so the
// table is usable only inside the journal module.
#[derive(Clone)]
pub(in crate::fs::fs_impls::ext4) struct RevokeTable {
    records: BTreeMap<Ext4Bid, Tid>,
}

impl RevokeTable {
    /// An empty table — the journal's initial state, and PASS_REVOKE's
    /// starting point before it collects the log's revoke records.
    pub(super) const fn new() -> Self {
        Self {
            records: BTreeMap::new(),
        }
    }

    /// Records that `tid` revoked `bid`, keeping the newest tid if a record
    /// already exists (max-wins; commits publish in tid order, so this is
    /// defensive there — but it is also exactly PASS_REVOKE's rule, jbd2
    /// `jbd2_journal_set_revoke`, and that pass records in log order where
    /// re-revokes do occur).
    pub(super) fn record(&mut self, bid: Ext4Bid, tid: Tid) {
        if self
            .records
            .get(&bid)
            .is_some_and(|existing| existing.geq(tid))
        {
            return;
        }
        self.records.insert(bid, tid);
    }

    /// Returns whether applying transaction `txn_tid`'s after-image of `bid`
    /// must be suppressed: a revoke record `(bid, tid_r)` with `txn_tid ≤
    /// tid_r` exists — the block was freed by a transaction at or after
    /// `txn_tid`, so this (older) image would clobber the block's post-free
    /// reuse. An image in a transaction *newer* than `tid_r` still applies:
    /// *"if there is a log entry for a block beyond the last revoke, then
    /// that log entry still gets replayed"* (revoke.c; jbd2
    /// `jbd2_journal_test_revoke`).
    pub(super) fn suppresses(&self, bid: Ext4Bid, txn_tid: Tid) -> bool {
        self.records.get(&bid).is_some_and(|r| r.geq(txn_tid))
    }

    /// Drops every record whose tid is at or before `committed` — the
    /// checkpoint retirement boundary (see the module docs on retention):
    /// after a pass has applied (or suppressed) all transactions up to
    /// `committed` and advanced the tail past them, a record with `tid_r ≤
    /// committed` can never fire again. Runs in the same critical section
    /// that evicts the checkpointed [`UncheckpointedImage`](super::transaction::UncheckpointedImage)s.
    pub(super) fn retire_through(&mut self, committed: Tid) {
        self.records.retain(|_, tid_r| !committed.geq(*tid_r));
    }

    /// The number of live records. Test inspection.
    #[cfg(ktest)]
    pub(super) fn len(&self) -> usize {
        self.records.len()
    }
}

/// Authorization to free one physical block run — the free site's recorded
/// decision on revoke duty.
///
/// [`Ext4::free_blocks`](super::super::fs::Ext4) consumes this instead of a
/// bare `(start, count)` pair, so every free site must state which of the two
/// legs it stands on (rust_rules ⑤ — free-without-forget is uncompilable):
///
/// - [`forget`] mints it *with* a [`RevokeDuty`], for blocks whose free
///   needs a revoke record: journaled metadata (extent-tree nodes, directory
///   blocks) and slow-symlink targets (Linux revokes those unconditionally,
///   fs/ext4/extents.c:2415-2417).
/// - [`without_revoke_duty`](Self::without_revoke_duty) mints it duty-free,
///   on the caller's claim that nothing about the blocks' history remains
///   for this free to revoke.
///
/// Minting is **pure** — the credential only carries the run and the intent;
/// the forget effects run at consumption (see the module docs and
/// [`RevokeDuty::discharge`]), so a credential dropped un-consumed (an
/// operation erroring between mint and free) changes no journal state: no
/// revoke stands for a still-referenced block, no capture was cancelled, no
/// retained image was evicted.
///
/// The run travels *inside* the credential so a forget on one range cannot
/// authorize a free of another.
#[must_use = "a minted free authorization must be consumed by Ext4::free_blocks"]
pub(in crate::fs::fs_impls::ext4) struct BlockFreeAuth {
    start: Ext4Bid,
    count: u32,
    duty: Option<RevokeDuty>,
}

impl BlockFreeAuth {
    /// Authorizes freeing `count` blocks at `start` with **no revoke duty**,
    /// on the caller's claim that *any prior journaled life of these blocks
    /// was ended by that life's own forget* — nothing about their history
    /// remains for THIS free to revoke. That covers ordered-mode file data
    /// (never journaled at all; Linux frees it with `flags == 0`,
    /// fs/ext4/extents.c:2413-2420) and rollback of freshly allocated,
    /// never-yet-referenced data blocks. Using this for a block whose
    /// journaled life is ending *with* this free re-opens the replay-clobber
    /// hazard [`forget`] closes — the constructor's name is the reviewable
    /// statement of the claim.
    pub(in crate::fs::fs_impls::ext4) fn without_revoke_duty(start: Ext4Bid, count: u32) -> Self {
        Self {
            start,
            count,
            duty: None,
        }
    }

    /// Surrenders the authorized run and its duty to the free itself
    /// (consuming the credential: one mint, one free).
    pub(in crate::fs::fs_impls::ext4) fn into_parts(self) -> (Ext4Bid, u32, Option<RevokeDuty>) {
        (self.start, self.count, self.duty)
    }
}

/// The revoke half of a [`BlockFreeAuth`] minted by [`forget`]: the
/// obligation to run the forget effects with the free. Its constructor is
/// private, so the effects cannot run without a free site's forget decision;
/// [`Ext4::free_blocks`](super::super::fs::Ext4) is the consumption funnel
/// that discharges it.
pub(in crate::fs::fs_impls::ext4) struct RevokeDuty {
    /// Private unit: mintable only by [`forget`].
    _forget_decision: (),
}

impl RevokeDuty {
    /// Runs the forget effects for `count` blocks at `start` — the
    /// running-transaction half of jbd2's `jbd2_journal_forget` +
    /// `jbd2_journal_revoke`. For each block this:
    ///
    /// 1. Cancels any capture of the block in the running transaction — the
    ///    free supersedes the pending write ("journaled and then revoked",
    ///    see the module docs), so the stale image must not commit;
    /// 2. Records the block in the running transaction's revoke set,
    ///    published to the journal's committed-revoke memory when the
    ///    transaction commits;
    /// 3. Evicts the block's committed-but-un-checkpointed image, so the
    ///    stale bytes stop seeding later captures
    ///    ([`get_write_access`](super::get_write_access)) and reads
    ///    ([`read_metadata_block`](super::read_metadata_block)). The image's
    ///    LOG copy is beyond eviction's reach — that side is what the revoke
    ///    record suppresses at checkpoint/replay.
    ///
    /// Called by `Ext4::free_blocks` for each contiguous per-group run of
    /// the authorization it consumes, immediately before that run's bitmap
    /// clear, in the same lock scope and under the same handle
    /// (record-then-free — Linux's `ext4_forget`-before-clear order,
    /// fs/ext4/mballoc.c:6676/6719). `start`/`count` must lie within the run
    /// of the credential this duty traveled in. A failure of the bitmap
    /// clear *itself* after the effects would leave that run's revoke
    /// standing for still-referenced blocks, so `Ext4::free_blocks` aborts
    /// the journal on ANY post-discharge failure (discharge and the bitmap
    /// free are atomic-or-dead): the standing revoke can never act.
    ///
    /// Can only fail *before* any effect runs (the journal lookup and the
    /// running-transaction check precede the per-block loop, which is
    /// infallible) — the caller relies on this to propagate a discharge
    /// `Err` without aborting.
    ///
    /// # Stale credentials
    ///
    /// The caller should hold no live [`WriteAccess`](super::WriteAccess)
    /// for the blocks: the capture cancel makes an outstanding credential
    /// stale, and a later `patch` through it errors `EIO` — "no prior
    /// capture", or a capture-generation mismatch if the block was
    /// re-captured after reallocation — rather than resurrecting the block.
    pub(in crate::fs::fs_impls::ext4) fn discharge(
        &self,
        handle: &Handle,
        start: Ext4Bid,
        count: u32,
    ) -> Result<()> {
        let journal = handle.journal()?;
        let mut state = journal.state_write();
        // Disjoint field borrows: the handle's transaction (capture cancel +
        // revoke record; found tid-keyed in the running or locking seat — a
        // force-locked transaction's own operations may still free blocks
        // while it drains) and the retained-image map (stale-seed eviction).
        let JournalState {
            running,
            locking,
            uncheckpointed,
            ..
        } = &mut *state;
        let txn = super::verify_active(running, locking, handle)?;
        for i in 0..count {
            let bid = start + Ext4Bid::from(i);
            txn.forget_block(bid);
            uncheckpointed.remove(&bid);
        }
        Ok(())
    }
}

/// How an inode's freed **data** blocks relate to the journal — the
/// per-inode-type policy Linux derives in `get_default_free_blocks_flags`
/// (fs/ext4/extents.c:2405-2420), threaded from the inode's type through its
/// `ExtentManager` (next to the csum seed) down to the truncate free sites.
#[derive(Clone, Copy, Debug)]
pub(in crate::fs::fs_impls::ext4) enum DataForgetPolicy {
    /// Freed data blocks are forgotten (revoked): directory blocks are
    /// journaled metadata (`journal_dir_block`), and slow-symlink targets
    /// mirror Linux's conservative `S_ISLNK → METADATA | FORGET` — one
    /// revoke record guards against the block class ever becoming journaled.
    Forget,
    /// Regular-file data: ordered mode never journals it, so its free needs
    /// no revoke record (Linux `flags == 0`).
    PlainData,
}

impl DataForgetPolicy {
    /// Authorizes freeing `count` data blocks at `start` under this policy:
    /// the [`Forget`](Self::Forget) leg mints the revoke duty via [`forget`],
    /// the [`PlainData`](Self::PlainData) leg states the no-duty claim. Pure
    /// either way — the effects belong to the free that consumes the
    /// authorization.
    pub(in crate::fs::fs_impls::ext4) fn authorize(
        self,
        start: Ext4Bid,
        count: u32,
    ) -> BlockFreeAuth {
        match self {
            Self::Forget => forget(start, count),
            Self::PlainData => BlockFreeAuth::without_revoke_duty(start, count),
        }
    }
}

/// Declares that `count` previously journaled (or revoke-covered) blocks at
/// `start` are being freed, minting the [`BlockFreeAuth`] whose consumption
/// runs the forget effects (jbd2 `jbd2_journal_forget` +
/// `jbd2_journal_revoke`; Linux callers reach the pair through `ext4_forget`,
/// fs/ext4/ext4_jbd2.c:266-296).
///
/// The mint is pure — see [`BlockFreeAuth`]: the effects (capture cancel,
/// revoke record, retained-image eviction) run inside
/// [`Ext4::free_blocks`](super::super::fs::Ext4), via
/// [`RevokeDuty::discharge`], immediately before the bitmap clear and under
/// the caller's own handle. On a non-journaled volume (no handle at the
/// free) the duty is inert — there is no log whose replay could resurrect
/// the blocks.
pub(in crate::fs::fs_impls::ext4) fn forget(start: Ext4Bid, count: u32) -> BlockFreeAuth {
    BlockFreeAuth {
        start,
        count,
        duty: Some(RevokeDuty {
            _forget_decision: (),
        }),
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;

    /// Max-wins recording and the suppression predicate's tid window:
    /// `suppresses(bid, T)` fires exactly for `T ≤ tid_r`, and a newer
    /// re-revoke raises the bar while an older one cannot lower it.
    #[ktest]
    fn revoke_table_max_wins_and_window() {
        let mut t = RevokeTable::new();
        assert!(!t.suppresses(7, Tid::new(1)));

        t.record(7, Tid::new(5));
        // Images at or before the revoke are suppressed; newer ones apply.
        assert!(t.suppresses(7, Tid::new(4)));
        assert!(t.suppresses(7, Tid::new(5)));
        assert!(!t.suppresses(7, Tid::new(6)));
        // A different block is untouched.
        assert!(!t.suppresses(8, Tid::new(4)));

        // Max-wins: an older record cannot lower the bar…
        t.record(7, Tid::new(3));
        assert!(t.suppresses(7, Tid::new(5)));
        // …and a newer one raises it.
        t.record(7, Tid::new(9));
        assert!(t.suppresses(7, Tid::new(9)));
        assert!(!t.suppresses(7, Tid::new(10)));
    }

    /// Retirement drops exactly the records the checkpoint boundary covers.
    #[ktest]
    fn revoke_table_retire_through_boundary() {
        let mut t = RevokeTable::new();
        t.record(7, Tid::new(5));
        t.record(8, Tid::new(6));
        t.record(9, Tid::new(8));
        assert_eq!(t.len(), 3);

        t.retire_through(Tid::new(6));
        assert_eq!(t.len(), 1);
        assert!(!t.suppresses(7, Tid::new(5)));
        assert!(!t.suppresses(8, Tid::new(6)));
        assert!(t.suppresses(9, Tid::new(8)));
    }

    /// The tid comparisons stay wrap-correct: a revoke just past the `u32`
    /// wrap still suppresses images from just before it.
    #[ktest]
    fn revoke_table_survives_tid_wraparound() {
        let mut t = RevokeTable::new();
        let before_wrap = Tid::new(u32::MAX);
        let after_wrap = before_wrap.next();

        t.record(7, after_wrap);
        assert!(t.suppresses(7, before_wrap));
        assert!(t.suppresses(7, after_wrap));

        t.retire_through(after_wrap);
        assert!(!t.suppresses(7, before_wrap));
    }
}
