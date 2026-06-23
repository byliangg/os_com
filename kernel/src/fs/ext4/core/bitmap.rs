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
pub(super) fn ext4_bmap_is_bit_set(bmap: &[u8], bit: u32) -> bool {
    let byte_idx = (bit >> 3) as usize;
    if byte_idx >= bmap.len() {
        return false;
    }
    bmap[byte_idx] & (1 << (bit & 7)) != 0
}

/// 检查位图中某位是否清零（`is_bit_set` 取反）。
///
/// [对照] ext4_rs `ext4_bmap_is_bit_clr`
pub(super) fn ext4_bmap_is_bit_clr(bmap: &[u8], bit: u32) -> bool {
    !ext4_bmap_is_bit_set(bmap, bit)
}

/// 置位图中某位。越界静默返回（不 panic）。
///
/// [对照] ext4_rs `ext4_bmap_bit_set`
pub(super) fn ext4_bmap_bit_set(bmap: &mut [u8], bit: u32) {
    let byte_idx = (bit >> 3) as usize;
    if byte_idx >= bmap.len() {
        return;
    }
    bmap[byte_idx] |= 1 << (bit & 7);
}

/// 清位图中某位。越界静默返回（不 panic）。
///
/// [对照] ext4_rs `ext4_bmap_bit_clr`
pub(super) fn ext4_bmap_bit_clr(bmap: &mut [u8], bit: u32) {
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
pub(super) fn ext4_bmap_bit_find_clr(bmap: &[u8], sbit: u32, ebit: u32, bit_id: &mut u32) -> bool {
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
pub(super) fn ext4_bmap_bits_free(bmap: &mut [u8], start_bit: u32, end_bit: u32) {
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

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::{
        ext4_bmap_bit_clr, ext4_bmap_bit_find_clr, ext4_bmap_bit_set, ext4_bmap_bits_free,
        ext4_bmap_is_bit_clr, ext4_bmap_is_bit_set,
    };
    use crate::fs::ext4::core::block_group::RawGroupDescriptor;
    use crate::fs::ext4::core::superblock::RawSuperblock;
    use crate::fs::ext4::core::test_util::slice_at;
    use crate::prelude::*;

    /// 一组覆盖典型边界的手工缓冲：(名字, 字节)。
    fn handcrafted_buffers() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            // 空缓冲（越界路径）
            ("empty", Vec::new()),
            // 全 0（全部空闲）
            ("all_zero_8", vec![0x00; 8]),
            // 全 1（全部占用，find_clr 须整字节跳过）
            ("all_ones_8", vec![0xFF; 8]),
            // 单孔：第 3 字节有唯一一个清零位（bit 26 = byte3 bit2）
            ("single_hole", vec![0xFF, 0xFF, 0xFF, 0xFB, 0xFF, 0xFF, 0xFF, 0xFF]),
            // 末位空闲：最后一字节最高位清零（bit 63 清，其余满）
            ("last_bit_free", vec![0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F]),
            // 跨字节图案：交替/混合，触发各段
            ("mixed", vec![0x00, 0xFF, 0x0F, 0xF0, 0xFF, 0x01, 0x80, 0xAA]),
            // 单字节缓冲（最小非空）
            ("one_byte", vec![0xFE]),
        ]
    }

    /// 真镜像 group-0 块位图 + 手工缓冲拼成统一的差分输入集。
    /// 块位图块号从超级块 + 组 0 描述符推导（不写死偏移，兼容任意几何）。
    /// 实测 ext4.img：first_data_block=0、block_size=4096、64BIT、desc_size=64，
    /// 真块位图在块 9（offset 36864），含已分配满 0xFF 区 + 尾部空闲 0 区（非全 0/全 1）。
    fn diff_inputs() -> Vec<(&'static str, Vec<u8>)> {
        let mut v = handcrafted_buffers();
        let sb = RawSuperblock::from_bytes(slice_at(1024, 1024));
        let bs = sb.block_size();
        let gdt_off = (sb.first_data_block as usize + 1) * bs;
        let desc0 = RawGroupDescriptor::from_bytes(slice_at(gdt_off, size_of::<RawGroupDescriptor>()));
        let bbmp_off = desc0.block_bitmap() as usize * bs;
        v.push(("real_block_bitmap", slice_at(bbmp_off, bs).to_vec()));
        v
    }

    /// is_bit_set / is_bit_clr：新 vs ext4_rs 逐位对拍（含越界位）。
    #[ktest]
    fn bitmap_diff_is_set_clr() {
        for (name, buf) in diff_inputs() {
            let nbits = (buf.len() as u32) * 8;
            // 覆盖区间内每一位 + 越界几位。
            let probe_end = nbits + 16;
            for bit in 0..probe_end {
                let new_set = ext4_bmap_is_bit_set(&buf, bit);
                let old_set = ext4_rs::ext4_bmap_is_bit_set(&buf, bit);
                assert_eq!(new_set, old_set, "is_set mismatch buf={name} bit={bit}");

                let new_clr = ext4_bmap_is_bit_clr(&buf, bit);
                let old_clr = ext4_rs::ext4_bmap_is_bit_clr(&buf, bit);
                assert_eq!(new_clr, old_clr, "is_clr mismatch buf={name} bit={bit}");
            }
        }
    }

    /// bit_set / bit_clr：两份相同缓冲分别用新/旧操作同一批 bit，最终逐字节相等。
    #[ktest]
    fn bitmap_diff_set_clr_mutate() {
        for (name, buf) in diff_inputs() {
            let nbits = (buf.len() as u32) * 8;
            let probe_end = nbits + 16; // 含越界 bit，验证静默忽略一致

            // bit_set 对拍
            let mut new_buf = buf.clone();
            let mut old_buf = buf.clone();
            for bit in 0..probe_end {
                ext4_bmap_bit_set(&mut new_buf, bit);
                ext4_rs::ext4_bmap_bit_set(&mut old_buf, bit);
            }
            assert_eq!(new_buf, old_buf, "bit_set buffer mismatch buf={name}");

            // bit_clr 对拍（从原始缓冲重新开始）
            let mut new_buf = buf.clone();
            let mut old_buf = buf.clone();
            for bit in 0..probe_end {
                ext4_bmap_bit_clr(&mut new_buf, bit);
                ext4_rs::ext4_bmap_bit_clr(&mut old_buf, bit);
            }
            assert_eq!(new_buf, old_buf, "bit_clr buffer mismatch buf={name}");
        }
    }

    /// bit_find_clr：新旧返回值 + bit_id 逐场景对拍。
    /// 重点覆盖三种边界：起点非字节对齐（逐位推进段）、整 0xFF 跳过（整字节段）、tail 收尾段。
    #[ktest]
    fn bitmap_diff_find_clr() {
        for (name, buf) in diff_inputs() {
            let nbits = (buf.len() as u32) * 8;
            // 构造多种 (sbit, ebit)：
            // - 起点对齐 / 非对齐（1,3,7 等）
            // - 区间跨多字节、跨整 0xFF 区、收尾落在 tail
            let mut ranges: Vec<(u32, u32)> = Vec::new();
            if nbits == 0 {
                // 空缓冲：仍喂几组区间，验证越界 false 一致。
                ranges.push((0, 0));
                ranges.push((0, 8));
                ranges.push((3, 17));
            } else {
                let starts = [0u32, 1, 3, 7, 8, 9, nbits.saturating_sub(1)];
                let ends = [
                    0u32,
                    1,
                    8,
                    9,
                    16,
                    nbits / 2,
                    nbits.saturating_sub(1),
                    nbits,
                    nbits + 8, // 超出缓冲，触发整字节段的 byte_idx 越界 false
                ];
                for &s in &starts {
                    for &e in &ends {
                        if e >= s {
                            ranges.push((s, e));
                        }
                    }
                }
            }

            for (sbit, ebit) in ranges {
                let mut new_id: u32 = 0;
                let mut old_id: u32 = 0;
                let new_found = ext4_bmap_bit_find_clr(&buf, sbit, ebit, &mut new_id);
                let old_found = ext4_rs::ext4_bmap_bit_find_clr(&buf, sbit, ebit, &mut old_id);
                assert_eq!(
                    new_found, old_found,
                    "find_clr found mismatch buf={name} sbit={sbit} ebit={ebit}"
                );
                // bit_id 只在 found=true 时有意义（源在 false 路径不写出参）。
                if new_found {
                    assert_eq!(
                        new_id, old_id,
                        "find_clr bit_id mismatch buf={name} sbit={sbit} ebit={ebit}"
                    );
                }
            }
        }
    }

    /// bits_free：闭区间清位 + 跨字节 + 边界 clamp（超长 / 落在最后一字节）逐字节对拍。
    #[ktest]
    fn bitmap_diff_bits_free() {
        for (name, buf) in diff_inputs() {
            let nbits = (buf.len() as u32) * 8;
            let mut ranges: Vec<(u32, u32)> = Vec::new();
            if nbits == 0 {
                ranges.push((0, 0));
                ranges.push((0, 100));
            } else {
                // 跨字节闭区间 + end 落最后一字节 + 超长 clamp + start 超界
                ranges.push((0, 0)); // 单位
                ranges.push((0, 7)); // 整首字节
                ranges.push((3, 12)); // 跨字节
                ranges.push((1, nbits.saturating_sub(1))); // 几乎全清
                ranges.push((nbits / 2, nbits.saturating_sub(1))); // 收尾在最后一字节
                ranges.push((0, nbits + 100)); // end 超长 → clamp
                ranges.push((nbits, nbits + 10)); // start 越界 → no-op
                ranges.push((nbits + 5, nbits + 50)); // 整段越界
            }

            for (start_bit, end_bit) in ranges {
                let mut new_buf = buf.clone();
                let mut old_buf = buf.clone();
                ext4_bmap_bits_free(&mut new_buf, start_bit, end_bit);
                ext4_rs::ext4_bmap_bits_free(&mut old_buf, start_bit, end_bit);
                assert_eq!(
                    new_buf, old_buf,
                    "bits_free buffer mismatch buf={name} start={start_bit} end={end_bit}"
                );
            }
        }
    }
}
