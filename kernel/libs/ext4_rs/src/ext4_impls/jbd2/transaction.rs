use crate::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalTransactionState {
    Running,
    Locked,
    Flush,
    Commit,
    Checkpoint,
}

#[derive(Debug, Clone)]
pub struct JournalWriteRegion {
    pub offset_in_block: u32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct JournalBuffer {
    pub block_nr: u64,
    pub block_data: Vec<u8>,
    pub dirty_ranges: Vec<JournalWriteRegion>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalCheckpointRange {
    pub start_block: u32,
    pub next_head: u32,
}

#[derive(Debug, Clone)]
pub struct JournalTransaction {
    tid: u32,
    state: JournalTransactionState,
    buffers: BTreeMap<u64, JournalBuffer>,
    handle_count: u32,
    reserved_blocks: u32,
    admitted_reserved_blocks: u32,
    data_sync_required: bool,
    trigger_op: Option<&'static str>,
    checkpoint_range: Option<JournalCheckpointRange>,
}

impl JournalTransaction {
    pub fn new(tid: u32) -> Self {
        Self {
            tid,
            state: JournalTransactionState::Running,
            buffers: BTreeMap::new(),
            handle_count: 0,
            reserved_blocks: 0,
            admitted_reserved_blocks: 0,
            data_sync_required: false,
            trigger_op: None,
            checkpoint_range: None,
        }
    }

    pub fn tid(&self) -> u32 {
        self.tid
    }

    pub fn state(&self) -> JournalTransactionState {
        self.state
    }

    pub fn set_state(&mut self, state: JournalTransactionState) {
        self.state = state;
    }

    pub fn handle_count(&self) -> u32 {
        self.handle_count
    }

    pub fn reserved_blocks(&self) -> u32 {
        self.reserved_blocks
    }

    pub fn admitted_reserved_blocks(&self) -> u32 {
        self.admitted_reserved_blocks
    }

    pub fn data_sync_required(&self) -> bool {
        self.data_sync_required
    }

    pub fn checkpoint_range(&self) -> Option<JournalCheckpointRange> {
        self.checkpoint_range
    }

    pub fn trigger_op(&self) -> Option<&'static str> {
        self.trigger_op
    }

    pub fn buffers(&self) -> &BTreeMap<u64, JournalBuffer> {
        &self.buffers
    }

    pub fn modified_block_count(&self) -> usize {
        self.buffers.len()
    }

    pub fn buffer(&self, block_nr: u64) -> Option<&JournalBuffer> {
        self.buffers.get(&block_nr)
    }

    pub fn has_buffer(&self, block_nr: u64) -> bool {
        self.buffers.contains_key(&block_nr)
    }

    pub fn register_handle(&mut self, reserved_blocks: u32, trigger_op: Option<&'static str>) {
        self.handle_count = self.handle_count.saturating_add(1);
        self.reserved_blocks = self.reserved_blocks.saturating_add(reserved_blocks);
        self.admitted_reserved_blocks = self
            .admitted_reserved_blocks
            .saturating_add(reserved_blocks);
        if trigger_op.is_some() {
            self.trigger_op = trigger_op;
        }
    }

    pub fn unregister_handle(&mut self, reserved_blocks: u32) {
        self.handle_count = self.handle_count.saturating_sub(1);
        self.reserved_blocks = self.reserved_blocks.saturating_sub(reserved_blocks);
    }

    pub fn require_data_sync(&mut self) {
        self.data_sync_required = true;
    }

    pub fn set_checkpoint_range(&mut self, start_block: u32, next_head: u32) {
        self.checkpoint_range = Some(JournalCheckpointRange {
            start_block,
            next_head,
        });
    }

    pub fn record_metadata_write<F>(
        &mut self,
        block_nr: u64,
        offset_in_block: u32,
        data: &[u8],
        block_size: usize,
        load_block: F,
    ) where
        F: FnOnce() -> Vec<u8>,
    {
        let buffer = self.buffers.entry(block_nr).or_insert_with(|| {
            let mut block_data = load_block();
            if block_data.len() < block_size {
                block_data.resize(block_size, 0);
            } else if block_data.len() > block_size {
                block_data.truncate(block_size);
            }

            JournalBuffer {
                block_nr,
                block_data,
                dirty_ranges: Vec::new(),
            }
        });

        let start = offset_in_block as usize;
        let end = start.saturating_add(data.len());
        if end > buffer.block_data.len() {
            let grow_to = end.max(block_size);
            buffer.block_data.resize(grow_to, 0);
        }
        buffer.block_data[start..end].copy_from_slice(data);
        buffer.dirty_ranges.push(JournalWriteRegion {
            offset_in_block,
            data: data.to_vec(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_metadata_write_builds_full_block_image() {
        let mut tx = JournalTransaction::new(7);
        tx.record_metadata_write(11, 2, &[1, 2, 3], 8, || vec![9; 8]);

        let buffer = tx.buffer(11).unwrap();
        assert_eq!(buffer.block_data, vec![9, 9, 1, 2, 3, 9, 9, 9]);
        assert_eq!(buffer.dirty_ranges.len(), 1);
        assert_eq!(buffer.dirty_ranges[0].offset_in_block, 2);
        assert_eq!(buffer.dirty_ranges[0].data, vec![1, 2, 3]);
    }

    #[test]
    fn record_metadata_write_reuses_cached_block_image() {
        let mut tx = JournalTransaction::new(9);
        tx.record_metadata_write(3, 0, &[1, 2], 4, || vec![0; 4]);
        tx.record_metadata_write(3, 2, &[3, 4], 4, || {
            panic!("cached block image should be reused")
        });

        let buffer = tx.buffer(3).unwrap();
        assert_eq!(buffer.block_data, vec![1, 2, 3, 4]);
        assert_eq!(buffer.dirty_ranges.len(), 2);
    }
}
