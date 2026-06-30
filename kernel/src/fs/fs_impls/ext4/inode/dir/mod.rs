// SPDX-License-Identifier: MPL-2.0

//! Ext4 linear directory read operations: `lookup` and `readdir`.
//!
//! A directory inode is page-cache-backed like a regular file; these methods
//! read its data blocks through the page cache and parse linear directory
//! entries. The htree index (Phase 6) only accelerates lookup; `readdir`
//! always walks blocks in physical order.

mod dir_entry;

use self::dir_entry::{DOT_BYTE, DOT_DOT_BYTE, DirBlockView, DirEntryFileType, DirEntryHeader};
use super::{
    super::{fs::Ext4, prelude::*},
    Inode, InodeInner,
};

/// A candidate slot found by [`InodeInner::find_dir_slot`] or freshly created
/// by [`InodeInner::grow_dir_block`], where a new entry can be written.
#[derive(Clone, Copy, Debug)]
struct DirSlotInfo {
    /// Byte offset of the slot within the directory
    /// (`block_idx * BLOCK_SIZE + offset_in_block`).
    dir_offset: usize,
    /// Current `rec_len` of the candidate slot.
    slot_rec_len: usize,
    /// Minimal occupied length of the existing entry head (0 if the slot is a
    /// free entry or a freshly grown empty block).
    used_rec_len: usize,
}

/// A located, live directory entry returned by [`InodeInner::find_entry_info`],
/// describing where it sits so it can be deleted.
#[derive(Clone, Copy, Debug)]
struct DirEntryInfo {
    /// Inode number of the located entry. Read by the unlink/rmdir path
    /// (Task 4) to fetch the child inode; the delete primitive itself only
    /// needs the offset and record length.
    #[expect(dead_code)]
    ino: Ext4Ino,
    /// Byte offset of the entry within the directory.
    dir_offset: usize,
    /// `rec_len` of the entry.
    entry_rec_len: usize,
}

impl InodeInner {
    /// Finds the inode number of the entry named `name`.
    ///
    /// Directory-index seam (Phase 6 htree): this and [`Self::find_dir_slot`]
    /// are the only linear scans; when the `INDEX` flag is set the htree index
    /// will be consulted here instead of walking blocks in physical order.
    fn find_entry_ino(&self, name: &str) -> Result<Ext4Ino> {
        if self.desc.type_() != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }
        let file_size = self.file_size();
        let name_bytes = name.as_bytes();
        let page_cache = self.page_cache()?;

        for block_idx in 0..file_size.div_ceil(BLOCK_SIZE) {
            let block = DirBlockView::from_index(page_cache, block_idx, file_size);
            let mut iter = block.iter_entries();
            while let Some((_offset, entry)) = iter.next_entry()? {
                if entry.header.ino != 0 && entry.name == name_bytes {
                    return Ok(entry.header.ino);
                }
            }
        }
        return_errno!(Errno::ENOENT)
    }

    /// Iterates entries from byte `offset`, feeding each active entry to
    /// `visitor`. Returns the number of bytes advanced.
    fn readdir_at(&self, offset: usize, visitor: &mut dyn DirentVisitor) -> Result<usize> {
        if self.desc.type_() != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }
        let size = self.file_size();
        let min_rec_len = DirEntryHeader::min_rec_len(1) as usize;
        if size < min_rec_len || offset > size - min_rec_len {
            return Ok(0);
        }

        let start_block = offset / BLOCK_SIZE;
        let mut current_offset = offset;
        let mut advanced = 0usize;
        let total_blocks = size.div_ceil(BLOCK_SIZE);
        let page_cache = self.page_cache()?;

        for block_idx in start_block..total_blocks {
            let block_offset = block_idx * BLOCK_SIZE;
            if block_offset >= size {
                break;
            }
            let block = DirBlockView::from_index(page_cache, block_idx, size);
            let mut iter = block.iter_entries();
            while let Some((entry_offset_in_block, entry)) = iter.next_entry()? {
                let entry_offset = block_offset + entry_offset_in_block;
                let rec_len = entry.header.rec_len as usize;
                let next_offset = entry_offset + rec_len;

                // Skip entries already reported before `offset`.
                if next_offset <= current_offset {
                    continue;
                }
                if entry_offset < current_offset {
                    current_offset = next_offset;
                    continue;
                }

                let ino = entry.header.ino;
                if ino != 0 {
                    let name = core::str::from_utf8(entry.name)
                        .map_err(|_| Error::with_message(Errno::EIO, "invalid dir entry name"))?;
                    let file_type = DirEntryFileType::try_from(entry.header.file_type)
                        .unwrap_or(DirEntryFileType::Unknown);
                    let inode_type = InodeType::from(file_type);
                    if visitor
                        .visit(name, ino as u64, inode_type, next_offset)
                        .is_err()
                    {
                        return Ok(current_offset - offset);
                    }
                }
                current_offset = next_offset;
            }
            advanced = current_offset - offset;
        }
        Ok(advanced)
    }

    /// Finds a reusable slot for a directory entry name of `name_len` bytes.
    ///
    /// Returns `None` when no existing block has reusable space; the caller then
    /// grows the directory. A returned slot is either a free entry (`ino == 0`)
    /// or the spare tail of a live entry that can be split.
    ///
    /// Directory-index seam (Phase 6 htree): together with
    /// [`Self::find_entry_ino`] this is the only linear scan; when the `INDEX`
    /// flag is set the htree index will pick the target leaf block here instead
    /// of walking blocks in physical order.
    fn find_dir_slot(&self, name_len: usize) -> Result<Option<DirSlotInfo>> {
        if self.desc.type_() != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let new_rec_len = DirEntryHeader::min_rec_len(name_len) as usize;
        debug_assert!(new_rec_len <= BLOCK_SIZE);

        let file_size = self.file_size();
        let data_blocks = file_size.div_ceil(BLOCK_SIZE);
        let page_cache = self.page_cache()?;

        for block_idx in 0..data_blocks {
            let block_offset = block_idx * BLOCK_SIZE;
            let block = DirBlockView::from_index(page_cache, block_idx, file_size);
            let mut iter = block.iter_entries();

            while let Some((entry_offset, header)) = iter.next_entry_header()? {
                let ino = header.ino;
                let rec_len = header.rec_len as usize;

                let used_rec_len = if ino == 0 {
                    0
                } else {
                    DirEntryHeader::min_rec_len(header.name_len as usize) as usize
                };

                // A free entry can be reused; a live entry can be split.
                if (ino == 0 && rec_len >= new_rec_len)
                    || (ino != 0 && rec_len >= used_rec_len + new_rec_len)
                {
                    return Ok(Some(DirSlotInfo {
                        dir_offset: block_offset + entry_offset,
                        slot_rec_len: rec_len,
                        used_rec_len,
                    }));
                }
            }
        }

        Ok(None)
    }

    /// Grows the directory by exactly one filesystem block and returns a slot
    /// spanning the whole new block.
    ///
    /// Unlike ext2 (which allocates an indirect-mapped block), ext4 directory
    /// data is extent-mapped: this routes through the Phase-2 extent write path
    /// (`prepare_write` → `ExtentManager::ensure_allocated`) to map and allocate
    /// the new logical block, then publishes the larger `file_size`. The journal
    /// seam for the block *allocation* is already threaded inside
    /// `ensure_allocated`; the entry bytes written here go through the page
    /// cache as data.
    ///
    /// The new block must not be left zeroed: a zero `rec_len` would make the
    /// entry iterator spin forever. It is therefore initialized as a single
    /// empty entry (`ino == 0`, `rec_len == BLOCK_SIZE`) spanning the block.
    fn grow_dir_block(&mut self, fs: &Ext4) -> Result<DirSlotInfo> {
        if self.desc.type_() != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let old_size = self.file_size();
        let new_size = old_size + BLOCK_SIZE;

        // Map and allocate the new logical block through the extent engine.
        self.prepare_write(fs, old_size, new_size)?;

        // Initialize the new block as one empty entry spanning the whole block
        // before publishing the new size, so any reader that observes the grown
        // size sees a well-formed (empty) entry chain rather than zeros.
        let init_result = (|| -> Result<()> {
            let page_cache = self.page_cache()?;
            page_cache.fill_zeros(old_size..new_size)?;
            let block = DirBlockView::create_view(page_cache, old_size, BLOCK_SIZE);
            let empty_header = DirEntryHeader {
                ino: 0,
                rec_len: (BLOCK_SIZE as u16).to_le(),
                name_len: 0,
                file_type: DirEntryFileType::Unknown as u8,
            };
            block.write_entry(0, empty_header, &[])
        })();

        if let Err(err) = init_result {
            self.rollback_write(old_size, new_size);
            return Err(err);
        }

        self.set_file_size(new_size);

        Ok(DirSlotInfo {
            dir_offset: old_size,
            slot_rec_len: BLOCK_SIZE,
            used_rec_len: 0,
        })
    }

    /// Writes a new entry into the selected slot, splitting the predecessor's
    /// `rec_len` first when reusing the spare tail of a live entry.
    fn add_entry(
        &mut self,
        slot: &DirSlotInfo,
        name: &str,
        ino: Ext4Ino,
        file_type: DirEntryFileType,
    ) -> Result<()> {
        debug_assert_ne!(ino, 0);

        let name_bytes = name.as_bytes();
        let new_rec_len = DirEntryHeader::min_rec_len(name_bytes.len()) as usize;
        debug_assert!(new_rec_len <= slot.slot_rec_len);

        let page_cache = self.page_cache()?;
        let mut entry_offset = slot.dir_offset;
        let mut entry_rec_len = slot.slot_rec_len;
        if slot.used_rec_len != 0 {
            debug_assert!(slot.used_rec_len < slot.slot_rec_len);
            // Splitting a live entry: shrink the predecessor's `rec_len` to its
            // minimal occupied length first, then write the new entry in the
            // reclaimed tail.
            let prev_view =
                DirBlockView::create_view(page_cache, slot.dir_offset, slot.slot_rec_len);
            prev_view.set_rec_len(0, slot.used_rec_len as u16)?;
            entry_offset = slot.dir_offset + slot.used_rec_len;
            entry_rec_len = slot.slot_rec_len - slot.used_rec_len;
        }

        let view = DirBlockView::create_view(page_cache, entry_offset, entry_rec_len);
        let header = DirEntryHeader {
            ino: ino.to_le(),
            rec_len: (entry_rec_len as u16).to_le(),
            name_len: name_bytes.len() as u8,
            file_type: file_type as u8,
        };
        view.write_entry(0, header, name_bytes)?;
        Ok(())
    }

    /// Inserts a new directory entry, growing the directory by one block when no
    /// existing block has a reusable slot.
    #[cfg_attr(not(ktest), expect(dead_code))]
    fn add_new_entry(
        &mut self,
        fs: &Ext4,
        name: &str,
        ino: Ext4Ino,
        file_type: DirEntryFileType,
    ) -> Result<()> {
        let slot = match self.find_dir_slot(name.len())? {
            Some(slot) => slot,
            None => self.grow_dir_block(fs)?,
        };
        self.add_entry(&slot, name, ino, file_type)
    }

    /// Locates a live entry by name, recording where it sits for deletion.
    #[cfg_attr(not(ktest), expect(dead_code))]
    fn find_entry_info(&self, name: &str) -> Result<DirEntryInfo> {
        if self.desc.type_() != InodeType::Dir {
            return_errno!(Errno::ENOTDIR);
        }

        let file_size = self.file_size();
        let name_bytes = name.as_bytes();
        let page_cache = self.page_cache()?;

        for block_idx in 0..file_size.div_ceil(BLOCK_SIZE) {
            let block_offset = block_idx * BLOCK_SIZE;
            let block = DirBlockView::from_index(page_cache, block_idx, file_size);
            let mut iter = block.iter_entries();
            while let Some((entry_offset, entry)) = iter.next_entry()? {
                let ino = entry.header.ino;
                if ino == 0 || entry.name != name_bytes {
                    continue;
                }
                return Ok(DirEntryInfo {
                    ino,
                    dir_offset: block_offset + entry_offset,
                    entry_rec_len: entry.header.rec_len as usize,
                });
            }
        }

        return_errno!(Errno::ENOENT)
    }

    /// Deletes a located entry by zeroing its inode and merging its space into
    /// the predecessor entry. The first entry in a block (always `.`) has no
    /// predecessor and is never the delete target.
    #[cfg_attr(not(ktest), expect(dead_code))]
    fn delete_entry(&mut self, target: &DirEntryInfo) -> Result<()> {
        let block_idx = target.dir_offset / BLOCK_SIZE;
        let entry_offset = target.dir_offset - block_idx * BLOCK_SIZE;

        let block = DirBlockView::from_index(self.page_cache()?, block_idx, self.file_size());
        block.delete_entry(entry_offset, target.entry_rec_len)?;
        Ok(())
    }

    /// Initializes a freshly created directory's first block with `.` and `..`.
    ///
    /// `.` points to the directory itself (`ino`) and `..` to its parent
    /// (`parent_ino`); `..` spans the rest of the block. The parent's link
    /// count bump lives in the create path (Task 3), not here.
    #[cfg_attr(not(ktest), expect(dead_code))]
    fn make_empty(&mut self, fs: &Ext4, ino: Ext4Ino, parent_ino: Ext4Ino) -> Result<()> {
        // Grow the empty directory by its first block; this maps and allocates
        // the block (rolling back on its own failure) and initializes it as one
        // empty entry spanning the block.
        let slot = self.grow_dir_block(fs)?;
        debug_assert_eq!(slot.dir_offset, 0);

        // Overwrite the empty entry with `.` (this dir) followed by `..` (the
        // parent), with `..` spanning the rest of the block. If a write fails
        // here the block stays mapped and the size published; the create path
        // (Task 4) clears the link count so `Drop` reclaims the inode.
        let dot_len = DirEntryHeader::min_rec_len(DOT_BYTE.len()) as usize;
        let page_cache = self.page_cache()?;
        let block = DirBlockView::from_index(page_cache, 0, self.file_size());

        let dot_header = DirEntryHeader {
            ino: ino.to_le(),
            rec_len: (dot_len as u16).to_le(),
            name_len: DOT_BYTE.len() as u8,
            file_type: DirEntryFileType::Dir as u8,
        };
        block.write_entry(0, dot_header, DOT_BYTE)?;

        let dot_dot_header = DirEntryHeader {
            ino: parent_ino.to_le(),
            rec_len: ((BLOCK_SIZE - dot_len) as u16).to_le(),
            name_len: DOT_DOT_BYTE.len() as u8,
            file_type: DirEntryFileType::Dir as u8,
        };
        block.write_entry(dot_len, dot_dot_header, DOT_DOT_BYTE)?;

        Ok(())
    }

    /// Returns whether this directory holds only the `.` and `..` entries.
    #[cfg_attr(not(ktest), expect(dead_code))]
    fn empty_dir(&self, self_ino: Ext4Ino) -> bool {
        if self.desc.type_() != InodeType::Dir {
            return false;
        }

        let file_size = self.file_size();
        let data_blocks = file_size.div_ceil(BLOCK_SIZE);
        let Ok(page_cache) = self.page_cache() else {
            return false;
        };

        for block_idx in 0..data_blocks {
            let block = DirBlockView::from_index(page_cache, block_idx, file_size);
            let mut iter = block.iter_entries();

            loop {
                let entry = match iter.next_entry() {
                    Ok(Some((_, entry))) => entry,
                    Ok(None) => break,
                    Err(_) => return false,
                };

                if entry.header.ino == 0 {
                    continue;
                }

                let name = entry.name;
                if name == DOT_BYTE {
                    if entry.header.ino != self_ino {
                        return false;
                    }
                    continue;
                }
                if name == DOT_DOT_BYTE {
                    continue;
                }
                return false;
            }
        }

        true
    }
}

impl Inode {
    /// Looks up a child entry by name and reads its inode.
    pub(in crate::fs::fs_impls::ext4) fn lookup(&self, name: &str) -> Result<Arc<Inode>> {
        let ino = self.inner.read().find_entry_ino(name)?;
        let fs = self
            .fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "filesystem dropped"))?;
        fs.read_inode(ino)
    }

    /// Iterates directory entries from `offset`, feeding them to `visitor`.
    pub(in crate::fs::fs_impls::ext4) fn readdir_at(
        &self,
        offset: usize,
        visitor: &mut dyn DirentVisitor,
    ) -> Result<usize> {
        self.inner.read().readdir_at(offset, visitor)
    }
}

#[cfg(ktest)]
mod tests {
    use alloc::{
        format,
        string::{String, ToString},
        vec::Vec,
    };

    use aster_block::BLOCK_SIZE;
    use ostd::prelude::*;

    use super::{
        super::super::test_utils::{
            Ext4FixtureBuilder, make_dir_block, make_dir_inode, make_file_inode,
        },
        DirEntryFileType, Inode,
    };
    use crate::{
        fs::{file::InodeType, utils::DirentVisitor},
        prelude::{Errno, Error, Result},
    };

    #[ktest]
    fn lookup_and_readdir() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();

        // A directory at ino 12 backed by data block 101 with three entries.
        let dir_block = 101u32;
        let block = make_dir_block(&[(2, ".", 2), (2, "..", 2), (11, "hello.txt", 1)]);
        f.write_data_block(dir_block, &block);
        f.write_raw_inode(12, &make_dir_inode(dir_block));
        // The looked-up file inode must exist to be read back.
        f.write_raw_inode(11, &make_file_inode(100, 0));

        let dir = f.ext4.read_inode(12).unwrap();

        let child = dir.lookup("hello.txt").unwrap();
        assert_eq!(child.ino(), 11);
        assert_eq!(child.inode_type(), InodeType::File);
        assert!(dir.lookup("nonexistent").is_err());

        let mut names: Vec<String> = Vec::new();
        dir.readdir_at(0, &mut names).unwrap();
        assert_eq!(names.len(), 3);
        assert_eq!(names[0], ".");
        assert_eq!(names[1], "..");
        assert_eq!(names[2], "hello.txt");
    }

    /// A visitor that simulates a `getdents` buffer holding `cap` entries: it
    /// records each entry, then fails (as a full user buffer would) once `cap`
    /// entries have been accepted in the current call.
    struct CappedVisitor {
        cap: usize,
        used: usize,
        names: Vec<String>,
    }

    impl DirentVisitor for CappedVisitor {
        fn visit(
            &mut self,
            name: &str,
            _ino: u64,
            _type_: InodeType,
            _offset: usize,
        ) -> Result<()> {
            if self.used >= self.cap {
                return Err(Error::with_message(Errno::EINVAL, "simulated buffer full"));
            }
            self.used += 1;
            self.names.push(name.to_string());
            Ok(())
        }
    }

    /// Drives `readdir_at` exactly the way `InodeHandle::readdir` does — advancing
    /// the directory offset by the returned byte count across multiple calls.
    /// This exercises offset resumption that the single-call test above cannot,
    /// mirroring a real mke2fs root layout (`.`, `..`, `lost+found`, files).
    #[ktest]
    fn readdir_resumes_across_getdents_calls() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let dir_block = 101u32;
        let block = make_dir_block(&[
            (2, ".", 2),
            (2, "..", 2),
            (11, "lost+found", 2),
            (12, "hello.txt", 1),
            (13, "subdir", 2),
            (15, "big.txt", 1),
        ]);
        f.write_data_block(dir_block, &block);
        f.write_raw_inode(12, &make_dir_inode(dir_block));
        let dir = f.ext4.read_inode(12).unwrap();

        let expected = [".", "..", "lost+found", "hello.txt", "subdir", "big.txt"];

        // Including one entry per call, which forces the most resumption steps.
        for cap in [1usize, 2, 3, 6] {
            let mut all: Vec<String> = Vec::new();
            let mut offset = 0usize;
            loop {
                let mut visitor = CappedVisitor {
                    cap,
                    used: 0,
                    names: Vec::new(),
                };
                let read_cnt = dir.readdir_at(offset, &mut visitor).unwrap();
                all.extend(visitor.names);
                if read_cnt == 0 {
                    break;
                }
                offset += read_cnt;
                assert!(
                    all.len() <= expected.len(),
                    "readdir over-reported (cap={cap})"
                );
            }
            assert_eq!(all, expected, "readdir mismatch at cap={cap}");
        }
    }

    use super::super::super::test_utils::{Ext4Fixture, make_empty_file_inode};
    use crate::time::clocks;

    const DIR_INO: u32 = 12;

    /// A fixture with a realistic bitmap and an empty (size 0) directory inode
    /// at `DIR_INO`, ready to be populated through the directory write path.
    fn fixture_with_empty_dir() -> Ext4Fixture {
        clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        // An empty directory: same on-disk shape as an empty file (size 0,
        // i_blocks 0, empty inline extent root) but typed as a directory.
        let mut raw = make_empty_file_inode();
        raw.mode = 0o040755; // S_IFDIR | 0755
        raw.link_count = 2;
        f.write_raw_inode(DIR_INO, &raw);
        f
    }

    /// Collects the live entry names of a directory via `readdir_at`.
    fn readdir_names(dir: &Inode) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        dir.readdir_at(0, &mut names).unwrap();
        names
    }

    /// `add_new_entry` then `find_entry_ino`/`readdir_at` see the new name, and
    /// multiple adds in one block all become visible.
    #[ktest]
    fn add_new_entries_visible() {
        let f = fixture_with_empty_dir();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        {
            let mut inner = dir.inner.write();
            inner.make_empty(&f.ext4, DIR_INO, 2).unwrap();
            inner
                .add_new_entry(&f.ext4, "alpha", 21, DirEntryFileType::File)
                .unwrap();
            inner
                .add_new_entry(&f.ext4, "beta", 22, DirEntryFileType::Dir)
                .unwrap();
            inner
                .add_new_entry(&f.ext4, "gamma", 23, DirEntryFileType::File)
                .unwrap();
        }

        // Each name resolves to the inode it was added with.
        assert_eq!(dir.inner.read().find_entry_ino("alpha").unwrap(), 21);
        assert_eq!(dir.inner.read().find_entry_ino("beta").unwrap(), 22);
        assert_eq!(dir.inner.read().find_entry_ino("gamma").unwrap(), 23);
        assert_eq!(
            dir.inner
                .read()
                .find_entry_ino("missing")
                .unwrap_err()
                .error(),
            Errno::ENOENT
        );

        // `.`/`..` plus the three names, in insertion order.
        assert_eq!(readdir_names(&dir), [".", "..", "alpha", "beta", "gamma"]);
        // Still one block; the directory did not need to grow.
        assert_eq!(dir.size(), BLOCK_SIZE);
    }

    /// An add that splits a live predecessor's slack reuses the same block.
    #[ktest]
    fn add_splits_predecessor_slack() {
        let f = fixture_with_empty_dir();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        // After `make_empty`, `..` owns all the slack to the block end. Adding a
        // name must split that slack rather than grow the directory.
        {
            let mut inner = dir.inner.write();
            inner.make_empty(&f.ext4, DIR_INO, 2).unwrap();
            inner
                .add_new_entry(&f.ext4, "split-me", 31, DirEntryFileType::File)
                .unwrap();
        }

        assert_eq!(dir.size(), BLOCK_SIZE);
        assert_eq!(dir.inner.read().find_entry_ino("split-me").unwrap(), 31);
        assert_eq!(dir.inner.read().find_entry_ino("..").unwrap(), 2);
        assert_eq!(readdir_names(&dir), [".", "..", "split-me"]);
    }

    /// Filling a block forces the next add to grow the directory; entries in
    /// both blocks are then found by `readdir_at`, and `file_size` grew by
    /// exactly one block.
    #[ktest]
    fn add_grows_directory_into_second_block() {
        let f = fixture_with_empty_dir();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        // 16-byte names => 24-byte records (min_rec_len(16) == 24). After the
        // 12+12 used by `.`/`..`, a 4 KiB block holds (4096-24)/24 = 169 such
        // records; add enough to overflow into a second block.
        let count = 200usize;
        {
            let mut inner = dir.inner.write();
            inner.make_empty(&f.ext4, DIR_INO, 2).unwrap();
            for i in 0..count {
                let name = format!("entry_file_{i:05}"); // 16 bytes
                assert_eq!(name.len(), 16);
                inner
                    .add_new_entry(&f.ext4, &name, 1000 + i as u32, DirEntryFileType::File)
                    .unwrap();
            }
        }

        // The directory grew past one block.
        assert!(dir.size() > BLOCK_SIZE, "directory did not grow");
        assert_eq!(dir.size() % BLOCK_SIZE, 0, "size not block-aligned");

        // Every name (including ones that landed in the grown block) is found.
        for i in 0..count {
            let name = format!("entry_file_{i:05}");
            assert_eq!(
                dir.inner.read().find_entry_ino(&name).unwrap(),
                1000 + i as u32
            );
        }
        // readdir sees `.`/`..` plus all names across both blocks.
        assert_eq!(readdir_names(&dir).len(), count + 2);
    }

    /// `delete_entry` removes a name, merges its space into the predecessor, and
    /// the reclaimed slot can hold a same-or-smaller name again.
    #[ktest]
    fn delete_entry_merges_and_reclaims() {
        let f = fixture_with_empty_dir();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        {
            let mut inner = dir.inner.write();
            inner.make_empty(&f.ext4, DIR_INO, 2).unwrap();
            inner
                .add_new_entry(&f.ext4, "keep", 41, DirEntryFileType::File)
                .unwrap();
            inner
                .add_new_entry(&f.ext4, "victim", 42, DirEntryFileType::File)
                .unwrap();
            inner
                .add_new_entry(&f.ext4, "tail", 43, DirEntryFileType::File)
                .unwrap();
        }
        assert_eq!(readdir_names(&dir).len(), 5); // . .. keep victim tail

        // Delete the middle entry; its space merges into `keep`.
        {
            let mut inner = dir.inner.write();
            let info = inner.find_entry_info("victim").unwrap();
            inner.delete_entry(&info).unwrap();
        }
        assert_eq!(
            dir.inner
                .read()
                .find_entry_ino("victim")
                .unwrap_err()
                .error(),
            Errno::ENOENT
        );
        assert_eq!(readdir_names(&dir), [".", "..", "keep", "tail"]);
        let size_after_delete = dir.size();

        // Re-add a same-or-smaller name; it must reuse the reclaimed slack
        // inside the existing block (no growth).
        {
            let mut inner = dir.inner.write();
            inner
                .add_new_entry(&f.ext4, "reuse", 44, DirEntryFileType::File)
                .unwrap();
        }
        assert_eq!(dir.size(), size_after_delete, "re-add should not grow dir");
        assert_eq!(dir.inner.read().find_entry_ino("reuse").unwrap(), 44);
        assert_eq!(readdir_names(&dir).len(), 5);
    }

    /// `make_empty` lays down `.`/`..`, `empty_dir` reports true, and a later add
    /// flips it to false.
    #[ktest]
    fn make_empty_then_empty_dir() {
        let f = fixture_with_empty_dir();
        let dir = f.ext4.read_inode(DIR_INO).unwrap();

        {
            let mut inner = dir.inner.write();
            inner.make_empty(&f.ext4, DIR_INO, 2).unwrap();
        }

        // `.` points to self, `..` to the parent (ino 2).
        assert_eq!(dir.inner.read().find_entry_ino(".").unwrap(), DIR_INO);
        assert_eq!(dir.inner.read().find_entry_ino("..").unwrap(), 2);
        assert_eq!(dir.size(), BLOCK_SIZE);
        assert!(dir.inner.read().empty_dir(DIR_INO));
        // `..` pointing elsewhere does not count as an extra live name.
        assert_eq!(readdir_names(&dir), [".", ".."]);

        // After adding a real entry, the directory is no longer empty.
        {
            let mut inner = dir.inner.write();
            inner
                .add_new_entry(&f.ext4, "child", 51, DirEntryFileType::Dir)
                .unwrap();
        }
        assert!(!dir.inner.read().empty_dir(DIR_INO));
    }
}
