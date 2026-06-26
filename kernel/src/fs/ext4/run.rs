// SPDX-License-Identifier: MPL-2.0
//! Phase 8 move-only split: the journaled-write / read execution hub relocated verbatim from
//! `fs.rs` — the `JournaledOp` op descriptor, the `EXT4_RS_RUNTIME_LOCK` runtime chokepoint, the
//! JBD2 checkpoint-tuning consts, the mount-time journal init/replay, the JBD2 handle / commit /
//! checkpoint state machine, the crash-injection replay hooks, and the `run_io_*` / `run_journaled_*`
//! wrappers. No behavior change.

use core::sync::atomic::Ordering;

use aster_cmdline::{KCMDLINE, ModuleArg};

use super::core::journal::commit::JournalCommitWriteStage;
use super::fs::{EXT4_SUPERBLOCK_OFFSET, Ext4Fs};
use super::profile::GENERIC014_SLOW_OP_LOG_THRESHOLD_NS;
use super::types::{DeviceMetadataWriter as Ext4MetadataWriter, EXT4_BLOCK_SIZE};
use crate::prelude::*;


// Lazy JBD2 checkpoint thresholds.
// Only checkpoint when journal free blocks drop below these limits, rather than
// after every single commit. This avoids a BioType::Flush per operation.
// Pre-commit: if free < NEEDED + JOURNAL_LOW_WATER, checkpoint first to make room.
// Intentionally small: typical journal is ~1024 blocks, so only flush when nearly full.
pub(super) const JOURNAL_LOW_WATER_MARK: u32 = 64;
// Post-commit: if free < JOURNAL_CHECKPOINT_THRESHOLD, checkpoint to keep headroom.
pub(super) const JOURNAL_CHECKPOINT_THRESHOLD: u32 = 128;
// Post-commit memory bound: committed-but-not-checkpointed transactions keep a
// copy of every metadata block they touched (~4 KiB each) in the in-memory
// `checkpoint_list`. The journal-space trigger above can leave that list to
// grow into the gigabytes during a long fsync-less transaction (e.g. SQLite's
// 500k-row inserts), exhausting the kernel heap. Force a checkpoint once the
// list reaches this depth so the in-memory footprint stays bounded.
pub(super) const JOURNAL_CHECKPOINT_MAX_DEPTH: usize = 64;
// Maximum number of metadata blocks to accumulate before forcing a commit.
// Batching many handles into one transaction reduces commit frequency and
// eliminates the per-handle journal write overhead for high-concurrency workloads
// like fsstress with many parallel processes.
pub(super) const JOURNAL_COMMIT_BATCH_BLOCKS: u32 = 128;
// Regular-file fsync can legally rely on committed journal transactions for
// crash durability, but if checkpointing is deferred indefinitely, committed
// metadata accumulates in memory under xfstests generic/047. Periodically drain
// the checkpoint queue to keep memory bounded without regressing to per-fsync
// full-filesystem sync.
pub(super) const REGULAR_FILE_FSYNC_CHECKPOINT_DEPTH: usize = 8;

// ext4_rs currently stores runtime block size in a global variable.
// Serialize ext4_rs calls across mounted ext4 instances to avoid
// cross-filesystem block-size races during xfstests mkfs/remount cycles.
pub(super) static EXT4_RS_RUNTIME_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug)]
pub(super) enum JournaledOp {
    Create,
    Mkdir,
    Unlink,
    Rmdir,
    Rename,
    Write { len: usize, ino: u32 },
    Truncate { ino: u32 },
    InodeMetadata { ino: u32 },
}

impl JournaledOp {
    /// Step 4a-2: returns the primary inode whose metadata is modified by this
    /// op, used by `finish_jbd2_handle` to update the inode→TID map for
    /// fsync force-commit.  Single-inode metadata ops carry inode info;
    /// directory ops touch multiple inodes (parents + child) and are not
    /// tracked at this granularity in v1.
    fn affected_ino(&self) -> Option<u32> {
        match self {
            Self::Write { ino, .. } | Self::Truncate { ino } | Self::InodeMetadata { ino } => {
                Some(*ino)
            }
            _ => None,
        }
    }
}

impl JournaledOp {
    /// Tag for buffered writes through `Ext4Fs::write_at`.
    ///
    /// Step 4a-2: previously this returned `None` for writes larger than
    /// `JOURNALED_SMALL_WRITE_MAX_BYTES` (192 B), causing the
    /// `inode_tids` map to miss large buffered writes — so fsync of those
    /// inodes had `target_tid = None` and skipped force-commit.
    /// generic/047 (32 K pwrite + fsync per file) exposed this: late files
    /// went un-committed and were lost after shutdown + replay.
    /// Now we always return `Some(Write { len, ino })` for non-empty
    /// writes; the journal credit estimation in
    /// `estimate_jbd2_reserved_blocks` already scales with `len`.
    pub(super) fn for_small_write(ino: u32, _offset: usize, data: &[u8]) -> Option<Self> {
        if data.is_empty() {
            return None;
        }
        Some(Self::Write {
            len: data.len(),
            ino,
        })
    }
}

impl Ext4Fs {
    pub(super) fn initialize_jbd2_journal(&self) {
        // Phase 6 Task 4: build the JBD2 journal entirely via `core/`, off ext4_rs.
        //
        // PARITY: this replaces `ext4_rs Ext4::load_journal` → `Jbd2Journal::load` → `JournalDevice::
        // load`. The journal-feature gate (`COMPAT_HAS_JOURNAL`) is the same one ext4_rs `load_journal`
        // checked; the journal inode's physical block vector is resolved by core
        // (`resolve_journal_physical_blocks`, byte-for-byte the same walk as `JournalDevice::load`);
        // the running ring geometry is seeded from the on-disk journal SB read through the overlay
        // bridge (`CoreJournalDriver::from_physical_blocks` → `load_journal_sb` +
        // `JournalSpace::from_superblock`) — identical to what ext4_rs built.
        if !self.core_sb.has_journal() {
            info!("ext4: filesystem has no JBD2 journal feature; using non-journal path");
            *self.jbd2_runtime.write() = None;
            return;
        }

        // Mount bootstrap reads go straight to the device (raw), like ext4_rs `JournalDevice::load` /
        // `JournalSuperblockState::load` — the overlay is empty at mount, so this is overlay-
        // independent and the exact parity match.
        let reader = super::core_adapter::CoreRawDeviceReader::new(self.adapter.clone());
        let (physical_blocks, block_size) =
            match super::core_adapter::resolve_journal_physical_blocks(&reader, &self.core_sb) {
                Ok(result) => result,
                Err(err) => {
                    warn!("ext4: failed to resolve journal physical blocks: {:?}", err);
                    *self.jbd2_runtime.write() = None;
                    return;
                }
            };
        let journal_inode = self.core_sb.journal_inode_number();
        match super::journal_driver::CoreJournalDriver::from_physical_blocks(
            &reader,
            physical_blocks,
            block_size,
        ) {
            Ok(driver) => {
                info!(
                    "ext4: loaded JBD2 journal inode={} blocks={} mapped_blocks={} block_size={} sequence={} start={} head={} first={} free_blocks={} incompat=0x{:x}",
                    journal_inode,
                    driver.journal_maxlen(),
                    driver.journal_logical_blocks(),
                    driver.block_size(),
                    driver.journal_sequence(),
                    driver.journal_start(),
                    driver.journal_head(),
                    driver.journal_first(),
                    driver.space_free_blocks(),
                    driver.journal_feature_incompat(),
                );
                *self.jbd2_runtime.write() = Some(driver);
            }
            Err(err) => {
                warn!(
                    "ext4: failed to build core JBD2 driver: {:?}; journal disabled",
                    err
                );
                *self.jbd2_runtime.write() = None;
            }
        }
    }

    pub(super) fn replay_mount_jbd2_journal(&self) {
        // Phase 6 Task 4: mount-time recovery via `core/`, off ext4_rs.
        //
        // PARITY: the needs-recovery gate is (journal SB `s_start != 0`) OR (fs-SB RECOVER flag) —
        // exactly the old `journal.needs_recovery() || inner.super_block.needs_recovery()`. The first
        // term is now `CoreJournalDriver::journal_needs_recovery` (→ `recovery::needs_recovery`); the
        // second is the core-owned `recover_flag` (the in-memory fs-SB RECOVER bit, seeded at mount).
        let journal_needs_recovery = self
            .jbd2_runtime
            .read()
            .as_ref()
            .is_some_and(|driver| driver.journal_needs_recovery());
        let needs_recovery =
            journal_needs_recovery || self.recover_flag.load(Ordering::Acquire);
        if !needs_recovery {
            return;
        }

        // Drive the three-pass replay over the core driver. `reader` reads journal log blocks straight
        // off the device (raw, no overlay — PARITY with ext4_rs `journal.recover(&self.adapter)`);
        // `writer` is a RAW home writer (bypasses the journal `MetadataWriter` — recovery replays
        // committed metadata straight to home, see `RecoverCtx::writer` doc). The driver resets +
        // stores the journal SB and re-seeds its own runtime/ring from the reset SB.
        let recovery_result = {
            let reader = super::core_adapter::CoreRawDeviceReader::new(self.adapter.clone());
            let writer = super::core_adapter::CoreRecoveryWriter::new(self.adapter.clone());
            let mut driver_guard = self.jbd2_runtime.write();
            let Some(driver) = driver_guard.as_mut() else {
                // PARITY: ext4_rs `replay_mount_jbd2_journal` returned WITHOUT clearing the flag when
                // `jbd2_journal` was `None` (`let Some(journal) = .. else { return; }`). Replicate
                // exactly — do NOT clear the RECOVER flag on the no-journal path (byte-frozen).
                return;
            };
            driver.recover_at_mount(&reader, &writer)
        };

        match recovery_result {
            Ok(result) => {
                // RED LINE write order (PARITY: old `sync_recovered_jbd2_state`): the replay already
                // wrote home blocks + the reset journal SB; now sync them, clear the fs-SB RECOVER
                // flag, sync again. `finalize_recovered_superblock` owns that sequence.
                if let Err(err) = self.finalize_recovered_superblock() {
                    warn!(
                        "ext4: JBD2 recovery replayed transactions but failed to finalize superblock state: {:?}",
                        err
                    );
                    return;
                }
                info!(
                    "ext4: JBD2 recovery complete: transactions={} metadata_blocks={} revoked={} last_sequence={:?}",
                    result.transactions_replayed,
                    result.metadata_blocks_replayed,
                    result.revoked_blocks,
                    result.last_sequence,
                );
            }
            Err(err) => {
                warn!("ext4: JBD2 recovery failed at mount: {:?}", err);
            }
        }
    }

    /// Finalize the on-disk superblock state after a mount-time replay: flush the replayed home
    /// blocks + reset journal SB, clear the fs-superblock `EXT4_FEATURE_INCOMPAT_RECOVER` flag, then
    /// flush the cleared flag. The journal driver's runtime/ring was already re-seeded from the reset
    /// SB inside `recover_at_mount`.
    ///
    /// PARITY: ext4_rs `sync_recovered_jbd2_state` write order — `block_device.sync()` →
    /// `super_block.set_needs_recovery(false) + sync_to_disk_with_csum` → `block_device.sync()`. The
    /// RECOVER-flag clear writes a `core_sb`-based SB image (mount-time free counts, RECOVER bit
    /// cleared, csum recomputed) through the journal bridge metadata writer; with no active handle at
    /// mount, that bridge write goes straight to the home superblock — byte-frozen vs ext4_rs's
    /// `inner.super_block.sync_to_disk_with_csum(&inner.metadata_writer)`.
    fn finalize_recovered_superblock(&self) -> Result<()> {
        self.block_device
            .sync()
            .map_err(|_| Error::with_message(Errno::EIO, "failed to sync recovered JBD2 blocks"))?;

        let io_epoch = self.prepare_ext4_io();
        let superblock_result = {
            let runtime_wait_start_ns = Self::monotonic_nanos();
            let runtime_guard = EXT4_RS_RUNTIME_LOCK.lock();
            self.record_ext4_rs_runtime_lock_wait(
                Self::monotonic_nanos().saturating_sub(runtime_wait_start_ns),
            );
            let runtime_hold_start_ns = Self::monotonic_nanos();
            // Clear the in-memory RECOVER flag + persist the cleared SB through the journal bridge.
            let writer: Arc<dyn Ext4MetadataWriter> = self.journal_io.clone();
            self.persist_recover_flag(false, writer.as_ref());
            drop(runtime_guard);
            self.record_ext4_rs_runtime_lock_hold(
                Self::monotonic_nanos().saturating_sub(runtime_hold_start_ns),
            );
            Ok::<(), Error>(())
        };
        let io_result = self.finish_ext4_io(io_epoch);
        superblock_result?;
        io_result?;

        self.block_device
            .sync()
            .map_err(|_| Error::with_message(Errno::EIO, "failed to sync cleared recovery flag"))?;
        Ok(())
    }

    /// Set / clear the in-memory `recover_flag` and persist a `core_sb`-based superblock image (the
    /// frozen mount snapshot with mount-time free counts) carrying the toggled RECOVER bit and a
    /// recomputed checksum, written via `writer`.
    ///
    /// PARITY: ext4_rs `inner.super_block.set_needs_recovery(enabled) +
    /// sync_to_disk_with_csum(&metadata_writer)`. ext4_rs's `inner.super_block` is the FROZEN mount
    /// snapshot (alloc/free never touch it), so the persisted SB carries mount-time free counts; we
    /// replicate that exactly by starting from `core_sb`. The `recover_flag` atomic mirrors ext4_rs's
    /// in-memory RECOVER bit so the lazy first-commit fast path can short-circuit.
    pub(super) fn persist_recover_flag(&self, enabled: bool, writer: &dyn Ext4MetadataWriter) {
        self.recover_flag.store(enabled, Ordering::Release);
        let mut sb = self.core_sb;
        sb.set_needs_recovery(enabled);
        sb.recompute_csum();
        // PARITY: ext4_rs writes the full 1024-byte SB to byte offset 1024 (`SUPERBLOCK_OFFSET`).
        writer.write_metadata(EXT4_SUPERBLOCK_OFFSET, sb.as_bytes());
    }

    fn estimate_jbd2_reserved_blocks(op: Option<&JournaledOp>) -> u32 {
        match op {
            Some(JournaledOp::Create) | Some(JournaledOp::Mkdir) => 8,
            Some(JournaledOp::Unlink) | Some(JournaledOp::Rmdir) => 8,
            Some(JournaledOp::Rename) => 12,
            Some(JournaledOp::Write { len, .. }) => {
                let blocks = len.div_ceil(EXT4_BLOCK_SIZE);
                u32::try_from(blocks.saturating_add(8)).unwrap_or(u32::MAX)
            }
            Some(JournaledOp::Truncate { .. }) | Some(JournaledOp::InodeMetadata { .. }) => 8,
            None => 8,
        }
    }

    fn jbd2_handle_op_name(op: Option<&JournaledOp>) -> &'static str {
        match op {
            Some(JournaledOp::Create) => "create",
            Some(JournaledOp::Mkdir) => "mkdir",
            Some(JournaledOp::Unlink) => "unlink",
            Some(JournaledOp::Rmdir) => "rmdir",
            Some(JournaledOp::Rename) => "rename",
            Some(JournaledOp::Write { .. }) => "write",
            Some(JournaledOp::Truncate { .. }) => "truncate",
            Some(JournaledOp::InodeMetadata { .. }) => "inode_metadata",
            None => "anonymous",
        }
    }

    /// Start a JBD2 handle for `op`, returning its unique `handle_id`.
    ///
    /// PARITY: ext4_rs `start_jbd2_handle` → `register_handle(reserved_blocks, trigger_op)`. The
    /// `mark_handle_requires_data_sync` flag was debug/accounting only (data-sync was always
    /// ordered-mode in practice), so it is dropped. `trigger_op` IS still tracked (it drives the
    /// injected-crash replay hold): a real op passes `Some(name)`, an anonymous (`None`) handle passes
    /// `None` so it preserves the transaction's prior trigger_op — mirroring ext4_rs `register_handle`
    /// (overwrite only when `trigger_op.is_some()`). The reserved-blocks estimate is unchanged.
    fn start_jbd2_handle(&self, op: Option<&JournaledOp>) -> Option<u64> {
        let reserved_blocks = Self::estimate_jbd2_reserved_blocks(op);
        // `Some(name)` for a real op, `None` for the anonymous handle — matches ext4_rs's
        // `Option<&'static str>` trigger_op (None = preserve prior, Some = overwrite, last-real-wins).
        let trigger_op = op.map(|_| Self::jbd2_handle_op_name(op));
        let mut runtime_guard = self.jbd2_runtime.write();
        let driver = runtime_guard.as_mut()?;
        driver.start_handle(reserved_blocks, trigger_op)
    }

    fn next_alloc_operation_id(&self) -> u64 {
        self.next_alloc_operation_id
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1)
            .max(1)
    }

    fn begin_alloc_operation(&self, operation_id: Option<u64>) -> u64 {
        let operation_id = operation_id.unwrap_or_else(|| self.next_alloc_operation_id());
        self.alloc_guard.begin_operation(operation_id);
        operation_id
    }

    fn finish_alloc_operation(&self, operation_id: Option<u64>) {
        if let Some(operation_id) = operation_id {
            self.alloc_guard.finish_operation(operation_id);
        }
    }

    fn finish_jbd2_handle(
        &self,
        handle: Option<u64>,
        op: Option<&JournaledOp>,
        op_name: &'static str,
        succeeded: bool,
    ) {
        let Some(handle_id) = handle else {
            return;
        };
        let summary = self
            .jbd2_runtime
            .write()
            .as_mut()
            .and_then(|driver| driver.stop_handle(handle_id));
        let Some((transaction_id, has_metadata)) = summary else {
            return;
        };

        // BUG-7 fix: a journaled op that errored mid-handle must NOT leave its partial / inconsistent
        // metadata in the running transaction to be committed by a later commit. Abort (discard) the
        // transaction's uncommitted buffers + revokes now. Discarding uncommitted, never-fsync'd
        // metadata is always crash-consistent (equivalent to those ops crashing before commit), so no
        // FS shutdown is required — only the durable, committed prior transactions survive. The
        // RUNTIME lock held across the whole op serializes ops, so this drops only this op's (and any
        // batched-but-unsynced prior op's) uncommitted work, never a concurrent op's. [对照] Linux
        // `jbd2_journal_abort` discards the transaction; we drop the in-memory tx and keep the FS
        // writable since nothing durable is lost.
        if !succeeded {
            let aborted = self
                .jbd2_runtime
                .write()
                .as_mut()
                .map(|driver| driver.abort_transaction(transaction_id))
                .unwrap_or(false);
            if aborted {
                warn!(
                    "ext4: aborted JBD2 transaction tid={} after failed op={} (uncommitted metadata discarded)",
                    transaction_id, op_name,
                );
                // Drop any inode→tid mapping that targeted the now-discarded tid so a later
                // fsync(ino) does not block forever waiting for a transaction that will never commit.
                // (The aborted tid's metadata is gone; the next write re-derives a fresh tid.)
                let mut tids = self.inode_tids.write();
                tids.retain(|_, t| *t != transaction_id);
            }
            self.commit_notifier.wake_all();
            return;
        }

        // Step 4a-2: record (ino → handle's TID) so a subsequent fsync(ino)
        // can force-commit exactly the TID containing this inode's metadata.
        // Only single-inode ops carry inode info in v1; directory ops touch
        // multiple inodes (parent + child) and rely on the Phase 2 inode
        // correctness lock to serialize fsync after the op completes.
        // PARITY: ext4_rs gated on `summary.modified_blocks > 0`.
        if succeeded && has_metadata {
            if let Some(ino) = op.and_then(JournaledOp::affected_ino) {
                self.record_inode_tid(ino, transaction_id);
            }
        }
        // Wake any fsync waiter blocked on this transaction (a stop_handle on
        // prev_running may have made it commit_ready below).
        self.commit_notifier.wake_all();

        debug!(
            "ext4: jbd2 handle op={} handle_id={} tid={} has_metadata={} success={}",
            op_name, handle_id, transaction_id, has_metadata, succeeded,
        );

        if succeeded {
            let (rotated_tid, batch_commit_ready) = {
                let mut rt = self.jbd2_runtime.write();
                let Some(driver) = rt.as_mut() else {
                    return;
                };
                let rotated_tid =
                    if driver.should_rotate_running_transaction(JOURNAL_COMMIT_BATCH_BLOCKS) {
                        driver.rotate_running_transaction()
                    } else {
                        None
                    };
                let batch_commit_ready = driver.batch_commit_ready(JOURNAL_COMMIT_BATCH_BLOCKS);
                (rotated_tid, batch_commit_ready)
            };
            if let Some(tid) = rotated_tid {
                debug!(
                    "ext4: rotated JBD2 running transaction tid={} after batch threshold",
                    tid
                );
            }
            if batch_commit_ready {
                let _ = self.try_commit_ready_jbd2_transaction();
            }
            if Self::should_force_commit_for_injected_crash(op_name) {
                let _ = self.try_commit_ready_jbd2_transaction();
            }
        }
    }

    fn has_active_jbd2_handle(&self) -> bool {
        self.jbd2_runtime
            .read()
            .as_ref()
            .is_some_and(|driver| driver.has_active_handle())
    }

    /// Step 4a-2: returns the highest TID whose `finish_commit` succeeded.
    /// `0` means no transaction has committed yet (post-mount fresh state)
    /// — fsync of an inode whose recorded TID is also 0 is therefore a no-op
    /// (no metadata change).
    pub(super) fn last_committed_tid(&self) -> u32 {
        self.jbd2_runtime
            .read()
            .as_ref()
            .map(|driver| driver.last_committed_tid())
            .unwrap_or(0)
    }

    /// Step 4a-2: record that this inode's metadata is committed in TID
    /// `tid` (or earlier). Always advances monotonically. Called by
    /// `finish_jbd2_handle` after a Write/Truncate handle stops.
    fn record_inode_tid(&self, ino: u32, tid: u32) {
        if tid == 0 {
            return;
        }
        let mut tids = self.inode_tids.write();
        let entry = tids.entry(ino).or_insert(0);
        if tid > *entry {
            *entry = tid;
        }
    }

    /// Step 4a-2: returns the latest TID with a known metadata change for
    /// this inode, or `None` if no Write/Truncate has been recorded.
    pub(super) fn lookup_inode_tid(&self, ino: u32) -> Option<u32> {
        self.inode_tids.read().get(&ino).copied()
    }

    /// Step 4a-2: drive forward and wait until the JBD2 transaction
    /// containing this inode's recent metadata changes is durable in the
    /// journal.  Mirrors Linux `jbd2_journal_force_commit_nested` +
    /// `jbd2_log_wait_commit` semantics.
    ///
    /// Algorithm:
    /// 1. Fast path: if `last_committed_tid >= target_tid`, return.
    /// 2. If `target_tid` is the current running TX, rotate it to
    ///    `prev_running` (so no new handles join, allowing existing handles
    ///    to drain to commit-readiness).
    /// 3. Loop: try `try_commit_ready_jbd2_transaction()` to drive any
    ///    commit-ready TX to disk; if `last_committed_tid < target_tid`,
    ///    block on `commit_notifier` until either a finish_commit or a
    ///    handle stop wakes us, then re-check.
    ///
    /// Wakeup correctness: every `finish_commit` and every `stop_handle`
    /// calls `commit_notifier.wake_all()`, so any state change that could
    /// advance `last_committed_tid` notifies waiters.  `WaitQueue::wait_until`
    /// enqueues the waker before re-evaluating the condition, so no wakeup
    /// is lost.
    pub(super) fn force_commit_for_tid(&self, target_tid: u32) {
        if target_tid == 0 {
            return;
        }
        // Fast path: already committed.
        if self.last_committed_tid() >= target_tid {
            return;
        }

        // Rotate target_tid out of `running` if it's still there. This
        // prevents new handles from joining and lets existing ones drain.
        {
            let mut runtime_guard = self.jbd2_runtime.write();
            if let Some(driver) = runtime_guard.as_mut() {
                let running_tid = driver.running_transaction_tid().unwrap_or(0);
                if running_tid == target_tid {
                    let _ = driver.rotate_running_transaction();
                }
            }
        }

        // Wait until target_tid is committed.  In each iteration we first
        // try to drive any commit-ready TX forward (this is what advances
        // `last_committed_tid`), then check the condition.  If still not
        // satisfied, `wait_until` enqueues us on `commit_notifier` and the
        // next `finish_commit` / `stop_handle` will wake us up.
        self.commit_notifier.wait_until(|| {
            // Drive forward: this commits the prev_running TX once its
            // active handles have drained, advancing `last_committed_tid`.
            let _ = self.try_commit_ready_jbd2_transaction();
            if self.last_committed_tid() >= target_tid {
                Some(())
            } else {
                None
            }
        });
    }

    pub(super) fn flush_pending_jbd2_transactions(&self) {
        // Drain any pending commit first (there should be at most one).
        while {
            let rt = self.jbd2_runtime.read();
            rt.as_ref().is_some_and(|rt| rt.commit_ready())
        } {
            if !self.try_commit_ready_jbd2_transaction() {
                break;
            }
        }
        // Batch checkpoint all accumulated transactions with a single disk flush,
        // rather than one flush per transaction.
        self.try_batch_checkpoint_all_jbd2_transactions();
    }

    fn commit_pending_jbd2_transactions(&self) {
        while {
            let rt = self.jbd2_runtime.read();
            rt.as_ref().is_some_and(|rt| rt.commit_ready())
        } {
            if !self.try_commit_ready_jbd2_transaction() {
                break;
            }
        }
    }

    pub(super) fn checkpoint_depth(&self) -> usize {
        self.jbd2_runtime
            .read()
            .as_ref()
            .map(|driver| driver.checkpoint_depth())
            .unwrap_or(0)
    }

    fn reconcile_jbd2_checkpoint_tail(&self) {
        // The journal tail and the checkpoint list are now owned by the SAME driver object (the
        // core-backed `CoreJournalDriver`), so they can never drift the way the old separate ext4_rs
        // `Jbd2Journal.space` and `JournalRuntime.checkpoint_list` could. We still call
        // `discard_checkpointed_before_tail(tail)` against the driver's own tail for parity safety —
        // it is a no-op when the front entry already starts at the tail (the normal case).
        let mut runtime_guard = self.jbd2_runtime.write();
        let Some(driver) = runtime_guard.as_mut() else {
            return;
        };
        let current_tail = driver.space_tail();
        let dropped = driver.discard_checkpointed_before_tail(current_tail);
        if dropped != 0 {
            debug!(
                "ext4: reconciled {} stale JBD2 checkpoint transactions at tail={}",
                dropped, current_tail
            );
        }
    }

    /// Checkpoints all pending transactions with a single BioType::Flush.
    /// Each individual checkpoint still advances the journal tail and updates the
    /// superblock, but home block writes are batched and synced together.
    ///
    /// PARITY: ext4_rs `try_batch_checkpoint_all_jbd2_transactions` — home-write ALL plans → ONE
    /// `block_device.sync()` → per-tx tail-advance + journal-SB `s_start` store (now via the
    /// core-backed driver's `checkpoint_transaction`). The single sync between home writes and the
    /// SB stores is preserved exactly.
    pub(super) fn try_batch_checkpoint_all_jbd2_transactions(&self) -> bool {
        let _checkpoint_guard = self.jbd2_checkpoint_lock.lock();
        self.reconcile_jbd2_checkpoint_tail();
        let (plans, block_size) = {
            let runtime_guard = self.jbd2_runtime.read();
            let Some(driver) = runtime_guard.as_ref() else {
                return false;
            };
            if !driver.checkpoint_ready() {
                return false;
            }
            (driver.all_checkpoint_plans(), driver.block_size())
        };
        if plans.is_empty() {
            return false;
        }

        // Write home blocks for ALL checkpoint transactions before syncing.
        for plan in &plans {
            for (block_nr, image) in &plan.metadata_blocks {
                let Some(block_offset) = (*block_nr as usize).checked_mul(block_size) else {
                    warn!(
                        "ext4: batch checkpoint block offset overflow block_nr={} block_size={}",
                        block_nr, block_size
                    );
                    continue;
                };
                self.adapter.write_offset(block_offset, image);
            }
        }

        // Single sync for all home blocks.
        if let Err(err) = self.block_device.sync() {
            warn!(
                "ext4: batch checkpoint sync failed ({} transactions): {:?}",
                plans.len(),
                err
            );
            return false;
        }

        // The journal-SB store for each checkpoint goes straight to the device (the home blocks are
        // already durable above; this is just the s_start metadata update).
        let writer =
            super::core_adapter::CoreCommitMetadataWriter::new(self.adapter.clone(), block_size);

        // Now finish each checkpoint individually (advances tail + updates journal SB).
        let mut any_checkpointed = false;
        for plan in &plans {
            let mut runtime_guard = self.jbd2_runtime.write();
            let Some(driver) = runtime_guard.as_mut() else {
                break;
            };
            let next_start = match driver.next_checkpoint_start_after(plan.tid) {
                Some(ns) => ns,
                None => break,
            };
            if driver.space_tail() != plan.start_block {
                warn!(
                    "ext4: batch checkpoint tail mismatch tid={} current_tail={} start={} next_head={}",
                    plan.tid,
                    driver.space_tail(),
                    plan.start_block,
                    plan.next_head
                );
            }
            match driver.checkpoint_transaction(
                &writer,
                0,
                plan.tid,
                plan.start_block,
                plan.next_head,
                next_start,
            ) {
                Ok(_) => {
                    any_checkpointed = true;
                }
                Err(err) => {
                    warn!(
                        "ext4: batch checkpoint tail update failed tid={}: {:?}",
                        plan.tid, err
                    );
                    break;
                }
            }
        }

        if any_checkpointed {
            warn!(
                "ext4: batch checkpointed {} transactions with single sync",
                plans.len()
            );
        }
        any_checkpointed
    }

    /// Commit the next commit-ready JBD2 transaction to the journal via the **core** emitter.
    ///
    /// PARITY: the old ext4_rs-driving body. The commit step (`prepare_commit` → core
    /// `write_commit_plan` → park-in-checkpoint-list) now drives the core-backed
    /// [`CoreJournalDriver`]. The single ordered-mode `sync()` barrier is inside core's
    /// `write_commit_plan` (descriptor+payload → ONE `sync()` → commit block → SB → ring) — we pass
    /// it the same `block_device.sync()` ext4_rs issued. The pre-commit space check, the
    /// injected-crash replay hold, the lazy checkpoint, and `mark_needs_recovery_if_needed` are
    /// replicated 逐位.
    fn try_commit_ready_jbd2_transaction(&self) -> bool {
        let prepared = {
            let mut runtime_guard = self.jbd2_runtime.write();
            let Some(driver) = runtime_guard.as_mut() else {
                return false;
            };
            if !driver.commit_ready() {
                return false;
            }
            driver.prepare_commit()
        };
        let Some((plan, trigger_op)) = prepared else {
            return false;
        };

        // Pre-commit space check: if journal is running low, checkpoint first to make room.
        // This prevents ENOSPC failures inside write_commit_plan without busy-looping.
        let required = plan.metadata_blocks.len() as u32 + 2;
        let free_before = self
            .jbd2_runtime
            .read()
            .as_ref()
            .map(|driver| driver.space_free_blocks())
            .unwrap_or(u32::MAX);
        if free_before < required.saturating_add(JOURNAL_LOW_WATER_MARK) {
            // Batch-checkpoint all pending transactions in one sync rather than one
            // sync per transaction. This keeps journal free space high and avoids
            // a sync on every commit once the journal fills up (e.g. ext4/045).
            self.try_batch_checkpoint_all_jbd2_transactions();
            let free_after = self
                .jbd2_runtime
                .read()
                .as_ref()
                .map(|driver| driver.space_free_blocks())
                .unwrap_or(u32::MAX);
            if free_after < required {
                warn!(
                    "ext4: journal out of space tid={} free={} required={}, aborting commit",
                    plan.tid, free_after, required
                );
                let _ = self
                    .jbd2_runtime
                    .write()
                    .as_mut()
                    .map(|driver| driver.abort_commit(plan.tid));
                return false;
            }
        }

        // Virtio-blk writes are synchronous DMA — data reaches the host before this
        // call returns, so ordering relative to the journal commit block is already
        // guaranteed by the write queue.  An explicit BioType::Flush here would add
        // ~50 ms per Write operation (hundreds of writes in generic/013) for no
        // benefit in guest-crash-only recovery scenarios that xfstests exercises.

        // The single ordered-mode barrier: `block_device.sync()`, issued from inside the core
        // emitter between the commit payloads and the commit block. Same point ext4_rs used.
        let sync_fn = || -> Result<()> {
            self.block_device
                .sync()
                .map(|_| ())
                .map_err(|_| Error::with_message(Errno::EIO, "journal commit barrier sync failed"))
        };
        let barrier = super::journal_driver::SyncBarrier::new(&sync_fn);
        // The commit emitter writes the journal-area home blocks straight to the device.
        let writer = super::core_adapter::CoreCommitMetadataWriter::new(
            self.adapter.clone(),
            self.jbd2_runtime
                .read()
                .as_ref()
                .map(|d| d.block_size())
                .unwrap_or(EXT4_BLOCK_SIZE),
        );

        let (write_result, free_after_commit) = {
            let mut runtime_guard = self.jbd2_runtime.write();
            let Some(driver) = runtime_guard.as_mut() else {
                return false;
            };
            let result = driver.write_commit_plan_with_hook(
                &writer,
                Some(&barrier),
                0,
                &plan,
                |stage| {
                    if let Some(op_name) = trigger_op {
                        if Self::should_hold_for_injected_crash(op_name, stage) {
                            warn!(
                                "ext4: replay hold point reached for op={} stage={} (kill VM now to simulate power loss)",
                                op_name,
                                Self::jbd2_commit_stage_name(stage),
                            );
                            loop {
                                core::hint::spin_loop();
                            }
                        }
                    }
                },
            );
            let free = driver.space_free_blocks();
            (result, free)
        };

        match write_result {
            Ok(tid) => {
                // Step 4a-2: wake any fsync waiter that was blocked on this TID.
                self.commit_notifier.wake_all();
                // Step 4b: ensure on-disk superblock has the
                // EXT4_FEATURE_INCOMPAT_RECOVER ("needs_recovery") flag set
                // after the first commit since last clean SB.
                self.mark_needs_recovery_if_needed();
                warn!(
                    "ext4: jbd2 committed tid={} metadata_blocks={} free_blocks={}",
                    tid,
                    plan.metadata_blocks.len(),
                    free_after_commit,
                );
                // Lazy checkpoint: flush home blocks when journal space is tight,
                // OR when the in-memory checkpoint list has grown deep enough to
                // threaten the kernel heap. Batch to amortize the sync cost.
                if free_after_commit < JOURNAL_CHECKPOINT_THRESHOLD
                    || self.checkpoint_depth() >= JOURNAL_CHECKPOINT_MAX_DEPTH
                {
                    self.try_batch_checkpoint_all_jbd2_transactions();
                }
                true
            }
            Err(err) => {
                warn!(
                    "ext4: failed to write JBD2 commit plan tid={} metadata_blocks={}: {:?}",
                    plan.tid,
                    plan.metadata_blocks.len(),
                    err
                );
                let _ = self
                    .jbd2_runtime
                    .write()
                    .as_mut()
                    .map(|driver| driver.abort_commit(plan.tid));
                false
            }
        }
    }

    pub(super) fn prepare_ext4_io(&self) -> u64 {
        self.adapter.begin_io_operation()
    }

    pub(super) fn finish_ext4_io(&self, io_epoch: u64) -> Result<()> {
        if self.adapter.io_failed_since(io_epoch) {
            return_errno_with_message!(Errno::EIO, "ext4 block I/O failure");
        }
        Ok(())
    }

    /// File-read variant: IO-epoch only (no runtime lock), mirroring `run_ext4_file_read_only`.
    pub(super) fn run_io_file_read_only<T>(
        &self,
        f: impl FnOnce(&super::core::file::ReadCtx) -> Result<T>,
    ) -> Result<T> {
        let io_epoch = self.prepare_ext4_io();
        let reader = self.core_reader();
        let result = {
            let ctx = super::core::file::ReadCtx::new(&reader, &self.core_sb);
            f(&ctx)
        };
        let io_result = self.finish_ext4_io(io_epoch);
        io_result?;
        result
    }

    /// Dir-read variant: IO-epoch only (no runtime lock), mirroring `run_ext4_dir_read_only`.
    pub(super) fn run_io_dir_read_only<T>(
        &self,
        f: impl FnOnce(&super::core::file::ReadCtx) -> Result<T>,
    ) -> Result<T> {
        let io_epoch = self.prepare_ext4_io();
        let reader = self.core_reader();
        let result = {
            let ctx = super::core::file::ReadCtx::new(&reader, &self.core_sb);
            f(&ctx)
        };
        let io_result = self.finish_ext4_io(io_epoch);
        io_result?;
        result
    }

    /// Dir-read non-fallible variant: IO-epoch only, mirroring `run_ext4_dir_read_only_noerr`.
    pub(super) fn run_io_dir_read_only_noerr<T>(
        &self,
        f: impl FnOnce(&super::core::file::ReadCtx) -> T,
    ) -> Result<T> {
        let io_epoch = self.prepare_ext4_io();
        let reader = self.core_reader();
        let result = {
            let ctx = super::core::file::ReadCtx::new(&reader, &self.core_sb);
            f(&ctx)
        };
        let io_result = self.finish_ext4_io(io_epoch);
        io_result?;
        Ok(result)
    }

    /// IO-epoch + runtime-lock variant, mirroring `run_ext4_read_only`.
    pub(super) fn run_io_read_only<T>(
        &self,
        f: impl FnOnce(&super::core::file::ReadCtx) -> Result<T>,
    ) -> Result<T> {
        let io_epoch = self.prepare_ext4_io();
        let runtime_wait_start_ns = Self::monotonic_nanos();
        let runtime_guard = EXT4_RS_RUNTIME_LOCK.lock();
        self.record_ext4_rs_runtime_lock_wait(
            Self::monotonic_nanos().saturating_sub(runtime_wait_start_ns),
        );
        let runtime_hold_start_ns = Self::monotonic_nanos();
        let reader = self.core_reader();
        let result = {
            let ctx = super::core::file::ReadCtx::new(&reader, &self.core_sb);
            f(&ctx)
        };
        drop(runtime_guard);
        self.record_ext4_rs_runtime_lock_hold(
            Self::monotonic_nanos().saturating_sub(runtime_hold_start_ns),
        );
        let io_result = self.finish_ext4_io(io_epoch);
        io_result?;
        result
    }

    /// IO-epoch + runtime-lock non-fallible variant, mirroring `run_ext4_read_only_noerr`.
    ///
    /// Provided for completeness alongside the fallible `run_io_read_only` (Task 1's `stat` uses
    /// the fallible variant because `load_inode` validates the inode number). Reserved for
    /// later-task read sites that drive a non-fallible core op.
    #[allow(dead_code)]
    fn run_io_read_only_noerr<T>(
        &self,
        f: impl FnOnce(&super::core::file::ReadCtx) -> T,
    ) -> Result<T> {
        let io_epoch = self.prepare_ext4_io();
        let runtime_wait_start_ns = Self::monotonic_nanos();
        let runtime_guard = EXT4_RS_RUNTIME_LOCK.lock();
        self.record_ext4_rs_runtime_lock_wait(
            Self::monotonic_nanos().saturating_sub(runtime_wait_start_ns),
        );
        let runtime_hold_start_ns = Self::monotonic_nanos();
        let reader = self.core_reader();
        let result = {
            let ctx = super::core::file::ReadCtx::new(&reader, &self.core_sb);
            f(&ctx)
        };
        drop(runtime_guard);
        self.record_ext4_rs_runtime_lock_hold(
            Self::monotonic_nanos().saturating_sub(runtime_hold_start_ns),
        );
        let io_result = self.finish_ext4_io(io_epoch);
        io_result?;
        Ok(result)
    }

    fn jbd2_op_name_from_bytes(name: &[u8]) -> Option<&'static str> {
        match name {
            b"create" => Some("create"),
            b"mkdir" => Some("mkdir"),
            b"unlink" => Some("unlink"),
            b"rmdir" => Some("rmdir"),
            b"rename" => Some("rename"),
            b"write" => Some("write"),
            b"truncate" => Some("truncate"),
            _ => None,
        }
    }

    fn jbd2_commit_stage_name(stage: JournalCommitWriteStage) -> &'static str {
        match stage {
            JournalCommitWriteStage::BeforeDescriptor => "before_commit",
            JournalCommitWriteStage::BeforeCommitBlock => "before_commit_block",
            JournalCommitWriteStage::AfterCommitBlock => "after_commit_block",
            JournalCommitWriteStage::AfterSuperblock => "after_commit",
        }
    }

    fn jbd2_commit_stage_from_name(name: &[u8]) -> Option<JournalCommitWriteStage> {
        match name {
            b"before_commit" | b"before_descriptor" => {
                Some(JournalCommitWriteStage::BeforeDescriptor)
            }
            b"mid_commit" | b"before_commit_block" => {
                Some(JournalCommitWriteStage::BeforeCommitBlock)
            }
            b"after_commit_block" => Some(JournalCommitWriteStage::AfterCommitBlock),
            b"after_commit" | b"after_superblock" => Some(JournalCommitWriteStage::AfterSuperblock),
            _ => None,
        }
    }

    fn replay_hold_request(op_name: &str) -> Option<JournalCommitWriteStage> {
        let Some(kcmd) = KCMDLINE.get() else {
            return None;
        };
        let Some(args) = kcmd.get_module_args("ext4fs") else {
            return None;
        };

        let mut enabled = false;
        let mut op_filter: Option<&'static str> = None;
        let mut stage = JournalCommitWriteStage::AfterSuperblock;
        for arg in args {
            match arg {
                ModuleArg::Arg(key) => {
                    if key.as_c_str().to_bytes() == b"replay_hold" {
                        enabled = true;
                    }
                }
                ModuleArg::KeyVal(key, value) => {
                    let key = key.as_c_str().to_bytes();
                    let value = value.as_c_str().to_bytes();
                    if key == b"replay_hold" {
                        if value == b"1" || value == b"true" || value == b"yes" {
                            enabled = true;
                        }
                    } else if key == b"replay_hold_op" {
                        op_filter = Self::jbd2_op_name_from_bytes(value);
                    } else if key == b"replay_hold_stage" {
                        if let Some(parsed) = Self::jbd2_commit_stage_from_name(value) {
                            stage = parsed;
                        }
                    }
                }
            }
        }

        if !enabled {
            return None;
        }
        let op_matches = match op_filter {
            Some(filter_op) => filter_op == op_name,
            None => true,
        };
        if op_matches { Some(stage) } else { None }
    }

    fn should_force_commit_for_injected_crash(op_name: &str) -> bool {
        Self::replay_hold_request(op_name).is_some()
    }

    fn should_hold_for_injected_crash(op_name: &str, stage: JournalCommitWriteStage) -> bool {
        Self::replay_hold_request(op_name).is_some_and(|requested| requested == stage)
    }

    /// Phase 6 Task 2: the journaled-write chokepoint, but driving **core**'s `file::*` write fns
    /// instead of the scoped ext4_rs engine. Same lock order + same JBD2 handle lifecycle + same
    /// cache-invalidation as `run_journaled_ext4`/[`run_journaled_core`]; only the engine inside `apply` changes.
    ///
    /// `apply` receives a fully-built core write context (`WriteCtx` over the overlay reader + the
    /// active-handle metadata writer + the data writer), a single `CoreBlockAlloc` (one running
    /// superblock + bitmap state, so tree/data block allocation stay consistent), and the loaded
    /// target inode. It calls one `core::file::X` (which writes the inode + bitmaps + group descs +
    /// SB back through the metadata writer into the active JBD2 transaction).
    ///
    /// **Two-engine free-counter coherency (C1 fix)**: ext4_rs keeps its **authoritative** free
    /// counters in `allocator_locks.superblock` (a `Mutex<Ext4Superblock>`), NOT the plain
    /// `Ext4.super_block` field (which is frozen at the mount value — ext4_rs alloc/free never touch
    /// it). Every ext4_rs block alloc/free (`subtract/add_superblock_free_blocks`) and inode alloc/free
    /// (`decrease/increase_superblock_free_inodes`) mutates that mutex copy and persists the full
    /// 1024-byte SB. Core's `balloc::write_superblock` likewise rewrites the WHOLE 1024-byte SB on
    /// every block alloc, so it MUST carry the correct `s_free_blocks_count` **and**
    /// `s_free_inodes_count`, else it clobbers the on-disk counters and e2fsck reports "Free
    /// inodes/blocks count wrong". We therefore seed BOTH counters from the authoritative mutex copy
    /// (`lock_superblock_counter()`), and sink the post-op `free_blocks` back into that same copy so a
    /// subsequent ext4_rs namespace op sees core's decrement. `free_inodes` is unchanged by file
    /// writes, so it round-trips once seeded. Geometry/other fields stay from the immutable mount
    /// snapshot `core_sb`. (ext4.rs:14-42 is the complete set of mutable SB counters ext4_rs touches.)
    pub(super) fn run_journaled_core<T>(
        &self,
        op: Option<JournaledOp>,
        ino: u32,
        apply: impl FnOnce(
            &super::core::extents::WriteCtx,
            &mut super::core_adapter::CoreBlockAlloc<
                super::core_adapter::CoreDeviceReader,
                super::core_adapter::CoreMetadataWriter,
            >,
            &mut super::core::inode::Inode,
        ) -> Result<T>,
    ) -> Result<T> {
        use super::core::balloc::BlockAllocator;

        self.check_not_shutdown()?;

        let generic014_like_write = matches!(
            op.as_ref(),
            Some(JournaledOp::Write { len, .. }) if *len == 512
        );
        let io_epoch = self.prepare_ext4_io();
        let runtime_wait_start_ns = Self::monotonic_nanos();
        let runtime_guard = EXT4_RS_RUNTIME_LOCK.lock();
        self.record_ext4_rs_runtime_lock_wait(
            Self::monotonic_nanos().saturating_sub(runtime_wait_start_ns),
        );
        let runtime_hold_start_ns = Self::monotonic_nanos();
        let profile_start_ns = Self::monotonic_nanos();
        let op_name = Self::jbd2_handle_op_name(op.as_ref());

        let start_handle_start_ns = Self::monotonic_nanos();
        let handle_id = self.start_jbd2_handle(op.as_ref());
        let start_handle_elapsed_ns = Self::monotonic_nanos().saturating_sub(start_handle_start_ns);
        let alloc_operation_id = self.begin_alloc_operation(handle_id);

        let apply_start_ns = Self::monotonic_nanos();
        let result = {
            // Phase 6 Task 4 (★SB ownership): seed the per-op running SB from the **core-owned**
            // authoritative running SB (`running_sb` — the single source of truth for free counts,
            // replacing ext4_rs's `allocator_locks` mutex). Geometry stays at the immutable mount
            // values (`running_sb` only ever has its free counts mutated). The C1 two-engine
            // seed/sink is gone: there is no ext4_rs mirror to read from or write back to.
            let mut sb = *self.running_sb.lock();

            let reader = super::core_adapter::CoreDeviceReader::new(self.journal_io.clone());
            let writer = super::core_adapter::CoreMetadataWriter::new(
                self.jbd2_runtime.clone(),
                handle_id.unwrap_or(0),
            );
            let data_writer = super::core_adapter::CoreDataWriter::new(self.adapter.clone());

            let r = (|| -> Result<(T, u64)> {
                let mut inode = super::core::inode::load_inode(&reader, &sb, ino)?;
                let block_alloc = BlockAllocator::new(sb, &reader, &writer);
                let mut alloc =
                    super::core_adapter::CoreBlockAlloc::new(block_alloc, inode.blocks_count());
                let ctx = super::core::extents::WriteCtx::new(&reader, &writer, &data_writer, &sb);
                let value = apply(&ctx, &mut alloc, &mut inode)?;
                // Harvest the post-op running free-block count from the per-op allocator's SB.
                let new_free = alloc.superblock().free_blocks_count();
                Ok((value, new_free))
            })();

            match r {
                Ok((value, new_free)) => {
                    // Sink the post-op free-block count back into the core-owned running SB so the
                    // next journaled/namespace op seeds from core's decrement. core's
                    // `write_superblock` already persisted the full SB (both counters) to disk during
                    // the op; this keeps the in-memory authoritative copy coherent with disk.
                    self.running_sb.lock().set_free_blocks_count(new_free);
                    Ok(value)
                }
                // On partial failure we do NOT roll the running SB counter back: ext4_rs had the same
                // no-rollback behavior (its alloc/free persist the SB eagerly per step), and the
                // failed transaction's metadata images (incl. any SB write) are dropped when the
                // handle stops without commit, so the in-memory authoritative copy and the
                // journaled-but-uncommitted SB image disagree only until that tx is discarded.
                Err(err) => Err(err),
            }
        };
        let apply_elapsed_ns = Self::monotonic_nanos().saturating_sub(apply_start_ns);

        // Same inode-meta / coverage cache invalidation as `run_journaled_ext4`.
        self.meta_cache_generation.fetch_add(1, Ordering::Release);
        match op.as_ref() {
            Some(JournaledOp::Write { ino, .. }) | Some(JournaledOp::InodeMetadata { ino }) => {
                self.inode_meta_cache.lock().remove(ino);
            }
            Some(JournaledOp::Truncate { ino }) => {
                self.inode_meta_cache.lock().remove(ino);
                self.coverage_invalidate(*ino);
            }
            _ => {
                self.inode_meta_cache.lock().clear();
            }
        }

        let finish_handle_start_ns = Self::monotonic_nanos();
        self.finish_jbd2_handle(handle_id, op.as_ref(), op_name, result.is_ok());
        let finish_handle_elapsed_ns =
            Self::monotonic_nanos().saturating_sub(finish_handle_start_ns);
        self.finish_alloc_operation(Some(alloc_operation_id));
        drop(runtime_guard);
        self.record_ext4_rs_runtime_lock_hold(
            Self::monotonic_nanos().saturating_sub(runtime_hold_start_ns),
        );

        let io_result = self.finish_ext4_io(io_epoch);
        let total_elapsed_ns = Self::monotonic_nanos().saturating_sub(profile_start_ns);
        if self.phase2_profile_enabled {
            self.journaled_op_profile.record(
                op.as_ref(),
                start_handle_elapsed_ns,
                apply_elapsed_ns,
                finish_handle_elapsed_ns,
                0,
                0,
                total_elapsed_ns,
            );
        }
        if generic014_like_write && total_elapsed_ns >= GENERIC014_SLOW_OP_LOG_THRESHOLD_NS {
            debug!(
                "ext4: generic014-like journaled(core) profile apply_ms={} finish_handle_ms={} total_ms={}",
                apply_elapsed_ns / 1_000_000,
                finish_handle_elapsed_ns / 1_000_000,
                total_elapsed_ns / 1_000_000
            );
        }
        match (result, io_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(err), _) => Err(err),
            (Ok(_), Err(err)) => Err(err),
        }
    }

    /// Phase 6 Task 3: the journaled chokepoint for **namespace** ops (create/mkdir/unlink/rmdir/
    /// rename), driving **core**'s `dir::*` over a `NamespaceCtx`. Same lock order + JBD2 handle
    /// lifecycle + cache invalidation as `run_journaled_ext4`/[`run_journaled_core`]; only the
    /// engine inside `apply` differs.
    ///
    /// `apply` receives a fully-built `&mut NamespaceCtx` (overlay reader + active-handle metadata
    /// writer + data writer + a running superblock seeded from the authoritative free counts). The
    /// closure calls one `core::dir::X`, which internally loads/mutates parent + child inodes,
    /// allocates inodes/blocks (decrementing the running SB's free counts), and writes everything
    /// back through the metadata writer into the active JBD2 transaction.
    ///
    /// **Free-counter coherency — namespace edition.** Namespace ops mutate BOTH
    /// `s_free_inodes_count` (inode alloc via `ialloc`) and `s_free_blocks_count` (directory-block
    /// alloc/free). We seed the running SB's free-block AND free-inode counts from the core-owned
    /// authoritative `running_sb` (the single source of truth). After the op we sink BOTH post-op
    /// counts (`nctx.superblock()`) back into `running_sb`, so all subsequent operations see the
    /// decremented counters that core just persisted to disk (core's `write_superblock` rewrote
    /// the whole 1024-byte SB during the op). Steady-state allocation is single-source throughout.
    pub(super) fn run_journaled_namespace<T>(
        &self,
        op: Option<JournaledOp>,
        apply: impl FnOnce(
            &mut super::core::dir::NamespaceCtx<
                '_,
                super::core_adapter::CoreDeviceReader,
                super::core_adapter::CoreMetadataWriter,
                super::core_adapter::CoreDataWriter,
            >,
        ) -> Result<T>,
    ) -> Result<T> {
        self.check_not_shutdown()?;

        let io_epoch = self.prepare_ext4_io();
        let runtime_wait_start_ns = Self::monotonic_nanos();
        let runtime_guard = EXT4_RS_RUNTIME_LOCK.lock();
        self.record_ext4_rs_runtime_lock_wait(
            Self::monotonic_nanos().saturating_sub(runtime_wait_start_ns),
        );
        let runtime_hold_start_ns = Self::monotonic_nanos();
        let profile_start_ns = Self::monotonic_nanos();
        let op_name = Self::jbd2_handle_op_name(op.as_ref());

        let start_handle_start_ns = Self::monotonic_nanos();
        let handle_id = self.start_jbd2_handle(op.as_ref());
        let start_handle_elapsed_ns = Self::monotonic_nanos().saturating_sub(start_handle_start_ns);
        let alloc_operation_id = self.begin_alloc_operation(handle_id);

        let apply_start_ns = Self::monotonic_nanos();
        let result = {
            // Phase 6 Task 4 (★SB ownership): seed the running SB from the **core-owned**
            // authoritative running SB (`running_sb`). Namespace ops mutate BOTH free counts (inode
            // alloc via `ialloc` + directory-block alloc/free), so we snapshot both. The C1
            // two-engine seed/sink is gone — `running_sb` is the single source of truth.
            let sb = *self.running_sb.lock();

            let reader = super::core_adapter::CoreDeviceReader::new(self.journal_io.clone());
            let writer = super::core_adapter::CoreMetadataWriter::new(
                self.jbd2_runtime.clone(),
                handle_id.unwrap_or(0),
            );
            let data_writer = super::core_adapter::CoreDataWriter::new(self.adapter.clone());

            let r = (|| -> Result<(T, u64, u32)> {
                let mut nctx =
                    super::core::dir::NamespaceCtx::new(&reader, &writer, &data_writer, sb);
                let value = apply(&mut nctx)?;
                // Harvest the post-op running SB: BOTH free counts may have changed (inode alloc +
                // directory-block alloc/free). Core already persisted the full SB to disk; this sinks
                // the in-memory authoritative copy so it stays coherent for the next op.
                let post = nctx.superblock();
                Ok((value, post.free_blocks_count(), post.free_inodes_count()))
            })();

            match r {
                Ok((value, new_free_blocks, new_free_inodes)) => {
                    let mut auth_sb = self.running_sb.lock();
                    auth_sb.set_free_blocks_count(new_free_blocks);
                    auth_sb.set_free_inodes_count(new_free_inodes);
                    Ok(value)
                }
                // On partial failure we do NOT roll the SB counters back — same no-rollback behavior
                // as `run_journaled_core` and ext4_rs (allocators persist the SB eagerly per step;
                // the failed transaction's metadata images are dropped when the handle stops without
                // commit, so the in-memory authoritative copy reconverges once that tx is discarded).
                Err(err) => Err(err),
            }
        };
        let apply_elapsed_ns = Self::monotonic_nanos().saturating_sub(apply_start_ns);

        // Same inode-meta / coverage cache invalidation as `run_journaled_ext4` / `run_journaled_core`.
        // All namespace ops fall through to the conservative clear-all (parent + child inodes touched).
        self.meta_cache_generation.fetch_add(1, Ordering::Release);
        match op.as_ref() {
            Some(JournaledOp::Write { ino, .. }) | Some(JournaledOp::InodeMetadata { ino }) => {
                self.inode_meta_cache.lock().remove(ino);
            }
            Some(JournaledOp::Truncate { ino }) => {
                self.inode_meta_cache.lock().remove(ino);
                self.coverage_invalidate(*ino);
            }
            _ => {
                self.inode_meta_cache.lock().clear();
            }
        }

        let finish_handle_start_ns = Self::monotonic_nanos();
        self.finish_jbd2_handle(handle_id, op.as_ref(), op_name, result.is_ok());
        let finish_handle_elapsed_ns =
            Self::monotonic_nanos().saturating_sub(finish_handle_start_ns);
        self.finish_alloc_operation(Some(alloc_operation_id));
        drop(runtime_guard);
        self.record_ext4_rs_runtime_lock_hold(
            Self::monotonic_nanos().saturating_sub(runtime_hold_start_ns),
        );

        let io_result = self.finish_ext4_io(io_epoch);
        let total_elapsed_ns = Self::monotonic_nanos().saturating_sub(profile_start_ns);
        if self.phase2_profile_enabled {
            self.journaled_op_profile.record(
                op.as_ref(),
                start_handle_elapsed_ns,
                apply_elapsed_ns,
                finish_handle_elapsed_ns,
                0,
                0,
                total_elapsed_ns,
            );
        }
        match (result, io_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(err), _) => Err(err),
            (Ok(_), Err(err)) => Err(err),
        }
    }

}
