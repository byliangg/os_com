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
//!   the metadata blocks it will commit, with the 7-state [`TransactionState`]
//!   lifecycle (`t_state`) and the open-handle / credit bookkeeping
//!   (`t_updates` / `t_outstanding_credits`).
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
// metadata funnels, and `Handle::tid` backs fsync's `sync_tid`. Still dead in
// non-ktest builds: `journal_extend`/`journal_restart` (mid-op credit growth —
// ops use fixed conservative credits until P7's precise accounting) and the
// non-`Running`/`Finished` `TransactionState` variants (the staged commit
// pipeline states). This one module-level expectation absorbs those (avoiding
// a marker on each); it is absent in ktest, where all of it is exercised.
#![cfg_attr(not(ktest), expect(dead_code))]

use super::{
    super::{inode::Inode, prelude::*},
    Journal, Tid,
};

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
        super::tid_geq(committed_tid, self.tid)
    }
}

/// The jbd2 transaction lifecycle (`transaction_t.t_state`).
///
/// Phase 4's commit pipeline drives only
/// [`Running`](TransactionState::Running) → [`Finished`](TransactionState::Finished)
/// (a single committer thread needs no intermediate states). The five
/// in-between states are reserved for P7's staged/group commit; they are
/// defined up front to keep the jbd2 lifecycle visible and avoid enum churn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TransactionState {
    /// Accepting new handles and metadata (`T_RUNNING`).
    Running,
    /// Closed to new handles, draining outstanding ones (`T_LOCKED`).
    #[expect(dead_code)]
    Locked,
    /// Flushing data buffers before the commit record (`T_FLUSH`).
    #[expect(dead_code)]
    Flush,
    /// Writing the log (`T_COMMIT`).
    #[expect(dead_code)]
    Commit,
    /// Flushing the commit record to the data device (`T_COMMIT_DFLUSH`).
    #[expect(dead_code)]
    CommitDFlush,
    /// Flushing the commit record to the journal device (`T_COMMIT_JFLUSH`).
    #[expect(dead_code)]
    CommitJFlush,
    /// Fully committed; awaiting checkpoint (`T_FINISHED`).
    Finished,
}

/// An in-memory transaction accumulating metadata after-images (jbd2
/// `transaction_t`).
///
/// Phase 4 is single-transaction: the [`Journal`] holds at most one of these as
/// its running transaction. It records the captured after-images plus the
/// open-handle and credit bookkeeping used to bound its size.
//
// Visible at the `ext4` level (`pub(in crate::fs::fs_impls::ext4)`) so it can be
// a field of the equally-visible `JournalState`.
pub(in crate::fs::fs_impls::ext4) struct Transaction {
    /// This transaction's id (`t_tid`).
    tid: Tid,
    /// The lifecycle state (`t_state`); Task 2a leaves it [`Running`](TransactionState::Running).
    state: TransactionState,
    /// Number of open handles (`journal_start` not yet `journal_stop`'d;
    /// `t_updates`).
    t_updates: usize,
    /// Sum of live handles' reserved credits — the max blocks they may dirty
    /// (`t_outstanding_credits`).
    outstanding_credits: usize,
    /// The captured after-images, keyed by physical block number. An ordered map
    /// so commit writes tags in a deterministic block order.
    metadata: BTreeMap<Ext4Bid, MetaBuffer>,
    /// The inodes whose **data** was dirtied under this transaction, for
    /// ordered-data mode (jbd2 `t_inode_list`). Keyed by ino so an inode dirtied
    /// several times in one transaction is flushed once; the value is a [`Weak`]
    /// so a running transaction never keeps an inode alive (a dropped inode's
    /// data is no longer this transaction's concern). At commit each is upgraded
    /// and its dirty data flushed to its final location before any log block is
    /// written.
    ordered_inodes: BTreeMap<Ext4Ino, Weak<Inode>>,
}

impl Transaction {
    /// Creates a fresh running transaction with id `tid` and no captured blocks.
    pub(super) fn new(tid: Tid) -> Self {
        Self {
            tid,
            state: TransactionState::Running,
            t_updates: 0,
            outstanding_credits: 0,
            metadata: BTreeMap::new(),
            ordered_inodes: BTreeMap::new(),
        }
    }

    /// This transaction's id.
    pub(super) fn tid(&self) -> Tid {
        self.tid
    }

    /// This transaction's lifecycle state.
    pub(super) fn state(&self) -> TransactionState {
        self.state
    }

    /// The number of open handles against this transaction.
    pub(super) fn nr_updates(&self) -> usize {
        self.t_updates
    }

    /// The number of distinct metadata blocks captured so far.
    pub(super) fn nr_metadata_blocks(&self) -> usize {
        self.metadata.len()
    }

    /// Captures a freshly allocated metadata block as a zeroed after-image
    /// (jbd2 `get_create_access`): its prior device content is meaningless, so
    /// no read is needed. Idempotent — a block already captured (possibly with
    /// patches applied) is left untouched.
    pub(super) fn capture_create(&mut self, bid: Ext4Bid) {
        self.metadata.entry(bid).or_insert_with(MetaBuffer::zeroed);
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
    /// re-seeded, so any patches already applied survive).
    pub(super) fn capture_write(
        &mut self,
        bid: Ext4Bid,
        seed: Option<&[u8]>,
        device: &dyn BlockDevice,
    ) -> Result<()> {
        if self.metadata.contains_key(&bid) {
            return Ok(());
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
        self.metadata.insert(bid, buffer);
        Ok(())
    }

    /// Retains a copy of every captured after-image in `retained`, keyed by
    /// block and tagged with this transaction's tid — called (under the journal
    /// state lock) at the moment this transaction leaves `running` to commit,
    /// so that from the very first instant a *new* transaction can exist, a
    /// capture of one of these blocks seeds from these bytes and never from the
    /// (lagging) device. Checkpoint evicts the entries once the device has
    /// caught up (see [`UncheckpointedImage`]).
    ///
    /// An entry for a block this transaction re-captured simply overwrites the
    /// older image: this transaction's is the newest.
    pub(super) fn stash_uncheckpointed(
        &self,
        retained: &mut BTreeMap<Ext4Bid, UncheckpointedImage>,
    ) {
        for (bid, buffer) in &self.metadata {
            retained.insert(
                *bid,
                UncheckpointedImage {
                    tid: self.tid,
                    image: buffer.duplicate(),
                },
            );
        }
    }

    /// Patches a captured block's after-image in place (jbd2 `dirty_metadata`):
    /// looks up `bid`'s buffer and hands its bytes to `patch`.
    ///
    /// This is how a whole-block object (a bitmap:
    /// `|b| b.copy_from_slice(bitmap.as_bytes())`) or a sub-block object (a group
    /// descriptor / inode: patch only its field bytes at its offset within the
    /// block) writes its after-image. Multiple sub-objects sharing one block
    /// patch the *same* buffer, so their images accumulate correctly.
    ///
    /// Errors with `EIO` if the block was not captured first (a
    /// `dirty_metadata` without a prior `get_*_access`).
    pub(super) fn apply_patch(
        &mut self,
        bid: Ext4Bid,
        patch: impl FnOnce(&mut [u8]),
    ) -> Result<()> {
        let Some(buffer) = self.metadata.get_mut(&bid) else {
            return_errno_with_message!(Errno::EIO, "dirty_metadata without prior get_*_access");
        };
        patch(buffer.as_mut());
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
        self.metadata.get(&bid).map(MetaBuffer::as_bytes)
    }

    /// Iterates the captured metadata blocks as `(destination block#,
    /// after-image bytes)` pairs, in ascending block-number order.
    ///
    /// The order is the [`BTreeMap`] key order, so the commit pipeline writes
    /// descriptor tags and their logged blocks in a single deterministic order
    /// (jbd2 walks `t_buffers` in insertion order; block order is equally valid
    /// and lets recovery apply each after-image to its final location).
    pub(super) fn metadata_blocks(&self) -> impl Iterator<Item = (Ext4Bid, &[u8])> {
        self.metadata
            .iter()
            .map(|(&bid, buffer)| (bid, buffer.as_bytes()))
    }

    /// Registers `inode` as an ordered-data inode of this transaction (jbd2
    /// `jbd2_journal_inode_ranges_write` / `t_inode_list`): its dirty data will be
    /// flushed to its final location before this transaction's metadata is
    /// committed. Keyed by ino, so a repeat registration of the same inode is a
    /// no-op beyond refreshing the `Weak`. Held weakly — the transaction never
    /// keeps the inode alive.
    pub(super) fn add_ordered_inode(&mut self, inode: &Arc<Inode>) {
        self.ordered_inodes
            .insert(inode.ino(), Arc::downgrade(inode));
    }

    /// Iterates the live ordered-data inodes, upgrading each [`Weak`] and skipping
    /// any inode that has since been dropped (its data is no longer this
    /// transaction's concern). The commit pipeline flushes each yielded inode's
    /// data before writing any log block.
    pub(super) fn ordered_inodes(&self) -> impl Iterator<Item = Arc<Inode>> + '_ {
        self.ordered_inodes.values().filter_map(Weak::upgrade)
    }

    /// The number of registered ordered-data inodes (including any whose inode may
    /// since have been dropped). Inspection/test accessor.
    pub(super) fn nr_ordered_inodes(&self) -> usize {
        self.ordered_inodes.len()
    }

    /// Moves this transaction to `state` (jbd2 `t_state` transitions driven by
    /// the commit pipeline).
    pub(super) fn set_state(&mut self, state: TransactionState) {
        self.state = state;
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
}

/// Ensures adding `extra` credits keeps the running transaction within the
/// journal's capacity, erroring `ENOSPC` otherwise. `running` must be the
/// journal's running transaction.
fn check_capacity(journal: &Journal, running: &Transaction, extra: usize) -> Result<()> {
    let needed = running.nr_metadata_blocks() + running.outstanding_credits + extra;
    if needed > journal.max_credits() {
        return_errno_with_message!(Errno::ENOSPC, "journal transaction is full");
    }
    Ok(())
}

/// Opens a handle on the journal's running transaction, reserving `credits`
/// metadata blocks (jbd2 `jbd2_journal_start`).
///
/// If no transaction is running, a fresh one is created with the next tid. When
/// the running transaction cannot fit the reservation, this blocks until it
/// commits or another handle releases credits, then retries — the minimal form
/// of jbd2 `add_transaction_credits`' wait loop. (Precise credit accounting and
/// commit *batching* under pressure remain P7; this only stops a full
/// transaction from surfacing as `ENOSPC` to unlink/write under load.)
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

        let (tid, epoch) = {
            let mut st = journal.state_write();

            if st.running.is_none() {
                let tid = st.next_tid;
                st.next_tid = st.next_tid.wrapping_add(1);
                st.running = Some(Transaction::new(tid));
            }

            // Split the borrow: read the capacity bound off `journal`, then
            // mutate the running transaction. `running` is `Some` by
            // construction above.
            let running = st.running.as_mut().unwrap();
            let tid = running.tid;

            if check_capacity(journal, running, credits).is_ok() {
                running.t_updates += 1;
                running.outstanding_credits += credits;

                return Ok(Handle {
                    tid,
                    credits,
                    journal: Arc::downgrade(journal),
                });
            }

            // Full. Snapshot the release epoch under the same lock that
            // observed fullness so a release between dropping the lock and
            // sleeping still wakes us (it must bump the epoch after this).
            (tid, journal.credit_release_epoch())
        };

        // Sleeping here can hold the caller's inode locks; see
        // `wait_for_transaction_room`'s locking contract for why that cannot
        // deadlock (open handles drain without our locks, and the commit
        // thread takes no `inner`).
        journal.wait_for_transaction_room(tid, epoch)?;
    }
}

/// Closes a handle, releasing its credit reservation (jbd2 `jbd2_journal_stop`).
///
/// When the last handle of a transaction closes (`t_updates` reaches 0) and the
/// transaction captured metadata, this signals the commit thread — Phase 4's
/// commit-per-op. The signal is asynchronous; durability is `fsync`'s job.
pub(super) fn journal_stop(handle: Handle) -> Result<()> {
    let journal = handle
        .journal
        .upgrade()
        .ok_or_else(|| Error::with_message(Errno::EIO, "journal dropped"))?;

    let released = {
        let mut st = journal.state_write();
        if let Some(running) = st.running.as_mut()
            && running.tid == handle.tid
        {
            running.t_updates = running.t_updates.saturating_sub(1);
            running.outstanding_credits =
                running.outstanding_credits.saturating_sub(handle.credits);
            // Once the last handle closes and the transaction has captured
            // metadata, it is committable. Phase 4 is commit-per-op: signal the
            // commit thread now. This is asynchronous — the operation does not wait
            // for the commit (durability is `fsync`'s job, via `log_wait_commit`).
            Some(running.t_updates == 0 && running.nr_metadata_blocks() > 0)
        } else {
            None
        }
    };

    if let Some(should_commit) = released {
        // Wake capacity-blocked `journal_start`s: this handle's reservation is
        // back in the pool even if the transaction is not committable.
        journal.note_credits_released();
        if should_commit {
            journal.request_commit();
        }
    }
    Ok(())
}

/// Grows a handle's reservation by `extra` blocks (jbd2 `jbd2_journal_extend`).
///
/// Fails `ENOSPC` if the transaction cannot fit the extra credits. (jbd2 returns
/// a distinct "cannot extend" signal so the caller can restart; `ENOSPC` is
/// adequate for Phase 4.)
pub(super) fn journal_extend(handle: &mut Handle, extra: usize) -> Result<()> {
    let journal = handle
        .journal
        .upgrade()
        .ok_or_else(|| Error::with_message(Errno::EIO, "journal dropped"))?;
    let mut st = journal.state_write();

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
/// which are P7's journal_restart work; this skeleton only releases the old
/// reservation and re-reserves on the *still-running* transaction.
pub(super) fn journal_restart(handle: &mut Handle, credits: usize) -> Result<()> {
    let journal = handle
        .journal
        .upgrade()
        .ok_or_else(|| Error::with_message(Errno::EIO, "journal dropped"))?;
    let mut st = journal.state_write();

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
            JOURNAL_INO,
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
        let txn = Transaction::new(7);
        assert_eq!(txn.tid(), 7);
        assert_eq!(txn.state(), TransactionState::Running);
        assert_eq!(txn.nr_updates(), 0);
        assert_eq!(txn.nr_metadata_blocks(), 0);
    }

    #[ktest]
    fn transaction_capture_create_is_idempotent() {
        let mut txn = Transaction::new(1);
        txn.capture_create(42);
        assert_eq!(txn.nr_metadata_blocks(), 1);
        // A freshly created block is all zeros.
        assert_eq!(txn.buffer_bytes(42), Some([0u8; BLOCK_SIZE].as_slice()));

        // Patch it, then a second `capture_create` must NOT wipe the patch.
        txn.apply_patch(42, |b| b[0..4].copy_from_slice(&[1, 2, 3, 4]))
            .unwrap();
        txn.capture_create(42);
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

        let mut txn = Transaction::new(1);
        txn.capture_write(300, None, f.ext4.block_device().as_ref())
            .unwrap();
        assert_eq!(txn.buffer_bytes(300), Some(first.as_slice()));

        // Change the device, then capture again: a no-op, so the buffer keeps the
        // FIRST content.
        let second = [0xCDu8; BLOCK_SIZE];
        f.write_data_block(300, &second);
        txn.capture_write(300, None, f.ext4.block_device().as_ref())
            .unwrap();
        assert_eq!(txn.buffer_bytes(300), Some(first.as_slice()));
    }

    #[ktest]
    fn transaction_apply_patch_accumulates_sub_objects() {
        let mut txn = Transaction::new(1);
        txn.capture_create(7);

        // Two sub-object patches at different offsets in the SAME block: both
        // persist (the Model-A correctness property for group descriptors /
        // inodes packed into one metadata block).
        txn.apply_patch(7, |b| b[0..4].copy_from_slice(&[1, 2, 3, 4]))
            .unwrap();
        txn.apply_patch(7, |b| b[64..68].copy_from_slice(&[5, 6, 7, 8]))
            .unwrap();

        let bytes = txn.buffer_bytes(7).unwrap();
        assert_eq!(&bytes[0..4], &[1, 2, 3, 4]);
        assert_eq!(&bytes[64..68], &[5, 6, 7, 8]);
        // Untouched bytes stay zero.
        assert_eq!(&bytes[4..64], &[0u8; 60]);
    }

    #[ktest]
    fn transaction_apply_patch_uncaptured_errors() {
        let mut txn = Transaction::new(1);
        assert!(txn.apply_patch(99, |_| {}).is_err());
    }

    #[ktest]
    fn journal_start_creates_and_shares_running_transaction() {
        let j = journaled_fixture(64, 1, 1);

        let h1 = journal_start(&j, 4).unwrap();
        {
            let st = j.state_write();
            let running = st.running.as_ref().unwrap();
            assert_eq!(running.tid(), 1); // == geometry.sequence()
            assert_eq!(running.nr_updates(), 1);
            assert_eq!(running.outstanding_credits, 4);
        }
        assert_eq!(h1.tid(), 1);
        assert_eq!(h1.credits(), 4);

        // A second start shares the same running transaction.
        let h2 = journal_start(&j, 2).unwrap();
        {
            let st = j.state_write();
            let running = st.running.as_ref().unwrap();
            assert_eq!(running.tid(), 1);
            assert_eq!(running.nr_updates(), 2);
            assert_eq!(running.outstanding_credits, 6);
        }
        assert_eq!(h2.tid(), 1);

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
            assert_eq!(running.tid(), 1);
        }

        journal_stop(h).unwrap();
    }

    #[ktest]
    fn max_credits_matches_geometry() {
        // maxlen 64, first 1: usable = 64 - 1, minus 2 overhead = 61.
        let j = journaled_fixture(64, 1, 1);
        assert_eq!(j.max_credits(), 61);
    }

    #[ktest]
    fn ordered_inodes_dedup_and_iterate() {
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

        let mut txn = Transaction::new(1);
        assert_eq!(txn.nr_ordered_inodes(), 0);

        // Register `a` twice (dedups by ino) and `b` once.
        txn.add_ordered_inode(&a);
        txn.add_ordered_inode(&a);
        txn.add_ordered_inode(&b);
        assert_eq!(txn.nr_ordered_inodes(), 2);

        // Both live inodes are yielded.
        let mut inos: Vec<_> = txn.ordered_inodes().map(|i| i.ino()).collect();
        inos.sort_unstable();
        assert_eq!(inos, vec![11, 12]);

        // Evict `b` from the cache and drop our only remaining strong ref: its
        // `Weak` then no longer upgrades, so the iterator skips it (but the key
        // remains counted).
        f.ext4.remove_inode(12);
        drop(b);
        let live: Vec<_> = txn.ordered_inodes().map(|i| i.ino()).collect();
        assert_eq!(live, vec![11]);
        assert_eq!(txn.nr_ordered_inodes(), 2);
    }
}
