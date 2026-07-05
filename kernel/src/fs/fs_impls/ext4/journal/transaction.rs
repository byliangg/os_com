// SPDX-License-Identifier: MPL-2.0

//! In-memory transaction machinery (jbd2 `transaction_t` / `handle_t`).
//!
//! Phase 4 journals metadata. Linux jbd2 keeps one buffer per metadata block —
//! the working copy *is* the logged copy — but our ext4 has no metadata buffer
//! cache: it serializes typed [`Dirty`](super::super::utils::Dirty) objects
//! (`Dirty<IdBitmap/BlockGroupDesc/SuperBlock/RawInode>`) lazily at sync time.
//! To journal, each modified metadata block's whole-block **after-image** must
//! be captured into the running transaction so the commit pipeline (a later
//! task) can write it to the log.
//!
//! # Model A: op-time capture
//!
//! Each running [`Transaction`] holds a map `physical block# → after-image
//! buffer`. The flow, mirroring jbd2's `get_write_access` / `get_create_access`
//! / `dirty_metadata`:
//!
//! 1. Before touching an *existing* metadata block, [`Transaction::capture_write`]
//!    seeds its buffer from the block's newest committed-but-un-checkpointed
//!    after-image if one is retained (see [`UncheckpointedImage`]), else from
//!    the device (jbd2 `get_write_access`).
//! 2. Before populating a *freshly allocated* metadata block,
//!    [`Transaction::capture_create`] seeds a zeroed buffer — no device read is
//!    needed (jbd2 `get_create_access`).
//! 3. After the operation mutates its typed object, [`Transaction::apply_patch`]
//!    patches the modified bytes into the captured buffer (jbd2
//!    `dirty_metadata`). Because sub-objects sharing one block patch the *same*
//!    buffer, their after-images accumulate correctly.
//!
//! # jbd2 correspondence
//!
//! - [`Transaction`] ≈ `transaction_t`: an in-memory transaction accumulating
//!   the metadata blocks it will commit, with the open-handle / credit
//!   bookkeeping (`t_updates` / `t_outstanding_credits`). Its lifecycle state
//!   (`t_state`) is residency, not a field: running in
//!   [`JournalState::running`](super::JournalState), mid-commit in
//!   [`JournalState::committing`](super::JournalState) (whose [`CommitPhase`]
//!   walks the pipeline), retired to its retained after-images in
//!   [`JournalState::uncheckpointed`](super::JournalState).
//! - [`Handle`] ≈ `handle_t`: one open unit of work against a transaction,
//!   holding a credit reservation, obtained from [`journal_start`] and released
//!   by [`journal_stop`].
//! - [`MetaBuffer`] ≈ the per-block `journal_head` after-image bytes.
//!
//! # Locking
//!
//! [`journal_start`] / [`journal_stop`] / [`journal_extend`] / [`journal_restart`]
//! take the journal state lock (`Journal::state`) for the whole operation. In the
//! global lock order this is the jbd2 handle — position ②, taken after the inode
//! inner lock ① and before the ExtentTree lock ③ (every journaled operation
//! now threads a live [`Handle`] through the metadata funnels in this order).

// Most of this module is live in non-ktest since Int-B: every journaled
// metadata operation opens a handle (`Ext4::begin_op` → `journal_start`,
// closed by `OpHandle::drop` → `journal_stop`), the capture half feeds the
// metadata funnels, `Handle::tid` backs fsync's `sync_tid`, and the
// [`CommitPhase`] walk is driven by the production commit pipeline (P7c-1).
// Four members are still dead in a non-ktest build: `journal_extend` and
// `journal_restart` (mid-op credit growth — ops use fixed conservative credits
// until P7d's precise accounting), plus the `nr_ordered_data` and
// `Handle::credits` inspection accessors (read only by ktest assertions). Each
// carries its OWN narrow `#[cfg_attr(not(ktest), expect(dead_code))]` rather
// than a module-wide marker, so future dead code surfaces instead of being
// absorbed silently.

use ostd::timer::{Jiffies, TIMER_FREQ};

use super::{
    super::{inode::Inode, prelude::*},
    Journal, JournalState, Tid,
    format::TagLayout,
    revoke::RevokeTable,
};

/// How long a running transaction may age before the commit thread commits it
/// (jbd2 `j_commit_interval`), in jiffies: 5 seconds, Linux's
/// `JBD2_DEFAULT_MAX_COMMIT_AGE` (include/linux/jbd2.h; ext4 mounts set
/// `j_commit_interval = HZ * JBD2_DEFAULT_MAX_COMMIT_AGE`, fs/ext4/super.c:5242).
/// This bounds the crash-loss window of un-synced work under group commit —
/// matching Linux's data=ordered semantics exactly, so anything an
/// application did not fsync may lose up to this much recent work in a crash
/// (plus the drain of then-open handles), never anything it did fsync.
const COMMIT_INTERVAL_JIFFIES: u64 = 5 * TIMER_FREQ;

/// One ordered-data registration: the pages an operation's new metadata will
/// reference, flushed by commit with no inode lock (see
/// [`Transaction::register_ordered_data`]).
struct OrderedData {
    /// Liveness gate only — never upgraded for access.
    inode: Weak<Inode>,
    pages: PageCache,
    len: usize,
}

/// The mint identity of one capture within its transaction (jbd2 gets this
/// from `journal_head` pointer identity; Model A's capture map is keyed by
/// `(transaction, block#)` alone, which a re-capture REUSES).
///
/// A block can be captured, forgotten (the capture cancelled by a free), and
/// captured again inside one transaction — the block reallocated to a new
/// owner. The map key is then identical, but the image is a different
/// object: a [`WriteAccess`](super::WriteAccess) credential minted against
/// the old capture must not patch the new one (it would write the old
/// owner's bytes into the new owner's image). Each capture therefore carries
/// the generation it was minted under, the credential stores it, and
/// [`Transaction::apply_patch`] refuses a mismatch with `EIO`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CaptureGeneration(u64);

/// One captured metadata block of a running transaction: its after-image
/// buffer plus the [`CaptureGeneration`] it was minted under.
struct Capture {
    generation: CaptureGeneration,
    buffer: MetaBuffer,
}

/// A captured whole-block after-image for one metadata block, held in a running
/// transaction until commit writes it to the log. Model A (op-time capture):
/// seeded by `get_write_access` (from the newest retained un-checkpointed
/// image, else the device) or `get_create_access` (zeros), then patched in
/// place as the operation modifies its typed metadata.
pub(super) struct MetaBuffer {
    data: Box<[u8; BLOCK_SIZE]>,
}

impl MetaBuffer {
    /// A freshly captured buffer of all zeros (a newly allocated metadata block,
    /// whose prior device content is meaningless).
    fn zeroed() -> Self {
        Self {
            data: Box::new([0u8; BLOCK_SIZE]),
        }
    }

    /// The captured after-image bytes.
    fn as_bytes(&self) -> &[u8] {
        self.data.as_slice()
    }

    /// Returns the captured after-image as a whole block — the commit
    /// pipeline logs (and, under csum v2/v3, checksums) full blocks, so it
    /// takes the width-carrying type rather than re-checking a slice length.
    fn as_block(&self) -> &[u8; BLOCK_SIZE] {
        &self.data
    }

    /// The after-image bytes, for seeding from the device or patching in place.
    fn as_mut(&mut self) -> &mut [u8] {
        self.data.as_mut_slice()
    }

    /// An owned copy of this after-image, for retaining a committing
    /// transaction's images past its consumption (see [`UncheckpointedImage`]).
    fn duplicate(&self) -> Self {
        Self {
            data: Box::new(*self.data),
        }
    }
}

/// A committed transaction's after-image of one metadata block, retained in
/// [`JournalState::uncheckpointed`](super::JournalState) until the checkpoint
/// that writes it to its final location completes.
///
/// While a block has such an image, the image — not the device — is the block's
/// newest content: the device lags until checkpoint. A later transaction's
/// `get_write_access` therefore seeds from it (see
/// [`Transaction::capture_write`]); seeding from the device inside that window
/// would resurrect the pre-image for every sub-block object the new transaction
/// does not itself patch, and checkpoint (tid order, newest wins) would clobber
/// the neighbors' committed writes — the B-1 shared-block stale-seed class
/// (inode-table blocks: silent neighbor-inode data loss). This is the invariant
/// jbd2 gets for free from the kernel buffer cache (one canonical `buffer_head`
/// per block); Model A's capture buffers are per-transaction, so the journal
/// retains the newest committed image itself.
//
// Visible at the `ext4` level for the same reason as [`Transaction`]: it is a
// value of the equally-visible `JournalState`'s map.
pub(in crate::fs::fs_impls::ext4) struct UncheckpointedImage {
    /// The committing transaction's tid — compared against the checkpointed-up-to
    /// tid to decide eviction (a newer commit's image must survive an older
    /// checkpoint pass).
    tid: Tid,
    /// The committed after-image bytes.
    image: MetaBuffer,
}

impl UncheckpointedImage {
    /// Returns the retained after-image bytes (the seed for a later capture).
    pub(super) fn image_bytes(&self) -> &[u8] {
        self.image.as_bytes()
    }

    /// Returns whether this image is checkpointed once everything up to
    /// `committed_tid` has been applied to its final location — i.e. whether
    /// eviction is due.
    pub(super) fn is_checkpointed_by(&self, committed_tid: Tid) -> bool {
        committed_tid.geq(self.tid)
    }
}

/// The committing transaction's pipeline phase — the observable half of the
/// jbd2 transaction lifecycle (`transaction_t.t_state`).
///
/// A transaction's *state* is encoded by where it resides, so no in-band
/// state field can drift out of step with residency:
///
/// - `T_RUNNING` ≡ residency in [`JournalState::running`](super::JournalState)
///   (accepting handles and captures);
/// - `T_LOCKED`-with-open-handles ≡ residency in
///   [`JournalState::locking`](super::JournalState) (accepting no NEW
///   handles; its own still-open handles keep patching until they close —
///   the drain, P7c-2);
/// - the five phases below ≡ residency in
///   [`JournalState::committing`](super::JournalState) (the slot's `phase`
///   field, advanced by the commit pipeline under the state lock);
/// - past retirement, only the transaction's after-images remain, in
///   [`JournalState::uncheckpointed`](super::JournalState), until checkpoint.
///
/// jbd2 mapping: the locking seat above is `T_LOCKED`'s draining window;
/// the slot's `Locked` phase is its drained tail end (+ the zero-width
/// `T_SWITCH` — staging happens only once every handle has closed, so
/// there is nothing left to switch), `Flush` ≈ `T_FLUSH`, `Commit` ≈ `T_COMMIT`,
/// `CommitRecord` ≈ `T_COMMIT_DFLUSH` + `T_COMMIT_JFLUSH` **collapsed** (one
/// block device and synchronous barriers: the data-device and
/// journal-device flushes of the commit record are one indivisible step
/// here), `Finished` ≈ `T_FINISHED` (`T_COMMIT_CALLBACK` has no equivalent —
/// there are no commit callbacks). Phases advance strictly one step
/// ([`next_in_pipeline`](Self::next_in_pipeline)); an out-of-order advance
/// is refused with `EIO` ([`Journal::advance_committing_phase`](super::Journal)),
/// failing that commit loudly instead of running the pipeline out of order.
//
// Visible at the `ext4` level for the same reason as [`Transaction`]: it is
// carried by the equally-visible `JournalState`'s committing slot (and its
// ktest inspection accessor); every constructor/consumer stays inside the
// journal module.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::fs::fs_impls::ext4) enum CommitPhase {
    /// Taken out of `running`, pre-I/O (`T_LOCKED`): the fit guard and chain
    /// building run here, including the inline tail-drain window of
    /// [`Journal::commit_or_drain_tail`](super::Journal) — nothing of the
    /// transaction has been written.
    Locked,
    /// Flushing the transaction's ordered data to its final locations
    /// (`T_FLUSH`, commit step 0).
    Flush,
    /// Writing the log: revoke blocks + the descriptor chain, and the
    /// pre-commit barrier (`T_COMMIT`, steps 1–2).
    Commit,
    /// Writing the commit block and making it durable (steps 3–5).
    CommitRecord,
    /// The commit block is durable and step 6 published the in-memory state;
    /// awaiting retirement from the slot (`T_FINISHED`).
    Finished,
}

impl CommitPhase {
    /// The phase that legally follows `self` in the commit pipeline, or
    /// `None` for the terminal [`Finished`](Self::Finished). The single
    /// definition of the legal order that
    /// [`Journal::advance_committing_phase`](super::Journal) enforces.
    pub(super) fn next_in_pipeline(self) -> Option<Self> {
        match self {
            Self::Locked => Some(Self::Flush),
            Self::Flush => Some(Self::Commit),
            Self::Commit => Some(Self::CommitRecord),
            Self::CommitRecord => Some(Self::Finished),
            Self::Finished => None,
        }
    }
}

/// An in-memory transaction accumulating metadata after-images (jbd2
/// `transaction_t`).
///
/// The [`Journal`] holds at most one of these as its running transaction and
/// at most one as its committing transaction (the two-transaction pipeline:
/// while the taken one commits, a new running one accepts captures). It
/// records the captured after-images plus the open-handle and credit
/// bookkeeping used to bound its size. From the moment it is staged for
/// commit it is **frozen**: it travels behind a shared `Arc` with no `&mut`
/// path left (the Linux `frozen_data` discipline), so the committer's I/O
/// and every state-lock-holding reader see one immutable object.
//
// Visible at the `ext4` level (`pub(in crate::fs::fs_impls::ext4)`) so it can be
// a field of the equally-visible `JournalState`.
pub(in crate::fs::fs_impls::ext4) struct Transaction {
    /// This transaction's id (`t_tid`).
    tid: Tid,
    /// When this transaction's age makes it due for commit (jbd2
    /// `t_expires = jiffies + j_commit_interval`), fixed at creation. The
    /// commit thread's age trigger compares it against the current jiffies
    /// ([`is_expired_at`](Self::is_expired_at)); it never changes, so the
    /// armed sleep deadline stays valid for the transaction's whole life.
    expires_at: Jiffies,
    /// Number of open handles (`journal_start` not yet `journal_stop`'d;
    /// `t_updates`).
    t_updates: usize,
    /// Sum of live handles' reserved credits — the max blocks they may dirty
    /// (`t_outstanding_credits`).
    outstanding_credits: usize,
    /// The captured after-images, keyed by physical block number. An ordered map
    /// so commit writes tags in a deterministic block order.
    metadata: BTreeMap<Ext4Bid, Capture>,
    /// The next [`CaptureGeneration`] to mint — bumped whenever a block gains
    /// a FRESH capture (an idempotent re-capture keeps the existing one), so
    /// a forget-then-recapture of the same block is distinguishable to the
    /// outstanding credentials of the old capture.
    next_capture_generation: u64,
    /// The blocks this transaction revoked (jbd2's *running* revoke table,
    /// revoke.c): each was freed under this transaction via
    /// [`forget`](super::revoke::forget) after having been (or being eligible
    /// to be) journaled. `running.take()` at commit is jbd2's
    /// `journal_switch_revoke_table()` — the set rides into the commit
    /// pipeline, which publishes it to the journal's committed-revoke memory
    /// ([`stash_revokes`](Self::stash_revokes)) once the commit block is
    /// durable. Capturing a revoked block again cancels its record
    /// (`jbd2_journal_cancel_revoke`; see [`capture_write`](Self::capture_write)).
    revoked: BTreeSet<Ext4Bid>,
    /// The inodes whose **data** was dirtied under this transaction, for
    /// ordered-data mode (jbd2 `t_inode_list`). Keyed by ino so an inode dirtied
    /// several times in one transaction is flushed once; the value is a [`Weak`]
    /// so a running transaction never keeps an inode alive (a dropped inode's
    /// data is no longer this transaction's concern). At commit each is upgraded
    /// and its dirty data flushed to its final location before any log block is
    /// written.
    ordered_data: BTreeMap<Ext4Ino, OrderedData>,
}

impl Transaction {
    /// Creates a fresh running transaction with id `tid` and no captured
    /// blocks, due for an age-triggered commit [`COMMIT_INTERVAL_JIFFIES`]
    /// from now (jbd2 `jbd2_get_transaction` stamping `t_expires`).
    pub(super) fn new(tid: Tid) -> Self {
        let mut expires_at = Jiffies::elapsed();
        expires_at.add(COMMIT_INTERVAL_JIFFIES);
        Self {
            tid,
            expires_at,
            t_updates: 0,
            outstanding_credits: 0,
            metadata: BTreeMap::new(),
            next_capture_generation: 0,
            revoked: BTreeSet::new(),
            ordered_data: BTreeMap::new(),
        }
    }

    /// Returns whether this transaction's age makes it due for commit at
    /// `now` (jbd2 `time_after_eq(jiffies, transaction->t_expires)`).
    /// Jiffies since boot never wrap a `u64` in practice, so the comparison
    /// is plain.
    pub(super) fn is_expired_at(&self, now: Jiffies) -> bool {
        now.as_u64() >= self.expires_at.as_u64()
    }

    /// Returns the time remaining until this transaction's age deadline (zero
    /// once expired) — what the commit thread arms its sleep timeout with.
    pub(super) fn until_expiry(&self, now: Jiffies) -> Duration {
        Jiffies::new(self.expires_at.as_u64().saturating_sub(now.as_u64())).as_duration()
    }

    /// Returns the log blocks this transaction's captured work serializes into
    /// so far — its captured metadata blocks plus its whole revoke blocks under
    /// `layout` — the measure the batch-size commit trigger compares against
    /// [`Journal::batch_trigger_credits`](super::Journal). Deliberately NOT
    /// `outstanding_credits`: reservations are fixed conservative worst
    /// cases until P7d's precise accounting, so counting them would trigger
    /// commits an order of magnitude early; captured work is the
    /// transaction's real, already-incurred footprint.
    pub(super) fn batch_footprint(&self, layout: TagLayout) -> usize {
        self.nr_metadata_blocks() + self.nr_revoke_blocks(layout)
    }

    /// Test helper: back-dates the age deadline so the transaction is
    /// expired at every future `now` — the deterministic form of the age
    /// trigger for ktest (real timer waits are non-deterministic there).
    #[cfg(ktest)]
    pub(super) fn force_expire_for_test(&mut self) {
        self.expires_at = Jiffies::new(0);
    }

    /// Mints the generation for a FRESH capture (see [`CaptureGeneration`]).
    fn mint_capture_generation(&mut self) -> CaptureGeneration {
        let generation = CaptureGeneration(self.next_capture_generation);
        self.next_capture_generation += 1;
        generation
    }

    /// This transaction's id.
    pub(super) fn tid(&self) -> Tid {
        self.tid
    }

    /// The number of open handles against this transaction.
    pub(super) fn nr_updates(&self) -> usize {
        self.t_updates
    }

    /// The number of distinct metadata blocks captured so far.
    pub(super) fn nr_metadata_blocks(&self) -> usize {
        self.metadata.len()
    }

    /// Returns the worst-case metadata log blocks this transaction would occupy
    /// with `extra` more credits reserved: its already-captured metadata plus every
    /// reservation's full worst case (`outstanding_credits + extra`, each
    /// reservation counted at its whole credit until its handle closes). This
    /// is the credit base BOTH reservation gates share — `check_capacity`
    /// (against [`Journal::max_credits`](super::Journal::max_credits)) and the
    /// log-space gate ([`journal_start`], through
    /// [`Journal::reservation_footprint`](super::Journal::reservation_footprint))
    /// — computed in ONE place so the two cannot diverge. It EXCLUDES revoke
    /// blocks: a whole revoke block is a log block carrying no descriptor tag
    /// ([`nr_revoke_blocks`](Self::nr_revoke_blocks)), charged separately by
    /// each gate exactly once.
    pub(super) fn reserved_metadata_blocks(&self, extra: usize) -> usize {
        self.nr_metadata_blocks() + self.outstanding_credits + extra
    }

    /// Returns whether this transaction carries a durable obligation recorded
    /// on the transaction object itself: a captured metadata after-image or a revoke
    /// record. A caller deciding "is this a non-empty transaction a commit must
    /// retire" must ALSO consult the state's pinned freed runs for this tid
    /// ([`JournalState::committable_running`](super::JournalState)) — a
    /// plain-data free pins its run there without recording a revoke here — so
    /// this is only the transaction-local half of that gate.
    pub(super) fn has_recorded_work(&self) -> bool {
        self.nr_metadata_blocks() > 0 || !self.revoked.is_empty()
    }

    /// Captures a freshly allocated metadata block as a zeroed after-image
    /// (jbd2 `get_create_access`): its prior device content is meaningless, so
    /// no read is needed. Idempotent — a block already captured (possibly with
    /// patches applied) is left untouched, and its existing generation is
    /// returned; a fresh capture is minted under a new [`CaptureGeneration`],
    /// which the returned value hands to the block's credential.
    ///
    /// Re-journaling a block this transaction revoked cancels the revoke
    /// (jbd2 `jbd2_journal_cancel_revoke`; revoke.c "block is revoked and
    /// then journaled"): the desired end state is the new image, which must
    /// both commit and stay applicable. Load-bearing for block reuse within
    /// one transaction — a freed-then-reallocated metadata block whose revoke
    /// survived would have its own new image suppressed at checkpoint.
    pub(super) fn capture_create(&mut self, bid: Ext4Bid) -> CaptureGeneration {
        self.revoked.remove(&bid);
        if let Some(capture) = self.metadata.get(&bid) {
            return capture.generation;
        }
        let generation = self.mint_capture_generation();
        self.metadata.insert(
            bid,
            Capture {
                generation,
                buffer: MetaBuffer::zeroed(),
            },
        );
        generation
    }

    /// Captures an existing metadata block's current content as its after-image
    /// (jbd2 `get_write_access`).
    ///
    /// The content comes from `seed` — the block's newest
    /// committed-but-un-checkpointed after-image — when one exists, because the
    /// device lags a committed transaction until its checkpoint completes;
    /// reading the device inside that window would capture stale bytes for every
    /// sub-block neighbor this transaction does not patch (see
    /// [`UncheckpointedImage`]). Without a retained image the device is
    /// authoritative and is read directly.
    ///
    /// Idempotent — if the block is already captured, does nothing (it is *not*
    /// re-seeded, so any patches already applied survive) and returns the
    /// existing generation; a fresh capture is minted under a new
    /// [`CaptureGeneration`], handed to the block's credential.
    ///
    /// Cancels any revoke this transaction holds for the block, exactly as
    /// [`capture_create`](Self::capture_create) does (jbd2
    /// `jbd2_journal_cancel_revoke`). Data writes never pass this funnel, so
    /// a revoked block reused as *data* keeps its revoke — revoke.c's third
    /// case ("revoked and then written as data: … the revoke is _not_
    /// cancelled").
    pub(super) fn capture_write(
        &mut self,
        bid: Ext4Bid,
        seed: Option<&[u8]>,
        device: &dyn BlockDevice,
    ) -> Result<CaptureGeneration> {
        self.revoked.remove(&bid);
        if let Some(capture) = self.metadata.get(&bid) {
            return Ok(capture.generation);
        }
        let mut buffer = MetaBuffer::zeroed();
        if let Some(seed) = seed {
            buffer.as_mut().copy_from_slice(seed);
        } else if device
            .read_bytes(Bid::new(bid).to_offset(), buffer.as_mut())
            .is_err()
        {
            return_errno_with_message!(Errno::EIO, "failed to read metadata block for journaling");
        }
        let generation = self.mint_capture_generation();
        self.metadata.insert(bid, Capture { generation, buffer });
        Ok(generation)
    }

    /// Retains a copy of every captured after-image in `retained`, keyed by
    /// block and tagged with this transaction's tid — called (under the journal
    /// state lock) at the moment this transaction is staged into the
    /// committing slot, so that from the very first instant a *new*
    /// transaction can exist, a capture of one of these blocks seeds from
    /// these bytes and never from the (lagging) device. This stash **is** the
    /// committing transaction's image visibility: seeders read the map, not
    /// the slot (see [`JournalState::uncheckpointed`](super::JournalState)).
    /// Checkpoint evicts the entries once the device has caught up (see
    /// [`UncheckpointedImage`]).
    ///
    /// An entry for a block this transaction re-captured simply overwrites the
    /// older image: this transaction's is the newest.
    pub(super) fn stash_uncheckpointed(
        &self,
        retained: &mut BTreeMap<Ext4Bid, UncheckpointedImage>,
    ) {
        for (bid, capture) in &self.metadata {
            retained.insert(
                *bid,
                UncheckpointedImage {
                    tid: self.tid,
                    image: capture.buffer.duplicate(),
                },
            );
        }
    }

    /// Cancels any captured after-image of `bid` and records the block in
    /// this transaction's revoke set — the running-transaction half of jbd2's
    /// `jbd2_journal_forget` + `jbd2_journal_revoke` (revoke.c "block is
    /// journaled and then revoked"): the free supersedes the pending write,
    /// so the stale capture must not commit (we take the header's
    /// cancel-the-journal-entry option), while the revoke record must — it
    /// is what stops *older* committed log images of the block from being
    /// applied over its post-free reuse. Reached through
    /// [`RevokeDuty::discharge`](super::revoke::RevokeDuty::discharge) —
    /// inside `Ext4::free_blocks`, with the bitmap clear, never at
    /// [`forget`](super::revoke::forget)-mint time — which also evicts the
    /// block's retained un-checkpointed image.
    pub(super) fn forget_block(&mut self, bid: Ext4Bid) {
        self.metadata.remove(&bid);
        self.revoked.insert(bid);
    }

    /// Copies this transaction's not-yet-published revoke set into `out` —
    /// the checkpoint pass's defer-prefix input (see
    /// [`checkpoint`](super::checkpoint::checkpoint)). While these revokes
    /// are unpublished, an older transaction's log image of any of these
    /// blocks may neither be applied (the block is already freed — and
    /// possibly reused — in memory) nor suppressed-and-retired (a crash may
    /// still erase this transaction), so the checkpoint pass must stop in
    /// front of it. Called under the journal state lock, uniformly for the
    /// running transaction and for the committing slot's (see
    /// [`checkpoint`](super::checkpoint::checkpoint)'s snapshot).
    pub(super) fn collect_unpublished_revokes(&self, out: &mut BTreeSet<Ext4Bid>) {
        out.extend(self.revoked.iter().copied());
    }

    /// Publishes this transaction's revoke set into the journal's
    /// committed-revoke memory, tagged with this transaction's tid (max-wins
    /// — "only the last one counts", revoke.c). Called by the commit pipeline
    /// under the journal state lock only after the commit block is durable;
    /// publishing at `running.take()` time instead would let the inline
    /// tail-drain checkpoint suppress older transactions' images on the
    /// authority of a commit a crash could still erase (see the
    /// [`revoke`](super::revoke) module docs on publication).
    pub(super) fn stash_revokes(&self, table: &mut RevokeTable) {
        for &bid in &self.revoked {
            table.record(bid, self.tid);
        }
    }

    /// The blocks currently in this transaction's revoke set, in block order
    /// — what the commit pipeline serializes into the transaction's revoke
    /// blocks (P7b-3), and the inspection accessor of the revoke/coverage
    /// tests.
    pub(super) fn revoked_blocks(&self) -> impl Iterator<Item = Ext4Bid> + '_ {
        self.revoked.iter().copied()
    }

    /// The log blocks this transaction's revoke set serializes into:
    /// `ceil(revokes / entries_per_block)` under `layout`'s entry geometry
    /// ([`TagLayout::revoke_entries_per_block`]) — zero for an empty set.
    /// One addend of the transaction's whole log footprint (the commit-time
    /// fit guard) and of the capacity charge ([`check_capacity`]).
    pub(super) fn nr_revoke_blocks(&self, layout: TagLayout) -> usize {
        self.revoked
            .len()
            .div_ceil(layout.revoke_entries_per_block())
    }

    /// Patches a captured block's after-image in place (jbd2 `dirty_metadata`):
    /// looks up `bid`'s buffer, verifies the caller's capture `generation`
    /// still names the live capture, and hands the bytes to `patch`.
    ///
    /// This is how a whole-block object (a bitmap:
    /// `|b| b.copy_from_slice(bitmap.as_bytes())`) or a sub-block object (a group
    /// descriptor / inode: patch only its field bytes at its offset within the
    /// block) writes its after-image. Multiple sub-objects sharing one block
    /// patch the *same* buffer, so their images accumulate correctly.
    ///
    /// Errors with `EIO` if the block was not captured first (a
    /// `dirty_metadata` without a prior `get_*_access`), or if the capture the
    /// caller's generation was minted under has since been cancelled and
    /// re-created — a forget-then-reallocate of the block within this
    /// transaction; the stale credential must not write the old owner's bytes
    /// into the new owner's image (see [`CaptureGeneration`]).
    pub(super) fn apply_patch(
        &mut self,
        bid: Ext4Bid,
        generation: CaptureGeneration,
        patch: impl FnOnce(&mut [u8]),
    ) -> Result<()> {
        let Some(capture) = self.metadata.get_mut(&bid) else {
            return_errno_with_message!(Errno::EIO, "dirty_metadata without prior get_*_access");
        };
        if capture.generation != generation {
            return_errno_with_message!(
                Errno::EIO,
                "stale write access: the block was re-captured since the credential was minted"
            );
        }
        patch(capture.buffer.as_mut());
        Ok(())
    }

    /// The captured after-image bytes for `bid`, or `None` if not captured.
    ///
    /// Backs the read side of the WAL suppression
    /// ([`read_metadata_block`](super::read_metadata_block)): while a block is
    /// captured here its newest bytes exist only in this buffer, so metadata
    /// readers must be served from it, never from the (lagging) device. Also
    /// the inspection accessor tests use.
    pub(super) fn buffer_bytes(&self, bid: Ext4Bid) -> Option<&[u8]> {
        self.metadata.get(&bid).map(|c| c.buffer.as_bytes())
    }

    /// Iterates the captured metadata blocks as `(destination block#,
    /// after-image bytes)` pairs, in ascending block-number order.
    ///
    /// The order is the [`BTreeMap`] key order, so the commit pipeline writes
    /// descriptor tags and their logged blocks in a single deterministic order
    /// (jbd2 walks `t_buffers` in insertion order; block order is equally valid
    /// and lets recovery apply each after-image to its final location).
    pub(super) fn metadata_blocks(&self) -> impl Iterator<Item = (Ext4Bid, &[u8; BLOCK_SIZE])> {
        self.metadata
            .iter()
            .map(|(&bid, capture)| (bid, capture.buffer.as_block()))
    }

    /// Registers an inode's data pages as **ordered data** of this transaction
    /// (jbd2 `jbd2_journal_inode_ranges_write` / `t_inode_list`): the pages are
    /// flushed to their final locations before this transaction's metadata is
    /// committed. Keyed by ino — a repeat registration replaces the entry with
    /// the newer snapshot (larger `len` after an extending write).
    ///
    /// The page-cache handle is cloned INTO the transaction at registration
    /// time, while the registering operation already holds the inode locks —
    /// so the commit thread flushes with **no inode lock at all**. That is
    /// load-bearing: an operation may sleep in `journal_start`'s capacity wait
    /// holding its `inner.write()`, waiting for this very commit; a flush that
    /// needed even a transient `inner.read()` would deadlock against it. The
    /// `Weak<Inode>` is never upgraded for access, only checked for liveness:
    /// a dropped (reclaimed) inode's pages are moot — and their extent-manager
    /// backend is gone, so flushing them would error, not write.
    pub(super) fn register_ordered_data(
        &mut self,
        ino: Ext4Ino,
        inode: Weak<Inode>,
        pages: PageCache,
        len: usize,
    ) {
        self.ordered_data
            .insert(ino, OrderedData { inode, pages, len });
    }

    /// Iterates the ordered-data registrations whose inode is still alive (see
    /// [`register_ordered_data`](Self::register_ordered_data) for why dead ones
    /// are skipped). The commit pipeline flushes each before writing any log
    /// block.
    pub(super) fn ordered_data(&self) -> impl Iterator<Item = (&PageCache, usize)> + '_ {
        self.ordered_data
            .values()
            .filter(|od| od.inode.strong_count() > 0)
            .map(|od| (&od.pages, od.len))
    }

    /// The number of registered ordered-data entries (including any whose inode
    /// may since have been dropped). Inspection/test accessor.
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn nr_ordered_data(&self) -> usize {
        self.ordered_data.len()
    }
}

/// An open handle against the running transaction (jbd2 `handle_t`).
///
/// Obtained from [`journal_start`] and released by [`journal_stop`]. It holds a
/// credit reservation (the max metadata blocks the caller may dirty) and a weak
/// back-reference to its [`Journal`]; the reference is weak to avoid a refcount
/// cycle with the commit thread the `Journal` owns.
pub(in crate::fs::fs_impls::ext4) struct Handle {
    /// The transaction this handle joined (`h_transaction->t_tid`).
    tid: Tid,
    /// Blocks reserved for this handle (`h_buffer_credits`).
    credits: usize,
    /// Weak back-reference to the owning journal.
    journal: Weak<Journal>,
}

impl Handle {
    /// The id of the transaction this handle joined. Exposed at the `ext4`
    /// level so the inode writeback can record it as the inode's `sync_tid`
    /// (what `fsync` waits on).
    pub(in crate::fs::fs_impls::ext4) fn tid(&self) -> Tid {
        self.tid
    }

    /// The blocks this handle has reserved.
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn credits(&self) -> usize {
        self.credits
    }

    /// Upgrades this handle's weak back-reference to its owning [`Journal`].
    ///
    /// Errors `EIO` if the journal has been dropped — impossible while an
    /// operation holds an open handle (the filesystem owns the journal), but
    /// checked so the metadata-capture funnels never dereference a dangling
    /// weak.
    pub(super) fn journal(&self) -> Result<Arc<Journal>> {
        self.journal.upgrade().ok_or_else(|| {
            Error::with_message(Errno::EIO, "journal dropped while a handle was open")
        })
    }

    /// Registers an inode's data pages as **ordered data** of this handle's
    /// transaction (jbd2 `data=ordered`): they are flushed to their final
    /// locations before the transaction's commit block is written, so recovery
    /// never replays metadata that points at blocks whose data missed the
    /// platter. Every operation that makes committed metadata reference new
    /// data blocks (allocating writes, tail-zeroing truncates, slow-symlink
    /// targets) must call this before its handle closes, passing a clone of
    /// the page cache it just wrote — the caller already holds the inode
    /// locks, and the commit thread must be able to flush without them (see
    /// [`Transaction::register_ordered_data`]).
    ///
    /// Requiring the open handle pins the running transaction, so the
    /// registration cannot land in a different transaction than the
    /// operation's own metadata captures. Takes the journal state lock
    /// transiently, exactly like the capture funnels (lock order unchanged).
    pub(in crate::fs::fs_impls::ext4) fn register_ordered_data(
        &self,
        ino: Ext4Ino,
        inode: Weak<Inode>,
        pages: PageCache,
        len: usize,
    ) -> Result<()> {
        let journal = self.journal()?;
        let mut st = journal.state_write();
        super::active_for(&mut st, self)?.register_ordered_data(ino, inode, pages, len);
        Ok(())
    }

    /// Aborts this handle's journal on a filesystem-detected inconsistency —
    /// the minimal Linux `ext4_error` → `jbd2_journal_abort` shape, for a
    /// site that has already run irreversible journal effects it cannot
    /// unwind (see `BlockGroup::free_blocks`' system-zone refusal): further
    /// `journal_start`s refuse `EIO` and sleepers wake with an error, so the
    /// poisoned state can never act. Quietly a no-op when the journal is
    /// already gone (unmount teardown) — there is nothing left to poison.
    pub(in crate::fs::fs_impls::ext4) fn abort_journal_on_fs_error(&self) {
        if let Ok(journal) = self.journal() {
            journal.abort_for_fs_error();
        }
    }
}

/// Ensures adding `extra` credits keeps the running transaction within the
/// journal's capacity, erroring `ENOSPC` otherwise. `running` must be the
/// journal's running transaction.
///
/// Each whole revoke block the transaction's revoke set serializes into
/// ([`Transaction::nr_revoke_blocks`]) is charged as one capture-equivalent,
/// which keeps [`Journal::max_credits`]' footprint proof intact (see its
/// docs: a revoke block costs one log block and no descriptor tag, so
/// charging it like a capture over-counts). The charge is a snapshot — the
/// revoke set grows at frees, which pass no capacity gate — so the
/// commit-time exact fit guard stays the hard line; see `max_credits`.
fn check_capacity(journal: &Journal, running: &Transaction, extra: usize) -> Result<()> {
    let revoke_blocks = running.nr_revoke_blocks(journal.geometry().tag_layout());
    let needed = running.reserved_metadata_blocks(extra) + revoke_blocks;
    if needed > journal.max_credits() {
        return_errno_with_message!(Errno::ENOSPC, "journal transaction is full");
    }
    Ok(())
}

/// Opens a handle on the journal's running transaction, reserving `credits`
/// metadata blocks (jbd2 `jbd2_journal_start`).
///
/// If no transaction is running, a fresh one is created with the next tid —
/// even while a committing transaction is mid-flight (the two-transaction
/// pipeline): starting neither consults nor waits on the committing slot
/// (iron law 1: the only waits here are for *space*, legal under inode
/// locks). Two conditions block, both resolved by the same waker set:
///
/// - **The locked barrier** (jbd2 `add_transaction_credits`:
///   `t_state != T_RUNNING` → `wait_transaction_locked`,
///   fs/jbd2/transaction.c:236-243): while a predecessor drains in the
///   locking seat, no handle may join it AND no successor may run — Model A
///   patches serialize the globally-current typed metadata state, so a
///   successor capture racing the locked transaction's still-open handles
///   would leak uncommitted successor state into the older commit's images
///   (an isolation break jbd2 closes the same way, by parking new handles
///   until the drain completes). The wait ends at STAGING, not at the end of
///   commit I/O — the successor then runs concurrently with the commit,
///   which is the pipeline overlap.
/// - **A full running transaction** (jbd2's wait loop): this escalates via
///   [`Journal::request_commit_for`], so under group commit the full
///   transaction is force-locked and drained rather than waited out
///   passively, then retries.
pub(super) fn journal_start(journal: &Arc<Journal>, credits: usize) -> Result<Handle> {
    // A reservation that exceeds an *empty* transaction's capacity can never
    // succeed no matter how many commits retire; refuse it outright so the
    // wait loop below always terminates.
    if credits > journal.max_credits() {
        return_errno_with_message!(Errno::ENOSPC, "reservation exceeds journal capacity");
    }
    loop {
        // An aborted journal (a commit failed and was lost) accepts no new
        // work: capturing into it would publish fragments of the lost
        // transaction. Re-checked every retry — the wait below also ends on
        // abort.
        if journal.is_aborted() {
            return_errno_with_message!(Errno::EIO, "journal aborted");
        }

        let admit = {
            let mut st = journal.state_write();

            if let Some(locking) = st.locking.as_ref() {
                // The locked barrier (see the function docs). Snapshot the
                // epoch under this lock: staging bumps it, so the wake
                // cannot be missed.
                Admit::WaitRoom(locking.tid(), journal.credit_release_epoch())
            } else {
                let created = st.running.is_none();
                if created {
                    let tid = st.next_tid;
                    st.next_tid = tid.next();
                    st.running = Some(Transaction::new(tid));
                }

                // Read the log-position fields before the mutable `running`
                // borrow (disjoint fields, but the guard derefs as a whole).
                let (head, tail_block) = (st.head, st.tail_block);
                // Split the borrow: read the capacity bound off `journal`,
                // then mutate the running transaction. `running` is `Some`
                // by construction above.
                let running = st.running.as_mut().unwrap();
                let tid = running.tid;

                if check_capacity(journal, running, credits).is_err() {
                    // Full. Snapshot the release epoch under the same lock
                    // that observed fullness so a release between dropping
                    // the lock and sleeping still wakes us.
                    Admit::WaitRoom(tid, journal.credit_release_epoch())
                } else if journal.has_commit_servicer() && {
                    // Log-space backpressure (P7c-3, jbd2
                    // `__jbd2_log_wait_for_space`): the reservation fits the
                    // whole ring (capacity), but does it fit the FREE segment
                    // given the un-checkpointed tail? If not, wait for the
                    // commit thread to checkpoint the tail forward rather than
                    // let the transaction build and overflow at commit time.
                    // Only when a commit thread is running to service the wait;
                    // without one, admit and let the commit-time fit guard
                    // drain inline (no servicer to free space, so waiting would
                    // spin). `check_capacity` above passed, so the reserved
                    // credits are `≤ max_credits` ⟹ this footprint `≤ usable`
                    // (the `max_credits` proof), so a full checkpoint frees
                    // enough — the wait terminates.
                    let space_needed = journal.reservation_footprint(running, credits);
                    space_needed > journal.geometry().free_log_blocks(head, tail_block)
                } {
                    Admit::WaitSpace(journal.credit_release_epoch())
                } else {
                    running.t_updates += 1;
                    running.outstanding_credits += credits;
                    drop(st);
                    if created {
                        // Wake the commit thread so it re-arms its age-
                        // trigger sleep on the new transaction's deadline
                        // (jbd2 arms `j_commit_timer` in
                        // `jbd2_get_transaction`; our timer is the commit
                        // thread's own timed sleep, so creation must nudge
                        // it to re-evaluate).
                        journal.request_commit();
                    }

                    return Ok(Handle {
                        tid,
                        credits,
                        journal: Arc::downgrade(journal),
                    });
                }
            }
        };

        // Sleeping here can hold the caller's inode locks; see
        // `wait_for_transaction_room`/`wait_for_log_space`'s locking contract
        // for why that cannot deadlock (open handles drain and the commit
        // thread reclaims log space, neither taking the caller's inode locks).
        match admit {
            Admit::WaitRoom(tid, epoch) => journal.wait_for_transaction_room(tid, epoch)?,
            Admit::WaitSpace(epoch) => journal.wait_for_log_space(epoch)?,
        }
    }
}

/// The outcome of one [`journal_start`] admission attempt: a handle was
/// granted (returned directly), or the caller must wait — for transaction room
/// (a full running transaction, or the locked barrier) or for log space (the
/// reservation does not fit the ring's free segment).
enum Admit {
    WaitRoom(Tid, u64),
    WaitSpace(u64),
}

/// Closes a handle, releasing its credit reservation (jbd2 `jbd2_journal_stop`).
///
/// Under group commit (P7c-2) this does NOT request a commit of the
/// transaction — the batching change from Phase 4's commit-per-op. The
/// running transaction keeps accumulating operations until a trigger fires:
/// an explicit durability demand (`fsync`/`O_SYNC`/`sync(2)` →
/// [`Journal::log_wait_commit`] escalates), the batch-size threshold
/// ([`Journal::batch_trigger_credits`]), or the transaction's age
/// ([`COMMIT_INTERVAL_JIFFIES`]) — jbd2's `h_sync` / size / `t_expires`
/// triggers respectively. **Crash-loss window**: un-synced work can now sit
/// un-committed for up to the age interval (5 s) plus the drain of
/// then-open handles — exactly Linux's data=ordered semantics; anything
/// fsync-acknowledged is still durable at the fsync's return.
///
/// The close always wakes the commit thread to re-evaluate its policy
/// (cheap when it is not waiting): that single uniform wake is what
/// completes a locked transaction's drain (the close of ITS last handle is
/// the event the committer's `Parked` wait sleeps on), arms the age trigger
/// once a transaction gains captures, and lets the size trigger fire at the
/// close that crossed the threshold.
pub(super) fn journal_stop(handle: Handle) -> Result<()> {
    let journal = handle
        .journal
        .upgrade()
        .ok_or_else(|| Error::with_message(Errno::EIO, "journal dropped"))?;

    let released = {
        let mut st = journal.state_write();
        // The handle's transaction is in `running`, or in `locking` if the
        // committer force-locked it while this handle was open (its own
        // handles keep reaching it there; only NEW handles are barred).
        let JournalState {
            running, locking, ..
        } = &mut *st;
        if let Some(txn) = super::active_txn_mut(running, locking, handle.tid) {
            txn.t_updates = txn.t_updates.saturating_sub(1);
            txn.outstanding_credits = txn.outstanding_credits.saturating_sub(handle.credits);
            true
        } else {
            false
        }
    };

    if released {
        // Wake capacity-blocked `journal_start`s: this handle's reservation
        // is back in the pool.
        journal.note_credits_released();
        // Let the committer re-evaluate (drain completion / size / age); see
        // the function docs.
        journal.request_commit();
    }
    Ok(())
}

/// Grows a handle's reservation by `extra` blocks (jbd2 `jbd2_journal_extend`).
///
/// Fails `ENOSPC` if the transaction cannot fit the extra credits, or if the
/// handle's transaction has been force-locked for commit (jbd2 refuses to
/// extend any transaction not in `T_RUNNING`, fs/jbd2/transaction.c
/// `jbd2_journal_extend`: the locked transaction must drain, not grow; the
/// caller's move is a restart). `ENOSPC` covers both for Phase 4.
#[cfg_attr(not(ktest), expect(dead_code))]
pub(super) fn journal_extend(handle: &mut Handle, extra: usize) -> Result<()> {
    let journal = handle
        .journal
        .upgrade()
        .ok_or_else(|| Error::with_message(Errno::EIO, "journal dropped"))?;
    let mut st = journal.state_write();

    if st
        .locking
        .as_ref()
        .is_some_and(|locked| locked.tid == handle.tid)
    {
        return_errno_with_message!(
            Errno::ENOSPC,
            "cannot extend a transaction locked for commit"
        );
    }
    let Some(running) = st.running.as_mut() else {
        return_errno_with_message!(Errno::EIO, "journal_extend without a running transaction");
    };
    check_capacity(&journal, running, extra)?;

    running.outstanding_credits += extra;
    handle.credits += extra;
    Ok(())
}

/// Re-reserves `credits` on this handle, dropping its old reservation (jbd2
/// `jbd2_journal_restart`).
///
/// Phase-4 note: a real restart forces a commit boundary — it commits the
/// current transaction and starts a fresh one so an unbounded operation (write /
/// truncate) never overflows a single transaction. That needs multi-transaction
/// operations (re-capture after the boundary, re-truncate orphan recovery),
/// which are P7d's journal_restart work; this skeleton only releases the old
/// reservation and re-reserves on the *still-running* transaction — so a
/// handle whose transaction was force-locked mid-operation is refused
/// `ENOSPC` (a true restart would close out of the locked transaction and
/// rejoin through `journal_start`'s locked barrier, jbd2's shape).
#[cfg_attr(not(ktest), expect(dead_code))]
pub(super) fn journal_restart(handle: &mut Handle, credits: usize) -> Result<()> {
    let journal = handle
        .journal
        .upgrade()
        .ok_or_else(|| Error::with_message(Errno::EIO, "journal dropped"))?;
    let mut st = journal.state_write();

    if st
        .locking
        .as_ref()
        .is_some_and(|locked| locked.tid == handle.tid)
    {
        return_errno_with_message!(
            Errno::ENOSPC,
            "cannot restart within a transaction locked for commit"
        );
    }
    let Some(running) = st.running.as_mut() else {
        return_errno_with_message!(Errno::EIO, "journal_restart without a running transaction");
    };

    // Release the old reservation first, so the capacity check for the new one
    // does not double-count this handle's credits.
    running.outstanding_credits = running.outstanding_credits.saturating_sub(handle.credits);
    check_capacity(&journal, running, credits)?;

    running.outstanding_credits += credits;
    handle.credits = credits;
    Ok(())
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::{
        super::{
            super::test_utils::{Ext4FixtureBuilder, make_multi_block_file_inode},
            JOURNAL_INO, MetadataCredits,
            format::{
                BLOCKTYPE_SUPERBLOCK_V2, Be32, JBD2_MAGIC, RawJournalHeader, RawJournalSuperblock,
            },
            load_geometry,
        },
        // `*` also re-exports the parent module's `Journal`, `Transaction`, etc.
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

    /// Builds a journaled fixture and its parsed geometry: writes the journal
    /// inode (ino 8) mapping `maxlen` log blocks at [200, 200+maxlen) and a valid
    /// journal superblock (with the given `sequence`) at physical block 200.
    fn journaled_fixture(maxlen: u32, first: u32, sequence: u32) -> Arc<Journal> {
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
        Journal::new(geometry, f.ext4.block_device().clone())
    }

    #[ktest]
    fn transaction_new_starts_running_and_empty() {
        let txn = Transaction::new(Tid::new(7));
        assert_eq!(txn.tid(), Tid::new(7));
        assert_eq!(txn.nr_updates(), 0);
        assert_eq!(txn.nr_metadata_blocks(), 0);
    }

    /// The commit pipeline's legal phase order is exactly Locked → Flush →
    /// Commit → CommitRecord → Finished, with Finished terminal — the single
    /// table [`Journal::advance_committing_phase`] enforces.
    #[ktest]
    fn commit_phase_pipeline_order() {
        use super::CommitPhase::*;
        assert_eq!(Locked.next_in_pipeline(), Some(Flush));
        assert_eq!(Flush.next_in_pipeline(), Some(Commit));
        assert_eq!(Commit.next_in_pipeline(), Some(CommitRecord));
        assert_eq!(CommitRecord.next_in_pipeline(), Some(Finished));
        assert_eq!(Finished.next_in_pipeline(), None);
    }

    #[ktest]
    fn transaction_capture_create_is_idempotent() {
        let mut txn = Transaction::new(Tid::new(1));
        let generation = txn.capture_create(42);
        assert_eq!(txn.nr_metadata_blocks(), 1);
        // A freshly created block is all zeros.
        assert_eq!(txn.buffer_bytes(42), Some([0u8; BLOCK_SIZE].as_slice()));

        // Patch it, then a second `capture_create` must NOT wipe the patch —
        // and, being the same capture, must return the same generation.
        txn.apply_patch(42, generation, |b| b[0..4].copy_from_slice(&[1, 2, 3, 4]))
            .unwrap();
        assert_eq!(txn.capture_create(42), generation);
        assert_eq!(txn.nr_metadata_blocks(), 1);
        assert_eq!(&txn.buffer_bytes(42).unwrap()[0..4], &[1, 2, 3, 4]);
    }

    #[ktest]
    fn transaction_capture_write_seeds_from_device_once() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();

        // Known content in a data block.
        let first = [0xABu8; BLOCK_SIZE];
        f.write_data_block(300, &first);

        let mut txn = Transaction::new(Tid::new(1));
        let generation = txn
            .capture_write(300, None, f.ext4.block_device().as_ref())
            .unwrap();
        assert_eq!(txn.buffer_bytes(300), Some(first.as_slice()));

        // Change the device, then capture again: a no-op (same capture, same
        // generation), so the buffer keeps the FIRST content.
        let second = [0xCDu8; BLOCK_SIZE];
        f.write_data_block(300, &second);
        let again = txn
            .capture_write(300, None, f.ext4.block_device().as_ref())
            .unwrap();
        assert_eq!(again, generation);
        assert_eq!(txn.buffer_bytes(300), Some(first.as_slice()));
    }

    #[ktest]
    fn transaction_apply_patch_accumulates_sub_objects() {
        let mut txn = Transaction::new(Tid::new(1));
        let generation = txn.capture_create(7);

        // Two sub-object patches at different offsets in the SAME block: both
        // persist (the Model-A correctness property for group descriptors /
        // inodes packed into one metadata block).
        txn.apply_patch(7, generation, |b| b[0..4].copy_from_slice(&[1, 2, 3, 4]))
            .unwrap();
        txn.apply_patch(7, generation, |b| b[64..68].copy_from_slice(&[5, 6, 7, 8]))
            .unwrap();

        let bytes = txn.buffer_bytes(7).unwrap();
        assert_eq!(&bytes[0..4], &[1, 2, 3, 4]);
        assert_eq!(&bytes[64..68], &[5, 6, 7, 8]);
        // Untouched bytes stay zero.
        assert_eq!(&bytes[4..64], &[0u8; 60]);
    }

    #[ktest]
    fn transaction_apply_patch_uncaptured_errors() {
        let mut txn = Transaction::new(Tid::new(1));
        // A generation from ANOTHER block's capture: block 99 itself was
        // never captured, so the patch must fail regardless.
        let generation = txn.capture_create(1);
        assert!(txn.apply_patch(99, generation, |_| {}).is_err());
    }

    #[ktest]
    fn journal_start_creates_and_shares_running_transaction() {
        let j = journaled_fixture(64, 1, 1);

        let h1 = journal_start(&j, 4).unwrap();
        {
            let st = j.state_write();
            let running = st.running.as_ref().unwrap();
            assert_eq!(running.tid(), Tid::new(1)); // == geometry.sequence()
            assert_eq!(running.nr_updates(), 1);
            assert_eq!(running.outstanding_credits, 4);
        }
        assert_eq!(h1.tid(), Tid::new(1));
        assert_eq!(h1.credits(), 4);

        // A second start shares the same running transaction.
        let h2 = journal_start(&j, 2).unwrap();
        {
            let st = j.state_write();
            let running = st.running.as_ref().unwrap();
            assert_eq!(running.tid(), Tid::new(1));
            assert_eq!(running.nr_updates(), 2);
            assert_eq!(running.outstanding_credits, 6);
        }
        assert_eq!(h2.tid(), Tid::new(1));

        // Stopping one handle releases its reservation.
        journal_stop(h1).unwrap();
        {
            let st = j.state_write();
            let running = st.running.as_ref().unwrap();
            assert_eq!(running.nr_updates(), 1);
            assert_eq!(running.outstanding_credits, 2);
        }
        journal_stop(h2).unwrap();
    }

    #[ktest]
    fn journal_start_over_capacity_errors() {
        let j = journaled_fixture(64, 1, 1);
        let over = j.max_credits() + 1;
        assert!(journal_start(&j, over).is_err());
    }

    #[ktest]
    fn journal_start_waits_for_credit_release() {
        let j = journaled_fixture(64, 1, 1);
        let h1 = journal_start(&j, j.max_credits()).unwrap();

        // A second start cannot fit until `h1` releases its reservation. Hand
        // `h1` to another thread to release it: the main thread blocks in
        // `journal_start` and must be woken by the release itself — the fixture
        // runs no commit thread, and the full transaction captured nothing, so
        // no commit will ever carry its tid.
        let releaser = crate::thread::kernel_thread::ThreadOptions::new(move || {
            crate::thread::Thread::yield_now();
            journal_stop(h1).unwrap();
        })
        .spawn();

        let h2 = journal_start(&j, 4).unwrap();
        journal_stop(h2).unwrap();
        releaser.join();
    }

    #[ktest]
    fn journal_extend_grows_and_caps_credits() {
        let j = journaled_fixture(64, 1, 1);
        let mut h = journal_start(&j, 4).unwrap();

        journal_extend(&mut h, 3).unwrap();
        assert_eq!(h.credits(), 7);
        {
            let st = j.state_write();
            assert_eq!(st.running.as_ref().unwrap().outstanding_credits, 7);
        }

        // Extending past capacity fails and does not change the reservation.
        let over = j.max_credits();
        assert!(journal_extend(&mut h, over).is_err());
        assert_eq!(h.credits(), 7);

        journal_stop(h).unwrap();
    }

    #[ktest]
    fn journal_restart_re_reserves_credits() {
        let j = journaled_fixture(64, 1, 1);
        let mut h = journal_start(&j, 10).unwrap();

        journal_restart(&mut h, 3).unwrap();
        assert_eq!(h.credits(), 3);
        {
            let st = j.state_write();
            let running = st.running.as_ref().unwrap();
            // The old 10-credit reservation was released, only 3 remain.
            assert_eq!(running.outstanding_credits, 3);
            // Still the same running transaction (no commit boundary yet).
            assert_eq!(running.tid(), Tid::new(1));
        }

        journal_stop(h).unwrap();
    }

    #[ktest]
    fn max_credits_matches_geometry() {
        // maxlen 64, first 1: usable = 63. v0 tags: t = 508 per descriptor.
        // Conservative bound t * (usable - 2) / (t + 1) = 508 * 61 / 509 = 60;
        // exact footprint check: 60 data + ceil(60/508) = 1 descriptor + 1
        // commit = 62 <= 63 usable ring blocks.
        let j = journaled_fixture(64, 1, 1);
        assert_eq!(j.max_credits(), 60);
    }

    /// The capacity check charges the running transaction's revoke-block
    /// footprint as capture-equivalents (P7b-3 space math): with a revoke
    /// set spanning two v0 revoke blocks (1021 records at 1020 per block),
    /// two credits of headroom vanish — an extension that would fit without
    /// the charge is refused.
    #[ktest]
    fn capacity_charges_revoke_block_footprint() {
        let j = journaled_fixture(64, 1, 1); // max_credits = 60
        let mut h = journal_start(&j, 30).unwrap();
        {
            let mut st = j.state_write();
            let running = st.running.as_mut().unwrap();
            for i in 0..1021u64 {
                running.forget_block(30_000 + i);
            }
            assert_eq!(
                running.nr_revoke_blocks(j.geometry().tag_layout()),
                2,
                "1021 records at 1020 per v0 block"
            );
        }
        // needed = 0 captures + 2 revoke blocks + 30 held + 28 extra = 60: fits.
        journal_extend(&mut h, 28).unwrap();
        // One more credit tips it to 61 > 60 — refused ONLY because of the
        // revoke charge (0 + 58 + 1 = 59 would fit without it).
        let err = journal_extend(&mut h, 1).unwrap_err();
        assert_eq!(err.error(), Errno::ENOSPC);
        journal_stop(h).unwrap();
    }

    #[ktest]
    fn ordered_data_dedup_and_liveness() {
        use super::super::super::test_utils::make_empty_file_inode;

        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        // Two distinct regular-file inodes, both read through the block-group
        // cache (which keeps a strong ref while cached).
        f.write_raw_inode(11, &make_empty_file_inode());
        f.write_raw_inode(12, &make_empty_file_inode());
        let a = f.ext4.read_inode(11).unwrap();
        let b = f.ext4.read_inode(12).unwrap();
        let a_pages = a.page_cache().unwrap();
        let b_pages = b.page_cache().unwrap();

        let mut txn = Transaction::new(Tid::new(1));
        assert_eq!(txn.nr_ordered_data(), 0);

        // Register `a` twice (dedups by ino, keeping the newer snapshot) and
        // `b` once.
        txn.register_ordered_data(
            11,
            a.self_arc().map(|i| Arc::downgrade(&i)).unwrap(),
            a_pages.clone(),
            100,
        );
        txn.register_ordered_data(
            11,
            a.self_arc().map(|i| Arc::downgrade(&i)).unwrap(),
            a_pages.clone(),
            200,
        );
        txn.register_ordered_data(
            12,
            b.self_arc().map(|i| Arc::downgrade(&i)).unwrap(),
            b_pages.clone(),
            300,
        );
        assert_eq!(txn.nr_ordered_data(), 2);

        // Both live entries are yielded; the repeat registration replaced the
        // first snapshot's length.
        let mut lens: Vec<_> = txn.ordered_data().map(|(_, len)| len).collect();
        lens.sort_unstable();
        assert_eq!(lens, vec![200, 300]);

        // Evict `b` from the cache and drop our only remaining strong ref: its
        // liveness gate then fails, so the iterator skips it (the key remains
        // counted). The stored page-cache clone must NOT keep it "alive".
        f.ext4.remove_inode(12);
        drop(b);
        let live: Vec<_> = txn.ordered_data().map(|(_, len)| len).collect();
        assert_eq!(live, vec![200]);
        assert_eq!(txn.nr_ordered_data(), 2);
    }

    /// `forget_block` is jbd2's "journaled and then revoked": the pending
    /// capture is cancelled (it must not commit), the revoke is recorded, and
    /// a patch through a stale credential path errors instead of resurrecting
    /// the block.
    #[ktest]
    fn forget_block_cancels_capture_and_records_revoke() {
        let mut txn = Transaction::new(Tid::new(1));
        let generation = txn.capture_create(42);
        txn.apply_patch(42, generation, |b| b[..4].copy_from_slice(&[1, 2, 3, 4]))
            .unwrap();
        assert_eq!(txn.nr_metadata_blocks(), 1);

        txn.forget_block(42);
        assert_eq!(txn.nr_metadata_blocks(), 0);
        assert_eq!(txn.buffer_bytes(42), None);
        assert_eq!(txn.revoked_blocks().collect::<Vec<_>>(), vec![42]);
        // A patch after the forget (an outstanding `WriteAccess` misused past
        // the free) fails loudly rather than re-journaling the freed block.
        assert!(txn.apply_patch(42, generation, |_| {}).is_err());

        // The block reallocated within the same transaction: a NEW capture
        // under a NEW generation. The stale credential's patch still fails —
        // it must not write the old owner's bytes into the new owner's image
        // — while the fresh generation patches normally.
        let recaptured = txn.capture_create(42);
        assert_ne!(recaptured, generation);
        assert!(txn.apply_patch(42, generation, |_| {}).is_err());
        txn.apply_patch(42, recaptured, |b| b[..4].copy_from_slice(&[9, 9, 9, 9]))
            .unwrap();
        assert_eq!(&txn.buffer_bytes(42).unwrap()[..4], &[9, 9, 9, 9]);
    }

    /// jbd2 `jbd2_journal_cancel_revoke` ("block is revoked and then
    /// journaled"): a NEW capture of a revoked block in the same running
    /// transaction cancels the revoke, through both capture funnels —
    /// load-bearing for freed-then-reallocated metadata within one
    /// transaction, whose new image a surviving revoke would suppress.
    #[ktest]
    fn recapture_cancels_revoke() {
        // The create funnel (a freed block reallocated as fresh metadata).
        let mut txn = Transaction::new(Tid::new(1));
        txn.forget_block(42);
        assert_eq!(txn.revoked_blocks().count(), 1);
        txn.capture_create(42);
        assert_eq!(txn.revoked_blocks().count(), 0);
        assert_eq!(txn.nr_metadata_blocks(), 1);

        // The write funnel (a reused block re-captured in place).
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut txn = Transaction::new(Tid::new(2));
        txn.forget_block(300);
        txn.capture_write(300, None, f.ext4.block_device().as_ref())
            .unwrap();
        assert_eq!(txn.revoked_blocks().count(), 0);
        assert_eq!(txn.nr_metadata_blocks(), 1);
    }

    // --- P7c-2: the group-commit batching policy (size / age / request). ---
    // Deterministic: no commit thread runs; `commit_if_due_for_test` is one
    // unforced pipeline advance — exactly the commit thread's wake.

    /// `journal_stop` no longer commits: below every trigger the batch
    /// persists across operations — a second op joins the SAME transaction.
    #[ktest]
    fn journal_stop_batches_instead_of_committing() {
        let j = journaled_fixture(64, 1, 1);
        let h = journal_start(&j, 4).unwrap();
        {
            let mut st = j.state_write();
            let txn = st.running.as_mut().unwrap();
            let generation = txn.capture_create(1500);
            txn.apply_patch(1500, generation, |b| b[..4].copy_from_slice(b"BTCH"))
                .unwrap();
        }
        journal_stop(h).unwrap();

        assert!(!j.commit_if_due_for_test(), "no trigger: nothing commits");
        assert_eq!(j.running_nr_metadata_blocks(), 1, "the batch persists");

        // The next operation joins the same running transaction: the batch.
        let h2 = journal_start(&j, 4).unwrap();
        assert_eq!(h2.tid(), Tid::new(1));
        journal_stop(h2).unwrap();
        assert!(!j.commit_if_due_for_test());
    }

    /// The size trigger: the captured footprint reaching a quarter of
    /// `max_credits` (jbd2's `j_max_transaction_buffers = total/4`) makes
    /// the transaction due — one capture short of it does not.
    #[ktest]
    fn size_threshold_makes_transaction_due() {
        // maxlen 64 → max_credits 60 → the quarter trigger is 15.
        let j = journaled_fixture(64, 1, 1);
        let h = journal_start(&j, 4).unwrap();
        {
            let mut st = j.state_write();
            let txn = st.running.as_mut().unwrap();
            for i in 0..14u64 {
                txn.capture_create(1500 + i);
            }
        }
        journal_stop(h).unwrap();
        assert!(
            !j.commit_if_due_for_test(),
            "one below the quarter: batching"
        );

        let h = journal_start(&j, 4).unwrap();
        {
            let mut st = j.state_write();
            st.running.as_mut().unwrap().capture_create(1600);
        }
        journal_stop(h).unwrap();
        assert!(j.commit_if_due_for_test(), "at the quarter: committed");
        assert_eq!(j.committed_tid(), Tid::new(1));
    }

    /// The age trigger: a batch too small for the size trigger commits once
    /// its transaction outlives the commit interval (deterministically
    /// back-dated; the real deadline is the commit thread's timed sleep).
    #[ktest]
    fn age_makes_transaction_due() {
        let j = journaled_fixture(64, 1, 1);
        let h = journal_start(&j, 4).unwrap();
        {
            let mut st = j.state_write();
            let txn = st.running.as_mut().unwrap();
            let generation = txn.capture_create(1500);
            txn.apply_patch(1500, generation, |b| b[..4].copy_from_slice(b"AGED"))
                .unwrap();
        }
        journal_stop(h).unwrap();
        assert!(!j.commit_if_due_for_test(), "young and small: batching");

        j.age_running_for_test();
        assert!(j.commit_if_due_for_test(), "expired: committed");
        assert_eq!(j.committed_tid(), Tid::new(1));
    }

    /// The durability trigger: `request_commit_for` (what `log_wait_commit`
    /// and the capacity escalation call) makes the named transaction due
    /// immediately, and is cleared by its staging.
    #[ktest]
    fn commit_request_makes_transaction_due() {
        let j = journaled_fixture(64, 1, 1);
        let h = journal_start(&j, 4).unwrap();
        let tid = h.tid();
        {
            let mut st = j.state_write();
            let txn = st.running.as_mut().unwrap();
            let generation = txn.capture_create(1500);
            txn.apply_patch(1500, generation, |b| b[..4].copy_from_slice(b"SYNC"))
                .unwrap();
        }
        journal_stop(h).unwrap();
        assert!(!j.commit_if_due_for_test());

        j.request_commit_for(tid);
        assert!(j.commit_if_due_for_test(), "requested: committed");
        assert_eq!(j.committed_tid(), tid);
        assert!(
            j.state_write().commit_request.is_none(),
            "staging clears the covered request"
        );
    }

    /// A batch-sized revoke set counts toward the size trigger through the
    /// same P7b-3 charge as capacity: whole revoke blocks are
    /// capture-equivalents, so a transaction that mostly FREES still
    /// commits at the quarter footprint (and its 14 revoke blocks + 1 data
    /// + 1 descriptor + 1 commit chain passes the commit fit guard).
    #[ktest]
    fn revoke_footprint_counts_toward_size_trigger() {
        // maxlen 64 → trigger 15: 1 capture + 14 revoke blocks (13*1020+1
        // records at 1020 per v0 block) = 15.
        let j = journaled_fixture(64, 1, 1);
        let h = journal_start(&j, 4).unwrap();
        {
            let mut st = j.state_write();
            let txn = st.running.as_mut().unwrap();
            let generation = txn.capture_create(1500);
            txn.apply_patch(1500, generation, |b| b[..4].copy_from_slice(b"FREE"))
                .unwrap();
            for i in 0..(13u64 * 1020 + 1) {
                txn.forget_block(30_000 + i);
            }
            assert_eq!(
                txn.batch_footprint(j.geometry().tag_layout()),
                15,
                "1 capture + 14 revoke blocks"
            );
        }
        journal_stop(h).unwrap();
        assert!(
            j.commit_if_due_for_test(),
            "revoke footprint tips the trigger"
        );
        assert_eq!(j.committed_tid(), Tid::new(1));
    }

    /// Finding-1 regression (P7c-3 adversarial review): the reservation-time
    /// log-space gate must charge a transaction's whole revoke blocks EXACTLY
    /// ONCE. A running transaction carrying a captured metadata block AND a
    /// whole revoke block (the metadata-`forget` free path) is measured through
    /// the gate's own footprint helper ([`Journal::reservation_footprint`], the
    /// `#[cfg(ktest)]`-observable value `journal_start` compares against the
    /// free segment). It must equal the exact commit-time footprint (`metadata
    /// + revoke + descriptors + commit`), NOT the value the pre-fix gate
    /// produced by feeding `metadata + revoke` as the descriptor-bearing credit
    /// arg — which re-added the revoke block and inflated the descriptor
    /// divisor. The footprint also fits the usable ring at the capacity
    /// boundary, so a full checkpoint always frees enough and the wait
    /// terminates even with revoke_blocks > 0. (The original pressure tests all
    /// had revoke_blocks == 0, so none exercised this term.)
    #[ktest]
    fn reservation_footprint_charges_revoke_blocks_once() {
        // usable 63, max_credits 60, t = 508 (see `max_credits_matches_geometry`).
        let j = journaled_fixture(64, 1, 1);
        let layout = j.geometry().tag_layout();
        let tags = layout.tags_per_descriptor();
        let usable = j.geometry().maxlen() - j.geometry().first();

        let h = journal_start(&j, 8).unwrap();
        let (metadata, revoke_blocks, footprint) = {
            let mut st = j.state_write();
            let txn = st.running.as_mut().unwrap();
            let generation = txn.capture_create(1500);
            txn.apply_patch(1500, generation, |b| b[..4].copy_from_slice(b"META"))
                .unwrap();
            // A handful of metadata frees — one whole revoke block (5 far below
            // the per-block entry count), the term the pre-fix gate double-counted.
            for i in 0..5u64 {
                txn.forget_block(30_000 + i);
            }
            let revoke_blocks = txn.nr_revoke_blocks(layout);
            let metadata = txn.reserved_metadata_blocks(4);
            (metadata, revoke_blocks, j.reservation_footprint(&*txn, 4))
        };
        assert_eq!(
            revoke_blocks, 1,
            "the forgets fill exactly one whole revoke block"
        );

        // Counted once: metadata data + 1 revoke + descriptors(metadata) + commit.
        let expected = metadata + revoke_blocks + metadata.div_ceil(tags) + 1;
        assert_eq!(
            footprint as usize, expected,
            "reservation footprint charges the revoke block exactly once"
        );
        // The pre-fix gate fed `metadata + revoke` as the credit arg, re-adding
        // the revoke block (and inflating the descriptor divisor) — strictly more.
        let double_counted = (metadata + revoke_blocks)
            + revoke_blocks
            + (metadata + revoke_blocks).div_ceil(tags)
            + 1;
        assert!(
            (footprint as usize) < double_counted,
            "corrected footprint is below the revoke-double-counted value"
        );

        // Termination with revoke_blocks > 0: the footprint fits a fully-drained
        // ring, and at the capacity limit — the worst case, since revoke blocks
        // add no descriptor — it still fits, so a parked journal_start always
        // finds room after a full checkpoint and the op completes.
        assert!(
            footprint <= usable,
            "revoke-bearing footprint fits the usable ring"
        );
        assert!(
            j.worst_case_footprint(MetadataCredits(j.max_credits()), 0) <= usable,
            "max-credit footprint fits the ring — the reservation wait terminates"
        );

        journal_stop(h).unwrap();
    }
}
