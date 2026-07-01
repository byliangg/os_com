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

use self::format::{JournalSuperblock, RawJournalSuperblock};
use super::{
    feature::FeatureCompatSet,
    fs::{Ext4, JOURNAL_INO},
    inode,
    prelude::*,
};

mod format;

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
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn maxlen(&self) -> u32 {
        self.superblock.maxlen()
    }

    /// Returns the first log block that holds log data (`s_first`).
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn first(&self) -> u32 {
        self.superblock.first()
    }

    /// Returns the first transaction id expected on recovery (`s_sequence`).
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn sequence(&self) -> Tid {
        self.superblock.sequence()
    }

    /// Returns the log block where recovery starts; 0 means clean (`s_start`).
    #[cfg_attr(not(ktest), expect(dead_code))]
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
    #[cfg_attr(not(ktest), expect(dead_code))]
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

/// A handle to an open journal transaction (jbd2 `handle_t`).
///
/// Phase 2 has no journal, so callers always pass `None`; the type exists only
/// to fix the wrapper signatures. Phase 4 makes it a real transaction obtained
/// from `journal_start(credits)` and threaded through the metadata wrappers in
/// inner → handle → ExtentTree lock order (report §5.2 rule 1).
pub(super) struct Handle {
    _private: (),
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
