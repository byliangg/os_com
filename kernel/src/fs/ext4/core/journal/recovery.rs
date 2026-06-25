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
use super::revoke::parse_revoke_block;
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

    // ===== 趟 2：REVOKE —— 扫 [start, end) 建扁平 revoke 集合 =====
    // PARITY: recovery.rs:48 —— self.scan_revoke_blocks(start, end)。BUG-5 下真镜像恒空。
    let revoked = scan_revoke_blocks(ctx, sb, &space, sb.start(), end)?;

    // ===== 趟 3：REPLAY —— 逐事务、逐块写回 home（跳过 revoked，escape 已在 SCAN 还原） =====
    // PARITY: recovery.rs:50-64。
    let mut metadata_blocks_replayed = 0u32;
    for transaction in &transactions {
        for metadata in &transaction.metadata_blocks {
            // PARITY: recovery.rs:53-55 —— 扁平包含（无 sequence 上界比较）；revoked 则跳过。
            if revoked.contains(&metadata.block_nr) {
                continue;
            }
            // PARITY: recovery.rs:56-61 —— home 偏移 = block_nr * block_size（溢出即 EINVAL）。
            let offset = (metadata.block_nr as usize)
                .checked_mul(ctx.block_size)
                .ok_or_else(|| {
                    Error::with_message(Errno::EINVAL, "recovery block offset overflow")
                })?;
            // PARITY: recovery.rs:61 —— block_device.write_offset(offset, &block_data)（raw home 写）。
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

/// SCAN：从 `s_start` 起逐事务读，直到第一个无效 / 缺失 commit 或回到 `head`。
/// PARITY: ext4_rs `scan_committed_transactions`（recovery.rs:93-126）逐字复刻。
fn scan_committed_transactions(
    ctx: &RecoverCtx<'_>,
    sb: &RawJournalSuperblock,
    space: &JournalSpace,
) -> Result<Vec<RecoveryTransaction>> {
    // PARITY: recovery.rs:97-100 —— start==0 早返（needs_recovery 已挡，这里防御）。
    let start = sb.start();
    if start == 0 {
        return Ok(Vec::new());
    }

    // PARITY: recovery.rs:102-106 —— head: s_head==0 时退回 start。
    let head = if sb.head() == 0 { start } else { sb.head() };
    let mut cursor = start;
    let mut walked_blocks = 0u32;
    let mut transactions = Vec::new();
    // PARITY: recovery.rs:110 —— max_walk = usable_blocks().max(1)（BUG-6：无 s_sequence 上界，
    // 仅靠环可用块数 + head 收口）。
    let max_walk = space.usable_blocks().max(1);

    // PARITY: recovery.rs:112-123 —— 逐事务读，advance/cursor/head 收口。
    while walked_blocks < max_walk {
        let Some(transaction) = try_read_committed_transaction(ctx, sb, space, cursor)? else {
            break;
        };
        // PARITY: recovery.rs:116 —— advanced = distance(cursor, next_head).max(1)。
        let advanced = space.distance(cursor, transaction.next_head).max(1);
        walked_blocks = walked_blocks.saturating_add(advanced);
        cursor = transaction.next_head;
        transactions.push(transaction);
        // PARITY: recovery.rs:120-122 —— 回到 head 即停。
        if cursor == head {
            break;
        }
    }

    Ok(transactions)
}

/// 尝试读一个落在 `descriptor_block` 的有效已 commit 事务。无效 / 缺失 commit 返回 `None`。
/// PARITY: ext4_rs `try_read_committed_transaction`（recovery.rs:128-174）逐字复刻。
///
/// **判定有效仅靠：descriptor magic + blocktype==DESCRIPTOR + 至少一个 tag；commit magic +
/// blocktype==COMMIT + commit.sequence == descriptor.sequence。NO csum 校验。**（坏 csum 不停。）
fn try_read_committed_transaction(
    ctx: &RecoverCtx<'_>,
    sb: &RawJournalSuperblock,
    space: &JournalSpace,
    descriptor_block: u32,
) -> Result<Option<RecoveryTransaction>> {
    // PARITY: recovery.rs:133-139 —— 读 descriptor 块头：magic 错 / 非 DESCRIPTOR → None。
    let descriptor_raw = read_journal_block(ctx, descriptor_block)?;
    let Some(header) = read_header(&descriptor_raw) else {
        return Ok(None);
    };
    if header.blocktype() != JBD2_DESCRIPTOR_BLOCK {
        return Ok(None);
    }

    // PARITY: recovery.rs:141-144 —— 解析 tag 串；空 / 无 LAST_TAG → None。
    let descriptor_tags = match parse_descriptor_tags(sb, &descriptor_raw, ctx.block_size) {
        Some(tags) if !tags.is_empty() => tags,
        _ => return Ok(None),
    };

    // PARITY: recovery.rs:146-158 —— 逐 tag 读 payload（escape 还原），cursor 顺序前进。
    let mut data_cursor = space.advance(descriptor_block, 1);
    let mut metadata_blocks = Vec::with_capacity(descriptor_tags.len());
    for tag in &descriptor_tags {
        let mut block_data = read_journal_block(ctx, data_cursor)?;
        // PARITY: recovery.rs:150-152 —— escape 还原：首 4 字节恢复成大端 JBD2 magic。
        if (tag.flags & JBD2_FLAG_ESCAPE) != 0 && block_data.len() >= size_of::<u32>() {
            block_data[..size_of::<u32>()].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
        }
        metadata_blocks.push(RecoveryBlock {
            block_nr: tag.target_fs_block,
            block_data,
        });
        data_cursor = space.advance(data_cursor, 1);
    }

    // PARITY: recovery.rs:160-166 —— 读 commit 块：magic 错 / 非 COMMIT / seq 不匹配 → None。
    // **这是唯一的「事务是否落盘」判据**——坏 commit-magic / type / seq 才停（坏 csum 不停）。
    let commit_raw = read_journal_block(ctx, data_cursor)?;
    let Some(commit_header) = read_header(&commit_raw) else {
        return Ok(None);
    };
    if commit_header.blocktype() != JBD2_COMMIT_BLOCK
        || commit_header.sequence() != header.sequence()
    {
        return Ok(None);
    }

    // PARITY: recovery.rs:168-173 —— next_head = commit 后一块。
    let next_head = space.advance(data_cursor, 1);
    Ok(Some(RecoveryTransaction {
        sequence: header.sequence(),
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

/// REVOKE 趟：扫 `[start, end)` 内所有 `JBD2_REVOKE_BLOCK`，解析成扁平 `BTreeSet<u64>`。
/// PARITY: ext4_rs `scan_revoke_blocks`（recovery.rs:176-205）逐字复刻——
/// `start==end` 早返空集；逐块读头，blocktype==REVOKE 则 `parse_revoke_block` 追加；
/// cursor 环内前进，回到 end 即停。**BUG-5 下真镜像无 revoke 块，恒解出空集。**
fn scan_revoke_blocks(
    ctx: &RecoverCtx<'_>,
    sb: &RawJournalSuperblock,
    space: &JournalSpace,
    start: u32,
    end: u32,
) -> Result<BTreeSet<Ext4Fsblk>> {
    let mut revoked = BTreeSet::new();
    // PARITY: recovery.rs:182-185 —— start==end 即空集。
    if start == end {
        return Ok(revoked);
    }

    // PARITY: recovery.rs:217 —— entry_size 由 64BIT 特性决定（8/4）；parse_revoke_block 取 is_64bit。
    let is_64bit = sb.has_incompat_feature(JBD2_FEATURE_INCOMPAT_64BIT);

    let mut cursor = start;
    // PARITY: recovery.rs:188 —— max_walk = distance(start, end).max(1)。
    let max_walk = space.distance(start, end).max(1);
    let mut walked = 0u32;
    while walked < max_walk {
        let raw = read_journal_block(ctx, cursor)?;
        // PARITY: recovery.rs:192-195 —— 块头 magic + blocktype==REVOKE 门控后解析。
        if let Some(header) = read_header(&raw) {
            if header.blocktype() == JBD2_REVOKE_BLOCK {
                // PARITY: recovery.rs:207-230 —— parse_revoke_entries（Task 4 `parse_revoke_block`）。
                parse_revoke_block(&raw, is_64bit, &mut revoked);
            }
        }
        cursor = space.advance(cursor, 1);
        walked = walked.saturating_add(1);
        // PARITY: recovery.rs:199-201 —— 回到 end 即停。
        if cursor == end {
            break;
        }
    }

    Ok(revoked)
}
