// SPDX-License-Identifier: MPL-2.0

//! The `Ext4` filesystem object: mount, geometry, and inode lookup.
//!
//! Phase 1 mounts a volume by reading and validating the superblock and the
//! block-group descriptor table, then locates and decodes inodes directly from
//! the device. The inode-table page cache, writeback, and allocation arrive in
//! later phases; the read-only path needs none of them.

use device_id::DeviceId;

use super::{
    block_group::{BlockGroupDesc, RawBlockGroup},
    inode::{Inode, InodeDesc, RawInode},
    prelude::*,
    super_block::{RawSuperBlock, SUPER_BLOCK_OFFSET, SuperBlock},
};
use crate::fs::vfs::file_system::FsEventSubscriberStats;

/// Root directory inode number.
pub(super) const ROOT_INO: Ext4Ino = 2;

/// Reserved inode holding the journal.
#[expect(dead_code)]
pub(super) const JOURNAL_INO: Ext4Ino = 8;

/// An ext4 filesystem instance.
pub struct Ext4 {
    block_device: Arc<dyn BlockDevice>,
    super_block: SuperBlock,
    block_groups: Vec<BlockGroupDesc>,
    fs_event_subscriber_stats: FsEventSubscriberStats,
    self_ref: Weak<Ext4>,
}

impl Ext4 {
    /// Mounts an ext4 volume from a block device.
    pub(super) fn open(device: Arc<dyn BlockDevice>) -> Result<Arc<Self>> {
        let raw_super_block = device.read_val::<RawSuperBlock>(SUPER_BLOCK_OFFSET)?;
        let super_block = SuperBlock::try_from(raw_super_block)?;

        let block_groups = Self::read_group_descriptors(device.as_ref(), &super_block)?;

        Ok(Arc::new_cyclic(|weak| Ext4 {
            block_device: device,
            super_block,
            block_groups,
            fs_event_subscriber_stats: FsEventSubscriberStats::new(),
            self_ref: weak.clone(),
        }))
    }

    pub(super) fn fs_event_subscriber_stats(&self) -> &FsEventSubscriberStats {
        &self.fs_event_subscriber_stats
    }

    /// Returns the device ID of the backing block device.
    pub(super) fn container_device_id(&self) -> DeviceId {
        self.block_device.id()
    }

    /// Reads the block-group descriptor table, which immediately follows the
    /// block holding the superblock.
    fn read_group_descriptors(
        device: &dyn BlockDevice,
        super_block: &SuperBlock,
    ) -> Result<Vec<BlockGroupDesc>> {
        let nr_groups = super_block.nr_block_groups() as usize;
        let gdt_bid = super_block.first_data_block() + 1;
        let gdt_base = gdt_bid as usize * super_block.block_size();

        let mut block_groups = Vec::with_capacity(nr_groups);
        for group_idx in 0..nr_groups {
            let raw: RawBlockGroup =
                device.read_val(gdt_base + group_idx * size_of::<RawBlockGroup>())?;
            block_groups.push(BlockGroupDesc::from(&raw));
        }
        Ok(block_groups)
    }

    pub(super) fn super_block(&self) -> &SuperBlock {
        &self.super_block
    }

    pub(super) fn block_device(&self) -> &Arc<dyn BlockDevice> {
        &self.block_device
    }

    /// Submits an asynchronous read of one or more blocks starting at `bid`.
    pub(super) fn read_blocks_async(
        &self,
        bid: Ext4Bid,
        bio_segment: BioSegment,
        complete_fn: Option<BioCompleteFn>,
        io_batch: &mut IoBatch,
    ) -> Result<()> {
        self.block_device
            .read_blocks_async(Bid::new(bid), bio_segment, complete_fn, io_batch)?;
        Ok(())
    }

    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn this(&self) -> Weak<Ext4> {
        self.self_ref.clone()
    }

    /// Locates and decodes an inode's metadata directly from the inode table.
    pub(super) fn read_inode_desc(&self, ino: Ext4Ino) -> Result<InodeDesc> {
        if ino == 0 {
            return_errno_with_message!(Errno::ENOENT, "invalid inode number 0");
        }
        let sb = &self.super_block;
        let group_idx = ((ino - 1) / sb.nr_inodes_per_group()) as usize;
        let idx_in_group = ((ino - 1) % sb.nr_inodes_per_group()) as usize;

        let group = self
            .block_groups
            .get(group_idx)
            .ok_or_else(|| Error::with_message(Errno::ENOENT, "inode block group out of range"))?;

        let offset =
            group.inode_table_bid() as usize * sb.block_size() + idx_in_group * sb.inode_size();
        let raw = self.block_device.read_val::<RawInode>(offset)?;
        InodeDesc::try_from(&raw)
    }

    /// Reads the root directory's inode metadata.
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn root_inode_desc(&self) -> Result<InodeDesc> {
        self.read_inode_desc(ROOT_INO)
    }

    /// Reads an inode and builds its in-memory object.
    pub(super) fn read_inode(&self, ino: Ext4Ino) -> Result<Arc<Inode>> {
        let desc = self.read_inode_desc(ino)?;
        let type_ = desc.type_();
        let block_group_idx = ((ino - 1) / self.super_block.nr_inodes_per_group()) as usize;
        Ok(Inode::new(
            ino,
            type_,
            Dirty::new(desc),
            block_group_idx,
            self.self_ref.clone(),
        ))
    }

    /// Reads the root directory inode.
    pub(super) fn root_inode(&self) -> Result<Arc<Inode>> {
        self.read_inode(ROOT_INO)
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::{
        super::test_utils::{Ext4FixtureBuilder, make_file_inode},
        *,
    };

    #[ktest]
    fn mount_and_read_root() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let root = f.ext4.root_inode_desc().unwrap();
        assert_eq!(root.type_(), InodeType::Dir);
        assert_eq!(root.link_count(), 2);
        assert!(root.is_extent_based());
        assert_eq!(f.ext4.super_block().nr_block_groups(), 1);
    }

    #[ktest]
    fn read_inode_zero_fails() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        assert!(f.ext4.read_inode_desc(0).is_err());
    }

    #[ktest]
    fn read_small_file_end_to_end() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let data_block = 100u32;
        let content = b"hello ext4 phase 1 read path!";
        f.write_data_block(data_block, content);
        f.write_raw_inode(11, &make_file_inode(data_block, content.len() as u32));

        let inode = f.ext4.read_inode(11).unwrap();
        assert_eq!(inode.inode_type(), InodeType::File);
        assert_eq!(inode.size(), content.len());

        let mut buf = vec![0u8; content.len()];
        let mut writer = VmWriter::from(buf.as_mut_slice()).to_fallible();
        let read = inode.read_at(0, &mut writer).unwrap();
        assert_eq!(read, content.len());
        assert_eq!(&buf[..], content);
    }
}
