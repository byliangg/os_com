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
//! **Task 0 only BUILDS the bridge — it does NOT cut over.** None of these adapters is wired into
//! a production call site yet (`run_journaled_ext4`, the `.ext4_*()` methods and mount all stay on
//! `ext4_rs`). They are `#[allow(dead_code)]` until Task 1+ re-points the orchestration at them.
//!
//! Shape is copied from the differential harness (`core/diff_harness.rs`): `DirectMetadataWriter`,
//! `CoreDirAllocAdapter` / `NamespaceBlockAlloc`, and `read_sb` are the templates — production
//! builds the same constructs from the real device instead of `MemDisk`.
//!
//! NOTE (`mod core` shadows the std `core` crate, see `mod.rs`): inside this sibling module the
//! bare path `core::` would resolve to our `super::core`. We reference our safe core as
//! `super::core::…` and the std crate as `::core::…` throughout.

use ::core::cell::RefCell;

use ostd::sync::RwMutex;

use super::core::{
    balloc::{BlockAllocator, InodeAllocCtx},
    extents::BlockAlloc,
    inode::Inode,
    io::{BlockReader, BlockWriter},
    journal::transaction::JournalRuntime as CoreJournalRuntime,
    metadata_writer::MetadataWriter,
    superblock::RawSuperblock,
    types::Ext4Fsblk,
};
use super::fs::{JournalIoBridge, KernelBlockDeviceAdapter};
use crate::prelude::*;

/// Shared, late-initialized handle to the safe-core JBD2 runtime.
///
/// Mirrors `fs.rs`'s `Arc<RwMutex<Option<JournalRuntime>>>` (the **ext4_rs** runtime holder), but
/// holds the **core** `JournalRuntime`. Task 2 installs the real runtime here when journaled-write
/// cutover lands; until then it is `None` and the metadata-writer adapter is a no-op recorder.
// Phase 6 Task 1+ wires this into the journaled-write orchestration.
pub(super) type CoreJournalRuntimeHandle = Arc<RwMutex<Option<CoreJournalRuntime>>>;

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
        // `Ext4BlockDevice` (== ext4_rs `BlockDevice`) provides `read_offset_into`, which is
        // home-read + overlay-merge. Bring the trait into scope locally to call it.
        use ext4_rs::BlockDevice as Ext4BlockDevice;
        self.bridge.read_offset_into(off, out);
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
        use ext4_rs::BlockDevice as Ext4BlockDevice;
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
// Phase 6 Task 2 wires this into `run_journaled_ext4` (replaces `JournalOperationMetadataWriter`).
#[allow(dead_code)]
pub(super) struct CoreMetadataWriter {
    runtime: CoreJournalRuntimeHandle,
    handle_id: u64,
}

#[allow(dead_code)]
impl CoreMetadataWriter {
    pub(super) fn new(runtime: CoreJournalRuntimeHandle, handle_id: u64) -> Self {
        Self { runtime, handle_id }
    }
}

impl MetadataWriter for CoreMetadataWriter {
    fn write_metadata_for_handle(
        &self,
        handle_id: u64,
        block: Ext4Fsblk,
        data: &[u8],
    ) -> Result<()> {
        // Record the full-block image into the active core JBD2 transaction (deferred home write).
        // `record_metadata_write` is a no-op when the runtime is absent / the handle is unknown,
        // matching the live bridge's behavior when no handle is active.
        if let Some(runtime) = self.runtime.write().as_mut() {
            runtime.record_metadata_write(handle_id, block, data);
        }
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

/// A running copy of the on-disk superblock, mirroring the harness `read_sb` template.
///
/// The on-disk superblock lives at byte offset 1024 (the first 1024 bytes after the boot block),
/// independent of block size. Core's allocators each carry a private running `RawSuperblock`
/// (free-blocks / free-inodes authority); the orchestration holds one authoritative copy here,
/// re-seeds each per-operation allocator from it, and syncs the allocator's running SB back after
/// each operation (the "single authoritative super_block" pattern from `NamespaceCtx`). Wrapped in
/// a `RefCell` so the (later) single-threaded-under-correctness-lock orchestration can mutate it.
// Phase 6 Task 2/3 maintains this running SB across journaled operations.
#[allow(dead_code)]
pub(super) struct RunningSuperblock {
    sb: RefCell<RawSuperblock>,
}

#[allow(dead_code)]
impl RunningSuperblock {
    /// Parse the superblock from `reader` at mount (offset 1024, 1024 bytes), holding the running
    /// copy. Mirrors harness `read_sb`.
    pub(super) fn parse(reader: &dyn BlockReader) -> Self {
        Self {
            sb: RefCell::new(read_superblock(reader)),
        }
    }

    /// Snapshot the current running superblock (allocators are seeded from this).
    pub(super) fn snapshot(&self) -> RawSuperblock {
        *self.sb.borrow()
    }

    /// Replace the running superblock with the allocator's post-operation running SB (sync-back).
    pub(super) fn store(&self, sb: RawSuperblock) {
        *self.sb.borrow_mut() = sb;
    }
}

/// Parse a [`RawSuperblock`] from the device via the core read seam (offset 1024, 1024 bytes).
/// Mirrors the differential harness `read_sb`; the running copy holder is [`RunningSuperblock`].
// Phase 6 Task 4 calls this from mount (after `verify_ext4_superblock`).
#[allow(dead_code)]
pub(super) fn read_superblock(reader: &dyn BlockReader) -> RawSuperblock {
    let mut sb_buf = vec![0u8; 1024];
    reader.read_at(1024, sb_buf.as_mut_slice());
    RawSuperblock::from_bytes(&sb_buf)
}
