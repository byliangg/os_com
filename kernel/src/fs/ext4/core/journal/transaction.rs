// SPDX-License-Identifier: MPL-2.0
//! JBD2 内存事务/handle 状态机 → [`JournalCommitPlan`]（纯内存，不碰盘）。
//!
//! 逐语义复刻 ext4_rs `ext4_impls/jbd2/{transaction,handle,journal}.rs` 的内存半部：
//! - [`JournalBuffer`]/[`JournalTransaction`]：事务持有的全块镜像（`BTreeMap<block_nr,..>`
//!   去重 + 按 block_nr 定序），`record_metadata_write` 的 size-clamp = **BUG-8**。
//! - [`JournalHandle`]：handle 凭据（唯一 handle_id + 所属 tid）。
//! - [`JournalRuntime`]：running/prev_running 双事务 + soft credit 1024 的 admission 轮转，
//!   `start_handle`/`record_metadata_write`/`stop_handle`/`prepare_commit`。
//!
//! Task 3 的 commit emitter 消费 [`JournalCommitPlan`]（全块镜像，按 block_nr 序）。差分对拍
//! ext4_rs `JournalCommitPlan` 的 `tid` + `metadata_blocks`（block 集合 + 序 + 镜像字节）。

use super::super::prelude::*;

/// soft credit 上限：running 事务已 admit 的预留块 + 新 handle 预留块 **超过**此值时，
/// 在 admission 时把 running 轮转到 prev_running（开新事务）。
/// PARITY: ext4_rs journal.rs:12 `JOURNAL_TRANSACTION_CREDIT_SOFT_LIMIT = 1024`。
const JOURNAL_TRANSACTION_CREDIT_SOFT_LIMIT: u32 = 1024;

/// 事务生命周期状态。PARITY: ext4_rs transaction.rs:3-10（`JournalTransactionState`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::fs::ext4) enum JournalTransactionState {
    Running,
    Locked,
    Commit,
}

/// 事务持有的一个 journaled 元数据块的**全块镜像**。
/// PARITY: ext4_rs transaction.rs:18-23（`JournalBuffer`）；core 不需要 `dirty_ranges`
/// （全块镜像由上层给定，差分只对拍 `block_nr`+`block_data`），故略去——commit plan 字段一致。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::fs::ext4) struct JournalBuffer {
    pub block_nr: Ext4Fsblk,
    pub block_data: Vec<u8>,
}

/// 内存事务：一组 handle 共享、收集元数据全块镜像，最终产 commit plan。
/// PARITY: ext4_rs transaction.rs:31-42（`JournalTransaction`）；core 只保留 commit plan
/// 所需子集（tid/state/buffers/handle_count/admitted_reserved_blocks）。
#[derive(Debug, Clone)]
pub(in crate::fs::ext4) struct JournalTransaction {
    tid: u32,
    state: JournalTransactionState,
    /// PARITY: ext4_rs transaction.rs:35 —— **BTreeMap keyed by block_nr**：去重重复写 +
    /// 迭代序固定按 block_nr 升序（commit plan 的 tag 序据此固定，差分必须同序）。
    buffers: BTreeMap<Ext4Fsblk, JournalBuffer>,
    handle_count: u32,
    admitted_reserved_blocks: u32,
}

impl JournalTransaction {
    /// PARITY: ext4_rs transaction.rs:45-57（`new`）。初始 Running、空 buffers。
    pub(in crate::fs::ext4) fn new(tid: u32) -> Self {
        Self {
            tid,
            state: JournalTransactionState::Running,
            buffers: BTreeMap::new(),
            handle_count: 0,
            admitted_reserved_blocks: 0,
        }
    }

    pub(in crate::fs::ext4) fn tid(&self) -> u32 {
        self.tid
    }

    pub(in crate::fs::ext4) fn state(&self) -> JournalTransactionState {
        self.state
    }

    pub(in crate::fs::ext4) fn set_state(&mut self, state: JournalTransactionState) {
        self.state = state;
    }

    pub(in crate::fs::ext4) fn handle_count(&self) -> u32 {
        self.handle_count
    }

    /// PARITY: ext4_rs transaction.rs:79-81（`admitted_reserved_blocks`）——
    /// admit 时累加、**不随 unregister 递减**（用于 soft-credit 轮转判定）。
    pub(in crate::fs::ext4) fn admitted_reserved_blocks(&self) -> u32 {
        self.admitted_reserved_blocks
    }

    /// PARITY: ext4_rs transaction.rs:95-97 —— commit plan 的 metadata 来源。
    pub(in crate::fs::ext4) fn buffers(&self) -> &BTreeMap<Ext4Fsblk, JournalBuffer> {
        &self.buffers
    }

    /// 查该事务持有的某 home 块的全块镜像（overlay read 用）。
    /// PARITY: ext4_rs transaction.rs `JournalTransaction::buffer`（journal.rs:204-229 经 `buffer`
    /// 在 running/prev/committing/checkpoint 各事务里查最新镜像）。core 这里只给单事务查询，
    /// 跨事务的 newest-wins 顺序由调用方（集成层 overlay）按 ext4_rs 的 latest_metadata_buffer 序拼。
    pub(in crate::fs::ext4) fn buffer(&self, block_nr: Ext4Fsblk) -> Option<&JournalBuffer> {
        self.buffers.get(&block_nr)
    }

    /// PARITY: ext4_rs transaction.rs:99-101 —— `buffers.len()`。
    pub(in crate::fs::ext4) fn modified_block_count(&self) -> usize {
        self.buffers.len()
    }

    /// PARITY: ext4_rs transaction.rs:115-124（`register_handle`）——
    /// handle_count + admitted_reserved_blocks 双饱和累加。core 不跟 trigger_op/reserved_blocks
    /// 镜像（commit plan 不需要），只保 admission 轮转所需。
    pub(in crate::fs::ext4) fn register_handle(&mut self, reserved_blocks: u32) {
        self.handle_count = self.handle_count.saturating_add(1);
        self.admitted_reserved_blocks = self
            .admitted_reserved_blocks
            .saturating_add(reserved_blocks);
    }

    /// PARITY: ext4_rs transaction.rs:126-129（`unregister_handle`）——handle_count 饱和递减。
    pub(in crate::fs::ext4) fn unregister_handle(&mut self) {
        self.handle_count = self.handle_count.saturating_sub(1);
    }

    /// 记录一个 journaled 元数据块的全块镜像（去重 + size-clamp）。
    ///
    /// `full_image` 是该块的全块镜像（上层给定）。重复写同一 `block_nr` 覆盖为最新镜像。
    ///
    /// PARITY: BUG-8 —— ext4_rs transaction.rs:152-178 在首次 insert 时对 `load_block()` 的结果
    /// 做 size-clamp：`len < block_size` 则 `resize(block_size, 0)`（零填到块大小）、
    /// `len > block_size` 则 `truncate(block_size)`（截断）。core 直收全块镜像，故对 `full_image`
    /// 直接 clamp 即得同一字节序列（差分喂超/欠 block_size 镜像对拍）。fix deferred (bug.md B-08)。
    pub(in crate::fs::ext4) fn record_metadata_write(
        &mut self,
        block_nr: Ext4Fsblk,
        full_image: &[u8],
        block_size: usize,
    ) {
        // PARITY: BUG-8 size-clamp（transaction.rs:154-158）。
        let mut block_data = full_image.to_vec();
        if block_data.len() < block_size {
            block_data.resize(block_size, 0);
        } else if block_data.len() > block_size {
            block_data.truncate(block_size);
        }

        // 去重：重复写同一 block_nr → 覆盖为最新镜像。BTreeMap 保证 block_nr 升序迭代。
        self.buffers.insert(
            block_nr,
            JournalBuffer {
                block_nr,
                block_data,
            },
        );
    }
}

/// 一个 commit plan 携带的元数据块（全块镜像）。PARITY: ext4_rs journal.rs:14-18
/// （`JournalCommitBlock { block_nr, block_data }`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::fs::ext4) struct JournalCommitBlock {
    pub block_nr: Ext4Fsblk,
    pub block_data: Vec<u8>,
}

/// 一个事务的 commit 计划：tid + 元数据全块镜像（按 block_nr 升序）。**纯内存，不碰盘。**
/// Task 3 的 on-disk emitter 消费它。差分对拍 ext4_rs `JournalCommitPlan` 的 `tid` +
/// `metadata_blocks`（block 集合 + 序 + 镜像字节）。
///
/// PARITY: ext4_rs journal.rs:20-27（`JournalCommitPlan`）；core 只保留 Task 3 所需子集
/// （`tid` + `metadata_blocks`）；`reserved_blocks`/`data_sync_required`/`trigger_op`
/// 是集成层 P6 才用的旁路字段，core/journal commit 写序不依赖，故略去。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::fs::ext4) struct JournalCommitPlan {
    pub tid: u32,
    pub metadata_blocks: Vec<JournalCommitBlock>,
}

/// handle 凭据：唯一 `handle_id` + 所属事务 `transaction_id` + 该 handle 的预留块数。
/// PARITY: ext4_rs handle.rs:3-10（`JournalHandle`）；core 只保 commit plan / 轮转所需子集。
#[derive(Debug, Clone, Copy)]
pub(in crate::fs::ext4) struct JournalHandle {
    handle_id: u64,
    transaction_id: u32,
    /// 该 handle 的预留块数。Task 2 不读（admission 轮转用事务级 admitted_reserved_blocks），
    /// 但 ext4_rs `stop_handle` 据此从事务 reserved_blocks 递减——Task 3 stop/commit 会用。
    #[allow(dead_code)] // Task 3 (stop/commit accounting) consumes per-handle reserved_blocks
    reserved_blocks: u32,
}

impl JournalHandle {
    pub(in crate::fs::ext4) fn handle_id(&self) -> u64 {
        self.handle_id
    }

    pub(in crate::fs::ext4) fn transaction_id(&self) -> u32 {
        self.transaction_id
    }

    #[allow(dead_code)] // 接口对齐 ext4_rs；Task 3 stop/commit accounting 用
    pub(in crate::fs::ext4) fn reserved_blocks(&self) -> u32 {
        self.reserved_blocks
    }
}

/// JBD2 内存事务运行时：管理 running/prev_running 双事务 + active handle 队列，产 commit plan。
///
/// PARITY: ext4_rs journal.rs:55-74（`JournalRuntime`）；core/journal 只做 Task 2 内存半部
/// （start/record/stop/prepare_commit + soft-credit admission 轮转），不含 overlay/checkpoint/
/// commit 编排（那些是 Task 3-5 / P6 集成层）。
#[derive(Debug)]
pub(in crate::fs::ext4) struct JournalRuntime {
    enabled: bool,
    block_size: usize,
    next_tid: u32,
    next_handle_id: u64,
    running: Option<JournalTransaction>,
    prev_running: Option<JournalTransaction>,
    /// 正在 commit 落盘的事务槽。PARITY: ext4_rs journal.rs:63 `committing: Option<..>`。
    /// `prepare_commit` 取出事务后停在此槽（不丢弃），`prepare_commit` 入口对它做幂等门控
    /// （`committing.is_some()` → 返回 None），`finish_commit` 在 Task 3 commit 落盘后清空。
    committing: Option<JournalTransaction>,
    active_handles: VecDeque<JournalHandle>,
}

impl JournalRuntime {
    /// PARITY: ext4_rs journal.rs:77-93（`new`）——`next_tid = first_tid.max(1)`、handle_id 从 1。
    pub(in crate::fs::ext4) fn new(block_size: usize, first_tid: u32) -> Self {
        Self {
            enabled: true,
            block_size,
            next_tid: first_tid.max(1),
            next_handle_id: 1,
            running: None,
            prev_running: None,
            committing: None,
            active_handles: VecDeque::new(),
        }
    }

    #[allow(dead_code)] // 接口对齐 ext4_rs；core/journal 差分用 new(..) 起步
    pub(in crate::fs::ext4) fn block_size(&self) -> usize {
        self.block_size
    }

    pub(in crate::fs::ext4) fn running_transaction(&self) -> Option<&JournalTransaction> {
        self.running.as_ref()
    }

    pub(in crate::fs::ext4) fn prev_running_transaction(&self) -> Option<&JournalTransaction> {
        self.prev_running.as_ref()
    }

    /// soft-credit admission 轮转判定。PARITY: ext4_rs journal.rs:315-325
    /// （`should_rotate_for_new_handle`）——`enabled && prev_running.is_none() && running`
    /// 已有 buffer(`modified_block_count!=0`) 且 `admitted+reserved > 1024`。
    fn should_rotate_for_new_handle(&self, reserved_blocks: u32) -> bool {
        self.enabled
            && self.prev_running.is_none()
            && self.running.as_ref().is_some_and(|transaction| {
                transaction.modified_block_count() != 0
                    && transaction
                        .admitted_reserved_blocks()
                        .saturating_add(reserved_blocks)
                        > JOURNAL_TRANSACTION_CREDIT_SOFT_LIMIT
            })
    }

    /// admission 时把 running 锁定并移到 prev_running（开新事务前）。
    /// PARITY: ext4_rs journal.rs:327-339（`rotate_running_transaction_for_admission`）。
    fn rotate_running_transaction_for_admission(&mut self, reserved_blocks: u32) -> Option<u32> {
        if !self.should_rotate_for_new_handle(reserved_blocks) {
            return None;
        }
        let mut transaction = self.running.take()?;
        transaction.set_state(JournalTransactionState::Locked);
        let tid = transaction.tid();
        self.prev_running = Some(transaction);
        Some(tid)
    }

    /// force-commit / batch 轮转：把 running（仍有活动 handle 且已有 buffer）锁定移到 prev_running，
    /// 使其后续可独立 commit、且不再接受新 handle。返回被轮转事务 tid。
    /// PARITY: ext4_rs `rotate_running_transaction`（journal.rs:341-353，门控 = `should_rotate_
    /// running_transaction(0)`：`prev_running.is_none && running.handle_count!=0 && modified!=0`）。
    /// `threshold_blocks` 由集成层在 batch 路径用 `should_rotate_running_transaction` 判定后再调；
    /// 此入口只做 threshold-0 force 轮转（fsync force-commit + batch 已判定后）。
    pub(in crate::fs::ext4) fn rotate_running_for_force(&mut self) -> Option<u32> {
        let should = self.prev_running.is_none()
            && self.running.as_ref().is_some_and(|t| {
                t.handle_count() != 0 && t.modified_block_count() != 0
            });
        if !should {
            return None;
        }
        let mut transaction = self.running.take()?;
        transaction.set_state(JournalTransactionState::Locked);
        let tid = transaction.tid();
        self.prev_running = Some(transaction);
        Some(tid)
    }

    /// 把 prepared 但未落盘的事务（committing 槽）回滚到 running/prev_running 重试。
    /// PARITY: ext4_rs `abort_commit`（journal.rs:621-637）——state→Running，按 running 是否空回 prev/running。
    pub(in crate::fs::ext4) fn abort_commit(&mut self, tid: u32) -> bool {
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

    /// 开 handle，返回唯一 `handle_id`（>=1）。
    /// PARITY: ext4_rs journal.rs:394-426（`start_handle`）——先 admission 轮转 → 取 handle_id →
    /// get_or_insert running 事务（Locked 复位 Running）→ register_handle → 入 active 队列。
    pub(in crate::fs::ext4) fn start_handle(&mut self, reserved_blocks: u32) -> Option<u64> {
        if !self.enabled {
            return None;
        }

        self.rotate_running_transaction_for_admission(reserved_blocks);
        let handle_id = self.next_handle_id;
        // PARITY: ext4_rs journal.rs:405 —— saturating_add(1).max(1)。
        self.next_handle_id = self.next_handle_id.saturating_add(1).max(1);

        let transaction_id = {
            let next_tid = &mut self.next_tid;
            let transaction = self.running.get_or_insert_with(|| {
                let tid = *next_tid;
                *next_tid = next_tid.saturating_add(1);
                JournalTransaction::new(tid)
            });
            // PARITY: ext4_rs journal.rs:414-416 —— 若上一个 handle 把状态置 Locked，复位 Running。
            if transaction.state() == JournalTransactionState::Locked {
                transaction.set_state(JournalTransactionState::Running);
            }
            transaction.register_handle(reserved_blocks);
            transaction.tid()
        };

        let handle = JournalHandle {
            handle_id,
            transaction_id,
            reserved_blocks,
        };
        self.active_handles.push_back(handle);
        Some(handle_id)
    }

    /// 找含 `tid` 的事务（running / prev_running）。
    /// PARITY: ext4_rs journal.rs:428-453（`transaction_mut` 子集——core 无 committing/checkpoint）。
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
        // PARITY: ext4_rs journal.rs:444-448 —— committing 槽也在查找范围内。
        if self
            .committing
            .as_ref()
            .is_some_and(|transaction| transaction.tid() == tid)
        {
            return self.committing.as_mut();
        }
        None
    }

    fn active_handle_by_id(&self, handle_id: u64) -> Option<&JournalHandle> {
        self.active_handles
            .iter()
            .find(|handle| handle.handle_id() == handle_id)
    }

    /// 记录一个元数据块的全块镜像到该 handle 所属事务（去重 + size-clamp = BUG-8）。
    ///
    /// PARITY: ext4_rs journal.rs:488-541（`record_metadata_write_for_handle`）的全块特化——
    /// ext4_rs 按 byte offset 分块记录，差分驱动总以 `offset = block_nr*block_size` + 全块镜像
    /// 调用，故落进事务的 `BTreeMap<block_nr, JournalBuffer>` 结果一致；core 直收 `block` + 全块
    /// `full_image`，转交事务 `record_metadata_write`（内含 BUG-8 size-clamp）。
    pub(in crate::fs::ext4) fn record_metadata_write(
        &mut self,
        handle_id: u64,
        block: Ext4Fsblk,
        full_image: &[u8],
    ) {
        if !self.enabled || self.block_size == 0 {
            return;
        }
        let Some(transaction_id) = self
            .active_handle_by_id(handle_id)
            .map(|handle| handle.transaction_id())
        else {
            return;
        };
        let block_size = self.block_size;
        if let Some(transaction) = self.transaction_mut(transaction_id) {
            transaction.record_metadata_write(block, full_image, block_size);
        }
    }

    /// 关 handle，返回其所属事务 `tid`。
    /// PARITY: ext4_rs journal.rs:543-559（`stop_handle`）——出 active 队列 → unregister_handle →
    /// `handle_count==0` 时置 Locked。core 返回 tid（差分对拍 plan.tid）。
    pub(in crate::fs::ext4) fn stop_handle(&mut self, handle_id: u64) -> Option<u32> {
        if !self.enabled {
            return None;
        }
        let pos = self
            .active_handles
            .iter()
            .position(|handle| handle.handle_id() == handle_id)?;
        let handle = self.active_handles.remove(pos)?;
        let tid = handle.transaction_id();
        if let Some(transaction) = self.transaction_mut(tid) {
            transaction.unregister_handle();
            if transaction.handle_count() == 0 {
                transaction.set_state(JournalTransactionState::Locked);
            }
        }
        Some(tid)
    }

    /// 当前正在 commit 落盘的事务（committing 槽）。PARITY: ext4_rs journal.rs:129-130
    /// （`committing_transaction`）。差分用它确认 `prepare_commit` 后事务停在 committing 槽。
    pub(in crate::fs::ext4) fn committing_transaction(&self) -> Option<&JournalTransaction> {
        self.committing.as_ref()
    }

    /// 查该 home `block_nr` 的**内存最新全块镜像**，按 newest-wins 序在 running → prev_running →
    /// committing 三槽里找（**不含 checkpoint_list**——core 薄 runtime 不持 checkpoint_list，那由
    /// P6 集成层维护并叠加在本结果之上）。给集成层 overlay read（read-your-writes）用。
    ///
    /// PARITY: ext4_rs `JournalRuntime::latest_metadata_buffer`（journal.rs:204-230）的「内存事务」
    /// 子集——ext4_rs 顺序是 running → prev_running → committing → checkpoint_list(rev)；本访问器复刻
    /// 前三槽，checkpoint_list 部分由集成层在其后查（与 ext4_rs 同序）。
    pub(in crate::fs::ext4) fn running_metadata_image(&self, block_nr: Ext4Fsblk) -> Option<&[u8]> {
        if let Some(transaction) = self.running.as_ref() {
            if let Some(buffer) = transaction.buffer(block_nr) {
                return Some(&buffer.block_data);
            }
        }
        if let Some(transaction) = self.prev_running.as_ref() {
            if let Some(buffer) = transaction.buffer(block_nr) {
                return Some(&buffer.block_data);
            }
        }
        if let Some(transaction) = self.committing.as_ref() {
            if let Some(buffer) = transaction.buffer(block_nr) {
                return Some(&buffer.block_data);
            }
        }
        None
    }

    /// 是否有活动 handle（延迟 home 写门控用）。PARITY: ext4_rs journal.rs:161-163
    /// （`has_active_handle`）+ `should_defer_metadata_write`（172-174：`enabled && 有活动 handle`）。
    pub(in crate::fs::ext4) fn should_defer_metadata_write(&self) -> bool {
        self.enabled && !self.active_handles.is_empty()
    }

    /// 产 commit plan（纯内存）：取 prev_running（无则 running），若仍有未关 handle 则放回返回 None；
    /// 否则置 Commit、把事务停进 **committing 槽**、收集 buffers（BTreeMap 序 = block_nr 升序）成 plan。
    ///
    /// PARITY: ext4_rs journal.rs:561-599（`prepare_commit`）——**含 committing 槽 + 幂等门控**
    /// （Task-2 review 补：Task 2 曾把取出的事务直接丢弃、无 committing 槽）：入口 `committing.is_some()`
    /// → 返回 None（一次只能有一个 in-commit 事务）；取出事务置 Commit 后 `committing = Some(transaction)`
    /// 停泊（不丢弃），落盘后由 [`finish_commit`](Self::finish_commit) 清槽。`metadata_blocks` 按
    /// `buffers().values()` 序（BTreeMap 升序）收集——与停泊的事务字节一致。
    pub(in crate::fs::ext4) fn prepare_commit(&mut self) -> Option<JournalCommitPlan> {
        // PARITY: ext4_rs journal.rs:562 —— 幂等：已有 in-commit 事务则不再开新 commit。
        if !self.enabled || self.committing.is_some() {
            return None;
        }

        // PARITY: ext4_rs journal.rs:566-570 —— 优先 prev_running，否则 running。
        let mut transaction = if let Some(transaction) = self.prev_running.take() {
            transaction
        } else {
            self.running.take()?
        };

        // PARITY: ext4_rs journal.rs:571-578 —— 仍有活动 handle 则放回，不 commit。
        if transaction.handle_count() != 0 {
            if self.running.is_some() {
                self.prev_running = Some(transaction);
            } else {
                self.running = Some(transaction);
            }
            return None;
        }

        transaction.set_state(JournalTransactionState::Commit);
        // PARITY: ext4_rs journal.rs:581-588 —— buffers().values() 序（BTreeMap = block_nr 升序）。
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
            metadata_blocks,
        };
        // PARITY: ext4_rs journal.rs:596 —— 事务停进 committing 槽（不丢弃）。
        self.committing = Some(transaction);
        Some(plan)
    }

    /// 落盘完成后清 committing 槽（tid 匹配才清）。PARITY: ext4_rs journal.rs:601-619
    /// （`finish_commit` 子集——core/journal 不做 checkpoint_list/last_committed_tid，那是 P6 集成层；
    /// 此处只复刻「committing 槽幂等清除」语义：tid 不匹配则放回返回 false）。Task 3 的 commit 编排
    /// 在 `write_commit_plan` 成功后调它，使 runtime 可接受下一个 commit。
    pub(in crate::fs::ext4) fn finish_commit(&mut self, tid: u32) -> bool {
        let Some(transaction) = self.committing.take() else {
            return false;
        };
        if transaction.tid() != tid {
            // PARITY: ext4_rs journal.rs:605-608 —— tid 不匹配，放回不清。
            self.committing = Some(transaction);
            return false;
        }
        true
    }
}
