// SPDX-License-Identifier: MPL-2.0
//! 文件读路径（report §5）——extent/间接映射之上的块范围规划与字节读出。
//!
//! 安全复刻 ext4_rs `ext4_impls/file.rs` 的读半部：`read_at`（:683）、
//! `map_blocks`（:793）→ `collect_block_ranges`（:361）、`plan_direct_read`（:803），
//! 以及它们共享的 `resolve_block_mapping`（:309）/`push_block_range`（:282）。
//!
//! **读语义（parity-first，逐字节复刻）：**
//! - hole（未映射）与 **unwritten extent** 在读路径都视为**读零**——`resolve_block_mapping`
//!   把二者都解析成 `None`，`read_at` 对 `None` 段 `fill(0)`、`collect_block_ranges` 对
//!   `None` 段跳过（不计入覆盖、截断当前 run）。
//! - 映射段按 lblock **升序** coalesce（物理连续才合并），载体 [`SimpleBlockRange`]。
//! - `read_at` clamp 到 `file_size`（`offset >= file_size → 0`、`offset+len > file_size`
//!   截到 `file_size - offset`）；`plan_direct_read` 再 floor 到整块。
//!
//! 只读上下文 [`ReadCtx`] 注入 `&dyn BlockReader` + `&RawSuperblock`，core **不持全局锁**。

use core::cmp::min;

use super::extents;
use super::inode::{load_inode, Inode};
use super::io::BlockReader;
use super::prelude::*;
use super::superblock::RawSuperblock;

/// extent 标志位（与 [`super::inode::EXT4_INODE_FLAG_EXTENTS`] 同值，本模块本地引用）。
const EXT4_INODE_FLAG_EXTENTS: u32 = 0x0008_0000;

/// 逻辑块 → 物理块的连续映射段（读路径载体）。
///
/// 字段名/序/类型逐字对齐 ext4_rs `SimpleBlockRange`（simple_interface/mod.rs:85），
/// 集成层二分依赖：`len` 单位 = **块**，映射向量按 `lblock` **升序**。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SimpleBlockRange {
    pub lblock: u32,
    pub pblock: u64,
    pub len: u32,
}

/// 只读上下文：块设备读接缝 + 超级块 + 块大小。core 读路径全经它，**不获取全局锁**。
pub(super) struct ReadCtx<'a> {
    pub reader: &'a dyn BlockReader,
    pub sb: &'a RawSuperblock,
    pub block_size: usize,
}

impl<'a> ReadCtx<'a> {
    pub(super) fn new(reader: &'a dyn BlockReader, sb: &'a RawSuperblock) -> Self {
        let block_size = sb.block_size();
        Self {
            reader,
            sb,
            block_size,
        }
    }
}

/// extent 缓存项：`(ext_start, ext_end, pblock_start, unwritten)`。
/// `pblock_start` = extent 的物理起始块（**未按 lblock 偏移**），与 ext4_rs 缓存一致。
type ExtentCache = Option<(u32, u32, Ext4Fsblk, bool)>;

/// 把 `(lblock, pblock, len)` 段并入 `mappings`，物理+逻辑都连续时与末段合并。
/// 逐位复刻 ext4_rs `push_block_range`（file.rs:282）。
fn push_block_range(mappings: &mut Vec<SimpleBlockRange>, lblock: u32, pblock: u64, len: u32) {
    if len == 0 {
        return;
    }
    if let Some(last) = mappings.last_mut() {
        let last_lend = last.lblock.saturating_add(last.len);
        let last_pend = last.pblock.saturating_add(last.len as u64);
        if last_lend == lblock && last_pend == pblock {
            last.len = last.len.saturating_add(len);
            return;
        }
    }
    mappings.push(SimpleBlockRange { lblock, pblock, len });
}

/// 把逻辑块 `lblock` 解析成**可读**物理块。hole 与 unwritten（预分配）extent 都报 `None`
/// （读路径据此零填），与 ext4_rs `resolve_block_mapping`（file.rs:309）逐位一致：
/// extent 缓存命中（含 unwritten 标志）走 O(1)；未命中则 `find_extent` 并按末层 extent 的
/// `[first, first+actual_len)` 区间再校验，命中写入缓存、unwritten→None、否则返回数据块号。
/// 非 extent inode 走 `map_block_for_read` 的间接分支（Phase 3 非目标，会 `unimplemented!`）。
fn resolve_block_mapping(
    ctx: &ReadCtx,
    inode: &Inode,
    uses_extents: bool,
    extent_cache: &mut ExtentCache,
    lblock: u32,
) -> Result<Option<Ext4Fsblk>> {
    if uses_extents {
        if let Some((ext_start, ext_end, pblock_start, unwritten)) = *extent_cache {
            if lblock >= ext_start && lblock < ext_end {
                if unwritten {
                    return Ok(None);
                }
                return Ok(Some(pblock_start + (lblock - ext_start) as u64));
            }
        }

        match extents::find_extent(ctx.reader, ctx.sb, inode, lblock) {
            Ok(path) => {
                let mapped = path.last().and_then(|node| node.extent).and_then(|extent| {
                    let ext_start = extent.first_block();
                    let ext_len = extent.len() as u32;
                    let ext_end = ext_start.checked_add(ext_len)?;
                    if lblock < ext_start || lblock >= ext_end {
                        return None;
                    }
                    let pblock_start = extent.start();
                    Some((ext_start, ext_end, pblock_start, extent.is_unwritten()))
                });

                if let Some((ext_start, ext_end, pblock_start, unwritten)) = mapped {
                    *extent_cache = Some((ext_start, ext_end, pblock_start, unwritten));
                    if unwritten {
                        return Ok(None);
                    }
                    return Ok(Some(pblock_start + (lblock - ext_start) as u64));
                }
                Ok(None)
            }
            Err(e) if e.error() == Errno::ENOENT => Ok(None),
            Err(e) => Err(e),
        }
    } else {
        // 间接映射（Phase 3 非目标）：复刻 ext4_rs 用 get_pblock_idx（ENOENT→None）。
        match super::block_map::map_block_for_read(ctx.reader, ctx.sb, inode, lblock) {
            Ok(Some((pblock, _))) => Ok(Some(pblock)),
            Ok(None) => Ok(None),
            Err(e) if e.error() == Errno::ENOENT => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// 把 `[lblock_start, lblock_start + lblock_count)` 解析成升序、coalesce 的映射向量。
/// 逐位复刻 ext4_rs `collect_block_ranges`（file.rs:361）。
///
/// **读语义**：hole/unwritten（`resolve_block_mapping` → None）跳过、不计入覆盖、截断
/// 当前 run；extent 命中且**非 unwritten**时走 extent 快速段（整段 push 到
/// `min(ext_end, lblock_end)`），否则逐块探测、物理不连续即断段。
fn collect_block_ranges(
    ctx: &ReadCtx,
    inode: &Inode,
    lblock_start: u32,
    lblock_count: u32,
) -> Result<Vec<SimpleBlockRange>> {
    if lblock_count == 0 {
        return Ok(Vec::new());
    }

    let lblock_end = lblock_start
        .checked_add(lblock_count)
        .ok_or_else(|| Error::with_message(Errno::EFBIG, "lblock range overflow"))?;
    let uses_extents = (inode.flags() & EXT4_INODE_FLAG_EXTENTS) != 0;
    let mut extent_cache: ExtentCache = None;
    let mut mappings: Vec<SimpleBlockRange> = Vec::new();
    let mut cursor = lblock_start;

    while cursor < lblock_end {
        let Some(pblock_start) =
            resolve_block_mapping(ctx, inode, uses_extents, &mut extent_cache, cursor)?
        else {
            // hole / unwritten：跳过该块，不并入任何 run。
            cursor += 1;
            continue;
        };

        if uses_extents {
            if let Some((ext_start, ext_end, cached_pblock_start, unwritten)) = extent_cache {
                if !unwritten && cursor >= ext_start && cursor < ext_end {
                    let run_end = ext_end.min(lblock_end);
                    push_block_range(
                        &mut mappings,
                        cursor,
                        cached_pblock_start + (cursor - ext_start) as u64,
                        run_end - cursor,
                    );
                    cursor = run_end;
                    continue;
                }
            }
        }

        // 通用路径：逐块探测、物理连续才续段。
        let run_start = cursor;
        cursor += 1;
        while cursor < lblock_end {
            let Some(next_pblock) =
                resolve_block_mapping(ctx, inode, uses_extents, &mut extent_cache, cursor)?
            else {
                break;
            };
            if next_pblock != pblock_start + (cursor - run_start) as u64 {
                break;
            }
            cursor += 1;
        }
        push_block_range(&mut mappings, run_start, pblock_start, cursor - run_start);
    }

    Ok(mappings)
}

/// `map_blocks`：解析 `[lblock_start, +lblock_count)` 为升序 coalesce 的映射向量。
/// 逐位复刻 ext4_rs `map_blocks`（file.rs:793）= `collect_block_ranges`。
#[allow(dead_code)]
pub(super) fn map_blocks(
    ctx: &ReadCtx,
    inode: &Inode,
    lblock_start: u32,
    lblock_count: u32,
) -> Result<Vec<SimpleBlockRange>> {
    collect_block_ranges(ctx, inode, lblock_start, lblock_count)
}

/// `plan_direct_read`：clamp 到 file_size、floor 到整块，返回 `(direct_len, 升序映射)`。
/// 逐位复刻 ext4_rs `plan_direct_read`（file.rs:803）。
#[allow(dead_code)]
pub(super) fn plan_direct_read(
    ctx: &ReadCtx,
    inode: &Inode,
    offset: usize,
    len: usize,
) -> Result<(usize, Vec<SimpleBlockRange>)> {
    let block_size = ctx.block_size;
    if len == 0 {
        return Ok((0, Vec::new()));
    }

    let file_size =
        usize::try_from(inode.size()).map_err(|_| Error::with_message(Errno::EFBIG, "file too big"))?;
    let read_end = offset.saturating_add(len).min(file_size);
    let read_len = read_end.saturating_sub(offset);
    let direct_len = read_len / block_size * block_size;
    if direct_len == 0 {
        return Ok((0, Vec::new()));
    }

    let lblock_start = u32::try_from(offset / block_size)
        .map_err(|_| Error::with_message(Errno::EFBIG, "lblock start too big"))?;
    let lblock_count = u32::try_from(direct_len / block_size)
        .map_err(|_| Error::with_message(Errno::EFBIG, "lblock count too big"))?;
    let mappings = collect_block_ranges(ctx, inode, lblock_start, lblock_count)?;
    Ok((direct_len, mappings))
}

/// `read_at`：从 `offset` 读 `read_buf.len()` 字节进 `read_buf`，返回实际读出字节数。
/// 逐字节复刻 ext4_rs `read_at`（file.rs:683）。
///
/// clamp：`read_buf` 空 → 0；`offset >= file_size → 0`；`offset+len > file_size` 截到
/// `file_size - offset`。逐 lblock 经 `resolve_pblock`（含 extent 缓存）解析：
/// 命中 → 读整块、取 `[inner, inner+len)`；hole/unwritten（None）→ `fill(0)`；
/// 物理块越 usize/整块乘溢出 → EIO。
#[allow(dead_code)]
pub(super) fn read_at(
    ctx: &ReadCtx,
    inode: &Inode,
    offset: usize,
    read_buf: &mut [u8],
) -> Result<usize> {
    let block_size = ctx.block_size;
    let mut read_buf_len = read_buf.len();
    if read_buf_len == 0 {
        return Ok(0);
    }

    let file_size = inode.size() as usize;
    if offset >= file_size {
        return Ok(0);
    }
    if offset + read_buf_len > file_size {
        read_buf_len = file_size - offset;
    }

    let uses_extents = (inode.flags() & EXT4_INODE_FLAG_EXTENTS) != 0;
    let mut extent_cache: ExtentCache = None;

    let mut block_data = vec![0u8; block_size];
    let mut cursor = 0usize;
    let mut current_offset = offset;

    while cursor < read_buf_len {
        let lblock = (current_offset / block_size) as u32;
        let block_inner_offset = current_offset % block_size;
        let read_length = min(read_buf_len - cursor, block_size - block_inner_offset);

        match resolve_block_mapping(ctx, inode, uses_extents, &mut extent_cache, lblock) {
            Ok(Some(pblock_idx)) => {
                let pblock =
                    usize::try_from(pblock_idx).map_err(|_| Error::with_message(Errno::EIO, "pblock out of usize range"))?;
                let block_offset = pblock
                    .checked_mul(block_size)
                    .ok_or_else(|| Error::with_message(Errno::EIO, "block offset overflow"))?;
                ctx.reader
                    .read_at(block_offset, block_data.as_mut_slice());
                read_buf[cursor..cursor + read_length].copy_from_slice(
                    &block_data[block_inner_offset..block_inner_offset + read_length],
                );
            }
            Ok(None) => {
                read_buf[cursor..cursor + read_length].fill(0);
            }
            Err(_) => {
                return Err(Error::with_message(
                    Errno::EIO,
                    "Failed to get physical block for logical block",
                ));
            }
        }

        cursor += read_length;
        current_offset += read_length;
    }

    Ok(read_buf_len)
}

/// 便捷入口：按 inode 号加载 inode 后 `read_at`（差分用例用，对齐 ext4_rs
/// `read_at(inode_num, ...)` 的签名形状）。
#[allow(dead_code)]
pub(super) fn read_at_by_ino(
    ctx: &ReadCtx,
    inode_num: u32,
    offset: usize,
    read_buf: &mut [u8],
) -> Result<usize> {
    let inode = load_inode(ctx.reader, ctx.sb, inode_num)?;
    read_at(ctx, &inode, offset, read_buf)
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::{map_blocks, plan_direct_read, read_at, ReadCtx};
    use crate::fs::ext4::core::diff_harness::MemDisk;
    use crate::fs::ext4::core::inode::load_inode;
    use crate::fs::ext4::core::io::BlockReader;
    use crate::fs::ext4::core::superblock::RawSuperblock;
    use crate::fs::ext4::core::test_util::{EXT4_IMAGE, EXT4_MULTIGROUP_IMAGE};
    use crate::prelude::*;

    /// 从内存盘读超级块（布局推导）。
    fn read_sb(disk: &MemDisk) -> RawSuperblock {
        let mut buf = vec![0u8; 1024];
        disk.read_at(1024, buf.as_mut_slice());
        RawSuperblock::from_bytes(&buf)
    }

    /// extent 映射逐 lblock + map_blocks/plan_direct_read 向量对拍 ext4_rs。
    ///
    /// 覆盖（EXT4_IMAGE，4K 块）：
    /// - inode #2 根目录：单 extent（1 块）；
    /// - inode #11 lost+found：单 extent 多块（4 块）；
    /// - inode #8 journal：三 extent（(0-9):15-24,(10-24):26-40,(25-1023):1066-2064），
    ///   含 extent 边界、物理不连续（不可 coalesce）、跨 extent；
    /// - 文件尾后逻辑块：hole（两侧 None / ENOENT）。
    /// 另跨组（EXT4_MULTIGROUP_IMAGE，1K 块）journal 单大 extent 跨组物理块。
    #[ktest]
    fn extent_read_map_parity() {
        for &(image, label) in &[
            (EXT4_IMAGE, "ext4.img"),
            (EXT4_MULTIGROUP_IMAGE, "ext4_multigroup.img"),
        ] {
            let new_disk = MemDisk::from_image(image);
            let old_disk = MemDisk::from_image(image);
            let sb = read_sb(&new_disk);
            let ctx = ReadCtx::new(&new_disk, &sb);
            let ext4 = ext4_rs::Ext4::open(Arc::new(old_disk.clone()));

            // 逐 lblock 对拍 get_pblock_idx_state。覆盖每个文件的全逻辑块 + 尾后 hole。
            for &ino in &[2u32, 11, 8] {
                let new_inode = load_inode(&new_disk, &sb, ino).expect("load_inode");
                let old_ref = ext4.get_inode_ref(ino);
                let file_size = new_inode.size() as usize;
                let bs = sb.block_size();
                let nblocks = file_size.div_ceil(bs.max(1)) as u32;
                // 多探一段尾后逻辑块，验证 hole 行为一致。
                let probe_end = nblocks + 4;
                for lblock in 0..probe_end {
                    let new_res = crate::fs::ext4::core::extents::get_pblock_idx_state(
                        &new_disk, &sb, &new_inode, lblock,
                    );
                    let old_res = ext4.get_pblock_idx_state(&old_ref, lblock);
                    match (new_res, old_res) {
                        (Ok(Some((np, nu))), Ok((op, ou))) => {
                            assert_eq!(np, op, "{label} ino={ino} lblock={lblock} pblock");
                            assert_eq!(nu, ou, "{label} ino={ino} lblock={lblock} unwritten");
                        }
                        (Ok(None), Err(e)) => {
                            assert_eq!(
                                e.error(),
                                ext4_rs::Errno::ENOENT,
                                "{label} ino={ino} lblock={lblock}: new=hole but old err != ENOENT"
                            );
                        }
                        (new_r, old_r) => panic!(
                            "{label} ino={ino} lblock={lblock} divergence: new={:?} old_ok={:?}",
                            new_r.as_ref().map(|o| o.is_some()),
                            old_r.is_ok(),
                        ),
                    }
                }

                // map_blocks 全文件 + 尾后一段，向量逐元素对拍。
                let new_mb = map_blocks(&ctx, &new_inode, 0, probe_end).expect("new map_blocks");
                let old_mb = ext4.map_blocks(ino, 0, probe_end).expect("old map_blocks");
                let new_tuples: Vec<(u32, u64, u32)> =
                    new_mb.iter().map(|r| (r.lblock, r.pblock, r.len)).collect();
                // 旧侧 SimpleBlockRange 类型未从 ext4_rs 重导出（无法命名），但 lblock/pblock/len
                // 字段是 pub，可直接取值对拍。
                let old_tuples: Vec<(u32, u64, u32)> =
                    old_mb.iter().map(|r| (r.lblock, r.pblock, r.len)).collect();
                assert_eq!(
                    new_tuples, old_tuples,
                    "{label} ino={ino} map_blocks vector mismatch"
                );

                // plan_direct_read 全文件，(direct_len, 向量) 对拍。
                let len = file_size + 4 * bs;
                let (new_dl, new_pr) =
                    plan_direct_read(&ctx, &new_inode, 0, len).expect("new plan_direct_read");
                let (old_dl, old_pr) =
                    ext4.plan_direct_read(ino, 0, len).expect("old plan_direct_read");
                assert_eq!(new_dl, old_dl, "{label} ino={ino} plan_direct_read direct_len");
                let new_pr_t: Vec<(u32, u64, u32)> =
                    new_pr.iter().map(|r| (r.lblock, r.pblock, r.len)).collect();
                let old_pr_t: Vec<(u32, u64, u32)> =
                    old_pr.iter().map(|r| (r.lblock, r.pblock, r.len)).collect();
                assert_eq!(
                    new_pr_t, old_pr_t,
                    "{label} ino={ino} plan_direct_read vector mismatch"
                );
            }
        }
    }

    /// 文件读出字节对拍 ext4_rs read_at。覆盖多 extent / hole / coalesce / 升序 /
    /// 跨块非对齐 / clamp（越界偏移、超长读）。
    #[ktest]
    fn file_read_bytes_parity() {
        for &image in &[EXT4_IMAGE, EXT4_MULTIGROUP_IMAGE] {
            let new_disk = MemDisk::from_image(image);
            let old_disk = MemDisk::from_image(image);
            let sb = read_sb(&new_disk);
            let bs = sb.block_size();
            let ctx = ReadCtx::new(&new_disk, &sb);
            let ext4 = ext4_rs::Ext4::open(Arc::new(old_disk.clone()));

            for &ino in &[2u32, 11, 8] {
                let new_inode = load_inode(&new_disk, &sb, ino).expect("load_inode");
                let file_size = new_inode.size() as usize;

                // 多组 (offset, len)：块对齐、跨块非对齐、起始非对齐、尾部不足整块、
                // 越界偏移（读 0）、超长读（截到 file_size）、空读。
                let cases: Vec<(usize, usize)> = vec![
                    (0, bs.min(file_size.max(1))),
                    (0, file_size),
                    (1, bs + 3),                 // 起始非对齐 + 跨块
                    (bs.saturating_sub(7), bs + 7), // 跨块边界
                    (file_size / 2, file_size),  // 中段到尾（超长截断）
                    (file_size, bs),             // 越界偏移 → 0
                    (file_size + 1, bs),         // 越界偏移 → 0
                    (0, 0),                      // 空读 → 0
                    (0, file_size + 2 * bs),     // 超长读 → 截到 file_size
                ];

                for (off, len) in cases {
                    let mut new_buf = vec![0xABu8; len];
                    let mut old_buf = vec![0xABu8; len];
                    let new_n = read_at(&ctx, &new_inode, off, &mut new_buf).expect("new read_at");
                    let old_n = ext4.read_at(ino, off, &mut old_buf).expect("old read_at");
                    assert_eq!(
                        new_n, old_n,
                        "image ino={ino} off={off} len={len}: read count mismatch"
                    );
                    assert_eq!(
                        new_buf, old_buf,
                        "image ino={ino} off={off} len={len}: read bytes mismatch"
                    );
                }
            }
        }
    }
}
