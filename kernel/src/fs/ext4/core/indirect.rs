// SPDX-License-Identifier: MPL-2.0
//! ext2/3 间接块映射接缝（report §5.9 接缝 #4）——**Phase 3 非目标**，仅签名 stub。
//!
//! ext4 在 `i_flags` 未置 `EXT4_INODE_FLAG_EXTENTS` 时回退到 ext2 风格的直接/一/二/三级间接
//! 块映射（对照 ext4_rs `Ext4::get_pblock_idx_legacy`，ext4_impls/inode.rs:50；以及 Linux
//! `fs/ext4/indirect.c`）。本项目预留 ext2 兼容接缝，但当前真镜像走 extent 映射，间接路径
//! 的安全重写排在后续 phase——本 Task 只留 `unimplemented!` 占位，保证派发接缝可编译且类型对齐。

use super::inode::Inode;
use super::prelude::*;

/// 逻辑块 `lblock` → 物理块号（ext2/3 间接映射）。**Phase 3 非目标**：`unimplemented!`。
///
/// [对照] ext4_rs `Ext4::get_pblock_idx_legacy`（ext4_impls/inode.rs:50）：直接块 [0,12)、
/// 一级间接 [12, 12+ppb)、二级、三级间接。后续 phase 复刻为安全实现。
#[allow(dead_code)]
pub(super) fn get_pblock_idx_legacy(_inode: &Inode, _lblock: Ext4Lblk) -> Result<Ext4Fsblk> {
    unimplemented!("ext2 indirect map seam — Phase 3 non-goal")
}
