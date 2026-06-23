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
}
