// SPDX-License-Identifier: MPL-2.0

//! ext4 语义化块号别名（对齐 Linux `ext4_fsblk_t`/`ext4_lblk_t`）。

/// 物理块号（filesystem block）。ext4 最多 48 位，用 u64 承载。
pub type Ext4Fsblk = u64;
/// 文件内逻辑块号。ext4 为 32 位。
pub type Ext4Lblk = u32;
/// 通用块号别名，用于尚未区分物理/逻辑语义的场合（当前等同物理块号 `Ext4Fsblk`）。
/// 语义明确时优先用 `Ext4Fsblk`（物理）或 `Ext4Lblk`（逻辑）。
pub type Ext4Bid = Ext4Fsblk;
