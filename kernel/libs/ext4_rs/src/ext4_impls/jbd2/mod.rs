mod device;
mod handle;
mod journal;
mod recovery;
mod superblock;
mod space;
mod transaction;

use crate::ext4_defs::*;
use crate::prelude::*;
use crate::return_errno_with_message;
use crate::utils::{EXT4_CRC32_INIT, ext4_crc32c};

pub use device::*;
pub use handle::*;
pub use journal::*;
pub use recovery::*;
pub use space::*;
pub use superblock::*;
pub use transaction::*;

#[derive(Debug, Clone, Copy)]
pub struct ProbeTransactionResult {
    pub sequence: u32,
    pub descriptor_block: u32,
    pub data_block: u32,
    pub commit_block: u32,
    pub next_head: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct JournalCommitResult {
    pub sequence: u32,
    pub start_block: u32,
    pub commit_block: u32,
    pub next_head: u32,
    pub metadata_blocks: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct JournalCheckpointResult {
    pub start_block: u32,
    pub next_head: u32,
    pub next_start: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalCommitWriteStage {
    BeforeDescriptor,
    BeforeCommitBlock,
    AfterCommitBlock,
    AfterSuperblock,
}

#[derive(Debug, Clone)]
pub struct Jbd2Journal {
    pub device: JournalDevice,
    pub superblock: JournalSuperblockState,
    pub space: JournalSpace,
}

impl Jbd2Journal {
    pub fn load(ext4: &Ext4) -> Result<Self> {
        let journal_inode = ext4.super_block.journal_inode_number();
        if journal_inode == 0 {
            return_errno_with_message!(Errno::EINVAL, "filesystem advertises JBD2 without journal inode");
        }

        let device = JournalDevice::load(ext4, journal_inode)?;
        let superblock = JournalSuperblockState::load(
            &ext4.block_device,
            &device,
            Some(ext4.super_block.journal_uuid()),
        )?;
        let space = JournalSpace::from_superblock(&superblock)?;

        Ok(Self {
            device,
            superblock,
            space,
        })
    }

    pub fn write_probe_transaction(
        &mut self,
        block_device: &Arc<dyn BlockDevice>,
        target_fs_block: u64,
        data: &[u8],
    ) -> Result<ProbeTransactionResult> {
        let block_size = self.device.fs_block_size() as usize;
        if data.len() != block_size {
            return_errno_with_message!(Errno::EINVAL, "probe transaction data size mismatch");
        }
        if self.superblock.start() != 0 {
            return_errno_with_message!(Errno::EBUSY, "probe transaction only supports empty journal");
        }
        if self.space.free_blocks() < 3 {
            return_errno_with_message!(Errno::ENOSPC, "journal does not have enough free space");
        }

        let sequence = self.superblock.sequence();
        let descriptor_block = self.space.head();
        let data_block = self.space.advance(descriptor_block, 1);
        let commit_block = self.space.advance(data_block, 1);
        let next_head = self.space.advance(commit_block, 1);

        let mut journal_data = data.to_vec();
        let mut tag_flags = JBD2_FLAG_SAME_UUID | JBD2_FLAG_LAST_TAG;
        let magic_bytes = JBD2_MAGIC_NUMBER.to_be_bytes();
        if journal_data.len() >= magic_bytes.len() && journal_data[..magic_bytes.len()] == magic_bytes {
            journal_data[..magic_bytes.len()].fill(0);
            tag_flags |= JBD2_FLAG_ESCAPE;
        }

        let descriptor = self.build_single_tag_descriptor(sequence, target_fs_block, data, tag_flags)?;
        let commit = self.build_commit_block(sequence);

        self.device.write_block(block_device, descriptor_block, &descriptor)?;
        self.device.write_block(block_device, data_block, &journal_data)?;
        self.device.write_block(block_device, commit_block, &commit)?;

        self.superblock.update_start(descriptor_block);
        self.superblock.update_head(next_head);
        self.superblock.store(block_device, &self.device)?;
        self.space.advance_head(3);

        Ok(ProbeTransactionResult {
            sequence,
            descriptor_block,
            data_block,
            commit_block,
            next_head,
        })
    }

    pub fn write_commit_plan(
        &mut self,
        block_device: &Arc<dyn BlockDevice>,
        plan: &JournalCommitPlan,
    ) -> Result<JournalCommitResult> {
        self.write_commit_plan_with_hook(block_device, plan, |_| {})
    }

    pub fn write_commit_plan_with_hook(
        &mut self,
        block_device: &Arc<dyn BlockDevice>,
        plan: &JournalCommitPlan,
        mut hook: impl FnMut(JournalCommitWriteStage),
    ) -> Result<JournalCommitResult> {
        let metadata_blocks = plan.metadata_blocks.len() as u32;
        if metadata_blocks == 0 {
            return_errno_with_message!(Errno::EINVAL, "commit plan contains no metadata blocks");
        }

        let required_blocks = metadata_blocks
            .checked_add(2)
            .ok_or_else(|| Ext4Error::with_message(Errno::EINVAL, "journal transaction too large"))?;
        if self.space.free_blocks() < required_blocks {
            return_errno_with_message!(Errno::ENOSPC, "journal does not have enough free space");
        }

        let sequence = plan.tid;
        let descriptor_block = self.space.head();
        let mut cursor = descriptor_block;
        let mut entries = Vec::with_capacity(plan.metadata_blocks.len());
        for metadata in &plan.metadata_blocks {
            cursor = self.space.advance(cursor, 1);
            let escaped_data = self.escape_journal_data(&metadata.block_data);
            entries.push(JournalDescriptorEntry {
                target_fs_block: metadata.block_nr,
                data: escaped_data.data,
                checksum_data: metadata.block_data.as_slice(),
                escaped: escaped_data.escaped,
            });
        }
        let commit_block = self.space.advance(cursor, 1);
        let next_head = self.space.advance(commit_block, 1);

        let descriptor = self.build_descriptor_block(sequence, entries.as_slice())?;
        hook(JournalCommitWriteStage::BeforeDescriptor);
        self.device.write_block(block_device, descriptor_block, &descriptor)?;

        let mut data_block = self.space.advance(descriptor_block, 1);
        for entry in &entries {
            self.device.write_block(block_device, data_block, entry.data.as_slice())?;
            data_block = self.space.advance(data_block, 1);
        }

        let commit = self.build_commit_block(sequence);
        hook(JournalCommitWriteStage::BeforeCommitBlock);
        self.device.write_block(block_device, commit_block, &commit)?;
        hook(JournalCommitWriteStage::AfterCommitBlock);

        let journal_was_empty = self.superblock.start() == 0;
        if journal_was_empty {
            self.superblock.update_start(descriptor_block);
        }
        self.superblock.update_head(next_head);
        self.superblock.update_sequence(sequence.saturating_add(1));
        self.superblock.store(block_device, &self.device)?;
        hook(JournalCommitWriteStage::AfterSuperblock);
        if journal_was_empty {
            self.space.set_tail(descriptor_block)?;
        }
        self.space.advance_head(required_blocks);

        Ok(JournalCommitResult {
            sequence,
            start_block: descriptor_block,
            commit_block,
            next_head,
            metadata_blocks,
        })
    }

    pub fn checkpoint_transaction(
        &mut self,
        block_device: &Arc<dyn BlockDevice>,
        checkpoint: &JournalCheckpointPlan,
        next_start: Option<u32>,
    ) -> Result<JournalCheckpointResult> {
        let current_tail = self.space.tail();
        if current_tail != checkpoint.range.start_block {
            return_errno_with_message!(
                Errno::EINVAL,
                "checkpoint start does not match current journal tail"
            );
        }

        let released_blocks = self
            .space
            .distance(checkpoint.range.start_block, checkpoint.range.next_head);
        self.space.advance_tail(released_blocks);

        match next_start {
            Some(start) => self.superblock.update_start(start),
            None => self.superblock.update_start(0),
        }
        self.superblock.store(block_device, &self.device)?;

        Ok(JournalCheckpointResult {
            start_block: checkpoint.range.start_block,
            next_head: checkpoint.range.next_head,
            next_start,
        })
    }

    fn build_single_tag_descriptor(
        &self,
        sequence: u32,
        target_fs_block: u64,
        data: &[u8],
        flags: u32,
    ) -> Result<Vec<u8>> {
        let block_size = self.device.fs_block_size() as usize;
        let mut descriptor = vec![0u8; block_size];
        descriptor[..size_of::<JournalHeader>()]
            .copy_from_slice(self.header_bytes(JBD2_DESCRIPTOR_BLOCK, sequence).as_slice());

        let has_csum = self.superblock.has_checksum_v2_or_v3();
        if self.superblock.has_incompat_feature(JBD2_FEATURE_INCOMPAT_CSUM_V3) {
            let checksum = if has_csum {
                self.data_block_checksum(sequence, data)
            } else {
                0
            };
            let tag = JournalBlockTag3::new(target_fs_block, checksum, flags);
            let tag_bytes = unsafe {
                core::slice::from_raw_parts(
                    &tag as *const _ as *const u8,
                    size_of::<JournalBlockTag3>(),
                )
            };
            let tag_offset = size_of::<JournalHeader>();
            descriptor[tag_offset..tag_offset + tag_bytes.len()].copy_from_slice(tag_bytes);
        } else {
            let checksum = if has_csum {
                (self.data_block_checksum(sequence, data) & 0xFFFF) as u16
            } else {
                0
            };
            let tag = JournalBlockTag::new(
                target_fs_block as u32,
                checksum,
                (flags & 0xFFFF) as u16,
            );
            let tag_len = if self.superblock.has_incompat_feature(JBD2_FEATURE_INCOMPAT_64BIT) {
                12
            } else {
                8
            };
            let tag_bytes = unsafe {
                core::slice::from_raw_parts(&tag as *const _ as *const u8, tag_len)
            };
            let tag_offset = size_of::<JournalHeader>();
            descriptor[tag_offset..tag_offset + tag_bytes.len()].copy_from_slice(tag_bytes);
        }

        if has_csum {
            let tail_offset = block_size - size_of::<JournalBlockTail>();
            let mut csum_data = Vec::with_capacity(16 + block_size);
            csum_data.extend_from_slice(self.superblock.uuid().as_slice());
            csum_data.extend_from_slice(&descriptor);
            let checksum = ext4_crc32c(EXT4_CRC32_INIT, &csum_data, csum_data.len() as u32);
            let tail = JournalBlockTail::new(checksum);
            let tail_bytes = unsafe {
                core::slice::from_raw_parts(
                    &tail as *const _ as *const u8,
                    size_of::<JournalBlockTail>(),
                )
            };
            descriptor[tail_offset..tail_offset + tail_bytes.len()].copy_from_slice(tail_bytes);
        }

        Ok(descriptor)
    }

    fn build_descriptor_block(
        &self,
        sequence: u32,
        entries: &[JournalDescriptorEntry<'_>],
    ) -> Result<Vec<u8>> {
        if entries.is_empty() {
            return_errno_with_message!(Errno::EINVAL, "descriptor block has no tags");
        }

        let block_size = self.device.fs_block_size() as usize;
        let mut descriptor = vec![0u8; block_size];
        descriptor[..size_of::<JournalHeader>()]
            .copy_from_slice(self.header_bytes(JBD2_DESCRIPTOR_BLOCK, sequence).as_slice());

        let has_csum = self.superblock.has_checksum_v2_or_v3();
        let tag_len = self.tag_length();
        let tail_len = if has_csum { size_of::<JournalBlockTail>() } else { 0 };
        let needed_len = size_of::<JournalHeader>()
            .saturating_add(tag_len.saturating_mul(entries.len()))
            .saturating_add(tail_len);
        if needed_len > block_size {
            return_errno_with_message!(Errno::ENOSPC, "descriptor block does not have enough tag space");
        }

        let mut tag_offset = size_of::<JournalHeader>();
        for (index, entry) in entries.iter().enumerate() {
            let mut flags = JBD2_FLAG_SAME_UUID;
            if index + 1 == entries.len() {
                flags |= JBD2_FLAG_LAST_TAG;
            }
            if entry.escaped {
                flags |= JBD2_FLAG_ESCAPE;
            }

            if self.superblock.has_incompat_feature(JBD2_FEATURE_INCOMPAT_CSUM_V3) {
                let checksum = if has_csum {
                    self.data_block_checksum(sequence, entry.checksum_data)
                } else {
                    0
                };
                let tag = JournalBlockTag3::new(entry.target_fs_block, checksum, flags);
                let tag_bytes = unsafe {
                    core::slice::from_raw_parts(
                        &tag as *const _ as *const u8,
                        size_of::<JournalBlockTag3>(),
                    )
                };
                descriptor[tag_offset..tag_offset + tag_bytes.len()].copy_from_slice(tag_bytes);
            } else {
                let checksum = if has_csum {
                    (self.data_block_checksum(sequence, entry.checksum_data) & 0xFFFF) as u16
                } else {
                    0
                };
                let tag = JournalBlockTag::new(
                    entry.target_fs_block as u32,
                    checksum,
                    (flags & 0xFFFF) as u16,
                );
                let tag_bytes = unsafe {
                    core::slice::from_raw_parts(&tag as *const _ as *const u8, tag_len)
                };
                descriptor[tag_offset..tag_offset + tag_bytes.len()].copy_from_slice(tag_bytes);
            }
            tag_offset += tag_len;
        }

        if has_csum {
            let tail_offset = block_size - size_of::<JournalBlockTail>();
            let mut csum_data = Vec::with_capacity(16 + block_size);
            csum_data.extend_from_slice(self.superblock.uuid().as_slice());
            csum_data.extend_from_slice(&descriptor);
            let checksum = ext4_crc32c(EXT4_CRC32_INIT, &csum_data, csum_data.len() as u32);
            let tail = JournalBlockTail::new(checksum);
            let tail_bytes = unsafe {
                core::slice::from_raw_parts(
                    &tail as *const _ as *const u8,
                    size_of::<JournalBlockTail>(),
                )
            };
            descriptor[tail_offset..tail_offset + tail_bytes.len()].copy_from_slice(tail_bytes);
        }

        Ok(descriptor)
    }

    fn build_commit_block(&self, sequence: u32) -> Vec<u8> {
        let block_size = self.device.fs_block_size() as usize;
        let mut bytes = vec![0u8; block_size];
        let mut commit = CommitBlock::new(sequence);

        if self.superblock.has_checksum_v2_or_v3() {
            let checksum = self.commit_block_checksum(&commit);
            commit = commit.with_checksum(checksum);
        }

        let commit_bytes = unsafe {
            core::slice::from_raw_parts(&commit as *const _ as *const u8, size_of::<CommitBlock>())
        };
        bytes[..commit_bytes.len()].copy_from_slice(commit_bytes);
        bytes
    }

    fn header_bytes(&self, blocktype: u32, sequence: u32) -> [u8; 12] {
        let header = JournalHeader::new(blocktype, sequence);
        let mut bytes = [0u8; 12];
        let header_bytes = unsafe {
            core::slice::from_raw_parts(&header as *const _ as *const u8, bytes.len())
        };
        bytes.copy_from_slice(header_bytes);
        bytes
    }

    fn data_block_checksum(&self, sequence: u32, data: &[u8]) -> u32 {
        let mut csum_data = Vec::with_capacity(16 + 4 + data.len());
        csum_data.extend_from_slice(self.superblock.uuid().as_slice());
        csum_data.extend_from_slice(&sequence.to_be_bytes());
        csum_data.extend_from_slice(data);
        ext4_crc32c(EXT4_CRC32_INIT, &csum_data, csum_data.len() as u32)
    }

    fn commit_block_checksum(&self, commit: &CommitBlock) -> u32 {
        let mut csum_data = Vec::with_capacity(16 + size_of::<CommitBlock>());
        csum_data.extend_from_slice(self.superblock.uuid().as_slice());
        let commit_bytes = unsafe {
            core::slice::from_raw_parts(commit as *const _ as *const u8, size_of::<CommitBlock>())
        };
        csum_data.extend_from_slice(commit_bytes);
        ext4_crc32c(EXT4_CRC32_INIT, &csum_data, csum_data.len() as u32)
    }

    fn tag_length(&self) -> usize {
        if self.superblock.has_incompat_feature(JBD2_FEATURE_INCOMPAT_CSUM_V3) {
            size_of::<JournalBlockTag3>()
        } else if self.superblock.has_incompat_feature(JBD2_FEATURE_INCOMPAT_64BIT) {
            12
        } else {
            8
        }
    }

    fn escape_journal_data(&self, data: &[u8]) -> EscapedJournalBlock {
        let mut journal_data = data.to_vec();
        let magic_bytes = JBD2_MAGIC_NUMBER.to_be_bytes();
        let escaped = journal_data.len() >= magic_bytes.len() && journal_data[..magic_bytes.len()] == magic_bytes;
        if escaped {
            journal_data[..magic_bytes.len()].fill(0);
        }
        EscapedJournalBlock { data: journal_data, escaped }
    }
}

struct JournalDescriptorEntry<'a> {
    target_fs_block: u64,
    data: Vec<u8>,
    checksum_data: &'a [u8],
    escaped: bool,
}

struct EscapedJournalBlock {
    data: Vec<u8>,
    escaped: bool,
}

impl Ext4 {
    pub fn load_journal(&self) -> Result<Option<Jbd2Journal>> {
        if !self.super_block.has_journal() {
            return Ok(None);
        }

        Ok(Some(Jbd2Journal::load(self)?))
    }
}
