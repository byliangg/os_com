use crate::ext4_defs::*;
use crate::prelude::*;

use super::{Jbd2Journal, JournalCommitBlock, JournalSpace};

#[derive(Debug, Clone)]
pub struct JournalRecoveryResult {
    pub transactions_replayed: u32,
    pub metadata_blocks_replayed: u32,
    pub revoked_blocks: u32,
    pub last_sequence: Option<u32>,
}

#[derive(Debug, Clone)]
struct JournalRecoveryTransaction {
    sequence: u32,
    next_head: u32,
    metadata_blocks: Vec<JournalCommitBlock>,
}

#[derive(Debug, Clone, Copy)]
struct DescriptorTag {
    target_fs_block: u64,
    flags: u32,
}

impl Jbd2Journal {
    pub fn needs_recovery(&self) -> bool {
        self.superblock.start() != 0
    }

    pub fn recover(&mut self, block_device: &Arc<dyn BlockDevice>) -> Result<JournalRecoveryResult> {
        if !self.needs_recovery() {
            return Ok(JournalRecoveryResult {
                transactions_replayed: 0,
                metadata_blocks_replayed: 0,
                revoked_blocks: 0,
                last_sequence: None,
            });
        }

        let transactions = self.scan_committed_transactions(block_device)?;
        let last_sequence = transactions.last().map(|tx| tx.sequence);
        let end = transactions
            .last()
            .map(|tx| tx.next_head)
            .unwrap_or(self.superblock.start());
        let revoked = self.scan_revoke_blocks(block_device, self.superblock.start(), end)?;

        let mut metadata_blocks_replayed = 0u32;
        for transaction in &transactions {
            for metadata in &transaction.metadata_blocks {
                if revoked.contains(&metadata.block_nr) {
                    continue;
                }
                let offset = (metadata.block_nr as usize)
                    .checked_mul(self.device.fs_block_size() as usize)
                    .ok_or_else(|| {
                        Ext4Error::with_message(Errno::EINVAL, "recovery block offset overflow")
                    })?;
                block_device.write_offset(offset, &metadata.block_data);
                metadata_blocks_replayed = metadata_blocks_replayed.saturating_add(1);
            }
        }

        let next_sequence = last_sequence
            .map(|sequence| sequence.saturating_add(1))
            .unwrap_or(self.superblock.sequence());
        self.reset_recovered_state(block_device, next_sequence)?;

        Ok(JournalRecoveryResult {
            transactions_replayed: transactions.len() as u32,
            metadata_blocks_replayed,
            revoked_blocks: revoked.len() as u32,
            last_sequence,
        })
    }

    fn reset_recovered_state(
        &mut self,
        block_device: &Arc<dyn BlockDevice>,
        next_sequence: u32,
    ) -> Result<()> {
        let first = self.superblock.first();
        self.superblock.update_sequence(next_sequence);
        self.superblock.update_start(0);
        self.superblock.update_head(first);
        self.superblock.store(block_device, &self.device)?;
        self.space = JournalSpace::new(first, self.superblock.maxlen(), first, first)?;
        Ok(())
    }

    fn scan_committed_transactions(
        &self,
        block_device: &Arc<dyn BlockDevice>,
    ) -> Result<Vec<JournalRecoveryTransaction>> {
        let start = self.superblock.start();
        if start == 0 {
            return Ok(Vec::new());
        }

        let head = if self.superblock.head() == 0 {
            start
        } else {
            self.superblock.head()
        };
        let mut cursor = start;
        let mut walked_blocks = 0u32;
        let mut transactions = Vec::new();
        let max_walk = self.space.usable_blocks().max(1);

        while walked_blocks < max_walk {
            let Some(transaction) = self.try_read_committed_transaction(block_device, cursor)? else {
                break;
            };
            let advanced = self.space.distance(cursor, transaction.next_head).max(1);
            walked_blocks = walked_blocks.saturating_add(advanced);
            cursor = transaction.next_head;
            transactions.push(transaction);
            if cursor == head {
                break;
            }
        }

        Ok(transactions)
    }

    fn try_read_committed_transaction(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        descriptor_block: u32,
    ) -> Result<Option<JournalRecoveryTransaction>> {
        let descriptor_raw = self.device.read_raw_block(block_device, descriptor_block)?;
        let Some(header) = self.read_header(&descriptor_raw) else {
            return Ok(None);
        };
        if header.blocktype() != JBD2_DESCRIPTOR_BLOCK {
            return Ok(None);
        }

        let descriptor_tags = match self.parse_descriptor_tags(&descriptor_raw) {
            Some(tags) if !tags.is_empty() => tags,
            _ => return Ok(None),
        };

        let mut data_cursor = self.space.advance(descriptor_block, 1);
        let mut metadata_blocks = Vec::with_capacity(descriptor_tags.len());
        for tag in &descriptor_tags {
            let mut block_data = self.device.read_raw_block(block_device, data_cursor)?;
            if (tag.flags & JBD2_FLAG_ESCAPE) != 0 && block_data.len() >= size_of::<u32>() {
                block_data[..size_of::<u32>()].copy_from_slice(&JBD2_MAGIC_NUMBER.to_be_bytes());
            }
            metadata_blocks.push(JournalCommitBlock {
                block_nr: tag.target_fs_block,
                block_data,
            });
            data_cursor = self.space.advance(data_cursor, 1);
        }

        let commit_raw = self.device.read_raw_block(block_device, data_cursor)?;
        let Some(commit_header) = self.read_header(&commit_raw) else {
            return Ok(None);
        };
        if commit_header.blocktype() != JBD2_COMMIT_BLOCK || commit_header.sequence() != header.sequence() {
            return Ok(None);
        }

        let next_head = self.space.advance(data_cursor, 1);
        Ok(Some(JournalRecoveryTransaction {
            sequence: header.sequence(),
            next_head,
            metadata_blocks,
        }))
    }

    fn scan_revoke_blocks(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        start: u32,
        end: u32,
    ) -> Result<BTreeSet<u64>> {
        let mut revoked = BTreeSet::new();
        if start == end {
            return Ok(revoked);
        }

        let mut cursor = start;
        let max_walk = self.space.distance(start, end).max(1);
        let mut walked = 0u32;
        while walked < max_walk {
            let raw = self.device.read_raw_block(block_device, cursor)?;
            if let Some(header) = self.read_header(&raw) {
                if header.blocktype() == JBD2_REVOKE_BLOCK {
                    self.parse_revoke_entries(&raw, &mut revoked);
                }
            }
            cursor = self.space.advance(cursor, 1);
            walked = walked.saturating_add(1);
            if cursor == end {
                break;
            }
        }

        Ok(revoked)
    }

    fn parse_revoke_entries(&self, raw: &[u8], revoked: &mut BTreeSet<u64>) {
        if raw.len() < size_of::<RevokeBlockHeader>() {
            return;
        }
        let header = unsafe { (raw.as_ptr() as *const RevokeBlockHeader).read_unaligned() };
        let used = header.count() as usize;
        if used < size_of::<RevokeBlockHeader>() || used > raw.len() {
            return;
        }

        let entry_size = self.superblock.raw().blocknr_size();
        let mut offset = size_of::<RevokeBlockHeader>();
        while offset + entry_size <= used {
            let block_nr = if entry_size == size_of::<u64>() {
                let bytes: [u8; 8] = raw[offset..offset + 8].try_into().unwrap();
                u64::from_be_bytes(bytes)
            } else {
                let bytes: [u8; 4] = raw[offset..offset + 4].try_into().unwrap();
                u32::from_be_bytes(bytes) as u64
            };
            revoked.insert(block_nr);
            offset += entry_size;
        }
    }

    fn parse_descriptor_tags(&self, raw: &[u8]) -> Option<Vec<DescriptorTag>> {
        let block_size = self.device.fs_block_size() as usize;
        if raw.len() < block_size || raw.len() < size_of::<JournalHeader>() {
            return None;
        }

        let tag_len = self.tag_length();
        let tail_len = if self.superblock.has_checksum_v2_or_v3() {
            size_of::<JournalBlockTail>()
        } else {
            0
        };
        let limit = block_size.checked_sub(tail_len)?;
        let mut offset = size_of::<JournalHeader>();
        let mut tags = Vec::new();
        while offset + tag_len <= limit {
            let tag = self.read_descriptor_tag(raw, offset)?;
            let flags = tag.flags;
            tags.push(tag);
            offset += tag_len;
            if (flags & JBD2_FLAG_LAST_TAG) != 0 {
                return Some(tags);
            }
        }
        None
    }

    fn read_descriptor_tag(&self, raw: &[u8], offset: usize) -> Option<DescriptorTag> {
        if self.superblock.has_incompat_feature(JBD2_FEATURE_INCOMPAT_CSUM_V3) {
            let bytes = raw.get(offset..offset + size_of::<JournalBlockTag3>())?;
            let tag = unsafe { (bytes.as_ptr() as *const JournalBlockTag3).read_unaligned() };
            Some(DescriptorTag {
                target_fs_block: tag.blocknr(),
                flags: tag.flags(),
            })
        } else if self.superblock.has_incompat_feature(JBD2_FEATURE_INCOMPAT_64BIT) {
            let bytes = raw.get(offset..offset + 12)?;
            let low = u32::from_be_bytes(bytes[0..4].try_into().ok()?);
            let checksum_and_flags = &bytes[4..8];
            let flags = u16::from_be_bytes(checksum_and_flags[2..4].try_into().ok()?) as u32;
            let high = u32::from_be_bytes(bytes[8..12].try_into().ok()?);
            Some(DescriptorTag {
                target_fs_block: ((high as u64) << 32) | low as u64,
                flags,
            })
        } else {
            let bytes = raw.get(offset..offset + size_of::<JournalBlockTag>())?;
            let tag = unsafe { (bytes.as_ptr() as *const JournalBlockTag).read_unaligned() };
            Some(DescriptorTag {
                target_fs_block: tag.blocknr() as u64,
                flags: tag.flags() as u32,
            })
        }
    }

    fn read_header(&self, raw: &[u8]) -> Option<JournalHeader> {
        let bytes = raw.get(..size_of::<JournalHeader>())?;
        let header = unsafe { (bytes.as_ptr() as *const JournalHeader).read_unaligned() };
        header.is_valid_magic().then_some(header)
    }
}
