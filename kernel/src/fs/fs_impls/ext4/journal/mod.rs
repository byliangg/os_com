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
//! (the fsync primitive), [`Tid`]/[`tid_geq`], and the metadata-access seam
//! below.
//!
//! # Metadata-access seam
//!
//! Every metadata modification is routed through four access wrappers so that
//! journaling of the filesystem's *own* writes turns on per call site by
//! threading a live [`Handle`] in, with no change to the wrappers:
//!
//! - [`get_write_access`] — about to modify an existing metadata block; under a
//!   handle it seeds the block's after-image from the device.
//! - [`get_create_access`] — about to populate a freshly allocated metadata
//!   block (extent index/leaf blocks, new directory blocks, new bitmaps); under
//!   a handle it seeds a zeroed after-image.
//! - [`dirty_metadata`] — the metadata block has been modified; under a handle
//!   its `patch` closure writes the modification into the captured after-image,
//!   so sub-objects sharing a block accumulate onto one buffer.
//! - [`forget`] — a previously journaled metadata block is being freed (the
//!   sole insertion point for Phase 7 revoke records); still a no-op.
//!
//! **Without a handle (`None`) every wrapper is inert** and persistence stays
//! ext2-style: metadata objects carry a [`Dirty`](super::utils::Dirty) flag
//! written back by `sync`. Phase 4 builds the capture machinery and recovery but
//! **no metadata operation opens a handle yet** (op-journaling — reshaping the
//! ordered-mode writes into write-ahead logging — is the Int-B follow-up), so
//! every production call site currently passes `None` and behaviour is unchanged
//! from Phase 3. Callers must **never assume [`dirty_metadata`] makes a block
//! persistent** — it marks the block for writeback (and, under a handle,
//! captures its after-image); "write through immediately" would bake in a
//! flush-timing assumption the ordered-mode journal breaks.
//!
//! Note (deviation, see `ext4_rebuild_report.md` §12): the report sketches a
//! `MetaBuffer` handle owning the raw block bytes. We instead reuse ext2's
//! typed-and-dirty-tracked metadata (`Dirty<IdBitmap>`, `Dirty<BlockGroupDesc>`,
//! the inode-table page cache) and identify the affected block by its number;
//! op-journaling captures the block's after-image into a [`Transaction`] buffer.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use ostd::sync::{RwMutexWriteGuard, WaitQueue};

use self::{
    commit::commit_transaction,
    format::{JournalSuperblock, RawJournalSuperblock},
    transaction::{Handle, Transaction},
};
use super::{
    feature::FeatureCompatSet,
    fs::{Ext4, JOURNAL_INO},
    inode,
    prelude::*,
};

mod checkpoint;
mod commit;
mod format;
mod recovery;
mod transaction;

/// Replays a dirty journal at mount time (jbd2 `jbd2_journal_recover`).
///
/// Re-exported at the `ext4` level so [`Ext4::open`](super::fs::Ext4) can drive
/// mount-time recovery; the pass machinery lives in [`recovery`]. A no-op when the
/// on-disk journal superblock is already clean (`s_start == 0`).
pub(in crate::fs::fs_impls::ext4) use self::recovery::recover;

/// Journal transaction id (jbd2 `tid_t`).
pub(super) type Tid = u32;

/// Wrapping-aware "is `a` at or after `b`?" for transaction ids (jbd2 `tid_geq`).
///
/// Tids are a monotonically increasing `u32` that wrap at `u32::MAX`. A plain
/// `a >= b` would answer wrongly across a wrap (e.g. `0` is *after* `u32::MAX`,
/// but `0 >= u32::MAX` is `false`). jbd2 solves this by working in the signed
/// difference: `(a - b)` computed with wrapping arithmetic, reinterpreted as an
/// `i32`, is `>= 0` exactly when `a` is within half the id space *ahead of* `b`.
/// This is the comparison [`Journal::log_wait_commit`] uses to decide whether the
/// target transaction has already committed.
pub(super) fn tid_geq(a: Tid, b: Tid) -> bool {
    (a.wrapping_sub(b) as i32) >= 0
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

    /// The number of block tags that fit in a single descriptor block.
    ///
    /// A descriptor block holds a 12-byte [`RawJournalHeader`](format::RawJournalHeader)
    /// then a tag array of 8-byte [`RawBlockTag`](format::RawBlockTag)s. The first
    /// tag is followed by a 16-byte journal UUID, so the conservative capacity
    /// (charging every tag the 16-byte UUID cost) is `(blocksize - 12 - 16) / 8`.
    /// Phase 4 writes one descriptor per transaction, so this bounds a single
    /// transaction's metadata blocks; used by [`Journal::max_credits`], hence live
    /// in non-ktest builds.
    pub(super) fn tags_per_descriptor(&self) -> usize {
        const HEADER_LEN: usize = 12;
        const UUID_LEN: usize = 16;
        const TAG_LEN: usize = 8;
        (BLOCK_SIZE - HEADER_LEN - UUID_LEN) / TAG_LEN
    }

    /// Returns the log block where recovery starts; 0 means clean (`s_start`).
    ///
    /// Used by [`Journal::new`] to seed the tail, so it is live in non-ktest.
    pub(super) fn start(&self) -> u32 {
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

    let desc = fs.read_inode_desc(JOURNAL_INO)?;
    if !desc.is_extent_based() {
        return_errno_with_message!(
            Errno::EUCLEAN,
            "journal inode is not extent-mapped (Phase 4 requires an extents journal)"
        );
    }

    // The journal file spans this many blocks; its log data starts at block 0.
    let nblocks = desc.size().div_ceil(BLOCK_SIZE as u64) as u32;
    if nblocks < 2 {
        return_errno_with_message!(Errno::EUCLEAN, "journal inode is too small");
    }

    let block_map =
        inode::map_all_blocks(fs.this(), *desc.raw_block(), desc.sector_count(), nblocks)?;

    // Log block 0 holds the journal superblock.
    let raw: RawJournalSuperblock = fs
        .block_device()
        .read_val(block_map[0] as usize * BLOCK_SIZE)
        .map_err(|_| Error::with_message(Errno::EIO, "failed to read the journal superblock"))?;
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
/// committer: it is the only path that calls
/// [`commit_transaction`](commit::commit_transaction) in production. Any number
/// of [`log_wait_commit`](Journal::log_wait_commit) callers only *wait* for a
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
/// The journal's owner ([`Ext4`], in the later integration task) MUST call
/// [`Journal::stop_commit_thread`] exactly once, from a thread **other than the
/// commit thread** (i.e. from `Ext4::drop`), before the last strong reference to
/// the journal goes away. Because the commit thread holds only a `Weak`, it can
/// never itself be the last strong-ref holder, so it can never trigger a
/// join-on-self. There is deliberately **no `join()` in a `Drop` impl** (see
/// [`Journal::stop_commit_thread`]).
///
/// # Lock order
///
/// The commit thread takes the [`state`](Journal::state) lock **only** to swap
/// the running transaction out (`running.take()`); it then commits *without*
/// holding that lock, because commit does device I/O and takes `inode.inner`
/// (the ordered-data flush). The state lock is never held across device I/O or
/// `inode.inner` — matching the leaf position the commit pipeline already
/// documents.
pub(super) struct Journal {
    /// The parsed on-disk geometry (the log block map + journal superblock).
    geometry: JournalGeometry,
    /// The block device the log lives on, so the commit thread can drive
    /// [`commit_transaction`](commit::commit_transaction) without threading the
    /// device through every wakeup.
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
    /// thread after each successful commit.
    commit_wait_queue: WaitQueue,
    /// Set by [`stop_commit_thread`](Journal::stop_commit_thread) to make the
    /// commit thread exit its loop on the next wake.
    stop: AtomicBool,
    /// The commit-thread handle, taken and joined by
    /// [`stop_commit_thread`](Journal::stop_commit_thread). A `Mutex<Option<_>>`
    /// so start/stop can move it in and out; it is **not** held while the thread
    /// runs (the thread itself lives on via the scheduler, referenced only weakly
    /// from its own closure).
    commit_thread: Mutex<Option<Arc<crate::thread::Thread>>>,
}

/// The mutable running-transaction state of a [`Journal`].
///
/// Phase 4 is single-transaction: there is at most one running transaction and
/// no pipelined committing transaction yet.
//
// Referenced by the transaction lifecycle functions (via `Journal::state_write`),
// so it counts as used in non-ktest builds.
pub(super) struct JournalState {
    /// The single running transaction, if any (`journal_t.j_running_transaction`).
    pub(super) running: Option<Transaction>,
    /// The tid to assign to the next transaction created
    /// (`journal_t.j_transaction_sequence`).
    pub(super) next_tid: Tid,
    /// The next free log block to write, i.e. the current log head (jbd2
    /// `journal_t.j_head`). Wraps within `[first, maxlen)`.
    pub(super) head: u32,
    /// The oldest un-checkpointed transaction's start log block — the on-disk
    /// `s_start` (jbd2 `journal_t.j_tail`). `0` means the journal is clean (no
    /// transaction awaits checkpoint).
    pub(super) tail_block: u32,
    /// The oldest un-checkpointed transaction's id — the on-disk `s_sequence`
    /// (jbd2 `journal_t.j_tail_sequence`).
    pub(super) tail_tid: Tid,
}

impl Journal {
    /// Builds an in-memory journal over a parsed [`JournalGeometry`], with no
    /// running transaction.
    ///
    /// The next tid is seeded from `s_sequence` — the first tid recovery expects
    /// (a fresh `mke2fs` journal has `s_sequence == 1`).
    ///
    /// The log-position state is seeded for a **clean** journal (Phase 4's
    /// current assumption — a later task re-seeds it after recovery replays an
    /// existing log):
    /// - `head = s_first`: the first writable log block.
    /// - `tail_block = s_start`: `0` when clean, so nothing awaits checkpoint.
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
        let committed_tid = geometry.sequence().wrapping_sub(1);
        // Fields are listed in struct-declaration order (clippy
        // `inconsistent_struct_constructor`); the geometry-derived values above
        // are pulled into locals so `geometry` can be moved in first.
        Arc::new(Self {
            geometry,
            device,
            state: RwMutex::new(JournalState {
                running: None,
                next_tid,
                head,
                tail_block,
                tail_tid,
            }),
            committed_tid: AtomicU32::new(committed_tid),
            commit_trigger: WaitQueue::new(),
            commit_wait_queue: WaitQueue::new(),
            stop: AtomicBool::new(false),
            commit_thread: Mutex::new(None),
        })
    }

    /// The maximum metadata blocks a single transaction may reserve.
    ///
    /// The bound is the smaller of two limits:
    /// - The usable log blocks (`s_maxlen - s_first`) minus a descriptor + commit
    ///   block of per-transaction overhead.
    /// - The tags that fit in a **single** descriptor block
    ///   ([`JournalGeometry::tags_per_descriptor`]). Phase 4's commit pipeline
    ///   writes one descriptor block per transaction, so admitting more blocks
    ///   than fit its tag array would make the transaction uncommittable.
    ///   Multi-descriptor transactions are a later/perf extension.
    ///
    /// Precise per-transaction credit accounting is a later task.
    pub(super) fn max_credits(&self) -> usize {
        let log_bound = (self.geometry.maxlen() - self.geometry.first()).saturating_sub(2) as usize;
        log_bound.min(self.geometry.tags_per_descriptor())
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
    /// discipline (the state lock is never held across device I/O).
    pub(super) fn state_read(&self) -> RwMutexReadGuard<'_, JournalState> {
        self.state.read()
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
        self.committed_tid.load(Ordering::Acquire)
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
            // Sleep until teardown is requested, the journal is gone, or a
            // committable running transaction exists. `wait_until` re-evaluates
            // this closure on every wake, so a spurious wake simply re-checks.
            //
            // Upgrading the `Weak` inside the closure keeps the journal alive only
            // for the duration of the check; between checks the thread holds no
            // strong reference, so `stop_commit_thread`'s strong count can drain.
            let action = {
                let Some(j) = weak.upgrade() else { break };
                j.commit_trigger
                    .wait_until(|| Self::poll_commit_action(weak))
            };

            match action {
                CommitAction::Exit => break,
                CommitAction::Commit => {
                    let Some(j) = weak.upgrade() else { break };
                    j.commit_one();
                    // Reclaim the log right after committing: Phase 4 is
                    // commit-per-op, so without eager checkpointing a stream of
                    // small transactions would fill the log. Checkpoint copies the
                    // committed after-images to their final locations and clears
                    // `s_start`. Failure is non-fatal — the log stays dirty and the
                    // next commit (or the unmount flush) retries. Batching commits
                    // with lazy, space-pressure-driven checkpoint is a P7
                    // optimization.
                    if let Err(e) = checkpoint::checkpoint(j.as_ref(), j.device.as_ref()) {
                        error!("ext4 journal checkpoint failed: {:?}", e);
                    }
                }
            }
        }
    }

    /// The `wait_until` condition for the commit thread: decides whether to exit,
    /// commit, or keep waiting, based on the current journal state.
    ///
    /// Returns `None` (keep waiting) when there is nothing to do. The short
    /// `state.read()` taken here is safe inside a `wait_until` closure: the guard
    /// is created and dropped entirely within this call, never held across a
    /// suspend, and the commit itself (which needs the *write* lock and does I/O)
    /// happens back in [`commit_one`](Journal::commit_one), outside any wait.
    fn poll_commit_action(weak: &Weak<Journal>) -> Option<CommitAction> {
        let j = weak.upgrade()?;
        if j.stop.load(Ordering::Acquire) {
            return Some(CommitAction::Exit);
        }
        // A running transaction with no open handles and some captured metadata
        // is committable.
        let st = j.state.read();
        match &st.running {
            Some(txn) if txn.nr_updates() == 0 && txn.nr_metadata_blocks() > 0 => {
                Some(CommitAction::Commit)
            }
            _ => None,
        }
    }

    /// Commits the running transaction, if it is (still) committable, and wakes
    /// `log_wait_commit` sleepers. Runs on the commit thread.
    ///
    /// The running transaction is taken **out** under the state write lock, then
    /// committed **without** holding that lock — commit does device I/O and takes
    /// `inode.inner` (the ordered-data flush), which must never happen under the
    /// journal state lock. A fresh running transaction is created lazily by the
    /// next [`journal_start`](transaction::journal_start).
    fn commit_one(&self) {
        let txn = {
            let mut st = self.state_write();
            st.running.take()
        };
        let Some(txn) = txn else { return };

        // Re-check committability after taking: between `poll_commit_action` and
        // this take, a new handle could have joined (raising `nr_updates`), so we
        // must not commit a still-open transaction. If it is not committable, put
        // it back untouched.
        if txn.nr_updates() != 0 || txn.nr_metadata_blocks() == 0 {
            self.state_write().running = Some(txn);
            return;
        }

        // Single-committer: this thread is the only production caller of
        // `commit_transaction`, so no commit lock is needed. `commit_transaction`
        // publishes `committed_tid` (Release) before returning.
        if let Err(e) = commit_transaction(self, self.device.as_ref(), txn) {
            // Phase 4's abort-journal handling is minimal: log and continue.
            // `committed_tid` is not advanced on error, so `log_wait_commit`
            // sleepers keep waiting (a Phase-7 abort path will wake them with an
            // error). The next `request_commit` will retry the new running txn.
            error!("ext4 journal commit failed: {:?}", e);
        }
        // Wake `log_wait_commit` sleepers to re-check `committed_tid`.
        self.commit_wait_queue.wake_all();
    }

    /// Wakes the commit thread to commit the running transaction (jbd2 requesting
    /// a commit). A single wake suffices: the thread re-evaluates the running
    /// transaction's committability in its `wait_until` closure.
    pub(super) fn request_commit(&self) {
        self.commit_trigger.wake_one();
    }

    /// Blocks until transaction `target` (and thus everything up to it) is
    /// committed to the log — the primitive `fsync`/`fdatasync` use (jbd2
    /// `jbd2_log_wait_commit`).
    ///
    /// Returns immediately if `target` is already committed. Otherwise it requests
    /// a commit and sleeps on [`commit_wait_queue`](Journal::commit_wait_queue)
    /// until the commit thread advances `committed_tid` past `target`. The
    /// `request_commit` + condition re-check pairing is what stops it hanging: the
    /// wake sets `committed_tid` *before* `wake_all`, and the `wait_until` closure
    /// re-reads it on every wake (`Acquire`, pairing with the commit's `Release`).
    ///
    /// # Locking
    ///
    /// MUST be called with **no filesystem locks held** — it sleeps on the commit
    /// thread, which needs those same locks. The caller records `target` (the tid
    /// its own now-closed handle joined), releases every inode/journal lock, then
    /// waits.
    ///
    /// # Phase 4 assumption
    ///
    /// The caller's own handle must already be [`journal_stop`](transaction::journal_stop)'d
    /// (so the running transaction has `nr_updates() == 0`) before calling; the
    /// single `request_commit` then suffices to make it committable. The
    /// concurrent-open-handle case — where a commit request must wait for *other*
    /// handles to drain first — is a Phase-7 refinement; a production integration
    /// should also wake [`commit_trigger`](Journal::commit_trigger) from
    /// `journal_stop` when the last handle of a transaction closes.
    // Still dead in non-ktest builds: nothing live waits on a commit yet. The
    // fsync/op integration that calls this (recording its handle's tid, releasing
    // locks, then waiting) is a later Phase-4 task; Int-A only loads + recovers +
    // starts the journal, so the wrappers still pass `None` and no transaction is
    // ever waited on. Drop this gate when fsync is wired to the journal.
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn log_wait_commit(&self, target: Tid) -> Result<()> {
        if tid_geq(self.committed_tid(), target) {
            return Ok(());
        }
        self.request_commit();
        self.commit_wait_queue.wait_until(|| {
            if tid_geq(self.committed_tid(), target) {
                Some(())
            } else {
                None
            }
        });
        Ok(())
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
        self.stop.store(true, Ordering::Release);
        self.commit_trigger.wake_all();
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
    pub(in crate::fs::fs_impls::ext4) fn flush_on_unmount(&self) -> Result<()> {
        let txn = {
            let mut st = self.state_write();
            st.running.take()
        };
        if let Some(txn) = txn
            && txn.nr_metadata_blocks() > 0
        {
            commit_transaction(self, self.device.as_ref(), txn)?;
        }
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
    /// The running transaction is committable: commit it.
    Commit,
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

/// Identifies the kind of metadata block being accessed.
///
/// Phase 2 ignores it; it is the hook where Phase 6 attaches the right checksum
/// computation when a metadata block is dirtied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[expect(dead_code)]
pub(super) enum TriggerType {
    Superblock,
    GroupDesc,
    BlockBitmap,
    InodeBitmap,
    InodeTable,
    ExtentBlock,
    DirBlock,
}

/// Returns the journal's running transaction, verifying it still matches the
/// handle's transaction id.
///
/// A mismatch means the handle outlived its transaction — impossible while the
/// handle holds an open update (the transaction cannot commit until its last
/// handle closes), but checked so a stale patch can never land on the wrong
/// transaction's after-image.
fn running_for<'a>(state: &'a mut JournalState, handle: &Handle) -> Result<&'a mut Transaction> {
    match state.running.as_mut() {
        Some(running) if running.tid() == handle.tid() => Ok(running),
        _ => return_errno_with_message!(Errno::EIO, "journal handle outlived its transaction"),
    }
}

/// Seeds an existing metadata block's after-image from the device so later
/// [`dirty_metadata`] patches accumulate onto its committed content (jbd2
/// `get_write_access`).
///
/// Without a handle (a non-journaled volume, or a caller that opened no
/// transaction) this is a no-op: writeback stays driven by the block's own
/// `Dirty` flag, exactly as in Phases 1–3.
pub(super) fn get_write_access(
    handle: Option<&Handle>,
    blocknr: Ext4Bid,
    _trigger: TriggerType,
) -> Result<()> {
    let Some(handle) = handle else {
        return Ok(());
    };
    let journal = handle.journal()?;
    let device = journal.device.clone();
    let mut state = journal.state_write();
    running_for(&mut state, handle)?.capture_write(blocknr, device.as_ref())
}

/// Seeds a freshly allocated metadata block's after-image as zeroes — its prior
/// device content is meaningless, so no read is issued (jbd2
/// `get_create_access`). No-op without a handle (see [`get_write_access`]).
pub(super) fn get_create_access(
    handle: Option<&Handle>,
    blocknr: Ext4Bid,
    _trigger: TriggerType,
) -> Result<()> {
    let Some(handle) = handle else {
        return Ok(());
    };
    let journal = handle.journal()?;
    let mut state = journal.state_write();
    running_for(&mut state, handle)?.capture_create(blocknr);
    Ok(())
}

/// Patches a metadata block's captured after-image via `patch` (jbd2
/// `dirty_metadata`): `patch` writes this site's modification into the seeded
/// block buffer, so sub-objects sharing one block accumulate onto the same
/// image (see [`Transaction::apply_patch`]).
///
/// Without a handle this is a no-op and `patch` is **not** invoked: writeback
/// stays on the block's own `Dirty` flag, so Phases 1–3 behaviour is unchanged.
/// A `dirty_metadata` under a handle requires a prior `get_*_access` on the same
/// block (else `EIO`).
pub(super) fn dirty_metadata(
    handle: Option<&Handle>,
    blocknr: Ext4Bid,
    _trigger: TriggerType,
    patch: impl FnOnce(&mut [u8]),
) -> Result<()> {
    let Some(handle) = handle else {
        return Ok(());
    };
    let journal = handle.journal()?;
    let mut state = journal.state_write();
    running_for(&mut state, handle)?.apply_patch(blocknr, patch)
}

/// Records that a previously journaled metadata block is being freed. Phase 2:
/// no-op. `is_metadata`/`blocknr` are the Phase-7 revoke insertion point.
pub(super) fn forget(
    _handle: Option<&Handle>,
    _is_metadata: bool,
    _blocknr: Ext4Bid,
) -> Result<()> {
    Ok(())
}

/// Adds an inode to the on-disk orphan list. **Phase 3: no-op.**
///
/// Phase 4 will journal-link the inode onto the superblock orphan chain
/// (`s_last_orphan` head + `i_dtime` "next" pointers) under
/// [`Ext4::s_orphan_lock`](super::fs::Ext4), so crash recovery (SCAN/REPLAY) can
/// finish a deletion that was interrupted after the link count hit 0 but before
/// the blocks/inode were freed. The call sites in `unlink`/`rmdir` (and Task 6's
/// `truncate`) are baked in now so Phase 4 fills only this body, not the
/// namespace operations.
pub(super) fn orphan_add(_handle: Option<&Handle>, _inode_ino: Ext4Ino) -> Result<()> {
    // Phase-4 fill point: acquire `s_orphan_lock`, then journal the orphan-list
    // head/chain update (handle locked *after* `s_orphan_lock`, superblock
    // *before*). Do not acquire the lock here — taking it to do nothing would be
    // flagged.
    Ok(())
}

/// Removes an inode from the on-disk orphan list. **Phase 3: no-op.**
///
/// The mirror of [`orphan_add`]: Phase 4 unlinks the inode from the orphan chain
/// once its blocks and inode have been freed. Called from
/// `try_reclaim_deleted_inode` after `free_inode`.
pub(super) fn orphan_del(_handle: Option<&Handle>, _inode_ino: Ext4Ino) -> Result<()> {
    // Phase-4 fill point: see `orphan_add`.
    Ok(())
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
    use self::format::{BLOCKTYPE_SUPERBLOCK_V2, Be32, JBD2_MAGIC, RawJournalHeader};
    let raw = RawJournalSuperblock {
        header: RawJournalHeader {
            h_magic: Be32::new(JBD2_MAGIC),
            h_blocktype: Be32::new(BLOCKTYPE_SUPERBLOCK_V2),
            h_sequence: Be32::new(0),
        },
        s_blocksize: Be32::new(BLOCK_SIZE as u32),
        s_maxlen: Be32::new(maxlen),
        s_first: Be32::new(first),
        s_sequence: Be32::new(sequence),
        s_start: Be32::new(0),
        s_nr_users: Be32::new(1),
        ..Default::default()
    };
    device
        .write_val(pblock as usize * BLOCK_SIZE, &raw)
        .map_err(|_| Error::with_message(Errno::EIO, "failed to write journal superblock"))
}

/// Test helper: commits a single-block transaction (`dest` ← `after`) to `journal`,
/// leaving the on-disk log dirty (`s_start != 0`) so a subsequent mount recovers it.
///
/// Encapsulates the journal-internal commit machinery ([`Transaction`],
/// [`commit_transaction`]) so the `fs.rs` mount-lifecycle tests can lay down a
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
    txn.capture_create(dest);
    txn.apply_patch(dest, |b| b.copy_from_slice(&after))?;
    commit_transaction(journal, device, txn)?;
    Ok(())
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::{
        super::test_utils::{Ext4FixtureBuilder, make_multi_block_file_inode},
        format::{BLOCKTYPE_SUPERBLOCK_V2, Be32, JBD2_MAGIC, RawJournalHeader},
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
        assert_eq!(geo.sequence(), 1);
        assert_eq!(geo.start(), 0);
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

    #[ktest]
    fn load_geometry_without_journal_is_none() {
        // Same fixture but without the HAS_JOURNAL compat bit.
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        assert!(load_geometry(&f.ext4).unwrap().is_none());
    }

    // --- Task 5: commit thread, log_wait_commit, teardown, tid_geq. ---

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
        assert!(tid_geq(1, 0));
        assert!(tid_geq(5, 5)); // equal is "at or after"
        assert!(!tid_geq(0, 1));
        // Wrap: 0 is "after" u32::MAX (0 == MAX + 1 in wrapping arithmetic).
        assert!(tid_geq(0, u32::MAX));
        assert!(!tid_geq(u32::MAX, 0));
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
            txn.capture_create(500);
            txn.apply_patch(500, |b| b[..4].copy_from_slice(b"META"))
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
        assert_eq!(desc.h_sequence.get(), running_tid);
        assert_eq!(read_first_tag(&f, 1).t_blocknr.get(), 500);
        let commit = read_log_header(&f, 3);
        assert_eq!(commit.h_blocktype.get(), BLOCKTYPE_COMMIT);
        assert_eq!(commit.h_sequence.get(), running_tid);

        // The running transaction was consumed by the commit.
        assert!(f.journal.state_write().running.is_none());

        // Teardown: stops and joins the commit thread. Returns (does not hang).
        f.journal.stop_commit_thread();
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
            txn.capture_create(500);
            txn.apply_patch(500, |b| b[..4].copy_from_slice(b"META"))
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
        get_write_access(Some(&handle), CAPTURE_BLOCK, TriggerType::BlockBitmap).unwrap();
        dirty_metadata(
            Some(&handle),
            CAPTURE_BLOCK,
            TriggerType::BlockBitmap,
            |buf| {
                buf[4] = 0xAB;
            },
        )
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
        get_create_access(Some(&handle), CAPTURE_BLOCK, TriggerType::ExtentBlock).unwrap();
        dirty_metadata(
            Some(&handle),
            CAPTURE_BLOCK,
            TriggerType::ExtentBlock,
            |buf| buf[0] = 1,
        )
        .unwrap();
        dirty_metadata(
            Some(&handle),
            CAPTURE_BLOCK,
            TriggerType::ExtentBlock,
            |buf| buf[1] = 2,
        )
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
        get_write_access(None, CAPTURE_BLOCK, TriggerType::BlockBitmap).unwrap();
        dirty_metadata(None, CAPTURE_BLOCK, TriggerType::BlockBitmap, |_| {
            patched = true
        })
        .unwrap();

        assert!(!patched, "patch must not run without a handle");
        assert!(
            f.journal.state_read().running.is_none(),
            "no transaction created without a handle"
        );
    }

    /// A `dirty_metadata` under a handle but without a prior `get_*_access` on
    /// that block errors: nothing seeded the after-image to patch.
    #[ktest]
    fn funnel_dirty_without_access_errors() {
        let f = journaled_fixture(16, 1, 1);
        let handle = journal_start(&f.journal, 4).unwrap();
        assert!(
            dirty_metadata(
                Some(&handle),
                CAPTURE_BLOCK,
                TriggerType::BlockBitmap,
                |_| {}
            )
            .is_err(),
            "dirty_metadata without get_*_access must fail"
        );
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
        get_write_access(Some(&handle), bid, TriggerType::BlockBitmap).unwrap();
        dirty_metadata(Some(&handle), bid, TriggerType::BlockBitmap, |buf| {
            buf[..8].copy_from_slice(b"UNMOUNT!")
        })
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
        assert_eq!(f.journal.state_read().tail_block, 0);
        assert!(f.journal.state_read().running.is_none());
    }

    /// `flush_on_unmount` on a journal that never ran a transaction is a clean
    /// no-op (nothing captured, tail already clean).
    #[ktest]
    fn flush_on_unmount_with_no_work_is_noop() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);
        f.journal.flush_on_unmount().unwrap();
        assert_eq!(f.journal.state_read().tail_block, 0);
    }
}
