// SPDX-License-Identifier: MPL-2.0
//! Core 本地读接缝（report §5）。
//!
//! 安全核心读盘只依赖本 trait，而非第三方 `ext4_rs::BlockDevice`——这样
//! `core/` 生产代码与 `ext4_rs` 解耦。集成层负责把真实块设备适配成 `BlockReader`
//! （Phase 5 接入）；差分测试里 `MemDisk` 同时实现本 trait 与 `ext4_rs::BlockDevice`，
//! 让新旧引擎共享同一份内存盘（见 `#[cfg(ktest)] mod diff_harness`）。

/// 按字节偏移读盘的本地接缝。`read_at` 把 `[off, off + out.len())` 的字节填进 `out`；
/// 落在盘尾之外的部分以 0 填充（与集成层 `read_offset_into` 的语义对齐）。
pub(super) trait BlockReader {
    /// 从偏移 `off` 读出 `out.len()` 字节到 `out`。
    fn read_at(&self, off: usize, out: &mut [u8]);
}
