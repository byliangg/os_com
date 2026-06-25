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

use super::extents::{self, BlockAlloc, WriteCtx};
use super::inode::{load_inode, write_back_inode, Inode};
use super::io::BlockReader;
use super::prelude::*;
use super::superblock::RawSuperblock;

/// extent 标志位（与 [`super::inode::EXT4_INODE_FLAG_EXTENTS`] 同值，本模块本地引用）。
const EXT4_INODE_FLAG_EXTENTS: u32 = 0x0008_0000;

/// 单块写的 unwritten 预分配尾块数。= ext4_rs `SMALL_WRITE_PREALLOC_BLOCKS`（file.rs:19）。
const SMALL_WRITE_PREALLOC_BLOCKS: u32 = 32;
/// 多块写的预分配尾块数（无预分配）。= ext4_rs `WRITE_PREALLOC_BLOCKS`（file.rs:18）。
const WRITE_PREALLOC_BLOCKS: u32 = 1;

/// fallocate 文件大小上界（值逐字复刻 ext4_rs `EXT4_MAX_FILE_SIZE`，consts.rs:43——
/// 字面量 `16 * 1024 * 1024 * 1024`；ext4_rs 注释写 16TB 但常量实际为 16GiB，**按值复刻**）。
/// `allocate_range` 用它做 EFBIG 守卫——这道上界正是 BUG-12 里 write_at/prepare 缺、
/// 唯独 allocate_range 复刻到位的那道（部分闭合 BUG-12）。
const EXT4_MAX_FILE_SIZE: u64 = 16 * 1024 * 1024 * 1024;

/// 逻辑块 → 物理块的连续映射段（读路径载体）。
///
/// 字段名/序/类型逐字对齐 ext4_rs `SimpleBlockRange`（simple_interface/mod.rs:85），
/// 集成层二分依赖：`len` 单位 = **块**，映射向量按 `lblock` **升序**。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::fs::ext4) struct SimpleBlockRange {
    pub lblock: u32,
    pub pblock: u64,
    pub len: u32,
}

/// 只读上下文：块设备读接缝 + 超级块 + 块大小。core 读路径全经它，**不获取全局锁**。
pub(in crate::fs::ext4) struct ReadCtx<'a> {
    pub reader: &'a dyn BlockReader,
    pub sb: &'a RawSuperblock,
    pub block_size: usize,
}

impl<'a> ReadCtx<'a> {
    pub(in crate::fs::ext4) fn new(reader: &'a dyn BlockReader, sb: &'a RawSuperblock) -> Self {
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
pub(in crate::fs::ext4) fn map_blocks(
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
pub(in crate::fs::ext4) fn plan_direct_read(
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
pub(in crate::fs::ext4) fn read_at(
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
pub(in crate::fs::ext4) fn read_at_by_ino(
    ctx: &ReadCtx,
    inode_num: u32,
    offset: usize,
    read_buf: &mut [u8],
) -> Result<usize> {
    let inode = load_inode(ctx.reader, ctx.sb, inode_num)?;
    read_at(ctx, &inode, offset, read_buf)
}

// =====================================================================
// 文件写半部（Phase 3 Task 3）。
//
// 安全复刻 ext4_rs `ext4_impls/file.rs` 写半部：write_at（:1615）、prepare_write_at（:1282）、
// ensure_write_range_mapped（:934）、insert_allocated_blocks_as_extents（:832）。
// ext4_rs 走 `Ext4` god-object（自持块设备 / 全局分配器）；core 把分配抽成注入的
// [`BlockAlloc`] 回调（Phase-2 `BlockAllocator::balloc_alloc_block(None)`），数据块写经
// [`WriteCtx::data_writer`]，extent 树写 / inode 写回经 `WriteCtx::writer`，**不持全局锁**。
//
// **写语义（parity-first）**：hole + unwritten 块在写前都零填、再经映射步变 written；
// 单块写预分配 `SMALL_WRITE_PREALLOC_BLOCKS=32` 的 unwritten 尾；prepare_write_at 返回全量
// 映射（含预分配尾），含「全已分配」快路径。
// =====================================================================

/// 把逻辑块解析成物理块号（hole → ENOENT），对齐 ext4_rs `get_pblock_idx`。
/// core `get_pblock_idx_state` 返回 `Ok(None)` 表 hole，这里转成 ENOENT 形态供写路径分支。
fn get_pblock_idx(ctx: &WriteCtx, inode: &Inode, lblock: u32) -> Result<Ext4Fsblk> {
    match extents::get_pblock_idx_state(ctx.reader, ctx.sb, inode, lblock)? {
        Some((pblock, _)) => Ok(pblock),
        None => Err(Error::with_message(Errno::ENOENT, "lblock not mapped")),
    }
}

/// `(pblock, unwritten)`（hole → ENOENT），对齐 ext4_rs `get_pblock_idx_state`。
fn get_pblock_idx_state(ctx: &WriteCtx, inode: &Inode, lblock: u32) -> Result<(Ext4Fsblk, bool)> {
    match extents::get_pblock_idx_state(ctx.reader, ctx.sb, inode, lblock)? {
        Some(v) => Ok(v),
        None => Err(Error::with_message(Errno::ENOENT, "lblock not mapped")),
    }
}

/// 把分配到的物理块按连续段插成 extent（written / unwritten）。
/// 复刻 ext4_rs `insert_allocated_blocks_as_extents_inner`（file.rs:874）。
fn insert_allocated_blocks_as_extents_inner(
    ctx: &WriteCtx,
    alloc: &mut dyn BlockAlloc,
    inode: &mut Inode,
    lblock_start: u32,
    allocated_blocks: &[Ext4Fsblk],
    unwritten: bool,
    inserted_out: &mut usize,
) -> Result<()> {
    *inserted_out = 0;
    if allocated_blocks.is_empty() {
        return Ok(());
    }
    // PARITY: unwritten 编码上限 EXT_INIT_MAX_LEN-1。
    let max_chunk = if unwritten { 32768 - 1 } else { 32768 } as usize;

    let mut logical = lblock_start;
    let mut seg_begin = 0usize;
    while seg_begin < allocated_blocks.len() {
        let mut seg_end = seg_begin + 1;
        while seg_end < allocated_blocks.len()
            && allocated_blocks[seg_end] == allocated_blocks[seg_end - 1] + 1
        {
            seg_end += 1;
        }
        let mut phys = allocated_blocks[seg_begin];
        let mut remaining = seg_end - seg_begin;
        while remaining > 0 {
            let chunk_len = min(remaining, max_chunk);
            let mut newex = extents::RawExtent::default();
            newex.first_block = logical;
            newex.store_pblock(phys);
            newex.block_count = chunk_len as u16;
            if unwritten {
                newex.mark_unwritten();
            }
            extents::insert_extent(ctx, alloc, inode, &newex)?;

            logical = logical
                .checked_add(chunk_len as u32)
                .ok_or_else(|| Error::with_message(Errno::EINVAL, "logical block overflow"))?;
            phys += chunk_len as u64;
            *inserted_out += chunk_len;
            remaining -= chunk_len;
        }
        seg_begin = seg_end;
    }
    Ok(())
}

fn insert_allocated_blocks_as_extents(
    ctx: &WriteCtx,
    alloc: &mut dyn BlockAlloc,
    inode: &mut Inode,
    lblock_start: u32,
    allocated_blocks: &[Ext4Fsblk],
) -> Result<usize> {
    let mut inserted = 0usize;
    insert_allocated_blocks_as_extents_inner(
        ctx,
        alloc,
        inode,
        lblock_start,
        allocated_blocks,
        false,
        &mut inserted,
    )?;
    Ok(inserted)
}

fn insert_unwritten_blocks_as_extents(
    ctx: &WriteCtx,
    alloc: &mut dyn BlockAlloc,
    inode: &mut Inode,
    lblock_start: u32,
    allocated_blocks: &[Ext4Fsblk],
    inserted_out: &mut usize,
) -> Result<()> {
    insert_allocated_blocks_as_extents_inner(
        ctx,
        alloc,
        inode,
        lblock_start,
        allocated_blocks,
        true,
        inserted_out,
    )
}

/// 预扫写区间 → 分配 hole、转换 unwritten → unwritten 预分配尾。返回新分配/转换的块数。
/// 复刻 ext4_rs `ensure_write_range_mapped`（file.rs:934，extent 分支；间接分支 Phase 3 非目标）。
fn ensure_write_range_mapped(
    ctx: &WriteCtx,
    alloc: &mut dyn BlockAlloc,
    inode: &mut Inode,
    start_bgid: &mut u32,
    start_lblock: u32,
    end_lblock: u32,
) -> Result<usize> {
    if start_lblock >= end_lblock {
        return Ok(0);
    }
    let uses_extents = (inode.flags() & EXT4_INODE_FLAG_EXTENTS) != 0;
    let requested_blocks = end_lblock.saturating_sub(start_lblock);
    let prealloc_blocks = if requested_blocks <= 1 {
        SMALL_WRITE_PREALLOC_BLOCKS
    } else {
        WRITE_PREALLOC_BLOCKS
    };

    let mut allocated_total = 0usize;
    let mut cursor = start_lblock;

    // 间接映射（非 extent）：Phase 3 非目标，差分两侧用 extent inode，不会进此分支。
    if !uses_extents {
        return Err(Error::with_message(
            Errno::EOPNOTSUPP,
            "legacy (indirect) write mapping not supported in Phase 3",
        ));
    }

    while cursor < end_lblock {
        let initial_probe = get_pblock_idx_state(ctx, inode, cursor);
        match initial_probe {
            Ok((_pblock, unwritten)) => {
                if unwritten {
                    // unwritten 预分配块：原地转 written（不分配）。
                    let conv_end = extents::convert_unwritten_span(
                        ctx, alloc, inode, cursor, end_lblock,
                    )?;
                    if conv_end <= cursor {
                        return Err(Error::with_message(
                            Errno::EIO,
                            "unwritten conversion made no progress",
                        ));
                    }
                    allocated_total += (conv_end - cursor) as usize;
                    cursor = conv_end;
                } else {
                    cursor += 1;
                }
            }
            Err(e) if e.error() == Errno::ENOENT => {
                let run_start = cursor;
                cursor += 1;
                // 扫到下一个已映射块（hole run）。
                while cursor < end_lblock {
                    match get_pblock_idx(ctx, inode, cursor) {
                        Ok(_) => break,
                        Err(ne) if ne.error() == Errno::ENOENT => cursor += 1,
                        Err(ne) => return Err(ne),
                    }
                }
                // 预分配尾探测（到 run_start + prealloc_blocks）。
                let prealloc_end = run_start.saturating_add(prealloc_blocks);
                while cursor < prealloc_end {
                    match get_pblock_idx(ctx, inode, cursor) {
                        Ok(_) => break,
                        Err(ne) if ne.error() == Errno::ENOENT => cursor += 1,
                        Err(ne) => return Err(ne),
                    }
                }
                let run_len = (cursor - run_start) as usize;
                let allocated_blocks = alloc.alloc_batch(inode, start_bgid, run_len)?;
                if allocated_blocks.is_empty() {
                    return Err(Error::with_message(
                        Errno::ENOSPC,
                        "no free blocks while mapping write range",
                    ));
                }

                // 写区间内的块插成 written；超出写区间的预分配尾插成 unwritten。
                let write_run_len =
                    end_lblock.saturating_sub(run_start).min(cursor - run_start) as usize;
                let written_take = allocated_blocks.len().min(write_run_len);
                let (write_part, tail_part) = allocated_blocks.split_at(written_take);

                let inserted =
                    insert_allocated_blocks_as_extents(ctx, alloc, inode, run_start, write_part)?;
                allocated_total += inserted;

                if inserted == 0 {
                    return Err(Error::with_message(
                        Errno::ENOSPC,
                        "no free blocks while mapping write range",
                    ));
                }

                // 预分配尾（best-effort）：插成 unwritten extent。
                if !tail_part.is_empty() && inserted == written_take {
                    let tail_start = run_start + written_take as u32;
                    let mut tail_inserted = 0usize;
                    // PARITY: ext4_rs 尾插失败时只 free 未插入后缀；core 差分里不构造该失败，
                    //   此处把错误向上抛（与 ext4_rs 成功路径字节一致）。
                    insert_unwritten_blocks_as_extents(
                        ctx,
                        alloc,
                        inode,
                        tail_start,
                        tail_part,
                        &mut tail_inserted,
                    )?;
                }

                if inserted < write_run_len {
                    cursor = run_start + inserted as u32;
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(allocated_total)
}

/// 分配起扫组：优先「前驱已映射块」组 → 「写区间尾」组 → extent 树首块所在组 →
/// inode 所在组。逐位复刻 ext4_rs `initial_write_alloc_bgid`（file.rs:25）。
fn initial_write_alloc_bgid(ctx: &WriteCtx, inode: &Inode, lblock_start: u32, lblock_end: u32) -> u32 {
    use super::block_group::GroupGeometry;
    let geom = GroupGeometry::new(ctx.sb);
    let try_mapped_bgid =
        |lblock: u32| get_pblock_idx(ctx, inode, lblock).ok().map(|pblock| geom.get_bgid_of_block(pblock));

    if let Some(prev_lblock) = lblock_start.checked_sub(1) {
        if let Some(bgid) = try_mapped_bgid(prev_lblock) {
            return bgid;
        }
    }
    if let Some(bgid) = try_mapped_bgid(lblock_end) {
        return bgid;
    }
    if (inode.flags() & EXT4_INODE_FLAG_EXTENTS) != 0 {
        if let Ok(path) = extents::find_extent(ctx.reader, ctx.sb, inode, lblock_start) {
            if let Some(node) = path.last() {
                if let Some(extent) = node.extent {
                    let ext_start = extent.first_block();
                    let ext_len = extent.len() as u32;
                    let candidate_pblock = if ext_len == 0 {
                        None
                    } else if lblock_start <= ext_start {
                        Some(extent.start())
                    } else {
                        Some(extent.start() + ext_len.saturating_sub(1) as u64)
                    };
                    if let Some(pblock) = candidate_pblock {
                        return geom.get_bgid_of_block(pblock);
                    }
                }
            }
        }
    }
    // 回退：inode 所在组 = (inode_num-1)/inodes_per_group。
    inode.num.saturating_sub(1) / ctx.sb.inodes_per_group()
}

/// `write_at`：从 `offset` 写 `write_buf` 进文件，返回写出字节数。
/// 逐字节复刻 ext4_rs `write_at`（file.rs:1615）。
///
/// 流程：预扫 hole/unwritten → `ensure_write_range_mapped`（分配 + 转换）→ 新映射块零填 →
/// 不对齐头 RMW + 整块 run 写 + 尾 RMW → 长 i_size + `write_back_inode`。
pub(in crate::fs::ext4) fn write_at(
    ctx: &WriteCtx,
    alloc: &mut dyn BlockAlloc,
    inode: &mut Inode,
    offset: usize,
    write_buf: &[u8],
) -> Result<usize> {
    let block_size = ctx.block_size;
    if write_buf.is_empty() {
        return Ok(0);
    }
    let file_size = inode.size();
    // PARITY/TODO(BUG-12): ext4_rs write_at (file.rs:1846) 还有 `write_end > EXT4_MAX_FILE_SIZE → EFBIG`
    // 上界检查，core 这里只防 checked_add 溢出（allocate_range 已复刻该上界）。16GiB 阈值现实小文件
    // 不触发，迁移后与 ext4_rs 对齐（见 bug.md BUG-12）。
    let write_end = offset
        .checked_add(write_buf.len())
        .ok_or_else(|| Error::with_message(Errno::EFBIG, "write end overflow"))?;
    let iblock_start = offset / block_size;
    let iblock_end = (write_end - 1) / block_size + 1;
    let lblock_start =
        u32::try_from(iblock_start).map_err(|_| Error::with_message(Errno::EFBIG, "lblock start too big"))?;
    let lblock_end =
        u32::try_from(iblock_end).map_err(|_| Error::with_message(Errno::EFBIG, "lblock end too big"))?;

    // 预扫：hole + unwritten 块都进零填表（映射后变 written、设备内容是垃圾）。
    let mut newly_hole_lblocks = Vec::new();
    for lblock in lblock_start..lblock_end {
        match get_pblock_idx_state(ctx, inode, lblock) {
            Ok((_, unwritten)) => {
                if unwritten {
                    newly_hole_lblocks.push(lblock);
                }
            }
            Err(e) if e.error() == Errno::ENOENT => newly_hole_lblocks.push(lblock),
            Err(e) => return Err(e),
        }
    }

    let mut start_bgid = initial_write_alloc_bgid(ctx, inode, lblock_start, lblock_end);
    let allocated_total =
        ensure_write_range_mapped(ctx, alloc, inode, &mut start_bgid, lblock_start, lblock_end)?;

    let blocks_count = ctx.sb.blocks_count();
    let block_offset = |pblock_idx: Ext4Fsblk| -> Result<usize> {
        if pblock_idx >= blocks_count {
            return Err(Error::with_message(Errno::EIO, "mapped block out of range"));
        }
        let pblock =
            usize::try_from(pblock_idx).map_err(|_| Error::with_message(Errno::EFBIG, "pblock too big"))?;
        pblock
            .checked_mul(block_size)
            .ok_or_else(|| Error::with_message(Errno::EFBIG, "block offset overflow"))
    };

    // 零填新映射块（hole + 已转 written 的 unwritten 块）。
    if !newly_hole_lblocks.is_empty() {
        let zero_block = vec![0u8; block_size];
        for lblock in newly_hole_lblocks {
            let pblock_idx = get_pblock_idx(ctx, inode, lblock)?;
            let blk_off = block_offset(pblock_idx)?;
            ctx.data_writer.write_at(blk_off, zero_block.as_slice());
        }
    }

    let mut written = 0usize;
    let mut iblk_idx = lblock_start;
    let unaligned = offset % block_size;
    let mut block_data = vec![0u8; block_size];

    // 不对齐头：RMW。
    if unaligned > 0 {
        let len = min(write_buf.len() - written, block_size - unaligned);
        let pblock_idx = get_pblock_idx(ctx, inode, iblk_idx)?;
        let blk_off = block_offset(pblock_idx)?;
        ctx.reader.read_at(blk_off, block_data.as_mut_slice());
        block_data[unaligned..unaligned + len].copy_from_slice(&write_buf[written..written + len]);
        ctx.data_writer.write_at(blk_off, block_data.as_slice());
        written += len;
        iblk_idx += 1;
    }

    // 整块 run：物理连续的块合并成一次写。
    while write_buf.len() - written >= block_size {
        let run_start_lblk = iblk_idx;
        let run_start_written = written;
        let run_start_pblk = get_pblock_idx(ctx, inode, run_start_lblk)?;
        let mut run_blocks = 1usize;
        let full_blocks_remaining = (write_buf.len() - written) / block_size;
        while run_blocks < full_blocks_remaining {
            let next_lblk = run_start_lblk + run_blocks as u32;
            let next_pblk = get_pblock_idx(ctx, inode, next_lblk)?;
            if next_pblk != run_start_pblk + run_blocks as u64 {
                break;
            }
            run_blocks += 1;
        }
        let run_bytes = run_blocks * block_size;
        let run_off = block_offset(run_start_pblk)?;
        ctx.data_writer
            .write_at(run_off, &write_buf[run_start_written..run_start_written + run_bytes]);
        written += run_bytes;
        iblk_idx += run_blocks as u32;
    }

    // 尾不足整块：RMW。
    if written < write_buf.len() {
        let len = write_buf.len() - written;
        let pblock_idx = get_pblock_idx(ctx, inode, iblk_idx)?;
        let blk_off = block_offset(pblock_idx)?;
        ctx.reader.read_at(blk_off, block_data.as_mut_slice());
        block_data[..len].copy_from_slice(&write_buf[written..written + len]);
        ctx.data_writer.write_at(blk_off, block_data.as_slice());
        written += len;
    }

    // 长 i_size + write_back（或仅分配过也写回，与 ext4_rs 一致）。
    let new_size = offset + written;
    if new_size > file_size as usize {
        inode.set_size(new_size as u64);
        write_back_inode(ctx.writer, ctx.reader, ctx.sb, inode)?;
    } else if allocated_total > 0 {
        write_back_inode(ctx.writer, ctx.reader, ctx.sb, inode)?;
    }

    Ok(written)
}

/// `prepare_write_at`：返回 `(lblock_start, 全量映射)`（含预分配尾）。
/// 逐字节复刻 ext4_rs `prepare_write_at`（file.rs:1282）；core 返回元组首项是 `lblock_start`
/// （ext4_rs 仅返 Vec；集成层另传 lblock_start，core 一并给出便于对拍）。
///
/// **全已分配快路径**：一遍探测，全已分配（含 unwritten）则转换 unwritten + 零填，跳过分配，
/// 据探测记录直接建映射；任一 hole → fall through 走通用分配路径。
pub(in crate::fs::ext4) fn prepare_write_at(
    ctx: &WriteCtx,
    alloc: &mut dyn BlockAlloc,
    inode: &mut Inode,
    offset: usize,
    len: usize,
) -> Result<(usize, Vec<SimpleBlockRange>)> {
    let block_size = ctx.block_size;
    if len == 0 {
        return Ok((0, Vec::new()));
    }
    let file_size = inode.size();
    // PARITY/TODO(BUG-12): ext4_rs prepare_write_at (file.rs:1298) 还有 `> EXT4_MAX_FILE_SIZE → EFBIG`
    // 上界，core 只防溢出（见 bug.md BUG-12，迁移后对齐）。
    let write_end = offset
        .checked_add(len)
        .ok_or_else(|| Error::with_message(Errno::EFBIG, "write end overflow"))?;
    let lblock_start = u32::try_from(offset / block_size)
        .map_err(|_| Error::with_message(Errno::EFBIG, "lblock start too big"))?;
    let lblock_end = u32::try_from((write_end - 1) / block_size + 1)
        .map_err(|_| Error::with_message(Errno::EFBIG, "lblock end too big"))?;

    // 全已分配快路径。
    {
        let mut runs: Vec<SimpleBlockRange> = Vec::new();
        let mut unwritten_blocks: Vec<(u32, Ext4Fsblk)> = Vec::new();
        let mut all_allocated = true;
        for lblock in lblock_start..lblock_end {
            match get_pblock_idx_state(ctx, inode, lblock) {
                Ok((pblock, unwritten)) => {
                    if unwritten {
                        unwritten_blocks.push((lblock, pblock));
                    }
                    push_block_range(&mut runs, lblock, pblock, 1);
                }
                Err(err) if err.error() == Errno::ENOENT => {
                    all_allocated = false;
                    break;
                }
                Err(err) => return Err(err),
            }
        }

        if all_allocated {
            // 转换 unwritten span（覆盖到的逻辑块都变 written）。
            let mut i = 0usize;
            while i < unwritten_blocks.len() {
                let (from, _) = unwritten_blocks[i];
                let conv_end = extents::convert_unwritten_span(ctx, alloc, inode, from, lblock_end)?;
                while i < unwritten_blocks.len() && unwritten_blocks[i].0 < conv_end {
                    i += 1;
                }
            }
            // 零填那些原 unwritten 的物理块。
            if !unwritten_blocks.is_empty() {
                let zero_block = vec![0u8; block_size];
                for (_, pblock) in &unwritten_blocks {
                    let pblock = usize::try_from(*pblock)
                        .map_err(|_| Error::with_message(Errno::EFBIG, "pblock too big"))?;
                    let block_offset = pblock
                        .checked_mul(block_size)
                        .ok_or_else(|| Error::with_message(Errno::EFBIG, "block offset overflow"))?;
                    ctx.data_writer.write_at(block_offset, zero_block.as_slice());
                }
            }
            if write_end > file_size as usize {
                inode.set_size(write_end as u64);
                write_back_inode(ctx.writer, ctx.reader, ctx.sb, inode)?;
            }
            return Ok((lblock_start as usize, runs));
        }
    }

    // 通用路径：hole + unwritten 都进零填表。
    let mut newly_hole_lblocks = Vec::new();
    for lblock in lblock_start..lblock_end {
        match get_pblock_idx_state(ctx, inode, lblock) {
            Ok((_, unwritten)) => {
                if unwritten {
                    newly_hole_lblocks.push(lblock);
                }
            }
            Err(err) if err.error() == Errno::ENOENT => newly_hole_lblocks.push(lblock),
            Err(err) => return Err(err),
        }
    }

    let mut start_bgid = initial_write_alloc_bgid(ctx, inode, lblock_start, lblock_end);
    let allocated_total =
        ensure_write_range_mapped(ctx, alloc, inode, &mut start_bgid, lblock_start, lblock_end)?;

    if !newly_hole_lblocks.is_empty() {
        let zero_block = vec![0u8; block_size];
        for lblock in newly_hole_lblocks {
            let pblock = get_pblock_idx(ctx, inode, lblock)?;
            let pblock = usize::try_from(pblock)
                .map_err(|_| Error::with_message(Errno::EFBIG, "pblock too big"))?;
            let block_offset = pblock
                .checked_mul(block_size)
                .ok_or_else(|| Error::with_message(Errno::EFBIG, "block offset overflow"))?;
            ctx.data_writer.write_at(block_offset, zero_block.as_slice());
        }
    }

    if write_end > file_size as usize {
        inode.set_size(write_end as u64);
        write_back_inode(ctx.writer, ctx.reader, ctx.sb, inode)?;
    } else if allocated_total > 0 {
        write_back_inode(ctx.writer, ctx.reader, ctx.sb, inode)?;
    }

    let read_ctx = ctx.read_ctx();
    let mappings = collect_block_ranges(&read_ctx, inode, lblock_start, lblock_end - lblock_start)?;
    Ok((lblock_start as usize, mappings))
}

// =====================================================================
// fallocate 入口三件套（Phase 3 Task 4）。
//
// 安全复刻 ext4_rs `ext4_impls/file.rs`：`allocate_range`（:1415）、`zero_range`（:1487）、
// `punch_hole_keep_size`（:1520）、`write_zeros_at`（:1540）。全建在 Task 3 的写映射
// （`ensure_write_range_mapped` 含 unwritten→written 转换）+ 读侧 `collect_block_ranges`
// 之上，**不持全局锁**、不重新实现 `convert_unwritten_span`。
//
// **语义（parity-first，逐字节复刻）：**
// - `allocate_range`：`len==0→空`；EFBIG 守卫（`offset+len` 溢出或 `> EXT4_MAX_FILE_SIZE`）；
//   预扫记录「曾经 hole/unwritten」逻辑块 → 映射（分配 + unwritten 转 written）→ 把那些块
//   **零填**（转 written 后必须读零）；`!keep_size && range_end>file_size` 才长 i_size；
//   返回 `[lblock_start, lblock_end)` 的 coalesce 映射向量。
// - `zero_range` = `allocate_range` 后再零写可见区：`zero_end = keep_size ? min(range_end,
//   file_size) : range_end`，零写 `[offset, zero_end)`，返回 zero_len。
// - `punch_hole_keep_size`：**只零可见字节、不释放块**（i_blocks 不变——名副其实 keep_size；
//   这是刻意简化、非真 FALLOC_FL_PUNCH_HOLE，登记 bug.md）；零写 `[offset,
//   min(offset+len, file_size))`，返回 zero_len。
// - Collapse / Insert / Unshare 在集成层（fs.rs:5868）即 `EOPNOTSUPP`——core 不提供入口。
// =====================================================================

/// `allocate_range`：为 `[offset, offset+len)` 分配/转换块，曾经 hole/unwritten 的块零填；
/// `keep_size` 控制是否长 i_size。返回该逻辑区间的 coalesce 映射向量。
/// 逐字节复刻 ext4_rs `allocate_range`（file.rs:1415）。
pub(in crate::fs::ext4) fn allocate_range(
    ctx: &WriteCtx,
    alloc: &mut dyn BlockAlloc,
    inode: &mut Inode,
    offset: usize,
    len: usize,
    keep_size: bool,
) -> Result<Vec<SimpleBlockRange>> {
    let block_size = ctx.block_size;
    if len == 0 {
        return Ok(Vec::new());
    }

    let file_size = inode.size();
    let range_end = offset
        .checked_add(len)
        .ok_or_else(|| Error::with_message(Errno::EFBIG, "fallocate range end overflow"))?;
    // PARITY: EXT4_MAX_FILE_SIZE 上界 EFBIG（ext4_rs file.rs:1432；部分闭合 BUG-12——
    //   唯独 allocate_range 有这道，write_at/prepare 仍缺）。
    if range_end > EXT4_MAX_FILE_SIZE as usize {
        return Err(Error::with_message(Errno::EFBIG, "file size too large"));
    }

    let lblock_start = u32::try_from(offset / block_size)
        .map_err(|_| Error::with_message(Errno::EFBIG, "lblock start too big"))?;
    let lblock_end = u32::try_from((range_end - 1) / block_size + 1)
        .map_err(|_| Error::with_message(Errno::EFBIG, "lblock end too big"))?;

    // PARITY: unwritten（预分配）块与 hole 一起记入零填表——映射步把它们转 written，
    //   暴露设备里的旧内容，必须读回零（ext4_rs file.rs:1444 同语义）。
    let mut previously_hole = Vec::new();
    for lblock in lblock_start..lblock_end {
        match get_pblock_idx_state(ctx, inode, lblock) {
            Ok((_, unwritten)) => {
                if unwritten {
                    previously_hole.push(lblock);
                }
            }
            Err(e) if e.error() == Errno::ENOENT => previously_hole.push(lblock),
            Err(e) => return Err(e),
        }
    }

    let mut start_bgid = initial_write_alloc_bgid(ctx, inode, lblock_start, lblock_end);
    let allocated_total =
        ensure_write_range_mapped(ctx, alloc, inode, &mut start_bgid, lblock_start, lblock_end)?;

    if !previously_hole.is_empty() {
        let zero_block = vec![0u8; block_size];
        for lblock in previously_hole {
            let pblock = get_pblock_idx(ctx, inode, lblock)?;
            let pblock = usize::try_from(pblock)
                .map_err(|_| Error::with_message(Errno::EFBIG, "pblock too big"))?;
            let block_offset = pblock
                .checked_mul(block_size)
                .ok_or_else(|| Error::with_message(Errno::EFBIG, "block offset overflow"))?;
            ctx.data_writer.write_at(block_offset, zero_block.as_slice());
        }
    }

    // PARITY: 仅 `!keep_size && range_end>file_size` 才长 i_size；否则只在分配过时写回
    //   （ext4_rs file.rs:1477）。
    if !keep_size && range_end > file_size as usize {
        inode.set_size(range_end as u64);
        write_back_inode(ctx.writer, ctx.reader, ctx.sb, inode)?;
    } else if allocated_total > 0 {
        write_back_inode(ctx.writer, ctx.reader, ctx.sb, inode)?;
    }

    let read_ctx = ctx.read_ctx();
    collect_block_ranges(&read_ctx, inode, lblock_start, lblock_end - lblock_start)
}

/// `zero_range`：`allocate_range`(`keep_size`) 后零写可见区，返回零写字节数。
/// 逐字节复刻 ext4_rs `zero_range`（file.rs:1487）。
pub(in crate::fs::ext4) fn zero_range(
    ctx: &WriteCtx,
    alloc: &mut dyn BlockAlloc,
    inode: &mut Inode,
    offset: usize,
    len: usize,
    keep_size: bool,
) -> Result<usize> {
    if len == 0 {
        return Ok(0);
    }

    allocate_range(ctx, alloc, inode, offset, len, keep_size)?;

    let file_size = inode.size() as usize;
    let range_end = offset
        .checked_add(len)
        .ok_or_else(|| Error::with_message(Errno::EFBIG, "zero_range end overflow"))?;
    // PARITY: zero_end = keep_size ? min(range_end, file_size) : range_end（ext4_rs file.rs:1505）。
    let zero_end = if keep_size {
        range_end.min(file_size)
    } else {
        range_end
    };
    if offset >= zero_end {
        return Ok(0);
    }

    let zero_len = zero_end - offset;
    write_zeros_at(ctx, alloc, inode, offset, zero_len)?;
    Ok(zero_len)
}

/// `punch_hole_keep_size`：**只零可见字节、不释放块**（i_blocks 不变），返回零写字节数。
/// 逐字节复刻 ext4_rs `punch_hole_keep_size`（file.rs:1520）。
///
/// PARITY: 刻意简化——名副其实 keep_size，只把 `[offset, min(offset+len, file_size))` 写零，
/// **不**回收物理块（非真 FALLOC_FL_PUNCH_HOLE；登记 bug.md）。
pub(in crate::fs::ext4) fn punch_hole_keep_size(
    ctx: &WriteCtx,
    alloc: &mut dyn BlockAlloc,
    inode: &mut Inode,
    offset: usize,
    len: usize,
) -> Result<usize> {
    if len == 0 {
        return Ok(0);
    }

    let file_size = inode.size() as usize;
    if offset >= file_size {
        return Ok(0);
    }
    let range_end = offset
        .checked_add(len)
        .ok_or_else(|| Error::with_message(Errno::EFBIG, "punch range end overflow"))?
        .min(file_size);
    let zero_len = range_end - offset;
    write_zeros_at(ctx, alloc, inode, offset, zero_len)?;
    Ok(zero_len)
}

/// `write_zeros_at`：按 64K chunk 循环 `write_at` 零写 `[offset, offset+len)`。
/// 逐字节复刻 ext4_rs `write_zeros_at`（file.rs:1540）。
fn write_zeros_at(
    ctx: &WriteCtx,
    alloc: &mut dyn BlockAlloc,
    inode: &mut Inode,
    offset: usize,
    len: usize,
) -> Result<()> {
    const ZERO_WRITE_CHUNK: usize = 64 * 1024;

    let zero_buf = vec![0u8; ZERO_WRITE_CHUNK.min(len)];
    let mut written = 0usize;
    while written < len {
        let chunk_len = (len - written).min(zero_buf.len());
        write_at(ctx, alloc, inode, offset + written, &zero_buf[..chunk_len])?;
        written += chunk_len;
    }
    Ok(())
}

/// extent 删除上界（逻辑块号）。= ext4_rs `EXT_MAX_BLOCKS`（ext4_defs/consts.rs:26）= `u32::MAX`。
const EXT_MAX_BLOCKS: u32 = u32::MAX;

/// 文件截断到 `new_size`。逐字节复刻 ext4_rs `truncate_inode`（ext4_impls/file.rs:1904）。
///
/// - `old == new` → 空操作（EOK）；
/// - `old < new`（增长）→ EFBIG 守卫（`new_size > EXT4_MAX_FILE_SIZE`）+ **稀疏** set_size
///   + write_back（只推进 i_size、留 hole 不映射）；
/// - `old > new`（缩小）→ ① 算 new/old 块数与 diff；② `new_size % bs != 0` 时**零填尾分块**
///   （unwritten 尾不动、written 尾 RMW 零填 `[tail_offset..]`、hole 不动）；③ `diff > 0` →
///   `extent_remove_space(new_blocks_cnt, EXT_MAX_BLOCKS)`；④ set_size + write_back。
///
/// PARITY（控制器澄清）：core `truncate_inode` 本身**干净**——bug.md 的「truncate size-clamp」
/// 实指集成层 `write_page_async` 的 PageCache 写回 clamp（C4），不在本函数内。这里只逐字节
/// 复刻这个干净的 truncate。**仅 extent 缩路径**；legacy（间接映射）缩不在本 Phase 范围。
pub(in crate::fs::ext4) fn truncate_inode(
    ctx: &WriteCtx,
    alloc: &mut dyn BlockAlloc,
    inode: &mut Inode,
    new_size: u64,
) -> Result<()> {
    let block_size = ctx.block_size as u64;
    let old_size = inode.size();

    if old_size == new_size {
        return Ok(());
    }
    if old_size < new_size {
        // 增长：EFBIG 守卫 + 稀疏 set_size + write_back（不分配、留 hole）。
        if new_size > EXT4_MAX_FILE_SIZE {
            return Err(Error::with_message(Errno::EFBIG, "file size too large"));
        }
        inode.set_size(new_size);
        write_back_inode(ctx.writer, ctx.reader, ctx.sb, inode)?;
        return Ok(());
    }

    // 缩小：算 new/old 块数（ceil）与 diff。
    let new_blocks_cnt = ((new_size + block_size - 1) / block_size) as u32;
    let old_blocks_cnt = ((old_size + block_size - 1) / block_size) as u32;
    let diff_blocks_cnt = old_blocks_cnt - new_blocks_cnt;

    // 尾分块零填（new_size 落在块中部时）。
    let tail_offset = (new_size % block_size) as usize;
    if tail_offset != 0 {
        let tail_lblock = u32::try_from(new_size / block_size)
            .map_err(|_| Error::with_message(Errno::EFBIG, "tail lblock overflow"))?;
        match extents::get_pblock_idx_state(ctx.reader, ctx.sb, inode, tail_lblock) {
            // unwritten 尾本就读零——RMW 会把未初始化盘内容写实，故不动。
            Ok(Some((_, true))) => {}
            Ok(Some((pblock, false))) => {
                // written 尾：RMW 把 `[tail_offset..]` 清零。
                let block_offset = usize::try_from(pblock)
                    .map_err(|_| Error::with_message(Errno::EFBIG, "tail pblock overflow"))?
                    .checked_mul(block_size as usize)
                    .ok_or_else(|| Error::with_message(Errno::EFBIG, "tail offset overflow"))?;
                let mut block_data = vec![0u8; block_size as usize];
                ctx.reader.read_at(block_offset, block_data.as_mut_slice());
                block_data[tail_offset..].fill(0);
                ctx.data_writer.write_at(block_offset, block_data.as_slice());
            }
            // hole 尾：不动（ext4_rs ENOENT 分支）。
            Ok(None) => {}
            Err(e) => return Err(e),
        }
    }

    if diff_blocks_cnt > 0 {
        extents::extent_remove_space(ctx, alloc, inode, new_blocks_cnt, EXT_MAX_BLOCKS)?;
    }

    inode.set_size(new_size);
    write_back_inode(ctx.writer, ctx.reader, ctx.sb, inode)?;

    Ok(())
}
