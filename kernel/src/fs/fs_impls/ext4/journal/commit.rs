// SPDX-License-Identifier: MPL-2.0

//! The metadata commit pipeline (jbd2 `jbd2_journal_commit_transaction`).
//!
//! [`commit_transaction`] writes one running [`Transaction`]'s captured
//! metadata after-images to the on-disk log as a single jbd2 transaction, with
//! the crash-safe write ordering that makes the transaction atomic, and updates
//! the on-disk journal superblock so recovery can find it.
//!
//! # Log layout produced
//!
//! A committed transaction occupies consecutive log blocks starting at the
//! current log head, wrapping within `[first, maxlen)`:
//!
//! ```text
//! [descriptor] [metadata 0] [metadata 1] ... [metadata N-1] [commit]
//! ```
//!
//! - **Descriptor** (`JBD2_DESCRIPTOR_BLOCK`): a 12-byte [`RawJournalHeader`]
//!   then one 8-byte [`RawBlockTag`] per captured block, in block-number order.
//!   The first tag is followed by a 16-byte journal UUID (written as zeros — we
//!   carry no journal UUID or checksum in Phase 4; recovery just skips it) and
//!   does *not* set `TAG_FLAG_SAME_UUID`; every later tag sets it (no UUID
//!   follows). The last tag also sets `TAG_FLAG_LAST_TAG`.
//! - **Metadata blocks**: the N captured after-images, in the same order as
//!   their tags, one full [`BLOCK_SIZE`] block each.
//! - **Commit** (`JBD2_COMMIT_BLOCK`): a [`RawCommitBlock`] sealing the
//!   transaction, carrying the wall-clock commit time.
//!
//! ## Escaping
//!
//! If a metadata block's first four bytes happen to equal [`JBD2_MAGIC`] on
//! disk, a naive recovery scan would mistake it for a log header. jbd2 avoids
//! this by *escaping*: the block's tag gets `TAG_FLAG_ESCAPE`, and the block is
//! written into the log with its first four bytes zeroed. Recovery restores the
//! magic when it applies the block. We mirror this exactly.
//!
//! # Crash-safe write ordering (the crux)
//!
//! The order below is what makes the transaction atomic across a crash. Each
//! barrier ([`BlockDevice::sync`], a Flush that waits) sits where it does for a
//! specific reason:
//!
//! 0. **Ordered data first (jbd2 `data=ordered`).** Flush every ordered inode's
//!    dirty **data** to its final on-disk location, then **barrier**. A file's
//!    data must be durable *before* the metadata that references it is committed
//!    to the log; otherwise recovery could replay an inode whose size/extents now
//!    cover a block whose data never reached the platter, exposing stale/garbage
//!    bytes or leaking. This is a *separate* barrier from step 2 on purpose: it
//!    makes "data durable before metadata" hold regardless of `flush_range`'s
//!    submit-vs-complete timing, matching jbd2's explicit wait-for-data step
//!    before the journal write. Skipped (no barrier) when the transaction has no
//!    ordered inodes. (Merging this barrier with step 2 is a valid Phase-7 perf
//!    optimization once `flush_dirty_pages` completion semantics are pinned down.)
//! 1. Write the descriptor + all N metadata blocks to the log.
//! 2. **Barrier.** The descriptor and data must be durable *before* the commit
//!    block; otherwise a crash could leave a commit record pointing at data that
//!    never reached the platter, and recovery would replay garbage.
//! 3. Write the commit block.
//! 4. **Barrier.** Once the commit block is durable the transaction is
//!    committed: recovery will now see a complete, checksum-free (Phase 4) log
//!    record and replay it.
//! 5. If the journal was **clean** before this commit (`s_start == 0`), update
//!    the on-disk journal superblock's `s_start`/`s_sequence` to point at this
//!    transaction, then barrier again. This is ordered *after* step 4
//!    deliberately: a crash between 4 and 5 leaves `s_start == 0`, so recovery
//!    skips the transaction — but its metadata lives only in the log, so the
//!    final locations still hold the pre-transaction bytes and the on-disk state
//!    is exactly the consistent pre-transaction state. This "final locations
//!    untouched" invariant holds because, under Model A, a metadata block's
//!    after-image reaches its final location only via checkpoint (a later task),
//!    never before its transaction commits; a live `dirty_metadata` must never
//!    write through to the final block. If the journal was already dirty,
//!    `s_start` already points at an older un-checkpointed transaction and must
//!    not be overwritten.
//! 6. Update the in-memory journal state under the state lock: advance `head`
//!    past the N+2 written blocks, set the tail to this transaction if the
//!    journal was clean, and publish `committed_tid`.
//!
//! # Phase 4 simplifications
//!
//! - **Single descriptor** per transaction: all N tags must fit one descriptor
//!   block ([`JournalGeometry::tags_per_descriptor`]); a larger transaction is
//!   rejected. [`Journal::max_credits`] enforces the same bound up front.
//! - **No checksums** (the commit block's csum fields are zero) — Phase 6/7.
//! - **No revoke records** — Phase 7.
//! - **Synchronous, no commit thread**: commit is driven inline by the caller
//!   (here, tests); the background commit thread is a later task.

// The whole commit pipeline is reachable only through the test-only `Journal`
// until a later task wires it into `journal_stop`/`fsync`; in non-ktest builds
// `commit_transaction` and its helpers form a closed, unreferenced cluster. One
// module-level attribute absorbs all of it (mirroring `transaction.rs`), rather
// than a marker on every helper. `allow` (not `expect`): once sibling modules
// (`checkpoint`, later `recovery`) reference this module's `pub(super)` helpers,
// a module-level `expect(dead_code)` flips to "unfulfilled"; `allow` is stable
// under that churn. The integration task drops this attribute wholesale once the
// pipeline is wired into a live path.
#![cfg_attr(not(ktest), allow(dead_code))]

use super::{
    super::prelude::*,
    format::{
        Be16, Be32, Be64, RawBlockTag, RawCommitBlock, RawJournalHeader, BLOCKTYPE_COMMIT,
        BLOCKTYPE_DESCRIPTOR, JBD2_MAGIC, TAG_FLAG_ESCAPE, TAG_FLAG_LAST_TAG, TAG_FLAG_SAME_UUID,
    },
    transaction::TransactionState,
    Journal, Tid, Transaction,
};

/// The four bytes a metadata block must start with to require escaping: the
/// big-endian on-disk encoding of [`JBD2_MAGIC`].
const JBD2_MAGIC_BYTES: [u8; 4] = JBD2_MAGIC.to_be_bytes();

/// The next log block after `cur`, wrapping to `first` at the end of the log.
///
/// The usable log is the ring `[first, maxlen)`; block 0 holds the journal
/// superblock and is never a log-data block, so wrapping returns to `first`.
///
/// Shared with [`checkpoint`](super::checkpoint): the checkpoint's
/// log-transaction reader walks the same ring as this writer, so both use one
/// definition of the wrap rule to stay byte-for-byte consistent.
pub(super) fn next_log_block(cur: u32, first: u32, maxlen: u32) -> u32 {
    let next = cur + 1;
    if next >= maxlen {
        first
    } else {
        next
    }
}

/// Writes a full [`BLOCK_SIZE`] log block: resolves log block `log` to its
/// physical device block and writes `block_buf` there.
fn write_log_block(
    journal: &Journal,
    device: &dyn BlockDevice,
    log: u32,
    block_buf: &[u8; BLOCK_SIZE],
) -> Result<()> {
    let pblock = journal
        .geometry
        .log_block_to_physical(log)
        .ok_or_else(|| Error::with_message(Errno::EUCLEAN, "log block out of range"))?;
    device
        .write_bytes(Bid::new(pblock).to_offset(), block_buf.as_slice())
        .map_err(|_| Error::with_message(Errno::EIO, "failed to write journal log block"))
}

/// Issues a durability barrier (jbd2's `blkdev_issue_flush` after a phase): a
/// Flush that waits for all prior writes to reach the platter.
///
/// Shared with [`checkpoint`](super::checkpoint), which needs the identical
/// "make prior writes durable" semantics between its final-location writes and
/// clearing `s_start`.
pub(super) fn barrier(device: &dyn BlockDevice) -> Result<()> {
    match device
        .sync()
        .map_err(|_| Error::with_message(Errno::EIO, "failed to enqueue journal flush"))?
    {
        BioStatus::Complete => Ok(()),
        _ => return_errno_with_message!(Errno::EIO, "journal flush did not complete"),
    }
}

/// Builds the descriptor block for `txn` into a fresh [`BLOCK_SIZE`] buffer.
///
/// Lays down the 12-byte header then one 8-byte tag per captured block (in
/// block order), with the first tag's 16-byte UUID region zeroed. Returns the
/// buffer plus, for each captured block in the same order, whether that block
/// must be escaped when written into the log (`escape[i] == true`).
fn build_descriptor_block(txn: &Transaction) -> Result<(Box<[u8; BLOCK_SIZE]>, Vec<bool>)> {
    let n = txn.metadata_blocks().count();
    if n == 0 {
        return_errno_with_message!(Errno::EINVAL, "cannot commit an empty transaction");
    }

    let mut block = Box::new([0u8; BLOCK_SIZE]);

    // Header: magic + descriptor block type + this transaction's tid.
    let header = RawJournalHeader {
        h_magic: Be32::new(JBD2_MAGIC),
        h_blocktype: Be32::new(BLOCKTYPE_DESCRIPTOR),
        h_sequence: Be32::new(txn.tid()),
    };
    let header_len = size_of::<RawJournalHeader>();
    block[..header_len].copy_from_slice(header.as_bytes());

    // The tag array starts right after the header. Each tag is 8 bytes; the
    // first tag is additionally followed by a 16-byte UUID (left zeroed).
    let tag_len = size_of::<RawBlockTag>();
    let uuid_len = 16;
    let mut offset = header_len;
    let mut escape = Vec::with_capacity(n);

    for (i, (bid, bytes)) in txn.metadata_blocks().enumerate() {
        let is_first = i == 0;
        let is_last = i == n - 1;

        let mut flags = 0u16;
        if !is_first {
            // Only the first tag carries a UUID; every later tag reuses it.
            flags |= TAG_FLAG_SAME_UUID;
        }
        if is_last {
            flags |= TAG_FLAG_LAST_TAG;
        }
        // Escape a block whose on-disk head would look like a jbd2 header.
        let needs_escape = bytes[..4] == JBD2_MAGIC_BYTES;
        if needs_escape {
            flags |= TAG_FLAG_ESCAPE;
        }
        escape.push(needs_escape);

        let tag = RawBlockTag {
            // Only the low 32 bits: 64-bit tags (INCOMPAT_64BIT) are rejected.
            t_blocknr: Be32::new(bid as u32),
            t_checksum: Be16::new(0),
            t_flags: Be16::new(flags),
        };

        // The tag region (tags + the first tag's UUID) must fit the block. This
        // is the single-descriptor bound; `max_credits` refuses over-large
        // transactions up front, but re-check here so a directly built
        // transaction cannot overflow the descriptor.
        let tag_end = offset + tag_len + if is_first { uuid_len } else { 0 };
        if tag_end > BLOCK_SIZE {
            return_errno_with_message!(
                Errno::ENOSPC,
                "transaction needs more than one descriptor block (unsupported in Phase 4)"
            );
        }

        block[offset..offset + tag_len].copy_from_slice(tag.as_bytes());
        offset = tag_end;
    }

    Ok((block, escape))
}

/// Builds the commit block into a fresh [`BLOCK_SIZE`] buffer: header +
/// wall-clock commit time, all checksum fields zero (Phase 4).
fn build_commit_block(tid: Tid) -> Box<[u8; BLOCK_SIZE]> {
    let now = super::super::utils::now();
    let commit = RawCommitBlock {
        header: RawJournalHeader {
            h_magic: Be32::new(JBD2_MAGIC),
            h_blocktype: Be32::new(BLOCKTYPE_COMMIT),
            h_sequence: Be32::new(tid),
        },
        h_chksum_type: 0,
        h_chksum_size: 0,
        h_padding: [0; 2],
        h_chksum: [Be32::new(0); 8],
        h_commit_sec: Be64::new(now.as_secs()),
        h_commit_nsec: Be32::new(now.subsec_nanos()),
    };

    let mut block = Box::new([0u8; BLOCK_SIZE]);
    let commit_len = size_of::<RawCommitBlock>();
    block[..commit_len].copy_from_slice(commit.as_bytes());
    block
}

/// Points the on-disk journal superblock at `txn_start`/`tid` as the oldest
/// un-checkpointed transaction (jbd2 updates `s_start`/`s_sequence` when the
/// journal transitions from clean to dirty), then barriers.
///
/// Called only when the journal was clean before this commit; a dirty journal
/// already records an older transaction that must be preserved.
///
/// Does NOT touch `s_head`: Linux only reads `s_head` on the clean-unmount fast
/// path (`recovery.c`, when `s_start == 0`), which Phase 4 never produces — our
/// commits always leave `s_start != 0` until a checkpoint clears it. **Task 6
/// (checkpoint / clean unmount) owns `s_head`: when it zeroes `s_start` on a
/// clean unmount it MUST also write a correct `s_head`, or Linux would resume
/// the log at a stale offset.** (Adversarial-review finding, Phase 4 Task 3.)
fn update_superblock_tail(
    journal: &Journal,
    device: &dyn BlockDevice,
    txn_start: u32,
    tid: Tid,
) -> Result<()> {
    use super::format::RawJournalSuperblock;

    let sb_pblock = journal
        .geometry
        .log_block_to_physical(0)
        .ok_or_else(|| Error::with_message(Errno::EUCLEAN, "journal superblock block unmapped"))?;
    let sb_offset = Bid::new(sb_pblock).to_offset();

    let mut raw: RawJournalSuperblock = device
        .read_val(sb_offset)
        .map_err(|_| Error::with_message(Errno::EIO, "failed to read journal superblock"))?;
    raw.s_start = Be32::new(txn_start);
    raw.s_sequence = Be32::new(tid);
    device
        .write_val(sb_offset, &raw)
        .map_err(|_| Error::with_message(Errno::EIO, "failed to write journal superblock"))?;

    // The recoverability pointer must itself be durable.
    barrier(device)
}

/// Advances the in-memory log head past `count` written log blocks, wrapping
/// within `[first, maxlen)`.
fn advance_head(head: u32, count: u32, first: u32, maxlen: u32) -> u32 {
    let mut h = head;
    for _ in 0..count {
        h = next_log_block(h, first, maxlen);
    }
    h
}

/// Commits `txn` to the on-disk log as a jbd2 transaction and makes it
/// recoverable. Consumes the transaction. Returns its tid.
///
/// Implements the log layout and crash-safe write ordering documented at the
/// module level: ordered data → barrier → log (descriptor + metadata) → barrier
/// → commit → barrier → (superblock → barrier if the journal was clean) →
/// in-memory state.
pub(super) fn commit_transaction(
    journal: &Journal,
    device: &dyn BlockDevice,
    mut txn: Transaction,
) -> Result<Tid> {
    let tid = txn.tid();
    let first = journal.geometry.first();
    let maxlen = journal.geometry.maxlen();

    // --- Step 0: ordered-data mode. Every ordered inode's dirty data must reach
    // its final location and be durable BEFORE any log block (and thus the commit
    // record) is written, so recovery never replays metadata (an inode whose
    // size/extents now cover a block) that references data which never hit the
    // platter — which would expose stale/garbage bytes or leak. This mirrors
    // jbd2's `data=ordered` step, which flushes and waits on the transaction's
    // ordered inodes (`journal_submit_inode_data_buffers` /
    // `journal_finish_inode_data_buffers`) before the commit phase.
    //
    // This is a SEPARATE barrier, not merged with the pre-commit metadata barrier
    // in step 2: `flush_range`'s exact submit-vs-complete timing is not something
    // we depend on here — an explicit barrier right after the data flush makes
    // "data durable before metadata" unconditionally correct regardless of when
    // `flush_dirty_pages` completes. (Merging it with the step-2 metadata barrier
    // is a valid Phase-7 perf optimization once `flush_dirty_pages` completion
    // semantics are pinned down.) The flush takes `inode.inner.read()` and holds
    // no journal state lock — a leaf in the global order.
    let mut flushed_any = false;
    for inode in txn.ordered_inodes() {
        inode.flush_ordered_data()?;
        flushed_any = true;
    }
    if flushed_any {
        barrier(device)?;
    }

    // The head to write at, and whether the journal is clean, are read under the
    // state lock; the writes themselves happen without holding it (Phase 4 is
    // single-transaction, so no other committer races us) and the state is
    // updated again at the end.
    let (start_head, was_clean) = {
        let st = journal.state_write();
        (st.head, st.tail_block == 0)
    };

    let (descriptor, escape) = build_descriptor_block(&txn)?;
    let n = escape.len() as u32;

    // --- Step 1: write the descriptor and every metadata after-image. ---
    let mut log = start_head;
    write_log_block(journal, device, log, &descriptor)?;

    for ((_, bytes), needs_escape) in txn.metadata_blocks().zip(escape.iter()) {
        log = next_log_block(log, first, maxlen);
        let mut buf = Box::new([0u8; BLOCK_SIZE]);
        buf.copy_from_slice(bytes);
        if *needs_escape {
            // Zero the head so recovery does not mistake it for a log header;
            // recovery restores the magic when applying the block.
            buf[..4].copy_from_slice(&[0u8; 4]);
        }
        write_log_block(journal, device, log, &buf)?;
    }

    // --- Step 2: barrier. Descriptor + data durable BEFORE the commit block,
    // so a commit record never certifies data that never reached the platter.
    barrier(device)?;

    // --- Step 3: write the commit block, sealing the transaction. ---
    let commit_log = next_log_block(log, first, maxlen);
    let commit = build_commit_block(tid);
    write_log_block(journal, device, commit_log, &commit)?;

    // --- Step 4: barrier. The commit block is now durable: the transaction is
    // committed and recovery will replay it.
    barrier(device)?;

    // --- Step 5: make it recoverable. Only if the journal was clean; otherwise
    // an older un-checkpointed transaction already owns `s_start`. Ordered after
    // step 4 on purpose: a crash between 4 and 5 leaves `s_start == 0`, so
    // recovery skips this transaction — and since checkpoint has not run, the
    // final locations still hold the pre-transaction bytes, i.e. a consistent
    // pre-transaction state.
    if was_clean {
        update_superblock_tail(journal, device, start_head, tid)?;
    }

    // --- Step 6: publish the new log position and commit id in memory. ---
    // Blocks written: 1 descriptor + N metadata + 1 commit = N + 2.
    let new_head = advance_head(start_head, n + 2, first, maxlen);
    {
        let mut st = journal.state_write();
        st.head = new_head;
        if was_clean {
            st.tail_block = start_head;
            st.tail_tid = tid;
        }
    }
    // Release `Acquire` in `committed_tid()`: publishes after the state update.
    journal
        .committed_tid
        .store(tid, core::sync::atomic::Ordering::Release);

    // The transaction is fully committed; consume it.
    txn.set_state(TransactionState::Finished);
    drop(txn);

    Ok(tid)
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::{
        // `*` re-exports the parent module's imports (`Journal`, `Transaction`,
        // `Be32`, `RawBlockTag`, `RawCommitBlock`, `RawJournalHeader`,
        // `BLOCKTYPE_COMMIT`, `BLOCKTYPE_DESCRIPTOR`, `JBD2_MAGIC`, the tag
        // flags). Only items the parent does not import are named explicitly.
        super::{
            super::test_utils::{make_multi_block_file_inode, Ext4FixtureBuilder},
            format::{RawJournalSuperblock, BLOCKTYPE_SUPERBLOCK_V2},
            load_geometry, JOURNAL_INO,
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

    /// A journaled fixture: the in-memory `Journal` plus the fixture that owns
    /// the disk (so tests can read the log straight off the device).
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

    /// Reads a whole log block off the device by its log index.
    fn read_log_block(f: &JournaledFixture, log: u32) -> [u8; BLOCK_SIZE] {
        let pblock = (JOURNAL_START_BLOCK + log) as usize * BLOCK_SIZE;
        let mut buf = [0u8; BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .read_bytes(pblock, &mut buf)
            .unwrap();
        buf
    }

    /// Reads the jbd2 header of a log block.
    fn read_log_header(f: &JournaledFixture, log: u32) -> RawJournalHeader {
        let pblock = (JOURNAL_START_BLOCK + log) as usize * BLOCK_SIZE;
        f.fixture.disk.segment().read_val(pblock).unwrap()
    }

    /// Reads tag `i` from a descriptor block at log index `log`.
    fn read_tag(f: &JournaledFixture, log: u32, i: usize) -> RawBlockTag {
        let base = (JOURNAL_START_BLOCK + log) as usize * BLOCK_SIZE;
        let header_len = size_of::<RawJournalHeader>();
        let tag_len = size_of::<RawBlockTag>();
        // Tag 0 sits right after the header; every later tag is offset by the
        // preceding tags plus the first tag's 16-byte UUID.
        let offset = if i == 0 {
            base + header_len
        } else {
            base + header_len + tag_len + 16 + (i - 1) * tag_len
        };
        f.fixture.disk.segment().read_val(offset).unwrap()
    }

    /// Builds a running transaction with `tid` capturing the given
    /// `(dest_block, content)` pairs.
    fn make_txn(tid: Tid, blocks: &[(Ext4Bid, [u8; BLOCK_SIZE])]) -> Transaction {
        let mut txn = Transaction::new(tid);
        for (bid, content) in blocks {
            txn.capture_create(*bid);
            txn.apply_patch(*bid, |b| b.copy_from_slice(content)).unwrap();
        }
        txn
    }

    #[ktest]
    fn commit_single_transaction_layout() {
        let f = journaled_fixture(16, 1, 1);
        let device = f.fixture.ext4.block_device();

        let (dest0, dest1) = (500u64, 700u64);
        let mut c0 = [0u8; BLOCK_SIZE];
        c0[..8].copy_from_slice(b"BLOCKZR0");
        let mut c1 = [0u8; BLOCK_SIZE];
        c1[..8].copy_from_slice(b"BLOCKZR1");

        let txn = make_txn(1, &[(dest0, c0), (dest1, c1)]);
        let tid = commit_transaction(f.journal.as_ref(), device.as_ref(), txn).unwrap();
        assert_eq!(tid, 1);

        // Descriptor at log block `first` (== 1).
        let desc = read_log_header(&f, 1);
        assert_eq!(desc.h_magic.get(), JBD2_MAGIC);
        assert_eq!(desc.h_blocktype.get(), BLOCKTYPE_DESCRIPTOR);
        assert_eq!(desc.h_sequence.get(), tid);

        // Tag 0 -> dest0, no SAME_UUID; tag 1 -> dest1, SAME_UUID | LAST_TAG.
        let tag0 = read_tag(&f, 1, 0);
        assert_eq!(tag0.t_blocknr.get(), dest0 as u32);
        assert_eq!(tag0.t_flags.get() & TAG_FLAG_SAME_UUID, 0);
        assert_eq!(tag0.t_flags.get() & TAG_FLAG_LAST_TAG, 0);

        let tag1 = read_tag(&f, 1, 1);
        assert_eq!(tag1.t_blocknr.get(), dest1 as u32);
        assert_ne!(tag1.t_flags.get() & TAG_FLAG_SAME_UUID, 0);
        assert_ne!(tag1.t_flags.get() & TAG_FLAG_LAST_TAG, 0);

        // The two after-images follow the descriptor, byte-equal.
        assert_eq!(read_log_block(&f, 2), c0);
        assert_eq!(read_log_block(&f, 3), c1);

        // Commit block at log block first+3.
        let commit = read_log_header(&f, 4);
        assert_eq!(commit.h_magic.get(), JBD2_MAGIC);
        assert_eq!(commit.h_blocktype.get(), BLOCKTYPE_COMMIT);
        assert_eq!(commit.h_sequence.get(), tid);

        assert_eq!(f.journal.committed_tid(), tid);

        // On-disk journal superblock now points at this transaction.
        let sb: RawJournalSuperblock = f
            .fixture
            .disk
            .segment()
            .read_val(JOURNAL_START_BLOCK as usize * BLOCK_SIZE)
            .unwrap();
        assert_eq!(sb.s_start.get(), 1); // == first
        assert_eq!(sb.s_sequence.get(), tid);
    }

    #[ktest]
    fn commit_escapes_block_starting_with_magic() {
        let f = journaled_fixture(16, 1, 1);
        let device = f.fixture.ext4.block_device();

        // A metadata block whose first four bytes ARE the jbd2 magic.
        let mut content = [0u8; BLOCK_SIZE];
        content[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
        content[4..8].copy_from_slice(b"REST");

        let txn = make_txn(1, &[(600u64, content)]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), txn).unwrap();

        // Its (sole) tag has ESCAPE set.
        let tag = read_tag(&f, 1, 0);
        assert_ne!(tag.t_flags.get() & TAG_FLAG_ESCAPE, 0);

        // The logged block has its head zeroed but the rest intact.
        let logged = read_log_block(&f, 2);
        assert_eq!(&logged[..4], &[0u8; 4]);
        assert_eq!(&logged[4..8], b"REST");
    }

    #[ktest]
    fn commit_two_sequential_transactions() {
        let f = journaled_fixture(24, 1, 1);
        let device = f.fixture.ext4.block_device();

        // T1: tid 1, two blocks -> occupies log [1..=4] (desc, 2 data, commit).
        let mut a = [0u8; BLOCK_SIZE];
        a[..4].copy_from_slice(b"T1A0");
        let mut b = [0u8; BLOCK_SIZE];
        b[..4].copy_from_slice(b"T1B0");
        let t1 = make_txn(1, &[(500u64, a), (700u64, b)]);
        let tid1 = commit_transaction(f.journal.as_ref(), device.as_ref(), t1).unwrap();
        assert_eq!(tid1, 1);

        // Head advanced past T1's 4 blocks: now at log block 5.
        {
            let st = f.journal.state_write();
            assert_eq!(st.head, 5);
            assert_eq!(st.tail_block, 1);
            assert_eq!(st.tail_tid, 1);
        }

        // T2: tid 2, one block -> occupies log [5..=7] (desc, 1 data, commit).
        let mut c = [0u8; BLOCK_SIZE];
        c[..4].copy_from_slice(b"T2C0");
        let t2 = make_txn(2, &[(900u64, c)]);
        let tid2 = commit_transaction(f.journal.as_ref(), device.as_ref(), t2).unwrap();
        assert_eq!(tid2, 2);

        // T1's descriptor and commit still carry tid 1.
        assert_eq!(read_log_header(&f, 1).h_sequence.get(), 1);
        assert_eq!(read_log_header(&f, 1).h_blocktype.get(), BLOCKTYPE_DESCRIPTOR);
        assert_eq!(read_log_header(&f, 4).h_sequence.get(), 1);
        assert_eq!(read_log_header(&f, 4).h_blocktype.get(), BLOCKTYPE_COMMIT);

        // T2's descriptor lands right after T1, at log block 5, with tid 2.
        assert_eq!(read_log_header(&f, 5).h_sequence.get(), 2);
        assert_eq!(read_log_header(&f, 5).h_blocktype.get(), BLOCKTYPE_DESCRIPTOR);
        assert_eq!(read_log_header(&f, 7).h_sequence.get(), 2);
        assert_eq!(read_log_header(&f, 7).h_blocktype.get(), BLOCKTYPE_COMMIT);

        assert_eq!(f.journal.committed_tid(), 2);
        {
            let st = f.journal.state_write();
            assert_eq!(st.head, 8);
            // s_start still points at T1 (the oldest un-checkpointed txn).
            assert_eq!(st.tail_block, 1);
            assert_eq!(st.tail_tid, 1);
        }

        // On-disk superblock unchanged since T1: still first/tid1.
        let sb: RawJournalSuperblock = f
            .fixture
            .disk
            .segment()
            .read_val(JOURNAL_START_BLOCK as usize * BLOCK_SIZE)
            .unwrap();
        assert_eq!(sb.s_start.get(), 1);
        assert_eq!(sb.s_sequence.get(), 1);
    }

    #[ktest]
    fn commit_issues_both_barriers() {
        let f = journaled_fixture(16, 1, 1);
        let device = f.fixture.ext4.block_device();

        let before = f.fixture.disk.flush_count();

        let mut content = [0u8; BLOCK_SIZE];
        content[..4].copy_from_slice(b"DATA");
        let txn = make_txn(1, &[(500u64, content)]);
        commit_transaction(f.journal.as_ref(), device.as_ref(), txn).unwrap();

        // At least the two data/commit barriers fired (a clean-journal commit
        // adds a third for the superblock update).
        let issued = f.fixture.disk.flush_count() - before;
        assert!(issued >= 2, "expected >= 2 barriers, got {issued}");
    }

    /// The money test for ordered-data mode: a file's data reaches its final
    /// on-disk location during commit (not before), because it was registered as
    /// an ordered inode of the committed transaction.
    #[ktest]
    fn commit_flushes_ordered_data_to_disk() {
        use super::super::super::test_utils::make_empty_file_inode;
        crate::time::clocks::init_for_ktest();

        let f = journaled_fixture(16, 1, 1);
        let device = f.fixture.ext4.block_device();

        // A regular file (ino 11) whose data we will dirty in the page cache.
        const FILE_INO: u32 = 11;
        f.fixture
            .write_raw_inode(FILE_INO, &make_empty_file_inode());
        let inode = f.fixture.ext4.read_inode(FILE_INO).unwrap();

        // Write known data: this dirties the page cache and allocates a data
        // block, but leaves the data in the cache (no flush yet).
        let mut data = [0u8; BLOCK_SIZE];
        data[..8].copy_from_slice(b"ORDERED!");
        data[BLOCK_SIZE - 4..].copy_from_slice(b"TAIL");
        let mut reader = VmReader::from(data.as_slice()).to_fallible();
        assert_eq!(inode.write_at(0, &mut reader).unwrap(), BLOCK_SIZE);

        // The data block's final physical location (logical block 0).
        let pblock = inode
            .data_block_of(0)
            .expect("logical block 0 must be allocated after the write");

        // BEFORE commit the final location still holds the fixture's zeroed disk:
        // the write left the data only in the page cache.
        let mut on_disk = [0u8; BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .read_bytes(pblock as usize * BLOCK_SIZE, &mut on_disk)
            .unwrap();
        assert_eq!(
            on_disk,
            [0u8; BLOCK_SIZE],
            "data must not be on disk before commit"
        );

        // Build a transaction that captures a metadata block (so the commit has
        // something to log) AND registers the inode as ordered data.
        let mut txn = Transaction::new(1);
        let mut meta = [0u8; BLOCK_SIZE];
        meta[..4].copy_from_slice(b"META");
        txn.capture_create(500);
        txn.apply_patch(500, |b| b.copy_from_slice(&meta)).unwrap();
        txn.add_ordered_inode(&inode);

        let before = f.fixture.disk.flush_count();
        commit_transaction(f.journal.as_ref(), device.as_ref(), txn).unwrap();

        // AFTER commit the ordered flush wrote the file's data to its final block.
        f.fixture
            .disk
            .segment()
            .read_bytes(pblock as usize * BLOCK_SIZE, &mut on_disk)
            .unwrap();
        assert_eq!(on_disk, data, "ordered data must be on disk after commit");

        // Barriers: data + metadata + commit = 3, plus the superblock barrier
        // (this is a clean-journal commit) = 4.
        let issued = f.fixture.disk.flush_count() - before;
        assert!(issued >= 3, "expected >= 3 barriers, got {issued}");
    }

    /// An empty ordered set issues no data barrier: only the metadata + commit
    /// barriers (2), plus the superblock barrier on a clean-journal commit (3).
    #[ktest]
    fn commit_without_ordered_inodes_skips_data_barrier() {
        let f = journaled_fixture(16, 1, 1);
        let device = f.fixture.ext4.block_device();

        let before = f.fixture.disk.flush_count();

        let mut content = [0u8; BLOCK_SIZE];
        content[..4].copy_from_slice(b"META");
        let txn = make_txn(1, &[(500u64, content)]);
        assert_eq!(txn.nr_ordered_inodes(), 0);
        commit_transaction(f.journal.as_ref(), device.as_ref(), txn).unwrap();

        // No ordered inodes => no data barrier: exactly the metadata + commit +
        // (clean-journal) superblock barriers, i.e. 3.
        let issued = f.fixture.disk.flush_count() - before;
        assert_eq!(issued, 3, "expected exactly 3 barriers (no data barrier)");
    }
}
