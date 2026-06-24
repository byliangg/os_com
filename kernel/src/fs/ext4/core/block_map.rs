// SPDX-License-Identifier: MPL-2.0
//! 逻辑块 → 物理块的映射派发接缝（report §5.9 接缝 #2/#4）。
//!
//! ext4 有两种块映射：**extent 树**（现代默认）与 **ext2/3 间接映射**（legacy）。映射入口
//! 据 inode 的 extent 标志位派发——与 ext4_rs `get_pblock_idx_inner`（ext4_impls/inode.rs:255）
//! 的首段判定一致：
//!
//! ```text
//! if (inode.flags() & EXT4_INODE_FLAG_EXTENTS) != 0 { extent 分支 } else { 间接分支 }
//! ```
//!
//! **本 Task（Phase 3 Task 1）只落地派发谓词 + stub**：
//! - extent 分支 → `unimplemented!("extent block map — Phase 3 Task 2")`（extent 读半部是 Task 2，
//!   不在本 Task 调用 `extents.rs`，故本模块自包含、不依赖 Task 2 代码即可编译）；
//! - 间接分支 → [`super::indirect::get_pblock_idx_legacy`]（其本身 `unimplemented!`，ext2 间接映射
//!   为 Phase 3 非目标）。
//!
//! 派发**谓词**（`inode.uses_extents()`）与 ext4_rs `inode_uses_extents` 逐位一致，由 ktest
//! `dispatch_predicate_parity` 钉死。

use super::extents;
use super::indirect;
use super::inode::Inode;
use super::io::BlockReader;
use super::prelude::*;
use super::superblock::RawSuperblock;

/// 逻辑块 `lblock` → 物理块映射（读路径）。据 inode extent 标志派发：
/// - `uses_extents()` → extent 读半部（[`extents::get_pblock_idx_state`]，Phase 3 Task 2）；
/// - 否则 → 间接映射（[`indirect::get_pblock_idx_legacy`]，Phase 3 非目标，`unimplemented!`）。
///
/// 返回 `Some((pblock, is_unwritten))`：映射到的物理块号 + 是否 unwritten extent；
/// `None` 表示空洞（hole）。间接映射无 unwritten 状态（恒 `false`）。
///
/// extent 分支需经块设备读 extent 树，故注入 `reader` + `sb`（不持全局锁，只读路径
/// 共享 guard 安全）；间接分支当前不读盘。
///
/// [对照] ext4_rs `Ext4::get_pblock_idx_inner`（ext4_impls/inode.rs:255-261）的派发首段。
#[allow(dead_code)]
pub(super) fn map_block_for_read(
    reader: &dyn BlockReader,
    sb: &RawSuperblock,
    inode: &Inode,
    lblock: Ext4Lblk,
) -> Result<Option<(Ext4Fsblk, bool)>> {
    if inode.uses_extents() {
        // extent 读半部（Task 2）：返回 Some((pblock, unwritten)) / None(hole)。
        extents::get_pblock_idx_state(reader, sb, inode, lblock)
    } else {
        // 间接映射：Phase 3 非目标，stub。
        let pblock = indirect::get_pblock_idx_legacy(inode, lblock)?;
        Ok(Some((pblock, false)))
    }
}
