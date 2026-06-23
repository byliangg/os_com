// SPDX-License-Identifier: MPL-2.0
//! 每操作内存级块预留（alloc_guard）——逐位复刻 `ext4_rs/src/ext4_impls/alloc_guard.rs`
//! 的 `LocalOperationAllocGuard` / `OperationScopedAllocGuard` 行为与 debug 计数，但全程
//! 零 unsafe，锁用 ostd 的 [`Mutex`]、计数用 `core::sync::atomic`。
//!
//! ## 作用与本 Phase 的「惰性」
//! JBD2 事务里位图写是**延迟的**（overlay，未提交前不落盘），balloc 重读盘上位图看不到
//! 「本操作刚分配但还没提交」的块，可能重复分配。guard 在内存里记下本操作已发出的块，
//! 让 balloc 在扫描候选时 `contains_current_block` 跳过它们、命中后 `reserve_current_block`
//! 登记。
//!
//! **但 Phase 2 的 [`DirectMetadataWriter`] 是立即写**（位图当场更新、无 overlay），位图当场
//! 就是权威——所以 guard 的「跳过位图未显示的块」**端到端 skip 效果在 Phase 2 不会真正触发**，
//! 要等 Phase 5（JBD2 overlay）才显现。本 Task 验的是 guard 机制本身的数据结构 parity 与
//! balloc 在与 ext4_rs **完全相同位置**调用 contains/reserve 的接线 parity。
//!
//! [对照来源] kernel/libs/ext4_rs/src/ext4_impls/alloc_guard.rs（:54-186）
//! kernel/libs/ext4_rs/src/ext4_defs/block.rs（:31-45：trait + DebugStats）

use core::sync::atomic::{AtomicU64, Ordering};

use super::prelude::*;

/// 默认操作 id——当通过 `OperationAllocGuard` 的 `*_current_*` 方法访问
/// [`LocalOperationAllocGuard`] 时使用的操作槽。
/// [对照] ext4_rs `DEFAULT_OPERATION_ID`（alloc_guard.rs:7）。
const DEFAULT_OPERATION_ID: u64 = 0;

/// alloc_guard 的调试计数快照（5 项）。逐字段对应 ext4_rs `OperationAllocGuardDebugStats`
/// （block.rs:31-37）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct OperationAllocGuardDebugStats {
    /// `clear_current_operation` / `clear_operation` 调用次数。
    pub(super) clear_calls: u64,
    /// `reserve_*` 调用次数（按「调用」计，批量算一次）。
    pub(super) reserve_calls: u64,
    /// 累计预留块数（批量按块数累加）。
    pub(super) reserved_blocks: u64,
    /// `contains_*` 查询次数。
    pub(super) contains_checks: u64,
    /// 任一操作槽出现过的最大块集合大小。
    pub(super) max_operation_blocks: u64,
}

/// 每操作块预留 guard 抽象。balloc 在扫描候选时用 [`contains_current_block`] 跳过本操作
/// 已预留的块，命中后用 [`reserve_current_block`] / [`reserve_current_blocks`] 登记。
/// [对照] ext4_rs `OperationAllocGuard` trait（block.rs:39-45）。
///
/// [`contains_current_block`]: OperationAllocGuard::contains_current_block
/// [`reserve_current_block`]: OperationAllocGuard::reserve_current_block
/// [`reserve_current_blocks`]: OperationAllocGuard::reserve_current_blocks
pub(super) trait OperationAllocGuard: Send + Sync {
    /// 清空当前操作已预留的块集合（计 `clear_calls`）。
    fn clear_current_operation(&self);
    /// 把单块登记进当前操作（计 `reserve_calls` + `reserved_blocks`）。
    fn reserve_current_block(&self, block: Ext4Fsblk);
    /// 把一批块登记进当前操作（空批量直接返回、不计数）。
    fn reserve_current_blocks(&self, blocks: &[Ext4Fsblk]);
    /// 当前操作是否已预留 `block`（计 `contains_checks`）。
    fn contains_current_block(&self, block: Ext4Fsblk) -> bool;
    /// 取调试计数快照。
    ///
    /// 生产路径（balloc）只调 contains/reserve，本方法是 parity trait surface 的一部分
    /// （对照 ext4_rs `OperationAllocGuard::debug_stats`），由差分 ktest 经具体类型的同名
    /// 内联方法消费、并供 Phase 5 诊断；故在生产编译下标 `dead_code`。
    #[allow(dead_code)]
    fn debug_stats(&self) -> OperationAllocGuardDebugStats;
}

/// 原子调试计数（内部）。逐位复刻 ext4_rs `OperationAllocGuardStats`（alloc_guard.rs:9-52）。
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

    /// CAS 单调抬高 `max_operation_blocks`。逐位复刻 ext4_rs `update_max_operation_blocks`。
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

/// 进程内的「每操作块预留表」：`op_id -> 该操作已预留的块集合`，附原子调试计数。
/// 逐位复刻 ext4_rs `LocalOperationAllocGuard`（alloc_guard.rs:54-164）。
///
/// 通过 [`OperationAllocGuard`] 的 `*_current_*` 方法访问时，落在 [`DEFAULT_OPERATION_ID`]
/// 槽（与 ext4_rs `Ext4::open` 安装的默认 guard 行为一致）；多操作并发由
/// [`OperationScopedAllocGuard`] 各持 op_id 转发到 `*_for_operation`。
pub(super) struct LocalOperationAllocGuard {
    allocated_blocks: Mutex<BTreeMap<u64, BTreeSet<Ext4Fsblk>>>,
    stats: OperationAllocGuardStats,
}

impl LocalOperationAllocGuard {
    pub(super) fn new() -> Self {
        Self {
            allocated_blocks: Mutex::new(BTreeMap::new()),
            stats: OperationAllocGuardStats::new(),
        }
    }

    /// 开一个操作槽（若已存在则保留）。[对照] ext4_rs `begin_operation`。
    pub(super) fn begin_operation(&self, operation_id: u64) {
        self.allocated_blocks.lock().entry(operation_id).or_default();
    }

    /// 结束并移除一个操作槽（**不计** `clear_calls`）。[对照] ext4_rs `finish_operation`。
    pub(super) fn finish_operation(&self, operation_id: u64) {
        self.allocated_blocks.lock().remove(&operation_id);
    }

    /// 取调试计数快照。
    pub(super) fn debug_stats(&self) -> OperationAllocGuardDebugStats {
        self.stats.snapshot()
    }

    /// 清空并移除一个操作槽（**计** `clear_calls`）。[对照] ext4_rs `clear_operation`。
    pub(super) fn clear_operation(&self, operation_id: u64) {
        self.stats.clear_calls.fetch_add(1, Ordering::Relaxed);
        self.allocated_blocks.lock().remove(&operation_id);
    }

    /// 把单块登记进 `operation_id` 槽。[对照] ext4_rs `reserve_block_for_operation`。
    pub(super) fn reserve_block_for_operation(&self, operation_id: u64, block: Ext4Fsblk) {
        self.stats.reserve_calls.fetch_add(1, Ordering::Relaxed);
        self.stats.reserved_blocks.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.allocated_blocks.lock();
        let blocks = guard.entry(operation_id).or_default();
        blocks.insert(block);
        self.stats.update_max_operation_blocks(blocks.len() as u64);
    }

    /// 把一批块登记进 `operation_id` 槽（空批量直接返回、不计数）。
    /// [对照] ext4_rs `reserve_blocks_for_operation`。
    pub(super) fn reserve_blocks_for_operation(&self, operation_id: u64, blocks: &[Ext4Fsblk]) {
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

    /// `operation_id` 槽是否已预留 `block`（计 `contains_checks`）。
    /// [对照] ext4_rs `contains_block_for_operation`。
    pub(super) fn contains_block_for_operation(&self, operation_id: u64, block: Ext4Fsblk) -> bool {
        self.stats.contains_checks.fetch_add(1, Ordering::Relaxed);
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

/// 把「当前操作」绑定到固定 `operation_id` 的 guard 视图：所有 `*_current_*` 调用转发到
/// 内层 [`LocalOperationAllocGuard`] 的 `*_for_operation(operation_id)`。逐位复刻 ext4_rs
/// `OperationScopedAllocGuard`（alloc_guard.rs:59-96）。
pub(super) struct OperationScopedAllocGuard {
    inner: Arc<LocalOperationAllocGuard>,
    operation_id: u64,
}

impl OperationScopedAllocGuard {
    pub(super) fn new(inner: Arc<LocalOperationAllocGuard>, operation_id: u64) -> Self {
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
