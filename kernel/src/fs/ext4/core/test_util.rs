// SPDX-License-Identifier: MPL-2.0
//! Phase-1 差分测试 helper（仅 ktest 构建）。

/// 编译期嵌入的真 ext4 镜像（mkfs.ext4 造，见 test/initramfs/Makefile）。
/// 路径从 kernel/src/fs/ext4/core/ 上溯 5 层到仓库根。
pub static EXT4_IMAGE: &[u8] =
    include_bytes!("../../../../../test/initramfs/build/ext4.img");

/// 多块组真镜像（1K 块、8 个块组、first_data_block=1、64bit、metadata_csum）。
/// 供跨组分配 / 组满回绕 / first_data_block!=0 几何的差分——4K 单组 `EXT4_IMAGE` 测不到。
pub static EXT4_MULTIGROUP_IMAGE: &[u8] =
    include_bytes!("../../../../../test/initramfs/build/ext4_multigroup.img");

/// metadata_csum 关闭的真镜像（4K 单组，几何同 `EXT4_IMAGE`，仅 csum 特性关）。
/// 供差分验证 csum gating：RO-compat 0x400 关时分配/释放不写位图/描述符 csum。
pub static EXT4_NOCSUM_IMAGE: &[u8] =
    include_bytes!("../../../../../test/initramfs/build/ext4_nocsum.img");

/// 取镜像 [off, off+len) 切片。
pub fn slice_at(off: usize, len: usize) -> &'static [u8] {
    &EXT4_IMAGE[off..off + len]
}
