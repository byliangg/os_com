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
//! `s_head`, documenting that the checkpoint pass (this module) owns it: when we clear
//! `s_start` on a clean journal, Linux's clean-unmount fast path
//! (`recovery.c`, `s_start == 0`) reads `s_head` to resume the log; a stale
//! `s_head` would make it resume at the wrong offset. So the clean-superblock
//! write here sets `s_head` to the live log head.
//!
//! # Shared with recovery
//!
//! [`apply_log_transaction`] is the log-transaction reader that mount-time
//! recovery (SCAN / REPLAY) reuses to replay a transaction found in the log. It mirrors
//! [`commit.rs`](super::commit)'s writer through the one shared definition of
//! the descriptor-tag byte layout, [`TagLayout`](super::format::TagLayout) —
//! reader and writer cannot drift because neither owns any offset itself.
//!
//! # Scope
//!
//! - Revoke **suppression** (P7b-2): because this checkpoint re-reads the LOG,
//!   a block freed (and revoked) by a later transaction must be skipped when
//!   an earlier transaction's images are applied, or its post-free reuse is
//!   clobbered at runtime, no crash required (audit S1). Each apply consults
//!   the committed-revoke memory ([`RevokeTable::suppresses`]); [`checkpoint`]
//!   retires the records at the same tid boundary that evicts the retained
//!   images. An **unpublished** revoke (a forget whose transaction has not
//!   committed) suppresses nothing — the pass instead **defers** in front of
//!   the first transaction touching such a block (see [`checkpoint`]'s
//!   defer-prefix rule). A revoking transaction's chain additionally carries
//!   on-disk revoke *blocks* (P7b-3, at the chain head): the walkers here
//!   step over a same-tid revoke block positionally — its records serve
//!   mount-time recovery's PASS_REVOKE; the live checkpoint's suppression
//!   authority is the in-memory table, published at commit step 6 with
//!   identical `(block, tid)` content. Any other block type where a chain
//!   block is expected is corruption (`EUCLEAN`), never panicked on.
//! - On a csum v2/v3 journal (admitted since P7a-4), [`apply_log_transaction`]
//!   verifies the descriptor tail and each tag's data checksum (see its docs)
//!   against what the commit pipeline stamped, and the clean-superblock
//!   rewrite restamps `s_checksum` through the
//!   [`JournalGeometry::write_superblock`](super::JournalGeometry::write_superblock)
//!   funnel.
//! - Synchronous and single-driver: run only by the commit thread — LAZILY,
//!   under space pressure ([`Journal::run_lazy_checkpoint`](super::Journal)),
//!   not after every commit (P7c-3) — plus the commit-time inline tail drain
//!   ([`Journal::commit_or_drain_tail`](super::Journal)) and the unmount flush
//!   ([`Journal::flush_on_unmount`](super::Journal)), both on that same thread
//!   (the flush after it stops). No second checkpoint driver, so a pass never
//!   overlaps a commit; the whole pass holds
//!   [`Journal::j_checkpoint`](super::Journal) (jbd2 `j_checkpoint_mutex`) so
//!   the journal superblock has one writer even if one is ever added.

use super::{
    super::prelude::*,
    Journal, Tid,
    commit::barrier,
    format::{
        BLOCKTYPE_COMMIT, BLOCKTYPE_DESCRIPTOR, BLOCKTYPE_REVOKE, Be32, JBD2_MAGIC,
        RawJournalHeader,
    },
    revoke::RevokeTable,
};

/// Applies one committed transaction from the log to its final locations, the
/// mirror of [`commit.rs`](super::commit)'s writer.
///
/// A transaction is a **descriptor chain** (jbd2 `do_one_pass`, matching the
/// scanner): zero or more revoke blocks (stepped over positionally — see the
/// revoke-suppression section) and descriptor blocks — each descriptor
/// followed by the metadata blocks its tags count — terminated by a commit
/// block, every chain block bearing `expected_tid`. For each descriptor this
/// validates it (magic, `DESCRIPTOR` type, `h_sequence == expected_tid`),
/// then for each of its tags writes the following log block to the tag's
/// destination block (`t_blocknr`), restoring the jbd2 magic in an escaped
/// block — all descriptors' blocks, in chain order. The chain ends at the
/// validated commit block (magic, `COMMIT`, `h_sequence == expected_tid`);
/// the zero-descriptor chain (a Linux `data=ordered` transaction that
/// carried only file data) applies nothing. Returns the log block
/// immediately after the commit block — the next transaction's start.
///
/// Apply granularity is the whole chain: the caller only reaches this for a
/// transaction SCAN found sealed, so there is no partial-descriptor apply —
/// a chain that turns out malformed mid-walk errors (`EUCLEAN`) and the
/// mount/checkpoint refuses.
///
/// # Writes go to FINAL locations
///
/// Every write here targets a filesystem block (`t_blocknr`), **not** a log
/// block. Re-applying the same committed transaction rewrites the same bytes, so
/// this is idempotent — the property [`checkpoint`]'s crash-safety and
/// mount-time recovery both rely on.
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
/// # Revoke suppression
///
/// Before a tag's logged block is read (and so before its checksum is
/// verified, matching jbd2's order — `jbd2_journal_test_revoke` gates the
/// whole replay branch, recovery.c:632-636), `revoked` is consulted: a
/// record `(blocknr, tid_r)` with `expected_tid ≤ tid_r` means the block was
/// freed by a transaction at or after this one, so this (older) after-image
/// must not be applied over the block's post-free reuse — the log block is
/// consumed by the walk but neither read nor written. An image in a
/// transaction *newer* than `tid_r` still applies. The checkpoint caller
/// passes the journal's committed-revoke memory; the mount-time replay
/// caller passes the recovery-local table PASS_REVOKE built from the log's
/// own revoke blocks — one predicate serves both consumers.
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
    revoked: &RevokeTable,
) -> Result<u32> {
    let seed = journal.geometry().csum_seed();

    // Set when any tag's logged data block fails its checksum: the block is
    // skipped, the walk continues, and the apply as a whole refuses at the
    // commit block (jbd2's skip-and-record shape; see the function docs).
    let mut bad_tag_csum = false;
    // Chain blocks consumed, for the anti-cycle bound (a valid chain can
    // never occupy more blocks than the log holds; more means the walk has
    // cycled the ring through stale same-tid blocks — refuse rather than
    // loop forever, the scanner's same defensive bound).
    let mut consumed: u32 = 0;

    let mut log = start_log;
    loop {
        // --- The next chain block: a descriptor, or the sealing commit. ---
        let mut chain_block = [0u8; BLOCK_SIZE];
        journal
            .geometry()
            .read_log_block(device, log, &mut chain_block)?;
        let header = RawJournalHeader::parse(&chain_block);
        if header.h_magic.get() != JBD2_MAGIC {
            return_errno_with_message!(Errno::EUCLEAN, "journal chain block has bad magic");
        }
        if Tid::new(header.h_sequence.get()) != expected_tid {
            return_errno_with_message!(Errno::EUCLEAN, "journal chain block has an unexpected tid");
        }

        if header.h_blocktype.get() == BLOCKTYPE_COMMIT {
            // A tag failed its data checksum above: every intact block was
            // applied, but the transaction as a whole is corrupt — refuse
            // (the caller refuses the mount / fails the checkpoint) rather
            // than pretend it replayed.
            if bad_tag_csum {
                return_errno_with_message!(
                    Errno::EUCLEAN,
                    "journal data block checksum mismatch during replay"
                );
            }
            // The next transaction starts right after this commit block.
            return Ok(journal.geometry().next_log_block(log));
        }
        // A same-tid revoke block (P7b-3 writes them at the chain head) is a
        // legitimate one-block chain member: step over it. Its records are
        // not read here — this apply's suppression authority is the
        // `revoked` table the caller passed (the live committed-revoke
        // memory for checkpoint; the PASS_REVOKE-built table, parsed from
        // exactly these blocks, for mount-time replay).
        if header.h_blocktype.get() == BLOCKTYPE_REVOKE {
            log = journal.geometry().next_log_block(log);
            consumed += 1;
            if consumed > journal.geometry().maxlen() {
                return_errno_with_message!(
                    Errno::EUCLEAN,
                    "journal transaction chain exceeds the log size"
                );
            }
            continue;
        }
        // A chain block must be a descriptor, a revoke block, or the commit;
        // any other type is corruption here.
        if header.h_blocktype.get() != BLOCKTYPE_DESCRIPTOR {
            return_errno_with_message!(Errno::EUCLEAN, "expected a journal descriptor block");
        }

        // Descriptor-tail checksum (csum journals): hard error before any of
        // this descriptor's tags is trusted — replay/checkpoint follows a
        // pass that already vouched for the transaction, so unlike SCAN there
        // is no stale-block excuse here (jbd2 recovery.c:570-580,
        // `-EFSBADCRC` in any pass but SCAN).
        if seed.is_some_and(|s| !s.verify_block_tail(&chain_block)) {
            return_errno_with_message!(Errno::EUCLEAN, "journal descriptor checksum mismatch");
        }

        // Walk this descriptor's tag array (the byte geometry lives in ONE
        // place, the journal's `TagLayout`, shared with the writer and the
        // recovery scanner), applying each tag's logged block to its final
        // location. `log` tracks the log block holding the *current* tag's
        // metadata: it steps one block per tag from the descriptor (the
        // writer wrote descriptor, then metadata 0, 1, …). A malformed tag
        // array (a tag overrunning the tag area) yields `EUCLEAN` from the
        // walker — corruption here, never a panic.
        for tag in journal.geometry().tag_layout().walk(&chain_block) {
            let tag = tag?;

            // This tag's metadata is the next log block after the previous one.
            log = journal.geometry().next_log_block(log);
            consumed += 1;

            // Revoke suppression (RED-LINE ③; see the function docs): a
            // block freed by a transaction at or after this one keeps its
            // reused content — the log block is consumed but never read,
            // verified, or applied (jbd2 skips a revoked block before its
            // checksum check too).
            if revoked.suppresses(tag.blocknr(), expected_tid) {
                continue;
            }

            let mut block = [0u8; BLOCK_SIZE];
            journal.geometry().read_log_block(device, log, &mut block)?;

            // Per-tag data checksum (csum journals), over the logged bytes
            // BEFORE the escape restoration below — the checksum covers the
            // block as it sits in the log. A mismatched block is skipped
            // (never applied with corrupt content), the walk continues so
            // intact blocks still land, and the apply fails at the commit —
            // jbd2's skip-and-record shape (see the function docs;
            // recovery.c:655-666).
            if seed.is_some_and(|s| !tag.verify_data_csum(s, expected_tid, &block)) {
                bad_tag_csum = true;
                continue;
            }

            // Restore the escaped head: the writer zeroed the first 4 bytes
            // of a block that began with JBD2_MAGIC so a recovery scan would
            // not mistake the metadata for a log header. Put the magic back
            // before applying.
            if tag.is_escaped() {
                block[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
            }

            // Apply to the FINAL location (a filesystem block), not a log block.
            device
                .write_bytes(Bid::new(tag.blocknr()).to_offset(), block.as_slice())
                .map_err(|_| Error::with_message(Errno::EIO, "failed to apply journaled block"))?;
        }

        // The next chain block — another descriptor, or the commit — follows
        // this descriptor's last metadata block.
        log = journal.geometry().next_log_block(log);
        consumed += 1;

        if consumed > journal.geometry().maxlen() {
            return_errno_with_message!(
                Errno::EUCLEAN,
                "journal transaction chain exceeds the log size"
            );
        }
    }
}

/// Pre-scans one committed transaction's descriptor chain — the same walk
/// [`apply_log_transaction`] applies, minus every data-block read — and
/// returns whether any of its destination blocks is in `unpublished`: the
/// gate of [`checkpoint`]'s defer-prefix rule.
///
/// Reads only the chain blocks (the descriptors and the sealing commit),
/// stepping over each descriptor's logged data blocks positionally, and
/// returns at the first covered block — so a pass with unpublished revokes
/// pays at most `descriptors + 1` extra log reads per candidate transaction
/// (and none at all when the set is empty; the caller skips the scan).
///
/// Validation is the structural minimum for a safe walk (magic / tid /
/// block type / tag bounds / the anti-cycle bound). Checksum verification
/// stays with the apply: a transaction this scan defers is deliberately not
/// verified, because nothing of it is trusted or written this pass.
fn chain_covers_unpublished(
    journal: &Journal,
    device: &dyn BlockDevice,
    start_log: u32,
    expected_tid: Tid,
    unpublished: &BTreeSet<Ext4Bid>,
) -> Result<bool> {
    let mut consumed: u32 = 0;
    let mut log = start_log;
    loop {
        let mut chain_block = [0u8; BLOCK_SIZE];
        journal
            .geometry()
            .read_log_block(device, log, &mut chain_block)?;
        let header = RawJournalHeader::parse(&chain_block);
        if header.h_magic.get() != JBD2_MAGIC {
            return_errno_with_message!(Errno::EUCLEAN, "journal chain block has bad magic");
        }
        if Tid::new(header.h_sequence.get()) != expected_tid {
            return_errno_with_message!(Errno::EUCLEAN, "journal chain block has an unexpected tid");
        }
        if header.h_blocktype.get() == BLOCKTYPE_COMMIT {
            return Ok(false);
        }
        // A same-tid revoke block: a one-block chain member with no
        // destination tags, so nothing of it can cover `unpublished`.
        if header.h_blocktype.get() == BLOCKTYPE_REVOKE {
            log = journal.geometry().next_log_block(log);
            consumed += 1;
            if consumed > journal.geometry().maxlen() {
                return_errno_with_message!(
                    Errno::EUCLEAN,
                    "journal transaction chain exceeds the log size"
                );
            }
            continue;
        }
        if header.h_blocktype.get() != BLOCKTYPE_DESCRIPTOR {
            return_errno_with_message!(Errno::EUCLEAN, "expected a journal descriptor block");
        }

        for tag in journal.geometry().tag_layout().walk(&chain_block) {
            let tag = tag?;
            if unpublished.contains(&tag.blocknr()) {
                return Ok(true);
            }
            log = journal.geometry().next_log_block(log);
            consumed += 1;
        }

        log = journal.geometry().next_log_block(log);
        consumed += 1;
        if consumed > journal.geometry().maxlen() {
            return_errno_with_message!(
                Errno::EUCLEAN,
                "journal transaction chain exceeds the log size"
            );
        }
    }
}

/// Advances the log tail by checkpointing committed-but-un-checkpointed
/// transactions (jbd2 `jbd2_log_do_checkpoint` + `jbd2_journal_update_sb_log_tail`),
/// returning whether it advanced.
///
/// Applies transactions from `[tail_tid ..= committed_tid]` from the log to
/// their final locations in tid order, barriers so those locations are durable,
/// then advances the tail — updating both the in-memory tail and the on-disk
/// journal superblock (`s_start`, `s_sequence`, `s_head`), with a trailing
/// barrier. How far it advances is set by `target_free`:
///
/// - `None` — a **full pass**: reclaim the whole reclaimable log, clearing the
///   journal (`s_start == 0`). The demanded-reclaim, inline-drain, unmount, and
///   recovery entry (through [`checkpoint`]).
/// - `Some(n)` — an **incremental pass**: stop once the free segment holds at
///   least `n` blocks (jbd2 checkpoints only as far as space demands), leaving
///   the newer tail dirty. The commit thread's steady low-water reclaim.
///
/// Either way it may **stop early** in front of an unpublished-revoke
/// transaction (the defer-prefix rule below); a partial advance points
/// `s_start` at the first unapplied transaction, exactly what recovery would
/// replay. This pass is LAZY — the commit thread runs it only under space
/// pressure ([`Journal::run_lazy_checkpoint`](super::Journal)), not per commit —
/// so the un-checkpointed tail (and thus the retained-image map, the revoke
/// table, and the deferred pre-scan) is now many transactions deep, not one;
/// the module docs' *Lazy-checkpoint invariants* section audits that depth.
/// The whole pass holds [`Journal::j_checkpoint`](super::Journal) so the journal
/// superblock has one writer.
///
/// # The defer-prefix rule (unpublished revokes)
///
/// The pass stops **in front of** the first transaction any of whose blocks
/// carries an *unpublished* revoke — a forget in the running transaction, or
/// in the committing slot's mid-flight transaction
/// ([`JournalState::committing`](super::JournalState), occupied during the
/// inline tail drain; empty for the post-commit and unmount passes, which
/// run after retirement). Such a block is already freed (and possibly reused) in
/// memory, so applying the older image is the S1 runtime clobber; yet the
/// revoking transaction may still vanish in a crash, so the older image may
/// not be suppressed-and-retired either (see the snapshot comment below).
/// Deferring is the only sound move: nothing from that transaction onward is
/// applied or retired, the tail advances exactly to it (prefix retirement —
/// the tail can never move past an unapplied transaction), and the pass
/// returns `Ok` with partial progress. The revoke publishes with its commit,
/// so the next pass proceeds under ordinary suppression-with-retention; on
/// the drain path the smaller reclaim surfaces as `NeedsLogSpace` → a loud
/// `ENOSPC` abort rather than corruption.
///
/// # Crash safety
///
/// See the module docs: the final-location writes are made durable **before**
/// the tail moves past them, and re-applying a committed transaction is
/// idempotent, so a crash mid-checkpoint leaves either the pre-checkpoint
/// state (recovery re-applies, same bytes) or the post-checkpoint state —
/// never a torn in-between. A partial (deferred) advance keeps `s_start` on
/// the first unapplied transaction, which is exactly what recovery replays.
pub(super) fn checkpoint_advance(
    journal: &Journal,
    device: &dyn BlockDevice,
    target_free: Option<u32>,
) -> Result<bool> {
    // Serialize this pass against any other checkpoint driver and the commit
    // pipeline's clean→dirty `s_start` write, so the journal superblock has
    // one writer at a time (jbd2 `j_checkpoint_mutex`; see
    // [`Journal::j_checkpoint`]). Held for the whole pass; taken before the
    // state lock and never the reverse.
    let _checkpoint = journal.lock_checkpoint();

    // Snapshot the tail / committed-tid / head, the committed-revoke memory
    // (cloned so the applies below run without the lock — the state lock is
    // never held across I/O), and the UNPUBLISHED revoke sets: the running
    // transaction's, the force-locked (draining) one's — exactly as
    // unpublished, its commit block does not exist yet, and its own
    // operations may still be adding forgets while it drains — plus, when
    // the inline tail drain runs this pass mid-commit, the committing
    // slot's, all under one state-lock hold (uniform: all three sets live
    // in the journal state, so the snapshot cannot tear between them).
    //
    // The committed-revoke clone holds only *committed* revokes by
    // construction (commit publishes at its step 6, commit block already
    // durable), and only those may SUPPRESS an older image. An unpublished
    // revoke is unsound to act on in either direction:
    //
    // - It must not suppress-and-retire: a crash may still erase its
    //   transaction, and the suppressed older (fsync-acknowledged) image
    //   would then be missing from both the device and the retired log.
    // - Its block must not be APPLIED over either: the forget's free already
    //   took effect in memory — the bitmap freed the block, and a new
    //   owner's bytes may already sit at the final location by the time this
    //   pass runs (a caller-thread data flush needs no committer) — so
    //   applying the older image is the S1 runtime clobber, no crash
    //   required.
    //
    // Hence the defer-prefix rule in the loop below: stop in front of the
    // first transaction touching an unpublished revoke's block.
    //
    // The snapshot is taken once per pass: a forget landing AFTER it (a free
    // racing this pass on another thread) is outside this pass's defer set.
    // That arrival is harmless since freed-block pinning closed the reuse
    // half of the hazard structurally (`JournalState::pinned_frees`, Linux's
    // mballoc `ext4_mb_free_metadata` discipline): the mid-pass free pins
    // its blocks, so no new owner can exist — applying an older image over a
    // freed-but-unreused block is sound in both crash directions (the block
    // still belongs to its old life if the freeing transaction vanishes, and
    // its content is dont-care garbage once that transaction commits and
    // publishes the revoke). Nor can the pins release mid-pass: release runs
    // at commit step 6, and commits are serialized with checkpoint passes
    // (the single committer thread runs both; the unmount flush runs after
    // that thread stopped).
    let (dirty_tail, tail_tid, committed_tid, head, revoked, unpublished) = {
        let st = journal.state_read();
        let mut unpublished = BTreeSet::new();
        if let Some(running) = st.running.as_ref() {
            running.collect_unpublished_revokes(&mut unpublished);
        }
        if let Some(locking) = st.locking.as_ref() {
            locking.collect_unpublished_revokes(&mut unpublished);
        }
        if let Some(committing) = st.committing.as_ref() {
            committing.collect_unpublished_revokes(&mut unpublished);
        }
        (
            st.tail_block,
            st.tail_tid,
            journal.committed_tid(),
            st.head,
            st.revoked.clone(),
            unpublished,
        )
    };

    // A clean journal (no dirty tail) has nothing un-checkpointed.
    let Some(tail_block) = dirty_tail else {
        return Ok(false);
    };

    // An incremental pass whose target is already met reclaims nothing.
    if let Some(want_free) = target_free
        && journal.geometry().free_log_blocks(head, Some(tail_block)) >= want_free
    {
        return Ok(false);
    }

    // Apply each committed transaction in tid order, oldest first, stopping
    // EARLY in front of a transaction the unpublished revokes cover (defer),
    // or once `target_free` log blocks are free (an incremental tail advance —
    // jbd2 checkpoints only as far as space demands, not the whole log). Each
    // applied transaction is known-committed, so `apply_log_transaction` must
    // find a valid commit block; a failure means log corruption (EUCLEAN/EIO),
    // which we propagate.
    //
    // Loop termination: `committed_tid.geq(tid)` is jbd2's wrapping-aware
    // "is committed_tid at or after tid?". `tid` starts at `tail_tid` and steps
    // by one per committed transaction; because at most half the id space
    // separates `tail_tid` from `committed_tid` (they bound the live log), the
    // predicate flips to false exactly once `tid` passes `committed_tid`,
    // terminating after applying tid `committed_tid`.
    let mut log = tail_block;
    let mut tid = tail_tid;
    let mut applied_any = false;
    let mut stopped_early = false;
    while committed_tid.geq(tid) {
        if !unpublished.is_empty()
            && chain_covers_unpublished(journal, device, log, tid, &unpublished)?
        {
            // Deferred: `log`/`tid` stop ON this (unapplied) transaction.
            stopped_early = true;
            break;
        }
        log = apply_log_transaction(journal, device, log, tid, &revoked)?;
        applied_any = true;
        tid = tid.next();
        // Incremental stop: enough freed, and more remain. `log`/`tid` now name
        // the next (unapplied) transaction — the same partial-advance shape as
        // a defer, so the tail is pointed there below. `head` is fixed for the
        // whole pass (the single committer runs this, so no commit advances it),
        // so the free-segment measure is exact.
        if let Some(want_free) = target_free
            && committed_tid.geq(tid)
            && journal.geometry().free_log_blocks(head, Some(log)) >= want_free
        {
            stopped_early = true;
            break;
        }
    }

    // Nothing applied (deferred at the very tail, or a clean journal that a
    // concurrent unmount already checkpointed): no retirement, a pure no-op —
    // the next pass (after the revoking transaction commits and publishes)
    // makes progress.
    if !applied_any {
        return Ok(false);
    }

    // Everything the retirement below may cover: the last applied tid. On a
    // full pass this is `committed_tid`; on a partial one (deferred or
    // target-reached), the first-unapplied transaction's predecessor.
    let applied_through = tid.prev();

    // --- Barrier: every final-location write durable BEFORE we drop the log's
    // record of them by advancing s_start. ---
    barrier(device)?;

    if stopped_early {
        // --- Partial progress: point the on-disk tail AT the first unapplied
        // transaction (`log`/`tid` stopped on it), never past it — its images
        // are unapplied, so its log record must survive a crash. The journal
        // stays dirty; `s_head` is only read on the clean fast path
        // (`s_start == 0`) and stays untouched, like the commit path's
        // clean→dirty transition through this same funnel. ---
        journal.update_superblock_tail(device, log, tid)?;
    } else {
        // --- Rewrite the on-disk journal superblock to the clean state. ---
        let mut raw = journal.geometry().read_raw_superblock(device)?;
        // Journal now clean: nothing awaits recovery.
        raw.s_start = Be32::new(0);
        // The tid recovery would expect for the first transaction after this point.
        raw.s_sequence = Be32::new(committed_tid.next().get());
        // A clean-unmount superblock MUST carry a correct s_head: Linux's clean
        // fast path (recovery.c, s_start == 0) resumes the log from s_head, so a
        // stale value would corrupt a subsequent mount. `commit.rs` deliberately
        // leaves this to us. (Phase 4 Task 3 adversarial-review requirement.)
        raw.s_head = Be32::new(head);
        // The funnel restamps `s_checksum` on a csum journal and barriers: the
        // clean superblock must itself be durable.
        journal.geometry().write_superblock(device, raw)?;
    }

    // Publish the new tail in memory: cleared on a full pass (`head` is left
    // as-is — the ring continues from where commit left it; the next commit
    // sees a clean tail and re-establishes `s_start` at its own start block),
    // or moved to the first unapplied transaction on a partial one.
    //
    // The same critical section evicts the retained after-images this pass made
    // durable (their final-location writes are behind the barrier above, so the
    // device is authoritative for them again). The tid comparison keeps an image
    // from a commit NEWER than `applied_through`: evicting it would hand a
    // later capture the device's still-lagging bytes.
    {
        let mut st = journal.state_write();
        st.tail_block = if stopped_early { Some(log) } else { None };
        st.tail_tid = tid;
        st.uncheckpointed
            .retain(|_, image| !image.is_checkpointed_by(applied_through));
        // Retire the revoke records at the same tid boundary: with every
        // transaction up to `applied_through` applied (or suppressed) and the
        // tail advanced past them, a record with tid ≤ `applied_through` can
        // never fire again — no runtime consumer will ever re-apply those
        // transactions. Records from newer commits survive, exactly like the
        // images above.
        st.revoked.retire_through(applied_through);
    }

    Ok(true)
}

/// A full checkpoint pass: apply every committed-but-un-checkpointed
/// transaction to its final location and clear the journal
/// ([`checkpoint_advance`] with no space target). The full-reclaim entry the
/// commit thread's demanded pass, the commit-time inline drain
/// ([`Journal::commit_or_drain_tail`](super::Journal)), the unmount flush
/// ([`Journal::flush_on_unmount`](super::Journal)), and mount-time recovery
/// all use; the incremental low-water pass calls [`checkpoint_advance`]
/// directly.
pub(super) fn checkpoint(journal: &Journal, device: &dyn BlockDevice) -> Result<()> {
    checkpoint_advance(journal, device, None).map(|_| ())
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::{
        super::{
            super::test_utils::{Ext4FixtureBuilder, make_multi_block_file_inode},
            JOURNAL_INO,
            commit::commit_transaction,
            format::{BLOCKTYPE_SUPERBLOCK_V2, RawJournalSuperblock},
            load_geometry,
            transaction::Transaction,
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
            let generation = txn.capture_create(*bid);
            txn.apply_patch(*bid, generation, |b| b.copy_from_slice(content))
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
        assert_eq!(f.journal.state_read().tail_block, None);
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
        assert_eq!(f.journal.state_read().tail_block, None);

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
        assert_ne!(f.journal.state_read().tail_block, None);

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
        let next1 = apply_log_transaction(
            f.journal.as_ref(),
            device.as_ref(),
            start_log,
            Tid::new(1),
            &RevokeTable::new(),
        )
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
        let next2 = apply_log_transaction(
            f.journal.as_ref(),
            device.as_ref(),
            start_log,
            Tid::new(1),
            &RevokeTable::new(),
        )
        .unwrap();
        assert_eq!(read_final_block(&f, dest), after);
        assert_eq!(next1, next2);
    }

    /// The S1 runtime clobber, red→green: a block journaled by T1 (committed,
    /// NOT checkpointed), freed+revoked by T2 (committed), then reused as
    /// data content X, must still hold X after the checkpoint pass applies
    /// T1 — the suppression skips T1's stale image (removing the
    /// `RevokeTable::suppresses` gate in `apply_log_transaction` writes T1's
    /// image over X and fails this test). A control block journaled by T1
    /// and NOT revoked is restored normally, proving the skip is targeted.
    #[ktest]
    fn checkpoint_suppresses_revoked_block_over_reuse() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(24, 1, 1);
        let device = f.fixture.ext4.block_device();

        let (freed, control, bitmapish) = (500u64, 501u64, 600u64);
        let mut t1_freed = [0u8; BLOCK_SIZE];
        t1_freed[..8].copy_from_slice(b"T1FREED!");
        let mut t1_control = [0u8; BLOCK_SIZE];
        t1_control[..8].copy_from_slice(b"T1KEEP00");

        // T1 journals both blocks; committed but NOT checkpointed — its
        // images live only in the log (the dirty-ring window).
        let t1 = make_txn(Tid::new(1), &[(freed, t1_freed), (control, t1_control)]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t1).unwrap();

        // T2 frees `freed` via the forget path (its bitmap-ish capture keeps
        // the transaction committable, as every real free's bitmap/GDT
        // captures do), and commits — publishing the revoke.
        let mut t2 = make_txn(Tid::new(2), &[(bitmapish, [0x22u8; BLOCK_SIZE])]);
        t2.forget_block(freed);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t2).unwrap();
        assert_eq!(f.journal.committed_revoke_records_for_test(), 1);

        // The freed block is reused as file DATA: content X reaches its
        // final location directly (ordered data never passes the journal).
        let reused_data = [0xEEu8; BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .write_bytes(freed as usize * BLOCK_SIZE, &reused_data)
            .unwrap();

        checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();

        // The reused content survives T1's replay (suppressed)…
        assert_eq!(
            read_final_block(&f, freed),
            reused_data,
            "checkpoint replayed a revoked block's stale image over its reuse"
        );
        // …while the non-revoked blocks applied normally (targeted skip).
        assert_eq!(read_final_block(&f, control), t1_control);
        assert_eq!(read_final_block(&f, bitmapish), [0x22u8; BLOCK_SIZE]);
        // The journal is clean and the revoke record retired with its window.
        assert_eq!(read_journal_super(&f).s_start.get(), 0);
        assert_eq!(f.journal.committed_revoke_records_for_test(), 0);
    }

    /// "A log entry for a block beyond the last revoke still gets replayed"
    /// (revoke.c): a revoke in T1 suppresses nothing from T2 — a block freed
    /// and then re-journaled by a LATER transaction gets that newer image.
    #[ktest]
    fn checkpoint_applies_image_newer_than_revoke() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(24, 1, 1);
        let device = f.fixture.ext4.block_device();

        let (reborn, bitmapish) = (500u64, 600u64);

        // T1 frees `reborn` (revoke tid 1).
        let mut t1 = make_txn(Tid::new(1), &[(bitmapish, [0x11u8; BLOCK_SIZE])]);
        t1.forget_block(reborn);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t1).unwrap();

        // T2 reallocates it as metadata and journals a new image.
        let mut t2_img = [0u8; BLOCK_SIZE];
        t2_img[..8].copy_from_slice(b"T2REBORN");
        let t2 = make_txn(Tid::new(2), &[(reborn, t2_img)]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t2).unwrap();

        checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();

        // tid 2 > revoke tid 1: the newer image applies.
        assert_eq!(read_final_block(&f, reborn), t2_img);
    }

    /// The unpublished-revoke runtime clobber (MAJOR A), red→green: a block
    /// journaled by T1 (committed, NOT checkpointed) is freed by a still
    /// RUNNING T2 — the forget landed in T2's revoke set, unpublished — and
    /// reused as data content X (a caller-thread flush needs no committer).
    /// The checkpoint pass must DEFER: T1's images are covered by the
    /// unpublished revoke, so nothing is applied (X survives) and nothing is
    /// retired (the tail does not move — T1's log record must survive a
    /// crash that erases T2). Once T2 commits (publishing the revoke), the
    /// next pass applies T1 under ordinary suppression and reclaims fully.
    /// Reverting the defer gate writes T1's stale image over X in the first
    /// pass and fails the content assert; retiring anyway fails the tail
    /// asserts.
    #[ktest]
    fn checkpoint_defers_across_unpublished_revoke() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(24, 1, 1);
        let device = f.fixture.ext4.block_device();

        let (freed, control, bitmapish) = (500u64, 501u64, 600u64);
        let mut t1_freed = [0u8; BLOCK_SIZE];
        t1_freed[..8].copy_from_slice(b"T1FREED!");
        let mut t1_control = [0u8; BLOCK_SIZE];
        t1_control[..8].copy_from_slice(b"T1KEEP00");

        // T1 journals both blocks; committed but NOT checkpointed.
        let t1 = make_txn(Tid::new(1), &[(freed, t1_freed), (control, t1_control)]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t1).unwrap();

        // A live RUNNING T2 frees `freed`: the revoke is UNPUBLISHED (no
        // commit). Its bitmap-ish capture is what a real free's bitmap/GDT
        // captures look like.
        let mut t2 = make_txn(Tid::new(2), &[(bitmapish, [0x22u8; BLOCK_SIZE])]);
        t2.forget_block(freed);
        f.journal.state_write().running = Some(t2);
        assert_eq!(f.journal.committed_revoke_records_for_test(), 0);

        // The freed block is reused as file DATA: content X lands directly.
        let reused_data = [0xEEu8; BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .write_bytes(freed as usize * BLOCK_SIZE, &reused_data)
            .unwrap();

        checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();

        // Deferred: the reuse survives, NOTHING of T1 was applied, and the
        // tail did not move (T1 not retired) — on disk or in memory.
        assert_eq!(
            read_final_block(&f, freed),
            reused_data,
            "checkpoint applied a stale image over an unpublished revoke's reuse"
        );
        assert_eq!(read_final_block(&f, control), [0u8; BLOCK_SIZE]);
        assert_eq!(read_journal_super(&f).s_start.get(), 1);
        assert_eq!(f.journal.state_read().tail_block, Some(1));
        assert_eq!(f.journal.state_read().tail_tid, Tid::new(1));

        // T2 commits: the revoke publishes. The next pass applies T1 with
        // the published suppression (X still survives), applies T2, and
        // reclaims the whole log.
        let t2 = f.journal.state_write().running.take().unwrap();
        commit_transaction(f.journal.as_ref(), device.as_ref(), t2).unwrap();
        assert_eq!(f.journal.committed_revoke_records_for_test(), 1);

        checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();

        assert_eq!(read_final_block(&f, freed), reused_data);
        assert_eq!(read_final_block(&f, control), t1_control);
        assert_eq!(read_final_block(&f, bitmapish), [0x22u8; BLOCK_SIZE]);
        assert_eq!(read_journal_super(&f).s_start.get(), 0);
        assert_eq!(f.journal.state_read().tail_block, None);
        assert_eq!(f.journal.committed_revoke_records_for_test(), 0);
    }

    /// The committing-slot half of the defer-prefix collect (P7c-1, the
    /// inline tail-drain window): a block journaled by the committed (NOT
    /// checkpointed) T1 is forgotten by T2, which sits MID-COMMIT in the
    /// committing slot — `running` is empty and there is no caller-side
    /// channel, so the pass must find the unpublished revoke in the slot
    /// itself and defer in front of T1. Once T2's commit finishes
    /// (publishing the revoke), the next pass applies T1 under ordinary
    /// suppression and reclaims the whole log.
    #[ktest]
    fn checkpoint_defers_on_the_committing_slots_unpublished_revoke() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(24, 1, 1);
        let device = f.fixture.ext4.block_device();

        let (freed, control, bitmapish) = (500u64, 501u64, 600u64);
        let mut t1_freed = [0u8; BLOCK_SIZE];
        t1_freed[..8].copy_from_slice(b"T1FREED!");
        let mut t1_control = [0u8; BLOCK_SIZE];
        t1_control[..8].copy_from_slice(b"T1KEEP00");

        // T1 journals both blocks; committed but NOT checkpointed.
        let t1 = make_txn(Tid::new(1), &[(freed, t1_freed), (control, t1_control)]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t1).unwrap();

        // T2 forgets `freed` and is STAGED mid-flight: its revoke is
        // unpublished and lives only in the committing slot.
        let mut t2 = make_txn(Tid::new(2), &[(bitmapish, [0x22u8; BLOCK_SIZE])]);
        t2.forget_block(freed);
        let t2 = f.journal.stage_transaction_for_test(t2).unwrap();
        assert!(f.journal.state_read().running.is_none());
        assert_eq!(f.journal.committed_revoke_records_for_test(), 0);

        // The freed block is reused as file DATA: content X lands directly.
        let reused_data = [0xEEu8; BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .write_bytes(freed as usize * BLOCK_SIZE, &reused_data)
            .unwrap();

        // Deferred on the slot's revoke: nothing applied, tail unmoved.
        checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();
        assert_eq!(
            read_final_block(&f, freed),
            reused_data,
            "the pass must defer on the committing slot's unpublished revoke"
        );
        assert_eq!(read_final_block(&f, control), [0u8; BLOCK_SIZE]);
        assert_eq!(read_journal_super(&f).s_start.get(), 1);
        assert_eq!(f.journal.state_read().tail_block, Some(1));

        // T2's commit finishes (publishing the revoke); the next pass
        // applies T1 under ordinary suppression and reclaims fully.
        f.journal.commit_staged(&t2).unwrap();
        assert_eq!(f.journal.committed_revoke_records_for_test(), 1);
        checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();
        assert_eq!(read_final_block(&f, freed), reused_data);
        assert_eq!(read_final_block(&f, control), t1_control);
        assert_eq!(read_final_block(&f, bitmapish), [0x22u8; BLOCK_SIZE]);
        assert_eq!(read_journal_super(&f).s_start.get(), 0);
        assert_eq!(f.journal.state_read().tail_block, None);
        assert_eq!(f.journal.committed_revoke_records_for_test(), 0);
    }

    /// Defer-prefix partial progress: with T1 (clean) and T2 (touching a
    /// block a running T3 forgot) both committed, the pass applies T1, stops
    /// in FRONT of T2, and advances the tail exactly to T2 — on disk
    /// (`s_start`/`s_sequence` point at T2, the journal stays dirty) and in
    /// memory — never past the unapplied transaction. After T3 commits, the
    /// next pass finishes under published suppression.
    #[ktest]
    fn checkpoint_partial_prefix_stops_before_deferred_txn() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(24, 1, 1);
        let device = f.fixture.ext4.block_device();

        let (clean_dest, freed, bitmapish) = (700u64, 500u64, 600u64);
        let mut t1_img = [0u8; BLOCK_SIZE];
        t1_img[..8].copy_from_slice(b"T1CLEAN0");
        let mut t2_img = [0u8; BLOCK_SIZE];
        t2_img[..8].copy_from_slice(b"T2FREED0");

        // T1 (log [1..=3]) touches nothing revoked; T2 (log [4..=6]) journals
        // the block T3 will free.
        let t1 = make_txn(Tid::new(1), &[(clean_dest, t1_img)]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t1).unwrap();
        let t2 = make_txn(Tid::new(2), &[(freed, t2_img)]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t2).unwrap();

        // Running T3 forgets `freed` (unpublished) and the block is reused.
        let mut t3 = make_txn(Tid::new(3), &[(bitmapish, [0x33u8; BLOCK_SIZE])]);
        t3.forget_block(freed);
        f.journal.state_write().running = Some(t3);
        let reused_data = [0xEEu8; BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .write_bytes(freed as usize * BLOCK_SIZE, &reused_data)
            .unwrap();

        checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();

        // T1 applied; T2 deferred (the reuse survives); the tail sits ON T2.
        assert_eq!(read_final_block(&f, clean_dest), t1_img);
        assert_eq!(
            read_final_block(&f, freed),
            reused_data,
            "the deferred transaction must not be applied"
        );
        let sb = read_journal_super(&f);
        assert_eq!(sb.s_start.get(), 4, "on-disk tail advanced exactly to T2");
        assert_eq!(sb.s_sequence.get(), 2);
        assert_eq!(f.journal.state_read().tail_block, Some(4));
        assert_eq!(f.journal.state_read().tail_tid, Tid::new(2));

        // T3 commits (revoke tid 3 published): the next pass suppresses T2's
        // image of `freed` (2 ≤ 3), applies T3, and the journal is clean.
        let t3 = f.journal.state_write().running.take().unwrap();
        commit_transaction(f.journal.as_ref(), device.as_ref(), t3).unwrap();
        checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();

        assert_eq!(read_final_block(&f, freed), reused_data);
        assert_eq!(read_final_block(&f, bitmapish), [0x33u8; BLOCK_SIZE]);
        assert_eq!(read_journal_super(&f).s_start.get(), 0);
        assert_eq!(f.journal.state_read().tail_block, None);
        assert_eq!(f.journal.committed_revoke_records_for_test(), 0);
    }

    /// The defer decision over a revoke-led chain —
    /// `chain_covers_unpublished`'s revoke-block skip arm: T1's own forget
    /// puts a revoke block at its chain head ([revoke][desc][data…][commit]),
    /// and the block a running T2 forgot sits in a descriptor BEHIND it. The
    /// skip must advance the cursor by exactly one block: stepping wrong
    /// reads a data block as a chain header (`EUCLEAN` — the pass dies) or
    /// walks the tags out of position and misjudges the defer — the
    /// off-by-one catastrophe class (applying T1's stale image over the
    /// reuse). The pass must defer in front of T1, and the next pass (T2
    /// committed, revoke published) must finish the same walk on the apply
    /// side.
    #[ktest]
    fn defer_decision_walks_revoke_led_chain() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(24, 1, 1);
        let device = f.fixture.ext4.block_device();

        let (freed, control, bitmapish, unrelated) = (500u64, 501u64, 600u64, 900u64);
        let mut t1_freed = [0u8; BLOCK_SIZE];
        t1_freed[..8].copy_from_slice(b"T1FREED!");
        let mut t1_control = [0u8; BLOCK_SIZE];
        t1_control[..8].copy_from_slice(b"T1KEEP00");

        // T1 revokes `unrelated` (a block it never captured), so its
        // committed chain LEADS with a revoke block; the descriptor tagging
        // `freed` and `control` follows it.
        let mut t1 = make_txn(Tid::new(1), &[(freed, t1_freed), (control, t1_control)]);
        t1.forget_block(unrelated);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t1).unwrap();

        // Running T2 forgets `freed` (unpublished), and the block is reused
        // as data.
        let mut t2 = make_txn(Tid::new(2), &[(bitmapish, [0x22u8; BLOCK_SIZE])]);
        t2.forget_block(freed);
        f.journal.state_write().running = Some(t2);
        let reused_data = [0xEEu8; BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .write_bytes(freed as usize * BLOCK_SIZE, &reused_data)
            .unwrap();

        // The pass steps over T1's revoke block, finds `freed` covered in
        // the LATER descriptor, and defers: nothing applied, tail unmoved.
        checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();
        assert_eq!(
            read_final_block(&f, freed),
            reused_data,
            "the deferred transaction must not be applied over the reuse"
        );
        assert_eq!(read_final_block(&f, control), [0u8; BLOCK_SIZE]);
        assert_eq!(read_journal_super(&f).s_start.get(), 1);
        assert_eq!(f.journal.state_read().tail_block, Some(1));
        assert_eq!(f.journal.state_read().tail_tid, Tid::new(1));

        // T2 commits (its revoke publishes): the next pass walks the same
        // revoke-led chain on the apply side — T1's image of `freed`
        // suppressed, `control` applied — then applies T2 and reclaims the
        // whole log. The cursor math held on both walks.
        let t2 = f.journal.state_write().running.take().unwrap();
        commit_transaction(f.journal.as_ref(), device.as_ref(), t2).unwrap();
        checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();

        assert_eq!(read_final_block(&f, freed), reused_data);
        assert_eq!(read_final_block(&f, control), t1_control);
        assert_eq!(read_final_block(&f, bitmapish), [0x22u8; BLOCK_SIZE]);
        assert_eq!(read_journal_super(&f).s_start.get(), 0);
        assert_eq!(f.journal.state_read().tail_block, None);
        assert_eq!(f.journal.committed_revoke_records_for_test(), 0);
    }

    /// The b2 residual (a forget landing after a pass's snapshot, the block
    /// reused and caller-thread-flushed mid-pass), now structurally
    /// unreachable: the forget's free PINS the block, so while the freeing
    /// transaction is uncommitted the allocator cannot hand it to a new
    /// owner — there is no reuse for the pass's older image to clobber,
    /// whichever side of the snapshot the forget lands on. End-to-end
    /// through the real free/alloc funnels: T1 journals B and commits
    /// (un-checkpointed); a running T2 forgets-and-frees B; the allocator
    /// refuses B; a pass defers in front of T1 (belt) and, deferred or not,
    /// finds B unreused; after T2 commits, the pin releases with the revoke
    /// publication, the next pass suppresses T1's image of B (suspenders),
    /// and B is allocatable again.
    #[ktest]
    fn pinning_closes_the_mid_pass_reuse_window() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_journal_inode(32)
            .build()
            .unwrap();
        let journal = f.ext4.journal().unwrap();
        journal.stop_commit_thread();
        let device = f.ext4.block_device().clone();

        // T1: B journaled as fresh metadata (content 0x51), committed, NOT
        // checkpointed — T1's image of B waits in the log for a pass.
        let op1 = f.ext4.begin_op(8).unwrap();
        let b = f.ext4.alloc_blocks(1, 0, op1.get()).unwrap().start;
        super::super::get_create_access(op1.get(), b)
            .unwrap()
            .patch(|buf| buf.fill(0x51))
            .unwrap();
        drop(op1);
        journal.commit_now_for_test();
        assert!(journal.state_read().tail_block.is_some());

        // Running T2 forgets-and-frees B through the real funnel: the revoke
        // is unpublished and B is pinned.
        let op2 = f.ext4.begin_op(8).unwrap();
        f.ext4
            .free_blocks(super::super::forget(b, 1), op2.get())
            .unwrap();
        assert!(journal.pinned_frees_snapshot().contains(&(b, 1)));

        // The reuse the residual race needed cannot happen: the allocator
        // refuses B (first-fit would return the just-freed lowest block).
        let other = f.ext4.alloc_blocks(1, b, op2.get()).unwrap();
        assert!(!other.contains(&b));

        // A pass right now defers in front of T1 (B's revoke is unpublished)
        // and applies nothing; B's final location keeps its pre-T1 bytes.
        checkpoint(journal.as_ref(), device.as_ref()).unwrap();
        assert!(journal.state_read().tail_block.is_some());
        let mut on_disk = [0u8; BLOCK_SIZE];
        f.disk
            .segment()
            .read_bytes(b as usize * BLOCK_SIZE, &mut on_disk)
            .unwrap();
        assert_eq!(on_disk, [0u8; BLOCK_SIZE]);

        // T2 commits: the pin releases with the revoke publication (step 6).
        drop(op2);
        journal.commit_now_for_test();
        assert!(journal.pinned_frees_snapshot().is_empty());

        // The next pass retires everything; the published revoke keeps T1's
        // stale image of B off the device.
        checkpoint(journal.as_ref(), device.as_ref()).unwrap();
        assert_eq!(journal.state_read().tail_block, None);
        f.disk
            .segment()
            .read_bytes(b as usize * BLOCK_SIZE, &mut on_disk)
            .unwrap();
        assert_eq!(on_disk, [0u8; BLOCK_SIZE]);

        // B is allocatable again (first-fit reclaims it).
        let op3 = f.ext4.begin_op(8).unwrap();
        let reused = f.ext4.alloc_blocks(1, b, op3.get()).unwrap();
        assert!(reused.contains(&b));
        drop(op3);
    }

    /// The locking-seat third of the defer-prefix collect (P7c-2, group
    /// commit): a block journaled by the committed (NOT checkpointed) T1 is
    /// forgotten by T2, which sits FORCE-LOCKED in the locking seat,
    /// draining an open handle — `running` is empty and T2 is not in the
    /// committing slot either, so the pass must find the unpublished revoke
    /// in the seat itself and defer in front of T1. Once T2 drains and
    /// commits (publishing the revoke), the next pass applies T1 under
    /// ordinary suppression and reclaims the whole log.
    #[ktest]
    fn checkpoint_defers_on_the_locking_seats_unpublished_revoke() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(24, 1, 1);
        let device = f.fixture.ext4.block_device();

        let (freed, control, bitmapish) = (500u64, 501u64, 600u64);
        let mut t1_freed = [0u8; BLOCK_SIZE];
        t1_freed[..8].copy_from_slice(b"T1FREED!");
        let mut t1_control = [0u8; BLOCK_SIZE];
        t1_control[..8].copy_from_slice(b"T1KEEP00");

        // T1 journals both blocks; committed but NOT checkpointed.
        let t1 = make_txn(Tid::new(1), &[(freed, t1_freed), (control, t1_control)]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t1).unwrap();

        // T2 forgets `freed` and is parked in the locking seat, mid-drain:
        // its revoke is unpublished and lives only in the seat.
        let mut t2 = make_txn(Tid::new(2), &[(bitmapish, [0x22u8; BLOCK_SIZE])]);
        t2.forget_block(freed);
        f.journal.state_write().locking = Some(t2);
        assert_eq!(f.journal.committed_revoke_records_for_test(), 0);

        // The freed block is reused as file DATA: content X lands directly.
        let reused_data = [0xEEu8; BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .write_bytes(freed as usize * BLOCK_SIZE, &reused_data)
            .unwrap();

        checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();

        // Deferred: the reuse survives, NOTHING of T1 was applied, and the
        // tail did not move.
        assert_eq!(
            read_final_block(&f, freed),
            reused_data,
            "checkpoint applied a stale image over the locking seat's unpublished revoke"
        );
        assert_eq!(read_final_block(&f, control), [0u8; BLOCK_SIZE]);
        assert_eq!(f.journal.state_read().tail_block, Some(1));

        // T2 drains (no handles in this fixture) and commits: the revoke
        // publishes; the next pass applies T1 under suppression and
        // reclaims the whole log.
        let t2 = f.journal.state_write().locking.take().unwrap();
        commit_transaction(f.journal.as_ref(), device.as_ref(), t2).unwrap();
        assert_eq!(f.journal.committed_revoke_records_for_test(), 1);

        checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();

        assert_eq!(read_final_block(&f, freed), reused_data);
        assert_eq!(read_final_block(&f, control), t1_control);
        assert_eq!(read_final_block(&f, bitmapish), [0x22u8; BLOCK_SIZE]);
        assert_eq!(f.journal.state_read().tail_block, None);
        assert_eq!(f.journal.committed_revoke_records_for_test(), 0);
    }

    /// Incremental tail advance (P7c-3, lazy checkpoint): with three committed
    /// transactions un-checkpointed (the deep tail lazy checkpoint now leaves),
    /// `checkpoint_advance(Some(target))` applies the OLDEST transactions
    /// oldest-first only until the free segment reaches `target`, leaving the
    /// newer tail dirty — not the whole-log reclaim the full pass does. A
    /// second pass finishes it.
    #[ktest]
    fn checkpoint_advance_stops_at_the_free_target() {
        crate::time::clocks::init_for_ktest();
        // usable = 23; each single-block transaction occupies 3 log blocks
        // (descriptor + data + commit): T1 log [1,2,3], T2 [4,5,6], T3 [7,8,9],
        // head = 10, free = 23 - 9 = 14 with all three dirty.
        let f = journaled_fixture(24, 1, 1);
        let device = f.fixture.ext4.block_device();

        let dests = [500u64, 501, 502];
        let mut contents = [[0u8; BLOCK_SIZE]; 3];
        for (i, c) in contents.iter_mut().enumerate() {
            c[..8].copy_from_slice(&[0xA0 + i as u8; 8]);
            let txn = make_txn(Tid::new(i as u32 + 1), &[(dests[i], *c)]);
            commit_transaction(f.journal.as_ref(), device.as_ref(), txn).unwrap();
        }
        assert_eq!(f.journal.committed_tid(), Tid::new(3));
        assert_eq!(f.journal.state_read().tail_block, Some(1));

        // Free the ring up to 15 blocks: applying T1 alone lifts free from 14
        // to 17, so the pass stops after T1 with the tail pointed at T2.
        let advanced = checkpoint_advance(f.journal.as_ref(), device.as_ref(), Some(15)).unwrap();
        assert!(advanced);
        assert_eq!(read_final_block(&f, dests[0]), contents[0]);
        assert_eq!(
            read_final_block(&f, dests[1]),
            [0u8; BLOCK_SIZE],
            "T2 not yet"
        );
        let sb = read_journal_super(&f);
        assert_ne!(sb.s_start.get(), 0, "journal still dirty (partial)");
        assert_eq!(sb.s_sequence.get(), 2, "on-disk tail at T2");
        assert_eq!(f.journal.state_read().tail_tid, Tid::new(2));

        // Already at/above the target: an incremental pass reclaims nothing.
        assert!(!checkpoint_advance(f.journal.as_ref(), device.as_ref(), Some(15)).unwrap());

        // The full pass finishes: T2, T3 land and the journal cleans.
        checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();
        assert_eq!(read_final_block(&f, dests[1]), contents[1]);
        assert_eq!(read_final_block(&f, dests[2]), contents[2]);
        assert_eq!(read_journal_super(&f).s_start.get(), 0);
        assert_eq!(f.journal.state_read().tail_block, None);
    }

    /// Revoke suppression across a DEEP un-checkpointed log (P7c-3 audit (a)):
    /// a block journaled by T1 and freed by T3 must be suppressed when a single
    /// checkpoint pass applies FOUR committed-but-un-checkpointed transactions
    /// oldest-first — the committed-revoke memory holds T3's revoke while T1
    /// (two transactions older) is applied, and drops it only once the tail
    /// advances past T3. A control block T1 journaled and no one revoked lands
    /// normally, proving the deep-log skip is targeted.
    #[ktest]
    fn checkpoint_suppresses_across_a_deep_log() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(40, 1, 1);
        let device = f.fixture.ext4.block_device();

        let (freed, control) = (500u64, 501u64);
        let mut t1_freed = [0u8; BLOCK_SIZE];
        t1_freed[..8].copy_from_slice(b"T1FREED!");
        let mut t1_control = [0u8; BLOCK_SIZE];
        t1_control[..8].copy_from_slice(b"T1KEEP00");

        // T1 journals `freed` + `control`; T2 journals an unrelated block; T3
        // frees `freed` (revoke) plus a bitmap-ish capture; T4 journals another
        // unrelated block. None checkpointed — a four-deep dirty tail.
        let t1 = make_txn(Tid::new(1), &[(freed, t1_freed), (control, t1_control)]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t1).unwrap();
        let t2 = make_txn(Tid::new(2), &[(600, [0x22u8; BLOCK_SIZE])]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t2).unwrap();
        let mut t3 = make_txn(Tid::new(3), &[(601, [0x33u8; BLOCK_SIZE])]);
        t3.forget_block(freed);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t3).unwrap();
        let t4 = make_txn(Tid::new(4), &[(602, [0x44u8; BLOCK_SIZE])]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t4).unwrap();
        assert_eq!(f.journal.committed_tid(), Tid::new(4));
        assert_eq!(f.journal.committed_revoke_records_for_test(), 1);

        // `freed` is reused as file data.
        let reused_data = [0xEEu8; BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .write_bytes(freed as usize * BLOCK_SIZE, &reused_data)
            .unwrap();

        // One deep pass: T1's image of `freed` is suppressed (revoke tid 3 ≥ 1),
        // `control` lands, T2/T3/T4 land, the log cleans, the revoke retires.
        checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();
        assert_eq!(
            read_final_block(&f, freed),
            reused_data,
            "a deep-log pass replayed a revoked block's stale image over its reuse"
        );
        assert_eq!(read_final_block(&f, control), t1_control);
        assert_eq!(read_final_block(&f, 600), [0x22u8; BLOCK_SIZE]);
        assert_eq!(read_final_block(&f, 601), [0x33u8; BLOCK_SIZE]);
        assert_eq!(read_final_block(&f, 602), [0x44u8; BLOCK_SIZE]);
        assert_eq!(read_journal_super(&f).s_start.get(), 0);
        assert_eq!(f.journal.committed_revoke_records_for_test(), 0);
    }
}
