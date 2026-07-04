// SPDX-License-Identifier: MPL-2.0

//! Wires ext4 types into the VFS trait interfaces (`FileSystem`, `FileOps`,
//! `Inode`): both read and write entry points translate to the corresponding
//! ext4-internal operations.

mod fs;
mod inode;
