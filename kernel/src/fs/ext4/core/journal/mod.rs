// SPDX-License-Identifier: MPL-2.0
//! JBD2 日志子系统。
//!
//! - Phase 1：on-disk format Pod 化（[`format`]，7 个大端结构 + accessor）。
//! - Phase 5：逻辑层逐 Task 接入——
//!   - Task 1 `superblock`（load/store + csum 套件），
//!   - Task 2 `space`/`transaction`（环形空间 + 事务状态机 → `JournalCommitPlan`），
//!   - Task 3 `commit`（commit 落盘，单屏障写序），
//!   - Task 4 `revoke`，Task 5 `recovery`（三趟 replay）。
//!
//! Task 0 仅扩展差分 harness（ktest 专用）以驱动/对拍 JBD2；
//! 本模块的逻辑子模块由 Task 1+ 逐个 `pub mod` 进来。

pub mod commit;
pub mod format;
pub mod recovery;
pub mod revoke;
pub mod space;
pub mod superblock;
pub mod transaction;
