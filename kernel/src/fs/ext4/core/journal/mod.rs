// SPDX-License-Identifier: MPL-2.0
//! JBD2 日志子系统（Phase 1 只做 on-disk format Pod 化；事务/commit/恢复留 Phase 5）。

pub mod format;
