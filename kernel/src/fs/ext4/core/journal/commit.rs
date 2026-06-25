// SPDX-License-Identifier: MPL-2.0
//! JBD2 commit 落盘（on-disk emitter）。**全大端。** ★RED-LINE★
//!
//! 消费 [`JournalCommitPlan`]（Task 2，纯内存全块镜像）→ 把一个事务写进 journal 环：
//! `[descriptor][payload...]` →（**唯一** `sync()` 屏障）→ `[commit]` → 更新 journal SB → ring advance。
//!
//! 逐字节复刻 ext4_rs `ext4_impls/jbd2/mod.rs` 的 `write_commit_plan_with_hook`（144-224）+
//! `build_descriptor_block`（328-412）/ `build_commit_block`（414-429）/ `escape_journal_data`
//! （469-477）/ `tag_length`（459-467）；4 类 csum 复用 Task 1 的 [`super::superblock`] 套件。
//!
//! **RED LINE（commit 写序，逐字复刻 mod.rs:179-215）：**
//! 1. 建 descriptor 块（header type=1 + 8/12/16B tag 串 + 可选 tail csum）；
//! 2. payload escape（首 4 字节 == JBD2 magic → 写盘那 4 字节零填 + tag 置 ESCAPE；csum 按**原始**数据）；
//! 3. 写 descriptor + 全部 payload（异步/缓冲）；
//! 4. **`sync()`——唯一屏障**（差分里 MemDisk no-op，但写序位置逐字复刻，供 P5/真盘）；
//! 5. 写 commit 块（header type=2 + 可选 commit csum），落在整零块的偏移 0；
//! 6. 更新 journal SB（空则 `s_start=descriptor_block`；`s_head=next_head`；`s_sequence=seq+1` 饱和；
//!    重算 SB csum；store 到 journal 逻辑块 0）；**commit/SB 后无第二屏障**；
//! 7. ring advance（空则 `set_tail(descriptor_block)`；`advance_head(N+2)`）。

use super::super::metadata_writer::MetadataWriter;
use super::super::prelude::*;
use super::format::{
    RawCommitBlock, RawJournalBlockTag, RawJournalBlockTag3, RawJournalBlockTail, RawJournalHeader,
    RawJournalSuperblock, JBD2_DESCRIPTOR_BLOCK, JBD2_FEATURE_INCOMPAT_64BIT, JBD2_FEATURE_INCOMPAT_CSUM_V3,
    JBD2_FLAG_ESCAPE, JBD2_FLAG_LAST_TAG, JBD2_FLAG_SAME_UUID, JBD2_MAGIC, JBD2_SUPERBLOCK_SIZE,
};
use super::space::JournalSpace;
use super::superblock::{commit_block_csum, descriptor_tail_csum, journal_sb_checksum, tag_data_csum};
use super::transaction::JournalCommitPlan;

/// commit emitter 的盘上下文：journal 区的物理块映射 + 元数据写回接缝 + 屏障源 + handle/几何。
///
/// **journal 区寻址**：journal 区是 journal inode 的数据块，ext4_rs 经 `JournalDevice` 把
/// journal **逻辑块**号映射到 fs **物理块**号再写（device.rs:146-166）。core 差分里 `MetadataWriter`
/// 按物理块号写（`DirectMetadataWriter` 折成 `block*block_size` 字节偏移），故本 ctx 用
/// `physical_blocks[logical] = 物理块` 做同一映射——两侧打到同一组物理块（见 harness `resolve_journal_area`）。
pub(in crate::fs::ext4) struct CommitCtx<'a> {
    /// journal inode 的物理块向量：`physical_blocks[i]` = journal 逻辑块 i 的 fs 物理块号。
    pub physical_blocks: &'a [Ext4Fsblk],
    /// 元数据写回接缝（差分里 `DirectMetadataWriter` 直写同一份字节）。
    pub writer: &'a dyn MetadataWriter,
    /// 屏障源（差分里 `MemDisk` 的 `sync` 是 no-op，但写序位置逐字复刻）。`None` 视为 no-op。
    pub barrier: Option<&'a dyn JournalBarrier>,
    /// 写元数据用的 handle_id（差分里 `DirectMetadataWriter` 忽略它）。
    pub handle_id: u64,
    /// journal 块大小（== fs block_size）。
    pub block_size: usize,
}

/// 单屏障源抽象：commit 写序里 descriptor+payload 持久后、写 commit 块前调用一次。
/// PARITY: ext4_rs `block_device.sync()`（mod.rs:200）——ordered-mode 唯一屏障。
/// 差分里 `MemDisk` 直写、sync no-op，但**写序位置**仍逐字复刻（供 P5/真盘）。
pub(in crate::fs::ext4) trait JournalBarrier {
    fn sync(&self) -> Result<()>;
}

/// commit 写序的 4 个 hook 点（崩溃注入差分用）。一一对应 ext4_rs `JournalCommitWriteStage`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::fs::ext4) enum JournalCommitWriteStage {
    /// descriptor/payload 尚未写。
    BeforeDescriptor,
    /// descriptor/payload 已写、commit 块尚未写（屏障已过）。
    BeforeCommitBlock,
    /// commit 块已写、SB 尚未更新。
    AfterCommitBlock,
    /// SB 已更新（事务完全持久）。
    AfterSuperblock,
}

/// escape 一个 payload：首 4 字节 == 大端 JBD2 magic 时把**写盘镜像**那 4 字节零填，置 escape 标志。
/// csum 仍按**原始**数据（escape 之前），故返回 `(写盘镜像, escaped)`，原始数据由调用方另持有。
/// PARITY: ext4_rs `escape_journal_data`（mod.rs:469-477）。
struct EscapedPayload {
    /// 写盘镜像（escape 后；若未 escape 即原始数据的拷贝）。
    data: Vec<u8>,
    escaped: bool,
}

fn escape_journal_data(data: &[u8]) -> EscapedPayload {
    let mut journal_data = data.to_vec();
    // PARITY: ext4_rs mod.rs:471 —— magic 的大端 4 字节。
    let magic_bytes = JBD2_MAGIC.to_be_bytes();
    let escaped = journal_data.len() >= magic_bytes.len()
        && journal_data[..magic_bytes.len()] == magic_bytes;
    if escaped {
        // PARITY: ext4_rs mod.rs:474 —— 写盘那 4 字节置零（csum 仍按原始数据）。
        journal_data[..magic_bytes.len()].fill(0);
    }
    EscapedPayload {
        data: journal_data,
        escaped,
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

/// 把 journal **逻辑块**号映射到 fs **物理块**号（ctx.physical_blocks 索引）。
/// PARITY: ext4_rs `JournalDevice::logical_to_physical`（device.rs:69-74）。
fn logical_to_physical(ctx: &CommitCtx<'_>, logical_block: u32) -> Result<Ext4Fsblk> {
    ctx.physical_blocks
        .get(logical_block as usize)
        .copied()
        .ok_or_else(|| Error::with_message(Errno::EINVAL, "journal logical block out of range"))
}

/// 写一个 journal 逻辑块（整块镜像，长度 == block_size）到其物理块。经 `MetadataWriter`。
/// PARITY: ext4_rs `JournalDevice::write_block`（device.rs:146-166）——写整块（不足补零由
/// 调用方传整块保证；core 这里要求 `block.len() == block_size`）。
fn write_journal_block(ctx: &CommitCtx<'_>, logical_block: u32, block: &[u8]) -> Result<()> {
    if block.len() != ctx.block_size {
        return Err(Error::with_message(
            Errno::EINVAL,
            "journal block write must be a full block",
        ));
    }
    let physical = logical_to_physical(ctx, logical_block)?;
    ctx.writer
        .write_metadata_for_handle(ctx.handle_id, physical, block)
}

/// 一个 descriptor tag 的来源：目标块号 + escape 后写盘镜像 + **原始**数据（算 csum 用）+ 是否 escape。
struct DescriptorEntry {
    target_fs_block: Ext4Fsblk,
    /// escape 后的写盘 payload（首 4 字节可能已零填）。
    data: Vec<u8>,
    /// 原始（未 escape）块数据——per-tag csum 按它算。
    checksum_data: Vec<u8>,
    escaped: bool,
}

/// 建 descriptor 块（整 block_size 字节）：header(type=1) + 紧排 tag 串 + 可选 tail csum。
/// PARITY: ext4_rs `build_descriptor_block`（mod.rs:328-412）。
fn build_descriptor_block(
    sb: &RawJournalSuperblock,
    sequence: u32,
    entries: &[DescriptorEntry],
    block_size: usize,
) -> Result<Vec<u8>> {
    if entries.is_empty() {
        return Err(Error::with_message(
            Errno::EINVAL,
            "descriptor block has no tags",
        ));
    }

    let mut descriptor = vec![0u8; block_size];
    // PARITY: mod.rs:339-340 —— header 写在块首。
    let header = RawJournalHeader::new(JBD2_DESCRIPTOR_BLOCK, sequence);
    descriptor[..size_of::<RawJournalHeader>()].copy_from_slice(header.as_bytes());

    let has_csum = sb.has_checksum_v2_or_v3();
    let tag_len = tag_length(sb);
    let tail_len = if has_csum {
        size_of::<RawJournalBlockTail>()
    } else {
        0
    };
    // PARITY: mod.rs:345-350 —— header + tag*N + tail 不得超块。
    let needed_len = size_of::<RawJournalHeader>()
        .saturating_add(tag_len.saturating_mul(entries.len()))
        .saturating_add(tail_len);
    if needed_len > block_size {
        return Err(Error::with_message(
            Errno::ENOSPC,
            "descriptor block does not have enough tag space",
        ));
    }

    let uuid = sb.uuid();
    let mut tag_offset = size_of::<RawJournalHeader>();
    for (index, entry) in entries.iter().enumerate() {
        // PARITY: mod.rs:354-360 —— SAME_UUID 恒置；末 tag 加 LAST_TAG；escape 加 ESCAPE。
        let mut flags = JBD2_FLAG_SAME_UUID;
        if index + 1 == entries.len() {
            flags |= JBD2_FLAG_LAST_TAG;
        }
        if entry.escaped {
            flags |= JBD2_FLAG_ESCAPE;
        }

        if sb.has_incompat_feature(JBD2_FEATURE_INCOMPAT_CSUM_V3) {
            // PARITY: mod.rs:362-375 —— v3：16B tag，t_checksum 用全 32 位（csum 按原始数据）。
            let checksum = if has_csum {
                tag_data_csum(&uuid, sequence, &entry.checksum_data)
            } else {
                0
            };
            let tag = RawJournalBlockTag3::new(entry.target_fs_block, checksum, flags);
            descriptor[tag_offset..tag_offset + size_of::<RawJournalBlockTag3>()]
                .copy_from_slice(tag.as_bytes());
        } else if sb.has_incompat_feature(JBD2_FEATURE_INCOMPAT_64BIT) {
            // 64BIT（非 v3）→ 12 字节 tag：t_blocknr(4) | t_checksum(2) | t_flags(2) | t_blocknr_high(4)。
            //
            // DIVERGENCE / B-段 bug：ext4_rs（mod.rs:382-390 + 459-466）从 8 字节 `JournalBlockTag`
            // 结构 `from_raw_parts(&tag, 12)` 读 4 字节越界（UB，读相邻栈/padding），且其 8 字节结构
            // **不含** t_blocknr_high 字段，无法产出正确 12 字节 tag。**不复刻该 UB**——这里写
            // 一个正确的 64 位 tag（低 4 字节用 8B tag、随后 4 字节填 t_blocknr_high 大端），与 Linux
            // `journal_block_tag_s` 布局一致。本路径**无测试镜像触发**（三镜像 journal feat_incompat=0），
            // 故不在差分路径上；记 bug.md（B 段：ext4_rs 64BIT-tag 越界 + 无 high 字段）。
            let checksum = if has_csum {
                (tag_data_csum(&uuid, sequence, &entry.checksum_data) & 0xFFFF) as u16
            } else {
                0
            };
            let low = RawJournalBlockTag::new(
                entry.target_fs_block as u32,
                checksum,
                (flags & 0xFFFF) as u16,
            );
            descriptor[tag_offset..tag_offset + size_of::<RawJournalBlockTag>()]
                .copy_from_slice(low.as_bytes());
            let high = ((entry.target_fs_block >> 32) as u32).to_be_bytes();
            descriptor[tag_offset + size_of::<RawJournalBlockTag>()..tag_offset + 12]
                .copy_from_slice(&high);
        } else {
            // PARITY: mod.rs:376-390（非 64BIT 分支）—— 8 字节 tag，t_checksum 取低 16 位。
            let checksum = if has_csum {
                (tag_data_csum(&uuid, sequence, &entry.checksum_data) & 0xFFFF) as u16
            } else {
                0
            };
            let tag = RawJournalBlockTag::new(
                entry.target_fs_block as u32,
                checksum,
                (flags & 0xFFFF) as u16,
            );
            descriptor[tag_offset..tag_offset + size_of::<RawJournalBlockTag>()]
                .copy_from_slice(tag.as_bytes());
        }
        tag_offset += tag_len;
    }

    if has_csum {
        // PARITY: mod.rs:395-408 —— tail csum 字段先零（块此时该区已是 0）、算 crc(UUID++整块)、写回块尾。
        let tail_offset = block_size - size_of::<RawJournalBlockTail>();
        let checksum = descriptor_tail_csum(&uuid, &descriptor);
        let tail = RawJournalBlockTail::new(checksum);
        descriptor[tail_offset..tail_offset + size_of::<RawJournalBlockTail>()]
            .copy_from_slice(tail.as_bytes());
    }

    Ok(descriptor)
}

/// 建 commit 块（整 block_size 字节，commit 结构落在偏移 0）。
/// PARITY: ext4_rs `build_commit_block`（mod.rs:414-429）。
fn build_commit_block(sb: &RawJournalSuperblock, sequence: u32, block_size: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; block_size];
    let mut commit = RawCommitBlock::new(sequence);
    if sb.has_checksum_v2_or_v3() {
        // PARITY: mod.rs:419-422 —— 先建 commit（h_chksum[0]=0）算 csum，再 with_checksum 写回。
        let checksum = commit_block_csum(&sb.uuid(), commit.as_bytes());
        commit = commit.with_checksum(checksum);
    }
    let commit_bytes = commit.as_bytes();
    bytes[..commit_bytes.len()].copy_from_slice(commit_bytes);
    bytes
}

/// 把 journal SB（1024 字节）store 回 journal **逻辑块 0**（= physical_blocks[0]）。
/// store **不重算 csum**（调用方先重算，与 ext4_rs 一致）；SB 占块 0 前 1024 字节，落整块需读改写——
/// 但 ext4_rs `write_block` 写不足 block_size 时**补零整块**（device.rs:162-164），故 SB store 实际
/// 把块 0 的 [0,1024) 写 SB、[1024,block_size) 写零。core 这里复刻：建整块（前 1024 = SB，余零）写整块。
/// PARITY: ext4_rs `JournalSuperblockState::store`（superblock.rs）→ `device.write_block(.., 0, &sb.to_bytes())`。
fn store_journal_sb(ctx: &CommitCtx<'_>, sb: &RawJournalSuperblock) -> Result<()> {
    let mut block = vec![0u8; ctx.block_size];
    block[..JBD2_SUPERBLOCK_SIZE].copy_from_slice(sb.as_bytes());
    write_journal_block(ctx, 0, &block)
}

/// 集成层 checkpoint 用：重算 journal SB csum 后把 SB store 回 journal 逻辑块 0（经独立 writer +
/// physical_blocks，不要求一个完整 `CommitCtx`）。PARITY: ext4_rs `JournalSuperblockState::store`
/// （checkpoint 路径 `update_start` 后 store）——store 前 `update_checksum`，再写整块 0（前 1024 = SB，
/// 余零）。P6 集成层 `CoreJournalDriver::checkpoint_transaction` 更新 `s_start` 后调它落盘。
pub(in crate::fs::ext4) fn store_journal_sb_via_writer(
    writer: &dyn MetadataWriter,
    handle_id: u64,
    physical_blocks: &[Ext4Fsblk],
    block_size: usize,
    sb: &mut RawJournalSuperblock,
) -> Result<()> {
    recompute_sb_checksum(sb);
    let physical = *physical_blocks.first().ok_or_else(|| {
        Error::with_message(Errno::EINVAL, "journal has no physical blocks")
    })?;
    let mut block = vec![0u8; block_size];
    block[..JBD2_SUPERBLOCK_SIZE].copy_from_slice(sb.as_bytes());
    writer.write_metadata_for_handle(handle_id, physical, &block)
}

/// 把一个事务的 commit plan 写进 journal 环并落盘，返回写入的 tid（= plan.tid）。
///
/// 见模块文档「RED LINE 写序」。`write_commit_plan_with_hook` 的 no-op hook 版。
pub(in crate::fs::ext4) fn write_commit_plan(
    ctx: &CommitCtx<'_>,
    space: &mut JournalSpace,
    sb: &mut RawJournalSuperblock,
    plan: &JournalCommitPlan,
) -> Result<u32> {
    write_commit_plan_with_hook(ctx, space, sb, plan, |_| {})
}

/// 同 [`write_commit_plan`]，但在写序 4 个 stage 触发 `hook`（崩溃注入差分 / harness 用）。
/// 返回写入的 tid。
///
/// PARITY: ext4_rs `write_commit_plan_with_hook`（mod.rs:144-224）逐字复刻：空间校验 → 算环位置 →
/// escape 收集 entries → 建 descriptor → hook(BeforeDescriptor) → 写 descriptor+payload →
/// **sync 屏障** → hook(BeforeCommitBlock) → 写 commit 块 → hook(AfterCommitBlock) → 更新+store SB →
/// hook(AfterSuperblock) → ring advance（空则 set_tail）。
pub(in crate::fs::ext4) fn write_commit_plan_with_hook(
    ctx: &CommitCtx<'_>,
    space: &mut JournalSpace,
    sb: &mut RawJournalSuperblock,
    plan: &JournalCommitPlan,
    mut hook: impl FnMut(JournalCommitWriteStage),
) -> Result<u32> {
    let metadata_blocks = plan.metadata_blocks.len() as u32;
    // PARITY: mod.rs:150-153 —— 空 plan 拒绝。
    if metadata_blocks == 0 {
        return Err(Error::with_message(
            Errno::EINVAL,
            "commit plan contains no metadata blocks",
        ));
    }

    // PARITY: mod.rs:155-160 —— 需 N+2 块（N payload + descriptor + commit）；不足即 ENOSPC。
    let required_blocks = metadata_blocks
        .checked_add(2)
        .ok_or_else(|| Error::with_message(Errno::EINVAL, "journal transaction too large"))?;
    if space.free_blocks() < required_blocks {
        return Err(Error::with_message(
            Errno::ENOSPC,
            "journal does not have enough free space",
        ));
    }

    // PARITY: mod.rs:162-177 —— 环位置：descriptor=head；每 payload advance 1；commit=最后一块后；
    // next_head = commit 后一块。entries 收集 escape 结果（写盘镜像 + 原始 csum 数据）。
    let sequence = plan.tid;
    let descriptor_block = space.head();
    let mut cursor = descriptor_block;
    let mut entries: Vec<DescriptorEntry> = Vec::with_capacity(plan.metadata_blocks.len());
    for metadata in &plan.metadata_blocks {
        cursor = space.advance(cursor, 1);
        let escaped = escape_journal_data(&metadata.block_data);
        entries.push(DescriptorEntry {
            target_fs_block: metadata.block_nr,
            data: escaped.data,
            // PARITY: mod.rs:172 —— csum 按**原始**块数据（escape 前）。
            checksum_data: metadata.block_data.clone(),
            escaped: escaped.escaped,
        });
    }
    let commit_block = space.advance(cursor, 1);
    let next_head = space.advance(commit_block, 1);

    // PARITY: mod.rs:179 —— 建 descriptor 块（含 tags + 可选 tail csum）。
    let descriptor = build_descriptor_block(sb, sequence, &entries, ctx.block_size)?;
    hook(JournalCommitWriteStage::BeforeDescriptor);

    // PARITY: mod.rs:185-194 —— 写 descriptor + 全部 payload（异步/缓冲；coalesced 在差分里等价逐块写，
    // 字节结果一致）。逻辑块：descriptor=descriptor_block，payload 从 descriptor_block+1 顺序排。
    write_journal_block(ctx, descriptor_block, &descriptor)?;
    let mut data_block = space.advance(descriptor_block, 1);
    for entry in &entries {
        write_journal_block(ctx, data_block, &entry.data)?;
        data_block = space.advance(data_block, 1);
    }

    // PARITY: mod.rs:196-201 —— 建 commit 块；hook(BeforeCommitBlock)；
    // **唯一屏障**：descriptor+payload 必须先持久，再写 commit 块让事务可 replay。
    let commit = build_commit_block(sb, sequence, ctx.block_size);
    hook(JournalCommitWriteStage::BeforeCommitBlock);
    // PARITY: single ordered-mode barrier（mod.rs:200 `block_device.sync()`）。差分里 no-op。
    if let Some(barrier) = ctx.barrier {
        barrier.sync()?;
    }
    write_journal_block(ctx, commit_block, &commit)?;
    hook(JournalCommitWriteStage::AfterCommitBlock);

    // PARITY: mod.rs:204-211 —— 更新 journal SB（空则置 s_start；s_head=next；s_sequence=seq+1 饱和；
    // 重算 SB csum；store 块 0）。**commit/SB 后无第二屏障。**
    let journal_was_empty = sb.start() == 0;
    if journal_was_empty {
        sb.set_start(descriptor_block);
    }
    sb.set_head(next_head);
    sb.set_sequence(sequence.saturating_add(1));
    // 重算 SB csum 再 store（csum 关时 set_checksum(0) 与盘上 0 一致；ext4_rs update_checksum 同此）。
    recompute_sb_checksum(sb);
    store_journal_sb(ctx, sb)?;
    hook(JournalCommitWriteStage::AfterSuperblock);

    // PARITY: mod.rs:212-215 —— ring advance：空则 set_tail(descriptor_block)；advance_head(N+2)。
    if journal_was_empty {
        space.set_tail(descriptor_block)?;
    }
    space.advance_head(required_blocks);

    // 返回写入的 tid（= plan.tid）。其余环位置（start/commit/next_head）已落进 SB + space，
    // 调用方需要时从那里取——commit emitter 对外只承诺 tid（brief Produces: `-> Result<u32>`）。
    let _ = (descriptor_block, commit_block, next_head, metadata_blocks);
    Ok(sequence)
}

/// 重算并写回 journal SB 校验和。PARITY: ext4_rs `update_checksum`（jbd2.rs:264-266）——
/// `compute_checksum`（[0xFC..0x100] 置零的 crc）→ `s_checksum = csum.to_be()`。
/// csum 关时 ext4_rs 同样无条件 `update_checksum`（store 前 `superblock.rs` 调用）——故 core 也无条件
/// 重算：csum 关镜像里盘上 s_checksum 本就是该 crc（mke2fs 也写它），逐字节一致。
fn recompute_sb_checksum(sb: &mut RawJournalSuperblock) {
    // 取 SB 的 1024 字节镜像算 csum（journal_sb_checksum 内部已置零 [0xFC..0x100]）。
    let mut image = [0u8; JBD2_SUPERBLOCK_SIZE];
    image.copy_from_slice(sb.as_bytes());
    let checksum = journal_sb_checksum(&image);
    sb.set_checksum(checksum);
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::escape_journal_data;
    use super::super::format::JBD2_MAGIC;
    use crate::prelude::*;

    /// escape 自检：首 4 字节 == 大端 JBD2 magic 的块被 escape（写盘那 4 字节零填），否则不动。
    #[ktest]
    fn escape_magic_zeroes_first_four() {
        // 命中：首 4 字节 = magic 的大端。
        let mut blk = vec![0xAAu8; 16];
        blk[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
        let e = escape_journal_data(&blk);
        assert!(e.escaped, "first 4 bytes == BE magic must escape");
        assert_eq!(&e.data[..4], &[0, 0, 0, 0], "escaped bytes zeroed in written image");
        assert_eq!(&e.data[4..], &blk[4..], "rest unchanged");

        // 未命中：首字节差一位即不 escape。
        let mut blk2 = blk.clone();
        blk2[0] ^= 0x01;
        let e2 = escape_journal_data(&blk2);
        assert!(!e2.escaped, "non-magic prefix must not escape");
        assert_eq!(e2.data, blk2, "non-escaped image is verbatim copy");
    }
}
