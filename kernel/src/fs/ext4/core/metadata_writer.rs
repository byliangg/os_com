// SPDX-License-Identifier: MPL-2.0
//! 核心 ↔ 日志之间的元数据写回调接缝（report §5.4）。
//!
//! 安全核心改元数据时不直接落盘，而是通过本 trait 回调集成层把全块镜像记入
//! 当前 JBD2 handle 的内存事务（home 写延迟）。核心只依赖这个抽象、不认识 journal；
//! 实现（JournalIoBridge / JournalOperationMetadataWriter）在集成层 journal_driver，
//! 于 rewrite Phase 5 接入。本阶段仅定义签名占位。

use super::prelude::*;

/// 把元数据块的全块镜像交给当前事务的回调接缝。
pub trait MetadataWriter {
    /// 将块 `block` 的全块镜像 `data` 记入 `handle_id` 对应的内存事务，延迟 home 写。
    fn write_metadata_for_handle(
        &self,
        handle_id: u64,
        block: Ext4Fsblk,
        data: &[u8],
    ) -> Result<()>;

    /// 记录一个**已被释放的 journaled 元数据块**到当前 JBD2 事务（BUG-5，Linux `ext4_forget` 模型）。
    ///
    /// 当一个曾作为 journaled 元数据（经 [`write_metadata_for_handle`](Self::write_metadata_for_handle)）
    /// 写过的块被释放、之后可能被复用（含复用为文件数据）时，core 在**执行释放的事务**里调本方法记一条
    /// revoke。集成层实现把它转成 `record_revoke`（→ commit 落 `JBD2_REVOKE_BLOCK`，sequence = 本事务），
    /// 并丢弃该块的内存 checkpoint 镜像（防 parked 事务把陈旧镜像写回 home）；recovery 的 REPLAY 趟据此
    /// 跳过任何**更早 sequence**对该块的陈旧 journaled 镜像。
    ///
    /// **只对真正 journaled 的元数据块调用**（extent 树 index/leaf 块；目录数据块）。普通文件数据块不经
    /// journal，**绝不**调本方法。固定位置元数据（位图/inode 表/组描述符/SB）永不复用为数据，也无需 revoke。
    ///
    /// 默认空实现：无 JBD2 接缝的实现（差分 harness / commit emitter 自身的 writer）可忽略。
    /// [对照] Linux `jbd2_journal_revoke` + `ext4_forget(is_metadata=1)`。
    fn record_journaled_metadata_freed(&self, _block: Ext4Fsblk) {}
}
