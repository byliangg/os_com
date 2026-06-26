// SPDX-License-Identifier: MPL-2.0
//! ext2/3 间接块映射接缝（report §5.9 接缝 #4）——**Phase 3 非目标**，仅签名 stub。
//!
//! ext4 在 `i_flags` 未置 `EXT4_INODE_FLAG_EXTENTS` 时回退到 ext2 风格的直接/一/二/三级间接
//! 块映射（对照 ext4_rs `Ext4::get_pblock_idx_legacy`，ext4_impls/inode.rs:50；以及 Linux
//! `fs/ext4/indirect.c`）。本项目预留 ext2 兼容接缝，但当前真镜像走 extent 映射，间接路径
//! 的安全重写排在后续 phase。
//!
//! **健壮性红线（绝不 panic）**：内核**绝不能**在 umount / writeback / sync 路径上 panic。
//! 间接映射未实现时返回 `Err(EOPNOTSUPP)`（与 file.rs 写接缝 `EOPNOTSUPP` 一致），由上层
//! 优雅丢弃/报错脏页，而非 `unimplemented!`/`panic!`。

use super::inode::Inode;
use super::prelude::*;

/// 逻辑块 `lblock` → 物理块号（ext2/3 间接映射）。**Phase 3 非目标**：返回 `Err(EOPNOTSUPP)`。
///
/// [对照] ext4_rs `Ext4::get_pblock_idx_legacy`（ext4_impls/inode.rs:50）：直接块 [0,12)、
/// 一级间接 [12, 12+ppb)、二级、三级间接。后续 phase 复刻为安全实现。
///
/// **不 panic**：间接映射尚未实现，但内核在 umount/writeback/sync 路径上绝不能崩溃。返回
/// `EOPNOTSUPP`，由调用方（block_map 读派发、page-cache 写回）把对应操作优雅地以错误结束。
#[allow(dead_code)]
pub(in crate::fs::ext4) fn get_pblock_idx_legacy(_inode: &Inode, _lblock: Ext4Lblk) -> Result<Ext4Fsblk> {
    Err(Error::with_message(
        Errno::EOPNOTSUPP,
        "legacy (indirect) read mapping not supported in Phase 3",
    ))
}
