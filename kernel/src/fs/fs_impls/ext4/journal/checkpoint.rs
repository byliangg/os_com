// SPDX-License-Identifier: MPL-2.0

//! Journal checkpointing (jbd2 `jbd2_log_do_checkpoint`).
//!
//! After [`commit_transaction`](super::commit::commit_transaction) writes a
//! transaction's metadata after-images to the log, those after-images live
//! **only** in the log — under Model A the transaction's final on-disk locations
//! still hold their pre-transaction bytes. Checkpointing is what copies each
//! committed after-image from the log to its final location and then reclaims
//! the log space it occupied.
//!
//! # Why checkpoint
//!
//! Without checkpointing the log is write-only: every commit consumes log blocks
//! and none are ever reclaimed, so the log fills and no further transaction can
//! commit. [`checkpoint`] walks the committed-but-un-checkpointed transactions,
//! applies each to its final locations, then advances the journal tail — making
//! the journal *clean* (`s_start == 0`) so a subsequent clean unmount / recovery
//! sees an empty journal and the freed log range is reusable.
//!
//! # Crash-safe ordering (the crux)
//!
//! The ordering below is what makes a crash mid-checkpoint safe. It mirrors
//! jbd2's `jbd2_journal_update_sb_log_tail`, which barriers the checkpoint
//! writes before rewriting the tail pointer:
//!
//! 1. Apply every committed transaction's after-images to their **final**
//!    locations ([`apply_log_transaction`]).
//! 2. **Barrier.** Every final-location write must be durable *before* we drop
//!    the log's record of it in step 3; otherwise a crash could leave a final
//!    location half-written with no log copy to recover from.
//! 3. Rewrite the on-disk journal superblock: clear `s_start` (journal now
//!    clean), advance `s_sequence` to the next tid recovery would expect, and
//!    write a correct `s_head`.
//! 4. **Barrier.** The clean superblock must itself be durable.
//!
//! A crash between any two steps is safe because applying a committed
//! transaction is **idempotent** (it rewrites the same bytes):
//!
//! - Crash before step 3 clears `s_start`: `s_start` still points at the tail,
//!   so recovery re-applies the same transactions — same bytes. No harm.
//! - Crash after step 3+4: the journal is cleanly empty and the final locations
//!   already carry the after-images.
//!
//! There is never a torn in-between: `s_start` flips from "points at the tail" to
//! `0` in a single durable superblock write, and everything it referenced is
//! already on its final block by then.
//!
//! # `s_head`
//!
//! [`commit.rs::update_superblock_tail`](super::commit) deliberately never writes
//! `s_head`, documenting that Task 6 (this module) owns it: when we clear
//! `s_start` on a clean journal, Linux's clean-unmount fast path
//! (`recovery.c`, `s_start == 0`) reads `s_head` to resume the log; a stale
//! `s_head` would make it resume at the wrong offset. So the clean-superblock
//! write here sets `s_head` to the live log head.
//!
//! # Shared with recovery (Task 7)
//!
//! [`apply_log_transaction`] is the log-transaction reader that Task 7 (SCAN /
//! REPLAY recovery) reuses to replay a transaction found in the log. It mirrors
//! [`commit.rs`](super::commit)'s writer through the one shared definition of
//! the descriptor-tag byte layout, [`TagLayout`](super::format::TagLayout) —
//! reader and writer cannot drift because neither owns any offset itself.
//!
//! # Scope
//!
//! - No revoke handling: our own logs carry no revoke blocks (P7b), so a
//!   block where a descriptor is expected must be a descriptor; any other block
//!   type is treated as corruption (`EUCLEAN`), never panicked on.
//! - Checksums are verify-only (P7a-3): on a csum v2/v3 journal,
//!   [`apply_log_transaction`] verifies the descriptor tail and each tag's
//!   data checksum (see its docs); the write side stamps nothing until P7a-4,
//!   so csum journals stay unadmitted at mount and these paths run under
//!   ktest fixtures.
//! - Synchronous, driven inline by the caller (here, tests); no background
//!   checkpoint thread yet.

use super::{
    super::prelude::*,
    Journal, Tid,
    commit::barrier,
    format::{
        BLOCKTYPE_COMMIT, BLOCKTYPE_DESCRIPTOR, Be32, JBD2_MAGIC, RawJournalHeader,
        RawJournalSuperblock,
    },
};

/// Applies one committed transaction from the log to its final locations, the
/// mirror of [`commit.rs`](super::commit)'s writer.
///
/// Reads the descriptor at `start_log`, validates it (magic, `DESCRIPTOR` type,
/// `h_sequence == expected_tid`), then for each tag writes the following log
/// block to the tag's destination block (`t_blocknr`), restoring the jbd2 magic
/// in an escaped block. Finally validates the trailing commit block (magic,
/// `COMMIT`, `h_sequence == expected_tid`). Returns the log block immediately
/// after the commit block — the next transaction's start.
///
/// # Writes go to FINAL locations
///
/// Every write here targets a filesystem block (`t_blocknr`), **not** a log
/// block. Re-applying the same committed transaction rewrites the same bytes, so
/// this is idempotent — the property [`checkpoint`]'s crash-safety and Task 7's
/// recovery both rely on.
///
/// # Descriptor byte layout
///
/// A descriptor block is `[12-byte header][tag 0][16-byte UUID][tag 1][tag 2]…`,
/// with every byte offset owned by the journal's
/// [`TagLayout`](super::format::TagLayout): this walk iterates
/// [`TagLayout::walk`](super::format::TagLayout::walk), the same definition the
/// commit writer's [`TagWriter`](super::format::TagWriter) and the recovery
/// scanner use, so reader and writer cannot drift.
///
/// Each tag's logged metadata block is the **next** log block after the previous
/// one, starting from `start_log` (the descriptor) — the same `next_log_block`
/// walk the writer used to place them, so reader and writer wrap the ring
/// identically.
///
/// # Checksum verification (csum v2/v3 journals)
///
/// Mirrors Linux `do_one_pass` PASS_REPLAY:
///
/// - The descriptor's tail checksum is verified before its tags are trusted;
///   a mismatch is an immediate hard error (jbd2 errors with `-EFSBADCRC` in
///   any pass but SCAN, fs/jbd2/recovery.c:570-580) — SCAN already vouched
///   for this transaction, so corruption here is real, mapped to `EUCLEAN`.
/// - Each tag's logged data block is verified against the tag's stored
///   checksum **before** any escape restoration (the checksum covers the
///   block as it sits in the log; see
///   [`DescriptorTag::verify_data_csum`](super::format::DescriptorTag::verify_data_csum)).
///   On a mismatch the block is skipped — never applied — and the walk
///   continues so every intact block still reaches its final location, then
///   the whole apply fails (`EUCLEAN`), exactly jbd2's skip-and-record shape
///   (recovery.c:655-666 skips the write and records `-EFSBADCRC`;
///   recovery.c:895-896 turns it into the pass verdict, which
///   `jbd2_journal_load` then refuses the mount over, journal.c:2072-2076 —
///   Linux does NOT mount off a log with a bad tag checksum either).
///   *Divergence*: Linux's REPLAY keeps salvaging **subsequent
///   transactions** before failing; our caller stops at this transaction.
///   The mount is refused either way — the extra salvage only feeds a
///   subsequent `e2fsck`, which replays the journal itself anyway.
/// - The commit block is not checksum-verified here, matching Linux (only
///   PASS_SCAN checks it, recovery.c:806-807 — by REPLAY it already sealed
///   the transaction).
///
/// # Errors
///
/// `EUCLEAN` if the descriptor or commit block is malformed (bad magic, wrong
/// type, wrong sequence, a tag offset that would run past the block, or a
/// checksum mismatch); `EIO` on a device failure. Never panics on a malformed
/// log.
pub(super) fn apply_log_transaction(
    journal: &Journal,
    device: &dyn BlockDevice,
    start_log: u32,
    expected_tid: Tid,
) -> Result<u32> {
    // --- Descriptor block. ---
    let mut descriptor = [0u8; BLOCK_SIZE];
    journal
        .geometry()
        .read_log_block(device, start_log, &mut descriptor)?;
    let header = RawJournalHeader::parse(&descriptor);
    if header.h_magic.get() != JBD2_MAGIC {
        return_errno_with_message!(Errno::EUCLEAN, "journal descriptor has bad magic");
    }
    // A block where a descriptor is expected must BE a descriptor. Phase 4 writes
    // no revoke blocks, so any other type (including BLOCKTYPE_REVOKE) is
    // corruption here; full revoke handling is Phase 7.
    if header.h_blocktype.get() != BLOCKTYPE_DESCRIPTOR {
        return_errno_with_message!(Errno::EUCLEAN, "expected a journal descriptor block");
    }
    if Tid::new(header.h_sequence.get()) != expected_tid {
        return_errno_with_message!(Errno::EUCLEAN, "journal descriptor has an unexpected tid");
    }

    // Descriptor-tail checksum (csum journals): hard error before any tag is
    // trusted — replay/checkpoint follows a pass that already vouched for the
    // transaction, so unlike SCAN there is no stale-block excuse here (jbd2
    // recovery.c:570-580, `-EFSBADCRC` in any pass but SCAN).
    let seed = journal.geometry().csum_seed();
    if seed.is_some_and(|s| !s.verify_block_tail(&descriptor)) {
        return_errno_with_message!(Errno::EUCLEAN, "journal descriptor checksum mismatch");
    }

    // Walk the tag array (the byte geometry lives in ONE place, the journal's
    // `TagLayout`, shared with the writer and the recovery scanner), applying
    // each tag's logged block to its final location. `log` tracks the log block
    // holding the *current* tag's metadata: it starts at the descriptor and steps
    // one block per tag (the writer wrote descriptor, then metadata 0, 1, …). A
    // malformed tag array (a tag overrunning the tag area) yields `EUCLEAN` from
    // the walker — corruption here, never a panic.
    let mut log = start_log;
    let mut bad_tag_csum = false;
    for tag in journal.geometry().tag_layout().walk(&descriptor) {
        let tag = tag?;

        // This tag's metadata is the next log block after the previous one.
        log = journal.geometry().next_log_block(log);
        let mut block = [0u8; BLOCK_SIZE];
        journal.geometry().read_log_block(device, log, &mut block)?;

        // Per-tag data checksum (csum journals), over the logged bytes BEFORE
        // the escape restoration below — the checksum covers the block as it
        // sits in the log. A mismatched block is skipped (never applied with
        // corrupt content), the walk continues so intact blocks still land,
        // and the apply fails at the end — jbd2's skip-and-record shape (see
        // the function docs; recovery.c:655-666).
        if seed.is_some_and(|s| !tag.verify_data_csum(s, expected_tid, &block)) {
            bad_tag_csum = true;
            continue;
        }

        // Restore the escaped head: the writer zeroed the first 4 bytes of a
        // block that began with JBD2_MAGIC so a recovery scan would not mistake
        // the metadata for a log header. Put the magic back before applying.
        if tag.is_escaped() {
            block[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
        }

        // Apply to the FINAL location (a filesystem block), not a log block.
        device
            .write_bytes(Bid::new(tag.blocknr()).to_offset(), block.as_slice())
            .map_err(|_| Error::with_message(Errno::EIO, "failed to apply journaled block"))?;
    }

    // --- Commit block: the next log block after the last metadata block. ---
    let commit_log = journal.geometry().next_log_block(log);
    let mut commit = [0u8; BLOCK_SIZE];
    journal
        .geometry()
        .read_log_block(device, commit_log, &mut commit)?;
    let commit_header = RawJournalHeader::parse(&commit);
    if commit_header.h_magic.get() != JBD2_MAGIC
        || commit_header.h_blocktype.get() != BLOCKTYPE_COMMIT
        || Tid::new(commit_header.h_sequence.get()) != expected_tid
    {
        return_errno_with_message!(Errno::EUCLEAN, "journal commit block is malformed");
    }

    // A tag failed its data checksum above: every intact block was applied,
    // but the transaction as a whole is corrupt — refuse (the caller refuses
    // the mount / fails the checkpoint) rather than pretend it replayed.
    if bad_tag_csum {
        return_errno_with_message!(
            Errno::EUCLEAN,
            "journal data block checksum mismatch during replay"
        );
    }

    // The next transaction starts right after this commit block.
    Ok(journal.geometry().next_log_block(commit_log))
}

/// Checkpoints ALL committed-but-un-checkpointed transactions
/// (jbd2 `jbd2_log_do_checkpoint` + `jbd2_journal_update_sb_log_tail`).
///
/// Applies each transaction in `[tail_tid ..= committed_tid]` from the log to its
/// final locations, barriers so those locations are durable, then advances the
/// tail — making the journal clean (`s_start == 0`) and reclaiming the log — by
/// updating both the in-memory tail and the on-disk journal superblock
/// (`s_start`, `s_sequence`, `s_head`), with a trailing barrier.
///
/// # Crash safety
///
/// See the module docs: the final-location writes are made durable **before**
/// `s_start` is cleared, and re-applying a committed transaction is idempotent,
/// so a crash mid-checkpoint leaves either the pre-checkpoint state (recovery
/// re-applies, same bytes) or the post-checkpoint clean state — never a torn
/// in-between.
pub(super) fn checkpoint(journal: &Journal, device: &dyn BlockDevice) -> Result<()> {
    // Snapshot the tail / committed-tid / head under the state lock; the device
    // I/O below runs WITHOUT the lock (the state lock is never held across I/O).
    // Phase 4 is single-transaction with no concurrent committer racing a
    // checkpoint, so a consistent snapshot is enough.
    let (tail_block, tail_tid, committed_tid, head) = {
        let st = journal.state_read();
        (st.tail_block, st.tail_tid, journal.committed_tid(), st.head)
    };

    // A clean journal (tail_block == 0) has nothing un-checkpointed.
    if tail_block == 0 {
        return Ok(());
    }

    // Apply each committed transaction in tid order, oldest first. Each is
    // known-committed, so `apply_log_transaction` must find a valid commit block;
    // a failure means log corruption (EUCLEAN/EIO), which we propagate.
    //
    // Loop termination: `committed_tid.geq(tid)` is jbd2's wrapping-aware
    // "is committed_tid at or after tid?". `tid` starts at `tail_tid` and steps
    // by one per committed transaction; because at most half the id space
    // separates `tail_tid` from `committed_tid` (they bound the live log), the
    // predicate flips to false exactly once `tid` passes `committed_tid`,
    // terminating after applying tid `committed_tid`.
    let mut log = tail_block;
    let mut tid = tail_tid;
    while committed_tid.geq(tid) {
        log = apply_log_transaction(journal, device, log, tid)?;
        tid = tid.next();
    }

    // --- Barrier: every final-location write durable BEFORE we drop the log's
    // record of them by clearing s_start. ---
    barrier(device)?;

    // --- Rewrite the on-disk journal superblock to the clean state. ---
    let sb_pblock = journal
        .geometry()
        .log_block_to_physical(0)
        .ok_or_else(|| Error::with_message(Errno::EUCLEAN, "journal superblock block unmapped"))?;
    let sb_offset = Bid::new(sb_pblock).to_offset();

    let mut raw: RawJournalSuperblock = device
        .read_val(sb_offset)
        .map_err(|_| Error::with_message(Errno::EIO, "failed to read journal superblock"))?;
    // Journal now clean: nothing awaits recovery.
    raw.s_start = Be32::new(0);
    // The tid recovery would expect for the first transaction after this point.
    raw.s_sequence = Be32::new(committed_tid.next().get());
    // A clean-unmount superblock MUST carry a correct s_head: Linux's clean
    // fast path (recovery.c, s_start == 0) resumes the log from s_head, so a
    // stale value would corrupt a subsequent mount. `commit.rs` deliberately
    // leaves this to us. (Phase 4 Task 3 adversarial-review requirement.)
    // On a csum journal this rewrite would also have to restamp `s_checksum`
    // (jbd2_write_superblock, journal.c:1812-1813) — P7a-4's write side;
    // until then admission keeps csum journals off real mounts.
    raw.s_head = Be32::new(head);
    device
        .write_val(sb_offset, &raw)
        .map_err(|_| Error::with_message(Errno::EIO, "failed to write journal superblock"))?;

    // --- Barrier: the clean superblock must itself be durable. ---
    barrier(device)?;

    // Publish the clean state in memory: the tail is cleared and its tid advances
    // to the next expected. `head` is left as-is — the ring continues from where
    // commit left it; the next commit sees `tail_block == 0` (clean) and
    // re-establishes `s_start` at its own start block.
    //
    // The same critical section evicts the retained after-images this pass made
    // durable (their final-location writes are behind the barrier above, so the
    // device is authoritative for them again). The tid comparison keeps an image
    // from a commit NEWER than this pass's snapshot: evicting it would hand a
    // later capture the device's still-lagging bytes.
    {
        let mut st = journal.state_write();
        st.tail_block = 0;
        st.tail_tid = committed_tid.next();
        st.uncheckpointed
            .retain(|_, image| !image.is_checkpointed_by(committed_tid));
    }

    Ok(())
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::{
        super::{
            super::test_utils::{Ext4FixtureBuilder, make_multi_block_file_inode},
            JOURNAL_INO, Transaction,
            commit::commit_transaction,
            format::BLOCKTYPE_SUPERBLOCK_V2,
            load_geometry,
        },
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

    /// A journaled fixture: the in-memory `Journal` plus the fixture that owns the
    /// disk (so tests can read/write final and log locations off the device).
    struct JournaledFixture {
        journal: Arc<Journal>,
        fixture: super::super::super::test_utils::Ext4Fixture,
    }

    /// Builds a journaled fixture with a `maxlen`-block log at physical
    /// `[200, 200+maxlen)`, first log-data block `first`, and `s_sequence`.
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

    /// Reads a final (filesystem) block off the device by its block number.
    fn read_final_block(f: &JournaledFixture, bid: u64) -> [u8; BLOCK_SIZE] {
        let mut buf = [0u8; BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .read_bytes(bid as usize * BLOCK_SIZE, &mut buf)
            .unwrap();
        buf
    }

    /// Reads the on-disk journal superblock.
    fn read_journal_super(f: &JournaledFixture) -> RawJournalSuperblock {
        f.fixture
            .disk
            .segment()
            .read_val(JOURNAL_START_BLOCK as usize * BLOCK_SIZE)
            .unwrap()
    }

    /// Builds a running transaction with `tid` capturing the given
    /// `(dest_block, content)` pairs.
    fn make_txn(tid: Tid, blocks: &[(Ext4Bid, [u8; BLOCK_SIZE])]) -> Transaction {
        let mut txn = Transaction::new(tid);
        for (bid, content) in blocks {
            txn.capture_create(*bid);
            txn.apply_patch(*bid, |b| b.copy_from_slice(content))
                .unwrap();
        }
        txn
    }

    /// Test 1: checkpoint applies a committed after-image to its final location
    /// and makes the journal clean.
    #[ktest]
    fn checkpoint_applies_after_image_and_cleans_journal() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);
        let device = f.fixture.ext4.block_device();

        let dest = 500u64;
        let mut after = [0u8; BLOCK_SIZE];
        after[..8].copy_from_slice(b"AFTERIMG");
        after[BLOCK_SIZE - 4..].copy_from_slice(b"TAIL");

        let txn = make_txn(Tid::new(1), &[(dest, after)]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), txn).unwrap();
        let committed = f.journal.committed_tid();
        assert_eq!(committed, Tid::new(1));

        // Before checkpoint: the final location still holds the fixture's zeroed
        // disk — the after-image lives only in the log.
        assert_eq!(read_final_block(&f, dest), [0u8; BLOCK_SIZE]);
        // And the journal is dirty (s_start points at the tail).
        assert_ne!(read_journal_super(&f).s_start.get(), 0);

        let head_before = f.journal.state_read().head;
        checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();

        // After checkpoint: the final location carries the after-image.
        assert_eq!(read_final_block(&f, dest), after);

        // The on-disk superblock is now clean, with the right next tid and head.
        let sb = read_journal_super(&f);
        assert_eq!(sb.s_start.get(), 0);
        assert_eq!(sb.s_sequence.get(), committed.next().get());
        assert_eq!(sb.s_head.get(), head_before);

        // In-memory tail is cleared.
        assert_eq!(f.journal.state_read().tail_block, 0);
    }

    /// Test 2: an escaped block round-trips through checkpoint — the final
    /// location gets the ORIGINAL bytes (magic restored), not the zeroed log copy.
    #[ktest]
    fn checkpoint_restores_escaped_block() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);
        let device = f.fixture.ext4.block_device();

        // A metadata block whose head IS the jbd2 magic, forcing an ESCAPE.
        let dest = 600u64;
        let mut original = [0u8; BLOCK_SIZE];
        original[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
        original[4..8].copy_from_slice(b"REST");

        let txn = make_txn(Tid::new(1), &[(dest, original)]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), txn).unwrap();

        checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();

        // The final location has the ORIGINAL bytes: the magic head restored, not
        // the zeroed log copy.
        assert_eq!(read_final_block(&f, dest), original);
    }

    /// Test 3: two transactions writing the same final block — checkpoint applies
    /// in tid order, so the newest (last-applied) wins.
    #[ktest]
    fn checkpoint_newest_wins_on_shared_block() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(24, 1, 1);
        let device = f.fixture.ext4.block_device();

        let dest = 700u64;
        let mut content_a = [0u8; BLOCK_SIZE];
        content_a[..8].copy_from_slice(b"CONTENTA");
        let mut content_b = [0u8; BLOCK_SIZE];
        content_b[..8].copy_from_slice(b"CONTENTB");

        // T1 (tid 1) writes A, T2 (tid 2) writes B, to the SAME final block.
        let t1 = make_txn(Tid::new(1), &[(dest, content_a)]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t1).unwrap();
        let t2 = make_txn(Tid::new(2), &[(dest, content_b)]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t2).unwrap();
        assert_eq!(f.journal.committed_tid(), Tid::new(2));

        checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();

        // Applied in tid order (T1 then T2), so B (the newest) is the final state.
        assert_eq!(read_final_block(&f, dest), content_b);
        // And the journal reclaimed cleanly.
        assert_eq!(read_journal_super(&f).s_start.get(), 0);
    }

    /// Test 4: after a clean checkpoint, a fresh commit re-establishes `s_start`
    /// (was-clean path), advances `committed_tid`, and a second checkpoint applies
    /// it — the log is reusable.
    #[ktest]
    fn checkpoint_then_commit_reclaims_cleanly() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);
        let device = f.fixture.ext4.block_device();

        // First transaction + checkpoint => journal clean.
        let dest1 = 500u64;
        let mut c1 = [0u8; BLOCK_SIZE];
        c1[..4].copy_from_slice(b"ONE0");
        let t1 = make_txn(Tid::new(1), &[(dest1, c1)]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t1).unwrap();
        checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();
        assert_eq!(read_journal_super(&f).s_start.get(), 0);
        assert_eq!(f.journal.state_read().tail_block, 0);

        // A fresh commit: the was-clean path must re-establish s_start at its
        // start block and advance committed_tid.
        let dest2 = 800u64;
        let mut c2 = [0u8; BLOCK_SIZE];
        c2[..4].copy_from_slice(b"TWO0");
        let t2 = make_txn(Tid::new(2), &[(dest2, c2)]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t2).unwrap();
        assert_eq!(f.journal.committed_tid(), Tid::new(2));

        // The journal is dirty again (s_start re-established) and the tail records
        // the new transaction.
        assert_ne!(read_journal_super(&f).s_start.get(), 0);
        assert_ne!(f.journal.state_read().tail_block, 0);

        // Its final location is still pre-transaction until we checkpoint again.
        assert_eq!(read_final_block(&f, dest2), [0u8; BLOCK_SIZE]);

        // A second checkpoint applies the new transaction and cleans the journal.
        checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();
        assert_eq!(read_final_block(&f, dest2), c2);
        assert_eq!(read_journal_super(&f).s_start.get(), 0);
        // s_sequence now expects tid 3.
        assert_eq!(read_journal_super(&f).s_sequence.get(), 3);
    }

    /// Test 5: `apply_log_transaction` is idempotent — applying the same committed
    /// transaction twice writes the same bytes to the final location.
    #[ktest]
    fn apply_log_transaction_is_idempotent() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);
        let device = f.fixture.ext4.block_device();

        let dest = 900u64;
        let mut after = [0u8; BLOCK_SIZE];
        after[..8].copy_from_slice(b"IDEMPOT!");

        let txn = make_txn(Tid::new(1), &[(dest, after)]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), txn).unwrap();

        // The transaction starts at log block `first` (== 1).
        let start_log = f.journal.geometry().first();

        // First apply.
        let next1 =
            apply_log_transaction(f.journal.as_ref(), device.as_ref(), start_log, Tid::new(1))
                .unwrap();
        assert_eq!(read_final_block(&f, dest), after);

        // Clobber the final location, then re-apply the SAME committed transaction:
        // it must restore the identical bytes and return the same "next" log block.
        f.fixture
            .disk
            .segment()
            .write_val(dest as usize * BLOCK_SIZE, &[0u8; BLOCK_SIZE])
            .unwrap();
        assert_eq!(read_final_block(&f, dest), [0u8; BLOCK_SIZE]);
        let next2 =
            apply_log_transaction(f.journal.as_ref(), device.as_ref(), start_log, Tid::new(1))
                .unwrap();
        assert_eq!(read_final_block(&f, dest), after);
        assert_eq!(next1, next2);
    }
}
