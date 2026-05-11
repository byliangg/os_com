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
    allocated_blocks: Mutex<BTreeMap<u64, BTreeSet<Ext4Fsblk>>>,
    stats: OperationAllocGuardStats,
}

pub struct OperationScopedAllocGuard {
    inner: Arc<LocalOperationAllocGuard>,
    operation_id: u64,
}

impl OperationScopedAllocGuard {
    pub fn new(inner: Arc<LocalOperationAllocGuard>, operation_id: u64) -> Self {
        Self {
            inner,
            operation_id,
        }
    }
}

impl OperationAllocGuard for OperationScopedAllocGuard {
    fn clear_current_operation(&self) {
        self.inner.clear_operation(self.operation_id);
    }

    fn reserve_current_block(&self, block: Ext4Fsblk) {
        self.inner
            .reserve_block_for_operation(self.operation_id, block);
    }

    fn reserve_current_blocks(&self, blocks: &[Ext4Fsblk]) {
        self.inner
            .reserve_blocks_for_operation(self.operation_id, blocks);
    }

    fn contains_current_block(&self, block: Ext4Fsblk) -> bool {
        self.inner
            .contains_block_for_operation(self.operation_id, block)
    }

    fn debug_stats(&self) -> OperationAllocGuardDebugStats {
        self.inner.debug_stats()
    }
}

impl LocalOperationAllocGuard {
    pub fn new() -> Self {
        Self {
            allocated_blocks: Mutex::new(BTreeMap::new()),
            stats: OperationAllocGuardStats::new(),
        }
    }

    pub fn begin_operation(&self, operation_id: u64) {
        self.allocated_blocks.lock().entry(operation_id).or_default();
    }

    pub fn finish_operation(&self, operation_id: u64) {
        self.allocated_blocks.lock().remove(&operation_id);
    }

    pub fn debug_stats(&self) -> OperationAllocGuardDebugStats {
        self.stats.snapshot()
    }

    pub fn clear_operation(&self, operation_id: u64) {
        self.stats.clear_calls.fetch_add(1, Ordering::Relaxed);
        self.allocated_blocks.lock().remove(&operation_id);
    }

    pub fn reserve_block_for_operation(&self, operation_id: u64, block: Ext4Fsblk) {
        self.stats.reserve_calls.fetch_add(1, Ordering::Relaxed);
        self.stats.reserved_blocks.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.allocated_blocks.lock();
        let blocks = guard.entry(operation_id).or_default();
        blocks.insert(block);
        self.stats.update_max_operation_blocks(blocks.len() as u64);
    }

    pub fn reserve_blocks_for_operation(&self, operation_id: u64, blocks: &[Ext4Fsblk]) {
        if blocks.is_empty() {
            return;
        }
        self.stats.reserve_calls.fetch_add(1, Ordering::Relaxed);
        self.stats
            .reserved_blocks
            .fetch_add(blocks.len() as u64, Ordering::Relaxed);
        let mut guard = self.allocated_blocks.lock();
        let operation_blocks = guard.entry(operation_id).or_default();
        for block in blocks {
            operation_blocks.insert(*block);
        }
        self.stats
            .update_max_operation_blocks(operation_blocks.len() as u64);
    }

    pub fn contains_block_for_operation(&self, operation_id: u64, block: Ext4Fsblk) -> bool {
        self.stats
            .contains_checks
            .fetch_add(1, Ordering::Relaxed);
        self.allocated_blocks
            .lock()
            .get(&operation_id)
            .is_some_and(|blocks| blocks.contains(&block))
    }
}

impl Default for LocalOperationAllocGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl OperationAllocGuard for LocalOperationAllocGuard {
    fn clear_current_operation(&self) {
        self.clear_operation(DEFAULT_OPERATION_ID);
    }

    fn reserve_current_block(&self, block: Ext4Fsblk) {
        self.reserve_block_for_operation(DEFAULT_OPERATION_ID, block);
    }

    fn reserve_current_blocks(&self, blocks: &[Ext4Fsblk]) {
        self.reserve_blocks_for_operation(DEFAULT_OPERATION_ID, blocks);
    }

    fn contains_current_block(&self, block: Ext4Fsblk) -> bool {
        self.contains_block_for_operation(DEFAULT_OPERATION_ID, block)
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
        guard.reserve_block_for_operation(1, 10);
        assert!(guard.contains_block_for_operation(1, 10));
        assert!(!guard.contains_block_for_operation(1, 20));

        guard.begin_operation(2);
        assert!(!guard.contains_block_for_operation(2, 10));
        guard.reserve_block_for_operation(2, 20);
        assert!(guard.contains_block_for_operation(2, 20));

        assert!(guard.contains_block_for_operation(1, 10));
        assert!(!guard.contains_block_for_operation(1, 20));

        guard.finish_operation(1);
        assert!(!guard.contains_block_for_operation(1, 10));

        assert!(guard.contains_block_for_operation(2, 20));
        guard.clear_operation(2);
        assert!(!guard.contains_block_for_operation(2, 20));
    }

    #[test]
    fn local_guard_interleaved_operations_do_not_share_current_slot() {
        let guard = LocalOperationAllocGuard::new();

        guard.begin_operation(100);
        guard.begin_operation(101);
        guard.reserve_block_for_operation(100, 7);
        guard.reserve_block_for_operation(101, 8);

        assert!(guard.contains_block_for_operation(100, 7));
        assert!(!guard.contains_block_for_operation(100, 8));
        assert!(guard.contains_block_for_operation(101, 8));
        assert!(!guard.contains_block_for_operation(101, 7));
    }

    #[test]
    fn scoped_guard_nested_clear_preserves_outer_operation() {
        let guard = Arc::new(LocalOperationAllocGuard::new());

        guard.begin_operation(200);
        guard.begin_operation(201);
        let outer = OperationScopedAllocGuard::new(guard.clone(), 200);
        let nested = OperationScopedAllocGuard::new(guard.clone(), 201);

        outer.reserve_current_block(30);
        nested.reserve_current_block(40);
        nested.clear_current_operation();

        assert!(outer.contains_current_block(30));
        assert!(!outer.contains_current_block(40));
        assert!(!nested.contains_current_block(40));

        outer.reserve_current_block(31);
        assert!(outer.contains_current_block(31));
        outer.clear_current_operation();
        assert!(!outer.contains_current_block(30));
        assert!(!outer.contains_current_block(31));
    }
}
