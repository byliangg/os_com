// SPDX-License-Identifier: MPL-2.0
//! ext4 位图原语（balloc/ialloc 的叶子依赖）。
//!
//! 纯函数：只在 `&[u8]` 缓冲上操作，不碰盘。
//! 逐位复刻 `ext4_rs/src/utils/bitmap.rs` 的语义与边界行为（差分前提）；
//! **不引入** Linux `find-next-zero-bit` 等优化。
//!
//! 位序约定：`byte = bit >> 3`，`mask = 1 << (bit & 7)`；
//! byte 越界一律静默处理（set/clr 直接返回，is_set 视为未置位）。
//!
//! [对照来源] kernel/libs/ext4_rs/src/utils/bitmap.rs

/// 检查位图中某位是否置位。
/// 越界（`bit >> 3 >= bmap.len()`）视为未置位，返回 `false`。
///
/// [对照] ext4_rs `ext4_bmap_is_bit_set`
pub(in crate::fs::ext4) fn ext4_bmap_is_bit_set(bmap: &[u8], bit: u32) -> bool {
    let byte_idx = (bit >> 3) as usize;
    if byte_idx >= bmap.len() {
        return false;
    }
    bmap[byte_idx] & (1 << (bit & 7)) != 0
}

/// 检查位图中某位是否清零（`is_bit_set` 取反）。
///
/// [对照] ext4_rs `ext4_bmap_is_bit_clr`
pub(in crate::fs::ext4) fn ext4_bmap_is_bit_clr(bmap: &[u8], bit: u32) -> bool {
    !ext4_bmap_is_bit_set(bmap, bit)
}

/// 置位图中某位。越界静默返回（不 panic）。
///
/// [对照] ext4_rs `ext4_bmap_bit_set`
pub(in crate::fs::ext4) fn ext4_bmap_bit_set(bmap: &mut [u8], bit: u32) {
    let byte_idx = (bit >> 3) as usize;
    if byte_idx >= bmap.len() {
        return;
    }
    bmap[byte_idx] |= 1 << (bit & 7);
}

/// 清位图中某位。越界静默返回（不 panic）。
///
/// [对照] ext4_rs `ext4_bmap_bit_clr`
pub(in crate::fs::ext4) fn ext4_bmap_bit_clr(bmap: &mut [u8], bit: u32) {
    let byte_idx = (bit >> 3) as usize;
    if byte_idx >= bmap.len() {
        return;
    }
    bmap[byte_idx] &= !(1 << (bit & 7));
}

/// 在 `[sbit, ebit)` 半开区间内查找首个清零位。
///
/// 三段扫描（**严格照 ext4_rs 循环结构与边界**）：
/// 1. 起点逐位推进到字节边界（`while i & 7 != 0`），途中命中清零位即返回。
/// 2. 按整字节 8 位一跳，跳过值为 `0xFF` 的满字节；非满字节再逐位查其 8 位。
/// 3. tail 段逐位扫到 `ebit`。
///
/// 找到则写 `*bit_id` 并返回 `true`，否则返回 `false`。
///
/// [对照] ext4_rs `ext4_bmap_bit_find_clr`
pub(in crate::fs::ext4) fn ext4_bmap_bit_find_clr(bmap: &[u8], sbit: u32, ebit: u32, bit_id: &mut u32) -> bool {
    let mut i: u32;
    let mut bcnt = ebit - sbit;

    i = sbit;

    while i & 7 != 0 {
        if bcnt == 0 {
            return false;
        }

        if ext4_bmap_is_bit_clr(bmap, i) {
            *bit_id = i;
            return true;
        }

        i += 1;
        bcnt -= 1;
    }

    let mut byte_idx = (i >> 3) as usize;
    let mut bit_pos = i;

    while bcnt >= 8 {
        if byte_idx >= bmap.len() {
            return false;
        }

        if bmap[byte_idx] != 0xFF {
            for j in 0..8 {
                let bit_idx = bit_pos + j;
                if ext4_bmap_is_bit_clr(bmap, bit_idx) {
                    *bit_id = bit_idx;
                    return true;
                }
            }
        }

        byte_idx += 1;
        bcnt -= 8;
        bit_pos += 8;
    }

    while bcnt > 0 {
        if bit_pos >= ebit {
            return false;
        }

        if ext4_bmap_is_bit_clr(bmap, bit_pos) {
            *bit_id = bit_pos;
            return true;
        }

        bit_pos += 1;
        bcnt -= 1;
    }

    false
}

/// 清 `[start_bit, end_bit]` **闭区间**的位，clamp 到 `bmap.len() * 8`。
///
/// 空缓冲、`start_bit` 超界直接返回；`end_bit` 截到最大合法位。
///
/// [对照] ext4_rs `ext4_bmap_bits_free`
pub(in crate::fs::ext4) fn ext4_bmap_bits_free(bmap: &mut [u8], start_bit: u32, end_bit: u32) {
    if bmap.is_empty() {
        return;
    }
    let max_bit = (bmap.len() as u32).saturating_mul(8).saturating_sub(1);
    if start_bit > max_bit {
        return;
    }
    let end_bit = core::cmp::min(end_bit, max_bit);
    for bit in start_bit..=end_bit {
        ext4_bmap_bit_clr(bmap, bit);
    }
}
