// SPDX-License-Identifier: MPL-2.0
#![forbid(unsafe_code)]

//! 安全重写的 ext4 核心。本子树零 unsafe（`#![forbid(unsafe_code)]` 强制）。
//!
//! on-disk 结构体一律 `RawXxx`（`#[repr(C)] #[derive(Pod)]`，磁盘字节布局）。
//! Phase 1 只建 `RawXxx` + 最小 accessor；逻辑视图与转换留 Phase 2-5。
//!
//! # Phase 6 Task 0 — 可见性接缝（visibility facade）
//!
//! cutover（Phase 6）要让生产集成层（`crate::fs::ext4`：`fs.rs` / `core_adapter.rs`）直接调
//! core 的操作 + 驱动 journal。原先 core 的 API 是 `pub(super)`（= `core` 模块内可见）、journal
//! 引擎是 `pub(in crate::fs::ext4::core)`（= `core` 子树内可见），二者都**够不到** `crate::fs::ext4`。
//!
//! **采取的方案 = 直接放宽可见性（brief 的 Option A），不另建 re-export facade**：
//! - 把 core 非 journal 模块（`io`/`balloc`/`ialloc`/`inode`/`superblock`/`extents`/`file`/`dir`/
//!   `block_group`/`block_map`/`indirect`/`migrate`/`crc`/`bitmap`/`alloc_guard`）里的 `pub(super)`
//!   条目统一抬到 `pub(in crate::fs::ext4)`；私有 `fn` 仍私有（只抬已暴露在模块边界的 API）。
//! - journal 引擎里 cutover 要用的条目（`transaction`/`space`/`commit`/`recovery`/`superblock`/
//!   `format`）从 `pub(in crate::fs::ext4::core)` 抬到 `pub(in crate::fs::ext4)`；`revoke`（C1/C2
//!   推迟项）保持不动。
//! - 模块路径也要可达：`io`/`balloc`/`ialloc` 原是私有 `mod`，抬成 `pub(in crate::fs::ext4) mod`。
//!
//! 为何不用 `pub(super) use` re-export facade：re-export 的可见性不能宽于被导出条目本身，
//! `pub(super) use`（目标 = `crate::fs::ext4`）导出一个 `pub(super)`（= `core`）条目会触发
//! E0364/E0365（私有条目泄漏）；要么先把条目抬宽、要么直接抬宽——直接抬宽更省一层间接。
//!
//! 抬宽**不破坏**任何现编译单元：`crate::fs::ext4` ⊇ `crate::fs::ext4::core`，core 内部调用照旧。
//! 集成层从 `crate::fs::ext4` 起以 `super::core::…` / `crate::fs::ext4::core::…` 命名（注意
//! `mod core` shadow std `core`，siblings 用 `::core::` 指 std crate）。

mod prelude;
pub mod types;

// Phase 6 Task 0: the integration layer (`crate::fs::ext4`, i.e. fs.rs / core_adapter.rs)
// must NAME `core::io::{BlockReader,BlockWriter}`, `core::balloc::{BlockAllocator,InodeAllocCtx}`
// and `core::ialloc::InodeAllocator` to build the production adapters. Their *items* were
// widened from `pub(super)` to `pub(in crate::fs::ext4)`; the *module paths* must be reachable
// too, so these three modules are `pub(in crate::fs::ext4)` (was `mod`). `bitmap`/`alloc_guard`
// stay private to core (no integration-layer caller names them directly).
pub(in crate::fs::ext4) mod io;
mod bitmap;
// Phase 6 Task 5b: `fs.rs` imports `LocalOperationAllocGuard` from here (it replaced the
// `ext4_rs::LocalOperationAllocGuard` the integration layer used), so the module path must be
// reachable from `crate::fs::ext4`.
pub(in crate::fs::ext4) mod alloc_guard;
pub(in crate::fs::ext4) mod balloc;
pub(in crate::fs::ext4) mod ialloc;

#[cfg(ktest)]
mod test_util;
#[cfg(ktest)]
mod diff_harness;

pub mod superblock;
pub mod block_group;
pub mod inode;
pub mod block_map;
mod indirect;
mod migrate;
pub mod extents;
pub mod file;
pub mod dir;
pub mod journal;
pub mod crc;
pub mod metadata_writer;
