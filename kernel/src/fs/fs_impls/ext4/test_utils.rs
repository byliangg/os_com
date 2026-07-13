// SPDX-License-Identifier: MPL-2.0

//! In-memory block device and image fixtures for ext4 kernel-mode tests.
//!
//! Phase 1 fixtures hand-build a minimal-feature image (extent + filetype, no
//! checksums/64-bit/flex_bg) directly in memory, since `mke2fs` cannot run
//! inside a kernel test. The image grows as later tasks need on-disk extents,
//! bitmaps, and directory data.

use core::{
    fmt,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use aster_block::{
    BlockDeviceMeta,
    bio::{BioEnqueueError, BioType, SubmittedBio},
};
use device_id::{DeviceId, MajorId, MinorId};
use ostd::mm::{HasSize, io::util::HasVmReaderWriter};

use super::{
    block_group::RawBlockGroup,
    fs::{Ext4, JOURNAL_INO, ROOT_INO},
    inode::{FileFlags, RawInode},
    prelude::*,
    super_block::{MAGIC_NUM, RawSuperBlock, SUPER_BLOCK_OFFSET, SuperBlock},
};

/// An in-memory block device backed by a zeroed frame segment.
pub(super) struct Ext4MemoryDisk {
    segment: Segment<()>,
    flush_count: AtomicUsize,
    /// When set, every write bio fails with `IoError`. Lets tests force a
    /// metadata writeback failure (e.g. to exercise `create_inode` rollback).
    fail_writes: AtomicBool,
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
            fail_writes: AtomicBool::new(false),
        }
    }

    pub(super) fn segment(&self) -> &Segment<()> {
        &self.segment
    }

    /// Makes every subsequent write bio fail (or stops failing them).
    pub(super) fn set_fail_writes(&self, fail: bool) {
        self.fail_writes.store(fail, Ordering::Relaxed);
    }

    /// The number of Flush bios issued so far (each [`BlockDevice::sync`] barrier
    /// is one). Lets the journal commit tests assert the crash-safe barriers
    /// fired.
    pub(super) fn flush_count(&self) -> usize {
        self.flush_count.load(Ordering::Relaxed)
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

        if bio.type_() == BioType::Write && self.fail_writes.load(Ordering::Relaxed) {
            bio.complete(BioStatus::IoError);
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

    /// Reads back the raw inode at inode number `ino` from the group-0 table.
    pub(super) fn read_raw_inode(&self, ino: u32) -> RawInode {
        let offset = INODE_TABLE_BID as usize * BLOCK_SIZE + (ino - 1) as usize * INODE_SIZE;
        self.disk.segment().read_val(offset).unwrap()
    }
}

/// Builds a minimal single-purpose ext4 image with a fixed group-0 layout:
/// block 0 = superblock, block 1 = GDT, 2 = block bitmap, 3 = inode bitmap,
/// 4.. = inode table.
pub(super) struct Ext4FixtureBuilder {
    blocks_per_group: u32,
    inodes_per_group: u32,
    nblocks: usize,
    /// When set, mark the group-0 system/reserved blocks as allocated in the
    /// block bitmap and seed the matching free-block counters. Off by default so
    /// the read-only fixtures keep their all-zero bitmap.
    mark_metadata: bool,
    /// When set, override the free-block counters to zero (for ENOSPC tests).
    no_free_blocks: bool,
    /// When set, cap the (single-group) fixture to exactly this many free blocks
    /// by marking all but the top `n` data blocks allocated. For
    /// ENOSPC-mid-operation tests.
    free_block_cap: Option<u32>,
    /// When set, mark the reserved inodes (1..`first_ino`) of group 0 as
    /// allocated in the inode bitmap and seed the matching free-inode counters.
    /// Off by default so the read-only fixtures keep their all-zero inode
    /// bitmap and zero counters.
    mark_inode_metadata: bool,
    /// When set, override the free-inode counters to zero (for inode-ENOSPC
    /// tests). Implies the inode metadata is marked.
    no_free_inodes: bool,
    /// When set, additionally mark this group-0 inode allocated in the inode
    /// bitmap and decrement the free-inode counters by one. Used to reserve a
    /// pre-placed test directory inode so the allocator does not hand its number
    /// back out to a freshly created child. Requires `mark_inode_metadata`.
    reserved_inode: Option<u32>,
    /// When set, OR the `HAS_JOURNAL` compat feature bit into the superblock so
    /// the journal geometry loader treats the volume as journaled.
    has_journal: bool,
    /// When set, OR the `DIR_INDEX` (htree) compat feature bit into the
    /// superblock so `SuperBlock::has_dir_index` reports true — the gate a
    /// directory must clear for the htree read / degrade paths to engage.
    has_dir_index: bool,
    /// When set to `Some(maxlen)`, lay down the journal inode (ino 8) and a clean
    /// journal superblock on disk *before* `Ext4::open`, so the mount path itself
    /// loads the journal. The log occupies `maxlen` physical blocks starting at
    /// [`JOURNAL_START_BLOCK`]. Implies `has_journal`.
    journal_inode: Option<u32>,
    /// Blocks reserved for privileged processes (`s_r_blocks_count`); `statfs`
    /// subtracts them from `bfree` to report `bavail`.
    reserved_blocks: u32,
    /// Volume UUID (`s_uuid`); `statfs` folds its low 8 bytes into `f_fsid`.
    uuid: [u8; 16],
}

/// The physical block where the fixture places the journal (log block 0 → this
/// device block). Matches the constant the `journal` module's own tests use.
pub(super) const JOURNAL_START_BLOCK: u32 = 200;

impl Ext4FixtureBuilder {
    pub(super) fn new(blocks_per_group: u32, inodes_per_group: u32, nblocks: usize) -> Self {
        Self {
            blocks_per_group,
            inodes_per_group,
            nblocks,
            mark_metadata: false,
            no_free_blocks: false,
            free_block_cap: None,
            mark_inode_metadata: false,
            no_free_inodes: false,
            reserved_inode: None,
            has_journal: false,
            has_dir_index: false,
            journal_inode: None,
            reserved_blocks: 0,
            uuid: [0; 16],
        }
    }

    /// Sets `s_r_blocks_count` so `statfs` has reserved blocks to subtract from
    /// `bfree` when reporting `bavail`.
    pub(super) fn with_reserved_blocks(mut self, blocks: u32) -> Self {
        self.reserved_blocks = blocks;
        self
    }

    /// Sets `s_uuid` so `statfs` has a non-zero `f_fsid` (its low 8 bytes).
    pub(super) fn with_uuid(mut self, uuid: [u8; 16]) -> Self {
        self.uuid = uuid;
        self
    }

    /// Lays down the journal inode (ino 8) and a *clean* journal superblock
    /// (`s_start == 0`, `s_sequence == 1`) on disk before `Ext4::open`, so the
    /// mount path loads and starts the journal. The log spans `maxlen` physical
    /// blocks at [`JOURNAL_START_BLOCK`]. Sets `HAS_JOURNAL` too.
    pub(super) fn with_journal_inode(mut self, maxlen: u32) -> Self {
        self.has_journal = true;
        self.journal_inode = Some(maxlen);
        self
    }

    /// Sets the `HAS_JOURNAL` compat feature bit in the superblock, so
    /// `journal::load_geometry` treats the volume as journaled and parses the
    /// journal inode (ino 8).
    ///
    /// Also lays down a *minimal* valid journal (a 2-block inode + clean
    /// superblock) so `Ext4::open` — which now loads the journal at mount time —
    /// succeeds. The journal-module tests that use this then overwrite ino 8 and
    /// the journal superblock with their own geometry after `build()` and drive a
    /// separately constructed `Journal`; the mount-time journal that `Ext4::open`
    /// loads sits idle (it never receives a running transaction) and is torn down
    /// when the fixture's `Ext4` drops.
    pub(super) fn with_has_journal(mut self) -> Self {
        self.has_journal = true;
        if self.journal_inode.is_none() {
            self.journal_inode = Some(2);
        }
        self
    }

    /// Sets the `DIR_INDEX` (htree) compat feature bit in the superblock, so
    /// `SuperBlock::has_dir_index` reports true. The htree read and
    /// `degrade_htree_to_linear` paths are gated on it; fixtures that fabricate
    /// an `INDEX`-flagged directory (there is no htree *build* yet) enable it so
    /// the rename / insert paths engage the degrade instead of treating the
    /// dx_root as a linear block.
    pub(super) fn with_dir_index(mut self) -> Self {
        self.has_dir_index = true;
        self
    }

    /// Reserves an extra group-0 inode (beyond the reserved 1..`first_ino`): its
    /// bitmap bit is marked allocated and the free-inode counters are reduced by
    /// one, so the allocator skips it. Used by directory fixtures whose
    /// pre-placed directory inode would otherwise be re-handed-out as a child.
    pub(super) fn with_reserved_inode(mut self, ino: u32) -> Self {
        self.mark_inode_metadata = true;
        self.reserved_inode = Some(ino);
        self
    }

    /// Marks the group-0 metadata + reserved blocks as allocated in the block
    /// bitmap and seeds free-block counters accordingly, giving allocator tests
    /// a realistic starting image.
    pub(super) fn with_block_bitmap_metadata_marked(mut self) -> Self {
        self.mark_metadata = true;
        self
    }

    /// Forces all free-block counters to zero (for ENOSPC tests). Implies that
    /// the bitmap is marked, so an allocation cannot succeed.
    pub(super) fn with_no_free_blocks(mut self) -> Self {
        self.no_free_blocks = true;
        self.mark_metadata = true;
        self
    }

    /// Caps the single-group fixture to exactly `n` free blocks (marking all but
    /// the top `n` data blocks allocated). For tests that must run the allocator
    /// out of space partway through a multi-block operation.
    pub(super) fn with_free_blocks(mut self, n: u32) -> Self {
        self.free_block_cap = Some(n);
        self.mark_metadata = true;
        self
    }

    /// Marks the reserved inodes (1..`first_ino`) of group 0 as allocated in the
    /// inode bitmap and seeds the free-inode counters accordingly, giving inode
    /// allocator tests a realistic starting image.
    pub(super) fn with_inode_bitmap_metadata_marked(mut self) -> Self {
        self.mark_inode_metadata = true;
        self
    }

    /// Forces all free-inode counters to zero (for inode-ENOSPC tests). Implies
    /// the inode bitmap is marked, so an inode allocation cannot succeed.
    pub(super) fn with_no_free_inodes(mut self) -> Self {
        self.no_free_inodes = true;
        self.mark_inode_metadata = true;
        self
    }

    pub(super) fn build(self) -> Result<Ext4Fixture> {
        let nr_groups = (self.nblocks as u32 - 1) / self.blocks_per_group + 1;
        let inodes_count = nr_groups * self.inodes_per_group;
        let inode_table_blocks = self.inodes_per_group / (BLOCK_SIZE / INODE_SIZE) as u32;

        // Each group's system zone spans, from its first block: the superblock
        // region (block 0 in group 0), the GDT block, block bitmap, inode bitmap,
        // and the inode-table blocks. first_data_block is 0 in this fixture.
        let metadata_end_block = INODE_TABLE_BID + inode_table_blocks; // exclusive

        // Sum the free blocks across all groups so the superblock counter matches
        // the per-group descriptors.
        let total_free: u32 = if let Some(n) = self.free_block_cap {
            n
        } else if self.no_free_blocks || !self.mark_metadata {
            0
        } else {
            (0..nr_groups)
                .map(|g| {
                    let group_first = g * self.blocks_per_group;
                    let group_size = if g == nr_groups - 1 {
                        self.nblocks as u32 - group_first
                    } else {
                        self.blocks_per_group
                    };
                    group_size - metadata_end_block
                })
                .sum()
        };

        // Reserved inodes (1..first_ino) occupy the low bits of group 0's inode
        // bitmap; `first_ino` is 11 in this fixture, so bits 0..10 are reserved.
        const FIRST_INO: u32 = 11;
        let reserved_inodes = FIRST_INO - 1;

        // Per-group free-inode counts, summed for the superblock counter. Only
        // group 0 carries the reserved inodes; the zero override applies to the
        // single-group fixtures used by the inode-ENOSPC test.
        // An extra reserved inode (a pre-placed test directory) lives in group 0
        // and removes one free inode from group 0's count.
        let extra_reserved = self.reserved_inode.is_some() as u32;
        let total_free_inodes: u32 = if self.no_free_inodes || !self.mark_inode_metadata {
            0
        } else {
            (0..nr_groups)
                .map(|g| {
                    if g == 0 {
                        self.inodes_per_group - reserved_inodes - extra_reserved
                    } else {
                        self.inodes_per_group
                    }
                })
                .sum()
        };

        let raw_sb = RawSuperBlock {
            inodes_count,
            blocks_count: self.nblocks as u32,
            reserved_blocks_count: self.reserved_blocks,
            free_blocks_count: total_free,
            free_inodes_count: total_free_inodes,
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
            // HAS_JOURNAL (0x4) when journaled, DIR_INDEX (0x20) when htree is
            // enabled; else no compat features. (Listed before the
            // incompat/ro_compat fields to match the struct declaration order —
            // clippy `inconsistent_struct_constructor`.)
            feature_compat: (if self.has_journal { 0x4 } else { 0 })
                | (if self.has_dir_index { 0x20 } else { 0 }),
            feature_incompat: 0x2 | 0x40, // FILETYPE | EXTENTS
            feature_ro_compat: 0x1,       // SPARSE_SUPER
            uuid: self.uuid,
            // The mount contract requires the internal journal at the reserved
            // ino 8 (`s_journal_inum`); `mke2fs` writes the same.
            journal_ino: if self.has_journal { JOURNAL_INO } else { 0 },
            ..Default::default()
        };
        let sb = SuperBlock::try_from(raw_sb)?;

        let disk = Arc::new(Ext4MemoryDisk::new(self.nblocks));
        disk.segment()
            .write_val(SUPER_BLOCK_OFFSET, &raw_sb)
            .unwrap();

        // Lay out a descriptor (and, when marking, a block bitmap) per group.
        // Each group `g` keeps its metadata at fixed in-group offsets: block
        // bitmap at +2, inode bitmap at +3, inode table at +4.
        for g in 0..nr_groups {
            let group_first = g * self.blocks_per_group; // first_data_block == 0
            let group_size = if g == nr_groups - 1 {
                self.nblocks as u32 - group_first
            } else {
                self.blocks_per_group
            };
            // `mark_end` is the exclusive bit up to which the bitmap is marked
            // allocated; capping leaves only the top `n` blocks free.
            let (free, mark_end) = if let Some(n) = self.free_block_cap {
                (n, group_size - n)
            } else if self.no_free_blocks {
                (0, metadata_end_block)
            } else if self.mark_metadata {
                (group_size - metadata_end_block, metadata_end_block)
            } else {
                (0, metadata_end_block)
            };

            // Per-group inode bookkeeping. `inode_mark_end` is the exclusive bit
            // up to which group `g`'s inode bitmap is marked allocated.
            let reserved_in_group = if g == 0 { reserved_inodes } else { 0 };
            let extra_reserved_in_group = if g == 0 { extra_reserved } else { 0 };
            let (free_inodes, inode_mark_end) = if self.no_free_inodes {
                (0, self.inodes_per_group)
            } else if self.mark_inode_metadata {
                (
                    self.inodes_per_group - reserved_in_group - extra_reserved_in_group,
                    reserved_in_group,
                )
            } else {
                (0, 0)
            };

            let raw_gd = RawBlockGroup {
                block_bitmap_lo: group_first + 2,
                inode_bitmap_lo: group_first + 3,
                inode_table_lo: group_first + INODE_TABLE_BID,
                free_blocks_count_lo: free as u16,
                free_inodes_count_lo: free_inodes as u16,
                ..Default::default()
            };
            disk.segment()
                .write_val(
                    BLOCK_SIZE + g as usize * size_of::<RawBlockGroup>(),
                    &raw_gd,
                )
                .unwrap();

            if self.mark_metadata {
                // Mark the in-group allocated zone (bits 0..mark_end) — the system
                // zone, plus extra blocks when capping free space. LSB-first.
                let mut bitmap = vec![0u8; BLOCK_SIZE];
                for bit in 0..mark_end as usize {
                    bitmap[bit / 8] |= 1 << (bit % 8);
                }
                disk.segment()
                    .write_bytes((group_first as usize + 2) * BLOCK_SIZE, &bitmap)
                    .unwrap();
            }

            if self.mark_inode_metadata {
                // Mark the in-group reserved/capped inode bits (0..inode_mark_end)
                // allocated in this group's inode bitmap. LSB-first.
                let mut inode_bitmap = vec![0u8; BLOCK_SIZE];
                for bit in 0..inode_mark_end as usize {
                    inode_bitmap[bit / 8] |= 1 << (bit % 8);
                }
                // Mark the extra reserved inode (a pre-placed test directory) so
                // the allocator skips its number.
                if g == 0
                    && let Some(ino) = self.reserved_inode
                {
                    let bit = (ino - 1) as usize;
                    inode_bitmap[bit / 8] |= 1 << (bit % 8);
                }
                disk.segment()
                    .write_bytes((group_first as usize + 3) * BLOCK_SIZE, &inode_bitmap)
                    .unwrap();
            }
        }

        let root = make_root_dir_inode();
        let root_offset =
            INODE_TABLE_BID as usize * BLOCK_SIZE + (ROOT_INO - 1) as usize * INODE_SIZE;
        disk.segment().write_val(root_offset, &root).unwrap();

        // Lay down the journal inode (ino 8) and a clean journal superblock BEFORE
        // `Ext4::open`, so the mount path itself loads and starts the journal.
        if let Some(maxlen) = self.journal_inode {
            let journal_inode = make_multi_block_file_inode(JOURNAL_START_BLOCK, maxlen as u16);
            let journal_offset =
                INODE_TABLE_BID as usize * BLOCK_SIZE + (JOURNAL_INO - 1) as usize * INODE_SIZE;
            disk.segment()
                .write_val(journal_offset, &journal_inode)
                .unwrap();
            super::journal::write_clean_journal_superblock_for_test(
                disk.as_ref(),
                JOURNAL_START_BLOCK as Ext4Bid,
                maxlen,
                1,                           // s_first: first log-data block
                super::journal::Tid::new(1), // s_sequence: fresh journal starts at tid 1
            )?;
        }

        let ext4 = Ext4::open(disk.clone() as Arc<dyn BlockDevice>, None)?;
        Ok(Ext4Fixture { disk, ext4, sb })
    }
}

/// A root-directory inode: link count 2, `EXT4_EXTENTS_FL` set, and a valid
/// **empty** extent root.
fn make_root_dir_inode() -> RawInode {
    let mut raw = RawInode {
        mode: 0o040755,
        size_lo: BLOCK_SIZE as u32,
        link_count: 2,
        sector_count: (BLOCK_SIZE / SECTOR_SIZE) as u32,
        flags: FileFlags::EXTENTS.bits(),
        extra_isize: 32,
        ..Default::default()
    };
    // A valid **empty** extent root — the root must parse, since
    // `ExtentTree::try_new` validates it when the inode is loaded. Tests that
    // read actual root-directory data overwrite ino 2 with a populated inode.
    raw.block[0] = 0xF30A; // eh_magic | eh_entries(=0)
    raw.block[1] = 4; // eh_max=4, eh_depth=0
    raw
}

/// Builds a regular-file inode mapping logical block 0 to `data_block` via a
/// depth-0 inline extent.
pub(super) fn make_file_inode(data_block: u32, size: u32) -> RawInode {
    let mut raw = RawInode {
        mode: 0o100644, // S_IFREG | 0644
        size_lo: size,
        link_count: 1,
        sector_count: (BLOCK_SIZE / SECTOR_SIZE) as u32,
        flags: FileFlags::EXTENTS.bits(),
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

/// Builds an extent-mapped inode whose `i_block` holds a single inline extent
/// mapping logical blocks `[0, len)` to physical `[data_block, data_block +
/// len)`, sized to exactly `len` blocks. Used for the journal inode (ino 8),
/// whose data blocks hold the log and which must map more than one block.
pub(super) fn make_multi_block_file_inode(data_block: u32, len: u16) -> RawInode {
    let mut raw = RawInode {
        mode: 0o100644, // S_IFREG | 0644
        size_lo: len as u32 * BLOCK_SIZE as u32,
        link_count: 1,
        sector_count: (len as u32) * (BLOCK_SIZE / SECTOR_SIZE) as u32,
        flags: FileFlags::EXTENTS.bits(),
        extra_isize: 32,
        ..Default::default()
    };
    // Inline extent root: 12-byte header (magic 0xF30A, 1 entry, max 4, depth 0)
    // followed by one written extent of `len` blocks. Each `i_block` word packs
    // two 16-bit fields.
    raw.block[0] = 0xF30A | (1 << 16); // eh_magic | eh_entries
    raw.block[1] = 4; // eh_max=4, eh_depth=0
    raw.block[2] = 0; // eh_generation
    raw.block[3] = 0; // ee_block = 0
    raw.block[4] = len as u32; // ee_len (low 16) | ee_start_hi (high 16, = 0)
    raw.block[5] = data_block; // ee_start_lo
    raw
}

/// Builds an empty regular-file inode: size 0, `i_blocks` 0, and an empty
/// inline extent root (header with 0 entries). Suitable as the target of a
/// fresh buffered write whose allocation/extents come from the write path.
pub(super) fn make_empty_file_inode() -> RawInode {
    let mut raw = RawInode {
        mode: 0o100644, // S_IFREG | 0644
        size_lo: 0,
        link_count: 1,
        sector_count: 0,
        flags: FileFlags::EXTENTS.bits(),
        extra_isize: 32,
        ..Default::default()
    };
    // Empty extent root: 12-byte header (magic 0xF30A, 0 entries, max 4, depth
    // 0), no extents. Each `i_block` word packs two 16-bit fields.
    raw.block[0] = 0xF30A; // eh_magic | eh_entries(=0)
    raw.block[1] = 4; // eh_max=4, eh_depth=0
    raw.block[2] = 0; // eh_generation
    raw
}

/// Builds a regular-file inode whose `i_block` holds a single *unwritten*
/// (preallocated) inline extent mapping logical blocks `[0, len)` to physical
/// `[data_block, data_block + len)`. The blocks count toward `i_blocks` (they
/// were allocated at fallocate time) but read as zeros until written. `size` is
/// the logical file size in bytes.
pub(super) fn make_unwritten_file_inode(data_block: u32, len: u16, size: u32) -> RawInode {
    let mut raw = RawInode {
        mode: 0o100644, // S_IFREG | 0644
        size_lo: size,
        link_count: 1,
        sector_count: (len as u32) * (BLOCK_SIZE / SECTOR_SIZE) as u32,
        flags: FileFlags::EXTENTS.bits(),
        extra_isize: 32,
        ..Default::default()
    };
    // Inline extent root: header (magic, 1 entry, max 4, depth 0) + one extent.
    // An unwritten extent encodes its length biased by 32768 (`MAX_WRITTEN_LEN`).
    raw.block[0] = 0xF30A | (1 << 16); // eh_magic | eh_entries
    raw.block[1] = 4; // eh_max=4, eh_depth=0
    raw.block[2] = 0; // eh_generation
    raw.block[3] = 0; // ee_block = 0
    // ee_len (low 16, biased for unwritten) | ee_start_hi (high 16, = 0).
    raw.block[4] = (len + 32768) as u32;
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
