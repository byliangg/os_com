// SPDX-License-Identifier: MPL-2.0

// NOTE: the safe-rewrite ext4 core lives in the `core/` subdir. `mod core` shadows the
// `core` crate within this module, so DO NOT add `use super::*` (nor `use crate::fs::ext4::core`)
// in sibling modules (`fs.rs`/`inode.rs`) — a glob would silently hijack their bare `core::`
// paths (`core::sync`, `core::fmt`, `core::result`, ...). Reference the crate as `::core::` if needed.
mod core;
// Phase 6 Task 0: production adapters bridging the integration seam (device / overlay bridge /
// JBD2 runtime) to `core/`'s traits. Built but NOT yet wired into any production call site
// (ext4_rs stays live); Task 1+ re-points the orchestration at these.
mod core_adapter;
mod fs;
mod inode;

use fs::Ext4Type;

pub(super) fn init() {
    super::registry::register(&Ext4Type).unwrap();
}
