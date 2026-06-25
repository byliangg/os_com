// SPDX-License-Identifier: MPL-2.0
//! Integration-layer ext4 types that used to be imported from the third-party `ext4_rs`
//! crate. Phase 6 Task 5b re-points the production integration layer (`fs.rs` / `inode.rs` /
//! `core_adapter.rs`) onto in-tree equivalents so `ext4_rs` can be deleted.
//!
//! Every constant / DTO / trait below is **byte-identical** to the `ext4_rs` item it replaces:
//! the mode bits, root inode number and block size are the standard ext4 on-disk values, and the
//! `Simple*` DTOs keep the exact field names / order / types so the integration call sites compile
//! and serialize unchanged. The block-device / metadata-writer seam traits keep the same method
//! signatures the integration adapters already implement.

use alloc::string::String;

/// Root directory inode number. = `ext4_rs::EXT4_ROOT_INODE` (`ROOT_INODE = 2`, consts.rs:12),
/// the fixed ext4 root inode.
pub(super) const EXT4_ROOT_INODE: u32 = 2;

/// ext4 block size in bytes. = `ext4_rs::BLOCK_SIZE` (`0x1000 = 4096`, consts.rs:3). The
/// integration layer (block cache, alignment math, fallocate) keys off the fixed 4 KiB block.
pub(super) const EXT4_BLOCK_SIZE: usize = 0x1000;

/// ext4 inode mode `S_IFMT` file-type bits. Field-for-field replacements of
/// `ext4_rs::InodeFileType::S_IF*.bits()` (ext4_defs/inode.rs:51-58) — the standard ext4 / POSIX
/// values. `InodeFileType` in `ext4_rs` is a `bitflags!` over `u16`; `.bits()` returns the raw
/// `u16`, so each constant below equals the corresponding `.bits()` exactly.
pub(super) mod mode {
    /// FIFO. = `InodeFileType::S_IFIFO.bits()` = 0x1000.
    pub(in crate::fs::ext4) const S_IFIFO: u16 = 0x1000;
    /// Character device. = `InodeFileType::S_IFCHR.bits()` = 0x2000.
    pub(in crate::fs::ext4) const S_IFCHR: u16 = 0x2000;
    /// Directory. = `InodeFileType::S_IFDIR.bits()` = 0x4000.
    pub(in crate::fs::ext4) const S_IFDIR: u16 = 0x4000;
    /// Block device. = `InodeFileType::S_IFBLK.bits()` = 0x6000.
    pub(in crate::fs::ext4) const S_IFBLK: u16 = 0x6000;
    /// Regular file. = `InodeFileType::S_IFREG.bits()` = 0x8000.
    pub(in crate::fs::ext4) const S_IFREG: u16 = 0x8000;
    /// Symbolic link. = `InodeFileType::S_IFLNK.bits()` = 0xA000.
    pub(in crate::fs::ext4) const S_IFLNK: u16 = 0xA000;
    /// Socket. = `InodeFileType::S_IFSOCK.bits()` = 0xC000.
    pub(in crate::fs::ext4) const S_IFSOCK: u16 = 0xC000;
}

/// Directory-entry DTO at the integration boundary. Field-for-field copy of
/// `ext4_rs::SimpleDirEntry` (simple_interface/mod.rs:60-65).
#[derive(Clone, Debug)]
pub(super) struct SimpleDirEntry {
    pub inode: u32,
    pub de_type: u8,
    pub name: String,
    pub next_offset: usize,
}

/// Inode-metadata DTO at the integration boundary (`stat` carrier). Field-for-field copy of
/// `ext4_rs::SimpleInodeMeta` (simple_interface/mod.rs:67-82). All 13 fields are kept to preserve
/// the byte-identical layout; `flags` is populated (`core_inode_meta`) but has no current reader,
/// exactly as when this DTO was sourced from `ext4_rs`.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // `flags` mirrors ext4_rs but is currently write-only.
pub(super) struct SimpleInodeMeta {
    pub ino: u32,
    pub mode: u16,
    pub file_type: u16,
    pub uid: u16,
    pub gid: u16,
    pub nlink: u16,
    pub size: u64,
    pub blocks: u64,
    pub atime: u32,
    pub mtime: u32,
    pub ctime: u32,
    pub rdev: u32,
    pub flags: u32,
}

/// Metadata write-back seam the integration adapters implement (`JournalIoBridge`,
/// `RawAdapterWriter`). Replaces `ext4_rs::MetadataWriter` (ext4_defs/block.rs:21-28) with the
/// identical method signatures — a **byte-offset** write plus the JBD2-handle-aware variant.
pub(super) trait DeviceMetadataWriter: Send + Sync {
    /// Write `data` to byte `offset` (no active JBD2 handle).
    fn write_metadata(&self, offset: usize, data: &[u8]);

    /// Write `data` to byte `offset`, recording it into `handle_id`'s in-memory transaction when a
    /// JBD2 handle is active. Default delegates to [`Self::write_metadata`], matching `ext4_rs`.
    /// Overridden by `JournalIoBridge` for the journaled metadata path; kept on the trait to
    /// preserve the `ext4_rs::MetadataWriter` contract even though the recovery-flag persist path
    /// calls [`Self::write_metadata`] directly.
    #[allow(dead_code)]
    fn write_metadata_for_jbd2_handle(&self, handle_id: Option<u64>, offset: usize, data: &[u8]) {
        let _ = handle_id;
        self.write_metadata(offset, data);
    }
}
