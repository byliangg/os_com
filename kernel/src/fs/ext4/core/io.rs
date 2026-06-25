// SPDX-License-Identifier: MPL-2.0
//! Core 本地读接缝（report §5）。
//!
//! 安全核心读盘只依赖本 trait，而非第三方 `ext4_rs::BlockDevice`——这样
//! `core/` 生产代码与 `ext4_rs` 解耦。集成层负责把真实块设备适配成 `BlockReader`
//! （Phase 5 接入）；差分测试里 `MemDisk` 同时实现本 trait 与 `ext4_rs::BlockDevice`，
//! 让新旧引擎共享同一份内存盘（见 `#[cfg(ktest)] mod diff_harness`）。

/// 按字节偏移读盘的本地接缝。`read_at` 把 `[off, off + out.len())` 的字节填进 `out`；
/// 落在盘尾之外的部分以 0 填充（与集成层 `read_offset_into` 的语义对齐）。
///
/// Phase 6 Task 0：可见性放宽到 `pub(in crate::fs::ext4)`——集成层适配器（`core_adapter.rs`）
/// 需对真实块设备 / overlay 桥 **实现** 本 trait（命名权 + impl 都要从 `crate::fs::ext4` 可达）。
pub(in crate::fs::ext4) trait BlockReader {
    /// 从偏移 `off` 读出 `out.len()` 字节到 `out`。
    fn read_at(&self, off: usize, out: &mut [u8]);
}

/// 按字节偏移写**数据块**的本地接缝（写路径用）。
///
/// 与 [`super::metadata_writer::MetadataWriter`] 分工：`MetadataWriter` 写的是经 JBD2
/// 记账的**元数据**全块镜像（inode 表 / extent 树块 / 位图 / SB），按块号粒度；本 trait
/// 写的是**文件数据块**（write_at 的字节落盘、prepare_write_at 的零填），按字节偏移、绕过
/// journal——逐位对齐 ext4_rs `write_at` 里直接走 `block_device.write_offset(...)` 的数据写。
/// 集成层在 Phase 5 接入真实块设备；差分里 `MemDisk` 同实现本 trait，把数据写穿同一份字节。
///
/// Phase 6 Task 0：可见性放宽到 `pub(in crate::fs::ext4)`——集成层适配器需 **实现** 本 trait
/// （绕 journal 的数据块写 + recovery 的 raw home 写）。
pub(in crate::fs::ext4) trait BlockWriter {
    /// 把 `data` 写到字节偏移 `[off, off + data.len())`（盘尾外丢弃）。
    fn write_at(&self, off: usize, data: &[u8]);
}
