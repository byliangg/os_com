// SPDX-License-Identifier: MPL-2.0

// Phase 8 move-only split: the VFS-trait impls folded out of `fs.rs` / `inode.rs`, mirroring
// `kernel/src/fs/ext2/impl_for_vfs/`. `fs.rs` holds `impl FileSystem for Ext4Fs`; `inode.rs` holds
// the `Ext4Inode` integration type and its `impl InodeIo` / `impl Inode`. No behavior change.
mod fs;
mod inode;

// Re-export the `Ext4Inode` integration type so the rest of the `ext4` module subtree
// (`fs.rs::make_inode`) keeps resolving it after the move (was `super::inode::Ext4Inode`).
pub(super) use inode::Ext4Inode;
