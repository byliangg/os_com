// SPDX-License-Identifier: MPL-2.0
//! JBD2 崩溃恢复 / replay（三趟）。**全大端。** ★RED-LINE★
//!
//! 逐字节 / 逐语义复刻 ext4_rs `ext4_impls/jbd2/recovery.rs` 的恢复三趟 +
//! `reset_recovered_state`（SB 重置）。`mod.rs` 的 `recover` 在 `needs_recovery()`
//! 为真时被调用（集成层 P6 才接入 fs-超级块 RECOVER 标志门控——core 这里只做
//! 「`s_start != 0` 即恢复」这一关，**不**看 ext4 fs-SB RECOVER 位，与 brief 一致）。
//!
//! **三趟（PARITY recovery.rs:42-77）：**
//! 1. **SCAN**（`scan_committed_transactions` + `try_read_committed_transaction`）：
//!    从 `s_start`（log tail）起，逐事务读 `descriptor → payloads → commit`，
//!    用 **magic + blocktype + commit.sequence == descriptor.sequence** 判定一个事务
//!    是否有效落盘；遇到第一个无效 / 缺失 commit 就停（返回 `None`，SCAN 结束）。
//!    **NO csum 校验**——ext4_rs recovery 全程不校 crc（仅 `is_valid_magic` + blocktype +
//!    seq 匹配），逐字复刻；坏 csum 不停（坏 commit-magic / type / seq 才停，见
//!    `journal_recover_badcsum_parity`）。
//! 2. **REVOKE**（`scan_revoke_blocks` → `parse_revoke_block`）：扫 `[start, end)` 内
//!    所有 `JBD2_REVOKE_BLOCK`（type=5），解析成一个**扁平 `BTreeSet<u64>`**（块号集合，
//!    **无 tid/sequence 维度**）。**复刻 BUG-5**：commit 路径从不写 revoke 块，故真镜像上
//!    这趟恒解出空集——但算法仍逐字复刻。
//! 3. **REPLAY**（`recover` 主循环）：对每个有效事务的每个 metadata 块，
//!    `if revoked.contains(&block_nr) { continue }`（**扁平包含**，无 sequence 上界），
//!    否则把 journaled payload（escape 已在 SCAN 阶段还原）写到 home（`block_nr * block_size`）。
//!
//! **SB 重置**（`reset_recovered_state`，PARITY recovery.rs:79-91）：
//! `s_sequence = next_sequence`（= 末事务 seq+1 饱和，无事务则保持原 `s_sequence`）；
//! `s_start = 0`；`s_head = s_first`；重算 SB csum；store 到 journal 逻辑块 0。
//!
//! **BUG-6**（recovery.rs，无 `s_sequence` 上界）：SCAN 不对 `s_sequence` 做上界 wrap 检测，
//! 只靠 commit.sequence == descriptor.sequence + ring `head` / `usable_blocks` 上限收口；
//! 逐字复刻（fix deferred，bug.md B-06）。

use super::super::io::BlockReader;
use super::super::io::BlockWriter;
use super::super::prelude::*;
use super::format::{
    RawJournalBlockTag, RawJournalBlockTag3, RawJournalBlockTail, RawJournalHeader,
    RawJournalSuperblock, JBD2_COMMIT_BLOCK, JBD2_DESCRIPTOR_BLOCK, JBD2_FEATURE_INCOMPAT_64BIT,
    JBD2_FEATURE_INCOMPAT_CSUM_V3, JBD2_FLAG_ESCAPE, JBD2_FLAG_LAST_TAG, JBD2_MAGIC,
    JBD2_REVOKE_BLOCK, JBD2_SUPERBLOCK_SIZE,
};
use super::revoke::{parse_revoke_block_into_table, RevokeTable};
use super::space::JournalSpace;
use super::superblock::journal_sb_checksum;

/// 恢复盘上下文：journal 区物理块映射 + journal 读接缝 + home/SB 写接缝 + 几何。
///
/// **读 journal 块**：`reader.read_at(physical_blocks[logical] * block_size, ..)` —— 与 ext4_rs
/// `JournalDevice::read_raw_block`（device.rs:76-88）同寻址。**写 home / store SB**：raw 字节写，
/// `writer.write_at(off, ..)` —— 对齐 ext4_rs recovery 写 home `block_device.write_offset(block_nr*bs, data)`
/// （recovery.rs:61）与 SB store `device.write_block(0, sb_bytes)`→`write_offset(physical_blocks[0]*bs, padded)`
/// （device.rs:146-166 / superblock.rs:155-162）。**注意**：home / SB 写**不经** JBD2 `MetadataWriter`
/// 记账接缝——recovery 是 replay 直写盘，不再记账（与 ext4_rs 一致）。
pub(in crate::fs::ext4) struct RecoverCtx<'a> {
    /// journal inode 的物理块向量：`physical_blocks[i]` = journal 逻辑块 i 的 fs 物理块号。
    pub physical_blocks: &'a [Ext4Fsblk],
    /// journal 块读接缝（差分里 `MemDisk` 直读同一份字节）。
    pub reader: &'a dyn BlockReader,
    /// home / SB 写接缝（差分里 `MemDisk` 直写同一份字节，按字节偏移）。
    pub writer: &'a dyn BlockWriter,
    /// journal 块大小（== fs block_size）。
    pub block_size: usize,
}

/// 恢复结果。PARITY: ext4_rs `JournalRecoveryResult`（recovery.rs:6-12）字段一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::fs::ext4) struct RecoverResult {
    /// 重放的有效事务数（SCAN 找到的有效事务个数）。
    pub transactions_replayed: u32,
    /// 实际写回 home 的 metadata 块数（已跳过 revoked 的）。
    pub metadata_blocks_replayed: u32,
    /// REVOKE 趟建出的扁平 revoke 集合大小（BUG-5 下真镜像恒 0）。
    pub revoked_blocks: u32,
    /// 末个有效事务的序号；无有效事务时 `None`。
    pub last_sequence: Option<u32>,
}

/// 一个有效落盘事务的恢复视图：序号 + 环内下一个事务起点 + metadata 全块镜像（escape 已还原）。
/// PARITY: ext4_rs `JournalRecoveryTransaction`（recovery.rs:14-19）。
#[derive(Debug, Clone)]
struct RecoveryTransaction {
    sequence: u32,
    next_head: u32,
    /// PARITY: ext4_rs `metadata_blocks: Vec<JournalCommitBlock>`——`(block_nr, escape 已还原的全块镜像)`。
    metadata_blocks: Vec<RecoveryBlock>,
}

/// 一个待重放的 metadata 块：目标 fs 块号 + escape 已还原的全块镜像。
#[derive(Debug, Clone)]
struct RecoveryBlock {
    block_nr: Ext4Fsblk,
    block_data: Vec<u8>,
}

/// 一个 descriptor tag 的解码结果：目标 fs 块号 + flags（escape 判定用）。
/// PARITY: ext4_rs `DescriptorTag`（recovery.rs:21-25）。
#[derive(Debug, Clone, Copy)]
struct DescriptorTag {
    target_fs_block: Ext4Fsblk,
    flags: u32,
}

/// 是否需要恢复。PARITY: ext4_rs `needs_recovery`（recovery.rs:28-30）—— `s_start != 0`。
/// **不**看 ext4 fs-超级块 RECOVER 标志（那是调用方 / 集成层 P6 的职责）。
// PARITY: recovery.rs:28-30 —— needs_recovery == (s_start != 0)。
pub(in crate::fs::ext4) fn needs_recovery(sb: &RawJournalSuperblock) -> bool {
    sb.start() != 0
}

/// 崩溃恢复三趟 replay + SB 重置。PARITY: ext4_rs `recover`（recovery.rs:32-77）逐字复刻。
///
/// `sb` 进出都是当前盘上 journal 超级块镜像；本函数在 replay 后就地把它重置（s_start=0 等）
/// 并 store 回 journal 逻辑块 0。`space` 与 ext4_rs 一样在重置后重建——core 这里在内部按需构造，
/// 不要求调用方传入（ext4_rs 持有 `self.space`，core/journal 不接集成层，故 SCAN 用的环数学
/// 由本函数从 `sb` 几何现场构造，与 ext4_rs `self.space` 同值）。
pub(in crate::fs::ext4) fn recover(
    ctx: &RecoverCtx<'_>,
    sb: &mut RawJournalSuperblock,
) -> Result<RecoverResult> {
    // PARITY: recovery.rs:33-40 —— 不需恢复则返回空结果（不动盘）。
    if !needs_recovery(sb) {
        return Ok(RecoverResult {
            transactions_replayed: 0,
            metadata_blocks_replayed: 0,
            revoked_blocks: 0,
            last_sequence: None,
        });
    }

    // SCAN 用的环数学：从 sb 几何构造（与 ext4_rs `self.space` 同值——head/tail 由 from_superblock 推）。
    let space = JournalSpace::from_superblock(sb)?;

    // ===== 趟 1：SCAN —— 找有效已 commit 事务序列 =====
    // PARITY: recovery.rs:42 —— self.scan_committed_transactions(..)。
    let transactions = scan_committed_transactions(ctx, sb, &space)?;

    // PARITY: recovery.rs:43-47 —— last_sequence / end（末事务的 next_head，无则 s_start）。
    let last_sequence = transactions.last().map(|tx| tx.sequence);
    let end = transactions
        .last()
        .map(|tx| tx.next_head)
        .unwrap_or_else(|| sb.start());

    // ===== 趟 2：REVOKE —— 扫 [start, end) 建带序列号的 revoke 表（BUG-5/6 fix） =====
    // [对照] Linux PASS_REVOKE：把每条 revoke 记录连同其所在事务序号记进 revoke 表（block→最高
    // 撤销 seq）。BUG-5 fix 后真镜像不再恒空——释放/复用块的事务会写出 revoke 块。
    let revoked = scan_revoke_blocks(ctx, sb, &space, sb.start(), end)?;

    // ===== 趟 3：REPLAY —— 逐事务、逐块写回 home（按序列号规则跳过 revoked，escape 已在 SCAN 还原） =====
    // [对照] Linux PASS_REPLAY + `jbd2_journal_test_revoke`。
    let mut metadata_blocks_replayed = 0u32;
    for transaction in &transactions {
        for metadata in &transaction.metadata_blocks {
            // BUG-5/6 fix（JBD2 revoke 规则）：仅当存在一条**序号 >= 本事务序号**的 revoke 记录时
            // 才跳过该块。否则（块在更早事务被 revoke、之后又被本事务重新 journal）必须 replay 新镜像。
            // [对照] Linux `jbd2_journal_test_revoke`：`record->sequence >= sequence` 才 revoked。
            if revoked.is_revoked_as_of(metadata.block_nr, transaction.sequence) {
                continue;
            }
            // [对照] home 偏移 = block_nr * block_size（溢出即 EINVAL）。
            let offset = (metadata.block_nr as usize)
                .checked_mul(ctx.block_size)
                .ok_or_else(|| {
                    Error::with_message(Errno::EINVAL, "recovery block offset overflow")
                })?;
            // [对照] block_device.write_offset(offset, &block_data)（raw home 写）。
            ctx.writer.write_at(offset, &metadata.block_data);
            metadata_blocks_replayed = metadata_blocks_replayed.saturating_add(1);
        }
    }

    // PARITY: recovery.rs:66-69 —— next_sequence = 末事务 seq+1 饱和；无事务则保持 sb.sequence()。
    let next_sequence = last_sequence
        .map(|sequence| sequence.saturating_add(1))
        .unwrap_or_else(|| sb.sequence());
    reset_recovered_state(ctx, sb, next_sequence)?;

    Ok(RecoverResult {
        transactions_replayed: transactions.len() as u32,
        metadata_blocks_replayed,
        revoked_blocks: revoked.len() as u32,
        last_sequence,
    })
}

// (revoke 表由 `scan_revoke_blocks` 建，REPLAY 趟据 `is_revoked_as_of` 按序列号过滤。)

/// SB 重置：恢复完成后把 journal 标记为「空 / 已 checkpoint」。
/// PARITY: ext4_rs `reset_recovered_state`（recovery.rs:79-91）——
/// `update_sequence(next)` / `update_start(0)` / `update_head(first)` 各自重算 csum（仅末次有效），
/// store 块 0。core 顺序一致：set_sequence → set_start(0) → set_head(first) → 重算 csum → store。
fn reset_recovered_state(
    ctx: &RecoverCtx<'_>,
    sb: &mut RawJournalSuperblock,
    next_sequence: u32,
) -> Result<()> {
    // PARITY: recovery.rs:84 —— first 取自当前 SB（重置前读）。
    let first = sb.first();
    // PARITY: recovery.rs:85-87 —— update_sequence / update_start(0) / update_head(first)。
    // set_sequence 同时改 s_header.h_sequence + s_sequence（format.rs，对齐 ext4_rs jbd2.rs:220-223）。
    sb.set_sequence(next_sequence);
    sb.set_start(0);
    sb.set_head(first);
    // PARITY: 每个 update_* 末尾 update_checksum——只末次有效，等价于此处统一重算一次。
    recompute_sb_checksum(sb);
    // PARITY: recovery.rs:88 —— store 块 0。core 复刻 ext4_rs store：建整块（前 1024=SB，余零）raw 写。
    store_journal_sb(ctx, sb)
    // 注：ext4_rs recovery.rs:89 还重建 `self.space`——core/journal 不持久 space（每次 recover 现场构造），
    // 无对应状态，故省略；不影响盘字节 / 返回结果。
}

/// 重算并写回 journal SB 校验和。PARITY: ext4_rs `update_checksum`（jbd2.rs:264-266）——
/// `compute_checksum`（[0xFC..0x100] 置零的 crc）→ `s_checksum = csum.to_be()`。
fn recompute_sb_checksum(sb: &mut RawJournalSuperblock) {
    let mut image = [0u8; JBD2_SUPERBLOCK_SIZE];
    image.copy_from_slice(sb.as_bytes());
    let checksum = journal_sb_checksum(&image);
    sb.set_checksum(checksum);
}

/// 把 journal SB（1024 字节）store 回 journal **逻辑块 0**（= physical_blocks[0]）。
/// PARITY: ext4_rs `JournalSuperblockState::store`（superblock.rs:155-162）→
/// `device.write_block(.., 0, &sb.to_bytes())`：写不足 block_size 时补零整块（device.rs:162-164），
/// 故 SB store 实际写块 0 的 [0,1024)=SB、[1024,block_size)=0。core 复刻：建整块 raw 写。
fn store_journal_sb(ctx: &RecoverCtx<'_>, sb: &RawJournalSuperblock) -> Result<()> {
    let sb_pblock = *ctx
        .physical_blocks
        .first()
        .ok_or_else(|| Error::with_message(Errno::EINVAL, "journal has no physical blocks"))?;
    let mut block = vec![0u8; ctx.block_size];
    block[..JBD2_SUPERBLOCK_SIZE].copy_from_slice(sb.as_bytes());
    let offset = (sb_pblock as usize)
        .checked_mul(ctx.block_size)
        .ok_or_else(|| Error::with_message(Errno::EINVAL, "journal SB offset overflow"))?;
    ctx.writer.write_at(offset, &block);
    Ok(())
}

/// 读一个 journal 逻辑块（整块，长度 == block_size）。
/// PARITY: ext4_rs `JournalDevice::read_raw_block`（device.rs:76-88）——`physical_blocks[logical]*bs` 字节偏移。
fn read_journal_block(ctx: &RecoverCtx<'_>, logical_block: u32) -> Result<Vec<u8>> {
    let physical = *ctx
        .physical_blocks
        .get(logical_block as usize)
        .ok_or_else(|| Error::with_message(Errno::EINVAL, "journal logical block out of range"))?;
    let offset = (physical as usize)
        .checked_mul(ctx.block_size)
        .ok_or_else(|| Error::with_message(Errno::EINVAL, "journal block offset overflow"))?;
    let mut block = vec![0u8; ctx.block_size];
    ctx.reader.read_at(offset, &mut block);
    Ok(block)
}

/// 读一个 journal 块头（12B 大端），magic 不合法返回 `None`。
/// PARITY: ext4_rs `read_header`（recovery.rs:287-291）——`is_valid_magic().then_some(header)`。
fn read_header(raw: &[u8]) -> Option<RawJournalHeader> {
    let bytes = raw.get(..size_of::<RawJournalHeader>())?;
    let header = RawJournalHeader::from_bytes(bytes);
    header.is_valid_magic().then_some(header)
}

/// SCAN：从 `s_start` 起逐事务读，直到第一个**序号不连续 / 无效 / 缺失 commit** 的块。
/// [对照] Linux `do_one_pass`：终止条件是 `sequence != next_commit_ID` 或块无效——**不**靠
/// 存储的 head 指针收口（见下方 over-replay 论证）。
fn scan_committed_transactions(
    ctx: &RecoverCtx<'_>,
    sb: &RawJournalSuperblock,
    space: &JournalSpace,
) -> Result<Vec<RecoveryTransaction>> {
    // start==0 早返（needs_recovery 已挡，这里防御）。
    let start = sb.start();
    if start == 0 {
        return Ok(Vec::new());
    }

    let mut cursor = start;
    let mut walked_blocks = 0u32;
    let mut transactions = Vec::new();
    // `max_walk` = 环可用块数，仅作**死循环软护栏**（绝不能比环还长），**不是**正确性终止条件。
    let max_walk = space.usable_blocks().max(1);

    // 终止条件（[对照] Linux `do_one_pass`）：逐事务 `try_read_committed_transaction` 返回有效已 commit
    // 事务、且其序号 == 期望的下一个 commit ID（首事务确立基线，其后严格 +1）。第一个**非连续** /
    // **无效 / 缺 commit** 的块即停。
    //
    // **为何 **不能** 用存储的 `s_head`（`s_sequence` 同款 under-replay 漏洞，本次修复）**：commit 写序
    // 里 commit 块写完后**没有第二道屏障**（commit.rs 唯一屏障在 descriptor/payload→commit 之间；commit
    // 块、SB 的 `s_head`/`s_sequence` 写、ring advance 都在屏障**之后**、彼此无序）。崩溃可能落在「最后
    // 一笔事务 L 的 commit 块已持久、推进 `s_head` 的 SB 写丢失」之间——此时盘上 `s_head` **滞后一笔**，
    // 指向 L 的起点而非 L 之后。旧码 `cursor == head` 硬停会在读 L **之前**停下 → 真正已 commit 的 L 不
    // replay = 丢元数据 = 损坏（与已删的 `s_sequence` 上界同类）。故 `s_head` 不作终止条件。
    //
    // **over-replay 边界（关键安全论证——无 head 收口时什么挡住越界 replay）**：日志序号**全局单调、
    // 永不复用**。活动窗口 `[s_start, head_true)` 持最近、序号最高的事务。越过真实末事务 M 之后，
    // `cursor` 落进 free 区，那里的块只能是：(a) 从未写过 → 全零 → `read_header` magic 校验失败 →
    // `try_read` 返回 None → 停；或 (b) **前一轮回绕**写下的陈旧事务 → 它写于更早一轮，序号必 **< M+1**
    // （M+1 是下一个待分配序号，尚未存在；任何已落盘块只能携带历史上某个 <= M 的序号）→ 严格连续校验
    // `seq != expected(M+1)` 命中 → 停。唯一能在 `cursor` 处出现序号 == M+1 的块，是 M+1 **真被 commit**
    // 过——那本就该 replay（正是 stale-head 会误丢的真实事务）。序号按**值**比较，与环位置 / 回绕无关
    // （`space.advance` 只算物理块号，不动序号）→ 回绕场景同样成立。故 strict-consecutive + commit 有效性
    // **充分**封死 over-replay，无需 head。
    let mut next_commit_id: Option<u32> = None;

    // 逐事务读；终止 = 序号不连续 / 无效 / 缺 commit。`max_walk` 仅防死循环（软护栏）。
    while walked_blocks < max_walk {
        let Some(transaction) = try_read_committed_transaction(ctx, sb, space, cursor)? else {
            break;
        };
        // 序号必须严格等于期望的下一个 commit ID（首事务确立基线），否则停。
        match next_commit_id {
            None => next_commit_id = Some(transaction.sequence.wrapping_add(1)),
            Some(expected) => {
                if transaction.sequence != expected {
                    break;
                }
                next_commit_id = Some(expected.wrapping_add(1));
            }
        }
        // advanced = distance(cursor, next_head).max(1)（推进软护栏计数）。
        let advanced = space.distance(cursor, transaction.next_head).max(1);
        walked_blocks = walked_blocks.saturating_add(advanced);
        cursor = transaction.next_head;
        transactions.push(transaction);
        // 注：**不再** 用 `cursor == sb.head()` 硬停——存储 head 可能 stale-by-one（见上），
        // 硬停会丢真正已 commit 的末事务。终止全靠 strict-consecutive + commit 有效性。
    }

    Ok(transactions)
}

/// 尝试读一个落在 `ring_start` 的有效已 commit 事务。无效 / 缺失 commit 返回 `None`。
///
/// **环内事务布局**（与 commit emitter `write_commit_plan` 逐字对齐，commit.rs:454-491）：
/// `[descriptor]?[payload × tag 数][JBD2_REVOKE_BLOCK × R][commit]`。
/// - 有 metadata → `ring_start` 是 descriptor 块；revoke-only 事务（无 metadata，仅 revoke）→
///   `ring_start` 是首个 `JBD2_REVOKE_BLOCK`（emitter 不写 descriptor，commit.rs:459/483）。
/// - **revoke 块（type=5）落在 payload 之后、commit 之前**（BUG-5 fix，commit.rs:504-506）。
///
/// **判定有效仅靠：descriptor magic + blocktype==DESCRIPTOR + 至少一个 tag（或 revoke-only 时
/// 首块 type==REVOKE）；中间的 REVOKE 块按本事务 sequence 跳过；commit magic + blocktype==COMMIT +
/// commit.sequence == 本事务 sequence。NO csum 校验。**（坏 csum 不停。）
fn try_read_committed_transaction(
    ctx: &RecoverCtx<'_>,
    sb: &RawJournalSuperblock,
    space: &JournalSpace,
    ring_start: u32,
) -> Result<Option<RecoveryTransaction>> {
    // 读环起点块头：magic 错 → None。
    let head_raw = read_journal_block(ctx, ring_start)?;
    let Some(header) = read_header(&head_raw) else {
        return Ok(None);
    };

    // 本事务的 sequence + payload 后的游标 + metadata 镜像，按起点块类型分两路：
    // (a) DESCRIPTOR：解析 tag → 逐 tag 读 payload；本事务 seq = descriptor.sequence；
    // (b) REVOKE（revoke-only 事务，无 descriptor/payload）：本事务 seq = 首 revoke 块 sequence，
    //     游标停在 ring_start（从这里开始跳 revoke 块），metadata 为空。
    let (tx_sequence, mut data_cursor, metadata_blocks): (u32, u32, Vec<RecoveryBlock>) =
        if header.blocktype() == JBD2_DESCRIPTOR_BLOCK {
            // 解析 tag 串；空 / 无 LAST_TAG → None。
            let descriptor_tags = match parse_descriptor_tags(sb, &head_raw, ctx.block_size) {
                Some(tags) if !tags.is_empty() => tags,
                _ => return Ok(None),
            };
            // 逐 tag 读 payload（escape 还原），cursor 顺序前进。
            let mut cursor = space.advance(ring_start, 1);
            let mut metadata_blocks = Vec::with_capacity(descriptor_tags.len());
            for tag in &descriptor_tags {
                let mut block_data = read_journal_block(ctx, cursor)?;
                // escape 还原：首 4 字节恢复成大端 JBD2 magic。
                if (tag.flags & JBD2_FLAG_ESCAPE) != 0 && block_data.len() >= size_of::<u32>() {
                    block_data[..size_of::<u32>()].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
                }
                metadata_blocks.push(RecoveryBlock {
                    block_nr: tag.target_fs_block,
                    block_data,
                });
                cursor = space.advance(cursor, 1);
            }
            (header.sequence(), cursor, metadata_blocks)
        } else if header.blocktype() == JBD2_REVOKE_BLOCK {
            // revoke-only 事务：无 descriptor/payload；本事务 seq = 首 revoke 块 sequence。
            // 游标停在 ring_start，下面的 revoke-skip 循环从这里开始消费 revoke 块。
            (header.sequence(), ring_start, Vec::new())
        } else {
            return Ok(None);
        };

    // BUG-5 fix（关键修复，与 commit emitter 写序对齐）：跳过本事务的 `JBD2_REVOKE_BLOCK`——它们落在
    // payload 之后、commit 之前（commit.rs:504-506）。只跳「magic 合法 + type==REVOKE + sequence==本
    // 事务 seq」的块（恰好消费 emitter 写的那些 revoke 块，不多吞 commit、不少跳第 2 个 revoke 块）。
    // 用环可用块数兜底防御坏镜像死循环（正常路径在首个非 revoke 块即停 = commit 块）。
    let max_skip = space.usable_blocks().max(1);
    let mut skipped = 0u32;
    loop {
        let raw = read_journal_block(ctx, data_cursor)?;
        match read_header(&raw) {
            Some(h) if h.blocktype() == JBD2_REVOKE_BLOCK && h.sequence() == tx_sequence => {
                data_cursor = space.advance(data_cursor, 1);
                skipped = skipped.saturating_add(1);
                if skipped >= max_skip {
                    // 环内全是「本事务 seq 的 revoke 块」——坏镜像，按缺 commit 处理。
                    return Ok(None);
                }
            }
            _ => break, // 首个非（本事务 revoke）块——应是 commit 块。
        }
    }

    // 读 commit 块：magic 错 / 非 COMMIT / seq 不匹配 → None。
    // **这是唯一的「事务是否落盘」判据**——坏 commit-magic / type / seq 才停（坏 csum 不停）。
    let commit_raw = read_journal_block(ctx, data_cursor)?;
    let Some(commit_header) = read_header(&commit_raw) else {
        return Ok(None);
    };
    if commit_header.blocktype() != JBD2_COMMIT_BLOCK || commit_header.sequence() != tx_sequence {
        return Ok(None);
    }

    // next_head = commit 后一块（已在 revoke 块之后——故 SCAN 的 cursor / 末事务 `end` 正确，
    // 喂给 scan_revoke_blocks(start, end) 的范围把本事务的 revoke 块也覆盖在内）。
    let next_head = space.advance(data_cursor, 1);
    Ok(Some(RecoveryTransaction {
        sequence: tx_sequence,
        next_head,
        metadata_blocks,
    }))
}

/// 解析 descriptor 块的 tag 串（从 JournalHeader 之后到块尾 [- tail csum 区]）。
/// PARITY: ext4_rs `parse_descriptor_tags`（recovery.rs:232-257）逐字复刻——
/// 含 csum 特性时把块尾 `JournalBlockTail`（4B）排除出 tag 扫描区；遇 LAST_TAG 收尾；
/// 否则扫到 limit 仍无 LAST_TAG → `None`（不合法 descriptor）。
fn parse_descriptor_tags(
    sb: &RawJournalSuperblock,
    raw: &[u8],
    block_size: usize,
) -> Option<Vec<DescriptorTag>> {
    // PARITY: recovery.rs:233-236 —— 块长 / header 长度防御。
    if raw.len() < block_size || raw.len() < size_of::<RawJournalHeader>() {
        return None;
    }

    let tag_len = tag_length(sb);
    // PARITY: recovery.rs:239-243 —— csum 特性开时 tail csum 区（4B）排除出 tag 扫描区。
    let tail_len = if sb.has_checksum_v2_or_v3() {
        size_of::<RawJournalBlockTail>()
    } else {
        0
    };
    let limit = block_size.checked_sub(tail_len)?;
    let mut offset = size_of::<RawJournalHeader>();
    let mut tags = Vec::new();
    // PARITY: recovery.rs:247-255 —— 逐 tag 读，遇 LAST_TAG 即收尾返回。
    while offset + tag_len <= limit {
        let tag = read_descriptor_tag(sb, raw, offset)?;
        let flags = tag.flags;
        tags.push(tag);
        offset += tag_len;
        if (flags & JBD2_FLAG_LAST_TAG) != 0 {
            return Some(tags);
        }
    }
    None
}

/// 解码一个 descriptor tag（v3=16B / 64BIT=12B / 否则 8B，全大端）。
/// PARITY: ext4_rs `read_descriptor_tag`（recovery.rs:259-285）逐语义复刻——但用 Pod 安全解析
/// 替换 ext4_rs 的 `read_unaligned`/手切片，字节结果一致。
fn read_descriptor_tag(sb: &RawJournalSuperblock, raw: &[u8], offset: usize) -> Option<DescriptorTag> {
    if sb.has_incompat_feature(JBD2_FEATURE_INCOMPAT_CSUM_V3) {
        // PARITY: recovery.rs:260-266 —— v3 16B tag。Pod from_bytes（替 read_unaligned）。
        let bytes = raw.get(offset..offset + size_of::<RawJournalBlockTag3>())?;
        let tag = RawJournalBlockTag3::from_bytes(bytes);
        Some(DescriptorTag {
            target_fs_block: tag.blocknr(),
            flags: tag.flags(),
        })
    } else if sb.has_incompat_feature(JBD2_FEATURE_INCOMPAT_64BIT) {
        // PARITY: recovery.rs:267-276 —— 64BIT（非 v3）12B tag：low(4) | _csum(2)+flags(2) | high(4)，全大端。
        let bytes = raw.get(offset..offset + 12)?;
        let low = u32::from_be_bytes(bytes[0..4].try_into().ok()?);
        // PARITY: recovery.rs:270-271 —— flags 在 [6..8]（[4..8] 的高半 2 字节）。
        let flags = u16::from_be_bytes(bytes[6..8].try_into().ok()?) as u32;
        let high = u32::from_be_bytes(bytes[8..12].try_into().ok()?);
        Some(DescriptorTag {
            target_fs_block: ((high as u64) << 32) | low as u64,
            flags,
        })
    } else {
        // PARITY: recovery.rs:277-284 —— 8B tag。Pod from_bytes（替 read_unaligned）。
        let bytes = raw.get(offset..offset + size_of::<RawJournalBlockTag>())?;
        let tag = RawJournalBlockTag::from_bytes(bytes);
        Some(DescriptorTag {
            target_fs_block: tag.blocknr() as Ext4Fsblk,
            flags: tag.flags() as u32,
        })
    }
}

/// descriptor tag 长度（字节）。PARITY: ext4_rs `tag_length`（mod.rs:459-467）——
/// CSUM_V3 → 16；否则 64BIT → 12；否则 8。
fn tag_length(sb: &RawJournalSuperblock) -> usize {
    if sb.has_incompat_feature(JBD2_FEATURE_INCOMPAT_CSUM_V3) {
        size_of::<RawJournalBlockTag3>() // 16
    } else if sb.has_incompat_feature(JBD2_FEATURE_INCOMPAT_64BIT) {
        12
    } else {
        size_of::<RawJournalBlockTag>() // 8
    }
}

/// REVOKE 趟：扫 `[start, end)` 内所有 `JBD2_REVOKE_BLOCK`，建带序列号的 [`RevokeTable`]
/// （block → 撤销它的最高 sequence，取自各 revoke 块头的 `h_sequence`）。
///
/// [对照] Linux PASS_REVOKE：`start==end` 早返空表；逐块读头，blocktype==REVOKE 则
/// `parse_revoke_block_into_table` 追加（携带块头 sequence）；cursor 环内前进，回到 end 即停。
/// BUG-5 fix 后真镜像会含 revoke 块（释放/复用块的事务写出）；REPLAY 趟据 `is_revoked_as_of`
/// 按序列号规则过滤（BUG-6）。
fn scan_revoke_blocks(
    ctx: &RecoverCtx<'_>,
    sb: &RawJournalSuperblock,
    space: &JournalSpace,
    start: u32,
    end: u32,
) -> Result<RevokeTable> {
    let mut revoked = RevokeTable::new();
    // start==end 即空表。
    if start == end {
        return Ok(revoked);
    }

    // entry_size 由 64BIT 特性决定（8/4）；parse_revoke_block_into_table 取 is_64bit。
    let is_64bit = sb.has_incompat_feature(JBD2_FEATURE_INCOMPAT_64BIT);

    let mut cursor = start;
    // max_walk = distance(start, end).max(1)。
    let max_walk = space.distance(start, end).max(1);
    let mut walked = 0u32;
    while walked < max_walk {
        let raw = read_journal_block(ctx, cursor)?;
        // 块头 magic + blocktype==REVOKE 门控后解析（携带块头 sequence 入表）。
        if let Some(header) = read_header(&raw) {
            if header.blocktype() == JBD2_REVOKE_BLOCK {
                parse_revoke_block_into_table(&raw, is_64bit, &mut revoked);
            }
        }
        cursor = space.advance(cursor, 1);
        walked = walked.saturating_add(1);
        // 回到 end 即停。
        if cursor == end {
            break;
        }
    }

    Ok(revoked)
}

#[cfg(ktest)]
mod test {
    use core::cell::RefCell;

    use ostd::prelude::*;

    use super::super::commit::{write_commit_plan, CommitCtx};
    use super::super::format::{RawJournalSuperblock, JBD2_MAGIC, JBD2_SUPERBLOCK_V2};
    use super::super::space::JournalSpace;
    use super::super::transaction::{JournalCommitBlock, JournalCommitPlan, JournalRuntime};
    use super::{recover, RecoverCtx};
    use crate::fs::ext4::core::io::{BlockReader, BlockWriter};
    use crate::fs::ext4::core::metadata_writer::MetadataWriter;
    use crate::fs::ext4::core::types::Ext4Fsblk;
    // 全量带进 Pod / Vec / BTreeSet 等；`Result` 单独显式导入消歧（避免 `ostd::prelude::Result`
    // 与 `crate::prelude::Result` 二义——`MetadataWriter` trait 要求 crate 的 error::Error Result）。
    use crate::prelude::*;
    use crate::prelude::Result;

    const BS: usize = 4096;
    /// 盘总块数：journal 区 [0, JMAX) + home 区（home 块号 >= 64，远离 journal 区）。
    const JMAX: u32 = 32;
    const DISK_BLOCKS: usize = 256;

    /// 内存盘：journal 写（commit 的 MetadataWriter）/ home 写（recovery 的 BlockWriter）/ 读
    /// 都打到同一份字节。home 块用高块号（>= 64）避免落进 journal 区。
    struct MemDisk {
        bytes: RefCell<Vec<u8>>,
    }
    impl MemDisk {
        fn new() -> Self {
            Self {
                bytes: RefCell::new(vec![0u8; DISK_BLOCKS * BS]),
            }
        }
        fn block_first_byte(&self, block_nr: usize) -> u8 {
            self.bytes.borrow()[block_nr * BS]
        }
        fn set_block(&self, block_nr: usize, fill: u8) {
            let mut b = self.bytes.borrow_mut();
            for slot in &mut b[block_nr * BS..(block_nr + 1) * BS] {
                *slot = fill;
            }
        }
    }
    impl BlockReader for MemDisk {
        fn read_at(&self, off: usize, out: &mut [u8]) {
            let b = self.bytes.borrow();
            for (i, slot) in out.iter_mut().enumerate() {
                *slot = b.get(off + i).copied().unwrap_or(0);
            }
        }
    }
    impl BlockWriter for MemDisk {
        fn write_at(&self, off: usize, data: &[u8]) {
            let mut b = self.bytes.borrow_mut();
            for (i, byte) in data.iter().enumerate() {
                if let Some(slot) = b.get_mut(off + i) {
                    *slot = *byte;
                }
            }
        }
    }
    impl MetadataWriter for MemDisk {
        fn write_metadata_for_handle(
            &self,
            _handle_id: u64,
            block: Ext4Fsblk,
            data: &[u8],
        ) -> Result<()> {
            let mut b = self.bytes.borrow_mut();
            let off = block as usize * BS;
            b[off..off + data.len()].copy_from_slice(data);
            Ok(())
        }
    }

    fn synth_journal_sb(first_sequence: u32) -> RawJournalSuperblock {
        let mut raw = [0u8; super::JBD2_SUPERBLOCK_SIZE];
        raw[0..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
        raw[4..8].copy_from_slice(&JBD2_SUPERBLOCK_V2.to_be_bytes());
        raw[8..12].copy_from_slice(&first_sequence.to_be_bytes());
        raw[12..16].copy_from_slice(&(BS as u32).to_be_bytes()); // s_blocksize
        raw[16..20].copy_from_slice(&JMAX.to_be_bytes()); // s_maxlen
        raw[20..24].copy_from_slice(&1u32.to_be_bytes()); // s_first
        raw[24..28].copy_from_slice(&first_sequence.to_be_bytes()); // s_sequence
        raw[28..32].copy_from_slice(&0u32.to_be_bytes()); // s_start = 0
        RawJournalSuperblock::from_bytes(&raw)
    }

    fn identity_physical() -> Vec<Ext4Fsblk> {
        (0..JMAX as u64).collect()
    }

    /// 在 journal 里 commit 一个事务（驱动真实 commit emitter，含 revoke 块）。
    fn commit_tx(
        disk: &MemDisk,
        physical: &[Ext4Fsblk],
        sb: &mut RawJournalSuperblock,
        space: &mut JournalSpace,
        tid: u32,
        metadata: Vec<(Ext4Fsblk, u8)>,
        revoked: Vec<Ext4Fsblk>,
    ) {
        let ctx = CommitCtx {
            physical_blocks: physical,
            writer: disk,
            barrier: None,
            handle_id: 0,
            block_size: BS,
        };
        let plan = JournalCommitPlan {
            tid,
            metadata_blocks: metadata
                .into_iter()
                .map(|(block_nr, fill)| JournalCommitBlock {
                    block_nr,
                    block_data: vec![fill; BS],
                })
                .collect(),
            revoked_blocks: revoked,
        };
        write_commit_plan(&ctx, space, sb, &plan).unwrap();
    }

    /// ★ BUG-5/6 crux：tx N 把 home 块 B 作为 metadata journal；tx N+1 revoke B（B 已被复用为
    /// 数据）。recovery 的 REPLAY 趟必须**跳过** B 的 tx-N 陈旧镜像——不能覆盖已复用的数据块。
    #[ktest]
    fn recovery_skips_replay_of_revoked_block() {
        let disk = MemDisk::new();
        let physical = identity_physical();
        let mut sb = synth_journal_sb(10);
        let mut space = JournalSpace::from_superblock(&sb).unwrap();

        const B: Ext4Fsblk = 100; // home 块 B（>= 64，在 journal 区外）
        // home 块 B 的「当前数据」= 0x77（复用为数据后的真实内容）。
        disk.set_block(B as usize, 0x77);

        // tx 10：把 B 作为 metadata 写（陈旧目录/extent 镜像 = 0x11）。
        commit_tx(&disk, &physical, &mut sb, &mut space, 10, vec![(B, 0x11)], vec![]);
        // tx 11：revoke B（B 被释放并复用为数据）。
        commit_tx(&disk, &physical, &mut sb, &mut space, 11, vec![(150, 0x22)], vec![B]);

        // 复用为数据后，home B 的真实内容（崩溃前已直写 home）。
        disk.set_block(B as usize, 0x77);

        // 崩溃 + remount：s_start != 0（未 checkpoint），跑 recovery。
        let ctx = RecoverCtx {
            physical_blocks: &physical,
            reader: &disk,
            writer: &disk,
            block_size: BS,
        };
        let result = recover(&ctx, &mut sb).unwrap();
        assert_eq!(result.transactions_replayed, 2, "both committed txs scanned");
        assert!(result.revoked_blocks >= 1, "revoke table built from revoke block");

        // ★ B 必须仍是复用后的数据 0x77——tx-10 的陈旧镜像 0x11 被 revoke 跳过，未 replay。
        assert_eq!(
            disk.block_first_byte(B as usize),
            0x77,
            "revoked block must NOT be overwritten by its stale tx-10 metadata image"
        );
        // 非 revoke 的块 150 正常 replay（= 0x22）。
        assert_eq!(disk.block_first_byte(150), 0x22, "non-revoked block replayed normally");
    }

    /// BUG-6：一个块在更早事务被 revoke、之后又被**更新**事务重新 journal → 必须 replay 新镜像
    /// （`is_revoked_as_of` 的 seq 规则：撤销它的 seq < 重放它的 tx seq → 不跳过）。
    #[ktest]
    fn recovery_replays_block_rejournaled_after_revoke() {
        let disk = MemDisk::new();
        let physical = identity_physical();
        let mut sb = synth_journal_sb(20);
        let mut space = JournalSpace::from_superblock(&sb).unwrap();

        const B: Ext4Fsblk = 120;
        // tx 20：revoke B（B 此时被释放）。
        commit_tx(&disk, &physical, &mut sb, &mut space, 20, vec![(150, 0x01)], vec![B]);
        // tx 21：B 又被分配为 metadata 并 journal（新镜像 0x33，seq 21 > revoke 的 20）。
        commit_tx(&disk, &physical, &mut sb, &mut space, 21, vec![(B, 0x33)], vec![]);

        disk.set_block(B as usize, 0x00); // home 初值

        let ctx = RecoverCtx {
            physical_blocks: &physical,
            reader: &disk,
            writer: &disk,
            block_size: BS,
        };
        recover(&ctx, &mut sb).unwrap();

        // B 的新镜像（tx 21，seq > revoke seq）必须 replay → 0x33（不被 revoke 误跳过）。
        assert_eq!(
            disk.block_first_byte(B as usize),
            0x33,
            "block re-journaled in a later tx must be replayed (revoke seq < replay tx seq)"
        );
    }

    /// BUG-6：sequence 上界——一个序号跳变（非期望下一个 commit ID）的事务即使有合法 commit 块
    /// 也不被 replay（SCAN 在序号不连续处停）。这里把 tx 30 后**手动**写一个 seq=99 的有效事务，
    /// 断言它不被 replay。
    #[ktest]
    fn recovery_sequence_upper_bound_stops_at_non_consecutive() {
        let disk = MemDisk::new();
        let physical = identity_physical();
        let mut sb = synth_journal_sb(30);
        let mut space = JournalSpace::from_superblock(&sb).unwrap();

        const B1: Ext4Fsblk = 130;
        const B2: Ext4Fsblk = 131;
        // tx 30：合法连续（期望首序号 == s_sequence == 30）。
        commit_tx(&disk, &physical, &mut sb, &mut space, 30, vec![(B1, 0xAA)], vec![]);
        // tx 99：序号跳变（应被 sequence 上界挡住，不 replay）。
        commit_tx(&disk, &physical, &mut sb, &mut space, 99, vec![(B2, 0xBB)], vec![]);

        disk.set_block(B1 as usize, 0x00);
        disk.set_block(B2 as usize, 0x00);

        let ctx = RecoverCtx {
            physical_blocks: &physical,
            reader: &disk,
            writer: &disk,
            block_size: BS,
        };
        let result = recover(&ctx, &mut sb).unwrap();

        // 只 replay tx 30（seq==30==期望）；tx 99（seq 跳变）被上界挡住。
        assert_eq!(result.transactions_replayed, 1, "only the consecutive-sequence tx replayed");
        assert_eq!(result.last_sequence, Some(30));
        assert_eq!(disk.block_first_byte(B1 as usize), 0xAA, "consecutive tx replayed");
        assert_eq!(
            disk.block_first_byte(B2 as usize),
            0x00,
            "sequence-jumped tx NOT replayed (BUG-6 upper bound)"
        );
    }

    /// Drive a commit plan out of the in-memory [`JournalRuntime`] (one handle per tx), so the test
    /// exercises the **real** record_metadata_write / record_revoke data flow, not a hand-built plan.
    fn commit_runtime_tx(
        disk: &MemDisk,
        physical: &[Ext4Fsblk],
        sb: &mut RawJournalSuperblock,
        space: &mut JournalSpace,
        rt: &mut JournalRuntime,
        metadata: &[(Ext4Fsblk, u8)],
        revoked: &[Ext4Fsblk],
    ) {
        let h = rt.start_handle(1).expect("start handle");
        for (block, fill) in metadata {
            rt.record_metadata_write(h, *block, &alloc::vec![*fill; BS]);
        }
        // Free-path trigger: the integration's `record_journaled_metadata_freed` lands here.
        for block in revoked {
            rt.record_revoke(*block);
        }
        rt.stop_handle(h);
        let plan = rt.prepare_commit().expect("prepare commit");
        let ctx = CommitCtx {
            physical_blocks: physical,
            writer: disk,
            barrier: None,
            handle_id: 0,
            block_size: BS,
        };
        write_commit_plan(&ctx, space, sb, &plan).unwrap();
        rt.finish_commit(plan.tid);
    }

    /// ★ BUG-5 free-path trigger end-to-end through the runtime: a metadata block B is journaled in
    /// tx N; in tx N+1 the integration FREES B and records a revoke via `JournalRuntime::record_revoke`
    /// (the seam the new `MetadataWriter::record_journaled_metadata_freed` drives). On crash + recovery
    /// the REPLAY pass must SKIP B's stale tx-N image (B has been freed/reused), and a later re-journal
    /// of B (tx N+2, seq > revoke seq) must replay normally.
    #[ktest]
    fn recovery_free_path_revoke_skips_then_rejournal_replays() {
        let disk = MemDisk::new();
        let physical = identity_physical();
        let mut sb = synth_journal_sb(1);
        let mut space = JournalSpace::from_superblock(&sb).unwrap();
        // Runtime first_tid must match the journal SB's first sequence so plan.tid == descriptor seq.
        let mut rt = JournalRuntime::new(BS, 1);

        const B: Ext4Fsblk = 110; // home block B (>= 64, outside journal area)

        // tx 1: B journaled as metadata (stale tree/dir image 0x11).
        commit_runtime_tx(&disk, &physical, &mut sb, &mut space, &mut rt, &[(B, 0x11)], &[]);
        // tx 2: B is FREED → record_revoke(B). (Also touch an unrelated block so the tx is non-empty.)
        commit_runtime_tx(&disk, &physical, &mut sb, &mut space, &mut rt, &[(160, 0x22)], &[B]);

        // After the free, B's home holds the reused content (e.g. freshly written file data 0x77).
        disk.set_block(B as usize, 0x77);

        let ctx = RecoverCtx {
            physical_blocks: &physical,
            reader: &disk,
            writer: &disk,
            block_size: BS,
        };
        let result = recover(&ctx, &mut sb).unwrap();
        assert_eq!(result.transactions_replayed, 2, "both runtime-committed txs scanned");
        assert!(result.revoked_blocks >= 1, "revoke persisted from the free-path record_revoke");
        // ★ B keeps the reused content — its tx-1 stale image was revoked and skipped on replay.
        assert_eq!(
            disk.block_first_byte(B as usize),
            0x77,
            "freed+reused block must NOT be overwritten by its stale tx-1 metadata image"
        );

        // Now re-journal B as fresh metadata in a later tx (seq 3 > revoke seq 2) and crash again.
        // Reset s_start so recovery runs over the new window only.
        let mut sb2 = sb;
        let mut space2 = JournalSpace::from_superblock(&sb2).unwrap();
        commit_runtime_tx(&disk, &physical, &mut sb2, &mut space2, &mut rt, &[(B, 0x33)], &[]);
        disk.set_block(B as usize, 0x00);
        let ctx2 = RecoverCtx {
            physical_blocks: &physical,
            reader: &disk,
            writer: &disk,
            block_size: BS,
        };
        recover(&ctx2, &mut sb2).unwrap();
        // B re-journaled in a later tx (seq > the revoke's seq) must be replayed (cancel/seq rule).
        assert_eq!(
            disk.block_first_byte(B as usize),
            0x33,
            "block re-journaled after the revoke (higher seq) must be replayed"
        );
    }

    /// ★ stale-`s_head` (same lost-metadata class as the dropped `s_sequence` bound): two consecutive
    /// committed txs, but the on-disk `s_head` is rewound by one (points at tx2's start, as if tx2's
    /// commit block is durable but the unordered post-barrier SB store that would advance `s_head` was
    /// lost in a crash). Recovery MUST still scan + replay BOTH txs — the terminator is
    /// strict-consecutive + commit-validity, NOT the stored head pointer.
    #[ktest]
    fn recovery_replays_past_stale_head() {
        let disk = MemDisk::new();
        let physical = identity_physical();
        let mut sb = synth_journal_sb(40);
        let mut space = JournalSpace::from_superblock(&sb).unwrap();

        const B1: Ext4Fsblk = 140;
        const B2: Ext4Fsblk = 141;
        // tx 40 (consecutive baseline).
        commit_tx(&disk, &physical, &mut sb, &mut space, 40, vec![(B1, 0xAA)], vec![]);
        // Capture tx2's ring start (= current head) BEFORE committing tx2 — this is the stale head.
        let stale_head = space.head();
        // tx 41 (genuinely committed: descriptor + payload + commit all durable).
        commit_tx(&disk, &physical, &mut sb, &mut space, 41, vec![(B2, 0xBB)], vec![]);

        // Simulate the lost SB store: rewind s_head by one tx (to tx2's start). tx2 IS on disk.
        sb.set_head(stale_head);

        disk.set_block(B1 as usize, 0x00);
        disk.set_block(B2 as usize, 0x00);

        let ctx = RecoverCtx {
            physical_blocks: &physical,
            reader: &disk,
            writer: &disk,
            block_size: BS,
        };
        let result = recover(&ctx, &mut sb).unwrap();

        // BOTH txs must replay despite the stale head (old `cursor==head` break would stop after tx1).
        assert_eq!(
            result.transactions_replayed, 2,
            "stale s_head must NOT drop the genuinely-committed last tx"
        );
        assert_eq!(result.last_sequence, Some(41));
        assert_eq!(disk.block_first_byte(B1 as usize), 0xAA, "tx40 replayed");
        assert_eq!(
            disk.block_first_byte(B2 as usize),
            0xBB,
            "tx41 (past stale head) replayed — no lost metadata"
        );
    }

    /// Counterpart to the stale-head test: a genuinely-torn tx2 (commit block clobbered so it is
    /// missing/invalid) MUST stop the SCAN after tx1 — the commit-validity terminator still works
    /// once the head hard-break is gone. Guards against turning the under-replay fix into over-replay.
    #[ktest]
    fn recovery_stops_at_torn_tx_after_valid_one() {
        let disk = MemDisk::new();
        let physical = identity_physical();
        let mut sb = synth_journal_sb(50);
        let mut space = JournalSpace::from_superblock(&sb).unwrap();

        const B1: Ext4Fsblk = 150;
        const B2: Ext4Fsblk = 151;
        // tx 50 (valid, consecutive baseline). Layout: descriptor@start, payload@+1, commit@+2.
        commit_tx(&disk, &physical, &mut sb, &mut space, 50, vec![(B1, 0xAA)], vec![]);
        // tx 51 written, but tear it: clobber its commit block so the SCAN sees no valid commit.
        let tx2_start = space.head();
        commit_tx(&disk, &physical, &mut sb, &mut space, 51, vec![(B2, 0xBB)], vec![]);
        // tx2 = [descriptor@tx2_start, payload@tx2_start+1, commit@tx2_start+2]; clobber the commit.
        let tx2_commit_logical = (tx2_start + 2) as usize;
        disk.set_block(tx2_commit_logical, 0x00); // zero the commit block -> invalid magic.

        disk.set_block(B1 as usize, 0x00);
        disk.set_block(B2 as usize, 0x00);

        let ctx = RecoverCtx {
            physical_blocks: &physical,
            reader: &disk,
            writer: &disk,
            block_size: BS,
        };
        let result = recover(&ctx, &mut sb).unwrap();

        // Only tx50 replays — tx51 has no valid commit block, so the SCAN stops there.
        assert_eq!(
            result.transactions_replayed, 1,
            "torn tx (missing commit) must stop the scan after the last valid tx"
        );
        assert_eq!(result.last_sequence, Some(50));
        assert_eq!(disk.block_first_byte(B1 as usize), 0xAA, "valid tx50 replayed");
        assert_eq!(
            disk.block_first_byte(B2 as usize),
            0x00,
            "torn tx51 NOT replayed (no over-replay)"
        );
    }
}
