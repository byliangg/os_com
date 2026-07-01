// SPDX-License-Identifier: MPL-2.0

//! The journaling seam (JBD2 wrappers).
//!
//! Phase 2 runs **without** a journal, but every metadata modification is
//! already routed through the four access wrappers below so that Phase 4 can
//! turn journaling on by filling in their bodies — without touching a single
//! call site. The wrappers are deliberately given their final, Phase-4-ready
//! signatures here:
//!
//! - [`get_write_access`] — about to modify an existing metadata block.
//! - [`get_create_access`] — about to populate a freshly allocated metadata
//!   block (extent index/leaf blocks, new directory blocks, new bitmaps).
//!   Missing this fourth wrapper in the first cut would force every
//!   "newly created metadata block" path to be retrofitted when Phase 4 lands.
//! - [`dirty_metadata`] — the metadata block has been modified.
//! - [`forget`] — a previously journaled metadata block is being freed (the
//!   sole insertion point for Phase 7 revoke records).
//!
//! # No-op contract (must hold for Phase 4 to slot in cleanly)
//!
//! In Phase 2 these wrappers are no-ops; the real persistence is ext2-style:
//! metadata objects carry a [`Dirty`](super::utils::Dirty) flag and are written
//! back by `sync`, with synchronous flushing happening **only** on
//! `fsync`/`fdatasync`. Callers must therefore **never assume that
//! [`dirty_metadata`] makes a block persistent**; it merely marks it for
//! writeback. Implementing it as "write through to the device immediately"
//! would bake in a flush-timing assumption that Phase 4's ordered-mode journal
//! breaks.
//!
//! Note (deviation, see `ext4_rebuild_report.md` §12): the report sketches a
//! `MetaBuffer` handle that owns the raw metadata block bytes. Phase 2 instead
//! reuses ext2's proven typed-and-dirty-tracked metadata (`Dirty<IdBitmap>`,
//! `Dirty<BlockGroupDesc>`, the inode-table page cache), and identifies the
//! affected block to these wrappers by its block number. Phase 4 introduces the
//! buffer-based journaling the block number is a handle for.

use core::sync::atomic::{AtomicU32, Ordering};

use ostd::sync::RwMutexWriteGuard;

use self::format::{JournalSuperblock, RawJournalSuperblock};
use self::transaction::{Handle, Transaction};
use super::{
    feature::FeatureCompatSet,
    fs::{Ext4, JOURNAL_INO},
    inode,
    prelude::*,
};

mod commit;
mod format;
mod transaction;

/// Journal transaction id (jbd2 `tid_t`).
pub(super) type Tid = u32;

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
#[cfg_attr(not(ktest), expect(dead_code))]
pub(super) fn load_geometry(fs: &Arc<Ext4>) -> Result<Option<JournalGeometry>> {
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

    let block_map = inode::map_all_blocks(fs.this(), *desc.raw_block(), desc.sector_count(), nblocks)?;

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

/// The in-memory journal: the parsed geometry plus the running-transaction
/// state (jbd2 `journal_t`).
///
/// Later tasks add the committed-tid counter, wait queues, the commit thread,
/// and the checkpoint machinery. Task 2a holds only enough to open, size, and
/// close transactions.
///
/// Constructed by [`Journal::new`], which a later task calls from
/// [`Ext4::open`]; for now it is reachable only from tests. The transaction
/// lifecycle functions in [`transaction`] reference it, so the type itself
/// counts as used in non-ktest builds even though nothing constructs it there
/// yet (`Journal::new` stays gated `dead_code`).
pub(super) struct Journal {
    /// The parsed on-disk geometry (the log block map + journal superblock).
    geometry: JournalGeometry,
    /// The running-transaction state, guarded for the lifecycle operations.
    state: RwMutex<JournalState>,
    /// The id of the most recently committed transaction (jbd2
    /// `journal_t.j_commit_sequence`).
    ///
    /// An atomic, not part of [`JournalState`], so a later task's
    /// `fsync`/`log_wait_commit` can observe commit progress without contending
    /// on the state lock.
    committed_tid: AtomicU32,
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

#[cfg_attr(not(ktest), expect(dead_code))]
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
    pub(super) fn new(geometry: JournalGeometry) -> Arc<Self> {
        let next_tid = geometry.sequence();
        let head = geometry.first();
        let tail_block = geometry.start();
        let tail_tid = geometry.sequence();
        Arc::new(Self {
            state: RwMutex::new(JournalState {
                running: None,
                next_tid,
                head,
                tail_block,
                tail_tid,
            }),
            committed_tid: AtomicU32::new(geometry.sequence().wrapping_sub(1)),
            geometry,
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
        let log_bound =
            (self.geometry.maxlen() - self.geometry.first()).saturating_sub(2) as usize;
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

    /// The id of the most recently committed transaction (jbd2
    /// `journal_t.j_commit_sequence`).
    ///
    /// Read with `Acquire` so a later task's `fsync`/`log_wait_commit` sees the
    /// commit pipeline's writes to this counter without the state lock.
    pub(super) fn committed_tid(&self) -> Tid {
        self.committed_tid.load(Ordering::Acquire)
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

/// Records intent to modify an existing metadata block. Phase 2: no-op.
pub(super) fn get_write_access(
    _handle: Option<&Handle>,
    _blocknr: Ext4Bid,
    _trigger: TriggerType,
) -> Result<()> {
    Ok(())
}

/// Records intent to populate a freshly allocated metadata block. Phase 2: no-op.
pub(super) fn get_create_access(
    _handle: Option<&Handle>,
    _blocknr: Ext4Bid,
    _trigger: TriggerType,
) -> Result<()> {
    Ok(())
}

/// Marks a metadata block as modified. Phase 2: no-op (writeback is driven by
/// the block's own `Dirty` flag; see the module-level no-op contract).
pub(super) fn dirty_metadata(
    _handle: Option<&Handle>,
    _blocknr: Ext4Bid,
    _trigger: TriggerType,
) -> Result<()> {
    Ok(())
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

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::{
        super::test_utils::{make_multi_block_file_inode, Ext4FixtureBuilder},
        format::{Be32, RawJournalHeader, BLOCKTYPE_SUPERBLOCK_V2, JBD2_MAGIC},
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
        assert_eq!(geo.log_block_to_physical(0), Some(JOURNAL_START_BLOCK as Ext4Bid));
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
}
