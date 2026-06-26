// SPDX-License-Identifier: MPL-2.0

// NOTE: the safe-rewrite ext4 core lives in the `core/` subdir. `mod core` shadows the
// `core` crate within this module, so DO NOT add `use super::*` (nor `use crate::fs::ext4::core`)
// in sibling modules (`fs.rs`/`inode.rs`) — a glob would silently hijack their bare `core::`
// paths (`core::sync`, `core::fmt`, `core::result`, ...). Reference the crate as `::core::` if needed.
mod core;
// Phase 6: production adapters bridging the integration seam (device / overlay bridge /
// JBD2 runtime) to `core/`'s traits. These are the live production bridges wiring `fs.rs`
// to core/ for all read, journaled-write, and namespace operations.
mod core_adapter;
mod fs;
mod inode;
// Phase 8 move-only split: leaf modules extracted verbatim from `fs.rs` (no behavior change).
mod caches;
mod device_adapter;
mod page_cache;
mod profile;
// Phase 8 move-only split: the journaled-write / read execution hub (JournaledOp, runtime lock,
// JBD2 handle/commit/checkpoint state machine, run_io_*/run_journaled_* wrappers).
mod run;
// Phase 6 Task 5b: in-tree replacements for the integration-layer types that used to be imported
// from the third-party `ext4_rs` crate (mode bits, root inode, block size, the `Simple*` DTOs and
// the metadata-writer seam trait). Byte-identical to the `ext4_rs` items they replace.
mod types;
// Phase 6 Task 2: integration-layer JBD2 commit/checkpoint driver re-derived over the safe `core/`
// journal (core's runtime is deliberately thinner — checkpoint_list / last_committed_tid / rotation
// / overlay live here). Drives the journaled WRITE path's commit + the single sync barrier.
mod journal_driver;

use fs::Ext4Type;

pub(super) fn init() {
    super::registry::register(&Ext4Type).unwrap();
}
