// SPDX-License-Identifier: MPL-2.0

//! Symlink read and write for ext4 inodes.
//!
//! Ext4 distinguishes two storage strategies for symbolic link targets:
//!
//! - **Fast symlink** — targets shorter than 60 bytes are stored inline in the
//!   60-byte `i_block` area of the inode without allocating any data block. One
//!   byte is reserved for the Linux-compatible trailing NUL. Unlike a regular
//!   file or directory, a fast symlink must *not* carry the `EXTENTS` flag: its
//!   `i_block` holds raw target bytes, not an extent-tree root, so the extent
//!   reader must never parse it.
//! - **Slow symlink** — longer targets are written to an extent-mapped data
//!   block through the normal page-cache path, exactly like a small file.

use super::{
    super::{fs::Ext4, prelude::*, utils},
    FileFlags, Inode, InodeInner, InodePayload, MAX_FAST_SYMLINK_LEN, RAW_BLOCK_PTRS_LEN,
    empty_extent_root,
};

/// Inline fast-symlink target stored in the raw `i_block` byte area.
#[derive(Debug)]
pub(super) struct FastSymlinkTarget {
    block_ptrs: [u32; RAW_BLOCK_PTRS_LEN],
}

impl FastSymlinkTarget {
    pub(super) fn new(block_ptrs: [u32; RAW_BLOCK_PTRS_LEN]) -> Self {
        Self { block_ptrs }
    }

    fn new_zeros() -> Self {
        Self {
            block_ptrs: [0; RAW_BLOCK_PTRS_LEN],
        }
    }

    fn write(&mut self, target: &[u8]) {
        debug_assert!(target.len() <= MAX_FAST_SYMLINK_LEN);
        self.block_ptrs.as_mut_bytes()[..target.len()].copy_from_slice(target);
    }

    fn read(&self, len: usize) -> Vec<u8> {
        self.block_ptrs.as_bytes()[..len].to_vec()
    }

    /// Returns the raw `i_block` words so the inline target can be written back
    /// to disk (the descriptor still owns the authoritative `i_block`).
    pub(super) fn block_ptrs(&self) -> [u32; RAW_BLOCK_PTRS_LEN] {
        self.block_ptrs
    }
}

impl Inode {
    /// Reads symbolic link target bytes and decodes them as UTF-8.
    #[cfg_attr(not(ktest), expect(dead_code))] // Wired into the VFS in Task 6.
    pub(in crate::fs::fs_impls::ext4) fn read_link(&self) -> Result<String> {
        if self.type_ != InodeType::SymLink {
            return_errno!(Errno::EINVAL);
        }
        self.inner.read().read_link()
    }

    /// Writes symbolic link target bytes into either fast-inline or slow
    /// (extent-mapped) storage.
    #[cfg_attr(not(ktest), expect(dead_code))] // Wired into the VFS in Task 6.
    pub(in crate::fs::fs_impls::ext4) fn write_link(&self, target: &str) -> Result<()> {
        if self.type_ != InodeType::SymLink {
            return_errno!(Errno::EINVAL);
        }
        let target_len = target.len();
        if target_len >= BLOCK_SIZE {
            return_errno!(Errno::ENAMETOOLONG);
        }
        let fs = self.fs()?;
        let mut inner = self.inner.write();
        inner.write_link(&fs, target)?;
        inner.set_mtime_ctime(utils::now());
        Ok(())
    }
}

impl InodeInner {
    /// Returns whether this symlink uses fast (inline) storage.
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn is_fast_symlink(&self) -> bool {
        matches!(self.payload, InodePayload::FastSymlink { .. })
    }

    fn write_link(&mut self, fs: &Arc<Ext4>, target: &str) -> Result<()> {
        let target_len = target.len();

        // Linux reserves one byte in `i_block` for a trailing NUL.
        if target_len < MAX_FAST_SYMLINK_LEN {
            // Fast path: store the target inline in the `i_block` area. Free any
            // data block held by a previous slow target first.
            if let InodePayload::DataBacked { block_manager, .. } = &self.payload {
                block_manager.truncate_to_byte_len(0)?;
            }
            let mut fast_target = FastSymlinkTarget::new_zeros();
            fast_target.write(target.as_bytes());
            // The `i_block` now holds raw bytes, not an extent root: clear the
            // `EXTENTS` flag so the extent reader never parses the target.
            self.desc.remove_flags(FileFlags::EXTENTS);
            // Mirror the inline bytes into the descriptor's `i_block` so the
            // writeback path persists the target, then publish the payload.
            self.desc.set_raw_block(fast_target.block_ptrs());
            self.payload = InodePayload::FastSymlink {
                target: fast_target,
            };
        } else {
            // Slow path: write through an extent-mapped data block. A symlink
            // created by `create_inode` already arrives `DataBacked` (extent
            // flagged, size 0); only a fast→slow switch needs to rebuild it.
            if !matches!(self.payload, InodePayload::DataBacked { .. }) {
                self.payload =
                    InodePayload::new_data_backed(0, empty_extent_root(), 0, Arc::downgrade(fs));
            }
            self.prepare_write(fs, 0, target_len)?;
            let mut reader = VmReader::from(target.as_bytes()).to_fallible();
            self.page_cache()?.write(0, &mut reader)?;
        }

        self.set_file_size(target_len);
        Ok(())
    }

    fn read_link(&self) -> Result<String> {
        let link_size = self.file_size();

        if let InodePayload::FastSymlink { target } = &self.payload {
            // Exclude the Linux-compatible trailing NUL byte from the target.
            let read_len = link_size.min(MAX_FAST_SYMLINK_LEN - 1);
            let target_bytes = target.read(read_len);
            return String::from_utf8(target_bytes)
                .map_err(|_| Error::with_message(Errno::EIO, "symlink target is not valid UTF-8"));
        }

        let mut buf = vec![0u8; link_size];
        let mut writer = VmWriter::from(buf.as_mut_slice()).to_fallible();
        self.page_cache()?.read(0, &mut writer).map_err(|_| {
            Error::with_message(Errno::EIO, "failed to read symlink target from page cache")
        })?;

        String::from_utf8(buf)
            .map_err(|_| Error::with_message(Errno::EIO, "symlink target is not valid UTF-8"))
    }
}

#[cfg(ktest)]
mod tests {
    use alloc::{string::String, sync::Arc, vec};

    use aster_block::{BLOCK_SIZE, SECTOR_SIZE};
    use ostd::prelude::*;

    use super::super::{
        super::test_utils::{Ext4Fixture, Ext4FixtureBuilder, make_empty_file_inode},
        FilePerm, Inode, InodePayload, MAX_FAST_SYMLINK_LEN,
    };
    use crate::{fs::file::InodeType, time::clocks};

    const DIR_INO: u32 = 12;

    /// A fixture with both bitmaps marked and an empty directory at `DIR_INO`,
    /// ready to host `create`d symlink children (the first `create` grows the
    /// directory its first block).
    fn fixture_with_dir() -> Ext4Fixture {
        clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .with_inode_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut raw = make_empty_file_inode();
        raw.mode = 0o040755; // S_IFDIR | 0755
        raw.link_count = 2;
        f.write_raw_inode(DIR_INO, &raw);
        f
    }

    /// Creates an empty symlink inode under the fixture directory, mirroring the
    /// Task-6 VFS flow `create(SymLink)` then `write_link`.
    fn create_symlink(f: &Ext4Fixture) -> Arc<Inode> {
        let dir = f.ext4.read_inode(DIR_INO).unwrap();
        dir.create(
            "link",
            InodeType::SymLink,
            FilePerm::from_bits_truncate(0o777),
        )
        .unwrap()
    }

    /// A freshly created symlink (before `write_link`) decodes without panicking:
    /// it is extent-flagged, size 0, and not yet a fast symlink.
    #[ktest]
    fn fresh_symlink_decodes() {
        let f = fixture_with_dir();
        let link = create_symlink(&f);
        assert_eq!(link.inode_type(), InodeType::SymLink);
        assert_eq!(link.size(), 0);
        let inner = link.inner.read();
        assert!(inner.desc.is_extent_based());
        assert!(!inner.is_fast_symlink());
    }

    /// A short target (< 60 bytes) is stored inline: `read_link` round-trips it,
    /// the inode is no longer extent-based, size equals the target length, and
    /// `is_fast_symlink` is true.
    #[ktest]
    fn fast_symlink_round_trip() {
        let f = fixture_with_dir();
        let link = create_symlink(&f);

        let target = "../relative/path/target";
        assert!(target.len() < MAX_FAST_SYMLINK_LEN);
        link.write_link(target).unwrap();

        assert_eq!(link.read_link().unwrap(), target);
        assert_eq!(link.size(), target.len());
        {
            let inner = link.inner.read();
            assert!(inner.is_fast_symlink());
            // The extent flag was cleared so the reader never parses the target.
            assert!(!inner.desc.is_extent_based());
        }
        // No data block: a fast symlink consumes no sectors.
        assert_eq!(link.sector_count(), 0);

        // The inline target survives a metadata writeback + re-read from disk.
        link.sync_metadata().unwrap();
        let reread = f.ext4.read_inode(link.ino()).unwrap();
        assert!(reread.inner.read().is_fast_symlink());
        assert_eq!(reread.read_link().unwrap(), target);
    }

    /// A long target (> 60 bytes, < BLOCK_SIZE) is stored in a data block:
    /// `read_link` round-trips it, the payload stays `DataBacked`, exactly one
    /// data block is allocated, and the extent flag is kept.
    #[ktest]
    fn slow_symlink_round_trip() {
        let f = fixture_with_dir();
        let link = create_symlink(&f);

        let target = "x".repeat(200); // > MAX_FAST_SYMLINK_LEN, < BLOCK_SIZE
        assert!(target.len() >= MAX_FAST_SYMLINK_LEN && target.len() < BLOCK_SIZE);
        link.write_link(&target).unwrap();

        assert_eq!(link.read_link().unwrap(), target);
        assert_eq!(link.size(), target.len());
        {
            let inner = link.inner.read();
            assert!(!inner.is_fast_symlink());
            assert!(matches!(inner.payload, InodePayload::DataBacked { .. }));
            // The extent flag is retained for a block-backed (slow) symlink.
            assert!(inner.desc.is_extent_based());
        }
        // Exactly one data block was allocated.
        assert_eq!(link.sector_count(), (BLOCK_SIZE / SECTOR_SIZE) as u64);

        // The target survives a full sync + re-read from disk.
        link.sync_data_and_meta().unwrap();
        let reread = f.ext4.read_inode(link.ino()).unwrap();
        assert!(!reread.inner.read().is_fast_symlink());
        assert_eq!(reread.read_link().unwrap(), target);
    }

    /// `read_link`/`write_link` reject non-symlink inodes.
    #[ktest]
    fn rejects_non_symlink() {
        let f = fixture_with_dir();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();
        assert!(dir.read_link().is_err());
        assert!(dir.write_link("nope").is_err());
    }

    /// A target at or past one block is rejected.
    #[ktest]
    fn rejects_overlong_target() {
        let f = fixture_with_dir();
        let link = create_symlink(&f);
        let target = String::from_utf8(vec![b'a'; BLOCK_SIZE]).unwrap();
        assert!(link.write_link(&target).is_err());
    }
}
