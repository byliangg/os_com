// SPDX-License-Identifier: MPL-2.0

//! Ext4 filesystem implementation (work in progress).
//!
//! A writable, journaled ext4 built as a sibling to `ext2`, mirroring its
//! layering and visibility discipline. Implemented so far: read-write mount;
//! extent-mapped file I/O (extent trees up to depth 2, Unwritten-first
//! allocation); the full directory namespace (create/unlink/rename/link/mknod/
//! symlink, htree reads with degrade-on-insert); JBD2 journaling (ordered-data,
//! SCAN/REPLAY recovery, orphan list, checkpoint); and the `metadata_csum`,
//! `64bit`, and `flex_bg` features. Full JBD2 (revoke apply, journal checksums,
//! group commit) is Phase 7 and performance work is Phase 9.
//!
//! The design and staged plan live in the project workspace under `stages/`;
//! the authoritative technical scheme is `ext4_rebuild_report.md`.

// Set this module's log prefix for `ostd::log`.
macro_rules! __log_prefix {
    () => {
        "ext4: "
    };
}

pub use fs::Ext4;
pub use inode::{FilePerm, Inode};

use self::fs_type::Ext4Type;
use crate::fs::vfs::registry;

mod block_group;
mod checksum;
mod feature;
mod fs;
mod fs_type;
mod impl_for_vfs;
mod inode;
mod journal;
mod prelude;
mod super_block;
mod utils;

#[cfg(ktest)]
mod test_utils;

/// Registers the ext4 filesystem type with the VFS registry.
pub(super) fn init() {
    registry::register(&Ext4Type).unwrap();
}
