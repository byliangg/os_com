// SPDX-License-Identifier: MPL-2.0
//! Phase 6 Task 2 — integration-layer JBD2 commit/checkpoint driver over the **safe core** journal.
//!
//! The production journaled-write path (`fs.rs::run_journaled_ext4` + its commit/checkpoint driver
//! + fsync force-commit) was historically built on the third-party `ext4_rs` **fat**
//! `JournalRuntime` (which owns `checkpoint_list` / `last_committed_tid` / rotation /
//! `commit_ready` / `all_checkpoint_plans` / `finish_commit`-with-checkpoint / overlay) plus the
//! `ext4_rs` `Jbd2Journal` (which owns the running `JournalSpace`, the journal superblock,
//! `write_commit_plan_with_hook`, and `checkpoint_transaction`).
//!
//! `core/journal` is **deliberately thinner**: its [`CoreJournalRuntime`] is just the in-memory
//! transaction state machine (`start_handle` / `record_metadata_write` / `stop_handle` /
//! `prepare_commit` / `finish_commit`), its [`commit::write_commit_plan`] is the byte-exact on-disk
//! commit emitter (descriptor + payload + the **single** ordered-mode `sync()` barrier + commit
//! block + journal-SB update + ring advance), and [`JournalSpace`] is the ring math. It carries
//! **no** `checkpoint_list` / `last_committed_tid` / rotation / batch-threshold / overlay — those
//! were marked "P6 integration-layer responsibility" by the Phase 5 rewrite.
//!
//! This module **re-derives** exactly those omitted pieces, 逐位 replicating the behavior of the
//! old `fs.rs` ext4_rs-driving code (which passes 守底), but driving **core**'s runtime +
//! `write_commit_plan`. [`CoreJournalDriver`] is the single owner of the core-backed running
//! journal state:
//! - the core [`CoreJournalRuntime`] (in-memory state machine);
//! - `checkpoint_list: VecDeque<CheckpointEntry>` (committed-but-not-yet-home-written transactions,
//!   each carrying its committed ring range + full metadata images);
//! - `last_committed_tid` (fsync force-commit fast path);
//! - the running [`JournalSpace`] + running [`RawJournalSuperblock`] + `physical_blocks` (the
//!   journal inode's fs block vector) — the geometry the core commit emitter consumes, seeded once
//!   at driver init from the on-disk journal SB (read through the overlay bridge).
//!
//! **RED LINES preserved 逐位 (report §5.4/5.5/5.7/§8):**
//! - **Lock order is owned by `fs.rs`** (correctness → RUNTIME → jbd2_runtime → inner; the jbd2
//!   journal/checkpoint locks are taken only inside commit/checkpoint, never while `inner` is held).
//!   This module exposes the same `&mut self` engine steps the old code drove under those locks; it
//!   does **not** take locks itself (the caller holds them), keeping the chokepoint identical.
//! - **The single commit `sync()` barrier** is inside `core::commit::write_commit_plan`
//!   (descriptor+payload → **one** `sync()` → commit block → SB → ring), exactly where ext4_rs put
//!   it. We pass a [`SyncBarrier`] that issues the *same* `block_device.sync()` ext4_rs issued.
//! - **Checkpoint write序** (home blocks → single `sync()` → tail advance → journal-SB store) is
//!   replicated here, batched the same way `try_batch_checkpoint_all_jbd2_transactions` was.
//! - **Overlay read-your-writes**: `running_metadata_image` (core, running/prev/committing) +
//!   `checkpoint_list` lookup (here) reproduce ext4_rs `latest_metadata_buffer`'s newest-wins order.
//!
//! `core/` stays `#![forbid(unsafe_code)]`; this integration-layer module is safe Rust.

use alloc::collections::{BTreeMap, VecDeque};

use super::core::journal::{
    commit::{self, CommitCtx, JournalBarrier, JournalCommitWriteStage},
    space::JournalSpace,
    superblock::load_journal_sb,
    transaction::{JournalCommitPlan, JournalRuntime as CoreJournalRuntime},
};
use super::core::{
    io::BlockReader, metadata_writer::MetadataWriter, types::Ext4Fsblk,
    journal::format::RawJournalSuperblock,
};
use crate::prelude::*;

/// A committed transaction parked in the checkpoint list: its ring range (for tail reconciliation)
/// + the full metadata block images (home block number → full-block image), in `block_nr` order.
///
/// PARITY: ext4_rs keeps the whole `JournalTransaction` (with its `checkpoint_range` + `buffers`) in
/// the `checkpoint_list`; we keep only what the home-write + tail-advance + overlay-read need.
struct CheckpointEntry {
    tid: u32,
    /// Ring start block of this transaction's descriptor (== journal logical block of descriptor).
    start_block: u32,
    /// Ring block one past this transaction's commit block (where the next transaction starts).
    next_head: u32,
    /// Full-block metadata images keyed by home (fs) block number, ascending — same order the
    /// commit plan wrote them. `Vec<(block_nr, image)>` (already deduped/clamped by the runtime).
    metadata_blocks: Vec<(Ext4Fsblk, Vec<u8>)>,
}

/// A single checkpoint plan handed to the home-write step.
pub(super) struct CheckpointPlan {
    pub tid: u32,
    pub start_block: u32,
    pub next_head: u32,
    pub metadata_blocks: Vec<(Ext4Fsblk, Vec<u8>)>,
}

/// The integration-layer journal driver: owns the core-backed running journal state and re-derives
/// the fat-runtime + Jbd2Journal commit/checkpoint logic the old ext4_rs path provided.
pub(super) struct CoreJournalDriver {
    runtime: CoreJournalRuntime,
    /// Running ring geometry (consumed/advanced by the core commit emitter + our checkpoint).
    space: JournalSpace,
    /// Running journal superblock (the core commit emitter updates s_start/s_head/s_sequence; our
    /// checkpoint updates s_start). Seeded once from the on-disk journal SB at init.
    sb: RawJournalSuperblock,
    /// journal inode's fs block vector: `physical_blocks[logical] = fs block`. Maps journal logical
    /// blocks to home (fs) blocks for the core commit emitter's `CommitCtx`.
    physical_blocks: Vec<Ext4Fsblk>,
    /// Committed-but-not-checkpointed transactions, oldest first (== ext4_rs `checkpoint_list`).
    checkpoint_list: VecDeque<CheckpointEntry>,
    /// Highest TID whose commit finished durably (== ext4_rs `last_committed_tid`). fsync fast path.
    last_committed_tid: u32,
    /// Per-tid "what op_name filled this transaction" — test-only, drives the injected-crash replay
    /// hold (the `replay_hold`/`replay_hold_op` cmdline). PARITY: ext4_rs carried `trigger_op` on the
    /// transaction → commit plan; core's plan omits it (it's not on the differential write序 path), so
    /// we track it integration-side. Cleared when the transaction commits.
    trigger_op_by_tid: BTreeMap<u32, &'static str>,
    block_size: usize,
}

impl CoreJournalDriver {
    /// Build the driver from the journal inode's `physical_blocks` (resolved at mount from the
    /// ext4_rs `Jbd2Journal` that the mount path still constructs), reading the on-disk journal SB
    /// through `reader` (the overlay bridge) to seed the running ring geometry.
    ///
    /// PARITY: `fs.rs::initialize_jbd2_journal` constructed both the ext4_rs `JournalRuntime`
    /// (first_tid = journal SB `s_sequence`) and the `Jbd2Journal` (whose `space` came from
    /// `JournalSpace::from_superblock`). We seed the core runtime with the same `first_tid` and the
    /// core space from the same on-disk SB.
    pub(super) fn from_physical_blocks(
        reader: &dyn BlockReader,
        physical_blocks: Vec<Ext4Fsblk>,
        block_size: usize,
    ) -> Result<Self> {
        let sb = load_journal_sb(reader, &physical_blocks, block_size)?;
        let space = JournalSpace::from_superblock(&sb)?;
        let first_tid = sb.sequence();
        Ok(Self {
            runtime: CoreJournalRuntime::new(block_size, first_tid),
            space,
            sb,
            physical_blocks,
            checkpoint_list: VecDeque::new(),
            last_committed_tid: 0,
            trigger_op_by_tid: BTreeMap::new(),
            block_size,
        })
    }

    pub(super) fn block_size(&self) -> usize {
        self.block_size
    }

    // =====================================================================================
    // Handle lifecycle (driven by fs.rs under the jbd2_runtime write lock).
    // =====================================================================================

    /// Start a JBD2 handle, returning its unique id. PARITY: ext4_rs `JournalRuntime::start_handle`
    /// (the `mark_handle_requires_data_sync` debug flag is dropped — data-sync was always ordered
    /// mode). `op_name` is recorded against the resulting running transaction's tid so the
    /// injected-crash replay hold (test-only) can fire at the matching op's commit.
    pub(super) fn start_handle(&mut self, reserved_blocks: u32, op_name: &'static str) -> Option<u64> {
        let handle_id = self.runtime.start_handle(reserved_blocks)?;
        if let Some(tid) = self.runtime.running_transaction().map(|t| t.tid()) {
            self.trigger_op_by_tid.entry(tid).or_insert(op_name);
        }
        Some(handle_id)
    }

    /// Record a full-block metadata image into the active transaction of `handle_id`.
    /// PARITY: ext4_rs `record_metadata_write_for_handle` (block-keyed; the caller already holds the
    /// full block image — no pre-image read needed, see `core_adapter::CoreMetadataWriter`).
    pub(super) fn record_metadata_write(&mut self, handle_id: u64, block: Ext4Fsblk, data: &[u8]) {
        self.runtime.record_metadata_write(handle_id, block, data);
    }

    /// Stop a handle, returning `(transaction_id, transaction_has_metadata)`. PARITY: ext4_rs
    /// `stop_handle` returns a summary; we need the tid for `record_inode_tid` and the
    /// "transaction has modified blocks" flag for the `record_inode_tid` gate (ext4_rs gates on
    /// `summary.modified_blocks > 0`). We read the transaction's current modified-block count after
    /// the stop (the handle's own transaction, found across running/prev/committing).
    pub(super) fn stop_handle(&mut self, handle_id: u64) -> Option<(u32, bool)> {
        let tid = self.runtime.stop_handle(handle_id)?;
        let has_metadata = self
            .runtime
            .transaction_modified_block_count(tid)
            .is_some_and(|count| count > 0);
        Some((tid, has_metadata))
    }

    /// Is there an active handle? PARITY: ext4_rs `should_defer_metadata_write` (overlay-defer门控).
    pub(super) fn should_defer_metadata_write(&self) -> bool {
        self.runtime.should_defer_metadata_write()
    }

    /// Is there at least one open handle? PARITY: ext4_rs `has_active_handle`.
    pub(super) fn has_active_handle(&self) -> bool {
        // `should_defer_metadata_write` is exactly `enabled && has active handle`; enabled is always
        // true for a constructed driver, so this is the same predicate.
        self.runtime.should_defer_metadata_write()
    }

    // =====================================================================================
    // Overlay read (read-your-writes). PARITY: ext4_rs `latest_metadata_buffer` order:
    // running → prev_running → committing → checkpoint_list (reverse). Core gives us the first
    // three via `running_metadata_image`; we append the checkpoint_list lookup (newest first).
    // =====================================================================================

    /// Overlay the newest in-memory metadata image for `[offset, offset+out.len())` onto `out`.
    /// Returns true if any byte was overlaid. PARITY: ext4_rs `overlay_metadata_read`.
    pub(super) fn overlay_metadata_read(&self, offset: usize, out: &mut [u8]) -> bool {
        if self.block_size == 0 || out.is_empty() {
            return false;
        }
        let Some(end) = offset.checked_add(out.len()) else {
            return false;
        };
        let first_block = offset / self.block_size;
        let last_block = (end - 1) / self.block_size;
        let mut overlaid = false;

        for block_nr in first_block..=last_block {
            let Some(image) = self.latest_metadata_image(block_nr as u64) else {
                continue;
            };
            let block_start = block_nr * self.block_size;
            let block_end = block_start + self.block_size;
            let overlap_start = core::cmp::max(offset, block_start);
            let overlap_end = core::cmp::min(end, block_end);
            if overlap_start >= overlap_end {
                continue;
            }
            let out_start = overlap_start - offset;
            let out_end = overlap_end - offset;
            let buf_start = overlap_start - block_start;
            let buf_end = overlap_end - block_start;
            out[out_start..out_end].copy_from_slice(&image[buf_start..buf_end]);
            overlaid = true;
        }
        overlaid
    }

    /// Newest full-block image for home `block_nr`: running/prev/committing (core) first, then the
    /// checkpoint_list in reverse (newest committed first). PARITY: ext4_rs `latest_metadata_buffer`.
    fn latest_metadata_image(&self, block_nr: u64) -> Option<&[u8]> {
        if let Some(image) = self.runtime.running_metadata_image(block_nr) {
            return Some(image);
        }
        for entry in self.checkpoint_list.iter().rev() {
            for (nr, image) in entry.metadata_blocks.iter() {
                if *nr == block_nr {
                    return Some(image.as_slice());
                }
            }
        }
        None
    }

    /// Drop the in-memory checkpoint image of a metadata block being rewritten in place (the data
    /// path is about to home-write it directly, so its stale journaled image must not overlay).
    /// PARITY: ext4_rs `revoke_checkpoint_metadata_block` (BUG-5: in-memory only — never writes a
    /// JBD2 revoke block; replicated to keep byte-frozen behavior). Returns count removed.
    pub(super) fn revoke_checkpoint_metadata_block(&mut self, block_nr: u64) -> usize {
        let mut revoked = 0usize;
        for entry in self.checkpoint_list.iter_mut() {
            let before = entry.metadata_blocks.len();
            entry.metadata_blocks.retain(|(nr, _)| *nr != block_nr);
            if entry.metadata_blocks.len() != before {
                revoked = revoked.saturating_add(1);
            }
        }
        revoked
    }

    // =====================================================================================
    // Commit readiness + rotation. PARITY: ext4_rs same-named methods (journal.rs:272-353).
    // =====================================================================================

    pub(super) fn commit_ready(&self) -> bool {
        if self
            .runtime
            .prev_running_transaction()
            .is_some_and(|t| t.handle_count() == 0 && t.modified_block_count() != 0)
        {
            return true;
        }
        if self.runtime.prev_running_transaction().is_some() {
            return false;
        }
        self.runtime
            .running_transaction()
            .is_some_and(|t| t.handle_count() == 0 && t.modified_block_count() != 0)
    }

    pub(super) fn batch_commit_ready(&self, threshold_blocks: u32) -> bool {
        if self
            .runtime
            .prev_running_transaction()
            .is_some_and(|t| t.handle_count() == 0 && t.modified_block_count() != 0)
        {
            return true;
        }
        if self.runtime.prev_running_transaction().is_some() {
            return false;
        }
        self.runtime.running_transaction().is_some_and(|t| {
            t.handle_count() == 0 && t.modified_block_count() as u32 >= threshold_blocks
        })
    }

    pub(super) fn should_rotate_running_transaction(&self, threshold_blocks: u32) -> bool {
        self.runtime.prev_running_transaction().is_none()
            && self.runtime.running_transaction().is_some_and(|t| {
                t.handle_count() != 0
                    && t.modified_block_count() != 0
                    && t.modified_block_count() as u32 >= threshold_blocks
            })
    }

    /// Rotate the running transaction to prev_running so existing handles drain to commit-readiness
    /// without new handles joining. PARITY: ext4_rs `rotate_running_transaction` (threshold 0 path).
    pub(super) fn rotate_running_transaction(&mut self) -> Option<u32> {
        self.runtime.rotate_running_for_force()
    }

    pub(super) fn running_transaction_tid(&self) -> Option<u32> {
        self.runtime.running_transaction().map(|t| t.tid())
    }

    pub(super) fn last_committed_tid(&self) -> u32 {
        self.last_committed_tid
    }

    pub(super) fn checkpoint_depth(&self) -> usize {
        self.checkpoint_list.len()
    }

    pub(super) fn checkpoint_ready(&self) -> bool {
        !self.checkpoint_list.is_empty()
    }

    pub(super) fn space_free_blocks(&self) -> u32 {
        self.space.free_blocks()
    }

    pub(super) fn space_tail(&self) -> u32 {
        self.space.tail()
    }

    // =====================================================================================
    // Commit. PARITY: ext4_rs `try_commit_ready_jbd2_transaction` core: prepare_commit (core
    // runtime) → write_commit_plan (core emitter, single sync barrier) → finish_commit (advance
    // last_committed_tid + park in checkpoint_list with its ring range).
    // =====================================================================================

    /// Prepare the next commit-ready transaction's plan + its trigger op_name (for the injected-crash
    /// replay hold). Returns None if nothing is commit-ready or a commit is already in flight.
    pub(super) fn prepare_commit(&mut self) -> Option<(JournalCommitPlan, Option<&'static str>)> {
        let plan = self.runtime.prepare_commit()?;
        let trigger_op = self.trigger_op_by_tid.get(&plan.tid).copied();
        Some((plan, trigger_op))
    }

    /// Commit `plan` to the journal via the core emitter, then park it in the checkpoint list with
    /// its committed ring range. PARITY: ext4_rs `write_commit_plan_with_hook` + `finish_commit`.
    ///
    /// `writer` is the integration `MetadataWriter` (records journal-area block writes into the
    /// journal home blocks — for commit it must write **straight to the device**, see fs.rs caller),
    /// `barrier` issues the single ordered-mode `block_device.sync()` between payloads and commit.
    /// `hook` is the crash-injection stage callback (no-op in production except injected-crash ops).
    ///
    /// Returns the committed tid on success.
    pub(super) fn write_commit_plan_with_hook(
        &mut self,
        writer: &dyn MetadataWriter,
        barrier: Option<&dyn JournalBarrier>,
        handle_id: u64,
        plan: &JournalCommitPlan,
        hook: impl FnMut(JournalCommitWriteStage),
    ) -> Result<u32> {
        // Snapshot ring start before the emitter advances `space.head` so we can record the parked
        // transaction's range. The descriptor lands at the current head; next_head = head + N + 2
        // (the emitter advances `space.head` by N+2 and re-validates free space itself).
        let start_block = self.space.head();
        let ctx = CommitCtx {
            physical_blocks: &self.physical_blocks,
            writer,
            barrier,
            handle_id,
            block_size: self.block_size,
        };
        let tid = commit::write_commit_plan_with_hook(
            &ctx,
            &mut self.space,
            &mut self.sb,
            plan,
            hook,
        )?;
        // next_head == ring position one past the commit block (== space.head after advance, but the
        // emitter recomputes the same via space.advance(commit_block, 1); read it back from space).
        let next_head = self.space.head();
        self.finish_commit(tid, start_block, next_head, plan);
        Ok(tid)
    }

    /// Park a committed transaction in the checkpoint list and advance `last_committed_tid`.
    /// PARITY: ext4_rs `JournalRuntime::finish_commit(tid, start_block, next_head)` (set_state
    /// Checkpoint + set_checkpoint_range + push_back + advance last_committed_tid) + core runtime
    /// `finish_commit(tid)` (clear committing slot).
    fn finish_commit(&mut self, tid: u32, start_block: u32, next_head: u32, plan: &JournalCommitPlan) {
        // Clear the core runtime's committing slot (idempotent guard on tid).
        if !self.runtime.finish_commit(tid) {
            return;
        }
        let metadata_blocks = plan
            .metadata_blocks
            .iter()
            .map(|b| (b.block_nr, b.block_data.clone()))
            .collect();
        self.checkpoint_list.push_back(CheckpointEntry {
            tid,
            start_block,
            next_head,
            metadata_blocks,
        });
        if tid > self.last_committed_tid {
            self.last_committed_tid = tid;
        }
        self.trigger_op_by_tid.remove(&tid);
    }

    /// Abort a prepared-but-not-committed transaction (ENOSPC / missing journal): roll it back out
    /// of `committing` into running/prev_running so it retries. PARITY: ext4_rs `abort_commit`.
    pub(super) fn abort_commit(&mut self, tid: u32) {
        self.runtime.abort_commit(tid);
    }

    // =====================================================================================
    // Checkpoint. PARITY: ext4_rs `try_batch_checkpoint_all_jbd2_transactions`:
    // reconcile tail → collect all plans → home-write all (caller) → single sync (caller) →
    // per-tx tail advance + journal-SB s_start store (here, via `checkpoint_transaction`).
    // =====================================================================================

    /// All pending checkpoint plans without removing them (caller home-writes them all, syncs once,
    /// then calls `checkpoint_transaction` per tid). PARITY: ext4_rs `all_checkpoint_plans`.
    pub(super) fn all_checkpoint_plans(&self) -> Vec<CheckpointPlan> {
        self.checkpoint_list
            .iter()
            .map(|entry| CheckpointPlan {
                tid: entry.tid,
                start_block: entry.start_block,
                next_head: entry.next_head,
                metadata_blocks: entry.metadata_blocks.clone(),
            })
            .collect()
    }

    /// The ring start of the checkpoint after `tid`, if `tid` is the front entry.
    /// PARITY: ext4_rs `next_checkpoint_start_after` (Some(Some) = next exists; Some(None) = last).
    pub(super) fn next_checkpoint_start_after(&self, tid: u32) -> Option<Option<u32>> {
        let front = self.checkpoint_list.front()?;
        if front.tid != tid {
            return None;
        }
        Some(self.checkpoint_list.get(1).map(|next| next.start_block))
    }

    /// Advance the journal tail past a checkpointed transaction's range and store the journal SB
    /// `s_start`, then drop it from the checkpoint list. PARITY: ext4_rs `checkpoint_transaction`
    /// (tail-advance + `update_start` + SB store) + `finish_checkpoint` (pop_front).
    ///
    /// The home blocks must already be durable (caller wrote + synced them). `writer` stores the
    /// journal SB to its logical-block-0 home; `next_start` is the next checkpoint's start (or None
    /// → s_start = 0 meaning "journal empty / no pending checkpoint").
    pub(super) fn checkpoint_transaction(
        &mut self,
        writer: &dyn MetadataWriter,
        handle_id: u64,
        tid: u32,
        start_block: u32,
        next_head: u32,
        next_start: Option<u32>,
    ) -> Result<()> {
        if self.space.tail() != start_block {
            return Err(Error::with_message(
                Errno::EINVAL,
                "checkpoint start does not match current journal tail",
            ));
        }
        let released = self.space.distance(start_block, next_head);
        self.space.advance_tail(released);
        // PARITY: ext4_rs `JournalSuperblockState::update_start` then `store`. s_start = next_start
        // or 0 (journal drained). The core commit emitter recomputes the SB csum on its own SB
        // updates; the checkpoint SB store must likewise recompute before storing.
        self.sb.set_start(next_start.unwrap_or(0));
        self.store_journal_sb(writer, handle_id)?;
        // Drop the front entry (== tid).
        if self.checkpoint_list.front().is_some_and(|e| e.tid == tid) {
            self.checkpoint_list.pop_front();
        }
        Ok(())
    }

    /// Reconcile the in-memory checkpoint list against the journal tail: drop entries already
    /// released (tail moved past them). PARITY: ext4_rs `discard_checkpointed_before_tail`.
    pub(super) fn discard_checkpointed_before_tail(&mut self, current_tail: u32) -> usize {
        let Some(front) = self.checkpoint_list.front() else {
            return 0;
        };
        if front.start_block == current_tail {
            return 0;
        }
        let keep_index = self
            .checkpoint_list
            .iter()
            .position(|entry| entry.start_block == current_tail);
        let Some(keep_index) = keep_index else {
            let all_released = self
                .checkpoint_list
                .back()
                .is_some_and(|entry| entry.next_head == current_tail);
            if !all_released {
                return 0;
            }
            let dropped = self.checkpoint_list.len();
            self.checkpoint_list.clear();
            return dropped;
        };
        for _ in 0..keep_index {
            self.checkpoint_list.pop_front();
        }
        keep_index
    }

    /// Store the running journal SB (1024 bytes) to journal logical block 0 (== physical_blocks[0]),
    /// recomputing its checksum first. PARITY: ext4_rs `JournalSuperblockState::store`
    /// (`update_checksum` then write block 0). Reuses the core SB-store byte layout.
    fn store_journal_sb(&mut self, writer: &dyn MetadataWriter, handle_id: u64) -> Result<()> {
        commit::store_journal_sb_via_writer(
            writer,
            handle_id,
            &self.physical_blocks,
            self.block_size,
            &mut self.sb,
        )
    }
}

/// A [`JournalBarrier`] that issues the single ordered-mode `block_device.sync()` ext4_rs issued
/// between the commit payloads and the commit block. Wraps the fs.rs sync closure so this module
/// stays free of the `ext4_rs::BlockDevice` adapter type.
pub(super) struct SyncBarrier<'a> {
    sync: &'a dyn Fn() -> Result<()>,
}

impl<'a> SyncBarrier<'a> {
    pub(super) fn new(sync: &'a dyn Fn() -> Result<()>) -> Self {
        Self { sync }
    }
}

impl JournalBarrier for SyncBarrier<'_> {
    fn sync(&self) -> Result<()> {
        (self.sync)()
    }
}

/// A no-op crash hook (production commit path without injected crash).
pub(super) fn no_commit_hook(_stage: JournalCommitWriteStage) {}
