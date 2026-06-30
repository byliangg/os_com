// SPDX-License-Identifier: MPL-2.0

//! Wires ext4 types into the VFS trait interfaces (`FileSystem`, `FileOps`,
//! `Inode`). Phase 1 is read-only: read paths translate to ext4-internal
//! operations, and every mutating entry point returns `EROFS`.

mod fs;
mod inode;
