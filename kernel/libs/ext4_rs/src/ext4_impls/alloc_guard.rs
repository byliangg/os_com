use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

use crate::ext4_defs::{Ext4Fsblk, OperationAllocGuard, OperationAllocGuardDebugStats};
use crate::prelude::*;

const DEFAULT_OPERATION_ID: u64 = 0;

struct OperationAllocGuardStats {
    clear_calls: AtomicU64,
    reserve_calls: AtomicU64,
    reserved_blocks: AtomicU64,
    contains_checks: AtomicU64,
    max_operation_blocks: AtomicU64,
}

impl OperationAllocGuardStats {
    const fn new() -> Self {
        Self {
            clear_calls: AtomicU64::new(0),
            reserve_calls: AtomicU64::new(0),
            reserved_blocks: AtomicU64::new(0),
            contains_checks: AtomicU64::new(0),
            max_operation_blocks: AtomicU64::new(0),
        }
    }

    fn update_max_operation_blocks(&self, blocks: u64) {
        let mut current = self.max_operation_blocks.load(Ordering::Relaxed);
        while blocks > current {
            match self.max_operation_blocks.compare_exchange_weak(
                current,
                blocks,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    fn snapshot(&self) -> OperationAllocGuardDebugStats {
        OperationAllocGuardDebugStats {
            clear_calls: self.clear_calls.load(Ordering::Relaxed),
            reserve_calls: self.reserve_calls.load(Ordering::Relaxed),
            reserved_blocks: self.reserved_blocks.load(Ordering::Relaxed),
            contains_checks: self.contains_checks.load(Ordering::Relaxed),
            max_operation_blocks: self.max_operation_blocks.load(Ordering::Relaxed),
        }
    }
}

pub struct LocalOperationAllocGuard {
    current_operation: Mutex<u64>,
    allocated_blocks: Mutex<BTreeMap<u64, BTreeSet<Ext4Fsblk>>>,
    stats: OperationAllocGuardStats,
}

impl LocalOperationAllocGuard {
    pub fn new() -> Self {
        Self {
            current_operation: Mutex::new(DEFAULT_OPERATION_ID),
            allocated_blocks: Mutex::new(BTreeMap::new()),
            stats: OperationAllocGuardStats::new(),
        }
    }

    pub fn begin_operation(&self, operation_id: u64) {
        *self.current_operation.lock() = operation_id;
        self.allocated_blocks.lock().entry(operation_id).or_default();
    }

    pub fn finish_operation(&self, operation_id: u64) {
        self.allocated_blocks.lock().remove(&operation_id);
        let mut current = self.current_operation.lock();
        if *current == operation_id {
            *current = DEFAULT_OPERATION_ID;
        }
    }

    pub fn debug_stats(&self) -> OperationAllocGuardDebugStats {
        self.stats.snapshot()
    }

    fn current_operation_id(&self) -> u64 {
        *self.current_operation.lock()
    }
}

impl Default for LocalOperationAllocGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl OperationAllocGuard for LocalOperationAllocGuard {
    fn clear_current_operation(&self) {
        self.stats.clear_calls.fetch_add(1, Ordering::Relaxed);
        let operation_id = self.current_operation_id();
        self.allocated_blocks.lock().remove(&operation_id);
    }

    fn reserve_current_block(&self, block: Ext4Fsblk) {
        self.stats.reserve_calls.fetch_add(1, Ordering::Relaxed);
        self.stats.reserved_blocks.fetch_add(1, Ordering::Relaxed);
        let operation_id = self.current_operation_id();
        let mut guard = self.allocated_blocks.lock();
        let blocks = guard.entry(operation_id).or_default();
        blocks.insert(block);
        self.stats.update_max_operation_blocks(blocks.len() as u64);
    }

    fn reserve_current_blocks(&self, blocks: &[Ext4Fsblk]) {
        if blocks.is_empty() {
            return;
        }
        self.stats.reserve_calls.fetch_add(1, Ordering::Relaxed);
        self.stats
            .reserved_blocks
            .fetch_add(blocks.len() as u64, Ordering::Relaxed);
        let operation_id = self.current_operation_id();
        let mut guard = self.allocated_blocks.lock();
        let operation_blocks = guard.entry(operation_id).or_default();
        for block in blocks {
            operation_blocks.insert(*block);
        }
        self.stats
            .update_max_operation_blocks(operation_blocks.len() as u64);
    }

    fn contains_current_block(&self, block: Ext4Fsblk) -> bool {
        self.stats
            .contains_checks
            .fetch_add(1, Ordering::Relaxed);
        let operation_id = self.current_operation_id();
        self.allocated_blocks
            .lock()
            .get(&operation_id)
            .is_some_and(|blocks| blocks.contains(&block))
    }

    fn debug_stats(&self) -> OperationAllocGuardDebugStats {
        self.debug_stats()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_guard_keeps_allocated_blocks_per_operation() {
        let guard = LocalOperationAllocGuard::new();

        guard.begin_operation(1);
        guard.reserve_current_block(10);
        assert!(guard.contains_current_block(10));
        assert!(!guard.contains_current_block(20));

        guard.begin_operation(2);
        assert!(!guard.contains_current_block(10));
        guard.reserve_current_block(20);
        assert!(guard.contains_current_block(20));

        guard.begin_operation(1);
        assert!(guard.contains_current_block(10));
        assert!(!guard.contains_current_block(20));

        guard.finish_operation(1);
        assert!(!guard.contains_current_block(10));

        guard.begin_operation(2);
        assert!(guard.contains_current_block(20));
        guard.clear_current_operation();
        assert!(!guard.contains_current_block(20));
    }
}
