// SPDX-License-Identifier: MPL-2.0
//! Phase 6 Task 0 — production adapters: integration seam → `core/` traits.
//!
//! Phase 6 cuts the production ext4 path from third-party `ext4_rs` over to the in-tree safe
//! `core/`. `core/` is **stateless free functions over injected borrowed contexts** that depend
//! only on three caller-implemented seam traits (`core::io::BlockReader` / `core::io::BlockWriter`
//! / `core::metadata_writer::MetadataWriter`) plus injected allocators. This module supplies the
//! *production* implementations of those traits, built over the same device / overlay / journal
//! machinery the live `ext4_rs` adapters (`KernelBlockDeviceAdapter` / `JournalIoBridge` /
//! `JournalOperationMetadataWriter` in `fs.rs`) already use.
//!
//! These adapters ARE the production bridges wiring `fs.rs` to `core/` traits: the overlay device
//! reader (`CoreDeviceReader`), journaled metadata writer (`CoreMetadataWriter`), direct data
//! writer (`CoreDataWriter`), block allocator context (`CoreBlockAlloc`), and the running
//! superblock reader (`read_superblock`). All are live call sites as of Phase 6 Task 4.
//!
//! NOTE (`mod core` shadows the std `core` crate, see `mod.rs`): inside this sibling module the
//! bare path `core::` would resolve to our `super::core`. We reference our safe core as
//! `super::core::…` and the std crate as `::core::…` throughout.

use ::core::cell::RefCell;

use ostd::sync::RwMutex;

use super::core::{
    balloc::{BlockAllocator, InodeAllocCtx},
    block_map::map_block_for_read,
    extents::BlockAlloc,
    inode::{load_inode, Inode},
    io::{BlockReader, BlockWriter},
    metadata_writer::MetadataWriter,
    superblock::RawSuperblock,
    types::Ext4Fsblk,
};
use super::fs::{JournalIoBridge, KernelBlockDeviceAdapter};
use super::journal_driver::CoreJournalDriver;
use crate::prelude::*;

/// Shared, late-initialized handle to the integration-layer JBD2 driver (which wraps the **core**
/// `JournalRuntime` plus the integration-owned checkpoint_list / last_committed_tid / rotation).
///
/// Mirrors `fs.rs`'s old `Arc<RwMutex<Option<JournalRuntime>>>` (the **ext4_rs** runtime holder),
/// but holds the **core-backed** [`CoreJournalDriver`]. Mount installs the real driver here; until
/// then it is `None` and the metadata-writer adapter is a no-op recorder.
pub(super) type CoreJournalRuntimeHandle = Arc<RwMutex<Option<CoreJournalDriver>>>;

// =============================================================================================
// 1. Read seam: core `BlockReader` over the overlay bridge (read-your-writes).
// =============================================================================================

/// Core read seam wired to the integration overlay bridge.
///
/// Core read paths only depend on [`BlockReader::read_at`] (byte-offset read, past-EOF zero-fill).
/// We route through [`JournalIoBridge::read_offset_into`] (via the `Ext4BlockDevice` trait), which
/// reads the device home block through `KernelBlockDeviceAdapter` **and** overlays uncommitted
/// journaled metadata — so core reads see read-your-writes exactly like the live `ext4_rs` read
/// path. The byte-offset / past-EOF-zero-fill semantics of `read_offset_into` match
/// `BlockReader::read_at` one-for-one.
// Phase 6 Task 1+ wires this into the read-path cutover.
#[allow(dead_code)]
pub(super) struct CoreDeviceReader {
    bridge: Arc<JournalIoBridge>,
}

#[allow(dead_code)]
impl CoreDeviceReader {
    pub(super) fn new(bridge: Arc<JournalIoBridge>) -> Self {
        Self { bridge }
    }
}

impl BlockReader for CoreDeviceReader {
    fn read_at(&self, off: usize, out: &mut [u8]) {
        // `read_offset_into` (home-read + overlay-merge) is an inherent method on the bridge
        // (Task 5b converted the former `ext4_rs::BlockDevice` impl to an inherent one).
        self.bridge.read_offset_into(off, out);
    }
}

/// Core read seam wired straight to the device adapter, **bypassing the overlay bridge**.
///
/// Mount-time JBD2 recovery reads the journal **log** blocks directly off the device — there is no
/// in-memory overlay yet (the driver's checkpoint_list / running transaction are empty at mount), and
/// PARITY with the old path requires it: ext4_rs `replay_mount_jbd2_journal` ran
/// `journal.recover(&self.adapter)` over the RAW `KernelBlockDeviceAdapter`, not the
/// `JournalIoBridge`. We mirror that exactly with a raw reader so recovery never sees a stale overlay
/// image. (At mount the overlay is empty, so this is byte-identical to `CoreDeviceReader`, but the
/// raw reader is the precise parity match and is overlay-independent.)
pub(super) struct CoreRawDeviceReader {
    adapter: Arc<KernelBlockDeviceAdapter>,
}

impl CoreRawDeviceReader {
    pub(super) fn new(adapter: Arc<KernelBlockDeviceAdapter>) -> Self {
        Self { adapter }
    }
}

impl BlockReader for CoreRawDeviceReader {
    fn read_at(&self, off: usize, out: &mut [u8]) {
        self.adapter.read_offset_into(off, out);
    }
}

// =============================================================================================
// 2. Data-write seam: core `BlockWriter` that BYPASSES the journal.
// =============================================================================================

/// Core data-block write seam wired straight to the device adapter, **bypassing JBD2**.
///
/// `BlockWriter::write_at` is the non-journaled file *data* write (and the recovery raw home
/// write). It must NOT go through the journal — it mirrors `ext4_rs write_at` writing data blocks
/// directly via `block_device.write_offset(...)`. We therefore target `KernelBlockDeviceAdapter`
/// directly (NOT the bridge's metadata path), the same chokepoint the O_DIRECT data bios use.
// Phase 6 Task 1+ wires this into the journaled-write cutover (data blocks) and Task 4 (recovery).
#[allow(dead_code)]
pub(super) struct CoreDataWriter {
    adapter: Arc<KernelBlockDeviceAdapter>,
}

#[allow(dead_code)]
impl CoreDataWriter {
    pub(super) fn new(adapter: Arc<KernelBlockDeviceAdapter>) -> Self {
        Self { adapter }
    }
}

impl BlockWriter for CoreDataWriter {
    fn write_at(&self, off: usize, data: &[u8]) {
        self.adapter.write_offset(off, data);
    }
}

// =============================================================================================
// 3. Metadata-write seam: core `MetadataWriter` (block-number + u64 handle) → JBD2 transaction.
// =============================================================================================

/// Core metadata write-back seam: records a full-block metadata image into the active JBD2
/// transaction of the **core** runtime, deferring the home write (journaled metadata).
///
/// **Contract difference vs ext4_rs (the whole point of this adapter being a rewrite, not a
/// rename):**
/// - ext4_rs's `MetadataWriter` / `JournalOperationMetadataWriter` take a **byte offset** plus an
///   `Option<u64>` handle, and the bridge captures the device *pre-image* of the home block before
///   recording (`record_metadata_write_for_handle(handle_id, offset, data, |block_nr| read_home)`).
/// - core's [`MetadataWriter::write_metadata_for_handle`] takes a **block number** (`Ext4Fsblk`)
///   plus a **non-Option `u64`** handle, and the caller already passes the **full block image**.
///   So core's runtime `record_metadata_write(handle_id, block, full_image)` needs **no pre-image
///   read** — it stores `full_image` directly into the transaction's `BTreeMap<block_nr, buffer>`.
///
/// This adapter therefore does the trivial mapping `(handle_id, block, data) → runtime
/// .record_metadata_write(handle_id, block, data)`. The block→byte-offset conversion that ext4_rs
/// needed (`block * block_size`) is **not** required on this side because core's runtime keys by
/// block number directly; the conversion only matters for the data/home seams above (which are
/// byte-offset). The deferred-home-write suppression (`should_defer_metadata_write`) and the
/// overlay read are NOT core-runtime responsibilities (core's runtime is the thin in-memory state
/// machine) — they stay in the integration layer's `JournalIoBridge`, unchanged.
// Phase 6 Task 2: replaces the old `JournalOperationMetadataWriter` in the journaled write path.
pub(super) struct CoreMetadataWriter {
    runtime: CoreJournalRuntimeHandle,
    handle_id: u64,
}

impl CoreMetadataWriter {
    pub(super) fn new(runtime: CoreJournalRuntimeHandle, handle_id: u64) -> Self {
        Self { runtime, handle_id }
    }
}

impl MetadataWriter for CoreMetadataWriter {
    fn write_metadata_for_handle(
        &self,
        _handle_id: u64,
        block: Ext4Fsblk,
        data: &[u8],
    ) -> Result<()> {
        // Record the full-block image into the active core JBD2 transaction (deferred home write).
        //
        // `_handle_id` (the value core passes) is **always 0** — `core/` is journal-agnostic and
        // hard-codes 0 at every `write_metadata_for_handle` call (`write_back_inode`, the bitmap /
        // group-descriptor / extent-tree writes, the per-op allocators). The *real* JBD2 handle is
        // the one bound to this adapter at `ext4_with_operation_context` time (`self.handle_id`), so
        // we substitute it. This is the integration seam that binds core's journal-agnostic metadata
        // writes to the active transaction — equivalent to ext4_rs's `JournalOperationMetadataWriter`
        // carrying the active `handle_id`.
        //
        // `record_metadata_write` is a no-op when the runtime is absent / the handle is unknown,
        // matching the live bridge's behavior when no handle is active.
        if let Some(driver) = self.runtime.write().as_mut() {
            driver.record_metadata_write(self.handle_id, block, data);
        }
        Ok(())
    }
}

/// Core metadata write seam for the **commit emitter + checkpoint + journal-SB store**: writes the
/// journal-area block **straight to the device** (block number → `block * block_size` byte offset),
/// bypassing the overlay/defer the active-op [`CoreMetadataWriter`] uses.
///
/// The journal commit itself is *the* durable write — `core::commit::write_commit_plan` lays the
/// descriptor / payload / commit block / journal SB into the journal's home blocks and must reach
/// the device (the single ordered-mode `sync()` barrier inside `write_commit_plan` then orders them
/// before the commit block). Likewise checkpoint home-writes + the checkpoint journal-SB store go
/// straight to the device. This is exactly what ext4_rs's commit/checkpoint did via
/// `block_device.write_offset(...)`. Mirrors the differential harness `DirectMetadataWriter`, but
/// over the production `KernelBlockDeviceAdapter` instead of `MemDisk`.
// Phase 6 Task 2 wires this into `CoreJournalDriver::write_commit_plan_with_hook` / `checkpoint`.
pub(super) struct CoreCommitMetadataWriter {
    adapter: Arc<KernelBlockDeviceAdapter>,
    block_size: usize,
}

impl CoreCommitMetadataWriter {
    pub(super) fn new(adapter: Arc<KernelBlockDeviceAdapter>, block_size: usize) -> Self {
        Self {
            adapter,
            block_size,
        }
    }
}

impl MetadataWriter for CoreCommitMetadataWriter {
    fn write_metadata_for_handle(
        &self,
        _handle_id: u64,
        block: Ext4Fsblk,
        data: &[u8],
    ) -> Result<()> {
        let off = (block as usize).saturating_mul(self.block_size);
        self.adapter.write_offset(off, data);
        Ok(())
    }
}

// =============================================================================================
// 4. Block-allocation seam: core `BlockAlloc` over `BlockAllocator` + `InodeAllocCtx`.
// =============================================================================================

/// Adapts the Phase-2 [`BlockAllocator`] + [`InodeAllocCtx`] into the write-half [`BlockAlloc`]
/// trait, syncing `i_blocks` back onto the [`Inode`] after every alloc / free.
///
/// Copies the harness `CoreDirAllocAdapter` / `NamespaceBlockAlloc` shape verbatim: tree-block and
/// data-block allocation must back onto the **same** allocator instance (one running superblock +
/// bitmap state) so free counts / bitmaps stay consistent — hence a single struct holding one
/// `BlockAllocator` + one `InodeAllocCtx`, exposing all three entry points.
///
/// `R` / `W` are the core seam impls above (`CoreDeviceReader` / `CoreMetadataWriter`) at the
/// production call site; left generic so the same adapter serves the harness types too.
// Phase 6 Task 2 wires this into the journaled-write `apply` closure (file write/alloc/truncate).
#[allow(dead_code)]
pub(super) struct CoreBlockAlloc<'a, R: BlockReader, W: MetadataWriter> {
    alloc: BlockAllocator<'a, R, W>,
    ictx: InodeAllocCtx,
}

#[allow(dead_code)]
impl<'a, R: BlockReader, W: MetadataWriter> CoreBlockAlloc<'a, R, W> {
    /// Build over an allocator (carrying the running superblock) and an initial `i_blocks` seed
    /// (take it from `inode.blocks_count()` at the call site, as the harness does).
    pub(super) fn new(alloc: BlockAllocator<'a, R, W>, i_blocks: u64) -> Self {
        Self {
            alloc,
            ictx: InodeAllocCtx::new(i_blocks),
        }
    }

    /// Current running superblock (free-blocks authority); the orchestration syncs this back into
    /// its authoritative SB copy after each operation (see `read_running_superblock`).
    pub(super) fn superblock(&self) -> &RawSuperblock {
        self.alloc.superblock()
    }
}

impl<'a, R: BlockReader, W: MetadataWriter> BlockAlloc for CoreBlockAlloc<'a, R, W> {
    fn alloc_one(&mut self, inode: &mut Inode) -> Result<Ext4Fsblk> {
        let blk = self.alloc.balloc_alloc_block(&mut self.ictx, None)?;
        inode.set_blocks_count(self.ictx.i_blocks());
        Ok(blk)
    }

    fn alloc_batch(
        &mut self,
        inode: &mut Inode,
        start_bgid: &mut u32,
        count: usize,
    ) -> Result<Vec<Ext4Fsblk>> {
        let v = self
            .alloc
            .balloc_alloc_block_batch(&mut self.ictx, start_bgid, count)?;
        inode.set_blocks_count(self.ictx.i_blocks());
        Ok(v)
    }

    fn free_blocks(&mut self, inode: &mut Inode, start: Ext4Fsblk, count: u32) {
        self.alloc.balloc_free_blocks(&mut self.ictx, start, count);
        inode.set_blocks_count(self.ictx.i_blocks());
    }
}

// =============================================================================================
// 5. Superblock parse + running copy at mount.
// =============================================================================================

/// Parse a [`RawSuperblock`] from the device via the core read seam (offset 1024, 1024 bytes).
// Phase 6 Task 4 calls this from mount (after `verify_ext4_superblock`).
#[allow(dead_code)]
pub(super) fn read_superblock(reader: &dyn BlockReader) -> RawSuperblock {
    let mut sb_buf = vec![0u8; 1024];
    reader.read_at(1024, sb_buf.as_mut_slice());
    RawSuperblock::from_bytes(&sb_buf)
}

// =============================================================================================
// 6. Journal inode physical-block resolution at mount (Phase 6 Task 4).
// =============================================================================================

/// Resolve the journal inode's physical (fs) block vector via **core**, replacing the ext4_rs
/// `Jbd2Journal::load` → `JournalDevice::load` path that the mount used to build `physical_blocks`.
///
/// PARITY: ext4_rs `JournalDevice::load` (`jbd2/device.rs:15-47`):
/// - journal inode number = `sb.journal_inode_number()` (`s_journal_inum`); 0 → error.
/// - `inode_size_bytes = sb.inode_size_file(&inode)`; for a **regular file** (the journal inode is
///   always `S_IFREG`) this folds `size_hi<<32` — identical to core `Inode::size()` for that inode
///   (core `RawInode::size()` always folds size_hi; the journal inode being a regular file makes the
///   two agree, see the subagent grounding). 0 → error.
/// - `logical_blocks = ceil(inode_size_bytes / block_size)`; must be `>= 2` (a JBD2 journal has at
///   least a superblock + one log block) else error.
/// - for `lblock in 0..logical_blocks`: `pblock = get_pblock_idx(inode, lblock)` (extent / legacy
///   dispatch). We use core `map_block_for_read`, which dispatches on `inode.uses_extents()`
///   identically to ext4_rs `get_pblock_idx_inner` (the same predicate the live read path uses); a
///   hole (`None`) inside the journal file is invalid → EINVAL, exactly as ext4_rs would fail to map.
///
/// Returns `(physical_blocks, fs_block_size)` so the caller can build the core JBD2 driver
/// (`CoreJournalDriver::from_physical_blocks`, which then reads the on-disk journal SB through the
/// same reader and seeds the running ring geometry — equivalent to ext4_rs `JournalSuperblockState`
/// + `JournalSpace::from_superblock`).
pub(super) fn resolve_journal_physical_blocks(
    reader: &dyn BlockReader,
    sb: &RawSuperblock,
) -> Result<(Vec<Ext4Fsblk>, usize)> {
    let journal_inode = sb.journal_inode_number();
    if journal_inode == 0 {
        return Err(Error::with_message(Errno::EINVAL, "journal inode is zero"));
    }
    let inode = load_inode(reader, sb, journal_inode)?;
    let block_size = sb.block_size();
    let block_size_u64 = block_size as u64;
    let inode_size_bytes = inode.size();
    if inode_size_bytes == 0 {
        return Err(Error::with_message(Errno::EINVAL, "journal inode is empty"));
    }
    let logical_blocks_u64 = inode_size_bytes
        .checked_add(block_size_u64 - 1)
        .ok_or_else(|| Error::with_message(Errno::EINVAL, "journal inode size overflow"))?
        / block_size_u64;
    let logical_blocks = u32::try_from(logical_blocks_u64)
        .map_err(|_| Error::with_message(Errno::EINVAL, "journal inode too large"))?;
    if logical_blocks < 2 {
        return Err(Error::with_message(Errno::EINVAL, "journal inode too small"));
    }

    let mut physical_blocks = Vec::with_capacity(logical_blocks as usize);
    for lblock in 0..logical_blocks {
        let pblock = map_block_for_read(reader, sb, &inode, lblock)?
            .map(|(pblock, _unwritten)| pblock)
            .ok_or_else(|| Error::with_message(Errno::EINVAL, "journal inode has a hole"))?;
        physical_blocks.push(pblock);
    }
    Ok((physical_blocks, block_size))
}

/// Raw home-block writer for mount-time JBD2 recovery: writes straight to the device adapter
/// (byte offset), **bypassing** both the journal `MetadataWriter` and the overlay deferral.
///
/// JBD2 recovery REPLAYS committed metadata straight to its home location (`block_nr * block_size`)
/// and stores the reset journal SB to journal logical block 0 — it does **not** re-journal anything
/// (PARITY: ext4_rs `recover` writes home via `block_device.write_offset`, see
/// `core/journal/recovery.rs` `RecoverCtx::writer` doc). This is exactly [`CoreDataWriter`]'s
/// behavior (data write straight to the adapter), but named for the recovery seam so the mount path
/// reads clearly. We reuse [`CoreDataWriter`] under the hood.
pub(super) type CoreRecoveryWriter = CoreDataWriter;
