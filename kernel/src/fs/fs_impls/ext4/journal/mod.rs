// SPDX-License-Identifier: MPL-2.0

//! The JBD2 journal (crash-consistency subsystem).
//!
//! The journal gives ext4 crash consistency via write-ahead logging, jbd2's
//! on-disk format (byte-for-byte compatible with Linux, so `e2fsck` and a stock
//! kernel interoperate with our images). [`Journal`] is the in-memory core,
//! loaded at mount by [`load_geometry`] + [`Journal::new`] from the journal
//! inode (ino 8); [`Ext4::open`](super::fs::Ext4) recovers a dirty log with
//! [`recover`] and starts the background commit thread
//! ([`Journal::start_commit_thread`], jbd2's `kjournald`), which
//! [`Ext4::drop`](super::fs::Ext4) stops.
//!
//! # Sub-modules
//!
//! - [`format`] — the jbd2 on-disk layout (big-endian header / superblock / tag
//!   / commit block) and the validated [`JournalSuperblock`].
//! - [`transaction`] — [`Transaction`] (the op-time metadata after-image
//!   capture) and [`Handle`] (`journal_start`/`journal_stop`).
//! - [`commit`] — the commit pipeline: a transaction's after-images →
//!   descriptor + metadata log blocks → barrier → commit block → barrier.
//! - [`checkpoint`] — copies committed after-images from the log to their final
//!   locations and reclaims log space.
//! - [`recovery`] — mount-time SCAN/REPLAY of a dirty log.
//!
//! This module root additionally hosts the commit thread + `log_wait_commit`
//! (the fsync primitive), [`Tid`] (with its wrapping [`geq`](Tid::geq)
//! comparison), and the metadata-access seam below.
//!
//! # Metadata-access seam
//!
//! Every metadata modification is routed through four access wrappers so that
//! journaling of the filesystem's *own* writes turns on per call site by
//! threading a live [`Handle`] in, with no change to the wrappers:
//!
//! - [`get_write_access`] — about to modify an existing metadata block; under a
//!   handle it seeds the block's after-image from the newest
//!   committed-but-un-checkpointed image when the journal retains one
//!   ([`JournalState::uncheckpointed`]), else from the device — the device lags
//!   a committed transaction until checkpoint, so it must never be the seed
//!   inside that window (see
//!   [`UncheckpointedImage`](transaction::UncheckpointedImage)).
//! - [`get_create_access`] — about to populate a freshly allocated metadata
//!   block (extent index/leaf blocks, new directory blocks, new bitmaps); under
//!   a handle it seeds a zeroed after-image.
//! - [`WriteAccess::patch`] — the metadata block has been modified; live,
//!   its `patch` closure writes the modification into the captured after-image,
//!   so sub-objects sharing a block accumulate onto one buffer.
//! - [`forget`] — a previously journaled metadata block is being freed: mints
//!   the [`BlockFreeAuth`] carrying the revoke duty that
//!   [`Ext4::free_blocks`](super::fs::Ext4) discharges with the bitmap clear
//!   (cancelling the running transaction's capture, recording the revoke,
//!   evicting the retained un-checkpointed image). The mint itself is pure —
//!   an operation erroring between mint and free changes nothing — see
//!   [`revoke`].
//! - [`pin_freed_run`] — every journaled free (either [`BlockFreeAuth`]
//!   flavor) additionally pins its run out of the allocator until the freeing
//!   transaction commits ([`JournalState::pinned_frees`]).
//!
//! **Without a handle (`None`) every wrapper is inert** and persistence stays
//! ext2-style: metadata objects carry a [`Dirty`](super::utils::Dirty) flag
//! written back by `sync` — the Phase 1–3 behaviour, still what a non-journaled
//! volume does. On a journaled volume every metadata operation opens a handle
//! via `Ext4::begin_op` (Int-B) and threads it through these wrappers. Callers
//! must **never assume [`WriteAccess::patch`] makes a block persistent** — it marks
//! the block for writeback (and, under a handle, captures its after-image);
//! "write through immediately" would bake in a flush-timing assumption the
//! ordered-mode journal breaks.
//!
//! # Group commit (P7c-2)
//!
//! The running transaction **batches** operations: `journal_stop` no longer
//! requests a commit — the transaction accumulates until one of three
//! triggers fires (jbd2's exact trigger set):
//!
//! - **Durability**: `fsync`/`O_SYNC`/`sync(2)` reach
//!   [`Journal::log_wait_commit`], which force-requests the target tid's
//!   commit ([`Journal::request_commit_for`], jbd2 `jbd2_log_start_commit`);
//!   a capacity-blocked `journal_start` and the pinned-block `ENOSPC` retry
//!   path escalate the same way.
//! - **Size**: the transaction's captured log footprint reaches
//!   [`Journal::batch_trigger_credits`] — a quarter of [`Journal::max_credits`],
//!   mirroring jbd2's `j_max_transaction_buffers = j_total_len / 4`
//!   (`jbd2_journal_get_max_txn_bufs`): committing at the quarter point
//!   keeps the running + committing + un-checkpointed tail comfortably
//!   inside the ring, so the commit-time fit guard's drain path stays rare.
//! - **Age**: the transaction outlives `COMMIT_INTERVAL_JIFFIES` (5 s, jbd2
//!   `JBD2_DEFAULT_MAX_COMMIT_AGE`); the commit thread arms its sleep with
//!   the running transaction's fixed deadline (jbd2 arms `j_commit_timer`
//!   per transaction). **Crash-loss window**: un-synced work may lose up to
//!   this interval — Linux data=ordered semantics, unchanged for anything
//!   fsync-acknowledged.
//!
//! When a trigger fires while handles are open, the committer parks the
//! transaction in [`JournalState::locking`] (jbd2 `T_LOCKED`) to **drain**:
//! its own handles keep patching it (the funnels are handle-tid-keyed,
//! [`active_txn_mut`]) until they close; NEW handles are barred — and, more
//! than barred, `journal_start` *blocks* until the drained transaction is
//! staged (jbd2 `add_transaction_credits` → `wait_transaction_locked`,
//! fs/jbd2/transaction.c:236-243). The block is load-bearing, not an
//! implementation convenience: Model A patches serialize the
//! globally-current typed metadata state, so a successor's captures running
//! concurrently with the locked transaction's still-open handles would leak
//! uncommitted successor state into the OLDER commit's after-images (crash
//! ⇒ e.g. a committed directory entry naming an inode whose transaction
//! never committed) — the isolation break jbd2 closes with this same
//! barrier. The barrier ends at STAGING, not at commit-I/O completion: the
//! successor then runs while the commit writes, which is the pipeline
//! overlap.
//!
//! Deadlock audit of the two waits this adds (law 1):
//!
//! - `journal_start`'s locked barrier extends the existing capacity wait
//!   (same waiter profile: only ①-inode locks held, no handle, no ③-level
//!   lock — the documented space-wait exception). The drain progresses
//!   without any lock a waiter holds: the locked handles' operations
//!   acquired all their inode locks *before* `journal_start` (lock order ①
//!   → ②) and never take another ①; the ③-level locks they may still need
//!   are held only by other handle-holders (order ② → ③), never by a
//!   thread parked in `journal_start`. Wakers: every `journal_stop` (epoch
//!   bump) and the committer's staging (epoch bump), neither of which
//!   takes a filesystem lock.
//! - The drain wait itself lives ONLY on the committer thread (its `Parked`
//!   poll outcome sends it back to an untimed sleep), which holds no
//!   filesystem lock — a user thread never waits for a drain it could be
//!   blocking.
//! - **`begin_op` reentrancy** is the one way a single task can wait on
//!   itself here: a task holding an open handle that opens a SECOND handle
//!   would park the nested [`journal_start`](transaction::journal_start) on
//!   the locked barrier (or the capacity wait), whose drain/release can only
//!   come from that same task closing its outer handle — a self-deadlock. c2
//!   widened this from the pre-c2 narrower hazard (the Drop-reclaim path
//!   re-taking `inner.write()` on an inode a still-held guard owns, a
//!   re-entrant `RwMutex` deadlock) to ANY open handle on ANY inode. The call
//!   sites are correct by declaration order — the op's handle closes before
//!   the reclaim-triggering `Arc<Inode>` drops, so reclaim's `begin_op` opens
//!   only after the outer handle closed — and [`OpHandle::start`] backstops
//!   that discipline: it registers the task in [`Journal::op_handle_owners`]
//!   at the operation boundary and refuses a nested open with `EDEADLK`
//!   before `journal_start` can park (jbd2's `WARN_ON(current->journal_info)`
//!   in `start_this_handle`, made a hard error since a hung kernel is worse
//!   than a failed op).
//!
//! # Duality invariants (the running/locking/committing pipeline, P7c-1/2)
//!
//! [`JournalState`] holds at most one *running*, at most one *locking*
//! (force-locked, draining its open handles — never coexisting with a
//! running transaction, see the barrier above), and at most one *committing*
//! transaction: a new running transaction starts (lazily, at the next
//! `journal_start`) while the staged one is still being written — the
//! two-transaction pipeline. What keeps the overlap sound, each re-verified
//! under explicit duality (and re-audited for the locking seat):
//!
//! 1. **One committer serializes commit and checkpoint.** All production
//!    checkpoint passes and commits run on the commit thread (the unmount
//!    flush strictly after it stops), so a commit's step-6 effects — revoke
//!    publication, pinned-free release ([`JournalState::release_pinned_frees`])
//!    — can never land in the middle of a checkpoint pass, and at most one
//!    [`CommittingTxn`] can ever exist (guarded anyway: every stager —
//!    [`Journal::advance_pipeline`], the unmount flush, and the ktest
//!    stager — refuses an occupied slot). Locking a due running transaction
//!    is equally the committer's move alone, so the seats advance in one
//!    place.
//! 2. **Committing images are visible to seeders from the staging instant**
//!    (law 4 / the B-1 restated invariant): staging stashes the transaction's
//!    after-images into [`JournalState::uncheckpointed`] in the same
//!    state-lock window that empties its seat, and the whole seed decision
//!    (running capture → locked capture → retained image → device) happens
//!    in one state-lock window ([`get_write_access`]). A LOCKING-seat
//!    transaction's captures are not yet in the map — they are still
//!    receiving its own handles' patches — but no successor exists to seed
//!    from them (the locked barrier), and the funnels consult the seat
//!    directly anyway (newest-wins order above), so the one-window property
//!    is preserved by construction, not by timing. See the `uncheckpointed`
//!    field docs for why the map is the committing-visibility mechanism and
//!    why tid-keyed eviction can never evict a mid-flight commit's images.
//! 3. **The committing transaction is frozen** ([`CommittingTxn`]): staging
//!    still happens only at `nr_updates() == 0` — the locking seat is where
//!    a transaction waits until that is true — and every mutating funnel
//!    (captures, forgets, ordered-data registration) reaches only the
//!    running/locking seats via handle-tid-keyed lookup ([`active_txn_mut`])
//!    — so the committer's lock-free I/O and the state-lock-holding readers
//!    share a read-only object. The one adjacent mutation, a forget's
//!    eviction of the block's retained image, acts on the *map*, not the
//!    transaction.
//! 4. **Unpublished-revoke collection is uniform across all three seats**:
//!    the checkpoint snapshot reads the running transaction's, the locking
//!    seat's, AND the committing slot's unpublished revokes from the state
//!    in one lock window ([`checkpoint`](checkpoint::checkpoint)), so a
//!    forget landed in any not-yet-durable transaction defers the pass
//!    exactly like before the split. Pinned-free release and revoke
//!    publication stay at commit step 6, keyed by the committed tid — a
//!    parked transaction's pins and revokes ride with it through the seats
//!    untouched.
//! 5. **Iron law 1 holds across the overlap**: `journal_start` under inode
//!    locks never waits on the committing transaction — joining/creating
//!    `running` ignores the slot entirely; its two waits (capacity, the
//!    locked barrier) are both space-shaped and released by handle closes
//!    and by the committer thread, which takes no fs lock (see the group-
//!    commit deadlock audit above and `wait_for_transaction_room`'s locking
//!    contract). `log_wait_commit` is called with no fs locks held, as
//!    always; commits stay strictly serial (one slot), so waiting on a tid
//!    still covers every earlier tid.
//!
//! # Lazy-checkpoint invariants (P7c-3)
//!
//! Checkpoint is no longer eager (one pass per commit). The commit thread
//! reclaims the tail LAZILY — under space pressure only
//! ([`run_lazy_checkpoint`](Journal::run_lazy_checkpoint)): a demanded reclaim
//! from a `journal_start` blocked in
//! [`wait_for_log_space`](Journal::wait_for_log_space), or the free segment
//! falling below [`checkpoint_low_water`](Journal::checkpoint_low_water). Space
//! backpressure moves to reservation time: `journal_start` refuses to build a
//! transaction that will not fit the ring's free segment and waits for the
//! committer to checkpoint first (jbd2 `__jbd2_log_wait_for_space` from
//! `start_this_handle`), so the commit-time fit guard's inline drain
//! ([`commit_or_drain_tail`](Journal::commit_or_drain_tail)) — and its loud
//! `ENOSPC` abort backstop — become the exception, not the rule. Checkpoint
//! stays SINGLE-DRIVER (the commit thread; the unmount flush after it stops):
//! the space wait sets a flag and the committer services it, so no second
//! thread ever runs a checkpoint pass — every duality invariant that rests on
//! "one committer serializes commit and checkpoint" (1, 4) survives unchanged.
//! With the tail now MANY transactions deep instead of one (the eager pass used
//! to empty it every commit), each b/c-stack invariant is re-audited at depth:
//!
//! - **(a) Revoke suppression across a deep log.** The committed-revoke memory
//!   ([`JournalState::revoked`], max-wins per block) holds EVERY committed
//!   transaction's revokes until [`checkpoint`](checkpoint::checkpoint) retires
//!   them — not just the last one's. A pass applies oldest-first, so when it
//!   applies transaction `M` the table still holds the revokes of every
//!   un-checkpointed `N > M` (all committed, each published at its own step 6),
//!   and [`RevokeTable::suppresses`](revoke::RevokeTable) suppresses `M`'s image
//!   of a block any `N ≥ M` freed. [`retire_through`](revoke::RevokeTable) drops
//!   only records `≤ applied_through` — transactions this pass fully applied AND
//!   advanced the tail past — so a record a still-unapplied older transaction
//!   needs is never dropped. Depth-correct by the tid boundary, not by "one
//!   deep".
//! - **(b) Freed-block pinning is orthogonal to checkpoint laziness.**
//!   [`release_pinned_frees`](JournalState::release_pinned_frees) runs at commit
//!   step 6, keyed by the COMMITTED tid — never at checkpoint. Lazy checkpoint
//!   delays checkpoint, not commit (group commit still commits on the size/age/
//!   request triggers), so the pinned set does NOT grow with the un-checkpointed
//!   tail; it holds only the not-yet-committed frees, bounded by one
//!   transaction's `max_credits`. The allocator skip cost and the transient-
//!   `ENOSPC` → `request_commit_of_running` retry are unchanged.
//! - **(c) The retained-image map is now many-deep, and stays newest-wins.**
//!   [`stash_uncheckpointed`](transaction::Transaction) inserts each staging
//!   transaction's images tagged with its tid, overwriting any older entry —
//!   and staging is tid-ordered (serial commits), so the map always holds the
//!   NEWEST committed image of each block, however deep the tail. Seeding reads
//!   that newest image; tid-keyed eviction
//!   ([`is_checkpointed_by`](transaction::UncheckpointedImage)) drops only
//!   images `≤ applied_through`, retaining a newer commit's image the device
//!   still lags. The "≤ one transaction deep" note the eager design carried
//!   (A1_ledger `lazy-checkpoint`) is void: correctness rests on the tid tag,
//!   not the depth.
//! - **(d) The defer-prefix pre-scan is bounded over the deep log.**
//!   [`chain_covers_unpublished`](checkpoint) is SKIPPED entirely when no
//!   forget is in flight (`unpublished.is_empty()`, the common case) — the deep
//!   tail costs nothing then. When a forget IS in flight it reads only chain
//!   blocks (descriptors + commit), returns at the first covered block, and is
//!   bounded per transaction by `descriptors + 1` and overall by the anti-cycle
//!   `maxlen` bound; it never reads a data block twice.
//!
//! Note (deviation, see `ext4_rebuild_report.md` §12): the report sketches a
//! `MetaBuffer` handle owning the raw block bytes. We instead reuse ext2's
//! typed-and-dirty-tracked metadata (`Dirty<IdBitmap>`, `Dirty<BlockGroupDesc>`,
//! the inode-table page cache) and identify the affected block by its number;
//! op-journaling captures the block's after-image into a [`Transaction`] buffer.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use ostd::{
    sync::{RwMutexWriteGuard, WaitQueue},
    timer::Jiffies,
};

use self::{
    commit::{CommitAttempt, try_commit_transaction},
    format::{
        BLOCKTYPE_SUPERBLOCK_V2, Be32, COMPAT_CHECKSUM, INCOMPAT_64BIT, INCOMPAT_CSUM_V3,
        INCOMPAT_SUPP, JBD2_CRC32C_CHKSUM, JournalCsumSeed, JournalSuperblock,
        RawJournalSuperblock, TagLayout,
    },
    transaction::{CommitPhase, Transaction},
};
use super::{
    feature::FeatureCompatSet,
    fs::{Ext4, JOURNAL_INO},
    prelude::*,
};
use crate::time::clocks::JIFFIES_TIMER_MANAGER;

mod checkpoint;
mod commit;
mod format;
#[cfg(ktest)]
mod interop_vectors;
mod recovery;
mod revoke;
mod transaction;

/// Replays a dirty journal at mount time (jbd2 `jbd2_journal_recover`).
///
/// Re-exported at the `ext4` level so [`Ext4::open`](super::fs::Ext4) can drive
/// mount-time recovery; the pass machinery lives in [`recovery`]. A no-op when the
/// on-disk journal superblock is already clean (`s_start == 0`).
pub(in crate::fs::fs_impls::ext4) use self::recovery::recover;
/// Re-exported at the `ext4` level: the forget-before-free protocol —
/// [`forget`] purely mints the [`BlockFreeAuth`] that
/// [`Ext4::free_blocks`](super::fs::Ext4) consumes (the forget *effects* run
/// at that consumption, with the bitmap clear), and [`DataForgetPolicy`] is
/// the per-inode-type rule the truncate paths thread down to their data
/// frees. See the [`revoke`] module.
pub(in crate::fs::fs_impls::ext4) use self::revoke::{BlockFreeAuth, DataForgetPolicy, forget};
/// Re-exported at the `ext4` level so allocation/extent paths can thread an
/// `Option<&Handle>` through to the [`get_write_access`]/[`WriteAccess::patch`]
/// funnels. The handle lifecycle (`journal_start`/`journal_stop`) stays inside
/// this module; callers only borrow a handle for capture.
pub(in crate::fs::fs_impls::ext4) use self::transaction::Handle;

/// The current task's identity for the nested-`begin_op` guard: its
/// [`Task`](ostd::task::Task) address, stable for the guard's lifetime because
/// a task holding an operation handle cannot be freed. `None` outside task
/// context (bootstrap/IRQ), where no metadata operation runs — the guard is
/// then inert.
fn current_op_owner() -> Option<usize> {
    let task = ostd::task::Task::current()?;
    let ptr: *const ostd::task::Task = &*task;
    Some(ptr.addr())
}

/// An operation's journal handle, scoped so [`journal_stop`](transaction::journal_stop)
/// runs on **every** exit path (an early `?`, an error, or the normal return),
/// not just the happy one.
///
/// A metadata operation opens one via [`Ext4::begin_op`](super::fs::Ext4) right
/// after taking its inode `inner` lock (lock order: `inner` ① → handle ②), holds
/// it for the operation, and threads [`get`](OpHandle::get) into the metadata
/// funnels. On drop it closes the handle, which — when it is the transaction's
/// last — signals the commit thread (asynchronously; durability is `fsync`'s
/// job). A non-journaled volume yields [`none`](OpHandle::none), whose `get()` is
/// `None`, so every funnel stays inert.
pub(in crate::fs::fs_impls::ext4) struct OpHandle {
    handle: Option<Handle>,
    /// This task's registration in [`Journal::op_handle_owners`], to release on
    /// close. `Some` for a journaled handle opened in task context, `None` for
    /// the no-op handle or outside task context (nothing to release).
    owner: Option<usize>,
}

impl OpHandle {
    /// A handle for a non-journaled volume: `get()` yields `None`.
    pub(in crate::fs::fs_impls::ext4) fn none() -> Self {
        Self {
            handle: None,
            owner: None,
        }
    }

    /// Opens a handle on `journal`'s running transaction, reserving `credits`
    /// metadata blocks (jbd2 `jbd2_journal_start`).
    ///
    /// Rejects a nested operation handle on the same task with `EDEADLK` — the
    /// c2 self-deadlock: a task holding an open handle that opens a second one
    /// would park [`journal_start`](transaction::journal_start) on the locked
    /// barrier (or the capacity wait), whose drain/release waits on that very
    /// task's outer handle to close. The check fires here, at the operation
    /// boundary, before `journal_start` can park; it is the backstop to the
    /// declaration-order discipline the call sites document (the reclaim path
    /// opens its handle only after the outer one closed, so its owner slot is
    /// clear by then). See the module deadlock audit.
    pub(in crate::fs::fs_impls::ext4) fn start(
        journal: &Arc<Journal>,
        credits: usize,
    ) -> Result<Self> {
        let owner = journal.claim_op_handle()?;
        let handle = match transaction::journal_start(journal, credits) {
            Ok(handle) => handle,
            Err(e) => {
                if let Some(id) = owner {
                    journal.release_op_handle(id);
                }
                return Err(e);
            }
        };
        Ok(Self {
            handle: Some(handle),
            owner,
        })
    }

    /// Borrows the handle for threading into the metadata funnels.
    pub(in crate::fs::fs_impls::ext4) fn get(&self) -> Option<&Handle> {
        self.handle.as_ref()
    }

    /// Returns the transaction id this handle joined, or `None` for the no-op
    /// handle of a non-journaled volume. An `fsync` records this before closing the
    /// handle, releases every filesystem lock, and then waits for the tid via
    /// [`Journal::log_wait_commit`] — commits are serial, so waiting on it also
    /// covers every earlier transaction that touched the inode.
    pub(in crate::fs::fs_impls::ext4) fn tid(&self) -> Option<Tid> {
        self.handle.as_ref().map(Handle::tid)
    }
}

impl Drop for OpHandle {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        // Grab the journal for the owner release before `journal_stop` consumes
        // the handle. A dropped journal (`Err`) means teardown, where the owner
        // set dies with it — nothing to release.
        let journal = handle.journal();
        if let Err(e) = transaction::journal_stop(handle) {
            error!("ext4 journal_stop at operation end failed: {:?}", e);
        }
        if let (Some(id), Ok(journal)) = (self.owner, journal) {
            journal.release_op_handle(id);
        }
    }
}

/// Journal transaction id (jbd2 `tid_t`).
///
/// A newtype over the wrapping `u32` sequence number, so a tid cannot be mixed
/// up with the journal's other `u32` quantities (log block indices, geometry
/// counts) and the wrapping successor/comparison rules live on the type.
/// Deliberately **no `Ord`**: a plain `>=` is only meaningful within a
/// non-wrapping window, so "is at or after" must go through [`Tid::geq`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Tid(u32);

impl Tid {
    /// Wraps a raw sequence number. The callers are the boundaries where a tid
    /// enters typed code: the on-disk big-endian `h_sequence`/`s_sequence`
    /// fields (every value is a valid tid, so no validation applies) and test
    /// fixtures.
    pub(super) const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// Returns the raw sequence number, for the boundaries that store a bare
    /// `u32`: the on-disk big-endian fields and the `committed_tid` atomic.
    pub(super) const fn get(self) -> u32 {
        self.0
    }

    /// Returns the next tid in sequence (tids wrap at `u32::MAX`).
    pub(super) const fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }

    /// Returns the previous tid in sequence (tids wrap at `u32::MAX`) — used
    /// to seed `committed_tid` one behind the first tid a commit will bear.
    pub(super) const fn prev(self) -> Self {
        Self(self.0.wrapping_sub(1))
    }

    /// Wrapping-aware "is `self` at or after `other`?" (jbd2 `tid_geq`).
    ///
    /// Tids are a monotonically increasing `u32` that wrap at `u32::MAX`. A
    /// plain `a >= b` would answer wrongly across a wrap (e.g. `0` is *after*
    /// `u32::MAX`, but `0 >= u32::MAX` is `false`). jbd2 solves this by working
    /// in the signed difference: `(a - b)` computed with wrapping arithmetic,
    /// reinterpreted as an `i32`, is `>= 0` exactly when `a` is within half the
    /// id space *ahead of* `b`. This is the comparison
    /// [`Journal::log_wait_commit`] uses to decide whether the target
    /// transaction has already committed.
    pub(super) fn geq(self, other: Tid) -> bool {
        // The `as` below is a same-width sign reinterpret, not a narrowing: it
        // IS the jbd2 wraparound algorithm (`tid_geq`, include/linux/jbd2.h),
        // kept verbatim (ledger: `tid-geq-sign-reinterpret`).
        (self.0.wrapping_sub(other.0) as i32) >= 0
    }
}

/// The parsed geometry of the on-disk journal.
///
/// It resolves each log block to its physical device block and holds the
/// validated journal superblock. Later tasks read and write the log through this
/// map; Task 1 only builds and validates it.
pub(super) struct JournalGeometry {
    /// Log block index → physical device block; `len() == maxlen`.
    block_map: Vec<Ext4Bid>,
    /// The validated journal superblock (log block 0).
    superblock: JournalSuperblock,
}

impl JournalGeometry {
    /// Returns the total number of log blocks (`s_maxlen`).
    ///
    /// Used by [`Journal::max_credits`], so it is live even in non-ktest builds.
    pub(super) fn maxlen(&self) -> u32 {
        self.superblock.maxlen()
    }

    /// Returns the first log block that holds log data (`s_first`).
    ///
    /// Used by [`Journal::max_credits`], so it is live even in non-ktest builds.
    pub(super) fn first(&self) -> u32 {
        self.superblock.first()
    }

    /// Returns the first transaction id expected on recovery (`s_sequence`).
    ///
    /// Used by [`Journal::new`], so it is live even in non-ktest builds.
    pub(super) fn sequence(&self) -> Tid {
        self.superblock.sequence()
    }

    /// Returns the descriptor-tag geometry this journal's feature bits select
    /// — the single source of truth for the tag byte layout, derived once at
    /// parse ([`format::TagLayout`]) and consumed by the commit writer, the
    /// recovery scanner, and the checkpoint/replay applier.
    ///
    /// Private to the journal module (unlike the `pub(super)` geometry
    /// accessors): the tag layout is a journal-internal concern, and
    /// [`TagLayout`] itself is not visible above the journal.
    fn tag_layout(&self) -> TagLayout {
        self.superblock.tag_layout()
    }

    /// Returns the csum v2/v3 seed, present iff the journal carries either
    /// checksum feature — derived once at parse ([`format::JournalCsumSeed`]).
    /// The recovery scanner and the checkpoint/replay applier gate every
    /// log-block checksum verification on this one value.
    ///
    /// Private to the journal module for the same reason as
    /// [`tag_layout`](Self::tag_layout): the seed is a journal-internal
    /// concern.
    fn csum_seed(&self) -> Option<JournalCsumSeed> {
        self.superblock.csum_seed()
    }

    /// Returns the log block where recovery starts (`s_start`), or `None`
    /// when the journal is clean (the on-disk `0` sentinel, decoded at the
    /// parse boundary).
    ///
    /// Used by [`Journal::new`] to seed the tail, so it is live in non-ktest.
    pub(super) fn start(&self) -> Option<u32> {
        self.superblock.start()
    }

    /// Returns the journal block size in bytes (`s_blocksize`).
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn blocksize(&self) -> u32 {
        self.superblock.blocksize()
    }

    /// Maps a log block index to its physical device block, or `None` if the
    /// index is past the end of the log.
    ///
    /// Referenced by the commit pipeline, so it is live in non-ktest.
    pub(super) fn log_block_to_physical(&self, log: u32) -> Option<Ext4Bid> {
        self.block_map.get(log as usize).copied()
    }

    /// Returns the next log block after `cur`, wrapping to `first` at the end
    /// of the log.
    ///
    /// The usable log is the ring `[first, maxlen)`; block 0 holds the journal
    /// superblock and is never a log-data block, so wrapping returns to
    /// `first`. The commit writer, the recovery scanner, and the checkpoint
    /// reader all walk the ring through this one definition, so the wrap rule
    /// stays byte-for-byte consistent.
    pub(super) fn next_log_block(&self, cur: u32) -> u32 {
        let next = cur + 1;
        if next >= self.maxlen() {
            self.first()
        } else {
            next
        }
    }

    /// Advances a log position past `count` blocks, wrapping within the ring.
    pub(super) fn advance(&self, mut pos: u32, count: u32) -> u32 {
        for _ in 0..count {
            pos = self.next_log_block(pos);
        }
        pos
    }

    /// Returns the number of log blocks a commit may write without touching
    /// the un-checkpointed tail: the ring distance from `head` forward to
    /// `dirty_tail` — the committed-but-un-checkpointed log occupies
    /// `[tail, head)` in ring order, so `[head, tail)` is free — or the whole
    /// usable ring `[first, maxlen)` when nothing awaits checkpoint
    /// (`dirty_tail` is `None`). On a dirty ring `head == tail` means the
    /// ring is FULL (the head has wrapped all the way around to the tail), so
    /// the distance is 0, never the ring size — a dirty ring is nonempty by
    /// definition.
    ///
    /// Both positions must be ring positions in `[first, maxlen)`, the
    /// invariant [`next_log_block`](Self::next_log_block) /
    /// [`advance`](Self::advance) maintain.
    pub(super) fn free_log_blocks(&self, head: u32, dirty_tail: Option<u32>) -> u32 {
        let ring = self.maxlen() - self.first();
        let Some(tail) = dirty_tail else {
            return ring;
        };
        debug_assert!(self.first() <= head && head < self.maxlen());
        debug_assert!(self.first() <= tail && tail < self.maxlen());
        // Work in ring offsets (position - first) so the wrap subtraction
        // cannot underflow.
        let head_off = head - self.first();
        let tail_off = tail - self.first();
        if tail_off >= head_off {
            tail_off - head_off
        } else {
            ring - (head_off - tail_off)
        }
    }

    /// Reads a full [`BLOCK_SIZE`] log block by its log index into `buf`.
    ///
    /// Resolves log block `log` to its physical device block via the block
    /// map, then reads the whole block. Errors `EUCLEAN` if the index is past
    /// the log, `EIO` on a device failure. The recovery scanner and the
    /// checkpoint reader are sibling readers of the same log, so both resolve
    /// and read blocks through this one definition.
    pub(super) fn read_log_block(
        &self,
        device: &dyn BlockDevice,
        log: u32,
        buf: &mut [u8; BLOCK_SIZE],
    ) -> Result<()> {
        let pblock = self
            .log_block_to_physical(log)
            .ok_or_else(|| Error::with_message(Errno::EUCLEAN, "log block out of range"))?;
        device
            .read_bytes(Bid::new(pblock).to_offset(), buf.as_mut_slice())
            .map_err(|_| Error::with_message(Errno::EIO, "failed to read journal log block"))
    }

    /// Returns the physical byte offset of the journal superblock (log block
    /// 0), the one location [`read_raw_superblock`](Self::read_raw_superblock)
    /// and [`write_superblock`](Self::write_superblock) address.
    fn superblock_offset(&self) -> Result<usize> {
        let sb_pblock = self.log_block_to_physical(0).ok_or_else(|| {
            Error::with_message(Errno::EUCLEAN, "journal superblock block unmapped")
        })?;
        Ok(Bid::new(sb_pblock).to_offset())
    }

    /// Reads the raw on-disk journal superblock (log block 0), the
    /// read-modify-write source for [`write_superblock`](Self::write_superblock):
    /// callers patch the fields they own and hand the struct back to the
    /// funnel. Private to the journal module, like
    /// [`tag_layout`](Self::tag_layout): the raw superblock is a
    /// journal-internal concern.
    fn read_raw_superblock(&self, device: &dyn BlockDevice) -> Result<RawJournalSuperblock> {
        device
            .read_val(self.superblock_offset()?)
            .map_err(|_| Error::with_message(Errno::EIO, "failed to read journal superblock"))
    }

    /// Writes the on-disk journal superblock (log block 0) and barriers — THE
    /// journal-superblock serialization funnel: the commit step-5 tail
    /// publish, the checkpoint clean rewrite, recovery's clean rewrite, and
    /// the D-4 mount upgrade all serialize through here, so a csum journal's
    /// superblock can never reach the device unstamped. This mirrors Linux,
    /// where every journal-superblock write funnels through
    /// `jbd2_write_superblock`, which stamps `s_checksum` under csum v2/v3
    /// (fs/jbd2/journal.c:1812-1813). The stamp keys off the raw's **own**
    /// feature bits ([`RawJournalSuperblock::stamp_checksum`]), so the funnel
    /// is total across v0 superblocks (bytes untouched — the frozen path) and
    /// the upgrade's just-flipped one alike.
    ///
    /// The trailing barrier is part of the funnel: every superblock rewrite
    /// moves the recoverability pointer (`s_start`/`s_sequence`) or the
    /// feature set, and none may be claimed done before it is durable — the
    /// same barrier each pre-P7a-4 site issued by hand.
    ///
    /// Private to the journal module (the sites above are all inside it),
    /// like [`read_raw_superblock`](Self::read_raw_superblock).
    fn write_superblock(
        &self,
        device: &dyn BlockDevice,
        mut raw: RawJournalSuperblock,
    ) -> Result<()> {
        raw.stamp_checksum();
        device
            .write_val(self.superblock_offset()?, &raw)
            .map_err(|_| Error::with_message(Errno::EIO, "failed to write journal superblock"))?;
        commit::barrier(device)
    }
}

/// The filesystem-side feature bits that drive the mount-time journal feature
/// upgrade ([`upgrade_journal_on_mount`], project decision D-4) — a plain-data
/// view, so the journal module reads nothing of the fs superblock itself.
pub(in crate::fs::fs_impls::ext4) struct JournalUpgradeNeeds {
    /// The fs carries `metadata_csum` — the upgrade trigger: Linux picks
    /// journal CSUM_V3 exactly for it (`set_journal_csum_feature_set`,
    /// fs/ext4/super.c:4078-4092).
    pub(in crate::fs::fs_impls::ext4) fs_has_metadata_csum: bool,
    /// The fs carries `64bit` — adds `INCOMPAT_64BIT` to the upgrade set
    /// (`ext4_load_and_init_journal`, fs/ext4/super.c:4909-4912).
    pub(in crate::fs::fs_impls::ext4) fs_is_64bit: bool,
}

/// The outcome of [`upgrade_journal_on_mount`], `#[must_use]` because it
/// carries an obligation: a rewritten superblock invalidates the parsed
/// [`JournalGeometry`] (its tag layout / csum seed were derived from the
/// pre-upgrade bytes), so the caller must examine the outcome and reload the
/// geometry when `upgraded` — dropping the value unexamined warns.
#[must_use = "a rewritten journal superblock obligates a geometry reload"]
pub(in crate::fs::fs_impls::ext4) struct JournalUpgradeOutcome {
    /// Whether the on-disk journal superblock was rewritten.
    pub(in crate::fs::fs_impls::ext4) upgraded: bool,
}

/// Upgrades a fresh (featureless) journal to match the filesystem's checksum
/// and width features at mount time — project decision D-4, a deliberately
/// NARROWER policy than Linux's. Linux clear-and-resets the journal's csum
/// features on every RW mount (`set_journal_csum_feature_set` clears
/// COMPAT_CHECKSUM + CSUM_V2 + CSUM_V3 and then sets what the fs wants,
/// fs/ext4/super.c:4094-4104 — so a v2 journal is upgraded to v3 — and
/// `ext4_load_and_init_journal` adds 64BIT to already-featured journals,
/// super.c:4909-4912). D-4 instead honors any pre-featured journal verbatim:
/// never downgrade, never reshape. Consequences: a v2 journal stays v2
/// (fully supported end to end), and a featured journal never gains 64BIT —
/// a block number past 2^32 then fails loudly at tag time (`EFBIG`) rather
/// than being reshaped under a live log. Only the "fresh featureless journal
/// on a metadata_csum fs" row acts, and there the outcome matches Linux's.
///
/// Returns a [`JournalUpgradeOutcome`] saying whether the on-disk journal
/// superblock was rewritten; when it was, the caller must reload the journal
/// geometry, because the in-memory tag layout / csum seed were parsed from
/// the pre-upgrade bytes (the outcome type is `#[must_use]` so the obligation
/// cannot be dropped silently).
///
/// # Policy (fs `metadata_csum` × journal features → action)
///
/// | fs metadata_csum | journal INCOMPAT bits | action                        |
/// |------------------|-----------------------|-------------------------------|
/// | no               | any (even csum)       | untouched — v0 path frozen; a |
/// |                  |                       | csum journal (tune2fs oddity) |
/// |                  |                       | is honored as-is              |
/// | yes              | any nonzero           | untouched — a Linux-touched   |
/// |                  |                       | journal keeps its features;   |
/// |                  |                       | never downgraded or reshaped  |
/// | yes              | none                  | set CSUM_V3 (+ 64BIT iff the  |
/// |                  |                       | fs is 64bit), name crc32c,    |
/// |                  |                       | stamp `s_checksum`, write     |
/// |                  |                       | with a barrier                |
///
/// D-4 deliberately diverges from Linux for a 64bit fs *without*
/// `metadata_csum` (Linux would still add journal 64BIT): such volumes stay
/// on the frozen v0 byte path — the whole pre-P7 crash-matrix baseline rides
/// on it — and a block number past 2^32 fails loudly at tag time (`EFBIG`)
/// rather than corrupting.
///
/// # Timing / crash safety
///
/// Must run after mount-time recovery and before the journal is published
/// for new transactions, and acts only on a clean, **empty** journal
/// (`s_start == 0`): the log then contains no transaction, so no logged byte
/// exists whose parse the new tag geometry could change — a crash before the
/// superblock write leaves the old featureless journal, a crash after it the
/// upgraded one, and either way the next mount scans an empty log. That is
/// what makes the flip a single plain superblock write and still crash-safe.
pub(in crate::fs::fs_impls::ext4) fn upgrade_journal_on_mount(
    geometry: &JournalGeometry,
    device: &dyn BlockDevice,
    needs: JournalUpgradeNeeds,
) -> Result<JournalUpgradeOutcome> {
    const UNTOUCHED: JournalUpgradeOutcome = JournalUpgradeOutcome { upgraded: false };

    // Only a metadata_csum fs upgrades its journal (D-4, first table row).
    if !needs.fs_has_metadata_csum {
        return Ok(UNTOUCHED);
    }

    let mut raw = geometry.read_raw_superblock(device)?;

    // A V1 superblock cannot carry feature bits at all; Linux refuses to set
    // features on one (`jbd2_format_support_feature`). Leave it alone.
    if raw.header.h_blocktype.get() != BLOCKTYPE_SUPERBLOCK_V2 {
        return Ok(UNTOUCHED);
    }
    // Any pre-existing INCOMPAT feature means a Linux-touched journal: honor
    // its choices verbatim (second table row).
    if raw.s_feature_incompat.get() != 0 {
        return Ok(UNTOUCHED);
    }
    // The flip is only crash-safe on a clean, empty log (see the docs).
    // Recovery already ran by the time the mount calls this, so a dirty
    // `s_start` is an anomaly (a dirty log the fs superblock did not flag for
    // recovery) we leave untouched rather than re-shape under.
    if raw.recovery_start().is_some() {
        return Ok(UNTOUCHED);
    }

    let mut incompat = INCOMPAT_CSUM_V3;
    if needs.fs_is_64bit {
        incompat |= INCOMPAT_64BIT;
    }
    raw.s_feature_incompat = Be32::new(incompat);
    // v3 names its algorithm and supersedes the v1 COMPAT checksum
    // (`jbd2_journal_set_features`, fs/jbd2/journal.c:2349-2353).
    raw.s_checksum_type = JBD2_CRC32C_CHKSUM;
    raw.s_feature_compat = Be32::new(raw.s_feature_compat.get() & !COMPAT_CHECKSUM);

    // The funnel stamps `s_checksum` (the bits above make this a csum
    // superblock) and barriers the write.
    geometry.write_superblock(device, raw)?;
    Ok(JournalUpgradeOutcome { upgraded: true })
}

/// Loads the journal geometry: reads the journal inode (ino 8), maps its blocks,
/// and parses and validates the on-disk journal superblock (log block 0).
///
/// Returns `Ok(None)` when the volume has no journal (the `has_journal` compat
/// feature is clear). Otherwise the returned [`JournalGeometry`] resolves every
/// log block to its physical device block.
///
/// Called from [`Ext4::open`](super::fs::Ext4) at mount time (the integration
/// point), so it is `pub(in crate::fs::fs_impls::ext4)` and live in all builds.
pub(in crate::fs::fs_impls::ext4) fn load_geometry(
    fs: &Arc<Ext4>,
) -> Result<Option<JournalGeometry>> {
    // No journal: nothing to load. The recovery/commit machinery simply stays
    // disabled for this volume.
    if !fs
        .super_block()
        .feature_compat()
        .contains(FeatureCompatSet::HAS_JOURNAL)
    {
        return Ok(None);
    }

    // Mount contract (report §4.5): only an *internal* journal at the fixed
    // reserved inode is supported. A superblock naming a different journal
    // inode, or an external journal device, must be rejected rather than
    // silently parsed as an ino-8 internal journal (which would then be
    // "replayed" from unrelated file content).
    if fs.super_block().journal_ino() != JOURNAL_INO {
        return_errno_with_message!(
            Errno::EINVAL,
            "unsupported journal inode number (only the reserved ino 8 is supported)"
        );
    }
    if fs.super_block().journal_dev() != 0 {
        return_errno_with_message!(Errno::EINVAL, "external journal devices are unsupported");
    }

    let desc = fs.read_inode_desc(JOURNAL_INO)?;
    if !desc.is_extent_based() {
        return_errno_with_message!(
            Errno::EUCLEAN,
            "journal inode is not extent-mapped (Phase 4 requires an extents journal)"
        );
    }

    // The journal file spans this many blocks; its log data starts at block 0.
    // The on-disk size is untrusted: reject one whose block count overflows
    // u32 instead of silently truncating the geometry.
    let Ok(nblocks) = u32::try_from(desc.size().div_ceil(BLOCK_SIZE as u64)) else {
        return_errno_with_message!(Errno::EUCLEAN, "journal inode is too large");
    };
    if nblocks < 2 {
        return_errno_with_message!(Errno::EUCLEAN, "journal inode is too small");
    }

    let block_map = desc.map_all_blocks(fs.this(), nblocks)?;

    // Log block 0 holds the journal superblock.
    let raw: RawJournalSuperblock = fs
        .block_device()
        .read_val(Bid::new(block_map[0]).to_offset())
        .map_err(|_| Error::with_message(Errno::EIO, "failed to read the journal superblock"))?;

    // Feature words carry meaning only on a V2 superblock (Linux
    // `journal_check_superblock` succeeds on a V1 BEFORE any feature gate,
    // via `jbd2_format_support_feature`, fs/jbd2/journal.c:1379-1380): a V1
    // journal is admitted whatever its feature bytes hold, and the parse
    // below reads it as featureless v0. Both feature-word gates live here,
    // together, on the raw bits.
    if raw.header.h_blocktype.get() == BLOCKTYPE_SUPERBLOCK_V2 {
        // Admission gate (mount policy, [`format::INCOMPAT_SUPP`]): refuse
        // every INCOMPAT feature we do not honor end-to-end BEFORE parsing
        // further. As of P7a-4 the set is REVOKE | 64BIT | CSUM_V2 | CSUM_V3
        // — the csum and width layouts parse (P7a-2), recovery verifies them
        // (P7a-3), and the commit pipeline stamps them (P7a-4), so an
        // admitted journal round-trips through our own recovery. Async/fast
        // commit stay refused, and csum_v2 + csum_v3 together is refused by
        // the parse below.
        if raw.s_feature_incompat.get() & !INCOMPAT_SUPP != 0 {
            return_errno_with_message!(
                Errno::EINVAL,
                "journal has an unsupported incompatible feature"
            );
        }
        // Linux refuses ANY unknown journal ro_compat bit at load
        // (fs/jbd2/journal.c:1382-1388), and `JBD2_KNOWN_ROCOMPAT_FEATURES`
        // is empty in 6.6 — so any set bit refuses. RO_COMPAT semantics
        // ("safe to read, unsafe to write") offer no fallback here either:
        // this port always mounts read-write.
        if raw.s_feature_ro_compat.get() != 0 {
            return_errno_with_message!(
                Errno::EINVAL,
                "journal has an unsupported read-only compatible feature"
            );
        }
    }

    let superblock = JournalSuperblock::try_from(raw)?;

    // The superblock's declared length must fit within the blocks the inode
    // actually maps, or the log map would be short.
    if superblock.maxlen() as usize > block_map.len() {
        return_errno_with_message!(
            Errno::EUCLEAN,
            "journal s_maxlen exceeds the mapped journal inode size"
        );
    }

    Ok(Some(JournalGeometry {
        block_map,
        superblock,
    }))
}

/// The in-memory journal: the parsed geometry, the running-transaction state,
/// the committed-tid counter, and the background commit thread (jbd2
/// `journal_t`).
///
/// Constructed by [`Journal::new`] from [`Ext4::open`](super::fs::Ext4) on a
/// journaled mount (after the `Ext4` `Arc` exists, since `load_geometry` reads
/// the journal inode through the fs). `Ext4::open` recovers a dirty log, starts
/// the commit thread, and holds the journal; `Ext4::drop` stops the thread.
///
/// # Commit-thread model (jbd2 `kjournald`)
///
/// A single background thread ([`Journal::start_commit_thread`]) is the **sole**
/// committer: it is the only path that drives
/// [`try_commit_transaction`](commit::try_commit_transaction) (through
/// [`Journal::commit_or_drain_tail`]) in production, besides the unmount
/// flush, which runs strictly after the thread stops. Any number of
/// [`log_wait_commit`](Journal::log_wait_commit) callers only *wait* for a
/// tid to become durable — they never commit — so commit is serialized to one
/// transaction at a time without an explicit commit lock. The thread's closure
/// holds a [`Weak<Journal>`] so it never keeps the journal alive; this is what
/// lets teardown work (see [`Journal::stop_commit_thread`]).
///
/// Phase 4 does the ordered-data flush **synchronously inside the commit thread**
/// (task context): there is no interrupt handoff / async-writeback-completion
/// path — that jbd2 optimization is deferred to Phase 7.
///
/// # Teardown contract
///
/// The journal's owner ([`Ext4`]) MUST call
/// [`Journal::stop_commit_thread`] exactly once, from a thread **other than the
/// commit thread** (i.e. from `Ext4::drop`), before the last strong reference to
/// the journal goes away. Because the commit thread holds only a `Weak`, it can
/// never itself be the last strong-ref holder, so it can never trigger a
/// join-on-self. There is deliberately **no `join()` in a `Drop` impl** (see
/// [`Journal::stop_commit_thread`]).
///
/// # Lock order
///
/// The commit thread takes the [`state`](Journal::state) lock only for short
/// windows: staging the running transaction out (`running.take()` + the slot
/// install), the pipeline's phase advances, the step-6 publication, and the
/// slot retirement. It commits *without* holding that lock — commit does
/// device I/O, which must never happen under the journal state lock (and it
/// takes no inode lock at all: the ordered-data flush works on page-cache
/// handles cloned in at registration time, the P5 deadlock invariant) —
/// matching the leaf position the commit pipeline already documents.
pub(super) struct Journal {
    /// The parsed on-disk geometry (the log block map + journal superblock).
    geometry: JournalGeometry,
    /// The block device the log lives on, so the commit thread can drive
    /// [`try_commit_transaction`](commit::try_commit_transaction) without
    /// threading the device through every wakeup.
    ///
    /// Held as a strong [`Arc`]: the device outlives the filesystem and does not
    /// hold the journal, so there is no reference cycle. (The cycle to avoid is
    /// the *thread → journal* one, handled by the thread's `Weak`, not this.)
    device: Arc<dyn BlockDevice>,
    /// The running-transaction state, guarded for the lifecycle operations.
    state: RwMutex<JournalState>,
    /// The id of the most recently committed transaction (jbd2
    /// `journal_t.j_commit_sequence`).
    ///
    /// An atomic, not part of [`JournalState`], so `log_wait_commit` can observe
    /// commit progress without contending on the state lock.
    committed_tid: AtomicU32,
    /// Wakes the commit thread to request a commit of the running transaction
    /// (jbd2 `j_wait_commit`-ish trigger). Woken by
    /// [`request_commit`](Journal::request_commit) and by
    /// [`stop_commit_thread`](Journal::stop_commit_thread).
    commit_trigger: WaitQueue,
    /// Where [`log_wait_commit`](Journal::log_wait_commit) sleepers wait for
    /// `committed_tid` to advance (jbd2 `j_wait_done_commit`). Woken by the commit
    /// thread after each successful commit, and by
    /// [`note_credits_released`](Journal::note_credits_released) so
    /// capacity-blocked `journal_start`s re-check room that opened up without a
    /// commit.
    commit_wait_queue: WaitQueue,
    /// Bumped whenever a handle releases its credit reservation
    /// (`journal_stop`), so a `journal_start` blocked on a full transaction can
    /// tell "room may have opened up" apart from a spurious wake — a full
    /// transaction whose reservations were never captured is not committable,
    /// so waiting on its commit alone could sleep forever while the space it
    /// held was already released.
    credit_release_epoch: AtomicU64,
    /// Set by [`stop_commit_thread`](Journal::stop_commit_thread) to make the
    /// commit thread exit its loop on the next wake.
    stop: AtomicBool,
    /// Set when a commit fails (jbd2 journal abort, minimal form).
    ///
    /// A failed [`commit_or_drain_tail`](Journal::commit_or_drain_tail) lost
    /// its transaction — consumed mid-write, or dropped because the ring
    /// could not make room even after draining the tail: the in-memory
    /// metadata is ahead of both the log and the device, and the retained
    /// after-images seeded from it can never be checkpointed — continuing to
    /// journal would publish fragments of the lost transaction through later
    /// commits. So the journal refuses further work: `journal_start` returns
    /// `EIO`, `log_wait_commit` sleepers wake with `EIO` (instead of hanging
    /// forever on a tid that will never commit), and the commit thread stops
    /// checkpointing. The full jbd2 abort/errno machinery is Phase 7.
    aborted: AtomicBool,
    /// The commit-thread handle, taken and joined by
    /// [`stop_commit_thread`](Journal::stop_commit_thread). A `Mutex<Option<_>>`
    /// so start/stop can move it in and out; it is **not** held while the thread
    /// runs (the thread itself lives on via the scheduler, referenced only weakly
    /// from its own closure).
    commit_thread: Mutex<Option<Arc<crate::thread::Thread>>>,
    /// The tasks currently holding an operation handle on THIS journal, keyed
    /// by [`Task`](ostd::task::Task) address — jbd2's per-task
    /// `current->journal_info`, a set here because operations on distinct tasks
    /// run concurrently. It backs the nested-`begin_op` deadlock guard: a task
    /// already in the set that opens a second handle is the c2 self-deadlock
    /// (see [`OpHandle::start`]). Populated only at the [`OpHandle`] boundary,
    /// where the one-operation-per-task invariant lives; the lower-level
    /// [`journal_start`](transaction::journal_start) primitive stays unguarded
    /// so its transaction-mechanics tests can drive several handles from one
    /// task. A leaf lock — taken alone, released before any other — so it adds
    /// no edge to the journal lock order.
    op_handle_owners: Mutex<BTreeSet<usize>>,
    /// Serializes the journal superblock's tail publication — jbd2
    /// `j_checkpoint_mutex`. Held across a whole checkpoint pass
    /// ([`checkpoint`](checkpoint::checkpoint), apply → barrier →
    /// `s_start`/`s_sequence`/`s_head` rewrite → barrier) and around the
    /// commit pipeline's clean→dirty `s_start` write (step 5), so the
    /// on-disk journal superblock has ONE writer at a time.
    ///
    /// Today every writer runs on the single commit thread (the unmount
    /// flush strictly after it stops), so the mutex never contends; it is
    /// the designated serialization point that makes "one journal-superblock
    /// writer" structural rather than incidental, and correct by
    /// construction if a second checkpoint driver is ever added (P7d/P7e
    /// concurrency). Lock order: `j_checkpoint` → `state` (a pass takes the
    /// state lock only for its brief snapshot/publish windows, never the
    /// reverse), it is NOT nested with the inode→handle→…→sb main chain
    /// (checkpoint runs on the commit thread, holding no filesystem lock),
    /// and it is NEVER held across a wait-for-commit.
    j_checkpoint: Mutex<()>,
    /// Set by a `journal_start` blocked on log space
    /// ([`wait_for_log_space`](Journal::wait_for_log_space)) to demand the
    /// commit thread checkpoint the tail forward NOW — the space
    /// backpressure that keeps the commit-time fit guard's drain path the
    /// exception (jbd2 `__jbd2_log_wait_for_space`). The commit thread
    /// clears it and runs a full reclaim pass; a stale set only over-
    /// checkpoints (harmless), and a waiter still short re-sets it.
    space_pressure: AtomicBool,
    /// Whether a background commit thread is running to service the
    /// checkpoint / space-backpressure waits: set by
    /// [`start_commit_thread`](Journal::start_commit_thread), cleared at the
    /// entry of [`stop_commit_thread`](Journal::stop_commit_thread). A
    /// `journal_start` only *waits* for log space when this is true; without
    /// a servicer (a no-thread test fixture, or teardown) it proceeds and
    /// the commit-time fit guard drains inline instead, so the wait can
    /// never hang on an absent committer.
    commit_servicer: AtomicBool,
    /// How many times a `journal_start` actually parked on the log-space gate
    /// ([`wait_for_log_space`](Journal::wait_for_log_space)) — the count of
    /// reservation-time backpressure events, distinct from the commit-time
    /// inline-drain backstop. Bumped once at entry to the wait (past the
    /// no-servicer bypass), so a nonzero value proves the reservation gate,
    /// not the backstop, absorbed the pressure. Exists for the small-journal
    /// pressure tests to assert backpressure genuinely fired.
    log_space_waits: AtomicU64,
}

/// One pinned freed block run: `count` blocks whose free was discharged under
/// the transaction `tid`, held out of the allocator until that transaction
/// commits (see [`JournalState::pinned_frees`]).
//
// Visible at the `ext4` level only because it is a value of the
// equally-visible `JournalState`'s map; it is constructed and read only
// inside the journal module.
pub(in crate::fs::fs_impls::ext4) struct PinnedRun {
    /// Blocks in the run.
    count: u32,
    /// The freeing transaction — the pin releases when it commits.
    tid: Tid,
}

/// The transaction currently occupying the commit pipeline
/// (`journal_t.j_committing_transaction`), resident in [`JournalState`] so
/// every consumer of "the in-flight transaction" reads it from the one
/// lock-guarded place: the checkpoint pass's defer-prefix snapshot (its
/// unpublished revokes), `sync(2)`'s durability probe
/// ([`Journal::commit_and_wait_running`], its tid), and the pipeline's own
/// phase walk. Capture seeding does NOT read the slot: the transaction's
/// after-images were stashed into [`JournalState::uncheckpointed`] in the
/// same lock window that staged it (see that field's docs) — the stash is
/// the image-visibility half, the slot is the control half.
///
/// The `Arc` is shared with the committer, which runs the pipeline's device
/// I/O through its own clone WITHOUT the state lock. That share is sound
/// because the transaction is **frozen** at staging: every handle has
/// closed, `&mut` access is gone with `running.take()`, and both the forget
/// path ([`RevokeDuty::discharge`](revoke::RevokeDuty)) and the capture
/// funnels reach only [`JournalState::running`] — a committing transaction's
/// captures and revoke set cannot change (the Linux `frozen_data`
/// discipline). Only `phase` here advances, under the state lock, at the
/// pipeline's step boundaries ([`Journal::advance_committing_phase`]).
//
// Visible at the `ext4` level only because it is a value of the
// equally-visible `JournalState`'s slot; it is constructed and read only
// inside the journal module.
pub(in crate::fs::fs_impls::ext4) struct CommittingTxn {
    /// The frozen transaction, shared with the committer's pipeline run.
    txn: Arc<Transaction>,
    /// Where the pipeline stands (jbd2 `t_state`); see [`CommitPhase`].
    phase: CommitPhase,
}

impl CommittingTxn {
    /// Stages `txn` as the committing transaction: the `Running` →
    /// [`Locked`](CommitPhase::Locked) transition (jbd2 `T_LOCKED`; ours is
    /// zero-width on the handle side — a transaction is only ever taken with
    /// zero open handles, so there is nothing to drain).
    fn new(txn: Arc<Transaction>) -> Self {
        Self {
            txn,
            phase: CommitPhase::Locked,
        }
    }

    /// The committing transaction's id.
    pub(super) fn tid(&self) -> Tid {
        self.txn.tid()
    }

    /// The pipeline phase the committing transaction is in.
    pub(super) fn phase(&self) -> CommitPhase {
        self.phase
    }

    /// Copies the committing transaction's not-yet-published revoke set into
    /// `out` — the checkpoint snapshot's committing half (see
    /// [`Transaction::collect_unpublished_revokes`]). Exact, not a racy
    /// sample: the set is frozen (see the type docs) and read under one
    /// state-lock window, and a [`Finished`](CommitPhase::Finished) slot
    /// contributes nothing — commit step 6 publishes the revokes in the
    /// same lock window that advances the phase, so from `Finished` until
    /// retirement the set already lives in [`JournalState::revoked`] and
    /// returning it here would call published revokes unpublished (an
    /// over-defer; harmless, but the exactness claim would be false).
    pub(super) fn collect_unpublished_revokes(&self, out: &mut BTreeSet<Ext4Bid>) {
        if self.phase == CommitPhase::Finished {
            return;
        }
        self.txn.collect_unpublished_revokes(out);
    }

    /// Advances the phase to `to`, refusing anything but the single legal
    /// successor ([`CommitPhase::next_in_pipeline`]) or a tid mismatch with
    /// `EIO` — the commit then fails loudly and the caller aborts the
    /// journal, rather than a panic in production or a pipeline running out
    /// of order.
    fn advance_to(&mut self, tid: Tid, to: CommitPhase) -> Result<()> {
        if self.tid() != tid {
            return_errno_with_message!(
                Errno::EIO,
                "phase advance names a different transaction than the committing one"
            );
        }
        if self.phase.next_in_pipeline() != Some(to) {
            error!(
                "ext4 journal: illegal commit-phase transition {:?} -> {:?}",
                self.phase, to
            );
            return_errno_with_message!(Errno::EIO, "illegal commit-phase transition");
        }
        self.phase = to;
        Ok(())
    }
}

/// The mutable transaction state of a [`Journal`]: the two-transaction
/// pipeline (P7c-1) holds at most one *running* transaction (accepting
/// handles and captures) and at most one *committing* transaction (staged
/// out of `running`, being written to the log) — a new running transaction
/// starts while its predecessor commits, which is what batches concurrent
/// operations into one commit.
//
// Referenced by the transaction lifecycle functions (via `Journal::state_write`),
// so it counts as used in non-ktest builds.
pub(super) struct JournalState {
    /// The single running transaction, if any (`journal_t.j_running_transaction`).
    pub(super) running: Option<Transaction>,
    /// The force-locked transaction draining its open handles (jbd2
    /// `T_LOCKED`), if any — the P7c-2 seat between `running` and
    /// `committing`. The committer parks a due running transaction here when
    /// handles are still open; the transaction's OWN handles keep reaching
    /// it through the tid-keyed funnels ([`active_txn_mut`]) until they
    /// close, while `journal_start` bars new handles AND blocks successor
    /// creation (the locked barrier — see the module docs' group-commit
    /// section for why that block is an isolation requirement, not a
    /// convenience). Invariant: `locking.is_some() ⟹ running.is_none()` —
    /// the successor is born only after this seat stages, so at most one
    /// capture-accepting transaction exists at any instant.
    pub(super) locking: Option<Transaction>,
    /// The newest commit-forcing request (jbd2 `j_commit_request`): a tid
    /// whose transaction must commit without waiting for the size/age
    /// triggers — set by `log_wait_commit` (fsync/O_SYNC/sync(2)), the
    /// capacity escalation ([`Journal::wait_for_transaction_room`]), and
    /// the pinned-block `ENOSPC` retry path
    /// ([`Journal::request_commit_of_running`]). Cleared when a staged
    /// transaction's tid covers it; a stale request (tid at or before
    /// `committed_tid`) never fires — the due check compares it against the
    /// running transaction's tid with `geq`.
    pub(super) commit_request: Option<Tid>,
    /// The transaction staged for (or mid-way through) commit, if any
    /// (`journal_t.j_committing_transaction`) — see [`CommittingTxn`]. Also
    /// what `commit_and_wait_running` waits on for a transaction that left
    /// `running` a moment before the caller looked: otherwise sync(2)
    /// returns while its captures are mid-commit, not yet durable.
    pub(super) committing: Option<CommittingTxn>,
    /// The tid to assign to the next transaction created
    /// (`journal_t.j_transaction_sequence`).
    pub(super) next_tid: Tid,
    /// The next free log block to write, i.e. the current log head (jbd2
    /// `journal_t.j_head`). Wraps within `[first, maxlen)`.
    pub(super) head: u32,
    /// The oldest un-checkpointed transaction's start log block — the on-disk
    /// `s_start` (jbd2 `journal_t.j_tail`). `None` means the journal is clean
    /// (no transaction awaits checkpoint); the on-disk `0` sentinel is decoded
    /// at the disk-format boundary and never travels in-band here.
    pub(super) tail_block: Option<u32>,
    /// The oldest un-checkpointed transaction's id — the on-disk `s_sequence`
    /// (jbd2 `journal_t.j_tail_sequence`).
    pub(super) tail_tid: Tid,
    /// The committed-revoke memory: for each revoked block, the tid of the
    /// newest committed transaction that revoked (freed) it.
    ///
    /// This is what suppresses the S1 runtime clobber: our checkpoint re-reads
    /// the LOG ([`checkpoint::apply_log_transaction`]) — so a block freed and
    /// reused after an older transaction journaled it would be overwritten by
    /// that transaction's log image on the next checkpoint pass, no crash
    /// required. Entries are published by the commit pipeline once a
    /// transaction's commit block is durable (never earlier — the inline
    /// tail-drain checkpoint runs mid-commit and must not trust an erasable
    /// commit's revokes), and retired by [`checkpoint`](checkpoint::checkpoint)
    /// at the same tid boundary that evicts `uncheckpointed`. See [`revoke`].
    pub(super) revoked: revoke::RevokeTable,
    /// The newest post-`running` after-image of each metadata block: the
    /// committing transaction's from the instant it is staged, and every
    /// committed-but-un-checkpointed transaction's until checkpoint writes
    /// the block to its final location.
    ///
    /// This is what makes [`get_write_access`] seeding stale-free (the B-1
    /// invariant, restated for the two-transaction pipeline): the abstract
    /// seed order is *running capture → committing image → un-checkpointed
    /// image → device*, and the middle two stations share this one map —
    /// [`Transaction::stash_uncheckpointed`] runs in the SAME state-lock
    /// window as `running.take()` ([`JournalState::stage_committing`]), so
    /// no instant exists where a new transaction can start but the
    /// committing images are missing, and a committing transaction's entries
    /// are the newest of their blocks by tid order. The whole seed decision
    /// is made in one state-lock window ([`get_write_access`]); the
    /// committing transaction itself is frozen (see [`CommittingTxn`]), so
    /// its images are read-only to seeders; and eviction stays tid-keyed —
    /// [`checkpoint`](checkpoint::checkpoint) evicts only up through its
    /// last *applied* tid, which can never reach a still-mid-flight commit
    /// (its tid is beyond `committed_tid` until its own step 6). The one
    /// mutation that may touch a committing transaction's entry is a forget
    /// ([`RevokeDuty::discharge`](revoke::RevokeDuty)): eviction-on-free is
    /// the forget semantics — the stale bytes must stop seeding — not a
    /// violation of the frozen commit (the LOG copy still commits; revoke
    /// suppression handles it at apply time).
    pub(super) uncheckpointed: BTreeMap<Ext4Bid, transaction::UncheckpointedImage>,
    /// The freed-but-uncommitted block runs, keyed by first block: every run
    /// a live-handle `Ext4::free_blocks` discharged ([`pin_freed_run`]),
    /// tagged with the freeing transaction's tid. While a run is here the
    /// allocator must not hand its blocks out
    /// ([`Journal::pinned_frees_snapshot`] feeds `Ext4::alloc_blocks`'s skip)
    /// — Linux's mballoc discipline: *"We need to make sure we don't reuse
    /// the freed block until after the transaction is committed"*
    /// (`ext4_mb_free_metadata` / `ext4_free_data`, fs/ext4/mballoc.c:6536,
    /// returned to the buddy only by the post-commit callback
    /// `ext4_free_data_in_buddy`). Reuse before the commit is unsound in both
    /// directions: a crash erases the freeing transaction, so the on-disk
    /// bitmap still assigns the block to its OLD life while the NEW owner's
    /// (flushed) bytes sit on it — cross-file exposure with a clean e2fsck —
    /// and at runtime an older log image could be checkpoint-applied over the
    /// reuse (the S1 clobber's last window; see [`checkpoint`]'s snapshot
    /// note).
    ///
    /// Entries are inserted by [`pin_freed_run`] under this state lock (the
    /// free site holds the superblock write lock, serializing it against the
    /// allocator's snapshot) and released by
    /// [`release_pinned_frees`](JournalState::release_pinned_frees) at commit
    /// step 6, in the same critical section that publishes the transaction's
    /// revokes — the pins and the revokes retire together. On journal abort
    /// nothing releases them: the filesystem is dead (`journal_start` refuses
    /// `EIO`), and keeping the pins is the safe default. A crash releases
    /// them trivially (in-memory only), which is exactly correct: the frees
    /// either committed (recovery sees them) or vanished with their
    /// transaction.
    pub(super) pinned_frees: BTreeMap<Ext4Bid, PinnedRun>,
}

impl JournalState {
    /// Stages `txn` as THE committing transaction — the (drained)
    /// `Running`/locking-seat → [`Locked`](CommitPhase::Locked) transition —
    /// retaining its after-images in [`uncheckpointed`](Self::uncheckpointed)
    /// in the same lock window (the B-1 stash: from the very first instant a
    /// new running transaction can exist, captures of these blocks seed from
    /// these bytes, never from the lagging device; a drain-parked
    /// transaction's final own-handle patches are all in by now, since
    /// staging requires `nr_updates() == 0`). The caller must have verified
    /// the slot is empty; all three stagers — the commit thread's pipeline
    /// advance ([`Journal::advance_pipeline`]), the unmount flush
    /// ([`Journal::flush_on_unmount`]), and the ktest stager — refuse an
    /// occupied slot before taking the transaction out of its seat.
    fn stage_committing(&mut self, txn: Transaction) -> Arc<Transaction> {
        debug_assert!(self.committing.is_none());
        debug_assert_eq!(txn.nr_updates(), 0);
        txn.stash_uncheckpointed(&mut self.uncheckpointed);
        // The staged transaction covers any commit request at or before its
        // tid (requests name transactions, and commits are serial).
        if self
            .commit_request
            .is_some_and(|requested| txn.tid().geq(requested))
        {
            self.commit_request = None;
        }
        let txn = Arc::new(txn);
        self.committing = Some(CommittingTxn::new(txn.clone()));
        txn
    }

    /// Advances the committing transaction's pipeline phase (see
    /// [`CommittingTxn::advance_to`]); `EIO` if nothing is committing. This
    /// state-level form exists so commit step 6 can advance to
    /// [`Finished`](CommitPhase::Finished) inside its existing lock window;
    /// [`Journal::advance_committing_phase`] wraps it for the other steps.
    pub(super) fn advance_committing_phase(&mut self, tid: Tid, to: CommitPhase) -> Result<()> {
        let Some(committing) = self.committing.as_mut() else {
            return_errno_with_message!(
                Errno::EIO,
                "phase advance without a committing transaction"
            );
        };
        committing.advance_to(tid, to)
    }

    /// Releases every pinned freed run whose freeing transaction is covered
    /// by `committed` — commit step 6, the moment those frees become durable
    /// (the commit block is behind the step-4 barrier), so their blocks may
    /// re-enter the allocator (Linux `ext4_process_freed_data`, driven by the
    /// jbd2 post-commit callback). Runs of a NEWER (still running)
    /// transaction survive, exactly like the revoke records above this
    /// boundary.
    pub(super) fn release_pinned_frees(&mut self, committed: Tid) {
        self.pinned_frees.retain(|_, run| !committed.geq(run.tid));
    }

    /// The running transaction paired with its tid, if it carries a durable
    /// obligation a commit must retire — a captured metadata after-image, a
    /// revoke record, or a pinned freed run — else `None` for an empty
    /// transaction. This is the shared "committable transaction" gate behind
    /// every commit trigger (age, request, batch, pipeline advance): an empty
    /// transaction must never be made due, since committing it writes no log
    /// block and a waiter on its tid would hang
    /// ([`Journal::log_wait_commit`]).
    ///
    /// The obligation set is checked structurally — metadata OR revokes OR
    /// pins — not "captured metadata alone" as the historical gate did. A
    /// revoke/pin-only transaction (zero captures) is unreachable today only
    /// because [`Ext4::free_blocks`](super::fs::Ext4) captures the group bitmap
    /// before it pins/revokes, so the metadata count is incidentally non-zero
    /// whenever pins or revokes exist; keying the gate on the obligation set
    /// itself keeps "a transaction with durable obligations is always
    /// committable" true structurally, not by that incidental capture. The
    /// transaction ref rides out with the verdict so callers run their trigger
    /// checks without re-deriving or re-unwrapping it.
    fn committable_running(&self) -> Option<(&Transaction, Tid)> {
        let txn = self.running.as_ref()?;
        let tid = txn.tid();
        (txn.has_recorded_work() || self.pins_charged_to(tid)).then_some((txn, tid))
    }

    /// Whether any pinned freed run is charged to `tid` — a durable obligation
    /// that commit step 6 ([`release_pinned_frees`](Self::release_pinned_frees))
    /// must discharge, independent of whether the freeing transaction also
    /// captured metadata.
    fn pins_charged_to(&self, tid: Tid) -> bool {
        self.pinned_frees.values().any(|run| run.tid == tid)
    }
}

impl Journal {
    /// Builds an in-memory journal over a parsed [`JournalGeometry`], with no
    /// running transaction.
    ///
    /// The next tid is seeded from `s_sequence` — the first tid recovery expects
    /// (a fresh `mke2fs` journal has `s_sequence == 1`).
    ///
    /// The log-position state is seeded for a **clean** journal (when a dirty
    /// log was replayed, [`recover`](recovery::recover) re-seeds
    /// head/tail/tids in memory from the replayed log before `Ext4::open`
    /// publishes the journal):
    /// - `head = s_first`: the first writable log block.
    /// - `tail_block = s_start`: `None` when clean, so nothing awaits checkpoint.
    /// - `tail_tid = s_sequence`.
    /// - `committed_tid = s_sequence - 1`: nothing is committed yet, and the
    ///   first commit will bear `s_sequence`.
    ///
    /// The commit thread is **not** started here; the owner calls
    /// [`start_commit_thread`](Journal::start_commit_thread) once it holds the
    /// `Arc<Journal>` (and must later pair it with
    /// [`stop_commit_thread`](Journal::stop_commit_thread)).
    pub(in crate::fs::fs_impls::ext4) fn new(
        geometry: JournalGeometry,
        device: Arc<dyn BlockDevice>,
    ) -> Arc<Self> {
        let next_tid = geometry.sequence();
        let head = geometry.first();
        let tail_block = geometry.start();
        let tail_tid = geometry.sequence();
        // Nothing is committed yet; the first commit will bear `s_sequence`.
        let committed_tid = geometry.sequence().prev();
        // Fields are listed in struct-declaration order (clippy
        // `inconsistent_struct_constructor`); the geometry-derived values above
        // are pulled into locals so `geometry` can be moved in first.
        Arc::new(Self {
            geometry,
            device,
            state: RwMutex::new(JournalState {
                running: None,
                locking: None,
                commit_request: None,
                committing: None,
                next_tid,
                head,
                tail_block,
                tail_tid,
                revoked: revoke::RevokeTable::new(),
                uncheckpointed: BTreeMap::new(),
                pinned_frees: BTreeMap::new(),
            }),
            committed_tid: AtomicU32::new(committed_tid.get()),
            commit_trigger: WaitQueue::new(),
            commit_wait_queue: WaitQueue::new(),
            credit_release_epoch: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            aborted: AtomicBool::new(false),
            commit_thread: Mutex::new(None),
            op_handle_owners: Mutex::new(BTreeSet::new()),
            j_checkpoint: Mutex::new(()),
            space_pressure: AtomicBool::new(false),
            commit_servicer: AtomicBool::new(false),
            log_space_waits: AtomicU64::new(0),
        })
    }

    /// The maximum metadata blocks a single transaction may reserve.
    ///
    /// A transaction of `n` captured blocks and `v` revoke blocks
    /// (`ceil(revokes / entries_per_block)`,
    /// [`Transaction::nr_revoke_blocks`]) occupies, in the log,
    ///
    /// ```text
    /// v revoke blocks + n data blocks + ceil(n / t) descriptors + 1 commit
    /// ```
    ///
    /// where `t` is [`TagLayout::tags_per_descriptor`] (the commit pipeline
    /// starts a fresh descriptor whenever the previous one's tag area fills,
    /// P7a-5; revoke blocks join the chain in P7b-3). The whole footprint
    /// must fit the usable ring (`s_maxlen - s_first`): the exact bound is
    /// the largest `n + v` with `v + n + ceil(n/t) + 1 <= usable`, and this
    /// solves it **conservatively** via `ceil(n/t) <= n/t + 1`:
    ///
    /// ```text
    /// n + v <= t * (usable - 2) / (t + 1)
    /// ```
    ///
    /// The bound is checked against `n + v` (each whole revoke block charged
    /// as one capture-equivalent, `check_capacity`), which keeps the proof:
    /// a revoke block costs one log block and no descriptor tag, so
    /// `v + n + ceil(n/t) + 1 <= (n+v) + ceil((n+v)/t) + 1 <= usable`
    /// whenever `n + v` passes the formula — the revoke charge only
    /// over-counts.
    ///
    /// Under-admitting by a block or two is harmless (`journal_start` just
    /// waits or refuses a little early); over-admitting would be corruption —
    /// the commit's log writes would wrap onto the transaction's own blocks.
    ///
    /// One residue: the revoke charge is only sampled at the capacity gates
    /// (`journal_start`/`extend`/`restart`) — the set itself grows at frees
    /// ([`RevokeDuty::discharge`](revoke::RevokeDuty)), which cannot refuse
    /// (the free already happened in memory). A single restart-less
    /// operation freeing hundreds of thousands of blocks could therefore
    /// outgrow the admitted slack; the commit-time exact fit guard is the
    /// hard line there (refuse → drain → loud `ENOSPC` abort, never an
    /// overwrite), and P7c's `journal_restart` commit boundaries bound the
    /// per-transaction revoke set the way Linux's do.
    ///
    /// This bounds ONE transaction against the whole usable ring; it says
    /// nothing about the log space an un-checkpointed PREDECESSOR still
    /// occupies (the ring can be dirty at commit time — the post-commit
    /// checkpoint is non-fatal on failure). That half is enforced where the
    /// footprint is exact, at commit time:
    /// [`try_commit_transaction`](commit::try_commit_transaction) checks the
    /// chain against the ring's FREE segment
    /// ([`JournalGeometry::free_log_blocks`]) and refuses to write rather
    /// than overwrite the tail, with
    /// [`commit_or_drain_tail`](Self::commit_or_drain_tail) draining the tail
    /// and retrying once before aborting.
    ///
    /// This is the whole-transaction capacity bound (`check_capacity`
    /// compares captured + reserved credits against it); precise
    /// *per-operation* credit accounting is P7's.
    pub(super) fn max_credits(&self) -> usize {
        let usable = (self.geometry.maxlen() - self.geometry.first()) as usize;
        let tags = self.geometry.tag_layout().tags_per_descriptor();
        tags * usable.saturating_sub(2) / (tags + 1)
    }

    /// Acquires the running-transaction state for writing.
    ///
    /// The transaction lifecycle operations ([`journal_start`](transaction::journal_start)
    /// and friends) hold this guard for their whole duration; see the
    /// [`transaction`] module's locking note.
    pub(super) fn state_write(&self) -> RwMutexWriteGuard<'_, JournalState> {
        self.state.write()
    }

    /// Acquires the running-transaction state for reading.
    ///
    /// [`checkpoint`](checkpoint::checkpoint) uses this to snapshot the tail /
    /// committed-tid / head under the lock before doing its (lock-free) device
    /// I/O, mirroring the commit pipeline's "read state, release lock, do I/O"
    /// discipline.
    ///
    /// The state lock's REAL invariant is the leaf discipline: **no
    /// filesystem lock is acquired while holding it** (the capture funnels
    /// take it under the inode/superblock/group locks, never the reverse).
    /// It is *not* "never held across device I/O": the capture-seed read is
    /// the one deliberate exception — [`get_write_access`] holds
    /// `state_write` across [`Transaction::capture_write`]'s device read, so
    /// the seed provenance decision (retained image vs device) and the
    /// capture it feeds are atomic against a concurrent commit's
    /// `running.take()` (the P4 B-1 seeding fix). Every other holder —
    /// commit, checkpoint, recovery — snapshots under the lock and does its
    /// I/O outside it.
    pub(super) fn state_read(&self) -> RwMutexReadGuard<'_, JournalState> {
        self.state.read()
    }

    /// The number of metadata blocks the running transaction has captured, or 0
    /// if none is running. Inspection accessor for op-journaling tests outside the
    /// journal module.
    #[cfg(ktest)]
    pub(in crate::fs::fs_impls::ext4) fn running_nr_metadata_blocks(&self) -> usize {
        self.state
            .read()
            .running
            .as_ref()
            .map_or(0, Transaction::nr_metadata_blocks)
    }

    /// The parsed on-disk geometry (log block map + journal superblock).
    ///
    /// [`checkpoint`](checkpoint::checkpoint) needs it to resolve log blocks to
    /// physical device blocks outside the commit pipeline.
    pub(super) fn geometry(&self) -> &JournalGeometry {
        &self.geometry
    }

    /// The id of the most recently committed transaction (jbd2
    /// `journal_t.j_commit_sequence`).
    ///
    /// Read with `Acquire` so `log_wait_commit` sees the commit pipeline's
    /// writes to this counter without the state lock.
    pub(super) fn committed_tid(&self) -> Tid {
        Tid::new(self.committed_tid.load(Ordering::Acquire))
    }

    /// Snapshots the pinned freed runs as `(first block, count)` pairs — the
    /// allocator's skip set (see [`JournalState::pinned_frees`]).
    ///
    /// Taken once per allocation, under a transient state-lock hold. The
    /// journal state lock is a leaf of the filesystem's lock order (the
    /// capture funnels already take it under the superblock and group locks,
    /// and nothing acquires a filesystem lock while holding it), so reading
    /// here from `Ext4::alloc_blocks` — which holds the superblock write lock
    /// — adds no edge. The snapshot cannot miss a pin: insertion
    /// ([`pin_freed_run`]) runs inside `Ext4::free_blocks`, which needs the
    /// same superblock write lock the allocator is holding. It can only be
    /// conservatively stale the other way — a release (commit step 6) landing
    /// mid-scan merely leaves a just-unpinned block skipped for this attempt.
    pub(in crate::fs::fs_impls::ext4) fn pinned_frees_snapshot(&self) -> Vec<(Ext4Bid, u32)> {
        self.state_read()
            .pinned_frees
            .iter()
            .map(|(&start, run)| (start, run.count))
            .collect()
    }

    /// Spawns the background commit thread (jbd2 `kjournald`).
    ///
    /// The thread's closure holds a [`Weak<Journal>`] so it never keeps the
    /// journal alive — essential for teardown (see
    /// [`stop_commit_thread`](Journal::stop_commit_thread)): otherwise the
    /// journal's `Arc` strong count would never reach `0` and `Drop` would never
    /// run. The thread sleeps in [`commit_trigger`](Journal::commit_trigger) until
    /// woken; on each wake it re-evaluates whether to exit or to commit the
    /// running transaction.
    ///
    /// Must be called once per journal, by the owner, right after construction;
    /// pair it with exactly one [`stop_commit_thread`](Journal::stop_commit_thread).
    pub(in crate::fs::fs_impls::ext4) fn start_commit_thread(self: &Arc<Journal>) {
        // Announce the servicer BEFORE spawning: a `journal_start` may only
        // wait for log space once a commit thread exists to service it (see
        // [`commit_servicer`](Journal::commit_servicer)).
        self.commit_servicer.store(true, Ordering::Release);
        let weak: Weak<Journal> = Arc::downgrade(self);
        let thread = crate::thread::kernel_thread::ThreadOptions::new(move || {
            Self::commit_thread_loop(&weak);
        })
        .spawn();
        *self.commit_thread.lock() = Some(thread);
    }

    /// The commit thread's main loop. Runs on the background thread; reaches the
    /// journal only through `weak`, so it holds no strong reference between wakes.
    fn commit_thread_loop(weak: &Weak<Journal>) {
        loop {
            // Arm the age trigger: the running transaction's fixed commit
            // deadline (jbd2 arms `j_commit_timer` at transaction creation;
            // our timer is this sleep's timeout). The armed key names the
            // transaction the deadline belongs to, so the poll can detect
            // that the answer changed (a transaction was created or gained
            // its first capture — `journal_start`/`journal_stop` wake this
            // thread for exactly that) and re-arm.
            let (armed_age_key, timeout) = {
                let Some(j) = weak.upgrade() else { break };
                j.age_deadline()
            };

            // Sleep until teardown, work, a re-arm, or the age deadline.
            // The closure re-evaluates on every wake, so a spurious wake
            // simply re-checks. Upgrading the `Weak` inside the closure
            // keeps the journal alive only for the duration of the check.
            let action = {
                let Some(j) = weak.upgrade() else { break };
                match j.commit_trigger.wait_until_or_timeout(
                    || Self::poll_commit_action(weak, armed_age_key),
                    timeout.as_ref(),
                ) {
                    Ok(action) => action,
                    // The age deadline fired (`ETIME`): advance — the aged
                    // transaction is now due, or the state moved on and the
                    // next iteration re-arms.
                    Err(_expired) => CommitAction::Advance,
                }
            };

            match action {
                CommitAction::Exit => break,
                CommitAction::Rearm => continue,
                CommitAction::Advance => {}
            }

            let Some(j) = weak.upgrade() else { break };
            // Drive the pipeline until nothing more is immediately
            // committable AND no checkpoint is owed: lock/stage/commit each
            // due transaction, then reclaim the log LAZILY. The loop matters
            // under batching — the size trigger can cross again while a
            // commit's I/O runs, and no further wake is guaranteed to be
            // pending; and a space-pressure wake with nothing to commit must
            // still reach the checkpoint below.
            loop {
                let committed = j.commit_one(false);
                if j.is_aborted() {
                    // A failed commit aborted the journal: the device state
                    // no longer matches the log; checkpointing would make it
                    // worse. Idle until teardown.
                    break;
                }
                // Lazy checkpoint (P7c-3): reclaim the tail only under space
                // pressure — a `journal_start` demanding room (full pass), or
                // the free segment below the low-water mark (incremental) —
                // NOT after every commit. Failure is non-fatal: the log stays
                // dirty and a later pass (or the unmount flush) retries; the
                // dirty tail is protected from overwrite by the commit-time
                // fit guard (`commit_or_drain_tail`). The committing slot is
                // empty here (`commit_one` retired it), so the pass's snapshot
                // collects unpublished forgets only from the running/locking
                // transactions.
                let checkpointed = match j.run_lazy_checkpoint() {
                    Ok(did) => did,
                    Err(e) => {
                        error!("ext4 journal checkpoint failed: {:?}", e);
                        false
                    }
                };
                if !committed && !checkpointed {
                    // Nothing staged and nothing to reclaim: idle, or a parked
                    // transaction is still draining (its last `journal_stop`
                    // wakes us).
                    break;
                }
            }
        }
    }

    /// The commit thread's lazy checkpoint step (jbd2 `__jbd2_log_wait_for_space`
    /// / `jbd2_log_do_checkpoint`, driven from the committer rather than
    /// per-commit). Runs a checkpoint pass ONLY under space pressure:
    ///
    /// - **A demanded reclaim** — a `journal_start` set
    ///   [`space_pressure`](Journal::space_pressure) because a reservation did
    ///   not fit the free segment: reclaim the WHOLE tail (`checkpoint`, full
    ///   pass) so the largest correctly-reserved transaction (`≤ max_credits ≤
    ///   usable`) then fits, and wake the waiter.
    /// - **Steady low-water** — the free segment fell below
    ///   [`checkpoint_low_water`](Journal::checkpoint_low_water): reclaim
    ///   incrementally just up to the low-water mark, keeping ordinary commits
    ///   fitting without a per-commit pass.
    ///
    /// Returns whether it ran a pass (so the caller's loop keeps servicing).
    /// A pass that advances the tail wakes log-space waiters
    /// ([`note_credits_released`](Journal::note_credits_released) bumps the
    /// epoch they sleep on). Single-committer: this and every commit run on
    /// this one thread, so no pass overlaps a commit's step-6 revoke
    /// publication / pin release (duality invariant 1).
    fn run_lazy_checkpoint(&self) -> Result<bool> {
        let demanded = self.space_pressure.swap(false, Ordering::AcqRel);
        // The free segment and whether the log is dirty, read once.
        let (free, dirty) = {
            let st = self.state_read();
            (
                self.geometry.free_log_blocks(st.head, st.tail_block),
                st.tail_block.is_some(),
            )
        };
        if !dirty {
            return Ok(false);
        }
        let target = if demanded {
            // A waiter needs room now: reclaim everything reclaimable so the
            // largest correctly-reserved transaction then fits.
            None
        } else if free < self.checkpoint_low_water() {
            // Steady state: advance the tail just past the low-water mark.
            Some(self.checkpoint_low_water())
        } else {
            return Ok(false);
        };
        let advanced = checkpoint::checkpoint_advance(self, self.device.as_ref(), target)?;
        if advanced {
            // The freed tail may satisfy a `journal_start` blocked in
            // `wait_for_log_space`; bump the epoch it sleeps on.
            self.note_credits_released();
        }
        Ok(advanced)
    }

    /// The commit thread's wait condition: decides whether to exit, advance
    /// the pipeline, re-arm the age timer, or keep waiting.
    ///
    /// Returns `None` (keep waiting) when there is nothing to do. The short
    /// `state.read()` taken here is safe inside a `wait_until` closure: the guard
    /// is created and dropped entirely within this call, never held across a
    /// suspend, and the pipeline advance itself (which needs the *write* lock
    /// and does I/O) happens back in the loop, outside any wait.
    fn poll_commit_action(
        weak: &Weak<Journal>,
        armed_age_key: Option<Tid>,
    ) -> Option<CommitAction> {
        let j = weak.upgrade()?;
        if j.stop.load(Ordering::Acquire) {
            return Some(CommitAction::Exit);
        }
        // An aborted journal commits nothing more; the thread idles until
        // teardown so `stop_commit_thread` still joins it normally.
        if j.is_aborted() {
            return None;
        }
        let st = j.state.read();
        // A mid-flight commit occupies the slot: commits are serial, so
        // there is nothing to do until it retires — keep waiting rather
        // than report work into an advance that refuses, which would turn
        // a stuck/slow commit into a busy loop of poll + checkpoint
        // passes. No lost wakeup: `clear_committing` wakes
        // `commit_trigger` after emptying the slot, and `wait_until`
        // re-checks this predicate before sleeping. Occupied-at-poll is
        // unreachable while the committer is the only production stager
        // (it retires its own stage before re-polling); defense in depth.
        if st.committing.is_some() {
            return None;
        }
        // Space pressure (P7c-3): a `journal_start` blocked in
        // [`wait_for_log_space`](Self::wait_for_log_space) set this to demand a
        // reclaim pass. Advance so [`run_lazy_checkpoint`](Self::run_lazy_checkpoint)
        // frees the tail even with nothing due to commit, and even while a
        // locking seat drains (the tail is reclaimable independently of the
        // drain). Transient — the pass clears it — so this cannot spin.
        if j.space_pressure.load(Ordering::Acquire) {
            return Some(CommitAction::Advance);
        }
        if let Some(locked) = st.locking.as_ref() {
            // A parked transaction stages the moment its drain completes;
            // until then there is nothing to do — the last `journal_stop`
            // of one of its handles wakes this queue.
            return (locked.nr_updates() == 0).then_some(CommitAction::Advance);
        }
        if let Some((txn, _tid)) = st.committable_running()
            && j.commit_triggered(txn, Jiffies::elapsed(), st.commit_request)
        {
            return Some(CommitAction::Advance);
        }
        // Nothing due — but if the transaction the age deadline was armed
        // for is no longer the one that needs one, re-arm the sleep.
        if Self::age_key(&st) != armed_age_key {
            return Some(CommitAction::Rearm);
        }
        None
    }

    /// The identity the age trigger is armed against: the running
    /// transaction's tid, if it carries a durable obligation to age (a
    /// non-committable transaction never becomes due, so arming a deadline for
    /// it would spin the timeout against nothing). Delegates to
    /// [`committable_running`](JournalState::committable_running) so the age
    /// gate is the same obligation set (metadata OR revokes OR pins) as every
    /// other commit trigger.
    fn age_key(st: &JournalState) -> Option<Tid> {
        st.committable_running().map(|(_, tid)| tid)
    }

    /// Computes the commit thread's sleep arming: the age key (see
    /// [`age_key`](Self::age_key)) and the time remaining until that
    /// transaction's commit deadline. The timeout is `None` — an untimed
    /// sleep — when nothing needs aging, or when the jiffies timer manager
    /// is not up (the ktest test-kernel boots no kernel-crate timer
    /// machinery; production initializes it at boot, and the deterministic
    /// ktest age path goes through `force_expire_for_test` + an explicit
    /// wake instead of real timeouts).
    fn age_deadline(&self) -> (Option<Tid>, Option<Duration>) {
        let st = self.state_read();
        // The verdict and the transaction ride out together, so the deadline
        // reads off the same transaction the key names — no re-lookup, no
        // guarded `expect`.
        let Some((txn, tid)) = st.committable_running() else {
            return (None, None);
        };
        let timeout = JIFFIES_TIMER_MANAGER
            .get()
            .is_some()
            .then(|| txn.until_expiry(Jiffies::elapsed()));
        (Some(tid), timeout)
    }

    /// The group-commit trigger test for an already-committable running
    /// transaction — the three triggers (see the module docs): an explicit
    /// commit request covering its tid (durability/escalation), its age
    /// deadline, or its captured batch footprint reaching
    /// [`batch_trigger_credits`](Self::batch_trigger_credits). The "is this a
    /// non-empty transaction a commit must retire" gate is applied once, by the
    /// callers through [`committable_running`](JournalState::committable_running)
    /// — an empty transaction never reaches here (committing it writes no log
    /// block, and waiting on its tid would hang — see
    /// [`log_wait_commit`](Self::log_wait_commit)'s contract).
    fn commit_triggered(&self, txn: &Transaction, now: Jiffies, requested: Option<Tid>) -> bool {
        requested.is_some_and(|r| r.geq(txn.tid()))
            || txn.is_expired_at(now)
            || txn.batch_footprint(self.geometry.tag_layout()) >= self.batch_trigger_credits()
    }

    /// The captured-footprint threshold at which the running transaction is
    /// committed for size: a quarter of [`max_credits`](Self::max_credits),
    /// mirroring jbd2's per-transaction cap `j_max_transaction_buffers =
    /// j_total_len / 4` (`jbd2_journal_get_max_txn_bufs`). Our `max_credits`
    /// bounds one transaction against the WHOLE usable ring — a correctness
    /// bound, not a batching policy — so the quarter point is where jbd2
    /// would have stopped growing the transaction: committing there yields
    /// jbd2-equivalent batch sizes and keeps running + committing +
    /// un-checkpointed tail comfortably inside the ring (the commit-time
    /// fit guard's drain path stays the exception). Measured on captured
    /// work ([`Transaction::batch_footprint`]), not reservations — see that
    /// method for why.
    fn batch_trigger_credits(&self) -> usize {
        (self.max_credits() / 4).max(1)
    }

    /// The free-log-block low-water mark the lazy checkpoint keeps the ring
    /// above (jbd2 `__jbd2_log_wait_for_space`'s target,
    /// `j_max_transaction_buffers`): once the free segment drops below it the
    /// commit thread reclaims the tail forward, keeping ordinary (batch-sized)
    /// commits fitting the free segment without the commit-time drain. A
    /// quarter of the usable ring — the same fraction as
    /// [`batch_trigger_credits`](Self::batch_trigger_credits), so the running +
    /// committing transactions and a quarter of checkpoint slack sit inside the
    /// ring at steady state. Clamped to a `u32` block count for
    /// [`JournalGeometry::free_log_blocks`].
    fn checkpoint_low_water(&self) -> u32 {
        let usable = self.geometry.maxlen() - self.geometry.first();
        (usable / 4).max(1)
    }

    /// The worst-case log footprint of a transaction whose `credits` counts
    /// its METADATA blocks alone (excluding revoke blocks) alongside its
    /// `revoke_blocks` whole revoke blocks:
    ///
    /// ```text
    /// credits data + revoke_blocks + ceil(credits / t) descriptors + 1 commit
    /// ```
    ///
    /// — the free-segment space its commit needs, matching the exact
    /// commit-time footprint ([`try_commit_transaction`](commit::try_commit_transaction):
    /// `revoke + nr_data + descriptors + 1`). `revoke_blocks` is added exactly
    /// ONCE, and the descriptor count charges only the metadata tags — a
    /// revoke block is a whole log block carrying no tag, so a caller must pass
    /// metadata-only `credits` here (see [`reservation_footprint`](Self::reservation_footprint),
    /// the sole non-test caller, which derives `credits` from
    /// [`Transaction::reserved_metadata_blocks`], never the revoke-inclusive
    /// capacity charge). `credits + revoke_blocks ≤ max_credits` (the capacity
    /// gate) makes this `≤ usable` by the `max_credits` formula, so a full
    /// checkpoint always frees enough — the reservation-time wait terminates. A
    /// footprint beyond a `u32` of blocks (unreachable — it is bounded by the
    /// ring) saturates, so the space check treats it as "needs the whole ring".
    pub(super) fn worst_case_footprint(&self, credits: usize, revoke_blocks: usize) -> u32 {
        let tags = self.geometry.tag_layout().tags_per_descriptor();
        let blocks = credits + revoke_blocks + credits.div_ceil(tags) + 1;
        u32::try_from(blocks).unwrap_or(u32::MAX)
    }

    /// The worst-case log footprint of the running `txn` if `credits` more
    /// metadata blocks were reserved — the free-segment space the
    /// reservation-time log-space gate ([`journal_start`](transaction::journal_start))
    /// waits for. Feeds the transaction's shared metadata base
    /// ([`Transaction::reserved_metadata_blocks`]) and its whole revoke blocks
    /// ([`Transaction::nr_revoke_blocks`]) through
    /// [`worst_case_footprint`](Self::worst_case_footprint), so revoke blocks
    /// are charged exactly once and descriptors only for the metadata. This is
    /// the log-space sibling of `check_capacity` (which sums the same metadata
    /// base plus revoke blocks against [`max_credits`](Self::max_credits));
    /// routing both gates through `reserved_metadata_blocks` is what keeps their
    /// footprint arithmetic from diverging.
    pub(super) fn reservation_footprint(&self, txn: &Transaction, credits: usize) -> u32 {
        let revoke_blocks = txn.nr_revoke_blocks(self.geometry.tag_layout());
        let metadata_blocks = txn.reserved_metadata_blocks(credits);
        self.worst_case_footprint(metadata_blocks, revoke_blocks)
    }

    /// Whether a background commit thread is running to service checkpoint /
    /// log-space waits (see [`commit_servicer`](Journal::commit_servicer)).
    pub(super) fn has_commit_servicer(&self) -> bool {
        self.commit_servicer.load(Ordering::Acquire)
    }

    /// Acquires the checkpoint mutex (jbd2 `j_checkpoint_mutex`) — held across
    /// a checkpoint pass and the commit pipeline's clean→dirty superblock
    /// write, so the journal superblock has one writer at a time (see
    /// [`j_checkpoint`](Journal::j_checkpoint)).
    pub(super) fn lock_checkpoint(&self) -> MutexGuard<'_, ()> {
        self.j_checkpoint.lock()
    }

    /// Advances the transaction pipeline by one step under a single
    /// state-lock window: stages the drained locking-seat transaction, or
    /// locks a due running transaction (staging it directly when it has no
    /// open handles — the zero-width `T_LOCKED` fast path; parking it in the
    /// locking seat to drain otherwise). `force_due` treats the running
    /// transaction as due regardless of policy (the ktest force-commit and
    /// nothing else).
    ///
    /// Staging happens in the SAME critical section as the seat take — it
    /// stashes the transaction's after-images into
    /// [`JournalState::uncheckpointed`] and installs the [`CommittingTxn`]
    /// slot ([`JournalState::stage_committing`]) — because the take is the
    /// instant from which the next `journal_start` opens a NEW transaction,
    /// so the images must already be in place for its captures to seed from
    /// (the device lags this commit until its checkpoint). The commit I/O
    /// itself runs without the lock.
    fn advance_pipeline(&self, force_due: bool) -> PipelineStep {
        let mut st = self.state_write();
        if st.committing.is_some() {
            // Commits are serialized on the single committer (the commit
            // thread; the unmount flush runs only after it stopped), so an
            // occupied slot here means "wait for it to retire" — never
            // pipeline a second commit, which would fork the
            // seeding/publication story.
            return PipelineStep::Idle;
        }
        if let Some(locked) = st.locking.take_if(|txn| txn.nr_updates() == 0) {
            return PipelineStep::Staged(st.stage_committing(locked));
        }
        if st.locking.is_some() {
            return PipelineStep::Parked;
        }
        let due = match st.committable_running() {
            Some((txn, _tid)) => {
                force_due || self.commit_triggered(txn, Jiffies::elapsed(), st.commit_request)
            }
            None => false,
        };
        if !due {
            return PipelineStep::Idle;
        }
        let txn = st
            .running
            .take()
            .expect("committable_running implies a running transaction");
        if txn.nr_updates() == 0 {
            return PipelineStep::Staged(st.stage_committing(txn));
        }
        // T_LOCKED with open handles: park to drain. From this instant
        // `journal_start` blocks (the locked barrier) while the
        // transaction's own handles finish and close; the last close wakes
        // the commit thread, whose next advance stages it above.
        st.locking = Some(txn);
        PipelineStep::Parked
    }

    /// Advances the pipeline once and, when that staged a transaction,
    /// commits it. Returns whether a commit was attempted — `false` sends
    /// the commit loop back to its wait instead of into a pointless
    /// checkpoint pass. Runs on the commit thread (and, force-variant, the
    /// ktest committer).
    fn commit_one(&self, force_due: bool) -> bool {
        match self.advance_pipeline(force_due) {
            PipelineStep::Staged(txn) => {
                // The seats moved: wake the locked-barrier / capacity
                // waiters in `journal_start` so a successor can be created
                // NOW, while this commit's I/O runs — the pipeline overlap
                // (jbd2 wakes `j_wait_transaction_locked` at the same
                // point, commit phase 1).
                self.note_credits_released();
                if let Err(e) = self.commit_staged(&txn) {
                    error!("ext4 journal commit failed, aborting the journal: {:?}", e);
                }
                true
            }
            PipelineStep::Parked | PipelineStep::Idle => false,
        }
    }

    /// Commits the staged committing transaction and retires the slot — the
    /// shared tail of both production committers ([`commit_one`](Self::commit_one)
    /// and the unmount flush), after [`advance_pipeline`](Self::advance_pipeline)
    /// (or the flush's own staging) installed `txn` in the slot.
    ///
    /// Single-committer: the caller's thread is the only production
    /// committer, so no commit lock is needed. A successful commit publishes
    /// `committed_tid` (Release) before this returns. On failure the journal
    /// is aborted **before** the slot is cleared — a `sync(2)` sampling the
    /// gap must see the committing transaction or the aborted flag, never a
    /// quiet journal that lost one — and the error is returned.
    fn commit_staged(&self, txn: &Arc<Transaction>) -> Result<Tid> {
        let result = self.commit_or_drain_tail(txn);
        if result.is_err() {
            // The transaction is lost: consumed mid-write, or dropped because
            // the ring could not make room even after draining the tail —
            // either way memory is ahead of the log and the device,
            // unrecoverably. Abort the journal (refuse further work and wake
            // sleepers with an error) rather than continue and publish
            // fragments of the lost transaction through later commits. The
            // full jbd2 abort/errno machinery is P7e.
            self.abort();
        } else {
            // A successful pipeline run ends in `Finished` by construction
            // (step 6's phase advance would have errored otherwise).
            debug_assert!(matches!(
                self.committing_phase(),
                Some(CommitPhase::Finished)
            ));
        }
        self.clear_committing(txn.tid());
        result
    }

    /// The committing slot's current phase, `None` when nothing is staged.
    fn committing_phase(&self) -> Option<CommitPhase> {
        self.state_read()
            .committing
            .as_ref()
            .map(CommittingTxn::phase)
    }

    /// Retires the committing slot and wakes `log_wait_commit` sleepers to
    /// re-check `committed_tid` — the [`Finished`](CommitPhase::Finished) →
    /// gone transition on a successful commit; on the failure paths (the
    /// journal already aborted, or a ktest attempt that refused) it discards
    /// whatever phase the pipeline stopped in.
    fn clear_committing(&self, tid: Tid) {
        {
            let mut st = self.state_write();
            if st
                .committing
                .as_ref()
                .is_some_and(|committing| committing.tid() == tid)
            {
                st.committing = None;
            } else {
                error!("ext4 journal: the committing slot does not hold the retiring txn");
            }
        }
        self.commit_wait_queue.wake_all();
        // Pairs with `poll_commit_action`'s occupied-slot gate: a committer
        // waiting out the slot re-polls now that it is clear. Production-
        // unreachable today (the committer retires its own stage), but the
        // gate must not be able to sleep through the only event that clears
        // it.
        self.commit_trigger.wake_all();
    }

    /// Advances the committing transaction's pipeline phase under a transient
    /// state-lock hold — the commit pipeline calls this at its step
    /// boundaries; an illegal transition (no committing transaction, a tid
    /// mismatch, or a skipped step) errors `EIO`, failing that commit so the
    /// caller aborts the journal, instead of panicking in production. (The
    /// step-6 advance to [`Finished`](CommitPhase::Finished) goes through
    /// [`JournalState::advance_committing_phase`] directly, inside the
    /// publication lock window.)
    pub(super) fn advance_committing_phase(&self, tid: Tid, to: CommitPhase) -> Result<()> {
        self.state_write().advance_committing_phase(tid, to)
    }

    /// Commits `txn` (already staged in the committing slot), draining the
    /// un-checkpointed tail inline (once) when the chain does not fit the
    /// ring's free segment.
    ///
    /// A dirty tail can be in the way because the post-commit checkpoint is
    /// non-fatal on failure: the tail transactions' after-images exist nowhere
    /// but their log blocks until checkpoint (Model A), so overwriting them
    /// would turn the next crash into a silent under-replay of
    /// fsync-acknowledged metadata. jbd2 never reaches this state — writers
    /// block up front on `jbd2_log_space_left` (fs/jbd2/transaction.c:291) —
    /// and P7c-3 builds that space backpressure; until then the commit refuses
    /// to write ([`CommitAttempt::NeedsLogSpace`]), this drains the tail (an
    /// inline [`checkpoint`](checkpoint::checkpoint) — the same thread context
    /// as the post-commit checkpoint, so no new lock interaction) and retries
    /// ONCE. If the checkpoint fails, or the chain still does not fit a clean
    /// ring, the error propagates and the caller aborts the journal: a loud
    /// abort is the only safe fallback left.
    ///
    /// The drain runs MID-COMMIT: `txn`'s revokes are unpublished (its commit
    /// block is not durable — a crash can still erase it), so the checkpoint
    /// must not apply-or-retire an older transaction touching a block `txn`
    /// forgot. The pass reads the committing slot's unpublished revokes out
    /// of the journal state itself (there is no side channel to thread), and
    /// defers in front of such a transaction; a defer here drains less space,
    /// and the retry below then fails loudly (`NeedsLogSpace` → `ENOSPC` →
    /// journal abort) rather than corrupt.
    fn commit_or_drain_tail(&self, txn: &Transaction) -> Result<Tid> {
        match try_commit_transaction(self, self.device.as_ref(), txn)? {
            CommitAttempt::Committed(tid) => return Ok(tid),
            CommitAttempt::NeedsLogSpace => {}
        }
        checkpoint::checkpoint(self, self.device.as_ref())?;
        match try_commit_transaction(self, self.device.as_ref(), txn)? {
            CommitAttempt::Committed(tid) => Ok(tid),
            CommitAttempt::NeedsLogSpace => Err(Error::with_message(
                Errno::ENOSPC,
                "transaction does not fit the journal even after draining the tail",
            )),
        }
    }

    /// Wakes the commit thread to re-evaluate its policy (drain completion,
    /// size, age, re-arm) WITHOUT forcing anything — the bare nudge. A
    /// single wake suffices: the thread re-evaluates in its wait closure.
    /// To force a commit regardless of the size/age triggers, use
    /// [`request_commit_for`](Self::request_commit_for).
    pub(super) fn request_commit(&self) {
        self.commit_trigger.wake_one();
    }

    /// Requests that the transaction bearing `target` (and everything before
    /// it — commits are serial) commit without waiting for the size/age
    /// triggers (jbd2 `jbd2_log_start_commit` setting `j_commit_request`),
    /// then wakes the commit thread. The waiters that need a tid durable —
    /// `log_wait_commit` (fsync/O_SYNC/sync(2)) and the capacity escalation
    /// — funnel through here: a running transaction holding `target` is
    /// force-locked and drained by the committer rather than aged out.
    pub(super) fn request_commit_for(&self, target: Tid) {
        {
            let mut st = self.state_write();
            if !st
                .commit_request
                .is_some_and(|existing| existing.geq(target))
            {
                st.commit_request = Some(target);
            }
        }
        self.request_commit();
    }

    /// Requests a commit of whatever the running transaction currently is
    /// (if it has captured work) — the escalation for waiters that need "the
    /// pins/space held by uncommitted transactions" released but hold no
    /// tid to name: the allocator's pinned-block `ENOSPC` retry path (Linux
    /// equivalently forces a commit via `ext4_should_retry_alloc` →
    /// `jbd2_journal_force_commit_nested`). If the blocking transaction is
    /// already in the locking/committing seats, the wake alone suffices —
    /// it is already on its way to durability.
    pub(in crate::fs::fs_impls::ext4) fn request_commit_of_running(&self) {
        {
            let mut st = self.state_write();
            // Extract the tid before mutating `commit_request`: the
            // committable verdict borrows the running transaction, so it must
            // be reduced to an owned `Tid` first.
            let committable = st.committable_running().map(|(_, tid)| tid);
            if let Some(tid) = committable
                && !st.commit_request.is_some_and(|existing| existing.geq(tid))
            {
                st.commit_request = Some(tid);
            }
        }
        self.request_commit();
    }

    /// Returns the current credit-release epoch (see
    /// [`credit_release_epoch`](Journal::credit_release_epoch)). Snapshot it
    /// under the state lock that just observed "transaction full": any release
    /// after that observation bumps the epoch, so a waiter comparing against
    /// the snapshot cannot miss the wakeup.
    pub(super) fn credit_release_epoch(&self) -> u64 {
        self.credit_release_epoch.load(Ordering::Acquire)
    }

    /// Records that admission conditions may have improved — a handle
    /// released its credit reservation, or the pipeline's seats moved
    /// (staging emptied the locking seat, lifting `journal_start`'s locked
    /// barrier) — and wakes blocked `journal_start` sleepers to re-check.
    /// One epoch serves both waits: they share the retry loop and the
    /// "cannot miss a wake" snapshot discipline.
    pub(super) fn note_credits_released(&self) {
        self.credit_release_epoch.fetch_add(1, Ordering::Release);
        self.commit_wait_queue.wake_all();
    }

    /// Registers the current task as holding an operation handle on this
    /// journal, or `Err(EDEADLK)` if it already holds one (the nested-`begin_op`
    /// self-deadlock; see [`OpHandle::start`]). `Ok(Some(id))` carries the
    /// identity to hand back to [`release_op_handle`](Journal::release_op_handle)
    /// at close; `Ok(None)` outside task context, where the guard is inert.
    fn claim_op_handle(&self) -> Result<Option<usize>> {
        let Some(id) = current_op_owner() else {
            return Ok(None);
        };
        if !self.op_handle_owners.lock().insert(id) {
            return_errno_with_message!(
                Errno::EDEADLK,
                "nested ext4 metadata operation on one task would deadlock the journal; \
                 open the second (e.g. reclaim) handle only after the first has closed"
            );
        }
        Ok(Some(id))
    }

    /// Releases a task's operation-handle registration (see
    /// [`claim_op_handle`](Journal::claim_op_handle)).
    fn release_op_handle(&self, id: usize) {
        self.op_handle_owners.lock().remove(&id);
    }

    /// Blocks until the admission pressure that kept a `journal_start` out of
    /// transaction `tid` may have eased: `tid` committed, the epoch moved
    /// past `epoch` (a handle released credits, or the pipeline's seats
    /// moved — the locked barrier's lift), or the journal aborted (error).
    /// The caller re-checks admission and retries — the sleeping half of
    /// jbd2 `add_transaction_credits`' wait loop, covering both its stalls
    /// (full transaction, and `t_state != T_RUNNING` →
    /// `wait_transaction_locked`). Entering the wait escalates: the
    /// blocking transaction is force-requested for commit
    /// ([`request_commit_for`](Self::request_commit_for)) so a full or
    /// locked transaction drains and commits rather than aging out under a
    /// waiter (jbd2's `wait_transaction_locked` similarly calls
    /// `jbd2_log_start_commit` on the way in).
    ///
    /// # Locking
    ///
    /// Callers hold no journal lock (the state lock is dropped before waiting)
    /// but typically **do** hold inode `inner` locks — that is safe because the
    /// wait only needs other handles to close or the commit thread to run, and
    /// neither takes `inner`: operations acquire all their inode locks *before*
    /// `journal_start` (lock order `inner` ① → handle ②), and the commit
    /// thread's ordered flush works on page-cache handles cloned into the
    /// transaction at registration time, touching no inode lock at all (see
    /// `Transaction::register_ordered_data`).
    ///
    /// Under the running/locking/committing pipeline this remains iron law
    /// 1's legal exception (waiting for *space*, like Linux): the wait can
    /// only be on the RUNNING transaction's fullness or the LOCKING seat's
    /// drain — never on the committing slot, which took none of the
    /// waiter's reservations with it — and everything that frees the space
    /// progresses without the waiter's locks: handle closes bump the epoch
    /// from their own threads (the module docs' group-commit section audits
    /// why a draining handle can never need a parked waiter's locks), and
    /// staging/commit/checkpoint run on the committer thread, which takes
    /// brief state-lock windows and device I/O only.
    pub(super) fn wait_for_transaction_room(&self, tid: Tid, epoch: u64) -> Result<()> {
        self.request_commit_for(tid);
        self.commit_wait_queue.wait_until(|| {
            if self.is_aborted() {
                // The journal died while we waited; surface it rather than
                // retrying against a journal that accepts no work.
                return Some(Err(Error::with_message(
                    Errno::EIO,
                    "journal aborted while waiting for transaction room",
                )));
            }
            if self.committed_tid().geq(tid) || self.credit_release_epoch() != epoch {
                return Some(Ok(()));
            }
            None
        })
    }

    /// Blocks until the commit thread has freed log space by checkpointing the
    /// tail forward (the reservation-time half of jbd2
    /// `__jbd2_log_wait_for_space`, called from `start_this_handle` before a
    /// handle is granted): `journal_start` found a reservation that does not
    /// fit the ring's free segment, so it waits here for room rather than
    /// letting the transaction build and overflow the log at commit time.
    ///
    /// Entering escalates on both fronts the space can be held: it sets
    /// [`space_pressure`](Journal::space_pressure) so the committer runs a
    /// FULL reclaim pass, and force-requests the running transaction's commit
    /// ([`request_commit_of_running`](Self::request_commit_of_running)) so
    /// space held by an uncommitted transaction — or the tail's reclaim
    /// blocked behind that transaction's unpublished revoke — is released
    /// (jbd2's `__jbd2_log_wait_for_space` likewise commits when checkpoint
    /// alone cannot free enough). The wait ends when a checkpoint pass (or a
    /// handle close, or a staging) bumps the epoch, and the caller re-checks
    /// the free segment and retries.
    ///
    /// # Termination
    ///
    /// The `max_credits` capacity gate already passed, so the reserved credits
    /// `n + v ≤ max_credits`, and the footprint the caller compared against the
    /// free segment ([`reservation_footprint`](Self::reservation_footprint) =
    /// `n + v + ceil(n/t) + 1`, revoke counted once) is therefore `≤ usable` by
    /// the `max_credits` formula; a full checkpoint frees the whole committed
    /// tail, so the retry eventually fits — unless the space is held by an
    /// uncommitted transaction, which the force-request commits.
    ///
    /// # Locking
    ///
    /// Same contract as [`wait_for_transaction_room`](Self::wait_for_transaction_room):
    /// callers may hold inode `inner` locks (lock order ① → ②), never a
    /// journal lock; the wait is for *space*, iron law 1's legal exception,
    /// and everything that frees it — the commit thread's checkpoint/commit —
    /// takes no inode lock. Returns `Ok(())` immediately when no commit thread
    /// is running to service it ([`has_commit_servicer`](Self::has_commit_servicer)):
    /// a no-thread test fixture or teardown proceeds and the commit-time fit
    /// guard drains inline, so this can never hang on an absent committer.
    pub(super) fn wait_for_log_space(&self, epoch: u64) -> Result<()> {
        if !self.has_commit_servicer() {
            return Ok(());
        }
        // Reservation-time backpressure genuinely fired: a servicer exists and
        // this handle is about to park for it (as opposed to the commit-time
        // inline-drain backstop). Counted once here, before the wait.
        self.log_space_waits.fetch_add(1, Ordering::Relaxed);
        self.space_pressure.store(true, Ordering::Release);
        self.request_commit_of_running();
        self.commit_wait_queue.wait_until(|| {
            if self.is_aborted() {
                return Some(Err(Error::with_message(
                    Errno::EIO,
                    "journal aborted while waiting for log space",
                )));
            }
            // The servicer withdrew (teardown): stop waiting and let the
            // commit-time fit guard handle the reservation inline.
            if !self.has_commit_servicer() {
                return Some(Ok(()));
            }
            if self.credit_release_epoch() != epoch {
                return Some(Ok(()));
            }
            None
        })
    }

    /// The number of times a `journal_start` parked on the log-space gate
    /// ([`log_space_waits`](Journal::log_space_waits)) — a nonzero value proves
    /// reservation-time backpressure fired rather than the commit-time drain.
    #[cfg(ktest)]
    pub(super) fn log_space_wait_count(&self) -> u64 {
        self.log_space_waits.load(Ordering::Relaxed)
    }

    /// Returns whether the journal has been aborted by a failed commit (see the
    /// [`aborted`](Journal::aborted) field).
    pub(in crate::fs::fs_impls::ext4) fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::Acquire)
    }

    /// Returns whether [`stop_commit_thread`](Journal::stop_commit_thread) has
    /// been requested — the unmount quiesce point. Once true (and after
    /// `flush_on_unmount` empties the log), direct metadata writes are safe
    /// again: nothing is uncommitted, so there is no WAL left to invert.
    pub(in crate::fs::fs_impls::ext4) fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// Aborts the journal after a failed commit: further `journal_start`s are
    /// refused with `EIO`, and `log_wait_commit` sleepers are woken to fail
    /// instead of waiting forever on a tid that will never commit.
    fn abort(&self) {
        self.aborted.store(true, Ordering::Release);
        self.commit_wait_queue.wake_all();
    }

    /// Aborts the journal on `EXT4_IOC_SHUTDOWN` (jbd2_journal_abort from
    /// `ext4_force_shutdown`): the running transaction is never committed and
    /// the log is left as-is — with `RECOVER` still stamped, the next mount
    /// replays exactly what had committed before the shutdown, which is the
    /// "crash here" semantics the ioctl exists to simulate.
    pub(in crate::fs::fs_impls::ext4) fn abort_for_shutdown(&self) {
        self.abort();
    }

    /// Aborts the journal on a filesystem-detected inconsistency (the
    /// minimal Linux `ext4_error` → `jbd2_journal_abort` shape), reached
    /// through [`Handle::abort_journal_on_fs_error`] by a site whose
    /// irreversible journal effects (a discharged revoke duty) cannot be
    /// unwound after the operation they served has failed.
    pub(super) fn abort_for_fs_error(&self) {
        self.abort();
    }

    /// Blocks until transaction `target` (and thus everything up to it) is
    /// committed to the log — the primitive `fsync`/`fdatasync` use (jbd2
    /// `jbd2_log_wait_commit`).
    ///
    /// Returns immediately if `target` is already committed. Otherwise it
    /// force-requests the commit ([`request_commit_for`](Self::request_commit_for)
    /// — the durability trigger, jbd2 `jbd2_log_start_commit` +
    /// `jbd2_log_wait_commit`) and sleeps on
    /// [`commit_wait_queue`](Journal::commit_wait_queue) until the commit
    /// thread advances `committed_tid` past `target`. The request + condition
    /// re-check pairing is what stops it hanging: the wake sets
    /// `committed_tid` *before* `wake_all`, and the `wait_until` closure
    /// re-reads it on every wake (`Acquire`, pairing with the commit's
    /// `Release`).
    ///
    /// Under group commit the target's transaction may still have OTHER
    /// operations' handles open: the request force-locks it (the committer
    /// parks it in [`JournalState::locking`] and stages it when the last
    /// handle closes), so the wait spans that drain — bounded by in-flight
    /// operations, never by the age interval. The P4 "caller's own handle
    /// closed ⟹ transaction committable" assumption is thereby retired.
    ///
    /// # Locking
    ///
    /// MUST be called with **no filesystem locks held** — it sleeps on the commit
    /// thread, which needs those same locks. The caller records `target` (the tid
    /// its own now-closed handle joined), releases every inode/journal lock, then
    /// waits.
    ///
    /// The caller must additionally have contributed (or observed) captured
    /// metadata for `target`'s transaction: a transaction that never captures a
    /// block is not committable, so waiting on its tid would sleep forever.
    /// `fsync` guards this by only waiting when the inode writeback actually
    /// wrote (see `Inode::sync_data_and_meta`).
    pub(in crate::fs::fs_impls::ext4) fn log_wait_commit(&self, target: Tid) -> Result<()> {
        if self.committed_tid().geq(target) {
            return Ok(());
        }
        self.request_commit_for(target);
        self.commit_wait_queue.wait_until(|| {
            if self.committed_tid().geq(target) {
                return Some(Ok(()));
            }
            if self.is_aborted() {
                // The commit that would have carried `target` failed and the
                // journal is aborted: fail instead of sleeping forever.
                return Some(Err(Error::with_message(
                    Errno::EIO,
                    "journal aborted; the transaction will never commit",
                )));
            }
            None
        })
    }

    /// Commits whatever the running transaction has captured and waits for it
    /// — the `sync(2)` durability point (jbd2's
    /// `jbd2_journal_force_commit`-lite). A no-op when nothing is captured
    /// (an empty transaction is not committable, so waiting on its tid would
    /// sleep forever — see [`log_wait_commit`](Self::log_wait_commit)).
    ///
    /// # Locking
    ///
    /// Same contract as [`log_wait_commit`](Self::log_wait_commit): the caller
    /// must hold no filesystem locks.
    pub(in crate::fs::fs_impls::ext4) fn commit_and_wait_running(&self) -> Result<()> {
        match self.sync_durability_target()? {
            Some(target) => self.log_wait_commit(target),
            None => Ok(()),
        }
    }

    /// The tid [`commit_and_wait_running`](Self::commit_and_wait_running) must
    /// wait on to claim durability, decided in one state-lock window — split
    /// out so the probe's target selection is testable deterministically,
    /// separate from the wait itself.
    fn sync_durability_target(&self) -> Result<Option<Tid>> {
        let st = self.state_read();
        // Newest-first across the three seats: the running transaction's
        // captures, else a force-locked (draining) transaction's, else the
        // mid-commit slot's — a transaction is not durable in ANY seat, so
        // waiting on the newest one with work covers them all (commits are
        // serial, and a running transaction can only exist once the locking
        // seat staged).
        if let Some(txn) = st
            .running
            .as_ref()
            .or(st.locking.as_ref())
            .filter(|txn| txn.nr_metadata_blocks() > 0)
        {
            return Ok(Some(txn.tid()));
        }
        // Nothing captured in the seats — but the transaction to make
        // durable may have just been TAKEN by the commit thread and be
        // mid-commit (its commit record not on disk yet). Waiting on
        // nothing here would let sync(2) return early.
        match st.committing.as_ref().map(CommittingTxn::tid) {
            Some(tid) => Ok(Some(tid)),
            // Nothing running, draining, or committing: everything captured
            // is durable — unless a failed commit aborted the journal and
            // dropped a transaction, in which case durability must not be
            // claimed (else sync(2) returns Ok for data the abort lost).
            None => {
                if self.is_aborted() {
                    return_errno_with_message!(Errno::EIO, "journal aborted");
                }
                Ok(None)
            }
        }
    }

    /// Returns the number of ordered-data entries registered with the running
    /// transaction. Test-only inspection for the write-path registration
    /// wiring; call with the commit thread stopped, or the transaction may be
    /// consumed between the operation and the assertion.
    #[cfg(ktest)]
    pub(in crate::fs::fs_impls::ext4) fn running_nr_ordered_data_for_test(&self) -> usize {
        self.state_read()
            .running
            .as_ref()
            .map_or(0, |txn| txn.nr_ordered_data())
    }

    /// Stops and joins the commit thread (jbd2 journal teardown).
    ///
    /// Sets [`stop`](Journal::stop), wakes the thread out of its `wait_until`, and
    /// joins it. Idempotent: after the handle is taken, a second call is a no-op.
    ///
    /// # Must not self-join
    ///
    /// MUST be called by the journal's owner (`Ext4::drop`, in the integration
    /// task) on a thread **other than the commit thread** — never by the commit
    /// thread itself, or [`join`](crate::thread::Thread::join) would spin forever
    /// waiting on itself. This is safe because the commit thread holds only a
    /// `Weak<Journal>` and so can never be the last strong-ref holder that would
    /// trigger this from `Drop`: the strong count reaches `0` only once all real
    /// owners (`Ext4`) have dropped, and an owner calls `stop_commit_thread`
    /// explicitly at that point.
    ///
    /// There is deliberately **no `join()` in a `Drop` impl** — a Drop-time join
    /// could, in principle, run on the commit thread and self-deadlock.
    pub(in crate::fs::fs_impls::ext4) fn stop_commit_thread(&self) {
        // Withdraw the servicer FIRST: a `journal_start` racing teardown must
        // not begin a log-space wait no committer will service (it proceeds
        // and the commit-time fit guard drains inline instead).
        self.commit_servicer.store(false, Ordering::Release);
        self.stop.store(true, Ordering::Release);
        self.commit_trigger.wake_all();
        // Wake any `journal_start` already parked in a log-space wait so it
        // stops waiting on a servicer that is going away (it re-checks
        // `commit_servicer` and proceeds).
        self.commit_wait_queue.wake_all();
        // Take the handle out (so a second call is a no-op) and join outside the
        // lock — `join` blocks, and holding `commit_thread` across it is pointless
        // and would serialize any concurrent stopper on a sleeping join.
        let thread = self.commit_thread.lock().take();
        if let Some(thread) = thread {
            thread.join();
        }
    }

    /// Flushes the journal at unmount so the on-disk log is left clean.
    ///
    /// MUST be called by the owner (`Ext4::drop`) **after**
    /// [`stop_commit_thread`](Journal::stop_commit_thread): with the commit thread
    /// gone, this thread is the sole committer, so it commits the final running
    /// transaction and checkpoints synchronously without racing the background
    /// committer. It commits any running transaction that captured metadata, then
    /// checkpoints every committed transaction to its final location and clears
    /// `s_start` — leaving the on-disk journal clean (`s_start == 0`) so the next
    /// mount sees an empty log and skips recovery.
    ///
    /// A no-op on a journal that never ran a transaction (nothing captured,
    /// already-clean tail): the take yields `None` and [`checkpoint`] returns early.
    ///
    /// # Aborted journal
    ///
    /// On an aborted journal this refuses at **entry** with `EIO`, dropping
    /// the running transaction unstaged and writing nothing: the abort lost
    /// a transaction (see [`aborted`](Journal::aborted)), so committing the
    /// running successor here would publish a recoverable transaction built
    /// on the lost one's absent effects — exactly the
    /// fragments-of-lost-transaction leak the abort discipline forbids. The
    /// on-disk log is left as the abort left it, and that is safe: `Ext4::drop`
    /// keeps `RECOVER` stamped on error, so the next mount replays the log —
    /// which holds only transactions *committed before* the abort (the lost
    /// and running ones never got a commit block, so recovery cannot see
    /// them). This is the same terminal state as a commit failing *inside*
    /// this flush (`commit_staged` aborts and its error propagates before
    /// the checkpoint below); entry gating just refuses one step earlier.
    pub(in crate::fs::fs_impls::ext4) fn flush_on_unmount(&self) -> Result<()> {
        if self.is_aborted() {
            let mut st = self.state_write();
            st.running = None;
            st.locking = None;
            return_errno_with_message!(Errno::EIO, "journal aborted; leaving the log for recovery");
        }
        // Up to TWO transactions can be left over under group commit: a
        // force-locked one the stopped committer never staged (its drain
        // completed after — or its trigger fired right before — the join),
        // and the running successor... except the locked barrier means a
        // successor only exists once the locking seat staged, so in
        // practice one seat is occupied; the loop handles both uniformly,
        // oldest first (the locking seat's transaction predates any
        // running one, and log order must be tid order).
        loop {
            let staged = {
                let mut st = self.state_write();
                // `stop_commit_thread` joined the sole other committer, so a
                // still-occupied slot is a broken contract; staging over it
                // would silently overwrite a mid-flight `CommittingTxn`.
                // Refuse like the other stagers do — the log stays dirty and
                // replay-safe for the next mount.
                if st.committing.is_some() {
                    error!(
                        "ext4 journal: the committing slot is occupied at unmount; not flushing"
                    );
                    return_errno_with_message!(Errno::EIO, "committing slot occupied at unmount");
                }
                // At unmount no operation is in flight, so every handle has
                // closed; an undrained seat is a broken contract (a leaked
                // OpHandle), and staging it would violate the freeze
                // invariant — leave the log dirty and replay-safe instead.
                if st
                    .locking
                    .as_ref()
                    .or(st.running.as_ref())
                    .is_some_and(|txn| txn.nr_updates() != 0)
                {
                    error!("ext4 journal: open journal handles at unmount; not flushing");
                    return_errno_with_message!(Errno::EIO, "open journal handles at unmount");
                }
                match st.locking.take().or_else(|| st.running.take()) {
                    // Mirror the commit thread: stage (which retains the
                    // images atomically with the take — nothing races at
                    // unmount, but the invariant is cheap and uniform: every
                    // transaction entering the pipeline is slot-resident
                    // with its images stashed).
                    Some(txn) if txn.nr_metadata_blocks() > 0 => Some(st.stage_committing(txn)),
                    // A captureless leftover is not committable; drop it and
                    // look at the next seat.
                    Some(_) => None,
                    // Both seats empty: flushing is done.
                    None => break,
                }
            };
            if let Some(txn) = staged {
                // `commit_staged` aborts the journal and retires the slot on
                // failure; the transaction is lost, so do not checkpoint
                // device state that no longer matches the log.
                self.commit_staged(&txn)?;
            }
        }
        // Nothing is running, draining (both seats emptied above), or
        // mid-commit (the slot was retired by `commit_staged`), so the
        // pass's snapshot finds no unpublished revokes.
        checkpoint::checkpoint(self, self.device.as_ref())
    }
}

/// What the commit thread should do after a wake (the decision made by
/// [`Journal::poll_commit_action`]).
//
// No dead-code marker: `Ext4::open` starts the commit thread on a journaled
// mount, so the whole commit-thread cluster (and this enum) is live in every
// build.
enum CommitAction {
    /// Teardown requested (or the journal is gone): leave the loop.
    Exit,
    /// The pipeline has work: a drained locked transaction to stage, or a
    /// due running transaction to lock/commit.
    Advance,
    /// The transaction the age deadline was armed for changed: recompute
    /// the sleep's timeout and wait again.
    Rearm,
}

/// One [`Journal::advance_pipeline`] outcome.
enum PipelineStep {
    /// A transaction was staged into the committing slot; commit it.
    Staged(Arc<Transaction>),
    /// A locked transaction is draining its open handles; nothing to do
    /// until its last `journal_stop` wakes the committer.
    Parked,
    /// Nothing due (or a mid-flight commit occupies the slot).
    Idle,
}

/// The owner is responsible for [`stop_commit_thread`](Journal::stop_commit_thread);
/// this only asserts it was honored, catching a forgotten teardown in debug
/// builds. It performs **no** `join` — see `stop_commit_thread` for why a
/// join-in-`Drop` is forbidden.
impl Drop for Journal {
    fn drop(&mut self) {
        debug_assert!(
            self.commit_thread.lock().is_none(),
            "Journal dropped without stop_commit_thread; the commit thread may outlive it"
        );
    }
}

/// Looks up the capture-accepting transaction bearing `tid` — in the running
/// seat, or in the locking seat while it drains (a force-locked
/// transaction's OWN handles keep patching it there; only NEW handles are
/// barred, see [`JournalState::locking`]).
///
/// Takes the two seats rather than the whole [`JournalState`] so a caller
/// can keep disjoint borrows of the state's other fields (the seed lookup in
/// [`get_write_access`] reads `uncheckpointed` alongside the returned
/// transaction; `journal_stop` mutates only the found seat).
fn active_txn_mut<'a>(
    running: &'a mut Option<Transaction>,
    locking: &'a mut Option<Transaction>,
    tid: Tid,
) -> Option<&'a mut Transaction> {
    match running.as_mut() {
        Some(txn) if txn.tid() == tid => Some(txn),
        _ => match locking.as_mut() {
            Some(txn) if txn.tid() == tid => Some(txn),
            _ => None,
        },
    }
}

/// [`active_txn_mut`] for a handle, erroring on a miss: a miss means the
/// handle outlived its transaction — impossible while the handle holds an
/// open update (the transaction cannot stage until its last handle closes),
/// but checked so a stale patch can never land on the wrong transaction's
/// after-image.
fn verify_active<'a>(
    running: &'a mut Option<Transaction>,
    locking: &'a mut Option<Transaction>,
    handle: &Handle,
) -> Result<&'a mut Transaction> {
    active_txn_mut(running, locking, handle.tid())
        .ok_or_else(|| Error::with_message(Errno::EIO, "journal handle outlived its transaction"))
}

/// [`verify_active`] over the whole state, for the funnels that need no other
/// state field.
fn active_for<'a>(state: &'a mut JournalState, handle: &Handle) -> Result<&'a mut Transaction> {
    let JournalState {
        running, locking, ..
    } = state;
    verify_active(running, locking, handle)
}

/// Seeds an existing metadata block's after-image and mints the
/// [`WriteAccess`] credential whose [`patch`](WriteAccess::patch) calls
/// accumulate onto its newest committed content (jbd2 `get_write_access`).
///
/// The seed order is newest-wins across the pipeline's stations: the
/// force-locked (draining) predecessor's capture when one exists — its
/// after-images are not in the retained map yet (they are still receiving
/// its own handles' patches until staging); unreachable while
/// `journal_start`'s locked barrier holds, since no successor can run
/// alongside the seat, but the funnel consults the seat so the order is
/// correct by construction, not by that gate — then the block's retained
/// post-`running` image ([`JournalState::uncheckpointed`] — which holds the
/// mid-flight COMMITTING transaction's images from the instant it was
/// staged, alongside the committed-but-un-checkpointed ones; see that
/// field's docs for the B-1 invariant restated under the pipeline), and the
/// device content otherwise: between a transaction's staging and its
/// checkpoint the device lags, and a device seed taken in that window would
/// hand this transaction stale bytes for every neighbor object it does not
/// patch itself (the B-1 clobber — see
/// [`UncheckpointedImage`](transaction::UncheckpointedImage)). The whole
/// decision — running-capture reuse vs locked capture vs retained image vs
/// device read — is made under ONE `state_write` window, atomic against a
/// concurrent staging.
///
/// A handle of the LOCKED transaction itself seeds from the map/device only
/// (its own captures are reused idempotently inside `capture_write`): the
/// running successor — if one could exist — is NEWER, and seeding an older
/// transaction from a newer one's uncommitted capture would be the
/// isolation break the locked barrier exists to prevent.
///
/// Without a handle (a non-journaled volume, or a caller that opened no
/// transaction) the returned credential is inert: nothing is captured and
/// writeback stays driven by the block's own `Dirty` flag, exactly as in
/// Phases 1–3.
pub(super) fn get_write_access<'h>(
    handle: Option<&'h Handle>,
    blocknr: Ext4Bid,
) -> Result<WriteAccess<'h>> {
    let Some(handle) = handle else {
        return Ok(WriteAccess { live: None });
    };
    let journal = handle.journal()?;
    let device = journal.device.clone();
    let mut state = journal.state_write();
    // Disjoint field borrows: the handle's transaction (mutated by the
    // capture), the other seat (read as a seed station), and the
    // retained-image map (read for the seed).
    let JournalState {
        running,
        locking,
        uncheckpointed,
        ..
    } = &mut *state;
    let (txn, locked_predecessor) = match running.as_mut() {
        // A running-transaction handle: the locking seat, if occupied,
        // holds the newest not-yet-retained images (see the docs above).
        Some(txn) if txn.tid() == handle.tid() => (txn, locking.as_ref()),
        // A locked transaction's own handle: no newer station to seed from.
        _ => match locking.as_mut() {
            Some(txn) if txn.tid() == handle.tid() => (txn, None),
            _ => {
                return_errno_with_message!(Errno::EIO, "journal handle outlived its transaction")
            }
        },
    };
    let seed = locked_predecessor
        .and_then(|locked| locked.buffer_bytes(blocknr))
        .or_else(|| {
            uncheckpointed
                .get(&blocknr)
                .map(transaction::UncheckpointedImage::image_bytes)
        });
    let generation = txn.capture_write(blocknr, seed, device.as_ref())?;
    Ok(WriteAccess {
        live: Some(LiveAccess {
            handle,
            bid: blocknr,
            generation,
        }),
    })
}

/// Seeds a freshly allocated metadata block's after-image as zeroes — its prior
/// device content is meaningless, so no read is issued (jbd2
/// `get_create_access`) — and mints the block's [`WriteAccess`]. Inert without
/// a handle (see [`get_write_access`]).
pub(super) fn get_create_access<'h>(
    handle: Option<&'h Handle>,
    blocknr: Ext4Bid,
) -> Result<WriteAccess<'h>> {
    let Some(handle) = handle else {
        return Ok(WriteAccess { live: None });
    };
    let journal = handle.journal()?;
    let mut state = journal.state_write();
    let generation = active_for(&mut state, handle)?.capture_create(blocknr);
    Ok(WriteAccess {
        live: Some(LiveAccess {
            handle,
            bid: blocknr,
            generation,
        }),
    })
}

/// Pins a just-freed block run out of the allocator until the freeing
/// transaction commits ([`JournalState::pinned_frees`] holds the run;
/// [`JournalState::release_pinned_frees`] releases it at commit step 6).
///
/// Called by `Ext4::free_blocks` for each contiguous per-group run it clears,
/// right after that run's bitmap clear, under the same superblock write lock
/// — which is what makes "clear the bits, then pin" atomic against the
/// allocator (it needs that lock too). Applies to **both** [`BlockFreeAuth`]
/// flavors: a revoke-duty free's old *journaled* image survives in the
/// un-retired log, and even a plain-data free is not durable until the
/// commit — reusing either early hands the block to a new owner whose bytes
/// a crash (or the checkpoint replay) then crosses with the old life. Linux
/// pins exactly this set in ordered mode (`ext4_mb_clear_bb`,
/// fs/ext4/mballoc.c:6536-6560: metadata always, data whenever the inode is
/// not writeback). Inert without a handle: a non-journaled volume has no
/// commit boundary for the pin to wait on, so its frees stay immediately
/// reusable.
///
/// A run pinned twice (a double free — already a warned bug path at the
/// bitmap) keeps the larger extent and the newer tid: over-pinning is merely
/// conservative, under-pinning would re-open the reuse window.
pub(super) fn pin_freed_run(handle: Option<&Handle>, start: Ext4Bid, count: u32) -> Result<()> {
    let Some(handle) = handle else {
        return Ok(());
    };
    if count == 0 {
        return Ok(());
    }
    let journal = handle.journal()?;
    let mut state = journal.state_write();
    let tid = active_for(&mut state, handle)?.tid();
    if let Some(run) = state.pinned_frees.get_mut(&start) {
        run.count = run.count.max(count);
        run.tid = tid;
    } else {
        state.pinned_frees.insert(start, PinnedRun { count, tid });
    }
    Ok(())
}

/// A capture credential for one metadata block — jbd2's "write access" made a
/// value: proof that [`get_write_access`] / [`get_create_access`] captured
/// this block's after-image into the running transaction. [`patch`](Self::patch)
/// (jbd2 `dirty_metadata`) is the only way to modify a captured image, so
/// patch-without-capture is unrepresentable, and the block number travels
/// inside the credential — a wrong-bid patch landing on a neighbor's capture
/// in the shared running transaction can no longer be written. The credential
/// carries no block-kind tag: journal checksums are keyed by the
/// journal-UUID seed plus commit tid, and revoke records by the free-side
/// [`DataForgetPolicy`] / [`forget`] protocol.
///
/// On a non-journaled volume — or from a caller with no open transaction —
/// the credential is **inert**: `patch` succeeds without invoking the closure
/// (writeback stays on the block's own `Dirty` flag, the Phase 1–3 semantics
/// verbatim), and [`is_live`](Self::is_live) lets a writer keep its
/// direct-write fallback in one place.
///
/// The credential holds only `&'h Handle`, never a journal state guard: the
/// block-group methods keep two credentials live at once, and pinning the
/// state lock across a capture's device I/O is forbidden — `patch` re-takes
/// the state lock transiently, exactly like the old free `dirty_metadata`.
/// Because it immutably borrows the `Handle`, borrowck statically drains all
/// live credentials before P7's `journal_restart(&mut Handle)` can run — the
/// stale-capture-across-restart hazard, checked at compile time.
#[must_use = "a capture without a patch (or is_live check) is almost always a bug"]
pub(super) struct WriteAccess<'h> {
    live: Option<LiveAccess<'h>>,
}

/// The live half of a [`WriteAccess`]: which block of which transaction, and
/// which *capture* of that block — the mint generation distinguishes a
/// credential of a capture that was since cancelled (forget) and re-created
/// (the block reallocated within the same transaction) from the live one, so
/// a stale credential's patch fails `EIO` instead of writing the old owner's
/// bytes into the new owner's image (see
/// [`CaptureGeneration`](transaction::CaptureGeneration)).
struct LiveAccess<'h> {
    handle: &'h Handle,
    bid: Ext4Bid,
    generation: transaction::CaptureGeneration,
}

impl WriteAccess<'_> {
    /// Returns whether this credential carries a real capture (a journaled
    /// volume with an open transaction). The writer's direct-write fallback keys off this
    /// instead of re-deriving "journaled?" from the handle.
    pub(super) fn is_live(&self) -> bool {
        self.live.is_some()
    }

    /// Patches the captured after-image via `patch`: writes this site's
    /// modification into the seeded block buffer, so sub-objects sharing one
    /// block accumulate onto the same image (see [`Transaction::apply_patch`]).
    /// Inert credential: succeeds **without** invoking `patch`.
    pub(super) fn patch(&self, patch: impl FnOnce(&mut [u8])) -> Result<()> {
        let Some(live) = &self.live else {
            return Ok(());
        };
        let journal = live.handle.journal()?;
        let mut state = journal.state_write();
        active_for(&mut state, live.handle)?.apply_patch(live.bid, live.generation, patch)
    }
}

/// Reads a metadata block through the journal's retained after-images: the
/// running transaction's capture when one exists, else the newest
/// post-`running` image (a mid-flight committing transaction's — stashed at
/// staging — or a committed-but-un-checkpointed one), else the device.
///
/// This is the **read side** of the WAL suppression. A captured block's newest
/// bytes live in the journal's buffers and the device lags them until
/// checkpoint, so every reader of a metadata block that the funnels capture
/// (extent-tree nodes today; bitmaps/descriptors/superblock have in-memory
/// owners and directory blocks live in the page cache) must read through here
/// — a bare device read inside that window returns stale bytes, the read-side
/// counterpart of the B-1 stale-seed hazard that [`get_write_access`] plugs on
/// the capture side. jbd2 gets this for free from the kernel buffer cache (one
/// canonical `buffer_head` per block); Model A retains the equivalents itself.
///
/// `journal` is `None` on a non-journaled volume — and during mount, before
/// the journal is published, when replay has already made the device
/// authoritative — leaving those reads plain device reads (Phases 1–3
/// unchanged).
pub(super) fn read_metadata_block(
    journal: Option<&Journal>,
    device: &dyn BlockDevice,
    blocknr: Ext4Bid,
) -> Result<[u8; BLOCK_SIZE]> {
    if let Some(journal) = journal {
        let state = journal.state_read();
        // Newest-wins across the stations: a running capture is newer than a
        // locked (draining) predecessor's, which is newer than any retained
        // image (a capture seeds *from* the older stations, then accumulates
        // patches). The locking-seat station is load-bearing on the READ
        // side even under the locked barrier: readers take no handle, so
        // they run concurrently with a drain, and a drained-transaction
        // capture's bytes exist nowhere else until staging stashes them.
        let newest = state
            .running
            .as_ref()
            .and_then(|txn| txn.buffer_bytes(blocknr))
            .or_else(|| {
                state
                    .locking
                    .as_ref()
                    .and_then(|txn| txn.buffer_bytes(blocknr))
            })
            .or_else(|| {
                state
                    .uncheckpointed
                    .get(&blocknr)
                    .map(transaction::UncheckpointedImage::image_bytes)
            });
        if let Some(bytes) = newest {
            let mut block = [0u8; BLOCK_SIZE];
            block.copy_from_slice(bytes);
            return Ok(block);
        }
    }
    Ok(device.read_val(Bid::new(blocknr).to_offset())?)
}

/// Test helper: writes a clean [`RawJournalSuperblock`] (`s_start == 0`) at the
/// given physical block, mirroring what `mke2fs` lays down for a fresh journal.
///
/// Lets the ext4-level fixture builder place a valid journal superblock on disk
/// before `Ext4::open` without naming the journal-internal on-disk format. Only
/// compiled for ktest.
#[cfg(ktest)]
pub(in crate::fs::fs_impls::ext4) fn write_clean_journal_superblock_for_test(
    device: &dyn BlockDevice,
    pblock: Ext4Bid,
    maxlen: u32,
    first: u32,
    sequence: Tid,
) -> Result<()> {
    use self::format::{JBD2_MAGIC, RawJournalHeader};
    let raw = RawJournalSuperblock {
        header: RawJournalHeader {
            h_magic: Be32::new(JBD2_MAGIC),
            h_blocktype: Be32::new(BLOCKTYPE_SUPERBLOCK_V2),
            h_sequence: Be32::new(0),
        },
        s_blocksize: Be32::new(BLOCK_SIZE as u32),
        s_maxlen: Be32::new(maxlen),
        s_first: Be32::new(first),
        s_sequence: Be32::new(sequence.get()),
        s_start: Be32::new(0),
        s_nr_users: Be32::new(1),
        ..Default::default()
    };
    device
        .write_val(pblock as usize * BLOCK_SIZE, &raw)
        .map_err(|_| Error::with_message(Errno::EIO, "failed to write journal superblock"))
}

impl Journal {
    /// Test helper: runs one FORCED commit pass (the commit thread's
    /// [`commit_one`](Journal::commit_one) with the due policy overridden)
    /// **without** the eager checkpoint that normally follows it, so a test
    /// can hold the journal in the committed-but-un-checkpointed window
    /// deterministically — the window in which the device lags the log and
    /// a capture's seed provenance matters. Under group commit "forced"
    /// means the batching triggers are bypassed, NOT the freeze invariant:
    /// a transaction with open handles is parked to drain, not committed.
    #[cfg(ktest)]
    pub(in crate::fs::fs_impls::ext4) fn commit_now_for_test(&self) {
        self.commit_one(true);
    }

    /// Test helper: one UNFORCED pipeline advance — exactly what the commit
    /// thread does on a wake, minus the checkpoint — so the batching policy
    /// (size/age/request triggers) is testable deterministically. Returns
    /// whether a commit was attempted.
    #[cfg(ktest)]
    pub(in crate::fs::fs_impls::ext4) fn commit_if_due_for_test(&self) -> bool {
        self.commit_one(false)
    }

    /// Test helper: FORCED staging without the commit I/O — the pipeline
    /// tests' deterministic "pin the pipeline mid-flight" primitive: the
    /// drained running transaction is staged into the committing slot (via
    /// [`advance_pipeline`](Self::advance_pipeline), policy overridden)
    /// and returned for a hand-driven [`commit_staged`](Self::commit_staged);
    /// `None` when nothing stages (no captures, open handles → parked, or
    /// an occupied slot).
    #[cfg(ktest)]
    pub(in crate::fs::fs_impls::ext4::journal) fn stage_running_for_test(
        &self,
    ) -> Option<Arc<Transaction>> {
        match self.advance_pipeline(true) {
            PipelineStep::Staged(txn) => Some(txn),
            PipelineStep::Parked | PipelineStep::Idle => None,
        }
    }

    /// Test helper: the locking seat's `(tid, open handles, captured
    /// blocks)`, or `None` when no transaction is draining. White-box
    /// inspection for the T_LOCKED drain tests.
    #[cfg(ktest)]
    pub(in crate::fs::fs_impls::ext4) fn locking_state_for_test(
        &self,
    ) -> Option<(Tid, usize, usize)> {
        self.state_read()
            .locking
            .as_ref()
            .map(|txn| (txn.tid(), txn.nr_updates(), txn.nr_metadata_blocks()))
    }

    /// Test helper: back-dates the running transaction's age deadline so the
    /// age trigger fires on the next (deterministic, test-driven) poll —
    /// real timer waits are not deterministic in ktest.
    #[cfg(ktest)]
    pub(in crate::fs::fs_impls::ext4) fn age_running_for_test(&self) {
        if let Some(txn) = self.state_write().running.as_mut() {
            txn.force_expire_for_test();
        }
    }

    /// Test helper: the blocks in the running transaction's revoke set, in
    /// block order (empty when no transaction runs). White-box inspection for
    /// the forget-coverage tests; call with the commit thread stopped, or the
    /// transaction may be consumed between the operation and the assertion.
    #[cfg(ktest)]
    pub(in crate::fs::fs_impls::ext4) fn running_revoked_blocks_for_test(&self) -> Vec<Ext4Bid> {
        self.state_read()
            .running
            .as_ref()
            .map_or_else(Vec::new, |txn| txn.revoked_blocks().collect())
    }

    /// Test helper: the number of records in the committed-revoke memory —
    /// nonzero between a revoking transaction's commit and the checkpoint
    /// that retires it.
    #[cfg(ktest)]
    pub(in crate::fs::fs_impls::ext4) fn committed_revoke_records_for_test(&self) -> usize {
        self.state_read().revoked.len()
    }

    /// Test helper: the blocks the running transaction has captured, in block
    /// order (empty when no transaction runs). White-box inspection for the
    /// forget-effects tests; call with the commit thread stopped.
    #[cfg(ktest)]
    pub(in crate::fs::fs_impls::ext4) fn running_captured_blocks_for_test(&self) -> Vec<Ext4Bid> {
        self.state_read()
            .running
            .as_ref()
            .map_or_else(Vec::new, |txn| {
                txn.metadata_blocks().map(|(bid, _)| bid).collect()
            })
    }

    /// Test helper: the blocks with a retained committed-but-un-checkpointed
    /// after-image. White-box inspection for the forget-effects tests.
    #[cfg(ktest)]
    pub(in crate::fs::fs_impls::ext4) fn uncheckpointed_blocks_for_test(&self) -> Vec<Ext4Bid> {
        self.state_read().uncheckpointed.keys().copied().collect()
    }

    /// Test helper: stages an externally built transaction into the
    /// committing slot WITHOUT committing it — the deterministic "pin the
    /// pipeline mid-flight" primitive of the duality tests, and the staging
    /// half of the ktest commit funnel
    /// ([`commit_transaction`](commit::commit_transaction)), so direct
    /// pipeline tests run slot-resident exactly like production. Errors
    /// `EIO` if a transaction is already staged. Production transactions
    /// reach the slot through
    /// [`advance_pipeline`](Journal::advance_pipeline) or the
    /// unmount flush's staging ([`flush_on_unmount`](Journal::flush_on_unmount)).
    #[cfg(ktest)]
    pub(in crate::fs::fs_impls::ext4::journal) fn stage_transaction_for_test(
        &self,
        txn: Transaction,
    ) -> Result<Arc<Transaction>> {
        let mut st = self.state_write();
        if st.committing.is_some() {
            return_errno_with_message!(Errno::EIO, "a transaction is already committing");
        }
        Ok(st.stage_committing(txn))
    }

    /// Test helper: the committing slot's `(tid, phase)`, or `None` when no
    /// transaction is committing. White-box inspection for the pipeline
    /// tests.
    #[cfg(ktest)]
    pub(in crate::fs::fs_impls::ext4) fn committing_for_test(&self) -> Option<(Tid, CommitPhase)> {
        self.state_read()
            .committing
            .as_ref()
            .map(|committing| (committing.tid(), committing.phase()))
    }
}

/// Test helper: commits a single-block transaction (`dest` ← `after`) to `journal`,
/// leaving the on-disk log dirty (`s_start != 0`) so a subsequent mount recovers it.
///
/// Encapsulates the journal-internal commit machinery ([`Transaction`],
/// [`commit_transaction`](commit::commit_transaction)) so the `fs.rs`
/// mount-lifecycle tests can lay down a
/// crashed (committed-but-un-checkpointed) journal on the fixture disk without
/// reaching into those internals themselves. Only compiled for ktest.
#[cfg(ktest)]
pub(in crate::fs::fs_impls::ext4) fn commit_single_block_for_test(
    journal: &Journal,
    device: &dyn BlockDevice,
    dest: Ext4Bid,
    after: [u8; BLOCK_SIZE],
) -> Result<()> {
    let mut txn = Transaction::new(journal.state_read().next_tid);
    let generation = txn.capture_create(dest);
    txn.apply_patch(dest, generation, |b| b.copy_from_slice(&after))?;
    commit::commit_transaction(journal, device, txn)?;
    Ok(())
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::{
        super::test_utils::{Ext4FixtureBuilder, make_multi_block_file_inode},
        format::{JBD2_MAGIC, RawJournalHeader},
        *,
    };

    /// The physical block holding the journal superblock and the first log block.
    const JOURNAL_START_BLOCK: u32 = 200;

    /// Builds an on-disk journal superblock with the given geometry.
    fn journal_super(maxlen: u32, first: u32, sequence: u32, start: u32) -> RawJournalSuperblock {
        RawJournalSuperblock {
            header: RawJournalHeader {
                h_magic: Be32::new(JBD2_MAGIC),
                h_blocktype: Be32::new(BLOCKTYPE_SUPERBLOCK_V2),
                h_sequence: Be32::new(0),
            },
            s_blocksize: Be32::new(BLOCK_SIZE as u32),
            s_maxlen: Be32::new(maxlen),
            s_first: Be32::new(first),
            s_sequence: Be32::new(sequence),
            s_start: Be32::new(start),
            s_nr_users: Be32::new(1),
            ..Default::default()
        }
    }

    #[ktest]
    fn load_geometry_end_to_end() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_has_journal()
            .build()
            .unwrap();

        // The journal inode (ino 8) maps log blocks [0, 2) to physical
        // [200, 202); log block 0 (physical 200) holds the journal superblock.
        let raw_journal_inode = make_multi_block_file_inode(JOURNAL_START_BLOCK, 2);
        f.write_raw_inode(JOURNAL_INO, &raw_journal_inode);

        // Write a valid journal superblock into physical block 200.
        f.disk
            .segment()
            .write_val(
                JOURNAL_START_BLOCK as usize * BLOCK_SIZE,
                &journal_super(2, 1, 1, 0),
            )
            .unwrap();

        let geo = load_geometry(&f.ext4).unwrap().unwrap();
        assert_eq!(geo.maxlen(), 2);
        assert_eq!(geo.first(), 1);
        assert_eq!(geo.sequence(), Tid::new(1));
        assert_eq!(geo.start(), None);
        assert_eq!(geo.blocksize(), BLOCK_SIZE as u32);
        assert_eq!(
            geo.log_block_to_physical(0),
            Some(JOURNAL_START_BLOCK as Ext4Bid)
        );
        assert_eq!(
            geo.log_block_to_physical(1),
            Some(JOURNAL_START_BLOCK as Ext4Bid + 1)
        );
        // Past the end of the log.
        assert_eq!(geo.log_block_to_physical(2), None);
    }

    /// The P7a-4 admission set, end to end through `load_geometry`: every
    /// stamped-and-verified feature combination (64bit, csum v2/v3, ±64bit,
    /// revoke) is admitted, while async commit, fast commit, and the
    /// contradictory csum_v2+csum_v3 pair stay refused with `EINVAL`.
    #[ktest]
    fn load_geometry_admission_matches_supp_set() {
        use super::format::{
            INCOMPAT_ASYNC_COMMIT, INCOMPAT_CSUM_V2, INCOMPAT_FAST_COMMIT, INCOMPAT_REVOKE,
        };

        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_has_journal()
            .build()
            .unwrap();
        let raw_journal_inode = make_multi_block_file_inode(JOURNAL_START_BLOCK, 2);
        f.write_raw_inode(JOURNAL_INO, &raw_journal_inode);

        let write_sb = |features: u32| {
            let mut raw = journal_super(2, 1, 1, 0);
            raw.s_feature_incompat = Be32::new(features);
            if features & (INCOMPAT_CSUM_V2 | INCOMPAT_CSUM_V3) != 0 {
                raw.s_checksum_type = JBD2_CRC32C_CHKSUM;
                raw.s_checksum = Be32::new(raw.checksum());
            }
            f.disk
                .segment()
                .write_val(JOURNAL_START_BLOCK as usize * BLOCK_SIZE, &raw)
                .unwrap();
        };

        for features in [
            0,
            INCOMPAT_REVOKE,
            INCOMPAT_64BIT,
            INCOMPAT_CSUM_V3,
            INCOMPAT_CSUM_V3 | INCOMPAT_64BIT,
            INCOMPAT_CSUM_V2,
            INCOMPAT_CSUM_V2 | INCOMPAT_64BIT,
        ] {
            write_sb(features);
            let geo = load_geometry(&f.ext4)
                .unwrap_or_else(|e| panic!("features {features:#x} refused: {e:?}"))
                .unwrap();
            // The parse derived the seed exactly for the csum sets.
            assert_eq!(
                geo.csum_seed().is_some(),
                features & (INCOMPAT_CSUM_V2 | INCOMPAT_CSUM_V3) != 0,
                "features {features:#x}"
            );
        }

        // Async commit and fast commit stay outside the set.
        for features in [INCOMPAT_ASYNC_COMMIT, INCOMPAT_FAST_COMMIT] {
            write_sb(features);
            let err = load_geometry(&f.ext4).map(|_| ()).unwrap_err();
            assert_eq!(err.error(), Errno::EINVAL, "features {features:#x}");
        }
        // csum_v2 + csum_v3 passes the SUPP mask (both bits are admitted) but
        // the parse refuses the contradictory tag layouts.
        write_sb(INCOMPAT_CSUM_V2 | INCOMPAT_CSUM_V3);
        let err = load_geometry(&f.ext4).map(|_| ()).unwrap_err();
        assert_eq!(err.error(), Errno::EINVAL);
    }

    /// Any set journal ro_compat bit refuses the mount: Linux refuses every
    /// unknown ro_compat bit at load (fs/jbd2/journal.c:1382-1388) and
    /// `JBD2_KNOWN_ROCOMPAT_FEATURES` is empty in 6.6 — there is no "mount
    /// read-only instead" fallback, and this port always mounts read-write.
    #[ktest]
    fn load_geometry_refuses_journal_ro_compat_bits() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_has_journal()
            .build()
            .unwrap();
        f.write_raw_inode(
            JOURNAL_INO,
            &make_multi_block_file_inode(JOURNAL_START_BLOCK, 2),
        );

        // An otherwise pristine featureless journal with one ro_compat bit.
        let mut raw = journal_super(2, 1, 1, 0);
        raw.s_feature_ro_compat = Be32::new(0x1);
        f.disk
            .segment()
            .write_val(JOURNAL_START_BLOCK as usize * BLOCK_SIZE, &raw)
            .unwrap();

        let err = load_geometry(&f.ext4).map(|_| ()).unwrap_err();
        assert_eq!(err.error(), Errno::EINVAL);
    }

    /// A V1-blocktype journal superblock's feature words are meaningless
    /// (Linux `jbd2_format_support_feature`; see the format-module parse
    /// test), so admission ignores them entirely: bits that refuse a V2
    /// journal two ways over — an unadmitted INCOMPAT (fast commit) and a
    /// set ro_compat word — load fine on a V1 and parse as featureless v0.
    #[ktest]
    fn load_geometry_admits_v1_journal_regardless_of_feature_bits() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_has_journal()
            .build()
            .unwrap();
        f.write_raw_inode(
            JOURNAL_INO,
            &make_multi_block_file_inode(JOURNAL_START_BLOCK, 2),
        );

        let mut raw = journal_super(2, 1, 1, 0);
        raw.header.h_blocktype = Be32::new(format::BLOCKTYPE_SUPERBLOCK_V1);
        raw.s_feature_incompat = Be32::new(0x20); // fast commit: refused on V2
        raw.s_feature_ro_compat = Be32::new(0xFF); // any bit: refused on V2
        f.disk
            .segment()
            .write_val(JOURNAL_START_BLOCK as usize * BLOCK_SIZE, &raw)
            .unwrap();

        let geo = load_geometry(&f.ext4).unwrap().unwrap();
        assert_eq!(geo.tag_layout(), TagLayout::from_features(0).unwrap());
        assert!(geo.csum_seed().is_none());
    }

    #[ktest]
    fn load_geometry_without_journal_is_none() {
        // Same fixture but without the HAS_JOURNAL compat bit.
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        assert!(load_geometry(&f.ext4).unwrap().is_none());
    }

    // --- Task 5: commit thread, log_wait_commit, teardown, Tid::geq. ---

    use super::{
        super::test_utils::Ext4Fixture,
        format::{BLOCKTYPE_COMMIT, BLOCKTYPE_DESCRIPTOR, RawBlockTag},
        transaction::{journal_start, journal_stop},
    };

    /// A journaled fixture that keeps the disk-owning [`Ext4Fixture`] alive so a
    /// test can read the on-disk log, plus the in-memory [`Journal`] with its
    /// commit thread available to start/stop.
    struct JournaledFixture {
        journal: Arc<Journal>,
        fixture: Ext4Fixture,
    }

    /// Builds a journaled fixture with a `maxlen`-block log at physical
    /// `[200, 200+maxlen)`, first log-data block `first`, and `s_sequence`. The
    /// commit thread is **not** started (each test starts it explicitly, so it can
    /// also assert clean teardown).
    fn journaled_fixture(maxlen: u32, first: u32, sequence: u32) -> JournaledFixture {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_has_journal()
            .build()
            .unwrap();

        let raw_journal_inode = make_multi_block_file_inode(JOURNAL_START_BLOCK, maxlen as u16);
        f.write_raw_inode(JOURNAL_INO, &raw_journal_inode);
        f.disk
            .segment()
            .write_val(
                JOURNAL_START_BLOCK as usize * BLOCK_SIZE,
                &journal_super(maxlen, first, sequence, 0),
            )
            .unwrap();

        let geometry = load_geometry(&f.ext4).unwrap().unwrap();
        let journal = Journal::new(geometry, f.ext4.block_device().clone());
        JournaledFixture {
            journal,
            fixture: f,
        }
    }

    /// Reads the jbd2 header of a log block by its log index.
    fn read_log_header(f: &JournaledFixture, log: u32) -> RawJournalHeader {
        let pblock = (JOURNAL_START_BLOCK + log) as usize * BLOCK_SIZE;
        f.fixture.disk.segment().read_val(pblock).unwrap()
    }

    /// Reads tag 0 from a descriptor block at log index `log`.
    fn read_first_tag(f: &JournaledFixture, log: u32) -> RawBlockTag {
        let base = (JOURNAL_START_BLOCK + log) as usize * BLOCK_SIZE;
        let header_len = size_of::<RawJournalHeader>();
        f.fixture
            .disk
            .segment()
            .read_val(base + header_len)
            .unwrap()
    }

    #[ktest]
    fn tid_geq_wrapping() {
        // Simple ordering.
        assert!(Tid::new(1).geq(Tid::new(0)));
        assert!(Tid::new(5).geq(Tid::new(5))); // equal is "at or after"
        assert!(!Tid::new(0).geq(Tid::new(1)));
        // Wrap: 0 is "after" u32::MAX (0 == MAX + 1 in wrapping arithmetic).
        assert!(Tid::new(0).geq(Tid::new(u32::MAX)));
        assert!(!Tid::new(u32::MAX).geq(Tid::new(0)));
    }

    /// The successor/predecessor helpers wrap at the `u32` boundary, matching
    /// the wrapping arithmetic the raw tids used.
    #[ktest]
    fn tid_next_prev_wrap() {
        assert_eq!(Tid::new(1).next(), Tid::new(2));
        assert_eq!(Tid::new(2).prev(), Tid::new(1));
        // Across the wrap in both directions.
        assert_eq!(Tid::new(u32::MAX).next(), Tid::new(0));
        assert_eq!(Tid::new(0).prev(), Tid::new(u32::MAX));
        // A wrapped successor is still "at or after" its predecessor.
        assert!(Tid::new(u32::MAX).next().geq(Tid::new(u32::MAX)));
    }

    /// The money test: a full commit driven by the background thread, waited on
    /// via `log_wait_commit`, then a clean teardown.
    #[ktest]
    fn commit_thread_end_to_end() {
        crate::time::clocks::init_for_ktest();

        let f = journaled_fixture(16, 1, 1);
        f.journal.start_commit_thread();

        // Open a handle, capture a metadata block into the running transaction,
        // then close the handle so the transaction has no open handles.
        let handle = journal_start(&f.journal, 4).unwrap();
        let running_tid = handle.tid();
        {
            let mut st = f.journal.state_write();
            let txn = st.running.as_mut().unwrap();
            let generation = txn.capture_create(500);
            txn.apply_patch(500, generation, |b| b[..4].copy_from_slice(b"META"))
                .unwrap();
        }
        journal_stop(handle).unwrap();

        // Wait for the commit thread to commit the running transaction.
        f.journal.log_wait_commit(running_tid).unwrap();

        // The transaction is now committed and durable.
        assert_eq!(f.journal.committed_tid(), running_tid);

        // The on-disk log holds the transaction: descriptor at log block `first`
        // (== 1) carrying our tid, its sole tag pointing at block 500, and a
        // commit block at log block 3 (desc + 1 data + commit).
        let desc = read_log_header(&f, 1);
        assert_eq!(desc.h_magic.get(), JBD2_MAGIC);
        assert_eq!(desc.h_blocktype.get(), BLOCKTYPE_DESCRIPTOR);
        assert_eq!(desc.h_sequence.get(), running_tid.get());
        assert_eq!(read_first_tag(&f, 1).t_blocknr.get(), 500);
        let commit = read_log_header(&f, 3);
        assert_eq!(commit.h_blocktype.get(), BLOCKTYPE_COMMIT);
        assert_eq!(commit.h_sequence.get(), running_tid.get());

        // The running transaction was consumed by the commit.
        assert!(f.journal.state_write().running.is_none());

        // Teardown: stops and joins the commit thread. Returns (does not hang).
        f.journal.stop_commit_thread();
    }

    /// The nested-`begin_op` guard (c2 hardening): a task holding an operation
    /// handle that opens a second one gets an immediate `EDEADLK` instead of the
    /// self-deadlock (the nested `journal_start` would park on a barrier only
    /// the first handle's close can clear). Structured so a regression that
    /// dropped the guard would make the second `start` succeed — the
    /// `unwrap_err` then FAILS the test loudly rather than hanging CI. The
    /// [`journal_start`] primitive stays unguarded (this drives `OpHandle`, the
    /// operation boundary, deliberately), so the transaction-sharing tests keep
    /// working.
    #[ktest]
    fn nested_op_handle_on_one_task_is_rejected() {
        let f = journaled_fixture(16, 1, 1);

        // First operation handle registers this task as an owner.
        let op1 = OpHandle::start(&f.journal, 4).unwrap();

        // A second handle on the SAME task is the nested self-deadlock: refused.
        let err = OpHandle::start(&f.journal, 4).map(|_| ()).unwrap_err();
        assert_eq!(err.error(), Errno::EDEADLK);

        // Closing the first clears the task's slot: a FRESH handle opened after
        // the predecessor closed (the reclaim pattern) is admitted again — the
        // guard does not false-positive on the legitimate declaration-order path.
        drop(op1);
        let op2 = OpHandle::start(&f.journal, 4).unwrap();
        drop(op2);
    }

    /// A running transaction with only durable obligations that are NOT captured
    /// metadata — a revoke record, or a pinned freed run — is still committable
    /// (c2 hardening of the due/age/request gate). The historical gate tested
    /// captured metadata alone and would have refused it; the widened gate
    /// counts the obligation set structurally. Constructed synthetically because
    /// `Ext4::free_blocks` always captures the group bitmap, so no real path
    /// reaches zero captures with a live obligation.
    #[ktest]
    fn revoke_or_pin_only_transaction_is_committable() {
        let f = journaled_fixture(64, 1, 1);
        let handle = journal_start(&f.journal, 4).unwrap();
        let tid = handle.tid();

        // Empty transaction: no metadata, no revokes, no pins — never
        // committable (committing it writes no log block; a `log_wait_commit`
        // waiter on its tid would hang).
        assert!(f.journal.state_read().committable_running().is_none());

        // A pinned freed run alone (a plain-data free, which records no revoke)
        // is a durable obligation commit step 6 must release: committable via
        // the pin branch of the gate, with zero captures and zero revokes.
        f.journal
            .state_write()
            .pinned_frees
            .insert(500, PinnedRun { count: 1, tid });
        assert_eq!(
            f.journal.state_read().committable_running().map(|(_, t)| t),
            Some(tid),
            "a pin-only transaction is committable",
        );

        // A recorded revoke (a journaled free's revoke) is equally one, and the
        // trigger test now fires the transaction for an explicit request.
        f.journal
            .state_write()
            .running
            .as_mut()
            .unwrap()
            .forget_block(90_000);
        {
            let st = f.journal.state_read();
            let (txn, gated_tid) = st.committable_running().unwrap();
            assert_eq!(gated_tid, tid);
            assert!(
                f.journal
                    .commit_triggered(txn, Jiffies::elapsed(), Some(tid)),
                "a committable transaction fires on a request covering its tid",
            );
        }

        journal_stop(handle).unwrap();
    }

    /// `log_wait_commit` returns immediately for an already-committed tid, without
    /// requesting another commit or hanging.
    #[ktest]
    fn log_wait_commit_returns_when_already_committed() {
        crate::time::clocks::init_for_ktest();

        let f = journaled_fixture(16, 1, 1);
        f.journal.start_commit_thread();

        let handle = journal_start(&f.journal, 4).unwrap();
        let tid = handle.tid();
        {
            let mut st = f.journal.state_write();
            let txn = st.running.as_mut().unwrap();
            let generation = txn.capture_create(500);
            txn.apply_patch(500, generation, |b| b[..4].copy_from_slice(b"META"))
                .unwrap();
        }
        journal_stop(handle).unwrap();

        f.journal.log_wait_commit(tid).unwrap();
        assert_eq!(f.journal.committed_tid(), tid);

        // Already committed: this must return promptly (the fast path takes no
        // wait), not block.
        f.journal.log_wait_commit(tid).unwrap();

        f.journal.stop_commit_thread();
    }

    /// Starting then immediately stopping the commit thread, with no work to do,
    /// returns promptly: the idle thread is woken by the stop and exits.
    #[ktest]
    fn teardown_with_no_work_is_prompt() {
        let f = journaled_fixture(16, 1, 1);
        f.journal.start_commit_thread();
        // No transaction, no work: stop must still wake the idle thread and join.
        f.journal.stop_commit_thread();
        // Idempotent: a second stop is a no-op (handle already taken).
        f.journal.stop_commit_thread();
    }

    // --- Int-B B1: metadata-capture funnels (get_*_access / dirty_metadata). ---

    /// A physical block used to exercise capture, clear of the log region.
    const CAPTURE_BLOCK: Ext4Bid = 300;

    /// `get_write_access` seeds the after-image from the device, then
    /// `dirty_metadata` patches it in place (the sub-block RMW pattern): the
    /// seeded device bytes survive everywhere the patch does not touch.
    #[ktest]
    fn funnel_write_access_seeds_from_device_then_patches() {
        let f = journaled_fixture(16, 1, 1);

        let mut on_disk = [0u8; BLOCK_SIZE];
        on_disk[0] = 0x11;
        on_disk[BLOCK_SIZE - 1] = 0x22;
        f.fixture
            .disk
            .segment()
            .write_val(CAPTURE_BLOCK as usize * BLOCK_SIZE, &on_disk)
            .unwrap();

        let handle = journal_start(&f.journal, 4).unwrap();
        get_write_access(Some(&handle), CAPTURE_BLOCK)
            .unwrap()
            .patch(|buf| {
                buf[4] = 0xAB;
            })
            .unwrap();

        let st = f.journal.state_read();
        let captured = st
            .running
            .as_ref()
            .unwrap()
            .buffer_bytes(CAPTURE_BLOCK)
            .unwrap();
        assert_eq!(captured[0], 0x11, "seeded device byte survives");
        assert_eq!(
            captured[BLOCK_SIZE - 1],
            0x22,
            "seeded device byte survives"
        );
        assert_eq!(captured[4], 0xAB, "patch landed on the after-image");
        drop(st);
        journal_stop(handle).unwrap();
    }

    /// `get_create_access` seeds a *zeroed* after-image (the device content is
    /// ignored), and repeated `dirty_metadata` patches on the same block
    /// accumulate onto that one buffer.
    #[ktest]
    fn funnel_create_access_zeroes_and_accumulates() {
        let f = journaled_fixture(16, 1, 1);

        // Fill the device block with garbage to prove create-access ignores it.
        let garbage = [0xFFu8; BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .write_val(CAPTURE_BLOCK as usize * BLOCK_SIZE, &garbage)
            .unwrap();

        let handle = journal_start(&f.journal, 4).unwrap();
        let access = get_create_access(Some(&handle), CAPTURE_BLOCK).unwrap();
        access.patch(|buf| buf[0] = 1).unwrap();
        // A re-minted credential patches the SAME capture (idempotent access).
        get_write_access(Some(&handle), CAPTURE_BLOCK)
            .unwrap()
            .patch(|buf| buf[1] = 2)
            .unwrap();

        let st = f.journal.state_read();
        let captured = st
            .running
            .as_ref()
            .unwrap()
            .buffer_bytes(CAPTURE_BLOCK)
            .unwrap();
        assert_eq!(captured[0], 1, "first patch");
        assert_eq!(captured[1], 2, "second patch accumulates");
        assert_eq!(captured[2], 0, "zeroed seed, not device garbage");
        drop(st);
        journal_stop(handle).unwrap();
    }

    /// Without a handle the funnels are inert: no transaction is created and the
    /// `dirty_metadata` patch is never invoked (Phases 1–3 behaviour).
    #[ktest]
    fn funnel_without_handle_is_inert() {
        let f = journaled_fixture(16, 1, 1);

        let mut patched = false;
        let access = get_write_access(None, CAPTURE_BLOCK).unwrap();
        assert!(!access.is_live());
        access.patch(|_| patched = true).unwrap();

        assert!(!patched, "patch must not run without a handle");
        assert!(
            f.journal.state_read().running.is_none(),
            "no transaction created without a handle"
        );
    }

    /// Patch-without-capture is unrepresentable through [`WriteAccess`] (the
    /// old free `dirty_metadata` could be called without a prior access); the
    /// transaction-level defensive check underneath still errors if reached.
    #[ktest]
    fn apply_patch_without_capture_errors_defensively() {
        let f = journaled_fixture(16, 1, 1);
        let handle = journal_start(&f.journal, 4).unwrap();
        let mut st = f.journal.state_write();
        // A generation minted for a DIFFERENT block: `CAPTURE_BLOCK` itself
        // was never captured, so the patch must fail regardless.
        let generation = st.running.as_mut().unwrap().capture_create(999);
        assert!(
            st.running
                .as_mut()
                .unwrap()
                .apply_patch(CAPTURE_BLOCK, generation, |_| {})
                .is_err(),
            "apply_patch without a capture must fail"
        );
        drop(st);
        journal_stop(handle).unwrap();
    }

    // --- Int-B B2.1: commit/checkpoint triggering (unmount flush). ---

    /// `flush_on_unmount` commits the running transaction and checkpoints it, so
    /// the after-image reaches its final location and the on-disk journal is left
    /// clean (`s_start == 0`). Driven synchronously here (no commit thread), the
    /// deterministic mirror of the unmount path.
    #[ktest]
    fn flush_on_unmount_commits_running_and_cleans_journal() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);

        let bid: Ext4Bid = 500;
        let handle = journal_start(&f.journal, 4).unwrap();
        get_write_access(Some(&handle), bid)
            .unwrap()
            .patch(|buf| buf[..8].copy_from_slice(b"UNMOUNT!"))
            .unwrap();
        // Closing the last handle marks the transaction committable (and pings the
        // — here unstarted — commit thread); the running transaction survives for
        // the synchronous flush below.
        journal_stop(handle).unwrap();

        f.journal.flush_on_unmount().unwrap();

        // The after-image reached its final location.
        let mut final_block = [0u8; BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .read_bytes(bid as usize * BLOCK_SIZE, &mut final_block)
            .unwrap();
        assert_eq!(&final_block[..8], b"UNMOUNT!");

        // The on-disk journal is clean and the in-memory tail cleared, so the next
        // mount skips recovery.
        let sb: RawJournalSuperblock = f
            .fixture
            .disk
            .segment()
            .read_val(JOURNAL_START_BLOCK as usize * BLOCK_SIZE)
            .unwrap();
        assert_eq!(sb.s_start.get(), 0);
        assert_eq!(f.journal.state_read().tail_block, None);
        assert!(f.journal.state_read().running.is_none());
    }

    /// `flush_on_unmount` on a journal that never ran a transaction is a clean
    /// no-op (nothing captured, tail already clean).
    #[ktest]
    fn flush_on_unmount_with_no_work_is_noop() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);
        f.journal.flush_on_unmount().unwrap();
        assert_eq!(f.journal.state_read().tail_block, None);
    }

    /// The abort discipline at unmount (P7c-1 review MAJOR): on an aborted
    /// journal `flush_on_unmount` refuses at entry — nothing staged, no log
    /// byte written, the running transaction dropped. Committing it would
    /// publish a recoverable successor of the transaction the abort lost;
    /// the untouched dirty log is what the next mount safely replays
    /// (committed-before-abort transactions only).
    #[ktest]
    fn flush_on_unmount_on_aborted_journal_writes_nothing() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);

        // A dirty (committable) running transaction at unmount time.
        let h = journal_start(&f.journal, 4).unwrap();
        get_write_access(Some(&h), 500)
            .unwrap()
            .patch(|b| b[..4].copy_from_slice(b"LOST"))
            .unwrap();
        journal_stop(h).unwrap();

        f.journal.abort_for_fs_error();
        let mut before = vec![0u8; 16 * BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .read_bytes(JOURNAL_START_BLOCK as usize * BLOCK_SIZE, &mut before)
            .unwrap();

        let err = f.journal.flush_on_unmount().unwrap_err();
        assert_eq!(err.error(), Errno::EIO);

        // Nothing staged, the running transaction gone, every log byte
        // exactly as the abort left it.
        assert_eq!(f.journal.committing_for_test(), None);
        assert!(f.journal.state_read().running.is_none());
        let mut after = vec![0u8; 16 * BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .read_bytes(JOURNAL_START_BLOCK as usize * BLOCK_SIZE, &mut after)
            .unwrap();
        assert_eq!(before, after, "the flush must not touch an aborted log");
    }

    // --- a5 review MAJOR 2: the free-segment fit bound and its abort
    // fallback. ---

    /// The free-segment ring distance ([`JournalGeometry::free_log_blocks`]):
    /// a clean ring frees the whole usable ring; a dirty ring frees the
    /// distance from the head forward to the tail, across the wrap; and
    /// `head == tail` on a dirty ring is FULL (0), never the ring size.
    #[ktest]
    fn free_log_blocks_ring_distance() {
        // maxlen 16, first 1: ring = 15, positions in [1, 16).
        let f = journaled_fixture(16, 1, 1);
        let geo = f.journal.geometry();

        // Clean: the whole ring, wherever the head sits.
        assert_eq!(geo.free_log_blocks(1, None), 15);
        assert_eq!(geo.free_log_blocks(9, None), 15);

        // Dirty, free segment wrapping the ring end: [4..16) + nothing
        // before tail 1 = 12.
        assert_eq!(geo.free_log_blocks(4, Some(1)), 12);
        // Dirty, forward free segment: [4..11) = 7.
        assert_eq!(geo.free_log_blocks(4, Some(11)), 7);
        // Head at the last ring position: {15} = 1.
        assert_eq!(geo.free_log_blocks(15, Some(1)), 1);
        // Tail at the last ring position: [1..15) = 14.
        assert_eq!(geo.free_log_blocks(1, Some(15)), 14);
        // Full ring: head wrapped onto the tail.
        assert_eq!(geo.free_log_blocks(7, Some(7)), 0);
    }

    /// The abort half of the fit bound: when a commit does not fit the free
    /// segment AND the inline tail drain fails, the commit thread ABORTS the
    /// journal rather than overwrite the un-checkpointed tail — those log
    /// blocks are the only copy of committed metadata, and overwriting them
    /// would turn the next crash into a silent under-replay. (No fault
    /// injection exists for checkpoint writes; the drain failure is driven by
    /// corrupting the tail transaction's on-disk descriptor magic, which
    /// `apply_log_transaction` refuses with `EUCLEAN`.)
    #[ktest]
    fn commit_aborts_when_tail_cannot_drain() {
        crate::time::clocks::init_for_ktest();
        // maxlen 16, first 1: ring = 15.
        let f = journaled_fixture(16, 1, 1);

        // T1: one capture -> log [1..4); head 4, tail 1, free 12.
        let mut t1 = Transaction::new(Tid::new(1));
        let generation = t1.capture_create(500);
        t1.apply_patch(500, generation, |b| b[..4].copy_from_slice(b"TAIL"))
            .unwrap();
        commit::commit_transaction(
            f.journal.as_ref(),
            f.fixture.ext4.block_device().as_ref(),
            t1,
        )
        .unwrap();

        // Break the tail's descriptor magic so the drain (checkpoint ->
        // `apply_log_transaction`) fails.
        let desc_off = usize::try_from(JOURNAL_START_BLOCK + 1).unwrap() * BLOCK_SIZE;
        let mut desc = [0u8; BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .read_bytes(desc_off, &mut desc)
            .unwrap();
        desc[..4].fill(0xFF);
        f.fixture
            .disk
            .segment()
            .write_bytes(desc_off, &desc)
            .unwrap();

        // Snapshot the whole log: the refused T2 must write NOTHING.
        let log_off = usize::try_from(JOURNAL_START_BLOCK).unwrap() * BLOCK_SIZE;
        let mut before = vec![0u8; 16 * BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .read_bytes(log_off, &mut before)
            .unwrap();

        // T2: 11 captures -> footprint 13 (11 data + 1 desc + 1 commit),
        // over the 12 free blocks. Plant it as the running transaction and
        // drive the commit thread's own path synchronously.
        let mut t2 = Transaction::new(Tid::new(2));
        for dest in 1000u64..1011 {
            let generation = t2.capture_create(dest);
            t2.apply_patch(dest, generation, |b| b[..4].copy_from_slice(b"OVER"))
                .unwrap();
        }
        f.journal.state_write().running = Some(t2);
        f.journal.commit_now_for_test();

        // Aborted, not overwritten: the journal refuses further work, every
        // log byte (the tail transaction included) is untouched, and no
        // state advanced.
        assert!(f.journal.is_aborted());
        let mut after = vec![0u8; 16 * BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .read_bytes(log_off, &mut after)
            .unwrap();
        assert_eq!(before, after, "the refused commit must write nothing");
        {
            let st = f.journal.state_read();
            assert_eq!(st.head, 4);
            assert_eq!(st.tail_block, Some(1));
        }
        assert_eq!(f.journal.committed_tid(), Tid::new(1));

        // Waiters fail loudly instead of hanging on a tid that will never
        // commit.
        let err = f.journal.log_wait_commit(Tid::new(2)).unwrap_err();
        assert_eq!(err.error(), Errno::EIO);
    }

    /// T1 regression (the B-1 shared-block stale-seed class — the Task-8 guest
    /// silent data loss): a capture in a NEW transaction must seed the block from
    /// the newest committed-but-un-checkpointed after-image, NOT from the device,
    /// which lags until checkpoint completes.
    ///
    /// Two sub-block writers share block 500 (as two inodes share an inode-table
    /// block): txn 1 patches bytes [0..4] and is committed but NOT checkpointed —
    /// the exact window the async commit thread creates between two back-to-back
    /// ops. Txn 2 then captures the same block and patches bytes [8..12]. If its
    /// seed comes from the device (stale: txn 1 not applied yet), txn 2's
    /// after-image resurrects the pre-txn-1 bytes and — checkpoint applying in
    /// tid order, newest last — clobbers txn 1's write at the final location.
    #[ktest]
    fn capture_after_commit_seeds_from_uncheckpointed_image() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);
        let device = f.fixture.ext4.block_device().clone();

        // Known device content for the shared block.
        let bid: Ext4Bid = 500;
        let base = [0xAAu8; BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .write_bytes(bid as usize * BLOCK_SIZE, &base)
            .unwrap();

        // Txn 1: "slot A" writes bytes [0..4].
        let h1 = journal_start(&f.journal, 4).unwrap();
        get_write_access(Some(&h1), bid)
            .unwrap()
            .patch(|buf| buf[..4].copy_from_slice(&[0x11; 4]))
            .unwrap();
        journal_stop(h1).unwrap();
        // Commit WITHOUT checkpoint: the device still holds the pre-txn-1 bytes.
        f.journal.commit_now_for_test();

        // Txn 2 (a fresh transaction): "slot B" writes bytes [8..12]. Its capture
        // of the shared block must see txn 1's [0..4] == 0x11.
        let h2 = journal_start(&f.journal, 4).unwrap();
        get_write_access(Some(&h2), bid)
            .unwrap()
            .patch(|buf| buf[8..12].copy_from_slice(&[0x22; 4]))
            .unwrap();
        journal_stop(h2).unwrap();
        f.journal.commit_now_for_test();
        checkpoint::checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();

        // Both writers' bytes reach the final location; untouched bytes keep the
        // device content.
        let mut final_block = [0u8; BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .read_bytes(bid as usize * BLOCK_SIZE, &mut final_block)
            .unwrap();
        assert_eq!(
            &final_block[..4],
            &[0x11; 4],
            "txn 1's bytes survive txn 2's capture (stale-seed clobber)"
        );
        assert_eq!(&final_block[8..12], &[0x22; 4], "txn 2's own bytes applied");
        assert_eq!(
            final_block[100], 0xAA,
            "unpatched bytes keep device content"
        );
    }

    /// After checkpoint, the retained after-images are dropped and the device —
    /// now up to date — is the seed source again: a doctored device byte shows up
    /// in the next capture (proving the fallback), and the stash does not grow
    /// without bound.
    #[ktest]
    fn capture_after_checkpoint_seeds_from_device_again() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);
        let device = f.fixture.ext4.block_device().clone();

        let bid: Ext4Bid = 501;
        // Txn 1 writes [0..4]; commit + checkpoint make the device authoritative.
        let h1 = journal_start(&f.journal, 4).unwrap();
        get_write_access(Some(&h1), bid)
            .unwrap()
            .patch(|buf| buf[..4].copy_from_slice(&[0x11; 4]))
            .unwrap();
        journal_stop(h1).unwrap();
        f.journal.commit_now_for_test();
        checkpoint::checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();

        // Doctor a byte on the device — a capture that seeds from the device (and
        // only such a capture) will see it.
        let mut doctored = [0u8; BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .read_bytes(bid as usize * BLOCK_SIZE, &mut doctored)
            .unwrap();
        doctored[100] = 0x77;
        f.fixture
            .disk
            .segment()
            .write_bytes(bid as usize * BLOCK_SIZE, &doctored)
            .unwrap();

        // Txn 2 captures the block again: post-checkpoint there is no retained
        // image, so the seed is the (current) device content.
        let h2 = journal_start(&f.journal, 4).unwrap();
        get_write_access(Some(&h2), bid)
            .unwrap()
            .patch(|buf| {
                assert_eq!(buf[100], 0x77, "post-checkpoint capture seeds from device");
                assert_eq!(
                    &buf[..4],
                    &[0x11; 4],
                    "and the device carries txn 1's write"
                );
                buf[8..12].copy_from_slice(&[0x22; 4])
            })
            .unwrap();
        journal_stop(h2).unwrap();
        f.journal.commit_now_for_test();
        checkpoint::checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();

        let mut final_block = [0u8; BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .read_bytes(bid as usize * BLOCK_SIZE, &mut final_block)
            .unwrap();
        assert_eq!(&final_block[8..12], &[0x22; 4]);
        assert_eq!(final_block[100], 0x77);
    }

    /// `forget` evicts the freed block's committed-but-un-checkpointed image:
    /// the stale bytes stop being served by the read funnel
    /// (`read_metadata_block`) and stop seeding later captures — the device
    /// is the fallback again. Also drives cancel-on-recapture end to end
    /// through the production funnels: re-capturing the forgotten block in
    /// the same running transaction cancels its revoke record
    /// (`jbd2_journal_cancel_revoke`).
    #[ktest]
    fn forget_evicts_uncheckpointed_image_and_recapture_cancels() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);
        let device = f.fixture.ext4.block_device().clone();

        let bid: Ext4Bid = 502;
        let base = [0xAAu8; BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .write_bytes(bid as usize * BLOCK_SIZE, &base)
            .unwrap();

        // Txn 1 journals the block; committed but NOT checkpointed — the
        // retained image (not the lagging device) is the block's newest
        // content, and the read funnel serves it.
        let h1 = journal_start(&f.journal, 4).unwrap();
        get_write_access(Some(&h1), bid)
            .unwrap()
            .patch(|buf| buf[..4].copy_from_slice(&[0x11; 4]))
            .unwrap();
        journal_stop(h1).unwrap();
        f.journal.commit_now_for_test();
        let served = read_metadata_block(Some(f.journal.as_ref()), device.as_ref(), bid).unwrap();
        assert_eq!(&served[..4], &[0x11; 4]);

        // Txn 2 frees the block: the retained image is evicted, so metadata
        // reads fall through to the device again. The effects run at the
        // authorization's CONSUMPTION (inside `Ext4::free_blocks`); this
        // white-box test discharges the minted duty directly, standing in
        // for that consumption funnel.
        let h2 = journal_start(&f.journal, 4).unwrap();
        let (start, count, duty) = forget(bid, 1).into_parts();
        duty.unwrap().discharge(&h2, start, count).unwrap();
        assert_eq!(f.journal.running_revoked_blocks_for_test(), vec![bid]);
        let served = read_metadata_block(Some(f.journal.as_ref()), device.as_ref(), bid).unwrap();
        assert_eq!(
            served, base,
            "a forgotten block's stale image must stop being served"
        );

        // …and a re-capture (the block reallocated as metadata within the
        // same transaction) seeds from the device, not the evicted image —
        // and cancels the revoke, so the block's new image will not be
        // suppressed at checkpoint.
        get_write_access(Some(&h2), bid)
            .unwrap()
            .patch(|buf| {
                assert_eq!(
                    &buf[..4],
                    &[0xAA; 4],
                    "the capture must seed from the device"
                );
                buf[..4].copy_from_slice(&[0x22; 4]);
            })
            .unwrap();
        assert!(f.journal.running_revoked_blocks_for_test().is_empty());
        journal_stop(h2).unwrap();
    }

    /// A `WriteAccess` credential minted against a capture that was then
    /// cancelled (forget) and re-created (the block reallocated within the
    /// SAME transaction) is stale: the capture map's `(transaction, block#)`
    /// key matches, but the image belongs to the block's new owner, so the
    /// old credential's patch must fail `EIO` (capture-generation mismatch)
    /// instead of writing the old owner's bytes into the new owner's image.
    /// The fresh credential works.
    #[ktest]
    fn stale_write_access_across_recapture_fails() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);
        let h = journal_start(&f.journal, 8).unwrap();

        let bid: Ext4Bid = 503;
        let old = get_write_access(Some(&h), bid).unwrap();
        old.patch(|b| b[..4].copy_from_slice(&[0x11; 4])).unwrap();

        // The block is freed (the forget effects cancel the capture) and
        // reallocated as fresh metadata in the same transaction — a new
        // capture, a new image. The duty is discharged directly, standing in
        // for `Ext4::free_blocks`'s consumption funnel.
        let (start, count, duty) = forget(bid, 1).into_parts();
        duty.unwrap().discharge(&h, start, count).unwrap();
        let new = get_create_access(Some(&h), bid).unwrap();

        // The stale credential must not write into the new image.
        let err = old
            .patch(|b| b[..4].copy_from_slice(&[0x66; 4]))
            .unwrap_err();
        assert_eq!(err.error(), Errno::EIO);

        // The fresh credential patches normally, unpolluted by the stale one.
        new.patch(|b| {
            assert_eq!(&b[..4], &[0u8; 4], "the new capture is a fresh zero image");
            b[..4].copy_from_slice(&[0x22; 4]);
        })
        .unwrap();
        assert_eq!(
            f.journal
                .state_read()
                .running
                .as_ref()
                .and_then(|txn| txn.buffer_bytes(bid).map(|b| b[..4].to_vec())),
            Some(vec![0x22; 4])
        );
        journal_stop(h).unwrap();
    }

    // --- P7c-1: the running/committing two-transaction pipeline. ---
    // Deterministic recipe (experience §10.6): no commit thread runs; the
    // pipeline is pinned mid-flight by staging via the production advance
    // (`stage_running_for_test` → `advance_pipeline`, policy forced) and
    // finished by hand (`commit_staged`).

    /// The pipeline overlap itself: while T1 sits staged in the committing
    /// slot (mid-flight, `Locked`), `journal_start` opens a NEW running
    /// transaction with the next tid — without consulting or waiting on the
    /// slot — and it accepts captures; finishing T1 leaves T2 untouched.
    #[ktest]
    fn new_running_transaction_starts_while_committing_is_in_flight() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);

        // T1 captures a block and becomes committable.
        let h1 = journal_start(&f.journal, 4).unwrap();
        let t1_tid = h1.tid();
        get_write_access(Some(&h1), 510)
            .unwrap()
            .patch(|b| b[..4].copy_from_slice(&[0x11; 4]))
            .unwrap();
        journal_stop(h1).unwrap();

        // Pin the pipeline mid-flight: T1 staged, no I/O run yet.
        let t1 = f.journal.stage_running_for_test().unwrap();
        assert_eq!(
            f.journal.committing_for_test(),
            Some((t1_tid, CommitPhase::Locked))
        );
        assert!(f.journal.state_read().running.is_none());

        // A new running transaction starts while T1 commits: the next tid,
        // accepting captures — the two-transaction pipeline.
        let h2 = journal_start(&f.journal, 4).unwrap();
        assert_eq!(h2.tid(), t1_tid.next());
        get_write_access(Some(&h2), 600)
            .unwrap()
            .patch(|b| b[..4].copy_from_slice(&[0x22; 4]))
            .unwrap();
        {
            let st = f.journal.state_read();
            assert_eq!(st.running.as_ref().unwrap().tid(), t1_tid.next());
            assert_eq!(st.committing.as_ref().unwrap().tid(), t1_tid);
        }
        journal_stop(h2).unwrap();

        // Finishing T1 commits and retires exactly T1; T2 stays running.
        assert_eq!(f.journal.commit_staged(&t1).unwrap(), t1_tid);
        assert_eq!(f.journal.committing_for_test(), None);
        assert_eq!(f.journal.committed_tid(), t1_tid);
        assert_eq!(
            f.journal.state_read().running.as_ref().unwrap().tid(),
            t1_tid.next()
        );
    }

    /// The B-1 invariant under explicit duality: with T1 staged mid-flight
    /// (its after-images not on the device — not even in the log), a capture
    /// in the new running T2 seeds from T1's stashed image, and the metadata
    /// read funnel serves it; the end-to-end result carries both writers'
    /// bytes.
    #[ktest]
    fn capture_while_committing_seeds_from_its_stashed_image() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);
        let device = f.fixture.ext4.block_device().clone();

        let bid: Ext4Bid = 511;
        let base = [0xAAu8; BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .write_bytes(bid as usize * BLOCK_SIZE, &base)
            .unwrap();

        // T1 patches bytes [0..4] and is STAGED — mid-flight, zero I/O: the
        // device (and the log) still hold the pre-T1 bytes.
        let h1 = journal_start(&f.journal, 4).unwrap();
        get_write_access(Some(&h1), bid)
            .unwrap()
            .patch(|b| b[..4].copy_from_slice(&[0x11; 4]))
            .unwrap();
        journal_stop(h1).unwrap();
        let t1 = f.journal.stage_running_for_test().unwrap();

        // The read funnel serves the committing transaction's image.
        let served = read_metadata_block(Some(f.journal.as_ref()), device.as_ref(), bid).unwrap();
        assert_eq!(&served[..4], &[0x11; 4]);
        assert_eq!(served[100], 0xAA);

        // A capture in the NEW running T2 seeds from T1's image, not the
        // lagging device.
        let h2 = journal_start(&f.journal, 4).unwrap();
        get_write_access(Some(&h2), bid)
            .unwrap()
            .patch(|b| {
                assert_eq!(&b[..4], &[0x11; 4], "seed must be the committing image");
                assert_eq!(b[100], 0xAA, "layered over the base content");
                b[8..12].copy_from_slice(&[0x22; 4]);
            })
            .unwrap();
        journal_stop(h2).unwrap();

        // Finish T1, then T2, then checkpoint: both writers' bytes land.
        f.journal.commit_staged(&t1).unwrap();
        f.journal.commit_now_for_test();
        checkpoint::checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();
        let mut final_block = [0u8; BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .read_bytes(bid as usize * BLOCK_SIZE, &mut final_block)
            .unwrap();
        assert_eq!(&final_block[..4], &[0x11; 4], "T1's bytes survive");
        assert_eq!(&final_block[8..12], &[0x22; 4], "T2's bytes applied");
        assert_eq!(final_block[100], 0xAA, "unpatched bytes keep the base");
    }

    /// The phase machine's guards: a skipped station and a wrong tid are
    /// refused with `EIO` (phase intact), the legal single-step walk goes
    /// through, and `Finished` is terminal.
    #[ktest]
    fn commit_phase_advance_guards_pipeline_order() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);

        let h = journal_start(&f.journal, 4).unwrap();
        let tid = h.tid();
        {
            let mut st = f.journal.state_write();
            let txn = st.running.as_mut().unwrap();
            let generation = txn.capture_create(512);
            txn.apply_patch(512, generation, |b| b[..4].copy_from_slice(b"PHSE"))
                .unwrap();
        }
        journal_stop(h).unwrap();
        let _t1 = f.journal.stage_running_for_test().unwrap();

        // Skipping a station is refused, with the phase intact.
        let err = f
            .journal
            .advance_committing_phase(tid, CommitPhase::Commit)
            .unwrap_err();
        assert_eq!(err.error(), Errno::EIO);
        assert_eq!(
            f.journal.committing_for_test(),
            Some((tid, CommitPhase::Locked))
        );
        // A wrong tid is refused even for the legal successor.
        let err = f
            .journal
            .advance_committing_phase(tid.next(), CommitPhase::Flush)
            .unwrap_err();
        assert_eq!(err.error(), Errno::EIO);

        // The legal single-step walk goes through…
        for to in [
            CommitPhase::Flush,
            CommitPhase::Commit,
            CommitPhase::CommitRecord,
            CommitPhase::Finished,
        ] {
            f.journal.advance_committing_phase(tid, to).unwrap();
            assert_eq!(f.journal.committing_for_test(), Some((tid, to)));
        }
        // …and Finished is terminal.
        let err = f
            .journal
            .advance_committing_phase(tid, CommitPhase::Finished)
            .unwrap_err();
        assert_eq!(err.error(), Errno::EIO);

        f.journal.clear_committing(tid);
        assert_eq!(f.journal.committing_for_test(), None);
    }

    /// Commits stay strictly serial under the pipeline (one slot), so
    /// waiting on an already-committed tid returns immediately even while a
    /// successor transaction is running with live captures.
    #[ktest]
    fn log_wait_commit_returns_for_earlier_tid_while_successor_runs() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);

        let h1 = journal_start(&f.journal, 4).unwrap();
        let t1_tid = h1.tid();
        get_write_access(Some(&h1), 513)
            .unwrap()
            .patch(|b| b[..4].copy_from_slice(&[0x11; 4]))
            .unwrap();
        journal_stop(h1).unwrap();
        let t1 = f.journal.stage_running_for_test().unwrap();
        f.journal.commit_staged(&t1).unwrap();

        // T2 running with a live capture (not committable — handle open).
        let h2 = journal_start(&f.journal, 4).unwrap();
        get_write_access(Some(&h2), 514)
            .unwrap()
            .patch(|b| b[..4].copy_from_slice(&[0x22; 4]))
            .unwrap();
        // The already-committed T1 must not be gated on T2's fate.
        f.journal.log_wait_commit(t1_tid).unwrap();
        journal_stop(h2).unwrap();
    }

    /// `sync(2)`'s durability probe covers the committing slot,
    /// deterministically (the earlier threaded form let the finisher outrun
    /// the probe on SMP and degrade to the empty-journal path while still
    /// passing): with `running` empty and T1 staged mid-flight, the probe's
    /// target IS T1's tid — the wait `commit_and_wait_running` enters;
    /// returning early would claim durability for a commit record not yet
    /// on disk. Once T1 commits and retires, the target clears and the
    /// production probe returns without waiting. (The wait primitive itself
    /// — blocking until `committed_tid` reaches the target — is covered by
    /// `commit_thread_end_to_end`.)
    #[ktest]
    fn sync_probe_targets_the_committing_slot() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);

        let h1 = journal_start(&f.journal, 4).unwrap();
        let t1_tid = h1.tid();
        get_write_access(Some(&h1), 515)
            .unwrap()
            .patch(|b| b[..4].copy_from_slice(&[0x11; 4]))
            .unwrap();
        // Captures present in `running`: the probe targets the running tid.
        assert_eq!(f.journal.sync_durability_target().unwrap(), Some(t1_tid));
        journal_stop(h1).unwrap();

        // Slot known-occupied, `running` empty: the probe targets the
        // committing tid.
        let t1 = f.journal.stage_running_for_test().unwrap();
        assert!(f.journal.state_read().running.is_none());
        assert_eq!(f.journal.sync_durability_target().unwrap(), Some(t1_tid));

        // Finish T1: no target remains and the production probe returns
        // with T1 durable.
        f.journal.commit_staged(&t1).unwrap();
        assert_eq!(f.journal.sync_durability_target().unwrap(), None);
        f.journal.commit_and_wait_running().unwrap();
        assert!(f.journal.committed_tid().geq(t1_tid));
    }

    /// The probe's empty path, split from the occupied-slot case: nothing
    /// running and nothing committing (and no abort) is trivially durable —
    /// `sync(2)` returns at once, with no tid to wait on.
    #[ktest]
    fn sync_probe_on_empty_journal_is_trivially_durable() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);
        assert_eq!(f.journal.sync_durability_target().unwrap(), None);
        f.journal.commit_and_wait_running().unwrap();
    }

    /// The committer's occupied-slot wait shape (P7c-1 review MINOR): with a
    /// commit mid-flight in the slot AND a committable running transaction
    /// behind it, the poll keeps WAITING (`None`) instead of reporting
    /// `Commit` into a take that refuses — otherwise the loop busy-spins
    /// full checkpoint passes against a stuck commit. Retiring the slot
    /// (which wakes the trigger, `clear_committing`) re-arms the poll.
    /// Occupied-at-poll is unreachable in production today — the committer
    /// is the only production stager while running and retires its own
    /// stage before re-polling — so this pins the defensive shape, not a
    /// reachable livelock.
    #[ktest]
    fn committer_waits_out_an_occupied_slot_instead_of_spinning() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);
        let weak = Arc::downgrade(&f.journal);

        // T1 staged mid-flight (slot occupied)…
        let h1 = journal_start(&f.journal, 4).unwrap();
        let t1_tid = h1.tid();
        get_write_access(Some(&h1), 516)
            .unwrap()
            .patch(|b| b[..4].copy_from_slice(&[0x11; 4]))
            .unwrap();
        journal_stop(h1).unwrap();
        let t1 = f.journal.stage_running_for_test().unwrap();
        // …and T2 committable behind it (commit-forced: under group commit
        // an untriggered transaction keeps batching instead).
        let h2 = journal_start(&f.journal, 4).unwrap();
        let t2_tid = h2.tid();
        get_write_access(Some(&h2), 517)
            .unwrap()
            .patch(|b| b[..4].copy_from_slice(&[0x22; 4]))
            .unwrap();
        journal_stop(h2).unwrap();
        f.journal.request_commit_for(t2_tid);

        // The poll waits rather than handing the loop a refusing advance…
        assert!(Journal::poll_commit_action(&weak, Some(t2_tid)).is_none());
        // …and the advance-time guard itself refuses, both transactions intact.
        assert!(f.journal.stage_running_for_test().is_none());
        assert_eq!(
            f.journal.committing_for_test(),
            Some((t1_tid, CommitPhase::Locked))
        );
        assert!(f.journal.state_read().running.is_some());

        // Retiring the slot (which wakes the trigger) re-arms the poll: T2
        // is now the pick.
        f.journal.commit_staged(&t1).unwrap();
        assert!(matches!(
            Journal::poll_commit_action(&weak, Some(t2_tid)),
            Some(CommitAction::Advance)
        ));
    }

    // --- P7a-4: the D-4 mount-time journal feature upgrade. ---

    /// Reads the raw on-disk journal superblock of a [`journaled_fixture`].
    fn read_fixture_journal_super(f: &JournaledFixture) -> RawJournalSuperblock {
        f.fixture
            .disk
            .segment()
            .read_val(JOURNAL_START_BLOCK as usize * BLOCK_SIZE)
            .unwrap()
    }

    /// Overwrites the on-disk journal superblock of a [`journaled_fixture`].
    fn write_fixture_journal_super(f: &JournaledFixture, raw: &RawJournalSuperblock) {
        f.fixture
            .disk
            .segment()
            .write_val(JOURNAL_START_BLOCK as usize * BLOCK_SIZE, raw)
            .unwrap();
    }

    /// D-4 upgrade, the acting row: a metadata_csum fs over a featureless
    /// clean journal flips it to csum_v3 (+64bit iff the fs is 64bit), names
    /// crc32c, and stamps a valid `s_checksum` — and the rewritten superblock
    /// re-parses through the production `load_geometry` with the new tag
    /// layout and a seed, exactly the reload the mount performs.
    #[ktest]
    fn upgrade_stamps_csum_v3_on_featureless_journal() {
        let f = journaled_fixture(16, 1, 1);
        assert_eq!(read_fixture_journal_super(&f).s_feature_incompat.get(), 0);

        let rewritten = upgrade_journal_on_mount(
            f.journal.geometry(),
            f.fixture.ext4.block_device().as_ref(),
            JournalUpgradeNeeds {
                fs_has_metadata_csum: true,
                fs_is_64bit: false,
            },
        )
        .unwrap();
        assert!(rewritten.upgraded);

        let sb = read_fixture_journal_super(&f);
        assert_eq!(sb.s_feature_incompat.get(), INCOMPAT_CSUM_V3);
        assert_eq!(sb.s_checksum_type, JBD2_CRC32C_CHKSUM);
        assert_eq!(sb.s_checksum.get(), sb.checksum());

        // The mount reloads the geometry from the upgraded bytes: admission
        // passes, the tag layout is csum_v3's, and the seed exists.
        let geo = load_geometry(&f.fixture.ext4).unwrap().unwrap();
        assert_eq!(
            geo.tag_layout(),
            TagLayout::from_features(INCOMPAT_CSUM_V3).unwrap()
        );
        assert!(geo.csum_seed().is_some());
    }

    /// D-4 upgrade on a 64bit fs adds `INCOMPAT_64BIT` alongside csum_v3
    /// (fs/ext4/super.c:4909-4912).
    #[ktest]
    fn upgrade_adds_64bit_iff_fs_is_64bit() {
        let f = journaled_fixture(16, 1, 1);
        let rewritten = upgrade_journal_on_mount(
            f.journal.geometry(),
            f.fixture.ext4.block_device().as_ref(),
            JournalUpgradeNeeds {
                fs_has_metadata_csum: true,
                fs_is_64bit: true,
            },
        )
        .unwrap();
        assert!(rewritten.upgraded);

        let sb = read_fixture_journal_super(&f);
        assert_eq!(
            sb.s_feature_incompat.get(),
            INCOMPAT_CSUM_V3 | INCOMPAT_64BIT
        );
        assert_eq!(sb.s_checksum.get(), sb.checksum());
        let geo = load_geometry(&f.fixture.ext4).unwrap().unwrap();
        assert_eq!(
            geo.tag_layout(),
            TagLayout::from_features(INCOMPAT_CSUM_V3 | INCOMPAT_64BIT).unwrap()
        );
    }

    /// Every "honor, don't touch" row of the D-4 policy leaves the on-disk
    /// journal superblock byte-identical: a non-metadata_csum fs (even a
    /// 64bit one — the deliberate divergence from Linux), a journal already
    /// carrying features (64bit-only or csum_v3, with or without fs
    /// metadata_csum), a dirty (`s_start != 0`) journal, and a V1-blocktype
    /// superblock that cannot carry features at all.
    #[ktest]
    fn upgrade_honors_journal_per_policy_table() {
        let f = journaled_fixture(16, 1, 1);
        let device = f.fixture.ext4.block_device();
        let geo = f.journal.geometry();

        let assert_untouched = |needs: JournalUpgradeNeeds, why: &str| {
            let before = read_fixture_journal_super(&f);
            let rewritten = upgrade_journal_on_mount(geo, device.as_ref(), needs).unwrap();
            assert!(!rewritten.upgraded, "{why}");
            let after = read_fixture_journal_super(&f);
            assert_eq!(after.as_bytes(), before.as_bytes(), "{why}");
        };

        // Non-metadata_csum fs: untouched, even when the fs is 64bit.
        assert_untouched(
            JournalUpgradeNeeds {
                fs_has_metadata_csum: false,
                fs_is_64bit: true,
            },
            "non-csum fs must leave the v0 journal frozen",
        );

        // A journal already carrying an INCOMPAT feature (64bit-only): honored.
        let mut raw = journal_super(16, 1, 1, 0);
        raw.s_feature_incompat = Be32::new(INCOMPAT_64BIT);
        write_fixture_journal_super(&f, &raw);
        assert_untouched(
            JournalUpgradeNeeds {
                fs_has_metadata_csum: true,
                fs_is_64bit: false,
            },
            "a Linux-touched journal keeps its features",
        );

        // A csum_v3 journal on a fs WITHOUT metadata_csum (tune2fs oddity):
        // honored as-is.
        let mut raw = journal_super(16, 1, 1, 0);
        raw.s_feature_incompat = Be32::new(INCOMPAT_CSUM_V3);
        raw.s_checksum_type = JBD2_CRC32C_CHKSUM;
        raw.s_checksum = Be32::new(raw.checksum());
        write_fixture_journal_super(&f, &raw);
        assert_untouched(
            JournalUpgradeNeeds {
                fs_has_metadata_csum: false,
                fs_is_64bit: false,
            },
            "journal csum features without fs metadata_csum are honored",
        );

        // A dirty featureless journal (s_start != 0 without the fs recovery
        // flag — an anomaly): never re-shaped underfoot.
        write_fixture_journal_super(&f, &journal_super(16, 1, 1, 3));
        assert_untouched(
            JournalUpgradeNeeds {
                fs_has_metadata_csum: true,
                fs_is_64bit: false,
            },
            "a non-empty log must never change tag geometry",
        );

        // A V1-blocktype superblock cannot carry feature bits
        // (jbd2_format_support_feature).
        let mut raw = journal_super(16, 1, 1, 0);
        raw.header.h_blocktype = Be32::new(format::BLOCKTYPE_SUPERBLOCK_V1);
        write_fixture_journal_super(&f, &raw);
        assert_untouched(
            JournalUpgradeNeeds {
                fs_has_metadata_csum: true,
                fs_is_64bit: false,
            },
            "a V1 superblock has no feature fields to set",
        );
    }

    // --- P7c-2: T_LOCKED — the locking seat, the drain, and the barrier. ---

    /// The drain machinery end to end: a due transaction with an open
    /// handle is PARKED (not committed), its own handle keeps capturing and
    /// patching into the seat (tid-keyed funnels) and the read funnel
    /// serves the seat's captures; the last close completes the drain, the
    /// next advance stages + commits; and — the §10.6 pin-window recipe
    /// extended — a successor capture then seeds the drained transaction's
    /// stashed image, never the lagging device.
    #[ktest]
    fn due_transaction_with_open_handles_parks_drains_and_stages() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);
        let device = f.fixture.ext4.block_device().clone();

        let h1 = journal_start(&f.journal, 4).unwrap();
        let tid = h1.tid();
        get_write_access(Some(&h1), 520)
            .unwrap()
            .patch(|b| b[..4].copy_from_slice(&[0x11; 4]))
            .unwrap();
        let h2 = journal_start(&f.journal, 4).unwrap();
        journal_stop(h1).unwrap();

        // Force the trigger with h2 still open: the advance PARKS the
        // transaction in the locking seat (T_LOCKED) instead of committing.
        f.journal.request_commit_for(tid);
        assert!(!f.journal.commit_if_due_for_test());
        assert_eq!(f.journal.locking_state_for_test(), Some((tid, 1, 1)));
        assert!(f.journal.state_read().running.is_none());

        // The locked transaction's OWN handle keeps working: a fresh
        // capture + patch land in the seat through the tid-keyed funnels…
        get_write_access(Some(&h2), 521)
            .unwrap()
            .patch(|b| b[..4].copy_from_slice(&[0x22; 4]))
            .unwrap();
        assert_eq!(f.journal.locking_state_for_test(), Some((tid, 1, 2)));
        // …and the metadata read funnel serves the seat's captures (readers
        // take no handle, so they run concurrently with a drain).
        let served = read_metadata_block(Some(f.journal.as_ref()), device.as_ref(), 520).unwrap();
        assert_eq!(&served[..4], &[0x11; 4]);

        // The last close completes the drain; the next advance stages and
        // commits the seat.
        journal_stop(h2).unwrap();
        assert!(f.journal.commit_if_due_for_test());
        assert!(f.journal.locking_state_for_test().is_none());
        assert_eq!(f.journal.committed_tid(), tid);

        // Pin window (§10.6): no checkpoint ran, so the device still lags.
        // The successor's capture must seed the drained transaction's
        // stashed image (both its blocks), not the device zeros.
        let h3 = journal_start(&f.journal, 4).unwrap();
        assert_eq!(h3.tid(), tid.next());
        get_write_access(Some(&h3), 520)
            .unwrap()
            .patch(|b| {
                assert_eq!(&b[..4], &[0x11; 4], "seed = the drained txn's image");
                b[8..12].copy_from_slice(&[0x33; 4]);
            })
            .unwrap();
        journal_stop(h3).unwrap();
    }

    /// The locked barrier: while a predecessor drains in the locking seat,
    /// `journal_start` neither joins it nor creates a successor — it blocks
    /// (jbd2 `wait_transaction_locked`) and joins the successor only once
    /// the seat staged.
    #[ktest]
    fn journal_start_blocks_at_the_locked_barrier() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);

        let h1 = journal_start(&f.journal, 4).unwrap();
        let tid = h1.tid();
        get_write_access(Some(&h1), 522)
            .unwrap()
            .patch(|b| b[..4].copy_from_slice(&[0x11; 4]))
            .unwrap();
        let h2 = journal_start(&f.journal, 4).unwrap();
        journal_stop(h1).unwrap();
        f.journal.request_commit_for(tid);
        assert!(!f.journal.commit_if_due_for_test());
        assert!(f.journal.locking_state_for_test().is_some());

        // A new operation's `journal_start` must wait out the drain.
        let joined = Arc::new(AtomicU32::new(u32::MAX));
        let joined_clone = joined.clone();
        let journal = f.journal.clone();
        let joiner = crate::thread::kernel_thread::ThreadOptions::new(move || {
            let h = journal_start(&journal, 4).unwrap();
            joined_clone.store(h.tid().get(), Ordering::Release);
            journal_stop(h).unwrap();
        })
        .spawn();

        // Give the joiner plenty of chances to (wrongly) run ahead: it must
        // not have created a successor while the seat drains.
        for _ in 0..16 {
            crate::thread::Thread::yield_now();
        }
        assert_eq!(joined.load(Ordering::Acquire), u32::MAX, "barrier held");
        assert!(f.journal.state_read().running.is_none());

        // Drain + stage: the barrier lifts at STAGING (the successor may
        // run while the commit's I/O would still be in flight).
        journal_stop(h2).unwrap();
        assert!(f.journal.commit_if_due_for_test());
        joiner.join();
        assert_eq!(joined.load(Ordering::Acquire), tid.next().get());
    }

    /// `sync(2)`'s durability probe covers the locking seat: with `running`
    /// empty and the transaction parked mid-drain, the probe's target IS
    /// its tid — returning early would claim durability for captures that
    /// are not even staged yet.
    #[ktest]
    fn sync_probe_targets_the_locking_seat() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);

        let h1 = journal_start(&f.journal, 4).unwrap();
        let tid = h1.tid();
        get_write_access(Some(&h1), 523)
            .unwrap()
            .patch(|b| b[..4].copy_from_slice(&[0x11; 4]))
            .unwrap();
        let h2 = journal_start(&f.journal, 4).unwrap();
        journal_stop(h1).unwrap();
        f.journal.request_commit_for(tid);
        assert!(!f.journal.commit_if_due_for_test());
        assert!(f.journal.locking_state_for_test().is_some());

        assert_eq!(f.journal.sync_durability_target().unwrap(), Some(tid));

        journal_stop(h2).unwrap();
        assert!(f.journal.commit_if_due_for_test());
        assert_eq!(f.journal.sync_durability_target().unwrap(), None);
    }

    /// The unmount flush commits a transaction left parked in the locking
    /// seat (the committer joined mid-drain): staged oldest-first, committed,
    /// checkpointed — the on-disk log ends clean and the bytes land.
    #[ktest]
    fn flush_on_unmount_commits_the_parked_transaction() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);

        let h1 = journal_start(&f.journal, 4).unwrap();
        let tid = h1.tid();
        get_write_access(Some(&h1), 524)
            .unwrap()
            .patch(|b| b[..4].copy_from_slice(&[0x44; 4]))
            .unwrap();
        let h2 = journal_start(&f.journal, 4).unwrap();
        journal_stop(h1).unwrap();
        f.journal.request_commit_for(tid);
        assert!(!f.journal.commit_if_due_for_test());
        // The drain completes, but no committer runs again before unmount.
        journal_stop(h2).unwrap();
        assert!(f.journal.locking_state_for_test().is_some());

        f.journal.flush_on_unmount().unwrap();

        assert_eq!(f.journal.committed_tid(), tid);
        assert!(f.journal.locking_state_for_test().is_none());
        assert_eq!(f.journal.state_read().tail_block, None, "log left clean");
        let mut buf = [0u8; BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .read_bytes(524 * BLOCK_SIZE, &mut buf)
            .unwrap();
        assert_eq!(&buf[..4], &[0x44; 4], "checkpointed to the final location");
    }
}
