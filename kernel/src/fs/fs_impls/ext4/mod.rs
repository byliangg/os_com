// SPDX-License-Identifier: MPL-2.0

//! Ext4 filesystem implementation (work in progress).
//!
//! Phase 1 scope: read-only mount, extent-based file reads, and linear
//! directory reads. The module is built as a sibling to `ext2`, mirroring its
//! layering and visibility discipline; the only core component replaced is the
//! block-mapping engine, where ext2's indirect-block tree gives way to an
//! extent reader (`inode::extent_manager`).
//!
//! The design and staged plan live in the project workspace under
//! `stages/P1_plan.md`; the authoritative technical scheme is
//! `ext4_rebuild_report.md`.

// Set this module's log prefix for `ostd::log`.
macro_rules! __log_prefix {
    () => {
        "ext4: "
    };
}

pub use fs::Ext4;
pub use inode::Inode;

use self::fs_type::Ext4Type;
use crate::fs::vfs::registry;

mod block_group;
mod feature;
mod fs;
mod fs_type;
mod impl_for_vfs;
mod inode;
mod prelude;
mod super_block;
mod utils;

#[cfg(ktest)]
mod test_utils;

/// Registers the ext4 filesystem type with the VFS registry.
pub(super) fn init() {
    registry::register(&Ext4Type).unwrap();
}
