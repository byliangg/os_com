// SPDX-License-Identifier: MPL-2.0

//! ext4 语义化块号别名（对齐 Linux `ext4_fsblk_t`/`ext4_lblk_t`）。

/// 物理块号（filesystem block）。ext4 最多 48 位，用 u64 承载。
pub type Ext4Fsblk = u64;
/// 文件内逻辑块号。ext4 为 32 位。
pub type Ext4Lblk = u32;
/// 通用块号别名（默认物理块号语义）。
pub type Ext4Bid = Ext4Fsblk;
