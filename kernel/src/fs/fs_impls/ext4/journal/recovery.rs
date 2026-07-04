// SPDX-License-Identifier: MPL-2.0

//! Journal recovery (jbd2 `recovery.c`, `do_one_pass` / `jbd2_journal_recover`).
//!
//! When a filesystem is mounted after a crash, its journal may hold
//! committed-but-un-checkpointed transactions whose after-images never reached
//! their final on-disk locations (checkpoint had not run, or a crash interrupted
//! it). Recovery replays those transactions so the filesystem is consistent
//! again. This is the mechanism the Phase-4 guest matrix (mount a dirty journal
//! → recover → host `e2fsck -fn` CLEAN) depends on.
//!
//! # Two-pass model
//!
//! jbd2 runs recovery in passes over the log, walking transactions from the tail
//! (`s_start`) forward, each transaction bearing the next `tid` in sequence
//! (`s_sequence`, `s_sequence + 1`, …). This module implements the two Phase-4
//! passes; PASS_REVOKE is Phase 7 (see the gaps below):
//!
//! - **PASS_SCAN** ([`scan_transaction`] in a loop): walks the log from the tail,
//!   counting how many complete committed transactions are present, and finds the
//!   first *uncommitted* tid — the boundary at which the log ends. It writes
//!   nothing.
//! - **PASS_REPLAY** ([`apply_log_transaction`](super::checkpoint::apply_log_transaction)
//!   in a loop): re-walks exactly the transactions SCAN found committed, applying
//!   each after-image to its **final** location. This is the same primitive
//!   checkpoint uses, so replay and checkpoint apply an identical transaction
//!   identically.
//!
//! After REPLAY the journal is marked clean on disk (`s_start = 0`) and the
//! in-memory state is reset so continued operation assigns fresh tids past the
//! recovered ones.
//!
//! # The monotonic-sequence staleness argument (why SCAN's boundary is correct)
//!
//! The log is a ring: after it wraps, a fresh descriptor may be written over a
//! log block that still physically holds a *stale* descriptor from a previous
//! wrap. What distinguishes the two is the transaction id in `h_sequence`: tids
//! increase by one per committed transaction and never repeat within one live log
//! (the log is bounded well under `2^32` transactions between checkpoints). SCAN
//! therefore expects an *exact* tid at each step (`s_sequence + k` at the k-th
//! transaction); a block whose `h_sequence` does not match the expected tid is a
//! stale leftover (or a blank/interrupted block), i.e. the boundary — never a
//! transaction to replay. Matching jbd2's `next_commit_ID` walk, this is what
//! makes recovery stop at the true end of the log rather than replaying garbage
//! from an earlier generation.
//!
//! On a csum v2/v3 journal, checksum verification (P7a-3) *strengthens* this
//! boundary decision: a block whose header happens to carry the expected tid
//! must now also re-checksum under this journal's UUID-derived seed before a
//! commit block seals its transaction, which catches torn writes and
//! leftovers from a *different* journal generation (different UUID ⇒
//! different seed ⇒ mismatch — the lazy-journal-init staleness jbd2 added the
//! checks for). What no checksum closes — ours or Linux's — is a
//! *same-journal* block from exactly `2^32` transactions ago: its seed,
//! sequence, and content are all identical, so it re-checksums identically.
//! That edge rests, exactly as in jbd2, on the boundedness argument above (a
//! log holds far fewer than `2^32` transactions between cleanings), not on
//! checksums.
//!
//! # Idempotence
//!
//! Recovery is idempotent, exactly as checkpoint's crash-safety argument relies
//! on: applying a committed transaction rewrites the same after-image bytes to
//! the same final locations, so running recovery twice — or after a crash *during*
//! recovery, before the clean-superblock write reached the platter — replays the
//! same committed transactions and reaches the same clean state. The clean-
//! superblock write is the single durable flip from "dirty (`s_start` at the
//! tail)" to "clean (`s_start == 0`)"; a crash before it simply re-runs SCAN +
//! REPLAY next mount (same bytes), a crash after it sees a clean journal and does
//! nothing.
//!
//! # Gaps
//!
//! - **No PASS_REVOKE**: we write no revoke blocks, so a [`BLOCKTYPE_REVOKE`]
//!   block met during recovery means an interop (Linux-written) log whose
//!   committed transactions carry revoke records we cannot apply. Rather than
//!   silently under-replay it (treating the revoke block as a boundary), SCAN
//!   refuses the mount with `EUCLEAN`. Full revoke replay-suppression is P7b.
//! - **Checksums are verify-only (P7a-3)**: on a csum v2/v3 journal, SCAN
//!   verifies the descriptor-tail and commit-block checksums (in the seal
//!   decision) and REPLAY verifies the descriptor-tail and per-tag data
//!   checksums; the jbd2 v1 COMPAT checksum (`JBD2_FEATURE_COMPAT_CHECKSUM`'s
//!   accumulated `crc32_be`) is not modeled — an unknown COMPAT bit is safe
//!   to ignore by definition. The write side does not stamp checksums yet
//!   (P7a-4), so csum journals stay unadmitted at mount and these paths run
//!   under ktest fixtures. The `2^32`-wrap ambiguity is NOT closed by any of
//!   this (see above).
//!
//! # Reuse
//!
//! REPLAY reuses [`apply_log_transaction`](super::checkpoint::apply_log_transaction)
//! verbatim; [`scan_transaction`] walks the same descriptor-tag geometry through
//! the one shared [`TagLayout`](super::format::TagLayout) definition but is
//! read-only and treats a missing/incomplete transaction as the boundary
//! rather than corruption. The wrap walk uses
//! [`next_log_block`](super::commit::next_log_block) and the final durability
//! flush uses [`barrier`](super::commit::barrier) — one definition each, shared
//! with commit/checkpoint.

// Recovery is now live in all builds: `Ext4::open` calls [`recover`] at mount
// time to replay a dirty journal, which pulls in the whole SCAN/REPLAY cluster
// (`scan_transaction` and the geometry's log readers). No dead-code gate is
// needed anymore (the earlier module-level `allow(dead_code)` was a placeholder
// for exactly this integration).

use core::sync::atomic::Ordering;

use super::{
    super::prelude::*,
    Journal, Tid,
    checkpoint::apply_log_transaction,
    commit::barrier,
    format::{
        BLOCKTYPE_COMMIT, BLOCKTYPE_DESCRIPTOR, BLOCKTYPE_REVOKE, Be32, JBD2_MAGIC,
        RawJournalHeader, RawJournalSuperblock,
    },
};

/// Scans one transaction at `start_log` expecting tid `expected_tid`, WITHOUT
/// writing anything (jbd2 `do_one_pass` in `PASS_SCAN`).
///
/// Returns `Some(next_start_log)` when a complete committed transaction is present
/// — a valid descriptor for `expected_tid` followed, after its metadata blocks, by
/// a valid commit block for `expected_tid`. Returns `Ok(None)` at the **log
/// boundary**: a block that is not a matching descriptor, or a descriptor with no
/// valid commit block (an interrupted commit). `None` is the normal end of the
/// log, **not** an error — this is the one place recovery differs from
/// [`apply_log_transaction`](super::checkpoint::apply_log_transaction), which
/// treats the same conditions as corruption (`EUCLEAN`).
///
/// # Byte layout (one shared definition)
///
/// The descriptor's tag geometry is owned by the journal's
/// [`TagLayout`](super::format::TagLayout): this walk iterates
/// [`TagLayout::walk`](super::format::TagLayout::walk), the same definition the
/// commit writer's [`TagWriter`](super::format::TagWriter) and
/// [`apply_log_transaction`](super::checkpoint::apply_log_transaction) use, so
/// the three cannot drift. Each tag's metadata block is the next log block
/// after the previous ([`next_log_block`]); the commit block is the next log
/// block after the last metadata block.
///
/// # Boundary conditions (each yields `Ok(None)`)
///
/// - The descriptor's magic/type is wrong, or its `h_sequence != expected_tid`
///   (a stale block left by a previous log wrap, or a blank/never-written block —
///   the monotonic sequence is what tells a fresh descriptor from a stale one).
///   A [`BLOCKTYPE_REVOKE`] block encountered during recovery is instead a hard
///   error (`EUCLEAN`) — see the revoke gap below.
/// - A tag offset would run past the descriptor block (a malformed descriptor is
///   not a valid transaction; do not panic).
/// - The trailing commit block's magic/type/`h_sequence` does not match (an
///   interrupted commit — the descriptor and metadata were written but the commit
///   record never reached the platter, so the transaction did not commit).
/// - A **bad descriptor-tail checksum with no valid commit block** downstream
///   (csum journals only): the torn tail tore inside the descriptor itself.
///
/// # Checksum politics (csum v2/v3 journals; `EUCLEAN`, not a boundary)
///
/// Mirrors Linux `do_one_pass` PASS_SCAN, minus the two escapes we
/// deliberately drop (documented divergences):
///
/// - A bad **descriptor-tail** checksum does not fail the scan by itself —
///   PASS_SCAN can meet stale lazy-init blocks, so jbd2 defers judgment
///   (`need_check_commit_time`, fs/jbd2/recovery.c:571-586) until the commit
///   slot. If a valid commit block for the same tid seals the transaction,
///   the descriptor corruption is real and the scan refuses (`EUCLEAN`);
///   jbd2 reaches the same refusal through the commit-time comparison
///   (recovery.c:749-760, `-EFSBADCRC`). *Divergence*: jbd2 treats a
///   **decreasing** `h_commit_sec` as stale pre-existing data and ends
///   recovery successfully at this boundary (recovery.c:761-767); we always
///   refuse — strictly safer (a refused mount, never an under-replayed one),
///   and we never lazily init a journal ourselves.
/// - A bad **commit-block** checksum on an otherwise matching commit block
///   refuses the scan (`EUCLEAN`): the transaction cannot prove it committed
///   intact. This is jbd2's sync-commit path (recovery.c:806-817 records
///   `j_failed_commit`, and `jbd2_journal_load` then refuses the mount with
///   `-EFSCORRUPTED`, fs/jbd2/journal.c:2078-2083). We do not support
///   `INCOMPAT_ASYNC_COMMIT`, so its keep-scanning leniency and the
///   decreasing-commit-time escape (recovery.c:810-811) are both dropped.
///
/// Device errors propagate as `Err(EIO)`.
fn scan_transaction(
    journal: &Journal,
    device: &dyn BlockDevice,
    start_log: u32,
    expected_tid: Tid,
) -> Result<Option<u32>> {
    // --- Descriptor block. ---
    let mut descriptor = [0u8; BLOCK_SIZE];
    journal
        .geometry()
        .read_log_block(device, start_log, &mut descriptor)?;
    let header = RawJournalHeader::parse(&descriptor);
    let blocktype = header.h_blocktype.get();
    // Revoke gap (full PASS_REVOKE is Phase 7): our own log never emits revoke
    // blocks, so one appearing during recovery means an interop (Linux-written)
    // journal whose committed transactions carry revoke records we cannot apply.
    // Treating it as a clean boundary would silently under-replay a committed
    // transaction (and everything after it), then stamp a too-small `s_sequence`
    // on the clean superblock — corruption. Refuse the mount loudly instead.
    if blocktype == BLOCKTYPE_REVOKE {
        return_errno_with_message!(
            Errno::EUCLEAN,
            "journal contains revoke records; recovery unsupported until Phase 7"
        );
    }
    // Boundary, not corruption: a stale/blank block (bad magic or a non-descriptor
    // type), or a descriptor from a different generation (wrong tid), marks the end
    // of the committed log. The monotonic `h_sequence` check is the crux: it
    // distinguishes a freshly written descriptor for `expected_tid` from a stale
    // one a previous wrap left behind.
    if header.h_magic.get() != JBD2_MAGIC
        || blocktype != BLOCKTYPE_DESCRIPTOR
        || Tid::new(header.h_sequence.get()) != expected_tid
    {
        return Ok(None);
    }

    // Descriptor-tail checksum (csum journals): verified here, judged at the
    // commit slot below — jbd2's PASS_SCAN defers a descriptor-checksum
    // failure (recovery.c:571-586) because the block may be stale garbage
    // whose transaction never committed (then it is the normal boundary, not
    // corruption). Only a valid commit block sealing this tid turns the
    // mismatch into a refusal. See the checksum-politics doc above.
    let seed = journal.geometry().csum_seed();
    let descriptor_csum_ok = seed.is_none_or(|s| s.verify_block_tail(&descriptor));

    // Walk the tag array (the byte geometry lives in ONE place, the journal's
    // `TagLayout`, shared with the writer and the checkpoint/replay reader),
    // advancing a `log` cursor one block per tag — the same walk the
    // reader/writer use to place the logged metadata. We only need the cursor's
    // final position (where the commit block sits); the count is implicit in
    // the walk.
    let mut log = start_log;
    for tag in journal.geometry().tag_layout().walk(&descriptor) {
        // A malformed tag array (a tag overrunning the tag area, or garbage
        // flags) is not a valid committed transaction: treat it as the
        // boundary (never panic on a bad log).
        if tag.is_err() {
            return Ok(None);
        }

        // This tag's metadata is the next log block after the previous one.
        log = journal.geometry().next_log_block(log);
    }

    // --- Commit block: the next log block after the last metadata block. ---
    let commit_log = journal.geometry().next_log_block(log);
    let mut commit = [0u8; BLOCK_SIZE];
    journal
        .geometry()
        .read_log_block(device, commit_log, &mut commit)?;
    let commit_header = RawJournalHeader::parse(&commit);
    if commit_header.h_magic.get() == JBD2_MAGIC {
        let commit_blocktype = commit_header.h_blocktype.get();
        if commit_blocktype == BLOCKTYPE_COMMIT
            && Tid::new(commit_header.h_sequence.get()) == expected_tid
        {
            // A matching commit block seals the transaction ONLY if the
            // checksums agree (csum journals; see the politics doc above):
            //
            // - The deferred descriptor verdict lands here: a valid commit
            //   sealing a checksum-bad descriptor is real corruption, never a
            //   boundary (jbd2 recovery.c:749-760, minus the commit-time
            //   staleness escape we deliberately drop).
            if !descriptor_csum_ok {
                return_errno_with_message!(
                    Errno::EUCLEAN,
                    "journal descriptor checksum invalid on a committed transaction"
                );
            }
            // - The commit block must itself re-checksum, or the transaction
            //   cannot prove it committed intact (jbd2's sync-commit refusal,
            //   recovery.c:806-817 + journal.c:2078-2083; we support no
            //   ASYNC_COMMIT leniency).
            if seed.is_some_and(|s| !s.verify_commit_block(&commit)) {
                return_errno_with_message!(
                    Errno::EUCLEAN,
                    "journal commit block checksum mismatch"
                );
            }
            // A complete committed transaction: the next one starts right after
            // this commit block.
            return Ok(Some(journal.geometry().next_log_block(commit_log)));
        }
        if commit_blocktype == BLOCKTYPE_REVOKE {
            // A revoke block sits where this transaction's commit was computed to
            // be. Linux writes revoke records between the metadata and the commit
            // block, so the real commit is further along and this transaction *did*
            // commit — but we cannot walk past revoke blocks (PASS_REVOKE is Phase
            // 7). Treating this as a torn tail would silently under-replay a
            // committed transaction, so fail loudly like the multi-descriptor case.
            return_errno_with_message!(
                Errno::EUCLEAN,
                "journal revoke records unsupported in recovery until Phase 7"
            );
        }
        if commit_blocktype == BLOCKTYPE_DESCRIPTOR {
            // A *second* descriptor where the commit block should be: this
            // transaction spans multiple descriptor blocks. Our writer never
            // produces these (one descriptor per transaction, capped at
            // `max_credits`), but a Linux-written log can for a large transaction
            // (`commit.c` starts a new descriptor when a tag no longer fits). We
            // have no code path to walk a second descriptor, so we would otherwise
            // mistake this for a torn tail and *silently under-replay a committed
            // transaction* — a latent inconsistency. Fail LOUDLY instead so the
            // mount refuses rather than corrupts (adversarial-review Finding 5).
            // Full multi-descriptor support is a later phase.
            return_errno_with_message!(
                Errno::EUCLEAN,
                "multi-descriptor journal transaction unsupported in Phase 4"
            );
        }
    }
    // Interrupted commit (or a stale/blank commit slot): the transaction did not
    // commit, so this is the boundary — the normal end of the log.
    Ok(None)
}

/// Recovers the journal after a crash (jbd2 `jbd2_journal_recover`): PASS_SCAN
/// finds the last committed transaction, PASS_REPLAY applies every committed
/// transaction to its final locations, then the journal is marked clean.
///
/// Idempotent — running it twice (or after a crash mid-recovery, before the
/// clean-superblock write was durable) replays the same committed transactions
/// and reaches the same clean state (see the module docs).
///
/// # Passes
///
/// 1. Read the on-disk journal superblock. `s_start == 0` means the journal is
///    already clean: nothing to recover, return `Ok(())`.
/// 2. **PASS_SCAN**: from `(s_start, s_sequence)`, [`scan_transaction`] each
///    transaction, counting the committed ones until it returns `None` (the
///    boundary). `end_tid` is then the first uncommitted / next-expected tid.
/// 3. **PASS_REPLAY**: re-walk exactly those `count` transactions with
///    [`apply_log_transaction`](super::checkpoint::apply_log_transaction),
///    applying each to its final locations. Each was found committed by SCAN, so
///    a failure here is real corruption and propagates.
/// 4. **Barrier**, so every replayed final-location write is durable before the
///    journal is marked clean.
/// 5. Rewrite the on-disk journal superblock to the clean state
///    (`s_start = 0`, `s_sequence = end_tid`, `s_head = first`), then barrier.
/// 6. Reset the in-memory journal state to the clean post-recovery state.
///
/// # SCAN/REPLAY count agreement
///
/// REPLAY re-walks from the identical `(s_start, s_sequence)` start with the same
/// per-tag / wrap arithmetic as SCAN (both use [`next_log_block`] and the same
/// offset stride, and [`apply_log_transaction`] is the exact reader
/// [`scan_transaction`] mirrors), so the k-th REPLAY step lands on the same log
/// block SCAN's k-th step did and consumes the same tid. Replaying exactly
/// `count` transactions therefore stops precisely at the boundary SCAN found — no
/// interrupted-tail transaction is ever replayed.
pub(in crate::fs::fs_impls::ext4) fn recover(
    journal: &Journal,
    device: &dyn BlockDevice,
) -> Result<()> {
    // --- Read the on-disk journal superblock (log block 0). ---
    let sb_pblock = journal
        .geometry()
        .log_block_to_physical(0)
        .ok_or_else(|| Error::with_message(Errno::EUCLEAN, "journal superblock block unmapped"))?;
    let sb_offset = Bid::new(sb_pblock).to_offset();
    let mut raw: RawJournalSuperblock = device
        .read_val(sb_offset)
        .map_err(|_| Error::with_message(Errno::EIO, "failed to read journal superblock"))?;

    let s_start = raw.s_start.get();
    let s_sequence = Tid::new(raw.s_sequence.get());

    // A clean journal (`s_start == 0`) has nothing to recover.
    if s_start == 0 {
        return Ok(());
    }

    let first = journal.geometry().first();

    // --- PASS_SCAN: count the committed transactions and find the boundary. ---
    // `tid` walks the expected sequence (s_sequence, s_sequence+1, …); `log`
    // walks the log from the tail. `scan_transaction` returns the next start on a
    // complete committed transaction, or `None` at the boundary. `end_tid` ends up
    // as the first uncommitted / next-expected tid.
    let mut log = s_start;
    let mut tid = s_sequence;
    let mut count = 0u64;
    while let Some(next) = scan_transaction(journal, device, log, tid)? {
        log = next;
        tid = tid.next();
        count += 1;
    }
    let end_tid = tid;

    // --- PASS_REPLAY: apply exactly the `count` committed transactions. ---
    // Re-walk from the same tail with the same tids; each was found committed by
    // SCAN, so `apply_log_transaction` must succeed (an error is real corruption
    // and propagates). Because REPLAY uses the same arithmetic as SCAN, applying
    // `count` transactions stops exactly at the boundary — an interrupted tail is
    // never replayed. (When `count == 0` nothing is replayed: the log records a
    // start but the first block is not a valid committed transaction; recovery
    // just marks the journal clean with `end_tid == s_sequence`.)
    let mut log = s_start;
    let mut tid = s_sequence;
    for _ in 0..count {
        log = apply_log_transaction(journal, device, log, tid)?;
        tid = tid.next();
    }

    // --- Barrier: every replayed final-location write must be durable BEFORE we
    // drop the log's record of it by clearing s_start (the same ordering
    // checkpoint uses). ---
    barrier(device)?;

    // --- Mark the journal clean on disk. ---
    // The log is now empty: `s_start = 0`, `s_sequence` advances to the first tid
    // recovery would expect next (`end_tid`), and `s_head` resets to `first` so a
    // subsequent commit restarts the ring at the first log-data block. (Linux's
    // clean fast path reads `s_head` when `s_start == 0`; a stale value would
    // resume the log at the wrong offset — mirroring checkpoint's `s_head` care.)
    // On a csum journal this rewrite would also have to restamp `s_checksum`
    // (Linux `jbd2_write_superblock`, journal.c:1812-1813) — that is P7a-4's
    // write side; until it lands, admission keeps csum journals off real
    // mounts, so no production superblock is ever rewritten unstamped.
    raw.s_start = Be32::new(0);
    raw.s_sequence = Be32::new(end_tid.get());
    raw.s_head = Be32::new(first);
    device
        .write_val(sb_offset, &raw)
        .map_err(|_| Error::with_message(Errno::EIO, "failed to write journal superblock"))?;

    // --- Barrier: the clean superblock must itself be durable. ---
    barrier(device)?;

    // --- Publish the clean post-recovery state in memory. ---
    // The log is empty and the ring restarts at `first`; the next transaction
    // assigns tid `end_tid` (and `committed_tid` is one behind, since nothing is
    // committed past what we just recovered-and-cleaned).
    {
        let mut st = journal.state_write();
        st.head = first;
        st.tail_block = 0;
        st.tail_tid = end_tid;
        st.next_tid = end_tid;
    }
    journal
        .committed_tid
        .store(end_tid.prev().get(), Ordering::Release);

    Ok(())
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::{
        super::{
            super::test_utils::{Ext4FixtureBuilder, make_multi_block_file_inode},
            JOURNAL_INO, JournalGeometry, Transaction,
            commit::commit_transaction,
            format::{
                BLOCKTYPE_SUPERBLOCK_V2, INCOMPAT_CSUM_V3, JBD2_CRC32C_CHKSUM, JournalSuperblock,
                RawCommitBlock, TAG_FLAG_LAST_TAG, TAG_FLAG_SAME_UUID,
            },
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

    /// Overwrites a final (filesystem) block off the device.
    fn write_final_block(f: &JournaledFixture, bid: u64, content: &[u8; BLOCK_SIZE]) {
        f.fixture
            .disk
            .segment()
            .write_val(bid as usize * BLOCK_SIZE, content)
            .unwrap();
    }

    /// Reads a whole log block off the device by its log index.
    fn read_log_block_at(f: &JournaledFixture, log: u32) -> [u8; BLOCK_SIZE] {
        let mut buf = [0u8; BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .read_bytes((JOURNAL_START_BLOCK + log) as usize * BLOCK_SIZE, &mut buf)
            .unwrap();
        buf
    }

    /// Writes a whole log block off the device by its log index.
    fn write_log_block_at(f: &JournaledFixture, log: u32, content: &[u8; BLOCK_SIZE]) {
        f.fixture
            .disk
            .segment()
            .write_val((JOURNAL_START_BLOCK + log) as usize * BLOCK_SIZE, content)
            .unwrap();
    }

    /// Reads the on-disk journal superblock.
    fn read_journal_super(f: &JournaledFixture) -> RawJournalSuperblock {
        f.fixture
            .disk
            .segment()
            .read_val(JOURNAL_START_BLOCK as usize * BLOCK_SIZE)
            .unwrap()
    }

    /// Restores the on-disk journal superblock's `s_start`/`s_sequence` to a
    /// pre-recovery (dirty) state. Used to simulate a crash where recovery had run
    /// but its single clean-superblock write (which flips BOTH fields together) was
    /// lost — so both must be restored, not just `s_start`.
    fn set_journal_tail(f: &JournaledFixture, start: u32, sequence: u32) {
        let mut raw = read_journal_super(f);
        raw.s_start = Be32::new(start);
        raw.s_sequence = Be32::new(sequence);
        f.fixture
            .disk
            .segment()
            .write_val(JOURNAL_START_BLOCK as usize * BLOCK_SIZE, &raw)
            .unwrap();
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

    /// A block filled distinctly enough to be recognized in the final location.
    fn tagged_block(tag: &[u8]) -> [u8; BLOCK_SIZE] {
        let mut b = [0u8; BLOCK_SIZE];
        b[..tag.len()].copy_from_slice(tag);
        b[BLOCK_SIZE - 4..].copy_from_slice(b"TAIL");
        b
    }

    /// Test 1: recovery replays a single committed-but-un-checkpointed transaction
    /// to its final location and marks the journal clean.
    #[ktest]
    fn recover_replays_committed_transaction() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);
        let device = f.fixture.ext4.block_device();

        let dest = 500u64;
        let after = tagged_block(b"AFTERIMG");

        // Commit (writes the after-image to the log + sets s_start), but do NOT
        // checkpoint: the final location still holds the fixture's zeroed disk.
        let txn = make_txn(Tid::new(1), &[(dest, after)]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), txn).unwrap();
        assert_eq!(f.journal.committed_tid(), Tid::new(1));
        assert_eq!(read_final_block(&f, dest), [0u8; BLOCK_SIZE]);
        assert_ne!(read_journal_super(&f).s_start.get(), 0);

        recover(f.journal.as_ref(), device.as_ref()).unwrap();

        // The after-image is now at its final location.
        assert_eq!(read_final_block(&f, dest), after);

        // The on-disk journal superblock is clean, with the next tid and a reset
        // head.
        let sb = read_journal_super(&f);
        assert_eq!(sb.s_start.get(), 0);
        assert_eq!(sb.s_sequence.get(), 2); // end_tid = committed_tid + 1
        assert_eq!(sb.s_head.get(), f.journal.geometry().first());

        // In-memory clean state.
        let st = f.journal.state_read();
        assert_eq!(st.tail_block, 0);
        assert_eq!(st.tail_tid, Tid::new(2));
        assert_eq!(st.next_tid, Tid::new(2));
        assert_eq!(st.head, f.journal.geometry().first());
        drop(st);
        assert_eq!(f.journal.committed_tid(), Tid::new(1)); // end_tid - 1
    }

    /// Test 2: recovery is idempotent — a straight double-recover is a no-op, and
    /// re-running after a "clean-write lost" crash restores the same bytes.
    #[ktest]
    fn recover_is_idempotent() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(16, 1, 1);
        let device = f.fixture.ext4.block_device();

        let dest = 500u64;
        let after = tagged_block(b"IDEMPOT!");

        let txn = make_txn(Tid::new(1), &[(dest, after)]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), txn).unwrap();

        recover(f.journal.as_ref(), device.as_ref()).unwrap();
        assert_eq!(read_final_block(&f, dest), after);
        assert_eq!(read_journal_super(&f).s_start.get(), 0);

        // A straight second recover sees a clean journal (s_start == 0) and is a
        // no-op: it must not error and must not disturb the final location.
        recover(f.journal.as_ref(), device.as_ref()).unwrap();
        assert_eq!(read_final_block(&f, dest), after);
        assert_eq!(read_journal_super(&f).s_start.get(), 0);

        // Simulate "recovery ran but the clean-superblock write was lost to a
        // crash": clobber the final location and restore the on-disk tail
        // (s_start + s_sequence) to its pre-recovery values — the clean write flips
        // both together, so a lost write means both are still at the dirty state.
        write_final_block(&f, dest, &[0u8; BLOCK_SIZE]);
        assert_eq!(read_final_block(&f, dest), [0u8; BLOCK_SIZE]);
        set_journal_tail(&f, f.journal.geometry().first(), 1);

        // Recover again: the same committed transaction replays to the same bytes
        // and the journal returns to the same clean state.
        recover(f.journal.as_ref(), device.as_ref()).unwrap();
        assert_eq!(read_final_block(&f, dest), after);
        let sb = read_journal_super(&f);
        assert_eq!(sb.s_start.get(), 0);
        assert_eq!(sb.s_sequence.get(), 2);
    }

    /// Test 3: two committed transactions both replay; on a shared block the newer
    /// (later-tid) one wins.
    #[ktest]
    fn recover_replays_two_transactions_newest_wins() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(24, 1, 1);
        let device = f.fixture.ext4.block_device();

        let shared = 700u64;
        let t1_only = 500u64;
        let content_a = tagged_block(b"CONTENTA");
        let content_b = tagged_block(b"CONTENTB");
        let t1_side = tagged_block(b"T1SIDE00");

        // T1 writes A to `shared` and a distinct block to `t1_only`; T2 writes B to
        // `shared`. Commit both, checkpoint neither.
        let t1 = make_txn(Tid::new(1), &[(shared, content_a), (t1_only, t1_side)]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t1).unwrap();
        let t2 = make_txn(Tid::new(2), &[(shared, content_b)]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t2).unwrap();
        assert_eq!(f.journal.committed_tid(), Tid::new(2));

        recover(f.journal.as_ref(), device.as_ref()).unwrap();

        // Both applied, in tid order: T2's B wins the shared block, and T1's own
        // block is present too (proving T1 was replayed, not skipped).
        assert_eq!(read_final_block(&f, shared), content_b);
        assert_eq!(read_final_block(&f, t1_only), t1_side);

        let sb = read_journal_super(&f);
        assert_eq!(sb.s_start.get(), 0);
        assert_eq!(sb.s_sequence.get(), 3); // end_tid = 3
        assert_eq!(f.journal.state_read().next_tid, Tid::new(3));
    }

    /// Test 4 (the crash-consistency crux): an interrupted tail transaction —
    /// committed descriptor + metadata but no valid commit block — is NOT
    /// replayed. SCAN stops at that boundary; only the fully committed prefix
    /// replays, and the clean superblock's `s_sequence` reflects "T1 committed, T2
    /// did not".
    #[ktest]
    fn recover_stops_at_interrupted_tail() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(24, 1, 1);
        let device = f.fixture.ext4.block_device();

        let dest1 = 500u64;
        let dest2 = 800u64;
        let content1 = tagged_block(b"T1COMMIT");
        let content2 = tagged_block(b"T2INTERR");

        // T1 (tid 1) commits fully: log [1..=3] = desc, 1 data, commit.
        let t1 = make_txn(Tid::new(1), &[(dest1, content1)]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t1).unwrap();

        // T2 (tid 2) commits for real right after T1: log [4..=6] = desc, 1 data,
        // commit. This lays down a correct descriptor + metadata + commit that we
        // then damage, so the descriptor/tag layout is exactly what the reader
        // expects (mirroring the writer) — the cleanest way to build an interrupted
        // tail is to write a real one and zero only its commit block.
        let t2 = make_txn(Tid::new(2), &[(dest2, content2)]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t2).unwrap();

        // Sanity: T2's descriptor is at log 4, its metadata at 5, its commit at 6.
        assert_eq!(
            RawJournalHeader::parse(&read_log_block_at(&f, 4))
                .h_blocktype
                .get(),
            BLOCKTYPE_DESCRIPTOR
        );
        assert_eq!(
            RawJournalHeader::parse(&read_log_block_at(&f, 4))
                .h_sequence
                .get(),
            2
        );
        assert_eq!(
            RawJournalHeader::parse(&read_log_block_at(&f, 6))
                .h_blocktype
                .get(),
            BLOCKTYPE_COMMIT
        );

        // "Interrupt" T2's commit: zero its commit block. Now T2 has a valid
        // descriptor + metadata but no valid commit — the crash-torn tail.
        write_log_block_at(&f, 6, &[0u8; BLOCK_SIZE]);

        // Point the on-disk s_start at the tail (T1) so recovery scans from there.
        // (commit left it at T1's start already; assert then keep it explicit.)
        assert_eq!(
            read_journal_super(&f).s_start.get(),
            f.journal.geometry().first()
        );

        recover(f.journal.as_ref(), device.as_ref()).unwrap();

        // ONLY T1 replayed: dest1 has T1's content, dest2 is untouched (still the
        // fixture's zeroed disk) — T2 was past the SCAN boundary.
        assert_eq!(read_final_block(&f, dest1), content1);
        assert_eq!(read_final_block(&f, dest2), [0u8; BLOCK_SIZE]);

        // The clean superblock records end_tid = 2: T1 committed (tid 1), T2 did
        // not, so the next expected tid is 2.
        let sb = read_journal_super(&f);
        assert_eq!(sb.s_start.get(), 0);
        assert_eq!(sb.s_sequence.get(), 2);
        assert_eq!(f.journal.state_read().next_tid, Tid::new(2));
    }

    /// A direct `scan_transaction` boundary check: a fully committed T1 scans to
    /// the next start, but the block after it (never a descriptor for tid 2) makes
    /// the following scan return `None`.
    #[ktest]
    fn scan_transaction_finds_boundary() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(24, 1, 1);
        let device = f.fixture.ext4.block_device();

        let t1 = make_txn(Tid::new(1), &[(500u64, tagged_block(b"SCANONE0"))]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t1).unwrap();

        let first = f.journal.geometry().first();
        // T1 at the tail scans to the next start (log 4, right after its commit at
        // 3).
        let next = scan_transaction(f.journal.as_ref(), device.as_ref(), first, Tid::new(1))
            .unwrap()
            .expect("T1 is a complete committed transaction");
        assert_eq!(next, 4);

        // The block after T1 was never written for tid 2, so scanning it as tid 2
        // hits the boundary.
        assert!(
            scan_transaction(f.journal.as_ref(), device.as_ref(), next, Tid::new(2))
                .unwrap()
                .is_none()
        );
    }

    /// Test 5: recovery on an already-clean journal (`s_start == 0`) is a no-op —
    /// it returns Ok and changes nothing on disk.
    #[ktest]
    fn recover_clean_journal_is_noop() {
        crate::time::clocks::init_for_ktest();
        // A fresh fixture's journal has s_start == 0 (see `journaled_fixture`).
        let f = journaled_fixture(16, 1, 1);
        let device = f.fixture.ext4.block_device();

        let before_sb = read_journal_super(&f);
        assert_eq!(before_sb.s_start.get(), 0);
        let sentinel = 500u64;
        let mark = tagged_block(b"UNTOUCHD");
        write_final_block(&f, sentinel, &mark);

        recover(f.journal.as_ref(), device.as_ref()).unwrap();

        // Nothing changed: the superblock is byte-identical and the sentinel block
        // was not overwritten.
        let after_sb = read_journal_super(&f);
        assert_eq!(after_sb.as_bytes(), before_sb.as_bytes());
        assert_eq!(read_final_block(&f, sentinel), mark);
    }

    /// A hand-built interrupted tail using a raw commit block corrupted in place,
    /// exercising the `scan_transaction` commit-mismatch path with a
    /// wrong-`h_sequence` commit rather than a zeroed one.
    #[ktest]
    fn scan_transaction_rejects_wrong_sequence_commit() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(24, 1, 1);
        let device = f.fixture.ext4.block_device();

        // A real single-block transaction: desc @1, data @2, commit @3.
        let t1 = make_txn(Tid::new(1), &[(500u64, tagged_block(b"WRONGSEQ"))]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t1).unwrap();

        // Rewrite the commit block with a mismatched h_sequence (2 instead of 1):
        // a valid COMMIT header for the wrong transaction is still an interrupted
        // commit for tid 1 — scan must treat it as the boundary.
        let mut commit = read_log_block_at(&f, 3);
        let bad = RawCommitBlock {
            header: RawJournalHeader {
                h_magic: Be32::new(JBD2_MAGIC),
                h_blocktype: Be32::new(BLOCKTYPE_COMMIT),
                h_sequence: Be32::new(2),
            },
            ..Default::default()
        };
        commit[..size_of::<RawCommitBlock>()].copy_from_slice(bad.as_bytes());
        write_log_block_at(&f, 3, &commit);

        let first = f.journal.geometry().first();
        assert!(
            scan_transaction(f.journal.as_ref(), device.as_ref(), first, Tid::new(1))
                .unwrap()
                .is_none()
        );
    }

    /// A second descriptor where the commit block should be (a multi-descriptor
    /// transaction, which a Linux-written log can produce but Phase 4 cannot
    /// parse) must fail LOUDLY, not be silently under-replayed as a torn tail
    /// (adversarial-review Finding 5).
    #[ktest]
    fn scan_transaction_rejects_multi_descriptor() {
        crate::time::clocks::init_for_ktest();
        let f = journaled_fixture(24, 1, 1);
        let device = f.fixture.ext4.block_device();

        // A real single-block transaction: desc @1, data @2, commit @3.
        let t1 = make_txn(Tid::new(1), &[(500u64, tagged_block(b"MULTIDSC"))]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t1).unwrap();

        // Overwrite the commit block (log 3) with a *descriptor* header for the
        // same tid: this is what the block after the first descriptor's LAST_TAG
        // would look like in a multi-descriptor Linux transaction. Scan must
        // return an error, not `None`.
        let mut block = read_log_block_at(&f, 3);
        let desc_header = RawJournalHeader {
            h_magic: Be32::new(JBD2_MAGIC),
            h_blocktype: Be32::new(BLOCKTYPE_DESCRIPTOR),
            h_sequence: Be32::new(1),
        };
        block[..size_of::<RawJournalHeader>()].copy_from_slice(desc_header.as_bytes());
        write_log_block_at(&f, 3, &block);

        let first = f.journal.geometry().first();
        assert!(scan_transaction(f.journal.as_ref(), device.as_ref(), first, Tid::new(1)).is_err());
        // And `recover` propagates the error rather than under-replaying.
        assert!(recover(f.journal.as_ref(), device.as_ref()).is_err());
    }

    // --- P7a-3: checksum verification during SCAN and REPLAY, on hand-built
    // csum_v3 fixtures. The checksum FORMULAS are pinned against real
    // Linux-written journal bytes in format.rs's gate tests; these tests pin
    // the recovery POLITICS built on them. ---

    /// The journal UUID baked into the csum_v3 fixtures (any bytes work; the
    /// seed just hashes them).
    const CSUM_UUID: [u8; 16] = *b"p7a3-test-uuid!!";

    /// An on-disk csum_v3 journal superblock: feature bit, named crc32c
    /// algorithm, UUID, and a correct self-checksum.
    fn csum_journal_super(
        maxlen: u32,
        first: u32,
        sequence: u32,
        start: u32,
    ) -> RawJournalSuperblock {
        let mut raw = journal_super(maxlen, first, sequence, start);
        raw.s_feature_incompat = Be32::new(INCOMPAT_CSUM_V3);
        raw.s_checksum_type = JBD2_CRC32C_CHKSUM;
        raw.s_uuid = CSUM_UUID;
        raw.s_checksum = Be32::new(raw.checksum());
        raw
    }

    /// Builds a csum_v3 journaled fixture — deliberately NOT through
    /// `load_geometry`, whose `INCOMPAT_SUPP` admission gate still refuses
    /// csum journals at mount (the flip waits for P7a-4's write-side
    /// stamping); the geometry is assembled directly, exactly as
    /// `load_geometry` would have.
    fn csum_journaled_fixture(
        maxlen: u32,
        first: u32,
        sequence: u32,
        start: u32,
    ) -> JournaledFixture {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_has_journal()
            .build()
            .unwrap();

        let raw_journal_inode = make_multi_block_file_inode(JOURNAL_START_BLOCK, maxlen as u16);
        f.write_raw_inode(JOURNAL_INO, &raw_journal_inode);
        let raw = csum_journal_super(maxlen, first, sequence, start);
        f.disk
            .segment()
            .write_val(JOURNAL_START_BLOCK as usize * BLOCK_SIZE, &raw)
            .unwrap();

        let superblock = JournalSuperblock::try_from(raw).unwrap();
        let block_map = (0..maxlen)
            .map(|log| Ext4Bid::from(JOURNAL_START_BLOCK + log))
            .collect();
        let geometry = JournalGeometry {
            block_map,
            superblock,
        };
        let journal = Journal::new(geometry, f.ext4.block_device().clone());
        JournaledFixture {
            journal,
            fixture: f,
        }
    }

    /// Hand-writes a fully checksum-stamped csum_v3 transaction (descriptor +
    /// one log block per tag + commit) at `start_log` with `tid` — a
    /// test-side stand-in for P7a-4's write-side stamping, so the read side
    /// has a valid log to verify. Tag placement goes through the shared
    /// `TagWriter`; only the checksum stamps (which the production writer
    /// does not produce yet) are poked in at the tag3 offsets its flags
    /// imply (first tag at 12 followed by the 16-byte UUID, then 16-byte
    /// strides).
    fn write_stamped_csum_v3_txn(
        f: &JournaledFixture,
        start_log: u32,
        tid: Tid,
        blocks: &[(Ext4Bid, [u8; BLOCK_SIZE])],
    ) {
        let seed = f.journal.geometry().csum_seed().unwrap();
        let layout = f.journal.geometry().tag_layout();

        let mut descriptor = Box::new([0u8; BLOCK_SIZE]);
        let header = RawJournalHeader {
            h_magic: Be32::new(JBD2_MAGIC),
            h_blocktype: Be32::new(BLOCKTYPE_DESCRIPTOR),
            h_sequence: Be32::new(tid.get()),
        };
        descriptor[..size_of::<RawJournalHeader>()].copy_from_slice(header.as_bytes());
        let mut writer = layout.writer(&mut descriptor);
        for (i, (bid, _)) in blocks.iter().enumerate() {
            let mut flags = if i == 0 { 0 } else { TAG_FLAG_SAME_UUID };
            if i == blocks.len() - 1 {
                flags |= TAG_FLAG_LAST_TAG;
            }
            writer.put(*bid, flags).unwrap();
        }
        let mut offset = 12;
        for (i, (_, content)) in blocks.iter().enumerate() {
            let csum = seed.data_block_csum(tid, content);
            // tag3's `t_checksum` is its trailing word, at tag offset + 12.
            descriptor[offset + 12..offset + 16].copy_from_slice(Be32::new(csum).as_bytes());
            offset += 16 + if i == 0 { 16 } else { 0 };
        }
        // The tail checksum covers the stamped tags, so it goes last.
        let tail = seed.block_tail_csum(&descriptor);
        descriptor[BLOCK_SIZE - 4..].copy_from_slice(Be32::new(tail).as_bytes());
        write_log_block_at(f, start_log, &descriptor);

        for (i, (_, content)) in blocks.iter().enumerate() {
            write_log_block_at(f, start_log + 1 + i as u32, content);
        }

        let mut commit = [0u8; BLOCK_SIZE];
        let commit_header = RawJournalHeader {
            h_magic: Be32::new(JBD2_MAGIC),
            h_blocktype: Be32::new(BLOCKTYPE_COMMIT),
            h_sequence: Be32::new(tid.get()),
        };
        commit[..size_of::<RawJournalHeader>()].copy_from_slice(commit_header.as_bytes());
        // `h_chksum[0]` (bytes [16, 20)) is stamped over the block with that
        // word still zero — exactly the region the verifier re-zeroes.
        let csum = seed.commit_block_csum(&commit);
        commit[16..20].copy_from_slice(Be32::new(csum).as_bytes());
        write_log_block_at(f, start_log + 1 + blocks.len() as u32, &commit);
    }

    /// SCAN on a csum_v3 journal: an intact stamped transaction seals; a
    /// commit block whose header matches but whose checksum does not REFUSES
    /// (`EUCLEAN` — jbd2's sync-commit `j_failed_commit` path, never a silent
    /// boundary); a zeroed commit slot stays the ordinary torn-tail boundary.
    #[ktest]
    fn scan_seals_only_with_valid_commit_csum() {
        crate::time::clocks::init_for_ktest();
        let f = csum_journaled_fixture(24, 1, 7, 0);
        let device = f.fixture.ext4.block_device();

        write_stamped_csum_v3_txn(&f, 1, Tid::new(7), &[(500u64, tagged_block(b"CSUMSCAN"))]);

        // Intact: desc @1, data @2, commit @3; the next start is 4.
        let next = scan_transaction(f.journal.as_ref(), device.as_ref(), 1, Tid::new(7))
            .unwrap()
            .expect("stamped transaction is sealed");
        assert_eq!(next, 4);

        // Flip a byte in the commit block BODY (header untouched).
        let mut commit = read_log_block_at(&f, 3);
        commit[100] ^= 0x01;
        write_log_block_at(&f, 3, &commit);
        let err =
            scan_transaction(f.journal.as_ref(), device.as_ref(), 1, Tid::new(7)).unwrap_err();
        assert_eq!(err.error(), Errno::EUCLEAN);

        // A zeroed commit slot: boundary, checksum never in play.
        write_log_block_at(&f, 3, &[0u8; BLOCK_SIZE]);
        assert!(
            scan_transaction(f.journal.as_ref(), device.as_ref(), 1, Tid::new(7))
                .unwrap()
                .is_none()
        );
    }

    /// SCAN defers a bad descriptor-tail checksum to the seal decision
    /// (jbd2's `need_check_commit_time` deferral): sealed by a valid commit →
    /// real corruption, `EUCLEAN`; with the commit torn away → the ordinary
    /// boundary (the tear hit the descriptor itself).
    #[ktest]
    fn scan_defers_descriptor_csum_verdict_to_the_seal() {
        crate::time::clocks::init_for_ktest();
        let f = csum_journaled_fixture(24, 1, 7, 0);
        let device = f.fixture.ext4.block_device();

        write_stamped_csum_v3_txn(&f, 1, Tid::new(7), &[(500u64, tagged_block(b"DESCDEFR"))]);

        // Corrupt the descriptor's stored tail checksum.
        let mut descriptor = read_log_block_at(&f, 1);
        descriptor[BLOCK_SIZE - 1] ^= 0xFF;
        write_log_block_at(&f, 1, &descriptor);

        let err =
            scan_transaction(f.journal.as_ref(), device.as_ref(), 1, Tid::new(7)).unwrap_err();
        assert_eq!(err.error(), Errno::EUCLEAN);

        // Tear the commit away: same descriptor, now just the boundary.
        write_log_block_at(&f, 3, &[0u8; BLOCK_SIZE]);
        assert!(
            scan_transaction(f.journal.as_ref(), device.as_ref(), 1, Tid::new(7))
                .unwrap()
                .is_none()
        );
    }

    /// REPLAY rejects a descriptor whose tail checksum mismatches, before any
    /// tag is applied (jbd2 hard-errors with `-EFSBADCRC` in any pass but
    /// SCAN).
    #[ktest]
    fn apply_rejects_descriptor_csum_mismatch() {
        crate::time::clocks::init_for_ktest();
        let f = csum_journaled_fixture(24, 1, 7, 0);
        let device = f.fixture.ext4.block_device();

        let dest = 500u64;
        write_stamped_csum_v3_txn(&f, 1, Tid::new(7), &[(dest, tagged_block(b"RPLYDESC"))]);

        let mut descriptor = read_log_block_at(&f, 1);
        descriptor[BLOCK_SIZE - 2] ^= 0x20;
        write_log_block_at(&f, 1, &descriptor);

        let err =
            apply_log_transaction(f.journal.as_ref(), device.as_ref(), 1, Tid::new(7)).unwrap_err();
        assert_eq!(err.error(), Errno::EUCLEAN);
        // Nothing was applied: the tail check precedes every tag write.
        assert_eq!(read_final_block(&f, dest), [0u8; BLOCK_SIZE]);
    }

    /// REPLAY on a tag whose data block fails its checksum mirrors jbd2's
    /// skip-and-record shape: the corrupt block is never applied, the intact
    /// sibling still lands, and the apply as a whole fails (`EUCLEAN` — the
    /// mount is refused, exactly as Linux's recovery error refuses it).
    #[ktest]
    fn apply_skips_csum_bad_block_and_fails() {
        crate::time::clocks::init_for_ktest();
        let f = csum_journaled_fixture(24, 1, 7, 0);
        let device = f.fixture.ext4.block_device();

        let good_dest = 500u64;
        let bad_dest = 800u64;
        let good = tagged_block(b"TAGGOOD0");
        let bad = tagged_block(b"TAGBAD00");
        write_stamped_csum_v3_txn(&f, 1, Tid::new(7), &[(good_dest, good), (bad_dest, bad)]);

        // Corrupt the SECOND tag's logged data block (log 3).
        let mut logged = read_log_block_at(&f, 3);
        logged[123] ^= 0x08;
        write_log_block_at(&f, 3, &logged);

        let err =
            apply_log_transaction(f.journal.as_ref(), device.as_ref(), 1, Tid::new(7)).unwrap_err();
        assert_eq!(err.error(), Errno::EUCLEAN);
        assert_eq!(read_final_block(&f, good_dest), good);
        assert_eq!(read_final_block(&f, bad_dest), [0u8; BLOCK_SIZE]);
    }

    /// End to end: `recover` replays a fully stamped csum_v3 log and marks
    /// the journal clean. (The clean-superblock rewrite does not restamp
    /// `s_checksum` yet — that is P7a-4's write side; admission keeps csum
    /// journals off real mounts until it lands.)
    #[ktest]
    fn recover_replays_stamped_csum_v3_log() {
        crate::time::clocks::init_for_ktest();
        // Dirty on-disk superblock: s_start = 1, s_sequence = 7.
        let f = csum_journaled_fixture(24, 1, 7, 1);
        let device = f.fixture.ext4.block_device();

        let dest = 600u64;
        let content = tagged_block(b"CSUMRCVR");
        write_stamped_csum_v3_txn(&f, 1, Tid::new(7), &[(dest, content)]);

        recover(f.journal.as_ref(), device.as_ref()).unwrap();

        assert_eq!(read_final_block(&f, dest), content);
        let sb = read_journal_super(&f);
        assert_eq!(sb.s_start.get(), 0);
        assert_eq!(sb.s_sequence.get(), 8);
    }

    /// End to end: a corrupt journaled data block refuses recovery (and thus
    /// the mount) — SCAN still seals the transaction (it does not read data
    /// blocks, matching Linux), REPLAY's tag verification catches it, and
    /// the journal is left dirty, never silently under-replayed.
    #[ktest]
    fn recover_refuses_corrupt_csum_log() {
        crate::time::clocks::init_for_ktest();
        let f = csum_journaled_fixture(24, 1, 7, 1);
        let device = f.fixture.ext4.block_device();

        let dest = 600u64;
        write_stamped_csum_v3_txn(&f, 1, Tid::new(7), &[(dest, tagged_block(b"CSUMBAD0"))]);
        let mut logged = read_log_block_at(&f, 2);
        logged[321] ^= 0x40;
        write_log_block_at(&f, 2, &logged);

        let err = recover(f.journal.as_ref(), device.as_ref()).unwrap_err();
        assert_eq!(err.error(), Errno::EUCLEAN);
        assert_eq!(read_final_block(&f, dest), [0u8; BLOCK_SIZE]);
        // The journal stays dirty: recovery never reached the clean write.
        assert_eq!(read_journal_super(&f).s_start.get(), 1);
    }
}
