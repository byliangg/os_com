// SPDX-License-Identifier: MPL-2.0

//! In-memory block device and image fixtures for ext4 kernel-mode tests.
//!
//! Phase 1 fixtures hand-build a minimal-feature image (extent + filetype, no
//! checksums/64-bit/flex_bg) directly in memory, since `mke2fs` cannot run
//! inside a kernel test. The image grows as later tasks need on-disk extents,
//! bitmaps, and directory data.

use core::{
    fmt,
    sync::atomic::{AtomicUsize, Ordering},
};

use aster_block::{
    BlockDeviceMeta,
    bio::{BioEnqueueError, BioType, SubmittedBio},
};
use device_id::{DeviceId, MajorId, MinorId};
use ostd::mm::{HasSize, io::util::HasVmReaderWriter};

use super::{
    block_group::RawBlockGroup,
    fs::{Ext4, ROOT_INO},
    inode::{EXTENTS_FL, RawInode},
    prelude::*,
    super_block::{MAGIC_NUM, RawSuperBlock, SUPER_BLOCK_OFFSET, SuperBlock},
};

/// An in-memory block device backed by a zeroed frame segment.
pub(super) struct Ext4MemoryDisk {
    segment: Segment<()>,
    flush_count: AtomicUsize,
}

impl Ext4MemoryDisk {
    pub(super) fn new(nblocks: usize) -> Self {
        let npages = (nblocks * BLOCK_SIZE).div_ceil(PAGE_SIZE);
        let segment = FrameAllocOptions::new()
            .zeroed(true)
            .alloc_segment(npages)
            .unwrap();
        Self {
            segment,
            flush_count: AtomicUsize::new(0),
        }
    }

    pub(super) fn segment(&self) -> &Segment<()> {
        &self.segment
    }
}

impl Debug for Ext4MemoryDisk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ext4MemoryDisk")
            .field("bytes", &self.segment.size())
            .finish()
    }
}

impl BlockDevice for Ext4MemoryDisk {
    fn enqueue(&self, bio: SubmittedBio) -> core::result::Result<(), BioEnqueueError> {
        if bio.type_() == BioType::Flush {
            self.flush_count.fetch_add(1, Ordering::Relaxed);
            bio.complete(BioStatus::Complete);
            return Ok(());
        }

        let mut cur_device_ofs = bio.sid_range().start.to_raw() as usize * SECTOR_SIZE;
        for seg in bio.segments() {
            let io_size = match bio.type_() {
                BioType::Read => seg
                    .inner_dma_slice()
                    .writer()
                    .unwrap()
                    .write(self.segment.reader().skip(cur_device_ofs)),
                BioType::Write => self
                    .segment
                    .writer()
                    .skip(cur_device_ofs)
                    .write(&mut seg.inner_dma_slice().reader().unwrap()),
                _ => {
                    bio.complete(BioStatus::NotSupported);
                    return Ok(());
                }
            };
            cur_device_ofs += io_size;
        }

        bio.complete(BioStatus::Complete);
        Ok(())
    }

    fn metadata(&self) -> BlockDeviceMeta {
        BlockDeviceMeta {
            max_nr_segments_per_bio: usize::MAX,
            nr_sectors: self.segment.size() / SECTOR_SIZE,
        }
    }

    fn name(&self) -> &str {
        "ext4-memory-disk"
    }

    fn id(&self) -> DeviceId {
        DeviceId::new(MajorId::new(1), MinorId::new(0))
    }
}

/// Fixed group-0 layout used by the fixture builder.
const INODE_SIZE: usize = 256;
const INODE_TABLE_BID: u32 = 4;

/// A mounted ext4 filesystem over an in-memory disk, for tests.
pub(super) struct Ext4Fixture {
    pub disk: Arc<Ext4MemoryDisk>,
    pub ext4: Arc<Ext4>,
    #[expect(dead_code)]
    pub sb: SuperBlock,
}

impl Ext4Fixture {
    /// Writes raw bytes into data block `block`.
    pub(super) fn write_data_block(&self, block: u32, data: &[u8]) {
        self.disk
            .segment()
            .write_bytes(block as usize * BLOCK_SIZE, data)
            .unwrap();
    }

    /// Writes a raw inode into the group-0 inode table at inode number `ino`.
    pub(super) fn write_raw_inode(&self, ino: u32, raw: &RawInode) {
        let offset = INODE_TABLE_BID as usize * BLOCK_SIZE + (ino - 1) as usize * INODE_SIZE;
        self.disk.segment().write_val(offset, raw).unwrap();
    }
}

/// Builds a minimal single-purpose ext4 image with a fixed group-0 layout:
/// block 0 = superblock, block 1 = GDT, 2 = block bitmap, 3 = inode bitmap,
/// 4.. = inode table.
pub(super) struct Ext4FixtureBuilder {
    blocks_per_group: u32,
    inodes_per_group: u32,
    nblocks: usize,
}

impl Ext4FixtureBuilder {
    pub(super) fn new(blocks_per_group: u32, inodes_per_group: u32, nblocks: usize) -> Self {
        Self {
            blocks_per_group,
            inodes_per_group,
            nblocks,
        }
    }

    pub(super) fn build(self) -> Result<Ext4Fixture> {
        let nr_groups = (self.nblocks as u32 - 1) / self.blocks_per_group + 1;
        let inodes_count = nr_groups * self.inodes_per_group;

        let raw_sb = RawSuperBlock {
            inodes_count,
            blocks_count: self.nblocks as u32,
            first_data_block: 0,
            log_block_size: 2,
            log_frag_size: 2,
            blocks_per_group: self.blocks_per_group,
            frags_per_group: self.blocks_per_group,
            inodes_per_group: self.inodes_per_group,
            magic: MAGIC_NUM,
            state: 1,      // VALID
            errors: 1,     // Continue
            creator_os: 0, // Linux
            rev_level: 1,  // Dynamic
            first_ino: 11,
            inode_size: INODE_SIZE as u16,
            feature_incompat: 0x2 | 0x40, // FILETYPE | EXTENTS
            feature_ro_compat: 0x1,       // SPARSE_SUPER
            ..Default::default()
        };
        let sb = SuperBlock::try_from(raw_sb)?;

        let raw_gd = RawBlockGroup {
            block_bitmap_lo: 2,
            inode_bitmap_lo: 3,
            inode_table_lo: INODE_TABLE_BID,
            ..Default::default()
        };

        let disk = Arc::new(Ext4MemoryDisk::new(self.nblocks));
        disk.segment()
            .write_val(SUPER_BLOCK_OFFSET, &raw_sb)
            .unwrap();
        disk.segment().write_val(BLOCK_SIZE, &raw_gd).unwrap();

        let root = make_root_dir_inode();
        let root_offset =
            INODE_TABLE_BID as usize * BLOCK_SIZE + (ROOT_INO - 1) as usize * INODE_SIZE;
        disk.segment().write_val(root_offset, &root).unwrap();

        let ext4 = Ext4::open(disk.clone() as Arc<dyn BlockDevice>)?;
        Ok(Ext4Fixture { disk, ext4, sb })
    }
}

/// A root-directory inode: one data block via an extent root (left empty here;
/// the directory data block is populated when a later task needs to read it),
/// link count 2, `EXT4_EXTENTS_FL` set.
fn make_root_dir_inode() -> RawInode {
    RawInode {
        mode: 0o040755,
        size_lo: BLOCK_SIZE as u32,
        link_count: 2,
        sector_count: (BLOCK_SIZE / SECTOR_SIZE) as u32,
        flags: EXTENTS_FL,
        extra_isize: 32,
        ..Default::default()
    }
}

/// Builds a regular-file inode mapping logical block 0 to `data_block` via a
/// depth-0 inline extent.
pub(super) fn make_file_inode(data_block: u32, size: u32) -> RawInode {
    let mut raw = RawInode {
        mode: 0o100644, // S_IFREG | 0644
        size_lo: size,
        link_count: 1,
        sector_count: (BLOCK_SIZE / SECTOR_SIZE) as u32,
        flags: EXTENTS_FL,
        extra_isize: 32,
        ..Default::default()
    };
    // Inline extent root in `i_block`: a 12-byte header (magic 0xF30A, 1 entry,
    // max 4, depth 0) followed by one extent mapping logical block 0 to
    // `data_block` with length 1. Each `i_block` word packs two 16-bit fields.
    raw.block[0] = 0xF30A | (1 << 16); // eh_magic | eh_entries
    raw.block[1] = 4; // eh_max=4, eh_depth=0
    raw.block[2] = 0; // eh_generation
    raw.block[3] = 0; // ee_block = 0
    raw.block[4] = 1; // ee_len=1, ee_start_hi=0
    raw.block[5] = data_block; // ee_start_lo
    raw
}

/// Builds a directory inode (type Dir, one block) mapping logical block 0 to
/// `data_block`.
pub(super) fn make_dir_inode(data_block: u32) -> RawInode {
    let mut raw = make_file_inode(data_block, BLOCK_SIZE as u32);
    raw.mode = 0o040755; // S_IFDIR | 0755
    raw.link_count = 2;
    raw
}

/// Builds one directory data block holding `entries` of `(ino, name,
/// file_type)`. The last entry's `rec_len` is extended to fill the block.
pub(super) fn make_dir_block(entries: &[(u32, &str, u8)]) -> Vec<u8> {
    let mut block = vec![0u8; BLOCK_SIZE];
    let mut offset = 0;
    for (i, (ino, name, file_type)) in entries.iter().enumerate() {
        let name_bytes = name.as_bytes();
        let rec_len = if i + 1 == entries.len() {
            BLOCK_SIZE - offset
        } else {
            (name_bytes.len() + 8).next_multiple_of(4)
        };
        block[offset..offset + 4].copy_from_slice(&ino.to_le_bytes());
        block[offset + 4..offset + 6].copy_from_slice(&(rec_len as u16).to_le_bytes());
        block[offset + 6] = name_bytes.len() as u8;
        block[offset + 7] = *file_type;
        block[offset + 8..offset + 8 + name_bytes.len()].copy_from_slice(name_bytes);
        offset += rec_len;
    }
    block
}
