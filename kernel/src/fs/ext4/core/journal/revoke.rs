// SPDX-License-Identifier: MPL-2.0
//! JBD2 revoke：内存 revoke 表 + checkpoint 删 buffer（**复刻 BUG-5：revoke 从不写盘**）+
//! recovery 侧 revoke 块解析（Task 5 用）。**全大端。**
//!
//! 逐语义复刻 ext4_rs `ext4_impls/jbd2/{journal,recovery}.rs` 的 revoke 半部：
//!
//! - **记录路径**（[`RevokeRuntime::revoke_checkpoint_metadata_block`]，对应 ext4_rs
//!   `JournalRuntime::revoke_checkpoint_metadata_block`，journal.rs:137-149）：只遍历内存
//!   `checkpoint_list`、把该块从每个已 commit 待 checkpoint 的事务 buffer 里删掉（让它不再被
//!   写回 home），并把 `(block, 撤销它的 tid)` 记进 [`RevokeTable`]。**commit 路径从不写
//!   `JBD2_REVOKE_BLOCK`（type=5）**——这是 BUG-5（bug.md B-05）。
//!
//! - **解析路径**（[`parse_revoke_block`]，对应 ext4_rs `Jbd2Journal::parse_revoke_entries`，
//!   recovery.rs:207-230）：读一个 `RawRevokeBlockHeader`（16B 大端）+ 其后紧排的块号
//!   （`blocknr_size` = 64BIT→8B / 否则 4B，大端），供 Task 5 的恢复 REVOKE 趟建表。写路径
//!   既然从不产出 revoke 块（BUG-5），此解析在真镜像上恒解出空集——但 Task 5 的三趟算法仍要
//!   逐字复刻这趟扫描，故解析器照样实现。
//!
//! **revoke 表语义 / recovery 用法（grounded against ext4_rs recovery.rs）**：ext4_rs 的恢复
//! （recovery.rs:48-64）把 [start,end) 内所有 revoke 块解析成一个**扁平 `BTreeSet<u64>`**（块号
//! 集合，**不带 tid/sequence**），REPLAY 时 `if revoked.contains(&block_nr) { continue }`——
//! **任何**在该窗口被 revoke 的块都跳过，无 sequence 上界比较。故 parity 下的「revoked-as-of-tid」
//! 规则实际退化为**扁平包含**（[`RevokeTable::is_revoked`]）。core 的 [`RevokeTable`] 额外保留
//! 「撤销它的最高 tid」以备 Task 5/集成层需要（[`RevokeTable::is_revoked_as_of`]），但 recovery
//! 复刻 ext4_rs 时只用扁平包含——见各方法文档。

use super::super::prelude::*;
use super::format::{RawRevokeBlockHeader, JBD2_REVOKE_BLOCK};

/// 内存 revoke 表：`block_nr → 撤销它的（最高）tid`。
///
/// PARITY 说明：ext4_rs 在 recovery（recovery.rs:176-230）只用一个扁平 `BTreeSet<u64>`
/// （块号集合，无 tid），REPLAY 用 `contains` 判跳过；记录路径（journal.rs:137-149）
/// 也不持有 block→tid 映射（它只是从 checkpoint 事务删 buffer）。core 的 [`RevokeTable`]
/// 把「撤销它的 tid」一并记下（用于 Task 5/集成层的可查询 revoke 状态），但**恢复复刻**
/// 仍以扁平包含（[`is_revoked`](Self::is_revoked)）为准——与 ext4_rs 字节/行为一致。
#[derive(Debug, Clone, Default)]
pub(in crate::fs::ext4::core) struct RevokeTable {
    /// block_nr → 撤销它的最高 tid。重复 revoke 同一块时保留最大 tid（最新撤销者胜）。
    entries: BTreeMap<Ext4Fsblk, u32>,
}

impl RevokeTable {
    /// 空表。
    pub(in crate::fs::ext4::core) fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// 记录一次 revoke：`block` 被序号 `tid` 的事务撤销。重复撤销同一块保留**最高** tid。
    ///
    /// PARITY: ext4_rs recovery `scan_revoke_blocks` 把所有 revoke 块号塞进一个集合
    /// （`revoked.insert(block_nr)`，recovery.rs:227）——无 tid 维度。core 这里额外携带 tid
    /// 但对 recovery 行为无影响（recovery 只用扁平包含）；保留最高 tid 以备可查询用途。
    pub(in crate::fs::ext4::core) fn record(&mut self, block: Ext4Fsblk, tid: u32) {
        self.entries
            .entry(block)
            .and_modify(|t| {
                if tid > *t {
                    *t = tid;
                }
            })
            .or_insert(tid);
    }

    /// 扁平包含查询：`block` 是否被 revoke 过（不看 tid）。
    ///
    /// **这是恢复复刻 ext4_rs 的权威判据**：recovery.rs:53 `if revoked.contains(&block_nr)`
    /// 用的就是扁平集合的 `contains`——任何被 revoke 的块在 REPLAY 时一律跳过，**无 sequence
    /// 上界比较**。Task 5 的 REPLAY 趟应调本方法。
    pub(in crate::fs::ext4::core) fn is_revoked(&self, block: Ext4Fsblk) -> bool {
        self.entries.contains_key(&block)
    }

    /// 可查询的「截至 tid 是否已被 revoke」：`block` 是否被某个序号 `>= tid` 的事务撤销。
    ///
    /// **不在 ext4_rs recovery 复刻路径上**——ext4_rs 不做这个 sequence 比较（见模块文档）。
    /// 提供它是因为 brief 要求一个「is block B revoked as of tid T」的可查询语义，且未来集成层
    /// （P6）/ 真实 JBD2 revoke-record 实现修 BUG-5 后会用到「撤销它的事务序号 >= 正在重放它的
    /// 事务序号」这条 Linux jbd2 规则。当前 parity 下，recovery 用 [`is_revoked`](Self::is_revoked)。
    pub(in crate::fs::ext4::core) fn is_revoked_as_of(&self, block: Ext4Fsblk, tid: u32) -> bool {
        self.entries
            .get(&block)
            .is_some_and(|&revoking_tid| revoking_tid >= tid)
    }

    /// 已记录的 revoke 条目数。
    pub(in crate::fs::ext4::core) fn len(&self) -> usize {
        self.entries.len()
    }

    /// 表是否为空。
    pub(in crate::fs::ext4::core) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 一个已 commit 待 checkpoint 的事务在内存里持有的全块镜像（`block_nr → 镜像`）。
///
/// PARITY: ext4_rs `JournalTransaction.buffers`（transaction.rs:35，`BTreeMap<u64, JournalBuffer>`）
/// 的 checkpoint 视图子集——revoke 记录路径只需「能按 block_nr 删 buffer」。core/journal 的
/// Task-2 `JournalRuntime` 是内存事务半部的窄子集（无 `checkpoint_list`），故 revoke 这里用一个
/// 独立的 [`CheckpointTransaction`] 表示「committing 后进入 checkpoint 队列的事务」。
#[derive(Debug, Clone)]
pub(in crate::fs::ext4::core) struct CheckpointTransaction {
    tid: u32,
    /// PARITY: BTreeMap keyed by block_nr（与 ext4_rs 事务 buffers 同结构，按 block_nr 升序）。
    buffers: BTreeMap<Ext4Fsblk, Vec<u8>>,
}

impl CheckpointTransaction {
    /// 从一组 `(block_nr, 全块镜像)` 建一个 tid 的 checkpoint 事务。
    pub(in crate::fs::ext4::core) fn new(
        tid: u32,
        blocks: impl IntoIterator<Item = (Ext4Fsblk, Vec<u8>)>,
    ) -> Self {
        Self {
            tid,
            buffers: blocks.into_iter().collect(),
        }
    }

    pub(in crate::fs::ext4::core) fn tid(&self) -> u32 {
        self.tid
    }

    /// 是否持有 `block` 的 checkpoint buffer。
    pub(in crate::fs::ext4::core) fn has_buffer(&self, block: Ext4Fsblk) -> bool {
        self.buffers.contains_key(&block)
    }

    /// 持有的待 checkpoint 块数（剩余未写回 home 的 buffer）。
    pub(in crate::fs::ext4::core) fn modified_block_count(&self) -> usize {
        self.buffers.len()
    }

    /// PARITY: ext4_rs `JournalTransaction::remove_metadata_block`（transaction.rs:111-113）——
    /// `buffers.remove(&block_nr).is_some()`。删一个 checkpoint buffer，返回是否删到。
    pub(in crate::fs::ext4::core) fn remove_metadata_block(&mut self, block: Ext4Fsblk) -> bool {
        self.buffers.remove(&block).is_some()
    }
}

/// revoke 记录运行时：内存 checkpoint 队列 + revoke 表。
///
/// **复刻 BUG-5（bug.md B-05）：revoke 只在内存里删 checkpoint buffer + 记表，commit 路径
/// 从不写 `JBD2_REVOKE_BLOCK`（type=5）到 journal。** 这与 core 的 commit emitter（commit.rs，
/// 只写 descriptor/payload/commit/SB，绝无 revoke 块）天然一致——本运行时不持有任何写盘接缝。
///
/// PARITY: ext4_rs `JournalRuntime`（journal.rs）的 revoke 子集——core/journal 的 Task-2
/// `JournalRuntime` 是窄子集（无 `checkpoint_list`），故把 revoke 所需的「checkpoint 队列 +
/// revoke 表」独立成本类型，语义逐字对齐 `revoke_checkpoint_metadata_block`。
#[derive(Debug, Default)]
pub(in crate::fs::ext4::core) struct RevokeRuntime {
    enabled: bool,
    /// PARITY: ext4_rs `JournalRuntime.checkpoint_list`（journal.rs:64，`VecDeque<JournalTransaction>`）——
    /// 已 commit 待 checkpoint 的事务，FIFO。revoke 遍历它删 buffer。
    checkpoint_list: VecDeque<CheckpointTransaction>,
    table: RevokeTable,
}

impl RevokeRuntime {
    /// 建一个启用的 revoke 运行时（空 checkpoint 队列 + 空 revoke 表）。
    pub(in crate::fs::ext4::core) fn new() -> Self {
        Self {
            enabled: true,
            checkpoint_list: VecDeque::new(),
            table: RevokeTable::new(),
        }
    }

    /// 建一个禁用的 revoke 运行时（`revoke_checkpoint_metadata_block` 恒返回 0，不动表/队列）。
    /// PARITY: ext4_rs journal.rs:138-140 `if !self.enabled { return 0; }`。
    #[allow(dead_code)] // 接口对齐 ext4_rs（disabled journal）；当前差分用 new()
    pub(in crate::fs::ext4::core) fn disabled() -> Self {
        Self {
            enabled: false,
            checkpoint_list: VecDeque::new(),
            table: RevokeTable::new(),
        }
    }

    /// 把一个已 commit 的事务推入 checkpoint 队列（commit 落盘后、checkpoint 写回 home 前）。
    /// PARITY: ext4_rs `finish_commit`（journal.rs:601-619）`checkpoint_list.push_back(transaction)`
    /// 的 revoke 视图——core/journal 的 commit 编排（差分驱动）在 commit 后调它登记 checkpoint 事务。
    pub(in crate::fs::ext4::core) fn push_checkpoint(&mut self, transaction: CheckpointTransaction) {
        self.checkpoint_list.push_back(transaction);
    }

    /// checkpoint 队列深度（待 checkpoint 事务数）。PARITY: ext4_rs `checkpoint_depth`（journal.rs:133-135）。
    pub(in crate::fs::ext4::core) fn checkpoint_depth(&self) -> usize {
        self.checkpoint_list.len()
    }

    /// 只读访问 revoke 表（差分对拍语义用）。
    pub(in crate::fs::ext4::core) fn table(&self) -> &RevokeTable {
        &self.table
    }

    /// 取某 tid 的 checkpoint 事务（差分查 revoke 后是否真删了 buffer 用）。
    pub(in crate::fs::ext4::core) fn checkpoint_transaction(
        &self,
        tid: u32,
    ) -> Option<&CheckpointTransaction> {
        self.checkpoint_list
            .iter()
            .find(|transaction| transaction.tid() == tid)
    }

    /// **revoke 一个块（复刻 BUG-5）**：遍历 checkpoint 队列，把 `block` 从每个事务的 buffer 删掉
    /// （它不再被写回 home），并把 `(block, 当前最高 checkpoint tid)` 记进 revoke 表。返回**实际删到
    /// 该块的 checkpoint 事务数**（与 ext4_rs 返回值一致）。
    ///
    /// PARITY: ext4_rs `JournalRuntime::revoke_checkpoint_metadata_block`（journal.rs:137-149）逐字复刻：
    /// `if !enabled { return 0 }`；遍历 `checkpoint_list`，`remove_metadata_block(block_nr)` 成功则
    /// `revoked = revoked.saturating_add(1)`；返回 `revoked`。
    ///
    // PARITY: BUG-5 revoke 从不写盘（bug.md B-05）——本路径**只**改内存（删 checkpoint buffer +
    // 记 revoke 表），绝不写 `JBD2_REVOKE_BLOCK`（type=5）；commit emitter（commit.rs）同样从不产出
    // revoke 块。块复用崩溃场景下可能重放陈旧数据；fix deferred（与 BUG-6 一起修）。
    pub(in crate::fs::ext4::core) fn revoke_checkpoint_metadata_block(
        &mut self,
        block: Ext4Fsblk,
    ) -> usize {
        // PARITY: ext4_rs journal.rs:138-140 —— 禁用时恒 0，不动表/队列。
        if !self.enabled {
            return 0;
        }

        // 记 revoke 表：撤销它的 tid 取**当前 checkpoint 队列最高 tid**（最新已 commit 事务）；
        // 队列空时用 0（无已 commit 事务承载该 revoke）。core 扩展字段，对 ext4_rs 行为无影响。
        let revoking_tid = self
            .checkpoint_list
            .iter()
            .map(|transaction| transaction.tid())
            .max()
            .unwrap_or(0);
        self.table.record(block, revoking_tid);

        // PARITY: ext4_rs journal.rs:142-148 —— 遍历 checkpoint_list 删 buffer，计实际删到数。
        let mut revoked = 0usize;
        for transaction in self.checkpoint_list.iter_mut() {
            if transaction.remove_metadata_block(block) {
                revoked = revoked.saturating_add(1);
            }
        }
        revoked
    }
}

/// 解析一个 journal 块为 revoke 条目（块号集合），追加进 `revoked`。
///
/// `raw` 是一整个 journal 块的字节（长度 >= block_size）。`is_64bit` = journal SB 是否带
/// `JBD2_FEATURE_INCOMPAT_64BIT`（决定每条 revoke 记录是 8B 还是 4B 块号）。块不是合法 revoke
/// 块（magic 错 / blocktype != 5 / count 越界）时**静默不动**（与 ext4_rs 防御一致）。
///
/// PARITY: ext4_rs `Jbd2Journal::parse_revoke_entries`（recovery.rs:207-230）逐字复刻：
/// - `raw.len() < size_of::<RevokeBlockHeader>()`（16）→ 返回；
/// - 读 16B header，`used = header.count()`（已用字节数，**含 16B 头**，大端）；
/// - `used < 16 || used > raw.len()` → 返回（防御越界 count）；
/// - `entry_size = blocknr_size()`（64BIT→8 / 否则 4）；
/// - 从 offset=16 起，`while offset + entry_size <= used`：读大端块号（8B→u64 / 4B→u32 as u64）、
///   `insert`、`offset += entry_size`。
///
/// 调用方（Task 5 的 REVOKE 趟）须先经块头 `blocktype() == JBD2_REVOKE_BLOCK` 门控再调本函数；
/// 本函数自身也再校验一遍 magic + blocktype，双重防御坏块不 panic。
pub(in crate::fs::ext4::core) fn parse_revoke_block(
    raw: &[u8],
    is_64bit: bool,
    revoked: &mut BTreeSet<Ext4Fsblk>,
) {
    // PARITY: recovery.rs:208-210 —— 不足一个 header 即返回。
    if raw.len() < size_of::<RawRevokeBlockHeader>() {
        return;
    }
    let header = RawRevokeBlockHeader::from_bytes(&raw[..size_of::<RawRevokeBlockHeader>()]);
    // 双重防御：magic + blocktype（ext4_rs 调用方在 recovery.rs:192-194 已门控，这里再校验）。
    if !header.header().is_valid_magic() || header.header().blocktype() != JBD2_REVOKE_BLOCK {
        return;
    }

    // PARITY: recovery.rs:212-215 —— used = count()（含 16B 头）；越界即返回。
    let used = header.count() as usize;
    if used < size_of::<RawRevokeBlockHeader>() || used > raw.len() {
        return;
    }

    // PARITY: recovery.rs:217 —— entry_size = blocknr_size()（64BIT→8 / 否则 4）。
    let entry_size = if is_64bit {
        size_of::<u64>()
    } else {
        size_of::<u32>()
    };

    // PARITY: recovery.rs:218-229 —— 从 16 起逐条读大端块号。
    let mut offset = size_of::<RawRevokeBlockHeader>();
    while offset + entry_size <= used {
        let block_nr = if entry_size == size_of::<u64>() {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&raw[offset..offset + 8]);
            u64::from_be_bytes(bytes)
        } else {
            let mut bytes = [0u8; 4];
            bytes.copy_from_slice(&raw[offset..offset + 4]);
            u32::from_be_bytes(bytes) as u64
        };
        revoked.insert(block_nr);
        offset += entry_size;
    }
}

/// 反查辅助（解析器 round-trip 测试 / Task 5 自检用）：把一组块号编码成一个 revoke 块。
///
/// **注意：此函数不在任何写盘路径上**——commit emitter 复刻 BUG-5，**绝不**产出 revoke 块。
/// 它只用于「手搓一个 revoke 块喂给 [`parse_revoke_block`] 验 round-trip」，确认解析侧（Task 5
/// REVOKE 趟）按 ext4_rs 大端布局正确解码。布局逐字对齐 ext4_rs `RevokeBlockHeader::new` +
/// recovery 解析的逆：16B 大端头（magic/type=5/seq + r_count=used）后紧排大端块号。
#[cfg(ktest)]
pub(in crate::fs::ext4::core) fn build_revoke_block(
    sequence: u32,
    blocks: &[Ext4Fsblk],
    is_64bit: bool,
    block_size: usize,
) -> Vec<u8> {
    use super::format::RawJournalHeader;

    let entry_size = if is_64bit {
        size_of::<u64>()
    } else {
        size_of::<u32>()
    };
    let used = size_of::<RawRevokeBlockHeader>() + blocks.len() * entry_size;

    let mut block = vec![0u8; block_size];
    // 16B 大端头：magic + blocktype=5 + sequence；r_count = used（含 16B 头），大端。
    let header = RawJournalHeader::new(JBD2_REVOKE_BLOCK, sequence);
    block[..size_of::<RawJournalHeader>()].copy_from_slice(header.as_bytes());
    block[size_of::<RawJournalHeader>()..size_of::<RawRevokeBlockHeader>()]
        .copy_from_slice(&(used as u32).to_be_bytes());

    // 块号大端紧排在 16B 头之后。
    let mut offset = size_of::<RawRevokeBlockHeader>();
    for &b in blocks {
        if is_64bit {
            block[offset..offset + 8].copy_from_slice(&b.to_be_bytes());
        } else {
            block[offset..offset + 4].copy_from_slice(&(b as u32).to_be_bytes());
        }
        offset += entry_size;
    }
    block
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::{
        build_revoke_block, parse_revoke_block, CheckpointTransaction, RevokeRuntime, RevokeTable,
    };
    use crate::prelude::*;

    /// RevokeTable 语义：record + 扁平包含 + as-of-tid 查询。
    #[ktest]
    fn revoke_table_semantics() {
        let mut table = RevokeTable::new();
        assert!(table.is_empty());
        assert!(!table.is_revoked(7), "fresh table revokes nothing");

        // 记 block 7 被 tid 3 撤销。
        table.record(7, 3);
        assert!(table.is_revoked(7), "recorded block is revoked (flat contains)");
        assert!(!table.is_revoked(8), "other block not revoked");
        assert_eq!(table.len(), 1);

        // 扁平包含 = recovery 复刻判据（无 tid 比较）。
        // as-of-tid：撤销它的 tid(3) >= 重放它的 tid → 跳过。
        assert!(table.is_revoked_as_of(7, 3), "revoked by tid>=replaying tid");
        assert!(table.is_revoked_as_of(7, 1), "revoked by a higher tid");
        assert!(!table.is_revoked_as_of(7, 4), "revoked by an older tid only");

        // 重复 revoke 同一块保留**最高** tid。
        table.record(7, 5);
        assert!(table.is_revoked_as_of(7, 5), "higher revoking tid retained");
        table.record(7, 2); // 更低 tid 不降级
        assert!(table.is_revoked_as_of(7, 5), "lower revoke does not lower retained tid");
        assert_eq!(table.len(), 1, "same block stays one entry");
    }

    /// `revoke_checkpoint_metadata_block` 复刻 BUG-5：只删内存 checkpoint buffer + 记表，
    /// 返回实际删到的事务数；禁用运行时恒返回 0。**不产出任何 revoke 块**（无写盘接缝）。
    #[ktest]
    fn revoke_checkpoint_removes_buffer_and_records() {
        let mut rt = RevokeRuntime::new();
        // 两个已 commit 待 checkpoint 的事务：tid 1 持有块 {2,9}，tid 2 持有块 {2,5}。
        rt.push_checkpoint(CheckpointTransaction::new(
            1,
            [(2u64, vec![0xAA; 8]), (9u64, vec![0xBB; 8])],
        ));
        rt.push_checkpoint(CheckpointTransaction::new(
            2,
            [(2u64, vec![0xCC; 8]), (5u64, vec![0xDD; 8])],
        ));
        assert_eq!(rt.checkpoint_depth(), 2);

        // revoke 块 2：两个事务都持有它 → 删到 2 个；revoke 表记 (2, 最高tid=2)。
        let removed = rt.revoke_checkpoint_metadata_block(2);
        assert_eq!(removed, 2, "block 2 removed from both checkpoint txns");
        assert!(rt.table().is_revoked(2), "revoke table records block 2");
        assert!(rt.table().is_revoked_as_of(2, 2), "revoked by highest ckpt tid (2)");
        assert!(!rt.checkpoint_transaction(1).unwrap().has_buffer(2), "buffer deleted in tid 1");
        assert!(!rt.checkpoint_transaction(2).unwrap().has_buffer(2), "buffer deleted in tid 2");
        // 其它块仍在（只删被 revoke 的那个）。
        assert!(rt.checkpoint_transaction(1).unwrap().has_buffer(9));
        assert!(rt.checkpoint_transaction(2).unwrap().has_buffer(5));
        assert_eq!(rt.checkpoint_transaction(1).unwrap().modified_block_count(), 1);

        // revoke 一个没人持有的块：删到 0，但表仍记下（恢复时仍要跳过）。
        let removed_none = rt.revoke_checkpoint_metadata_block(42);
        assert_eq!(removed_none, 0, "no checkpoint txn holds block 42");
        assert!(rt.table().is_revoked(42), "block 42 still recorded in table");

        // 禁用运行时：恒 0，不动表。
        let mut disabled = RevokeRuntime::disabled();
        disabled.push_checkpoint(CheckpointTransaction::new(1, [(2u64, vec![0; 8])]));
        assert_eq!(disabled.revoke_checkpoint_metadata_block(2), 0, "disabled returns 0");
        assert!(!disabled.table().is_revoked(2), "disabled records nothing");
    }

    /// 解析器 round-trip：手搓一个 revoke 块 → `parse_revoke_block` 解回同一组块号（4B / 8B 两种条目）。
    #[ktest]
    fn parse_revoke_block_roundtrip() {
        let bs = 4096usize;

        // 4 字节条目（非 64BIT）。
        let blocks32: [u64; 3] = [0x10, 0x1234, 0x00AB_CDEF];
        let raw32 = build_revoke_block(7, &blocks32, false, bs);
        let mut got32: BTreeSet<u64> = BTreeSet::new();
        parse_revoke_block(&raw32, false, &mut got32);
        let want32: BTreeSet<u64> = blocks32.iter().copied().collect();
        assert_eq!(got32, want32, "4B revoke entries round-trip (big-endian)");

        // 8 字节条目（64BIT）：含一个 > 2^32 的块号验高半位。
        let blocks64: [u64; 2] = [0x1_0000_0005, 0x42];
        let raw64 = build_revoke_block(9, &blocks64, true, bs);
        let mut got64: BTreeSet<u64> = BTreeSet::new();
        parse_revoke_block(&raw64, true, &mut got64);
        let want64: BTreeSet<u64> = blocks64.iter().copied().collect();
        assert_eq!(got64, want64, "8B revoke entries round-trip (64BIT, big-endian)");

        // 防御：非 revoke 块（全零，magic 错）解出空集、不 panic。
        let zero = vec![0u8; bs];
        let mut empty: BTreeSet<u64> = BTreeSet::new();
        parse_revoke_block(&zero, false, &mut empty);
        assert!(empty.is_empty(), "non-revoke block parses to empty set");

        // 防御：count 越界（伪造 r_count > 块长）→ 不读、空集。
        let mut bad = build_revoke_block(1, &[0x55u64], false, bs);
        bad[12..16].copy_from_slice(&(bs as u32 + 100).to_be_bytes()); // r_count 越界
        let mut bad_out: BTreeSet<u64> = BTreeSet::new();
        parse_revoke_block(&bad, false, &mut bad_out);
        assert!(bad_out.is_empty(), "out-of-range count parses to empty set");
    }
}
