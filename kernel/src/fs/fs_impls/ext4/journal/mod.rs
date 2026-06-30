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

use super::prelude::*;

/// Journal transaction id (jbd2 `tid_t`).
pub(super) type Tid = u32;

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
