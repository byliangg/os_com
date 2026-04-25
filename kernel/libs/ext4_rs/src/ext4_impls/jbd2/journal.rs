use crate::prelude::*;
use core::cmp::{max, min};

use super::{
    JournalCheckpointRange, JournalHandle, JournalHandleSummary, JournalTransaction,
    JournalTransactionState,
};

const JOURNAL_TRANSACTION_CREDIT_SOFT_LIMIT: u32 = 1024;

#[derive(Debug, Clone)]
pub struct JournalCommitBlock {
    pub block_nr: u64,
    pub block_data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct JournalCommitPlan {
    pub tid: u32,
    pub reserved_blocks: u32,
    pub data_sync_required: bool,
    pub trigger_op: Option<&'static str>,
    pub metadata_blocks: Vec<JournalCommitBlock>,
}

#[derive(Debug, Clone)]
pub struct JournalCheckpointPlan {
    pub tid: u32,
    pub range: JournalCheckpointRange,
    pub metadata_blocks: Vec<JournalCommitBlock>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JournalRuntimeDebugStats {
    pub started_handles: u64,
    pub finished_handles: u64,
    pub active_handle_samples: u64,
    pub active_handle_sample_sum: u64,
    pub max_active_handles: u32,
    pub max_running_handles: u32,
    pub max_running_reserved_blocks: u32,
    pub max_running_metadata_blocks: u32,
    pub rotated_transactions: u64,
    pub prepared_commits: u64,
    pub finished_commits: u64,
    pub finished_checkpoints: u64,
    pub overlay_reads: u64,
    pub overlay_hits: u64,
    pub metadata_write_records: u64,
}

#[derive(Debug, Clone)]
pub struct JournalRuntime {
    enabled: bool,
    block_size: usize,
    next_tid: u32,
    next_handle_id: u64,
    running: Option<JournalTransaction>,
    prev_running: Option<JournalTransaction>,
    committing: Option<JournalTransaction>,
    checkpoint_list: VecDeque<JournalTransaction>,
    active_handles: VecDeque<JournalHandle>,
    debug_stats: JournalRuntimeDebugStats,
}

impl JournalRuntime {
    pub fn new(block_size: usize, first_tid: u32) -> Self {
        Self {
            enabled: true,
            block_size,
            next_tid: first_tid.max(1),
            next_handle_id: 1,
            running: None,
            prev_running: None,
            committing: None,
            checkpoint_list: VecDeque::new(),
            active_handles: VecDeque::new(),
            debug_stats: JournalRuntimeDebugStats::default(),
        }
    }

    pub fn disabled(block_size: usize) -> Self {
        Self {
            enabled: false,
            block_size,
            next_tid: 1,
            next_handle_id: 1,
            running: None,
            prev_running: None,
            committing: None,
            checkpoint_list: VecDeque::new(),
            active_handles: VecDeque::new(),
            debug_stats: JournalRuntimeDebugStats::default(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }

    pub fn running_transaction(&self) -> Option<&JournalTransaction> {
        self.running.as_ref()
    }

    pub fn prev_running_transaction(&self) -> Option<&JournalTransaction> {
        self.prev_running.as_ref()
    }

    pub fn committing_transaction(&self) -> Option<&JournalTransaction> {
        self.committing.as_ref()
    }

    pub fn checkpoint_depth(&self) -> usize {
        self.checkpoint_list.len()
    }

    pub fn active_handle(&self) -> Option<&JournalHandle> {
        self.active_handles.front()
    }

    pub fn has_active_handle(&self) -> bool {
        !self.active_handles.is_empty()
    }

    pub fn debug_stats(&self) -> JournalRuntimeDebugStats {
        self.debug_stats
    }

    pub fn should_defer_metadata_write(&self) -> bool {
        self.enabled && !self.active_handles.is_empty()
    }

    fn observe_active_state(&mut self) {
        let active_handles = self.active_handles.len() as u64;
        self.debug_stats.active_handle_samples =
            self.debug_stats.active_handle_samples.saturating_add(1);
        self.debug_stats.active_handle_sample_sum = self
            .debug_stats
            .active_handle_sample_sum
            .saturating_add(active_handles);
        self.debug_stats.max_active_handles = self
            .debug_stats
            .max_active_handles
            .max(active_handles as u32);
        if let Some(transaction) = self.running.as_ref() {
            self.debug_stats.max_running_handles = self
                .debug_stats
                .max_running_handles
                .max(transaction.handle_count());
            self.debug_stats.max_running_reserved_blocks = self
                .debug_stats
                .max_running_reserved_blocks
                .max(transaction.admitted_reserved_blocks());
            self.debug_stats.max_running_metadata_blocks = self
                .debug_stats
                .max_running_metadata_blocks
                .max(transaction.modified_block_count() as u32);
        }
    }

    fn latest_metadata_buffer(&self, block_nr: u64) -> Option<&super::JournalBuffer> {
        if let Some(transaction) = self.running.as_ref() {
            if let Some(buffer) = transaction.buffer(block_nr) {
                return Some(buffer);
            }
        }

        if let Some(transaction) = self.prev_running.as_ref() {
            if let Some(buffer) = transaction.buffer(block_nr) {
                return Some(buffer);
            }
        }

        if let Some(transaction) = self.committing.as_ref() {
            if let Some(buffer) = transaction.buffer(block_nr) {
                return Some(buffer);
            }
        }

        for transaction in self.checkpoint_list.iter().rev() {
            if let Some(buffer) = transaction.buffer(block_nr) {
                return Some(buffer);
            }
        }

        None
    }

    pub fn overlay_metadata_read(&mut self, offset: usize, out: &mut [u8]) -> bool {
        if !self.enabled || self.block_size == 0 || out.is_empty() {
            return false;
        }

        self.debug_stats.overlay_reads = self.debug_stats.overlay_reads.saturating_add(1);
        let Some(end) = offset.checked_add(out.len()) else {
            return false;
        };
        let first_block = offset / self.block_size;
        let last_block = (end - 1) / self.block_size;
        let mut overlaid = false;

        for block_nr in first_block..=last_block {
            let Some(buffer) = self.latest_metadata_buffer(block_nr as u64) else {
                continue;
            };

            let block_start = block_nr * self.block_size;
            let block_end = block_start + self.block_size;
            let overlap_start = max(offset, block_start);
            let overlap_end = min(end, block_end);
            if overlap_start >= overlap_end {
                continue;
            }

            let out_start = overlap_start - offset;
            let out_end = overlap_end - offset;
            let buf_start = overlap_start - block_start;
            let buf_end = overlap_end - block_start;
            out[out_start..out_end].copy_from_slice(&buffer.block_data[buf_start..buf_end]);
            overlaid = true;
        }

        if overlaid {
            self.debug_stats.overlay_hits = self.debug_stats.overlay_hits.saturating_add(1);
        }
        overlaid
    }

    pub fn commit_ready(&self) -> bool {
        if self.prev_running.as_ref().is_some_and(|transaction| {
            transaction.handle_count() == 0 && transaction.modified_block_count() != 0
        }) {
            return true;
        }

        if self.prev_running.is_some() {
            return false;
        }

        self.running.as_ref().is_some_and(|transaction| {
            transaction.handle_count() == 0 && transaction.modified_block_count() != 0
        })
    }

    pub fn batch_commit_ready(&self, threshold_blocks: u32) -> bool {
        if self.prev_running.as_ref().is_some_and(|transaction| {
            transaction.handle_count() == 0 && transaction.modified_block_count() != 0
        }) {
            return true;
        }

        if self.prev_running.is_some() {
            return false;
        }

        self.running.as_ref().is_some_and(|transaction| {
            transaction.handle_count() == 0
                && transaction.modified_block_count() as u32 >= threshold_blocks
        })
    }

    pub fn should_rotate_running_transaction(&self, threshold_blocks: u32) -> bool {
        self.enabled
            && self.prev_running.is_none()
            && self.running.as_ref().is_some_and(|transaction| {
                transaction.handle_count() != 0
                    && transaction.modified_block_count() != 0
                    && transaction.modified_block_count() as u32 >= threshold_blocks
            })
    }

    fn should_rotate_for_new_handle(&self, reserved_blocks: u32) -> bool {
        self.enabled
            && self.prev_running.is_none()
            && self.running.as_ref().is_some_and(|transaction| {
                transaction.handle_count() == 0
                    && transaction.modified_block_count() != 0
                    && transaction
                        .admitted_reserved_blocks()
                        .saturating_add(reserved_blocks)
                        > JOURNAL_TRANSACTION_CREDIT_SOFT_LIMIT
            })
    }

    fn rotate_running_transaction_for_admission(&mut self, reserved_blocks: u32) -> Option<u32> {
        if !self.should_rotate_for_new_handle(reserved_blocks) {
            return None;
        }

        let mut transaction = self.running.take()?;
        transaction.set_state(JournalTransactionState::Locked);
        let tid = transaction.tid();
        self.prev_running = Some(transaction);
        self.debug_stats.rotated_transactions =
            self.debug_stats.rotated_transactions.saturating_add(1);
        Some(tid)
    }

    pub fn rotate_running_transaction(&mut self) -> Option<u32> {
        if !self.should_rotate_running_transaction(0) {
            return None;
        }

        let mut transaction = self.running.take()?;
        transaction.set_state(JournalTransactionState::Locked);
        let tid = transaction.tid();
        self.prev_running = Some(transaction);
        self.debug_stats.rotated_transactions =
            self.debug_stats.rotated_transactions.saturating_add(1);
        Some(tid)
    }

    pub fn checkpoint_ready(&self) -> bool {
        !self.checkpoint_list.is_empty()
    }

    pub fn discard_checkpointed_before_tail(&mut self, current_tail: u32) -> usize {
        let Some(front) = self.checkpoint_list.front() else {
            return 0;
        };
        if front
            .checkpoint_range()
            .is_some_and(|range| range.start_block == current_tail)
        {
            return 0;
        }

        let Some(keep_index) = self.checkpoint_list.iter().position(|transaction| {
            transaction
                .checkpoint_range()
                .is_some_and(|range| range.start_block == current_tail)
        }) else {
            let all_released = self
                .checkpoint_list
                .back()
                .and_then(|transaction| transaction.checkpoint_range())
                .is_some_and(|range| range.next_head == current_tail);
            if !all_released {
                return 0;
            }
            let dropped = self.checkpoint_list.len();
            self.checkpoint_list.clear();
            return dropped;
        };

        for _ in 0..keep_index {
            self.checkpoint_list.pop_front();
        }
        keep_index
    }

    pub fn start_handle(
        &mut self,
        reserved_blocks: u32,
        trigger_op: Option<&'static str>,
    ) -> Option<JournalHandle> {
        if !self.enabled {
            return None;
        }

        self.rotate_running_transaction_for_admission(reserved_blocks);
        let handle_id = self.next_handle_id;
        self.next_handle_id = self.next_handle_id.saturating_add(1).max(1);
        let transaction_id = {
            let transaction = self.running.get_or_insert_with(|| {
                let tid = self.next_tid;
                self.next_tid = self.next_tid.saturating_add(1);
                JournalTransaction::new(tid)
            });
            // If a previous handle set state to Locked (handle_count dropped to 0),
            // reset it to Running so the transaction correctly tracks active handles.
            if transaction.state() == JournalTransactionState::Locked {
                transaction.set_state(JournalTransactionState::Running);
            }
            transaction.register_handle(reserved_blocks, trigger_op);
            transaction.tid()
        };

        let handle = JournalHandle::new(handle_id, transaction_id, reserved_blocks);
        self.active_handles.push_back(handle.clone());
        self.debug_stats.started_handles = self.debug_stats.started_handles.saturating_add(1);
        self.observe_active_state();
        Some(handle)
    }

    fn transaction_mut(&mut self, tid: u32) -> Option<&mut JournalTransaction> {
        if self
            .running
            .as_ref()
            .is_some_and(|transaction| transaction.tid() == tid)
        {
            return self.running.as_mut();
        }
        if self
            .prev_running
            .as_ref()
            .is_some_and(|transaction| transaction.tid() == tid)
        {
            return self.prev_running.as_mut();
        }
        if self
            .committing
            .as_ref()
            .is_some_and(|transaction| transaction.tid() == tid)
        {
            return self.committing.as_mut();
        }
        self.checkpoint_list
            .iter_mut()
            .find(|transaction| transaction.tid() == tid)
    }

    fn remove_active_handle(&mut self, handle: JournalHandle) -> JournalHandle {
        let Some(pos) = self
            .active_handles
            .iter()
            .position(|active| active.handle_id() == handle.handle_id())
        else {
            return handle;
        };
        self.active_handles.remove(pos).unwrap_or(handle)
    }

    fn active_handle_mut_by_id(&mut self, handle_id: u64) -> Option<&mut JournalHandle> {
        self.active_handles
            .iter_mut()
            .find(|handle| handle.handle_id() == handle_id)
    }

    pub fn mark_handle_requires_data_sync(&mut self, handle_id: u64) {
        if !self.enabled {
            return;
        }

        let Some(transaction_id) = self.active_handle_mut_by_id(handle_id).map(|handle| {
            handle.require_data_sync();
            handle.transaction_id()
        }) else {
            return;
        };
        if let Some(transaction) = self.transaction_mut(transaction_id) {
            transaction.require_data_sync();
        }
    }

    pub fn record_metadata_write_for_handle<F>(
        &mut self,
        handle_id: u64,
        offset: usize,
        data: &[u8],
        mut load_block: F,
    )
    where
        F: FnMut(u64) -> Vec<u8>,
    {
        if !self.enabled || data.is_empty() || self.block_size == 0 {
            return;
        }

        let block_size = self.block_size;
        let mut consumed = 0usize;
        while consumed < data.len() {
            let write_offset = offset + consumed;
            let block_nr = (write_offset / block_size) as u64;
            let block_offset = write_offset % block_size;
            let chunk_len = min(block_size - block_offset, data.len() - consumed);
            let chunk = &data[consumed..consumed + chunk_len];
            let transaction_id = {
                let Some(handle) = self.active_handle_mut_by_id(handle_id) else {
                    return;
                };
                handle.record_metadata_block(block_nr);
                handle.transaction_id()
            };
            self.debug_stats.metadata_write_records =
                self.debug_stats.metadata_write_records.saturating_add(1);
            let needs_base_image = self
                .transaction_mut(transaction_id)
                .is_some_and(|transaction| !transaction.has_buffer(block_nr));
            let overlay_base = if needs_base_image {
                self.latest_metadata_buffer(block_nr)
                    .map(|buffer| buffer.block_data.clone())
            } else {
                None
            };
            let Some(transaction) = self.transaction_mut(transaction_id) else {
                return;
            };
            transaction.record_metadata_write(
                block_nr,
                block_offset as u32,
                chunk,
                block_size,
                || overlay_base.unwrap_or_else(|| load_block(block_nr)),
            );
            self.observe_active_state();

            consumed += chunk_len;
        }
    }

    pub fn stop_handle(&mut self, handle: JournalHandle) -> Option<JournalHandleSummary> {
        if !self.enabled {
            return None;
        }

        let active = self.remove_active_handle(handle);
        if let Some(transaction) = self.transaction_mut(active.transaction_id()) {
            transaction.unregister_handle(active.reserved_blocks());
            if transaction.handle_count() == 0 {
                transaction.set_state(JournalTransactionState::Locked);
            }
        }

        self.debug_stats.finished_handles = self.debug_stats.finished_handles.saturating_add(1);
        self.observe_active_state();
        Some(active.summary())
    }

    pub fn prepare_commit(&mut self) -> Option<JournalCommitPlan> {
        if !self.enabled || self.committing.is_some() {
            return None;
        }

        let mut transaction = if let Some(transaction) = self.prev_running.take() {
            transaction
        } else {
            self.running.take()?
        };
        if transaction.handle_count() != 0 {
            if self.running.is_some() {
                self.prev_running = Some(transaction);
            } else {
                self.running = Some(transaction);
            }
            return None;
        }

        transaction.set_state(JournalTransactionState::Commit);
        let metadata_blocks = transaction
            .buffers()
            .values()
            .map(|buffer| JournalCommitBlock {
                block_nr: buffer.block_nr,
                block_data: buffer.block_data.clone(),
            })
            .collect();
        let plan = JournalCommitPlan {
            tid: transaction.tid(),
            reserved_blocks: transaction.admitted_reserved_blocks(),
            data_sync_required: transaction.data_sync_required(),
            trigger_op: transaction.trigger_op(),
            metadata_blocks,
        };
        self.committing = Some(transaction);
        self.debug_stats.prepared_commits = self.debug_stats.prepared_commits.saturating_add(1);
        Some(plan)
    }

    pub fn finish_commit(&mut self, tid: u32, start_block: u32, next_head: u32) -> bool {
        let Some(mut transaction) = self.committing.take() else {
            return false;
        };
        if transaction.tid() != tid {
            self.committing = Some(transaction);
            return false;
        }

        transaction.set_state(JournalTransactionState::Checkpoint);
        transaction.set_checkpoint_range(start_block, next_head);
        self.checkpoint_list.push_back(transaction);
        self.debug_stats.finished_commits = self.debug_stats.finished_commits.saturating_add(1);
        true
    }

    pub fn abort_commit(&mut self, tid: u32) -> bool {
        let Some(mut transaction) = self.committing.take() else {
            return false;
        };
        if transaction.tid() != tid {
            self.committing = Some(transaction);
            return false;
        }

        transaction.set_state(JournalTransactionState::Running);
        if self.running.is_some() {
            self.prev_running = Some(transaction);
        } else {
            self.running = Some(transaction);
        }
        true
    }

    pub fn prepare_checkpoint(&self) -> Option<JournalCheckpointPlan> {
        let transaction = self.checkpoint_list.front()?;
        Some(JournalCheckpointPlan {
            tid: transaction.tid(),
            range: transaction.checkpoint_range()?,
            metadata_blocks: transaction
                .buffers()
                .values()
                .map(|buffer| JournalCommitBlock {
                    block_nr: buffer.block_nr,
                    block_data: buffer.block_data.clone(),
                })
                .collect(),
        })
    }

    pub fn finish_checkpoint(&mut self, tid: u32) -> Option<Option<u32>> {
        let transaction = self.checkpoint_list.front()?;
        if transaction.tid() != tid {
            return None;
        }
        self.checkpoint_list.pop_front();
        self.debug_stats.finished_checkpoints =
            self.debug_stats.finished_checkpoints.saturating_add(1);
        let next_start = self
            .checkpoint_list
            .front()
            .and_then(|next| next.checkpoint_range().map(|range| range.start_block));
        Some(next_start)
    }

    pub fn next_checkpoint_start_after(&self, tid: u32) -> Option<Option<u32>> {
        let transaction = self.checkpoint_list.front()?;
        if transaction.tid() != tid {
            return None;
        }
        let next_start = self
            .checkpoint_list
            .get(1)
            .and_then(|next| next.checkpoint_range().map(|range| range.start_block));
        Some(next_start)
    }

    /// Returns plans for all pending checkpoint transactions without removing them.
    /// Used for batch checkpointing (write all home blocks, one sync, then finish each).
    pub fn all_checkpoint_plans(&self) -> Vec<JournalCheckpointPlan> {
        self.checkpoint_list
            .iter()
            .filter_map(|transaction| {
                let range = transaction.checkpoint_range()?;
                Some(JournalCheckpointPlan {
                    tid: transaction.tid(),
                    range,
                    metadata_blocks: transaction
                        .buffers()
                        .values()
                        .map(|buffer| JournalCommitBlock {
                            block_nr: buffer.block_nr,
                            block_data: buffer.block_data.clone(),
                        })
                        .collect(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_metadata(
        runtime: &mut JournalRuntime,
        handle: &JournalHandle,
        offset: usize,
        data: &[u8],
    ) {
        runtime.record_metadata_write_for_handle(handle.handle_id(), offset, data, |_| vec![0; 8]);
    }

    #[test]
    fn prepare_commit_collects_full_metadata_blocks() {
        let mut runtime = JournalRuntime::new(8, 1);
        let handle = runtime.start_handle(4, None).unwrap();
        runtime.record_metadata_write_for_handle(handle.handle_id(), 2, &[1, 2, 3], |_| vec![9; 8]);
        let summary = runtime.stop_handle(handle).unwrap();

        assert_eq!(summary.handle_id, 1);
        assert_eq!(summary.modified_blocks, 1);
        assert!(runtime.commit_ready());

        let plan = runtime.prepare_commit().unwrap();
        assert_eq!(plan.tid, 1);
        assert_eq!(plan.metadata_blocks.len(), 1);
        assert_eq!(plan.metadata_blocks[0].block_nr, 0);
        assert_eq!(plan.metadata_blocks[0].block_data, vec![9, 9, 1, 2, 3, 9, 9, 9]);
        assert!(runtime.finish_commit(plan.tid, 5, 8));
        assert_eq!(runtime.checkpoint_depth(), 1);
    }

    #[test]
    fn checkpoint_plan_tracks_next_start() {
        let mut runtime = JournalRuntime::new(8, 1);

        let handle1 = runtime.start_handle(4, None).unwrap();
        record_metadata(&mut runtime, &handle1, 0, &[1]);
        runtime.stop_handle(handle1).unwrap();
        let plan1 = runtime.prepare_commit().unwrap();
        assert!(runtime.finish_commit(plan1.tid, 5, 7));

        let handle2 = runtime.start_handle(4, None).unwrap();
        record_metadata(&mut runtime, &handle2, 8, &[2]);
        runtime.stop_handle(handle2).unwrap();
        let plan2 = runtime.prepare_commit().unwrap();
        assert!(runtime.finish_commit(plan2.tid, 7, 9));

        let checkpoint1 = runtime.prepare_checkpoint().unwrap();
        assert_eq!(checkpoint1.tid, 1);
        assert_eq!(checkpoint1.range.start_block, 5);
        assert_eq!(checkpoint1.range.next_head, 7);
        assert_eq!(checkpoint1.metadata_blocks.len(), 1);
        assert_eq!(checkpoint1.metadata_blocks[0].block_nr, 0);
        assert_eq!(runtime.finish_checkpoint(checkpoint1.tid), Some(Some(7)));

        let checkpoint2 = runtime.prepare_checkpoint().unwrap();
        assert_eq!(checkpoint2.tid, 2);
        assert_eq!(checkpoint2.metadata_blocks.len(), 1);
        assert_eq!(checkpoint2.metadata_blocks[0].block_nr, 1);
        assert_eq!(runtime.finish_checkpoint(checkpoint2.tid), Some(None));
        assert!(!runtime.checkpoint_ready());
    }

    #[test]
    fn overlay_metadata_read_prefers_newest_transaction_state() {
        let mut runtime = JournalRuntime::new(8, 1);

        let handle1 = runtime.start_handle(4, None).unwrap();
        record_metadata(&mut runtime, &handle1, 8, &[1, 1, 1, 1]);
        runtime.stop_handle(handle1).unwrap();
        let plan1 = runtime.prepare_commit().unwrap();
        assert!(runtime.finish_commit(plan1.tid, 5, 7));

        let handle2 = runtime.start_handle(4, None).unwrap();
        runtime.record_metadata_write_for_handle(
            handle2.handle_id(),
            8,
            &[2, 2, 2, 2],
            |_| vec![9; 8],
        );

        let mut out = vec![0xFF; 8];
        assert!(runtime.overlay_metadata_read(8, &mut out));
        assert_eq!(out, vec![2, 2, 2, 2, 0, 0, 0, 0]);

        runtime.stop_handle(handle2).unwrap();
    }

    #[test]
    fn record_metadata_write_uses_overlay_base_before_disk_base() {
        let mut runtime = JournalRuntime::new(8, 1);

        let handle1 = runtime.start_handle(4, None).unwrap();
        record_metadata(&mut runtime, &handle1, 8, &[1, 1, 1, 1]);
        runtime.stop_handle(handle1).unwrap();
        let plan1 = runtime.prepare_commit().unwrap();
        assert!(runtime.finish_commit(plan1.tid, 5, 7));

        let handle2 = runtime.start_handle(4, None).unwrap();
        record_metadata(&mut runtime, &handle2, 12, &[2, 2]);

        let running = runtime.running_transaction().unwrap();
        let buffer = running.buffer(1).unwrap();
        assert_eq!(buffer.block_data, vec![1, 1, 1, 1, 2, 2, 0, 0]);

        runtime.stop_handle(handle2).unwrap();
    }

    #[test]
    fn stop_handle_matches_unique_handle_id_not_transaction_id() {
        let mut runtime = JournalRuntime::new(8, 1);

        let handle1 = runtime.start_handle(3, None).unwrap();
        let handle2 = runtime.start_handle(5, None).unwrap();
        assert_eq!(handle1.transaction_id(), handle2.transaction_id());
        assert_ne!(handle1.handle_id(), handle2.handle_id());

        record_metadata(&mut runtime, &handle2, 8, &[2]);
        let summary2 = runtime.stop_handle(handle2).unwrap();
        assert_eq!(summary2.handle_id, 2);
        assert_eq!(summary2.reserved_blocks, 5);
        assert_eq!(summary2.modified_blocks, 1);

        let running = runtime.running_transaction().unwrap();
        assert_eq!(running.handle_count(), 1);
        assert_eq!(running.reserved_blocks(), 3);

        let summary1 = runtime.stop_handle(handle1).unwrap();
        assert_eq!(summary1.handle_id, 1);
        assert_eq!(summary1.reserved_blocks, 3);
        assert_eq!(summary1.modified_blocks, 0);
        assert!(runtime.commit_ready());
    }

    #[test]
    fn data_sync_mark_targets_unique_handle_id() {
        let mut runtime = JournalRuntime::new(8, 1);

        let handle1 = runtime.start_handle(3, None).unwrap();
        let handle2 = runtime.start_handle(5, None).unwrap();
        runtime.mark_handle_requires_data_sync(handle2.handle_id());
        record_metadata(&mut runtime, &handle2, 8, &[2]);

        let summary1 = runtime.stop_handle(handle1).unwrap();
        let summary2 = runtime.stop_handle(handle2).unwrap();
        assert!(!summary1.data_sync_required);
        assert!(summary2.data_sync_required);

        let plan = runtime.prepare_commit().unwrap();
        assert!(plan.data_sync_required);
    }

    #[test]
    fn credit_admission_rotates_idle_transaction_before_overflow() {
        let mut runtime = JournalRuntime::new(8, 1);

        let handle1 = runtime.start_handle(1020, None).unwrap();
        record_metadata(&mut runtime, &handle1, 0, &[1]);
        runtime.stop_handle(handle1).unwrap();

        let handle2 = runtime.start_handle(8, None).unwrap();
        assert_eq!(handle2.transaction_id(), 2);
        assert_eq!(runtime.prev_running_transaction().unwrap().tid(), 1);
        assert_eq!(runtime.running_transaction().unwrap().tid(), 2);

        record_metadata(&mut runtime, &handle2, 8, &[2]);
        runtime.stop_handle(handle2).unwrap();
        assert!(runtime.batch_commit_ready(1));
    }

    #[test]
    fn rotation_closes_old_transaction_and_new_handles_use_next_tid() {
        let mut runtime = JournalRuntime::new(8, 1);

        let handle1 = runtime.start_handle(4, None).unwrap();
        let handle2 = runtime.start_handle(4, None).unwrap();
        record_metadata(&mut runtime, &handle1, 0, &[1]);
        runtime.stop_handle(handle1).unwrap();

        assert!(runtime.should_rotate_running_transaction(1));
        assert_eq!(runtime.rotate_running_transaction(), Some(1));
        assert_eq!(runtime.prev_running_transaction().unwrap().tid(), 1);
        assert!(!runtime.commit_ready());

        let handle3 = runtime.start_handle(4, None).unwrap();
        assert_eq!(handle3.transaction_id(), 2);

        record_metadata(&mut runtime, &handle2, 8, &[2]);
        runtime.stop_handle(handle2).unwrap();
        assert!(runtime.batch_commit_ready(1));

        let plan1 = runtime.prepare_commit().unwrap();
        assert_eq!(plan1.tid, 1);
        assert_eq!(plan1.metadata_blocks.len(), 2);
        assert!(runtime.finish_commit(plan1.tid, 5, 8));

        record_metadata(&mut runtime, &handle3, 16, &[3]);
        runtime.stop_handle(handle3).unwrap();

        let plan2 = runtime.prepare_commit().unwrap();
        assert_eq!(plan2.tid, 2);
        assert_eq!(plan2.metadata_blocks.len(), 1);
    }

    #[test]
    fn pending_prev_running_blocks_newer_batch_commit() {
        let mut runtime = JournalRuntime::new(8, 1);

        let handle1 = runtime.start_handle(4, None).unwrap();
        let handle2 = runtime.start_handle(4, None).unwrap();
        record_metadata(&mut runtime, &handle1, 0, &[1]);
        runtime.stop_handle(handle1).unwrap();
        assert_eq!(runtime.rotate_running_transaction(), Some(1));

        let handle3 = runtime.start_handle(4, None).unwrap();
        record_metadata(&mut runtime, &handle3, 8, &[2]);
        runtime.stop_handle(handle3).unwrap();

        assert!(!runtime.batch_commit_ready(1));
        assert!(runtime.prepare_commit().is_none());

        runtime.stop_handle(handle2).unwrap();
        assert!(runtime.batch_commit_ready(1));
        let plan = runtime.prepare_commit().unwrap();
        assert_eq!(plan.tid, 1);
    }
}
