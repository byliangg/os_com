// SPDX-License-Identifier: MPL-2.0
//! Phase-1 差分测试 helper（仅 ktest 构建）。

/// 编译期嵌入的真 ext4 镜像（mkfs.ext4 造，见 test/initramfs/Makefile）。
/// 路径从 kernel/src/fs/ext4/core/ 上溯 5 层到仓库根。
pub static EXT4_IMAGE: &[u8] =
    include_bytes!("../../../../../test/initramfs/build/ext4.img");

/// 取镜像 [off, off+len) 切片。
pub fn slice_at(off: usize, len: usize) -> &'static [u8] {
    &EXT4_IMAGE[off..off + len]
}
