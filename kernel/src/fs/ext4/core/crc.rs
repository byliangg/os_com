// SPDX-License-Identifier: MPL-2.0
//! crc32c（Castagnoli，反射多项式 0x82F63B78）——ext4 metadata_csum。
//! 复刻 ext4_rs `ext4_crc32c` 的查表算法，逐位一致；表用 const fn 编译期生成（免手抄 256 项）。

/// crc32c 初值（与 ext4_rs `EXT4_CRC32_INIT` 一致）。
pub const EXT4_CRC32_INIT: u32 = 0xFFFF_FFFF;

/// 反射形式的 Castagnoli 多项式。
const CRC32C_POLY: u32 = 0x82F6_3B78;

/// 编译期生成 CRC32C 查表（256 项），等价于 ext4_rs 的 `CRC32C_TAB`。
const fn build_crc32c_table() -> [u32; 256] {
    let mut tab = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ CRC32C_POLY
            } else {
                crc >> 1
            };
            j += 1;
        }
        tab[i] = crc;
        i += 1;
    }
    tab
}

const CRC32C_TAB: [u32; 256] = build_crc32c_table();

/// 计算 crc32c。与 ext4_rs `ext4_crc32c(crc, buf, buf.len())` 逐位一致。
pub fn ext4_crc32c(crc: u32, buf: &[u8]) -> u32 {
    let mut crc = crc;
    for &b in buf {
        crc = CRC32C_TAB[((crc as u8) ^ b) as usize] ^ (crc >> 8);
    }
    crc
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::{EXT4_CRC32_INIT, ext4_crc32c};
    use crate::fs::ext4::core::test_util::slice_at;
    use crate::prelude::*;

    /// 与旧 ext4_rs 实现对拍同一输入。
    fn diff(data: &[u8]) {
        let mine = ext4_crc32c(EXT4_CRC32_INIT, data);
        let old = ext4_rs::ext4_crc32c(EXT4_CRC32_INIT, data, data.len() as u32);
        assert_eq!(mine, old, "crc32c mismatch vs ext4_rs");
    }

    #[ktest]
    fn crc32c_matches_ext4_rs() {
        diff(&[]);
        diff(b"123456789");
        diff(b"The quick brown fox jumps over the lazy dog");
        // 真镜像超级块的若干片段。
        diff(slice_at(1024, 512));
        diff(slice_at(2048, 256));
        diff(slice_at(0, 1024));
    }

    #[ktest]
    fn crc32c_empty_is_init() {
        assert_eq!(ext4_crc32c(EXT4_CRC32_INIT, &[]), EXT4_CRC32_INIT);
    }
}
