use crate::prelude::*;

#[derive(Debug, Clone)]
pub struct JournalHandle {
    transaction_id: u32,
    reserved_blocks: u32,
    modified_blocks: BTreeSet<u64>,
    data_sync_required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalHandleSummary {
    pub transaction_id: u32,
    pub reserved_blocks: u32,
    pub modified_blocks: u32,
    pub data_sync_required: bool,
}

impl JournalHandle {
    pub fn new(transaction_id: u32, reserved_blocks: u32) -> Self {
        Self {
            transaction_id,
            reserved_blocks,
            modified_blocks: BTreeSet::new(),
            data_sync_required: false,
        }
    }

    pub fn transaction_id(&self) -> u32 {
        self.transaction_id
    }

    pub fn reserved_blocks(&self) -> u32 {
        self.reserved_blocks
    }

    pub fn modified_blocks(&self) -> &BTreeSet<u64> {
        &self.modified_blocks
    }

    pub fn record_metadata_block(&mut self, block_nr: u64) {
        self.modified_blocks.insert(block_nr);
    }

    pub fn require_data_sync(&mut self) {
        self.data_sync_required = true;
    }

    pub fn data_sync_required(&self) -> bool {
        self.data_sync_required
    }

    pub fn summary(&self) -> JournalHandleSummary {
        JournalHandleSummary {
            transaction_id: self.transaction_id,
            reserved_blocks: self.reserved_blocks,
            modified_blocks: self.modified_blocks.len() as u32,
            data_sync_required: self.data_sync_required,
        }
    }
}
