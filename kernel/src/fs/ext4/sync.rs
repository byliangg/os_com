// SPDX-License-Identifier: MPL-2.0
//! Phase 8 move-only split: the fsync / shutdown / filesystem-sync path relocated verbatim from
//! `fs.rs` — `shutdown`, the needs-recovery markers, the unchecked filesystem-sync helper, and
//! `fsync_regular_file`. No behavior change.

use core::sync::atomic::Ordering;

use super::device_adapter::KernelBlockDeviceAdapter;
use super::fs::Ext4Fs;
use super::run::REGULAR_FILE_FSYNC_CHECKPOINT_DEPTH;
use super::types::DeviceMetadataWriter as Ext4MetadataWriter;
use crate::prelude::*;

impl Ext4Fs {

    /// Step 4b: implements the `EXT4_IOC_SHUTDOWN` ioctl.
    ///
    /// Three flag values follow Linux ext4 semantics:
    ///   - `EXT4_GOING_FLAGS_DEFAULT (0x0)`: best-effort sync of dirty
    ///     metadata, then forced shutdown.  Implemented as `Ext4Fs::sync()`
    ///     followed by setting the shutdown bit (slightly more conservative
    ///     than Linux, which does not actively force-commit on DEFAULT).
    ///   - `EXT4_GOING_FLAGS_LOGFLUSH (0x1)`: force-commit all pending
    ///     transactions, flush the journal and the device, then shutdown.
    ///     This is the "clean-ish" variant.
    ///   - `EXT4_GOING_FLAGS_NOLOGFLUSH (0x2)`: discard in-flight commits,
    ///     no flush, no journal sync.  This is the strongest crash
    ///     simulation — equivalent to a hard power-cut at the moment of
    ///     the ioctl.  After this, the on-disk state is whatever was
    ///     already persisted; remount must replay the journal.
    ///
    /// In all three cases, after this call returns, all subsequent
    /// journaled operations and fsync return EIO until remount.
    pub(super) fn shutdown(&self, flag: u32) -> Result<()> {
        const EXT4_GOING_FLAGS_DEFAULT: u32 = 0x0;
        const EXT4_GOING_FLAGS_LOGFLUSH: u32 = 0x1;
        const EXT4_GOING_FLAGS_NOLOGFLUSH: u32 = 0x2;

        // Idempotent: a second shutdown call is a no-op.
        if self.is_shutdown() {
            return Ok(());
        }

        match flag {
            EXT4_GOING_FLAGS_NOLOGFLUSH => {
                // Hard crash simulation: do NOT flush the journal.  Just
                // mark the FS as shutdown.  Any pending JBD2 transactions
                // on disk are left in their current state; remount will
                // see needs_recovery (s_start != 0) and replay.
                warn!("ext4: shutdown NOLOGFLUSH (hard crash simulation)");
            }
            EXT4_GOING_FLAGS_LOGFLUSH | EXT4_GOING_FLAGS_DEFAULT => {
                // Clean-ish shutdown: force-commit and flush journal +
                // device first.  The existing FileSystem::sync() path
                // already does flush_pending_jbd2_transactions +
                // block_device.sync().
                warn!(
                    "ext4: shutdown {} — force-commit + flush before mark",
                    if flag == EXT4_GOING_FLAGS_LOGFLUSH {
                        "LOGFLUSH"
                    } else {
                        "DEFAULT"
                    }
                );
                if let Err(err) = self.do_filesystem_sync_unchecked() {
                    warn!("ext4: shutdown sync failed (continuing anyway): {:?}", err);
                }
            }
            _ => {
                return_errno_with_message!(Errno::EINVAL, "ext4: unsupported shutdown flag");
            }
        }

        // Step 4b: ensure dumpe2fs / e2fsprogs see "needs_recovery" after a
        // forced shutdown.  This is what generic/052/054/055 (and Linux ext4
        // mount-time pessimistic flag) rely on.  We persist the flag here
        // unconditionally for ALL shutdown flags — even LOGFLUSH leaves
        // EXT4_FEATURE_INCOMPAT_RECOVER set in Linux because LOGFLUSH only
        // flushes the journal area, it does not perform a clean unmount.
        self.mark_needs_recovery_for_shutdown();

        self.shutdown_state.store(1, Ordering::Release);
        Ok(())
    }

    /// Step 4b: force-write `EXT4_FEATURE_INCOMPAT_RECOVER` to the on-disk
    /// superblock and flush.  Called from `shutdown()` so dumpe2fs reports
    /// "dirty log" after `EXT4_IOC_SHUTDOWN`, regardless of which flag was
    /// used.  Bypasses `JournalIoBridge` deferral by routing the write
    /// through a raw adapter wrapper so the value reaches the disk even
    /// on the NOLOGFLUSH (no journal commit) path.
    fn mark_needs_recovery_for_shutdown(&self) {
        // Lightweight metadata writer that bypasses the journal overlay
        // and writes directly via the underlying block adapter.
        struct RawAdapterWriter(Arc<KernelBlockDeviceAdapter>);
        impl Ext4MetadataWriter for RawAdapterWriter {
            fn write_metadata(&self, offset: usize, data: &[u8]) {
                self.0.write_offset(offset, data);
            }
            fn write_metadata_for_jbd2_handle(
                &self,
                _handle_id: Option<u64>,
                offset: usize,
                data: &[u8],
            ) {
                self.0.write_offset(offset, data);
            }
        }
        let raw_writer = RawAdapterWriter(self.adapter.clone());

        // Phase 6 Task 4: set the core-owned RECOVER flag + persist a `core_sb`-based SB image
        // (mount-time free counts, RECOVER bit set, csum recomputed) straight to the home superblock
        // via the raw adapter writer. PARITY: ext4_rs `inner.super_block.set_needs_recovery(true) +
        // sync_to_disk_with_csum(&RawAdapterWriter)`.
        self.persist_recover_flag(true, &raw_writer);
        // Flush so dumpe2fs / next mount sees the updated superblock.
        let _ = self.block_device.sync();
    }

    /// Internal helper: same as `FileSystem::sync()` but does not check
    /// the shutdown bit (used during shutdown itself).
    fn do_filesystem_sync_unchecked(&self) -> Result<()> {
        self.flush_pending_jbd2_transactions();
        self.block_device.sync()?;
        self.flush_pending_jbd2_transactions();
        Ok(())
    }

    /// Step 4b: lazily set the on-disk superblock's `needs_recovery` flag
    /// (`EXT4_FEATURE_INCOMPAT_RECOVER`) on first journal commit since the
    /// flag was last clean.  After this, dumpe2fs-style probes will report
    /// the FS as "dirty log" until the next clean shutdown / replay clears
    /// the flag.  No-op if the flag is already set (cheap fast path).
    pub(super) fn mark_needs_recovery_if_needed(&self) {
        // Phase 6 Task 4: fast path on the core-owned in-memory RECOVER flag. PARITY: ext4_rs
        // checked `!inner.super_block.needs_recovery()` (the in-memory bit) before persisting.
        if self.recover_flag.load(Ordering::Acquire) {
            return;
        }
        // Persist the SB with RECOVER set through the journal bridge metadata writer. This is called
        // post-commit with no active handle, so the bridge write goes straight to the home superblock
        // — byte-frozen vs ext4_rs `inner.super_block.sync_to_disk_with_csum(&inner.metadata_writer)`.
        let writer: Arc<dyn Ext4MetadataWriter> = self.journal_io.clone();
        self.persist_recover_flag(true, writer.as_ref());
    }

    pub(super) fn fsync_regular_file(&self, ino: u32) -> Result<()> {
        // Step 4b: post-shutdown fsync must return EIO so that callers
        // (e.g. xfstests after godown) know that the filesystem is dead.
        self.check_not_shutdown()?;

        let inode_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
        let _inode_guard = inode_lock.write();

        // Step 4a-2: look up the highest TID that contains a metadata change
        // for this inode (recorded by `finish_jbd2_handle` after Write/Truncate).
        // If `None`, the inode has no recorded metadata changes and fsync is
        // a no-op for the journal — the VFS-layer device flush in
        // `Ext4Inode::sync_all` (Step 4a-1) still runs to handle any
        // already-issued data writes from prior writers.
        let target_tid = self.lookup_inode_tid(ino);
        let committed = self.last_committed_tid();

        // Step 1 observation: log fsync entry state for diagnostic purposes.
        warn!(
            "ext4: fsync ino={} target_tid={:?} committed_tid={}",
            ino, target_tid, committed
        );

        if let Some(target_tid) = target_tid {
            // Step 4a-2: force-commit the target TID. Internally:
            //   - Fast path returns if already committed.
            //   - If target is the running TX, rotate it to prev_running.
            //   - Drive `try_commit_ready_jbd2_transaction()` and block on
            //     `commit_notifier` until last_committed_tid >= target_tid.
            // This replaces the previous best-effort
            // `commit_pending_jbd2_transactions()` calls, which silently
            // no-op'd when commit_ready=false (e.g. when other workers held
            // active handles) — a POSIX violation.
            self.force_commit_for_tid(target_tid);
        }

        // Lazy checkpoint: only when journal pressure is high. Checkpoint
        // writes home blocks back from the journal area, freeing journal
        // space; not strictly required for fsync correctness (replay handles
        // it on crash). Phase 2 batch_checkpoint policy preserved.
        if self.checkpoint_depth() >= REGULAR_FILE_FSYNC_CHECKPOINT_DEPTH {
            self.try_batch_checkpoint_all_jbd2_transactions();
        }
        Ok(())
    }
}
