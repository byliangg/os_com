// SPDX-License-Identifier: MPL-2.0

//! The metadata commit pipeline (jbd2 `jbd2_journal_commit_transaction`).
//!
//! [`try_commit_transaction`] writes one running [`Transaction`]'s captured
//! metadata after-images to the on-disk log as a single jbd2 transaction, with
//! the crash-safe write ordering that makes the transaction atomic, and updates
//! the on-disk journal superblock so recovery can find it. When the chain does
//! not fit the ring's free segment it instead refuses before any write and
//! hands the transaction back ([`CommitAttempt::NeedsLogSpace`]) so the caller
//! can drain the un-checkpointed tail and retry.
//!
//! # Log layout produced
//!
//! A committed transaction occupies consecutive log blocks starting at the
//! current log head, wrapping within `[first, maxlen)`. When all N captured
//! blocks fit one descriptor's tag array and nothing was revoked, the layout
//! is:
//!
//! ```text
//! [descriptor] [metadata 0] [metadata 1] ... [metadata N-1] [commit]
//! ```
//!
//! A larger transaction splits across a **descriptor chain** (P7a-5,
//! mirroring jbd2 `journal_commit_transaction`, which starts a fresh
//! descriptor whenever the previous one fills, fs/jbd2/commit.c:606-635 /
//! 700-737):
//!
//! ```text
//! [descriptor 1] [its metadata ...] [descriptor 2] [its metadata ...] ... [commit]
//! ```
//!
//! A transaction with a nonempty revoke set (P7b-3) additionally writes its
//! **revoke blocks** at the head of the chain, before the first descriptor —
//! mirroring jbd2's log-block allocation order: revoke records are written in
//! commit phase 2a (`jbd2_journal_write_revoke_records`,
//! fs/jbd2/commit.c:551), before the metadata loop of phase 2b, each revoke
//! descriptor taking its log block from the same `j_head` cursor:
//!
//! ```text
//! [revoke 1] ... [revoke R] [descriptor 1] [its metadata ...] ... [commit]
//! ```
//!
//! Each revoke block is a [`RawRevokeHeader`](super::format::RawRevokeHeader)
//! followed by back-to-back big-endian block numbers, split at
//! [`TagLayout::revoke_entries_per_block`] exactly as the captures split at
//! `tags_per_descriptor`; an empty revoke set writes no revoke block, so the
//! no-revoke chain stays byte-identical to Phase 4. Recovery treats a
//! same-tid revoke block as an ordinary chain member: SCAN steps over it,
//! PASS_REVOKE consumes it, and the one commit block still seals the whole
//! chain.
//!
//! - **Descriptor** (`JBD2_DESCRIPTOR_BLOCK`): a 12-byte [`RawJournalHeader`]
//!   then one block tag per captured block, in block-number order, with every
//!   byte offset owned by the journal's [`TagLayout`] (tag size, the 64-bit
//!   high word, the reserved checksum tail). Each descriptor of the chain is
//!   independent, exactly as in jbd2's `first_tag`-per-descriptor loop: **its**
//!   first tag is followed by a 16-byte journal UUID (written as zeros — we
//!   carry no journal UUID; recovery just skips it) and does *not* set
//!   `TAG_FLAG_SAME_UUID`; every later tag of that descriptor sets it (no
//!   UUID follows). **Its** last tag sets `TAG_FLAG_LAST_TAG`
//!   (`JBD2_FLAG_LAST_TAG` marks the end of *each* descriptor's tag array,
//!   commit.c:700-710 — recovery uses it to find the next chain block), and
//!   on a csum layout each descriptor carries its **own** tail checksum
//!   (`jbd2_descriptor_block_csum_set` runs per descriptor, commit.c:712-714).
//! - **Metadata blocks**: each descriptor's captured after-images follow it,
//!   in the same order as its tags, one full [`BLOCK_SIZE`] block each.
//! - **Commit** (`JBD2_COMMIT_BLOCK`): ONE [`RawCommitBlock`] sealing the
//!   whole chain, carrying the wall-clock commit time. The transaction is
//!   atomic at the chain level: recovery applies all of its descriptors or —
//!   absent a valid same-tid commit block — none.
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
//! 1. Write the transaction's log blocks: its revoke blocks (if any), then
//!    the descriptor chain — every descriptor and all N metadata blocks.
//! 2. **Barrier.** The revoke blocks, descriptors and data must be durable
//!    *before* the commit block; otherwise a crash could leave a commit record
//!    pointing at data that never reached the platter, and recovery would
//!    replay garbage — or, for a torn revoke block, under-suppress a freed
//!    block's stale image.
//! 3. Write the commit block.
//! 4. **Barrier.** Once the commit block is durable the transaction is
//!    committed: recovery will now see a complete (and, on a csum journal,
//!    checksum-verifying) log record and replay it.
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
//!    past the written blocks (N data + one per descriptor + the commit), set
//!    the tail to this transaction if the journal was clean, and publish
//!    `committed_tid`.
//!
//! # Checksums (csum v2/v3 journals, P7a-4)
//!
//! On a journal carrying [`INCOMPAT_CSUM_V2`](super::format::INCOMPAT_CSUM_V2)
//! or [`INCOMPAT_CSUM_V3`](super::format::INCOMPAT_CSUM_V3), every log
//! structure is stamped exactly where jbd2 stamps it: each tag's data checksum
//! at tag placement (over the block **as logged**, i.e. post-escape —
//! [`TagWriter::put`](super::format::TagWriter::put)), the descriptor tail at
//! the seal ([`TagWriter::finish`](super::format::TagWriter::finish)), the
//! commit block's `h_chksum[0]`
//! ([`JournalCsumSeed::stamp_commit_block`]), and the journal superblock on
//! every rewrite (the [`JournalGeometry::write_superblock`](super::JournalGeometry::write_superblock)
//! funnel). A featureless (v0) journal writes byte-identical logs to Phase 4.
//!
//! # Revoke records (P7b-3)
//!
//! A committed transaction's revoke set travels twice, once per consumer:
//! serialized into the chain's revoke blocks for mount-time recovery's
//! PASS_REVOKE (the *crash* half — without it, recovery would replay a freed
//! block's old image over its post-free reuse), and published to the
//! journal's in-memory committed-revoke table at step 6 — the
//! [`revoke`](super::revoke) publication rule — for the *runtime* checkpoint
//! replay. Both memories carry the same `(block, tid)` records and feed the
//! same suppression predicate
//! ([`RevokeTable::suppresses`](super::revoke::RevokeTable)).
//!
//! # Phase 4 simplifications
//!
//! - **Synchronous**: [`try_commit_transaction`] does its device I/O inline.
//!   Production reaches it only through
//!   [`Journal::commit_or_drain_tail`](super::Journal) (the background commit
//!   thread and the unmount flush); ordered-data flushing is synchronous in
//!   that thread's task context, with no interrupt handoff (a Phase-7
//!   optimization).

use super::{
    super::prelude::*,
    Journal, Tid, Transaction,
    format::{
        BLOCKTYPE_COMMIT, BLOCKTYPE_DESCRIPTOR, Be32, Be64, JBD2_MAGIC, JournalCsumSeed,
        RawCommitBlock, RawJournalHeader, TAG_FLAG_ESCAPE, TAG_FLAG_LAST_TAG, TAG_FLAG_SAME_UUID,
        TagLayout,
    },
    transaction::CommitPhase,
};

/// The four bytes a metadata block must start with to require escaping: the
/// big-endian on-disk encoding of [`JBD2_MAGIC`].
const JBD2_MAGIC_BYTES: [u8; 4] = JBD2_MAGIC.to_be_bytes();

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

/// One descriptor block of a transaction's chain, sealed, plus the logged
/// sequence it tags (in tag order): each captured after-image paired with its
/// escape decision. The log writer consumes exactly the sequence the builder
/// tagged — the pairing travels inside the run, so writer and builder cannot
/// desync.
struct DescriptorRun<'t> {
    descriptor: Box<[u8; BLOCK_SIZE]>,
    blocks: Vec<LoggedBlock<'t>>,
}

/// One tagged metadata block of a [`DescriptorRun`], to be written into the
/// log right after its descriptor.
struct LoggedBlock<'t> {
    /// The captured after-image, borrowed from the transaction the chain was
    /// built from.
    bytes: &'t [u8; BLOCK_SIZE],
    /// Whether the block's tag carries [`TAG_FLAG_ESCAPE`]: its 4-byte head
    /// must be zeroed as it is written into the log (recovery restores it).
    escape: bool,
}

impl DescriptorRun<'_> {
    /// Returns the number of metadata blocks this descriptor tags (== the log
    /// blocks that follow it before the next chain block).
    fn nr_blocks(&self) -> usize {
        self.blocks.len()
    }
}

impl Transaction {
    /// Builds this transaction's descriptor chain: one sealed descriptor
    /// block per [`TagLayout::tags_per_descriptor`]-sized run of captured
    /// blocks, in block-number order (jbd2 `journal_commit_transaction`
    /// starts a fresh descriptor whenever the previous one's tag area fills,
    /// fs/jbd2/commit.c:606-635; a transaction that fits one descriptor
    /// produces a single-element chain with the frozen Phase-4 bytes).
    ///
    /// Every byte offset — tag stride, each descriptor's first-tag 16-byte
    /// UUID, the reserved checksum tail — is owned by `layout`'s tag writer
    /// ([`TagLayout::writer`]), the same source of truth the recovery scanner
    /// and the checkpoint/replay applier walk with. On a csum journal (`seed`
    /// present) the writer stamps every tag's data checksum and each
    /// descriptor's own tail checksum.
    fn build_descriptor_chain(
        &self,
        layout: TagLayout,
        seed: Option<JournalCsumSeed>,
    ) -> Result<Vec<DescriptorRun<'_>>> {
        let captures: Vec<(Ext4Bid, &[u8; BLOCK_SIZE])> = self.metadata_blocks().collect();
        if captures.is_empty() {
            return_errno_with_message!(Errno::EINVAL, "cannot commit an empty transaction");
        }

        captures
            .chunks(layout.tags_per_descriptor())
            .map(|run| self.build_descriptor_run(run, layout, seed))
            .collect()
    }

    /// Builds one descriptor block of the chain, tagging `run`'s captures.
    ///
    /// The per-descriptor flag discipline mirrors jbd2's (`first_tag` resets
    /// with every fresh descriptor, fs/jbd2/commit.c:635 / 679-693, and
    /// `JBD2_FLAG_LAST_TAG` marks the end of each descriptor's tag array,
    /// commit.c:700-710): **this** descriptor's first tag carries the UUID
    /// area and no `SAME_UUID`; its later tags set `SAME_UUID`; its last tag
    /// sets `LAST_TAG` — whether or not more descriptors follow in the chain.
    fn build_descriptor_run<'t>(
        &self,
        run: &[(Ext4Bid, &'t [u8; BLOCK_SIZE])],
        layout: TagLayout,
        seed: Option<JournalCsumSeed>,
    ) -> Result<DescriptorRun<'t>> {
        let mut block = Box::new([0u8; BLOCK_SIZE]);

        // Header: magic + descriptor block type + this transaction's tid
        // (every descriptor of the chain bears the same tid; the shared tid
        // plus ONE trailing commit block is what makes the chain one atomic
        // transaction to recovery).
        let header = RawJournalHeader {
            h_magic: Be32::new(JBD2_MAGIC),
            h_blocktype: Be32::new(BLOCKTYPE_DESCRIPTOR),
            h_sequence: Be32::new(self.tid().get()),
        };
        block[..size_of::<RawJournalHeader>()].copy_from_slice(header.as_bytes());

        // The writer owns the buffer from here on; only `finish` (the seal)
        // hands it back, so an unsealed descriptor cannot reach the log.
        let mut writer = layout.writer(block, self.tid(), seed)?;
        let mut blocks = Vec::with_capacity(run.len());

        for (i, (bid, bytes)) in run.iter().enumerate() {
            let is_first = i == 0;
            let is_last = i == run.len() - 1;

            let mut flags = 0u16;
            if !is_first {
                // Only this descriptor's first tag carries a UUID; every
                // later tag of the same descriptor reuses it.
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
            blocks.push(LoggedBlock {
                bytes,
                escape: needs_escape,
            });

            // `put` refuses a block number that does not fit the layout's
            // tag (`EFBIG`, only possible without 64-bit tags) and a tag that
            // would overrun the descriptor's tag area (`ENOSPC` — unreachable
            // here by construction: the chain builder cuts each run at
            // `tags_per_descriptor`, but the writer keeps its own bound).
            //
            // The tag checksum covers the block AS LOGGED (jbd2 checksums the
            // escaped `wbuf` copy, commit.c:684): hand `put` the zero-headed
            // form an escaped block will have in the log, not the in-memory
            // original — step 1 below applies the same transform when writing.
            if needs_escape {
                let mut logged = Box::new([0u8; BLOCK_SIZE]);
                logged.copy_from_slice(*bytes);
                logged[..4].fill(0);
                writer.put(*bid, flags, &logged)?;
            } else {
                writer.put(*bid, flags, bytes)?;
            }
        }

        // Seal: stamps this descriptor's own tail checksum over the tags
        // above on a csum layout (byte-identical on v0 — the frozen byte
        // path) and returns the sealed bytes.
        Ok(DescriptorRun {
            descriptor: writer.finish(),
            blocks,
        })
    }

    /// Serializes this transaction's revoke set into zero or more sealed
    /// revoke blocks (jbd2 `jbd2_journal_write_revoke_records`,
    /// fs/jbd2/revoke.c:530-565): one
    /// [`RevokeBlockWriter`](super::format::RevokeBlockWriter) per
    /// [`TagLayout::revoke_entries_per_block`]-sized run of revoked block
    /// numbers, in block order — the same chunking shape as
    /// [`build_descriptor_chain`](Self::build_descriptor_chain), with the
    /// per-block bound owned by the writer (Linux starts a fresh descriptor
    /// when a record would cross `j_blocksize - csum_size`,
    /// revoke.c:602-607). Each block bears this transaction's tid and, on a
    /// csum journal, its own tail checksum ([`RevokeBlockWriter::finish`]).
    ///
    /// An empty revoke set yields no blocks: the no-revoke chain stays
    /// byte-identical to the pre-P7b layout.
    fn build_revoke_blocks(
        &self,
        layout: TagLayout,
        seed: Option<JournalCsumSeed>,
    ) -> Result<Vec<Box<[u8; BLOCK_SIZE]>>> {
        let revokes: Vec<Ext4Bid> = self.revoked_blocks().collect();
        revokes
            .chunks(layout.revoke_entries_per_block())
            .map(|run| {
                let mut writer = layout.revoke_writer(self.tid(), seed)?;
                for &blocknr in run {
                    writer.put(blocknr)?;
                }
                Ok(writer.finish())
            })
            .collect()
    }
}

/// Builds the commit block into a fresh [`BLOCK_SIZE`] buffer: header +
/// wall-clock commit time. On a csum journal (`seed` present) the block's
/// crc32c is stamped into `h_chksum[0]`
/// ([`JournalCsumSeed::stamp_commit_block`]); `h_chksum_type`/`h_chksum_size`
/// stay zero under csum v2/v3 — they belong to the v1 COMPAT checksum.
fn build_commit_block(tid: Tid, seed: Option<JournalCsumSeed>) -> Box<[u8; BLOCK_SIZE]> {
    let now = super::super::utils::now();
    let commit = RawCommitBlock {
        header: RawJournalHeader {
            h_magic: Be32::new(JBD2_MAGIC),
            h_blocktype: Be32::new(BLOCKTYPE_COMMIT),
            h_sequence: Be32::new(tid.get()),
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
    if let Some(seed) = seed {
        seed.stamp_commit_block(&mut block);
    }
    block
}

impl Journal {
    /// Points the on-disk journal superblock at `txn_start`/`tid` as the oldest
    /// un-checkpointed transaction (jbd2 updates `s_start`/`s_sequence` when the
    /// journal transitions from clean to dirty), then barriers.
    ///
    /// Two callers, both moving the tail onto a live transaction: this commit
    /// pipeline when the journal was clean before the commit (a dirty journal
    /// already records an older transaction that must be preserved), and the
    /// checkpoint pass's partial advance, when the defer-prefix rule stops it
    /// on a transaction it must not apply (see
    /// [`checkpoint`](super::checkpoint::checkpoint)).
    ///
    /// Does NOT touch `s_head`: Linux only reads `s_head` on the clean-unmount fast
    /// path (`recovery.c`, when `s_start == 0`), which neither caller produces —
    /// `s_start` stays nonzero until a full checkpoint clears it. **The
    /// checkpoint pass's clean rewrite owns `s_head`: when it zeroes `s_start`
    /// it MUST also write a correct `s_head`, or Linux would resume the log at
    /// a stale offset.** (Adversarial-review finding, Phase 4 Task 3.)
    pub(super) fn update_superblock_tail(
        &self,
        device: &dyn BlockDevice,
        txn_start: u32,
        tid: Tid,
    ) -> Result<()> {
        let mut raw = self.geometry.read_raw_superblock(device)?;
        raw.s_start = Be32::new(txn_start);
        raw.s_sequence = Be32::new(tid.get());
        // The funnel restamps `s_checksum` on a csum journal and barriers:
        // the recoverability pointer must itself be durable.
        self.geometry.write_superblock(device, raw)
    }
}

/// The outcome of one commit attempt ([`try_commit_transaction`]).
///
/// An `Err` from the attempt means the transaction is lost (the caller must
/// abort the journal); these variants are the two NON-fatal outcomes, kept
/// apart from `Err` so "refused before any write, retryable" can never be
/// confused with "failed mid-write".
pub(super) enum CommitAttempt {
    /// The transaction committed: its commit record is durable in the log.
    Committed(Tid),
    /// The chain footprint exceeds the ring's free segment
    /// ([`JournalGeometry::free_log_blocks`](super::JournalGeometry::free_log_blocks)):
    /// an un-checkpointed predecessor still occupies `[tail, head)`, whose log
    /// blocks are the ONLY copy of its committed after-images (Model A) —
    /// writing would overwrite them and turn the next crash into a silent
    /// under-replay. NOTHING was written or flushed and no phase advanced
    /// (the transaction stays [`Locked`](CommitPhase::Locked) in the
    /// committing slot), so the caller can drain the tail (checkpoint) and
    /// retry with the same staged transaction.
    NeedsLogSpace,
}

/// The staged commit funnel for the ktest suites, which drive commits
/// directly against known-sized rings: stages `txn` into the committing slot
/// (so the pipeline's phase walk runs slot-resident exactly like
/// production), runs one attempt, and retires the slot — treating "does not
/// fit the free segment" as a plain `ENOSPC` error instead of draining.
/// Production commits go through
/// [`Journal::commit_or_drain_tail`](super::Journal) under the commit
/// thread's / unmount flush's own staging.
#[cfg(ktest)]
pub(super) fn commit_transaction(
    journal: &Journal,
    device: &dyn BlockDevice,
    txn: Transaction,
) -> Result<Tid> {
    let txn = journal.stage_transaction_for_test(txn)?;
    let outcome = match try_commit_transaction(journal, device, &txn) {
        Ok(CommitAttempt::Committed(tid)) => Ok(tid),
        Ok(CommitAttempt::NeedsLogSpace) => Err(Error::with_message(
            Errno::ENOSPC,
            "transaction does not fit the journal's free segment",
        )),
        Err(e) => Err(e),
    };
    journal.clear_committing(txn.tid());
    outcome
}

/// Commits `txn` — the transaction staged in the journal's committing slot —
/// to the on-disk log as a jbd2 transaction and makes it recoverable
/// ([`CommitAttempt::Committed`]), walking the slot's [`CommitPhase`] through
/// `Locked → Flush → Commit → CommitRecord → Finished` at the step
/// boundaries below. Unless the chain does not fit the ring's free segment,
/// in which case NOTHING is written, no phase advances, and
/// [`CommitAttempt::NeedsLogSpace`] asks the caller to checkpoint and retry.
/// On `Err` the transaction is lost mid-write and the caller must abort.
///
/// Implements the log layout and crash-safe write ordering documented at the
/// module level: fit guard → ordered data → barrier → log (descriptors +
/// metadata) → barrier → commit → barrier → (superblock → barrier if the
/// journal was clean) → in-memory state.
pub(super) fn try_commit_transaction(
    journal: &Journal,
    device: &dyn BlockDevice,
    txn: &Transaction,
) -> Result<CommitAttempt> {
    let tid = txn.tid();

    // The csum seed exists iff the journal carries csum v2/v3; threading it
    // into the revoke/descriptor/commit builders is what turns their
    // stamping on. The revoke blocks and the chain are built first (pure
    // in-memory work): their exact footprint drives the fit guard below.
    let seed = journal.geometry.csum_seed();
    let revoke_blocks = txn.build_revoke_blocks(journal.geometry.tag_layout(), seed)?;
    let chain = txn.build_descriptor_chain(journal.geometry.tag_layout(), seed)?;
    let nr_data: usize = chain.iter().map(DescriptorRun::nr_blocks).sum();
    // Total log blocks this transaction occupies: its revoke blocks, its
    // data blocks, one descriptor per run, and the commit block.
    let nr_log_blocks = revoke_blocks.len() + nr_data + chain.len() + 1;
    let Ok(footprint) = u32::try_from(nr_log_blocks) else {
        // No ring is this large (`s_maxlen` is a u32 of blocks), so no drain
        // can ever make it fit: a hard refusal, not a retryable one.
        return_errno_with_message!(Errno::ENOSPC, "transaction does not fit the journal");
    };

    // The head to write at and the un-checkpointed tail are read under the
    // state lock; the writes themselves happen without holding it (a single
    // committer, so no other committer races us) and the state is updated
    // again at the end.
    let (start_head, dirty_tail) = {
        let st = journal.state_write();
        (st.head, st.tail_block)
    };
    let was_clean = dirty_tail.is_none();

    // Fit guard (BEFORE any write, including the ordered-data flush): the
    // exact chain footprint must fit the ring's FREE segment `[head, tail)`,
    // not merely the whole ring. The dirty segment `[tail, head)` holds
    // committed transactions whose after-images exist nowhere but the log
    // until checkpoint (Model A); overwriting them would make a crash
    // silently under-replay fsync-acknowledged metadata. A dirty tail CAN be
    // in the way here — the commit thread's post-commit checkpoint is
    // non-fatal on failure, and `max_credits` bounds one transaction against
    // the whole ring only. jbd2 never reaches this point: it blocks writers
    // up front on `jbd2_log_space_left` (fs/jbd2/transaction.c:291); until
    // P7c builds that backpressure, refusing to write — letting the caller
    // drain the tail and retry, or abort loudly — is the minimal safe
    // behavior.
    if footprint > journal.geometry.free_log_blocks(start_head, dirty_tail) {
        // The transaction stays `Locked` in the committing slot: nothing was
        // written, so the caller may drain the tail and retry it.
        return Ok(CommitAttempt::NeedsLogSpace);
    }

    // The transaction is going to be written: enter the I/O phases. Each
    // advance below asserts the pipeline order under the state lock; an
    // illegal transition errors `EIO` and the caller aborts (see
    // `Journal::advance_committing_phase`).
    journal.advance_committing_phase(tid, CommitPhase::Flush)?;

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
    // semantics are pinned down.) The flush works on page-cache handles cloned
    // into the transaction at registration time and takes NO inode or journal
    // lock — an operation may be sleeping in `journal_start`'s capacity wait
    // holding its `inner.write()`, waiting on this very commit.
    let mut flushed_any = false;
    for (pages, len) in txn.ordered_data() {
        pages.flush_range(0..len)?;
        flushed_any = true;
    }
    if flushed_any {
        barrier(device)?;
    }

    // Ordered data is durable; the log writes begin (`T_FLUSH` → `T_COMMIT`).
    journal.advance_committing_phase(tid, CommitPhase::Commit)?;

    // --- Step 1: write the transaction's log blocks. Its revoke blocks go
    // first, before the metadata descriptors — jbd2's log-block allocation
    // order: `jbd2_journal_write_revoke_records` runs in commit phase 2a
    // (fs/jbd2/commit.c:551), before the phase-2b metadata loop, each revoke
    // descriptor drawing its block from the same `j_head` cursor. Then the
    // descriptor chain — each descriptor followed by the after-images it
    // tags ([desc 1][its data...][desc 2][its data...]…, the jbd2 chain
    // layout; a revoke-less single-run chain is the frozen Phase-4
    // [descriptor][data...] bytes). `log` is the NEXT slot to write
    // throughout, so after the loops it is the commit block's. ---
    let mut log = start_head;
    for revoke_block in &revoke_blocks {
        write_log_block(journal, device, log, revoke_block)?;
        log = journal.geometry.next_log_block(log);
    }
    for run in &chain {
        write_log_block(journal, device, log, &run.descriptor)?;
        log = journal.geometry.next_log_block(log);

        // Each run carries the after-image sequence its descriptor tagged
        // ([`LoggedBlock`]), so the writer emits exactly what the builder
        // saw — there is no second capture walk to fall out of step with.
        for block in &run.blocks {
            if block.escape {
                // Zero the head so recovery does not mistake it for a log
                // header; recovery restores the magic when applying the
                // block.
                let mut buf = Box::new(*block.bytes);
                buf[..4].fill(0);
                write_log_block(journal, device, log, &buf)?;
            } else {
                write_log_block(journal, device, log, block.bytes)?;
            }
            log = journal.geometry.next_log_block(log);
        }
    }
    // The chain (which borrows `txn`'s captures) is fully written.
    drop(chain);

    // --- Step 2: barrier. Revoke blocks + descriptors + data durable BEFORE
    // the commit block, so a commit record never certifies log content that
    // never reached the platter.
    barrier(device)?;

    // The chain is durable; the commit record is next (`T_COMMIT` →
    // `CommitRecord`, jbd2's DFLUSH+JFLUSH collapsed — one device,
    // synchronous barriers).
    journal.advance_committing_phase(tid, CommitPhase::CommitRecord)?;

    // --- Step 3: write the commit block, sealing the transaction. ---
    let commit_log = log;
    let commit = build_commit_block(tid, seed);
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
        // Serialize this clean→dirty `s_start` write against a checkpoint
        // pass's tail publication so the journal superblock has one writer at
        // a time (jbd2 `j_checkpoint_mutex`; see [`Journal::j_checkpoint`]).
        // Taken alone, released before the state lock below — no lock-order
        // edge. Single-committer today, so it never contends.
        let _checkpoint = journal.lock_checkpoint();
        journal.update_superblock_tail(device, start_head, tid)?;
    }

    // --- Step 6: publish the new log position and commit id in memory. ---
    // Blocks written: the revoke blocks + the data blocks + one descriptor
    // per chain run + the commit block (`footprint`, already proven to fit
    // the free segment).
    let new_head = journal.geometry.advance(start_head, footprint);
    {
        let mut st = journal.state_write();
        // The commit record is durable: `CommitRecord` → `Finished`, inside
        // the same lock window that publishes the transaction's effects, so
        // no observer can see a `Finished` slot whose revokes/pins are still
        // unpublished (or vice versa).
        st.advance_committing_phase(tid, CommitPhase::Finished)?;
        st.head = new_head;
        if was_clean {
            st.tail_block = Some(start_head);
            st.tail_tid = tid;
        }
        // Publish the transaction's revoke records: they become effective
        // exactly now, with the commit block durable (step 4) — never
        // earlier. `commit_or_drain_tail`'s inline drain checkpoint runs
        // mid-commit; had these been published at `running.take()` time, the
        // drain would suppress OLDER transactions' after-images on the
        // authority of a commit a crash could still erase, leaving the
        // device short of committed (fsync-acknowledged) metadata whose log
        // blocks the drain just retired. (jbd2 equivalently keeps a freed
        // buffer on the older checkpoint list until the freeing transaction
        // commits.)
        txn.stash_revokes(&mut st.revoked);
        // Release this transaction's freed-block pins in the same critical
        // section: its frees are durable exactly now, so the blocks may
        // re-enter the allocator — the pins and the revokes retire together
        // (Linux `ext4_process_freed_data`, the jbd2 post-commit callback).
        st.release_pinned_frees(tid);
    }
    // Release `Acquire` in `committed_tid()`: publishes after the state update.
    journal
        .committed_tid
        .store(tid.get(), core::sync::atomic::Ordering::Release);

    Ok(CommitAttempt::Committed(tid))
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::{
        // `*` re-exports the parent module's imports (`Journal`, `Transaction`,
        // `Be32`, `RawCommitBlock`, `RawJournalHeader`, `BLOCKTYPE_COMMIT`,
        // `BLOCKTYPE_DESCRIPTOR`, `JBD2_MAGIC`, `TagLayout`, the tag flags).
        // Only items the parent does not import are named explicitly.
        super::{
            super::test_utils::{Ext4FixtureBuilder, make_multi_block_file_inode},
            JOURNAL_INO,
            format::{
                BLOCKTYPE_REVOKE, BLOCKTYPE_SUPERBLOCK_V2, INCOMPAT_64BIT, INCOMPAT_CSUM_V3,
                RawBlockTag, RawJournalSuperblock,
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
            // A `maxlen`-block journal (not the 2-block `with_has_journal` default)
            // so operations run against the fixture's Ext4 can open journal handles
            // (a 2-block log admits zero credits).
            .with_journal_inode(maxlen)
            .build()
            .unwrap();
        // The fixture's Ext4 auto-started a commit thread on mount. These tests
        // drive commits manually through `journal` below (and some run operations
        // that now open handles against the fixture's Ext4), so stop that
        // background committer to keep this test's on-disk log deterministic.
        f.ext4.journal().unwrap().stop_commit_thread();

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
            let generation = txn.capture_create(*bid);
            txn.apply_patch(*bid, generation, |b| b.copy_from_slice(content))
                .unwrap();
        }
        txn
    }

    /// The v0 (feature-less) descriptor block is BIT-IDENTICAL to the
    /// pre-TagLayout writer's output — the byte freeze the whole 160×825
    /// crash-matrix baseline rides on. A transaction that fits one descriptor
    /// must produce a single-run chain (P7a-5 changed nothing on this path).
    /// Expected bytes hardcoded from the pre-change code's layout: 12-byte
    /// header, 8-byte tag 0, 16 zero UUID bytes, 8-byte tag 1
    /// (SAME_UUID | LAST_TAG), zeros to the end.
    #[ktest]
    fn descriptor_block_v0_bytes_are_frozen() {
        let c = [0u8; BLOCK_SIZE];
        let txn = make_txn(Tid::new(7), &[(0x123u64, c), (0x456u64, c)]);
        let chain = txn
            .build_descriptor_chain(TagLayout::from_features(0).unwrap(), None)
            .unwrap();
        assert_eq!(chain.len(), 1);
        let run = &chain[0];
        let block = &run.descriptor;
        let escapes: Vec<bool> = run.blocks.iter().map(|b| b.escape).collect();
        assert_eq!(escapes, vec![false, false]);

        let mut expected = [0u8; BLOCK_SIZE];
        // Header: magic, DESCRIPTOR (1), tid 7 — all big-endian.
        expected[..12].copy_from_slice(&[
            0xC0, 0x3B, 0x39, 0x98, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x07,
        ]);
        // Tag 0 at 12: t_blocknr 0x123, t_checksum 0, t_flags 0; UUID zeros
        // [20, 36).
        expected[12..20].copy_from_slice(&[0x00, 0x00, 0x01, 0x23, 0, 0, 0, 0]);
        // Tag 1 at 36: t_blocknr 0x456, t_checksum 0, t_flags SAME_UUID |
        // LAST_TAG = 0x000A.
        expected[36..44].copy_from_slice(&[0x00, 0x00, 0x04, 0x56, 0, 0, 0x00, 0x0A]);

        assert_eq!(block.as_slice(), expected.as_slice());
    }

    /// The builder emits 12-byte 64-bit tags (including a > 32-bit block
    /// number) that the shared recovery-side walker reads back verbatim.
    #[ktest]
    fn descriptor_builder_64bit_round_trip() {
        let layout = TagLayout::from_features(INCOMPAT_64BIT).unwrap();
        let small: Ext4Bid = 0x321;
        let wide: Ext4Bid = (1 << 32) | 0x700;
        let c = [0u8; BLOCK_SIZE];
        let txn = make_txn(Tid::new(3), &[(small, c), (wide, c)]);

        let chain = txn.build_descriptor_chain(layout, None).unwrap();
        assert_eq!(chain.len(), 1);
        let block = &chain[0].descriptor;
        let escapes: Vec<bool> = chain[0].blocks.iter().map(|b| b.escape).collect();
        assert_eq!(escapes, vec![false, false]);

        let tags: Vec<_> = layout.walk(block).map(|tag| tag.unwrap()).collect();
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].blocknr(), small);
        assert!(!tags[0].is_last());
        assert_eq!(tags[1].blocknr(), wide);
        assert!(tags[1].is_last());
    }

    /// The builder emits 16-byte csum-v3 tags the shared walker reads back,
    /// with the escape flag surviving the round trip, every tag checksum
    /// stamped over the LOGGED (post-escape) form, and the descriptor tail
    /// sealed.
    #[ktest]
    fn descriptor_builder_csum_v3_round_trip() {
        let layout = TagLayout::from_features(INCOMPAT_CSUM_V3 | INCOMPAT_64BIT).unwrap();
        let seed = JournalCsumSeed::for_test(b"p7a4-commit-uuid");
        let tid = Tid::new(4);
        let mut escaped = [0u8; BLOCK_SIZE];
        escaped[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
        escaped[4..8].copy_from_slice(b"REST");
        let plain = [0u8; BLOCK_SIZE];
        let wide: Ext4Bid = (9 << 32) | 0x800;
        let txn = make_txn(tid, &[(0x200u64, escaped), (wide, plain)]);

        let chain = txn.build_descriptor_chain(layout, Some(seed)).unwrap();
        assert_eq!(chain.len(), 1);
        let block = &chain[0].descriptor;
        // The run pairs each escape decision with the after-image it tagged.
        let escapes: Vec<bool> = chain[0].blocks.iter().map(|b| b.escape).collect();
        assert_eq!(escapes, vec![true, false]);
        assert_eq!(chain[0].blocks[0].bytes, &escaped);
        assert_eq!(chain[0].blocks[1].bytes, &plain);

        let tags: Vec<_> = layout.walk(block).map(|tag| tag.unwrap()).collect();
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].blocknr(), 0x200);
        assert!(tags[0].is_escaped());
        assert_eq!(tags[1].blocknr(), wide);
        assert!(!tags[1].is_escaped());
        assert!(tags[1].is_last());

        // The escaped tag's checksum covers the zero-headed LOG form, not the
        // in-memory original (jbd2 checksums the escaped wbuf, commit.c:684).
        let mut logged = escaped;
        logged[..4].fill(0);
        assert!(tags[0].verify_data_csum(seed, tid, &logged));
        assert!(!tags[0].verify_data_csum(seed, tid, &escaped));
        assert!(tags[1].verify_data_csum(seed, tid, &plain));
        // `finish` sealed the descriptor tail over the stamped tags.
        assert!(seed.verify_block_tail(block));
    }

    /// Without 64-bit tags a > 32-bit destination block still refuses with
    /// `EFBIG` (the pre-TagLayout guard, now layout-conditional).
    #[ktest]
    fn descriptor_builder_rejects_wide_block_on_v0_layout() {
        let c = [0u8; BLOCK_SIZE];
        let txn = make_txn(Tid::new(1), &[((1u64 << 32) | 5, c)]);
        let err = txn
            .build_descriptor_chain(TagLayout::from_features(0).unwrap(), None)
            .map(|chain| chain.len())
            .unwrap_err();
        assert_eq!(err.error(), Errno::EFBIG);
    }

    /// A transaction with more captures than one descriptor's tag area holds
    /// splits into a chain, and the per-descriptor flag discipline mirrors
    /// jbd2's: EACH descriptor's first tag carries the UUID area (no
    /// `SAME_UUID`) and EACH descriptor's last tag sets `LAST_TAG`
    /// (fs/jbd2/commit.c:679-710 — `first_tag` resets per descriptor and the
    /// end-of-descriptor marker is written whenever one fills).
    #[ktest]
    fn descriptor_chain_splits_with_per_descriptor_flags() {
        let layout = TagLayout::from_features(0).unwrap();
        let per_descriptor = layout.tags_per_descriptor();
        let n = per_descriptor + 2;

        let c = [0u8; BLOCK_SIZE];
        let mut txn = Transaction::new(Tid::new(9));
        for i in 0..n {
            let bid = Ext4Bid::try_from(1000 + i).unwrap();
            let generation = txn.capture_create(bid);
            txn.apply_patch(bid, generation, |b| b.copy_from_slice(&c))
                .unwrap();
        }

        let chain = txn.build_descriptor_chain(layout, None).unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].nr_blocks(), per_descriptor);
        assert_eq!(chain[1].nr_blocks(), 2);

        // Both descriptors bear the transaction's tid.
        for run in &chain {
            let header = RawJournalHeader::parse(&run.descriptor);
            assert_eq!(header.h_magic.get(), JBD2_MAGIC);
            assert_eq!(header.h_blocktype.get(), BLOCKTYPE_DESCRIPTOR);
            assert_eq!(header.h_sequence.get(), 9);
        }

        // Descriptor 1: a full tag array; only ITS first tag lacks SAME_UUID,
        // only ITS last tag is LAST_TAG.
        let tags1: Vec<_> = layout
            .walk(&chain[0].descriptor)
            .map(|tag| tag.unwrap())
            .collect();
        assert_eq!(tags1.len(), per_descriptor);
        assert_eq!(tags1[0].flags() & TAG_FLAG_SAME_UUID, 0);
        assert!(
            tags1[1..]
                .iter()
                .all(|t| t.flags() & TAG_FLAG_SAME_UUID != 0)
        );
        assert!(tags1[..per_descriptor - 1].iter().all(|t| !t.is_last()));
        assert!(tags1[per_descriptor - 1].is_last());

        // Descriptor 2: the 2-tag remainder, with the SAME per-descriptor
        // discipline — its first tag again carries the UUID area.
        let tags2: Vec<_> = layout
            .walk(&chain[1].descriptor)
            .map(|tag| tag.unwrap())
            .collect();
        assert_eq!(tags2.len(), 2);
        assert_eq!(tags2[0].flags() & TAG_FLAG_SAME_UUID, 0);
        assert!(!tags2[0].is_last());
        assert_ne!(tags2[1].flags() & TAG_FLAG_SAME_UUID, 0);
        assert!(tags2[1].is_last());

        // The chain tags every capture in block order across the split.
        assert_eq!(tags1[0].blocknr(), 1000);
        assert_eq!(tags2[1].blocknr(), Ext4Bid::try_from(1000 + n - 1).unwrap());
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

        let txn = make_txn(Tid::new(1), &[(dest0, c0), (dest1, c1)]);
        let tid = commit_transaction(f.journal.as_ref(), device.as_ref(), txn).unwrap();
        assert_eq!(tid, Tid::new(1));

        // Descriptor at log block `first` (== 1).
        let desc = read_log_header(&f, 1);
        assert_eq!(desc.h_magic.get(), JBD2_MAGIC);
        assert_eq!(desc.h_blocktype.get(), BLOCKTYPE_DESCRIPTOR);
        assert_eq!(desc.h_sequence.get(), tid.get());

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
        assert_eq!(commit.h_sequence.get(), tid.get());

        assert_eq!(f.journal.committed_tid(), tid);

        // On-disk journal superblock now points at this transaction.
        let sb: RawJournalSuperblock = f
            .fixture
            .disk
            .segment()
            .read_val(JOURNAL_START_BLOCK as usize * BLOCK_SIZE)
            .unwrap();
        assert_eq!(sb.s_start.get(), 1); // == first
        assert_eq!(sb.s_sequence.get(), tid.get());
    }

    #[ktest]
    fn commit_escapes_block_starting_with_magic() {
        let f = journaled_fixture(16, 1, 1);
        let device = f.fixture.ext4.block_device();

        // A metadata block whose first four bytes ARE the jbd2 magic.
        let mut content = [0u8; BLOCK_SIZE];
        content[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
        content[4..8].copy_from_slice(b"REST");

        let txn = make_txn(Tid::new(1), &[(600u64, content)]);
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
        let t1 = make_txn(Tid::new(1), &[(500u64, a), (700u64, b)]);
        let tid1 = commit_transaction(f.journal.as_ref(), device.as_ref(), t1).unwrap();
        assert_eq!(tid1, Tid::new(1));

        // Head advanced past T1's 4 blocks: now at log block 5.
        {
            let st = f.journal.state_write();
            assert_eq!(st.head, 5);
            assert_eq!(st.tail_block, Some(1));
            assert_eq!(st.tail_tid, Tid::new(1));
        }

        // T2: tid 2, one block -> occupies log [5..=7] (desc, 1 data, commit).
        let mut c = [0u8; BLOCK_SIZE];
        c[..4].copy_from_slice(b"T2C0");
        let t2 = make_txn(Tid::new(2), &[(900u64, c)]);
        let tid2 = commit_transaction(f.journal.as_ref(), device.as_ref(), t2).unwrap();
        assert_eq!(tid2, Tid::new(2));

        // T1's descriptor and commit still carry tid 1.
        assert_eq!(read_log_header(&f, 1).h_sequence.get(), 1);
        assert_eq!(
            read_log_header(&f, 1).h_blocktype.get(),
            BLOCKTYPE_DESCRIPTOR
        );
        assert_eq!(read_log_header(&f, 4).h_sequence.get(), 1);
        assert_eq!(read_log_header(&f, 4).h_blocktype.get(), BLOCKTYPE_COMMIT);

        // T2's descriptor lands right after T1, at log block 5, with tid 2.
        assert_eq!(read_log_header(&f, 5).h_sequence.get(), 2);
        assert_eq!(
            read_log_header(&f, 5).h_blocktype.get(),
            BLOCKTYPE_DESCRIPTOR
        );
        assert_eq!(read_log_header(&f, 7).h_sequence.get(), 2);
        assert_eq!(read_log_header(&f, 7).h_blocktype.get(), BLOCKTYPE_COMMIT);

        assert_eq!(f.journal.committed_tid(), Tid::new(2));
        {
            let st = f.journal.state_write();
            assert_eq!(st.head, 8);
            // s_start still points at T1 (the oldest un-checkpointed txn).
            assert_eq!(st.tail_block, Some(1));
            assert_eq!(st.tail_tid, Tid::new(1));
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
        let txn = make_txn(Tid::new(1), &[(500u64, content)]);
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

        // A 32-block log so the write's handle (WRITE_CREDITS) fits.
        let f = journaled_fixture(32, 1, 1);
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
            on_disk, [0u8; BLOCK_SIZE],
            "data must not be on disk before commit"
        );

        // Build a transaction that captures a metadata block (so the commit has
        // something to log) AND registers the inode as ordered data.
        let mut txn = Transaction::new(Tid::new(1));
        let mut meta = [0u8; BLOCK_SIZE];
        meta[..4].copy_from_slice(b"META");
        let generation = txn.capture_create(500);
        txn.apply_patch(500, generation, |b| b.copy_from_slice(&meta))
            .unwrap();
        let pages = inode.page_cache().unwrap();
        txn.register_ordered_data(
            inode.ino(),
            inode.self_arc().map(|i| Arc::downgrade(&i)).unwrap(),
            pages,
            data.len(),
        );

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

    /// A forgotten block leaves no trace in the committed log's DESCRIPTORS
    /// (jbd2 "block is journaled and then revoked", the
    /// cancel-the-journal-entry option) and instead names the block in the
    /// chain's leading REVOKE block (P7b-3): the revoke block heads the
    /// chain with the freed block as its one be32 entry, the descriptor tags
    /// only the surviving capture, and the revoke is also published to the
    /// journal's committed-revoke memory at commit.
    #[ktest]
    fn commit_omits_forgotten_block_and_publishes_revoke() {
        let f = journaled_fixture(16, 1, 1);
        let device = f.fixture.ext4.block_device();

        let (freed, kept) = (500u64, 700u64);
        let mut freed_img = [0u8; BLOCK_SIZE];
        freed_img[..8].copy_from_slice(b"FREEDIMG");
        let mut kept_img = [0u8; BLOCK_SIZE];
        kept_img[..8].copy_from_slice(b"KEPT-IMG");

        let mut txn = make_txn(Tid::new(1), &[(freed, freed_img), (kept, kept_img)]);
        txn.forget_block(freed);
        commit_transaction(f.journal.as_ref(), device.as_ref(), txn).unwrap();

        // The chain leads with the revoke block: header (magic / REVOKE /
        // tid 1), r_count = 16 + one 4-byte entry, the freed block number.
        let revoke = read_log_block(&f, 1);
        let revoke_header = read_log_header(&f, 1);
        assert_eq!(revoke_header.h_magic.get(), JBD2_MAGIC);
        assert_eq!(revoke_header.h_blocktype.get(), BLOCKTYPE_REVOKE);
        assert_eq!(revoke_header.h_sequence.get(), 1);
        assert_eq!(&revoke[12..16], &[0, 0, 0, 20]);
        assert_eq!(
            &revoke[16..20],
            &u32::try_from(freed).unwrap().to_be_bytes()
        );

        // Then the descriptor: exactly one tag, naming the kept block (with
        // LAST_TAG), followed by its after-image.
        let desc = read_log_header(&f, 2);
        assert_eq!(desc.h_blocktype.get(), BLOCKTYPE_DESCRIPTOR);
        let tag0 = read_tag(&f, 2, 0);
        assert_eq!(tag0.t_blocknr.get(), kept as u32);
        assert_ne!(tag0.t_flags.get() & TAG_FLAG_LAST_TAG, 0);
        assert_eq!(read_log_block(&f, 3), kept_img);
        // The chain block right after the single logged image is the commit
        // block — no second data block (the freed image) was written.
        assert_eq!(read_log_header(&f, 4).h_blocktype.get(), BLOCKTYPE_COMMIT);
        // The in-memory head accounts for all four chain blocks.
        assert_eq!(f.journal.state_write().head, 5);

        // The revoke crossed into the committed-revoke memory with the
        // commit, and only then retires (by checkpoint — whose chain walk
        // now steps over the on-disk revoke block).
        assert_eq!(f.journal.committed_revoke_records_for_test(), 1);
        super::super::checkpoint::checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();
        assert_eq!(f.journal.committed_revoke_records_for_test(), 0);
        // The checkpoint really applied through the revoke-led chain: the
        // kept image reached its final location and the journal is clean.
        let mut final_block = [0u8; BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .read_bytes(kept as usize * BLOCK_SIZE, &mut final_block)
            .unwrap();
        assert_eq!(final_block, kept_img);
    }

    /// Without 64-bit entries a > 32-bit revoked block number refuses
    /// loudly before any write (the revoke builder's `EFBIG`, mirroring the
    /// tag writer's) rather than truncating to revoke the wrong block. (The
    /// per-feature-set raw-bytes pin of the revoke serialization lives with
    /// the recovery round trips, `commit_writes_revoke_records_across_feature_sets`.)
    #[ktest]
    fn commit_refuses_wide_revoke_on_v0_layout() {
        let f = journaled_fixture(16, 1, 1);
        let device = f.fixture.ext4.block_device();
        let before = read_log_block(&f, 1);
        let mut txn = make_txn(Tid::new(1), &[(700u64, [0u8; BLOCK_SIZE])]);
        txn.forget_block((1 << 32) | 5);
        let err = commit_transaction(f.journal.as_ref(), device.as_ref(), txn).unwrap_err();
        assert_eq!(err.error(), Errno::EFBIG);
        assert_eq!(read_log_block(&f, 1), before);
    }

    /// An empty ordered set issues no data barrier: only the metadata + commit
    /// barriers (2), plus the superblock barrier on a clean-journal commit (3).
    #[ktest]
    fn commit_without_ordered_data_skips_data_barrier() {
        let f = journaled_fixture(16, 1, 1);
        let device = f.fixture.ext4.block_device();

        let before = f.fixture.disk.flush_count();

        let mut content = [0u8; BLOCK_SIZE];
        content[..4].copy_from_slice(b"META");
        let txn = make_txn(Tid::new(1), &[(500u64, content)]);
        assert_eq!(txn.nr_ordered_data(), 0);
        commit_transaction(f.journal.as_ref(), device.as_ref(), txn).unwrap();

        // No ordered inodes => no data barrier: exactly the metadata + commit +
        // (clean-journal) superblock barriers, i.e. 3.
        let issued = f.fixture.disk.flush_count() - before;
        assert_eq!(issued, 3, "expected exactly 3 barriers (no data barrier)");
    }

    // --- a5 review MAJOR 2: the fit guard bounds the chain against the
    // ring's FREE segment `[head, tail)`, never the whole ring. ---

    /// A distinct per-index after-image for the free-segment tests.
    fn indexed_block(i: u64) -> [u8; BLOCK_SIZE] {
        let mut b = [0u8; BLOCK_SIZE];
        b[..8].copy_from_slice(&i.to_le_bytes());
        b[8..16].copy_from_slice(b"FREESEG!");
        b
    }

    /// Builds a transaction of `n` captures at destinations
    /// `first_dest..first_dest + n`, each carrying [`indexed_block`]`(i)`.
    fn indexed_txn(tid: Tid, first_dest: u64, n: u64) -> Transaction {
        let blocks: Vec<(Ext4Bid, [u8; BLOCK_SIZE])> =
            (0..n).map(|i| (first_dest + i, indexed_block(i))).collect();
        make_txn(tid, &blocks)
    }

    /// With an un-checkpointed T1 at the tail, a T2 that fits the whole ring
    /// but NOT the free segment is refused with NOTHING written — its writes
    /// would have wrapped onto T1's log blocks, the only copy of T1's
    /// committed after-images — staying `Locked` in the committing slot
    /// ([`CommitAttempt::NeedsLogSpace`]); after a checkpoint drains the
    /// tail, the SAME staged transaction commits (the commit thread's
    /// drain-and-retry flow, driven by hand).
    #[ktest]
    fn commit_refuses_chain_crossing_uncheckpointed_tail() {
        crate::time::clocks::init_for_ktest();
        // maxlen 16, first 1: ring = 15.
        let f = journaled_fixture(16, 1, 1);
        let device = f.fixture.ext4.block_device();

        // T1: 1 capture -> log [1..4); head 4, tail 1 -> free 12.
        let t1 = indexed_txn(Tid::new(1), 500, 1);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t1).unwrap();
        {
            let st = f.journal.state_write();
            assert_eq!(st.head, 4);
            assert_eq!(st.tail_block, Some(1));
        }

        // T2: 11 captures -> footprint 13 (11 data + 1 desc + 1 commit):
        // fits the 15-block ring, NOT the 12-block free segment. From head 4
        // its blocks would cover [4..16) and wrap onto [1..3) — T1's chain.
        // Staged like production, so the refusal is observable in the slot.
        let t2 = f
            .journal
            .stage_transaction_for_test(indexed_txn(Tid::new(2), 1000, 11))
            .unwrap();
        let before: Vec<[u8; BLOCK_SIZE]> = (1u32..4).map(|log| read_log_block(&f, log)).collect();
        let attempt = try_commit_transaction(f.journal.as_ref(), device.as_ref(), &t2).unwrap();
        assert!(
            matches!(attempt, CommitAttempt::NeedsLogSpace),
            "a chain crossing the tail must be refused"
        );

        // NOTHING written: T1's log blocks are intact, no state moved, and
        // no phase advanced — the refused transaction is still `Locked`.
        let after: Vec<[u8; BLOCK_SIZE]> = (1u32..4).map(|log| read_log_block(&f, log)).collect();
        assert_eq!(before, after, "the refused commit must not touch the tail");
        {
            let st = f.journal.state_write();
            assert_eq!(st.head, 4);
            assert_eq!(st.tail_block, Some(1));
        }
        assert_eq!(f.journal.committed_tid(), Tid::new(1));
        assert_eq!(
            f.journal.committing_for_test(),
            Some((Tid::new(2), CommitPhase::Locked))
        );

        // Drain the tail — the pass snapshots the committing slot's (empty)
        // unpublished-revoke set from the journal state, exactly the
        // production drain shape — then retry the SAME staged transaction:
        // it now fits the clean ring and commits, ending `Finished`.
        super::super::checkpoint::checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();
        let attempt = try_commit_transaction(f.journal.as_ref(), device.as_ref(), &t2).unwrap();
        let CommitAttempt::Committed(tid) = attempt else {
            panic!("after the drain the chain fits");
        };
        assert_eq!(tid, Tid::new(2));
        assert_eq!(f.journal.committed_tid(), Tid::new(2));
        assert_eq!(
            f.journal.committing_for_test(),
            Some((Tid::new(2), CommitPhase::Finished))
        );
        f.journal.clear_committing(Tid::new(2));

        // The retried commit is a real one: checkpointing lands T2's
        // after-images at their final locations.
        super::super::checkpoint::checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();
        let mut final_block = [0u8; BLOCK_SIZE];
        f.fixture
            .disk
            .segment()
            .read_bytes(1000 * BLOCK_SIZE, &mut final_block)
            .unwrap();
        assert_eq!(final_block, indexed_block(0));
    }

    /// The free-segment arithmetic across the ring wrap: a chain that
    /// exactly fills the wrapped free segment `[head..ring end) + [first..tail)`
    /// commits — the head lands exactly ON the tail, a legally FULL ring —
    /// and the next commit is refused with zero free blocks, the tail intact.
    #[ktest]
    fn commit_fills_wrapped_free_segment_exactly() {
        crate::time::clocks::init_for_ktest();
        // maxlen 16, first 1: ring = 15.
        let f = journaled_fixture(16, 1, 1);
        let device = f.fixture.ext4.block_device();

        // T1 (8 captures -> 10 blocks [1..11)) pushes the head deep into the
        // ring; checkpoint reclaims it (head stays at 11, ring clean).
        let t1 = indexed_txn(Tid::new(1), 500, 8);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t1).unwrap();
        super::super::checkpoint::checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();
        assert_eq!(f.journal.state_write().head, 11);

        // T2 (6 captures -> 8 blocks) wraps the ring end: [11..16) + [1..4);
        // the was-clean tail is re-established at 11. Free = [4..11) = 7.
        let t2 = indexed_txn(Tid::new(2), 600, 6);
        commit_transaction(f.journal.as_ref(), device.as_ref(), t2).unwrap();
        {
            let st = f.journal.state_write();
            assert_eq!(st.head, 4);
            assert_eq!(st.tail_block, Some(11));
        }

        // T3 (5 captures -> 7 blocks) fills the free segment EXACTLY: the
        // head wraps forward onto the tail without touching a tail block.
        let t3 = f
            .journal
            .stage_transaction_for_test(indexed_txn(Tid::new(3), 700, 5))
            .unwrap();
        let attempt = try_commit_transaction(f.journal.as_ref(), device.as_ref(), &t3).unwrap();
        assert!(matches!(attempt, CommitAttempt::Committed(_)));
        f.journal.clear_committing(Tid::new(3));
        assert_eq!(f.journal.state_write().head, 11);

        // Full ring: even the smallest chain (1 capture -> 3 blocks) has
        // zero free blocks. Refused; T2's tail descriptor is untouched.
        let tail_desc = read_log_block(&f, 11);
        let t4 = f
            .journal
            .stage_transaction_for_test(indexed_txn(Tid::new(4), 800, 1))
            .unwrap();
        let attempt = try_commit_transaction(f.journal.as_ref(), device.as_ref(), &t4).unwrap();
        assert!(matches!(attempt, CommitAttempt::NeedsLogSpace));
        f.journal.clear_committing(Tid::new(4));
        assert_eq!(read_log_block(&f, 11), tail_desc);

        // Everything committed into the wrapped ring is real: draining the
        // full ring applies T2 and T3 to their final locations.
        super::super::checkpoint::checkpoint(f.journal.as_ref(), device.as_ref()).unwrap();
        for (first_dest, n) in [(600u64, 6u64), (700, 5)] {
            for i in 0..n {
                let mut b = [0u8; BLOCK_SIZE];
                f.fixture
                    .disk
                    .segment()
                    .read_bytes(
                        usize::try_from(first_dest + i).unwrap() * BLOCK_SIZE,
                        &mut b,
                    )
                    .unwrap();
                assert_eq!(b, indexed_block(i), "dest {}", first_dest + i);
            }
        }
    }
}
