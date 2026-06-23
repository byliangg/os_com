// SPDX-License-Identifier: MPL-2.0
#![forbid(unsafe_code)]

//! 安全重写的 ext4 核心。本子树零 unsafe（`#![forbid(unsafe_code)]` 强制）。
//!
//! on-disk 结构体一律 `RawXxx`（`#[repr(C)] #[derive(Pod)]`，磁盘字节布局）。
//! Phase 1 只建 `RawXxx` + 最小 accessor；逻辑视图与转换留 Phase 2-5。

mod prelude;
pub mod types;

#[cfg(ktest)]
mod test_util;

pub mod superblock;
pub mod block_group;
pub mod inode;
pub mod extents;
pub mod dir;
pub mod journal;
pub mod crc;
pub mod metadata_writer;
