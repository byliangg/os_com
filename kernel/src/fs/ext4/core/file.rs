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
pub(super) fn write_at(
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
pub(super) fn prepare_write_at(
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
pub(super) fn allocate_range(
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
pub(super) fn zero_range(
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
pub(super) fn punch_hole_keep_size(
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
pub(super) fn truncate_inode(
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

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::{
        allocate_range, map_blocks, plan_direct_read, prepare_write_at, punch_hole_keep_size,
        read_at, write_at, zero_range, ReadCtx,
    };
    use crate::fs::ext4::core::balloc::{BlockAllocator, InodeAllocCtx};
    use crate::fs::ext4::core::block_group::RawGroupDescriptor;
    use crate::fs::ext4::core::diff_harness::{assert_disk_eq, DirectMetadataWriter, MemDisk};
    use crate::fs::ext4::core::extents::{
        find_extent, get_pblock_idx_state, insert_extent, BlockAlloc, RawExtent, RawExtentHeader,
        WriteCtx,
    };
    use crate::fs::ext4::core::inode::{load_inode, Inode};
    use crate::fs::ext4::core::io::BlockReader;
    use crate::fs::ext4::core::superblock::RawSuperblock;
    use crate::fs::ext4::core::test_util::{EXT4_IMAGE, EXT4_MULTIGROUP_IMAGE, EXT4_NOCSUM_IMAGE};
    use crate::fs::ext4::core::types::Ext4Fsblk;
    // 核心路径返回 `crate::Result`（= ostd::Result 别名）；显式引入 core prelude 的
    // `Result`/`Error`/`Errno` 以免 `use ostd::prelude::*` 引入的同名项遮蔽造成歧义。
    use crate::prelude::{Error, Errno, Result};
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

    // =================================================================
    // Phase 3 Task 3：extent 写 + 文件写差分。
    //
    // 差分骨架（ambiguity #1/#4）：
    // - 新旧各持一张独立 `MemDisk`（同一份 setup 字节）；旧侧 `ext4_rs::Ext4::write_at`，
    //   新侧 core `WriteCtx::write_at`；每步后 `assert_disk_eq` **全盘逐字节**对拍——extent
    //   非根块在数据区，snapshot_meta 覆盖不到，全盘比对一次抓全。
    // - **每步重建**核心分配器 + WriteCtx（从盘重读 SB / inode），与旧侧共享同一份盘字节
    //   语义（旧侧 Ext4 每次 open 也从盘重读）。
    // =================================================================

    /// 把 Phase-2 `BlockAllocator` + `InodeAllocCtx` 适配成 core 写半部要的 [`BlockAlloc`]。
    /// 每次分配后把 `InodeAllocCtx` 累加出的 i_blocks 同步回 `inode.raw.blocks`——与 ext4_rs
    /// 在共享 `Ext4InodeRef` 上累加 `blocks_count` 等价，使后续 write_back_inode 落对值。
    struct CoreAllocAdapter<'a, R: BlockReader, W: crate::fs::ext4::core::metadata_writer::MetadataWriter>
    {
        alloc: BlockAllocator<'a, R, W>,
        ictx: InodeAllocCtx,
    }

    impl<'a, R: BlockReader, W: crate::fs::ext4::core::metadata_writer::MetadataWriter> BlockAlloc
        for CoreAllocAdapter<'a, R, W>
    {
        fn alloc_one(&mut self, inode: &mut Inode) -> Result<Ext4Fsblk> {
            let blk = self.alloc.balloc_alloc_block(&mut self.ictx, None)?;
            inode.set_blocks_count(self.ictx.i_blocks());
            Ok(blk)
        }
        fn alloc_batch(
            &mut self,
            inode: &mut Inode,
            start_bgid: &mut u32,
            count: usize,
        ) -> Result<Vec<Ext4Fsblk>> {
            let v = self
                .alloc
                .balloc_alloc_block_batch(&mut self.ictx, start_bgid, count)?;
            inode.set_blocks_count(self.ictx.i_blocks());
            Ok(v)
        }
        fn free_blocks(&mut self, inode: &mut Inode, start: Ext4Fsblk, count: u32) {
            self.alloc.balloc_free_blocks(&mut self.ictx, start, count);
            // 与 ext4_rs 在共享 inode_ref 上递减 blocks_count 等价：释放后把 ictx 的
            // i_blocks（512B 单位）同步回 Inode，使随后 write_back_inode 落对值。
            inode.set_blocks_count(self.ictx.i_blocks());
        }
    }

    /// 用 ext4_rs 在一张基准盘上创建一个空 extent 文件（写 inode + 目录项），返回 (字节, ino)。
    /// 该 setup 是被测代码之外的共享前置，两侧从同一份字节起跑。
    fn make_base_disk_with_empty_file(image: &[u8], name: &str) -> (Vec<u8>, u32) {
        let base = MemDisk::from_image(image);
        let ext4 = ext4_rs::Ext4::open(Arc::new(base.clone()));
        // 在根目录下建一个常规文件（extent-mapped，空树）。
        let child = ext4
            .create(ext4_rs::EXT4_ROOT_INODE, name, 0o100644)
            .expect("create empty extent file");
        let ino = child.inode_num;
        let bytes = base.backing().lock().clone();
        (bytes, ino)
    }

    /// 在 `disk` 上构造一个新核写上下文（reader=disk, metadata writer=直写, data writer=disk,
    /// sb=从盘重读），返回 (sb, writer)。WriteCtx 与 adapter 由调用方据此组装（借用期所限）。
    fn new_sb_and_writer(disk: &MemDisk) -> (RawSuperblock, DirectMetadataWriter) {
        let sb = read_sb(disk);
        let bs = sb.block_size();
        let writer = DirectMetadataWriter::new(disk.clone(), bs);
        (sb, writer)
    }

    /// 在 `new_disk` 上跑一次 core `write_at`：从盘重建 SB / 分配器 / WriteCtx / inode，写后
    /// 返回写出字节数。i_blocks 初值取 inode 当前盘上值（每步重建，counter 一致）。
    fn core_write_at(new_disk: &MemDisk, ino: u32, off: usize, buf: &[u8]) -> Result<usize> {
        let (sb, writer) = new_sb_and_writer(new_disk);
        let alloc = BlockAllocator::new(sb, new_disk, &writer);
        let mut inode = load_inode(new_disk, &sb, ino)?;
        let ictx = InodeAllocCtx::new(inode.blocks_count());
        let mut adapter = CoreAllocAdapter { alloc, ictx };
        let ctx = WriteCtx::new(new_disk, &writer, new_disk, &sb);
        write_at(&ctx, &mut adapter, &mut inode, off, buf)
    }

    /// 同上但走 `prepare_write_at`，返回 (lblock_start, 映射向量)。
    fn core_prepare_write_at(
        new_disk: &MemDisk,
        ino: u32,
        off: usize,
        len: usize,
    ) -> Result<(usize, Vec<super::SimpleBlockRange>)> {
        let (sb, writer) = new_sb_and_writer(new_disk);
        let alloc = BlockAllocator::new(sb, new_disk, &writer);
        let mut inode = load_inode(new_disk, &sb, ino)?;
        let ictx = InodeAllocCtx::new(inode.blocks_count());
        let mut adapter = CoreAllocAdapter { alloc, ictx };
        let ctx = WriteCtx::new(new_disk, &writer, new_disk, &sb);
        prepare_write_at(&ctx, &mut adapter, &mut inode, off, len)
    }

    /// 一步写差分：旧侧 ext4_rs.write_at，新侧 core write_at，比 (A) 返回字节 / Ok-Err；
    /// (B) 全盘逐字节；(C) i_blocks/size（藏在 inode 表里，全盘比对已覆盖）。
    fn diff_write_step(old_disk: &MemDisk, new_disk: &MemDisk, ino: u32, off: usize, buf: &[u8]) {
        let ext4 = ext4_rs::Ext4::open(Arc::new(old_disk.clone()));
        let old_ret = ext4.write_at(ino, off, buf);
        let new_ret = core_write_at(new_disk, ino, off, buf);
        match (&old_ret, &new_ret) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "write_at returned bytes mismatch off={off}"),
            (Err(_), Err(_)) => {}
            _ => panic!("write_at ok/err mismatch off={off}: old={old_ret:?} new={new_ret:?}"),
        }
        // (B) 全盘逐字节（含 inode 表 i_blocks/size + extent 块 + 数据块 + 位图 + GDT + SB）。
        assert_disk_eq(old_disk, new_disk);
    }

    /// extent 写差分主测试：脚本化写序列（append / sparse hole / cross-extent / merge /
    /// create_new_leaf / ext_grow_indepth），每步全盘对拍；并触发 ENOTSUP（两侧同 Err）。
    #[ktest]
    fn extent_write_parity() {
        let (base_bytes, ino) = make_base_disk_with_empty_file(EXT4_IMAGE, "twf");
        let old_disk = MemDisk::from_image(&base_bytes);
        let new_disk = MemDisk::from_image(&base_bytes);
        let bs = read_sb(&new_disk).block_size();

        // ① 顺序 append：连续多块写（触发 root extent 插入 + 合并 + 预分配尾）。
        diff_write_step(&old_disk, &new_disk, ino, 0, &vec![0xA1u8; bs]); // lblock 0
        diff_write_step(&old_disk, &new_disk, ino, bs, &vec![0xA2u8; bs]); // lblock 1（命中 unwritten 尾 → convert + merge）
        diff_write_step(&old_disk, &new_disk, ino, 2 * bs, &vec![0xA3u8; bs]); // lblock 2

        // ② 稀疏写造 hole：跳到远逻辑块写（中间留 hole）。
        diff_write_step(&old_disk, &new_disk, ino, 100 * bs, &vec![0xB1u8; bs]); // lblock 100

        // ③ 跨 extent 多块写（一次写多块，跨已有 extent 边界）。
        diff_write_step(&old_disk, &new_disk, ino, 50 * bs, &vec![0xC1u8; 4 * bs]);

        // ④ 触发树加深：多个物理不连续的离散 extent，逼满 root（4 槽）后 ext_grow_indepth。
        //    每次写一个远离前者的逻辑块，制造不可合并的独立 extent。
        for k in 0..8u32 {
            let lblk = 200 + k * 100; // 互不相邻、各成独立 extent
            diff_write_step(&old_disk, &new_disk, ino, lblk as usize * bs, &vec![0xD0 + k as u8; bs]);
        }

        // ⑤ depth>0 读回（补 Task 2 deferred 覆盖）：此时树已 ext_grow_indepth 成 depth>0；
        //    逐 lblock 对拍 get_pblock_idx_state，并对一段做 map_blocks 向量对拍。
        {
            let sb = read_sb(&new_disk);
            let ext4 = ext4_rs::Ext4::open(Arc::new(old_disk.clone()));
            let new_inode = load_inode(&new_disk, &sb, ino).expect("load new inode");
            let old_ref = ext4.get_inode_ref(ino);
            // 确认确实成了 depth>0 深树（root header depth）。
            let root = new_inode.i_block_bytes();
            let depth = RawExtentHeader::from_bytes(&root[..size_of::<RawExtentHeader>()]).depth;
            assert!(depth > 0, "expected depth>0 tree after grow_indepth, got {depth}");

            for lblock in 0..900u32 {
                let new_res = get_pblock_idx_state(&new_disk, &sb, &new_inode, lblock);
                let old_res = ext4.get_pblock_idx_state(&old_ref, lblock);
                match (new_res, old_res) {
                    (Ok(Some((np, nu))), Ok((op, ou))) => {
                        assert_eq!(np, op, "depth>0 lblock={lblock} pblock");
                        assert_eq!(nu, ou, "depth>0 lblock={lblock} unwritten");
                    }
                    (Ok(None), Err(e)) => {
                        assert_eq!(
                            e.error(),
                            ext4_rs::Errno::ENOENT,
                            "depth>0 lblock={lblock}: new hole but old err != ENOENT"
                        );
                    }
                    (nr, or) => panic!(
                        "depth>0 lblock={lblock} divergence: new={:?} old_ok={:?}",
                        nr.as_ref().map(|o| o.is_some()),
                        or.is_ok()
                    ),
                }
            }
            // map_blocks 向量对拍（0..900）。
            let ctx = ReadCtx::new(&new_disk, &sb);
            let new_mb = map_blocks(&ctx, &new_inode, 0, 900).expect("new map_blocks");
            let old_mb = ext4.map_blocks(ino, 0, 900).expect("old map_blocks");
            let new_t: Vec<(u32, u64, u32)> =
                new_mb.iter().map(|r| (r.lblock, r.pblock, r.len)).collect();
            let old_t: Vec<(u32, u64, u32)> =
                old_mb.iter().map(|r| (r.lblock, r.pblock, r.len)).collect();
            assert_eq!(new_t, old_t, "depth>0 map_blocks vector mismatch");
        }

        // ⑥ prepare_write_at 差分：对已分配区做一次 prepare（全已分配快路径），全盘比对。
        {
            let old_disk2 = MemDisk::from_image(&base_bytes);
            let new_disk2 = MemDisk::from_image(&base_bytes);
            // 先在两侧写一块（lblock 0），制造已分配 + 预分配尾。
            diff_write_step(&old_disk2, &new_disk2, ino, 0, &vec![0xE1u8; bs]);
            // 对 lblock 0（已 written）做 prepare_write_at；旧侧返回 Vec、新侧返回 (start, Vec)。
            let ext4 = ext4_rs::Ext4::open(Arc::new(old_disk2.clone()));
            let old_runs = ext4.prepare_write_at(ino, 0, bs).expect("old prepare");
            let (start, new_runs) = core_prepare_write_at(&new_disk2, ino, 0, bs).expect("new prepare");
            assert_eq!(start, 0, "prepare lblock_start");
            let new_t: Vec<(u32, u64, u32)> =
                new_runs.iter().map(|r| (r.lblock, r.pblock, r.len)).collect();
            let old_t: Vec<(u32, u64, u32)> =
                old_runs.iter().map(|r| (r.lblock, r.pblock, r.len)).collect();
            assert_eq!(new_t, old_t, "prepare_write_at mapping mismatch");
            assert_disk_eq(&old_disk2, &new_disk2);
        }
    }

    /// 写后读回字节一致：写若干段后，新侧 core read_at vs 旧侧 ext4_rs read_at 全字节对拍，
    /// 并全盘对拍。覆盖 append / 中段覆写 / 跨块非对齐写后读。
    #[ktest]
    fn file_write_then_read_parity() {
        let (base_bytes, ino) = make_base_disk_with_empty_file(EXT4_IMAGE, "rwf");
        let old_disk = MemDisk::from_image(&base_bytes);
        let new_disk = MemDisk::from_image(&base_bytes);
        let bs = read_sb(&new_disk).block_size();

        // 一串写：整块 append、跨块非对齐、覆写已写区、稀疏跳写。
        let payloads: Vec<(usize, Vec<u8>)> = vec![
            (0, vec![0x11u8; bs]),                 // lblock 0 整块
            (bs + 7, vec![0x22u8; bs + 13]),       // 起始非对齐 + 跨块
            (3, vec![0x33u8; 20]),                 // 覆写 lblock 0 中段
            (10 * bs, vec![0x44u8; 100]),          // 稀疏：lblock 10 部分
        ];
        for (off, buf) in &payloads {
            diff_write_step(&old_disk, &new_disk, ino, *off, buf);
        }

        // 写后读回逐字节对拍（覆盖 hole / 已写 / 覆写后）。
        let sb = read_sb(&new_disk);
        let ctx = ReadCtx::new(&new_disk, &sb);
        let ext4 = ext4_rs::Ext4::open(Arc::new(old_disk.clone()));
        let new_inode = load_inode(&new_disk, &sb, ino).expect("load inode");
        let file_size = new_inode.size() as usize;

        let read_cases: Vec<(usize, usize)> = vec![
            (0, bs),
            (0, file_size),
            (3, 25),
            (bs, 2 * bs),
            (5 * bs, 6 * bs),    // 含 hole 段
            (10 * bs, 200),
            (0, file_size + bs), // 超长读截断
        ];
        for (off, len) in read_cases {
            let mut new_buf = vec![0xCCu8; len];
            let mut old_buf = vec![0xCCu8; len];
            let new_n = read_at(&ctx, &new_inode, off, &mut new_buf).expect("new read_at");
            let old_n = ext4.read_at(ino, off, &mut old_buf).expect("old read_at");
            assert_eq!(new_n, old_n, "read count mismatch off={off} len={len}");
            assert_eq!(new_buf, old_buf, "read bytes mismatch off={off} len={len}");
        }

        // 全盘逐字节（已在每步 diff_write_step 比过，这里再确认终态）。
        assert_disk_eq(&old_disk, &new_disk);
    }

    // =================================================================
    // Phase 3 Task 4：fallocate 入口三件套差分。
    //
    // 旧侧 ext4_rs `allocate_range`/`zero_range`/`punch_hole_keep_size`，新侧 core 同名 fn。
    // 每步从盘重建 ctx/分配器/inode；每步后 `assert_disk_eq` **全盘逐字节**（覆盖 inode 表
    // i_blocks/size + extent 块 + 数据块 + 位图 + GDT + SB）。
    // =================================================================

    /// 在 `new_disk` 上跑一次 core `allocate_range`，返回映射向量。每步重建分配器/WriteCtx/inode。
    fn core_allocate_range(
        new_disk: &MemDisk,
        ino: u32,
        off: usize,
        len: usize,
        keep_size: bool,
    ) -> Result<Vec<super::SimpleBlockRange>> {
        let (sb, writer) = new_sb_and_writer(new_disk);
        let alloc = BlockAllocator::new(sb, new_disk, &writer);
        let mut inode = load_inode(new_disk, &sb, ino)?;
        let ictx = InodeAllocCtx::new(inode.blocks_count());
        let mut adapter = CoreAllocAdapter { alloc, ictx };
        let ctx = WriteCtx::new(new_disk, &writer, new_disk, &sb);
        allocate_range(&ctx, &mut adapter, &mut inode, off, len, keep_size)
    }

    /// 在 `new_disk` 上跑一次 core `zero_range`，返回零写字节数。
    fn core_zero_range(
        new_disk: &MemDisk,
        ino: u32,
        off: usize,
        len: usize,
        keep_size: bool,
    ) -> Result<usize> {
        let (sb, writer) = new_sb_and_writer(new_disk);
        let alloc = BlockAllocator::new(sb, new_disk, &writer);
        let mut inode = load_inode(new_disk, &sb, ino)?;
        let ictx = InodeAllocCtx::new(inode.blocks_count());
        let mut adapter = CoreAllocAdapter { alloc, ictx };
        let ctx = WriteCtx::new(new_disk, &writer, new_disk, &sb);
        zero_range(&ctx, &mut adapter, &mut inode, off, len, keep_size)
    }

    /// 在 `new_disk` 上跑一次 core `punch_hole_keep_size`，返回零写字节数。
    fn core_punch_hole(
        new_disk: &MemDisk,
        ino: u32,
        off: usize,
        len: usize,
    ) -> Result<usize> {
        let (sb, writer) = new_sb_and_writer(new_disk);
        let alloc = BlockAllocator::new(sb, new_disk, &writer);
        let mut inode = load_inode(new_disk, &sb, ino)?;
        let ictx = InodeAllocCtx::new(inode.blocks_count());
        let mut adapter = CoreAllocAdapter { alloc, ictx };
        let ctx = WriteCtx::new(new_disk, &writer, new_disk, &sb);
        punch_hole_keep_size(&ctx, &mut adapter, &mut inode, off, len)
    }

    /// 一步 allocate_range 差分：旧侧 ext4_rs.allocate_range，新侧 core，比 (A) 映射向量 /
    /// Ok-Err（EFBIG 别 expect）；(B) 全盘逐字节（含 i_blocks/size）。
    fn diff_allocate_step(
        old_disk: &MemDisk,
        new_disk: &MemDisk,
        ino: u32,
        off: usize,
        len: usize,
        keep_size: bool,
    ) {
        let ext4 = ext4_rs::Ext4::open(Arc::new(old_disk.clone()));
        let old_ret = ext4.allocate_range(ino, off, len, keep_size);
        let new_ret = core_allocate_range(new_disk, ino, off, len, keep_size);
        match (&old_ret, &new_ret) {
            (Ok(old_v), Ok(new_v)) => {
                let old_t: Vec<(u32, u64, u32)> =
                    old_v.iter().map(|r| (r.lblock, r.pblock, r.len)).collect();
                let new_t: Vec<(u32, u64, u32)> =
                    new_v.iter().map(|r| (r.lblock, r.pblock, r.len)).collect();
                assert_eq!(
                    new_t, old_t,
                    "allocate_range mapping mismatch off={off} len={len} keep={keep_size}"
                );
            }
            (Err(_), Err(_)) => {}
            _ => panic!(
                "allocate_range ok/err mismatch off={off} len={len}: old_ok={} new_ok={}",
                old_ret.is_ok(),
                new_ret.is_ok()
            ),
        }
        assert_disk_eq(old_disk, new_disk);
    }

    /// 一步 zero_range 差分：返回 zero_len + Ok/Err + 全盘逐字节。
    fn diff_zero_step(
        old_disk: &MemDisk,
        new_disk: &MemDisk,
        ino: u32,
        off: usize,
        len: usize,
        keep_size: bool,
    ) {
        let ext4 = ext4_rs::Ext4::open(Arc::new(old_disk.clone()));
        let old_ret = ext4.zero_range(ino, off, len, keep_size);
        let new_ret = core_zero_range(new_disk, ino, off, len, keep_size);
        match (&old_ret, &new_ret) {
            (Ok(a), Ok(b)) => assert_eq!(
                a, b,
                "zero_range zero_len mismatch off={off} len={len} keep={keep_size}"
            ),
            (Err(_), Err(_)) => {}
            _ => panic!(
                "zero_range ok/err mismatch off={off} len={len}: old={old_ret:?} new={new_ret:?}"
            ),
        }
        assert_disk_eq(old_disk, new_disk);
    }

    /// 一步 punch_hole_keep_size 差分：返回 zero_len + Ok/Err + 全盘逐字节。
    /// 全盘比对覆盖「i_blocks 不变」（punch 只零不释放——名副其实 keep_size）。
    fn diff_punch_step(
        old_disk: &MemDisk,
        new_disk: &MemDisk,
        ino: u32,
        off: usize,
        len: usize,
    ) {
        let ext4 = ext4_rs::Ext4::open(Arc::new(old_disk.clone()));
        let old_ret = ext4.punch_hole_keep_size(ino, off, len);
        let new_ret = core_punch_hole(new_disk, ino, off, len);
        match (&old_ret, &new_ret) {
            (Ok(a), Ok(b)) => {
                assert_eq!(a, b, "punch zero_len mismatch off={off} len={len}")
            }
            (Err(_), Err(_)) => {}
            _ => panic!(
                "punch ok/err mismatch off={off} len={len}: old={old_ret:?} new={new_ret:?}"
            ),
        }
        assert_disk_eq(old_disk, new_disk);
    }

    /// 读回逐字节对拍（新侧 core read_at vs 旧侧 ext4_rs read_at），不改盘。
    fn assert_read_eq(old_disk: &MemDisk, new_disk: &MemDisk, ino: u32, off: usize, len: usize) {
        let sb = read_sb(new_disk);
        let ctx = ReadCtx::new(new_disk, &sb);
        let ext4 = ext4_rs::Ext4::open(Arc::new(old_disk.clone()));
        let new_inode = load_inode(new_disk, &sb, ino).expect("load inode");
        let mut new_buf = vec![0xEEu8; len];
        let mut old_buf = vec![0xEEu8; len];
        let new_n = read_at(&ctx, &new_inode, off, &mut new_buf).expect("new read_at");
        let old_n = ext4.read_at(ino, off, &mut old_buf).expect("old read_at");
        assert_eq!(new_n, old_n, "read count mismatch off={off} len={len}");
        assert_eq!(new_buf, old_buf, "read bytes mismatch off={off} len={len}");
    }

    /// fallocate 差分主测试。序列（每步全盘逐字节 + 返回值对拍）：
    /// ① `allocate_range`(keep_size=false) 造 unwritten 区 → ② `read_at` 读得**零**
    ///    （补 Task 2 deferred 的 unwritten 读零覆盖）→ ③ 部分写（`write_at`）触发
    ///    `convert_unwritten_span` 分裂 → ④ 跨 unwritten 写。
    /// 覆盖：keep_size 真/假（i_size 长不长，由全盘比对捕获）、post-EOF 可见性、
    /// `zero_range` keep_size 截断、**punch 只零不释放（i_blocks 不变）**、
    /// Collapse/Insert/Unshare 在集成层即 EOPNOTSUPP（core 无入口，不在 core 测）。
    #[ktest]
    fn unwritten_fallocate_parity() {
        let (base_bytes, ino) = make_base_disk_with_empty_file(EXT4_IMAGE, "ufp");
        let old_disk = MemDisk::from_image(&base_bytes);
        let new_disk = MemDisk::from_image(&base_bytes);
        let bs = read_sb(&new_disk).block_size();

        // ① allocate_range(keep_size=false)：在空文件上为 [0, 8*bs) 分配，i_size 长到 8*bs。
        //    单块写预分配尾使部分块成 unwritten；但 allocate_range 把请求区零填成可读零。
        diff_allocate_step(&old_disk, &new_disk, ino, 0, 8 * bs, false);

        // ② 读回 [0, 8*bs)：全零（hole/unwritten 都读零）。补 Task 2 deferred 的 unwritten 读零。
        assert_read_eq(&old_disk, &new_disk, ino, 0, 8 * bs);

        // ③ keep_size=true：为 [16*bs, 20*bs) 预分配但不长 i_size（区在 EOF 之外）。
        //    全盘比对捕获「i_size 不变」；随后 read_at 读这段在 EOF 之外应为 0（被 clamp）。
        diff_allocate_step(&old_disk, &new_disk, ino, 16 * bs, 4 * bs, true);
        assert_read_eq(&old_disk, &new_disk, ino, 16 * bs, 4 * bs); // EOF 之外 → 读 0

        // ④ 部分写：往 [bs+3, ...) 写 (bs+13) 字节，触发 unwritten→written 分裂转换 +
        //    convert_unwritten_span。全盘比对 + 读回对拍。
        diff_write_step(&old_disk, &new_disk, ino, bs + 3, &vec![0x55u8; bs + 13]);
        assert_read_eq(&old_disk, &new_disk, ino, 0, 8 * bs);

        // ⑤ 跨 unwritten 写：一次写多块跨过仍 unwritten 的逻辑块（4*bs，从 lblock 4 起），
        //    触发跨段的分裂/合并路径。
        diff_write_step(&old_disk, &new_disk, ino, 4 * bs, &vec![0x66u8; 4 * bs]);
        assert_read_eq(&old_disk, &new_disk, ino, 0, 8 * bs);

        // ⑥ post-EOF 可见性：keep_size=false 为 [8*bs, 12*bs) 分配，长 i_size 到 12*bs；
        //    随后这段可见、读回应为 0（曾经 hole/unwritten 已零填）。
        diff_allocate_step(&old_disk, &new_disk, ino, 8 * bs, 4 * bs, false);
        assert_read_eq(&old_disk, &new_disk, ino, 8 * bs, 4 * bs);

        // ⑦ zero_range keep_size=false：把 [0, 2*bs) 写零（区在 i_size 内）。
        diff_zero_step(&old_disk, &new_disk, ino, 0, 2 * bs, false);
        assert_read_eq(&old_disk, &new_disk, ino, 0, 4 * bs);

        // ⑧ zero_range keep_size=true 截断：区 [10*bs, 30*bs) 跨过 i_size(12*bs)——
        //    keep_size 把 zero_end clamp 到 file_size，只零 [10*bs, 12*bs)；返回 zero_len。
        diff_zero_step(&old_disk, &new_disk, ino, 10 * bs, 20 * bs, true);
        assert_read_eq(&old_disk, &new_disk, ino, 10 * bs, 4 * bs);

        // ⑨ punch_hole_keep_size：只零可见字节、不释放块（i_blocks 不变，由全盘比对捕获）。
        //    punch [bs, 3*bs) 在 i_size 内 → 写零 [bs, 3*bs)，返回 2*bs。
        diff_punch_step(&old_disk, &new_disk, ino, bs, 2 * bs);
        assert_read_eq(&old_disk, &new_disk, ino, 0, 4 * bs);

        // ⑩ punch 跨 EOF 截断：punch [11*bs, 40*bs) → range_end clamp 到 file_size(12*bs)，
        //    只零 [11*bs, 12*bs)，返回 bs。
        diff_punch_step(&old_disk, &new_disk, ino, 11 * bs, 29 * bs);
        assert_read_eq(&old_disk, &new_disk, ino, 8 * bs, 4 * bs);

        // ⑪ punch 起点 >= file_size → 返回 0、不改盘。
        diff_punch_step(&old_disk, &new_disk, ino, 100 * bs, bs);

        // ⑫ EFBIG 守卫：offset+len 超 EXT4_MAX_FILE_SIZE（16GiB）→ 两侧同 Err，不改盘。
        diff_allocate_step(
            &old_disk,
            &new_disk,
            ino,
            (16 * 1024 * 1024 * 1024usize) - bs,
            2 * bs,
            false,
        );

        // ⑬ len==0：allocate/zero/punch 都返回空/0、不改盘。
        diff_allocate_step(&old_disk, &new_disk, ino, 0, 0, false);
        diff_zero_step(&old_disk, &new_disk, ino, 0, 0, false);
        diff_punch_step(&old_disk, &new_disk, ino, 0, 0);
    }

    // =================================================================
    // Phase 3 Task 5：extent 树删除 + truncate 差分。
    //
    // 旧侧 ext4_rs `extent_remove_space`/`truncate_inode`，新侧 core 同名 fn。每步从盘重建
    // ctx/分配器/inode；每步后 `assert_disk_eq` **全盘逐字节**（覆盖 inode 表 i_blocks/size
    // + extent 块 + 位图释放 + GDT/SB）。覆盖：缩到 extent 中间 split、删到叶空 idx 删/根塌、
    // pos==0 first_block 传播（传到根 + 中途停两分支）、跨多 extent 释放、truncate grow（稀疏）
    // / shrink（零尾 + remove + free）、乱序插入后删的 position 正确性。
    // =================================================================

    /// 在 `new_disk` 上跑一次 core `extent_remove_space(from, to)`，返回 Result。
    /// 每步重建分配器/WriteCtx/inode，**不**额外 write_back（与 ext4_rs 直接调一致）。
    fn core_extent_remove_space(new_disk: &MemDisk, ino: u32, from: u32, to: u32) -> Result<()> {
        let (sb, writer) = new_sb_and_writer(new_disk);
        let alloc = BlockAllocator::new(sb, new_disk, &writer);
        let mut inode = load_inode(new_disk, &sb, ino)?;
        let ictx = InodeAllocCtx::new(inode.blocks_count());
        let mut adapter = CoreAllocAdapter { alloc, ictx };
        let ctx = WriteCtx::new(new_disk, &writer, new_disk, &sb);
        crate::fs::ext4::core::extents::extent_remove_space(&ctx, &mut adapter, &mut inode, from, to)
    }

    /// 在 `new_disk` 上跑一次 core `truncate_inode(new_size)`，返回 Result。每步重建。
    fn core_truncate(new_disk: &MemDisk, ino: u32, new_size: u64) -> Result<()> {
        let (sb, writer) = new_sb_and_writer(new_disk);
        let alloc = BlockAllocator::new(sb, new_disk, &writer);
        let mut inode = load_inode(new_disk, &sb, ino)?;
        let ictx = InodeAllocCtx::new(inode.blocks_count());
        let mut adapter = CoreAllocAdapter { alloc, ictx };
        let ctx = WriteCtx::new(new_disk, &writer, new_disk, &sb);
        super::truncate_inode(&ctx, &mut adapter, &mut inode, new_size)
    }

    /// 一步 extent_remove_space 差分：旧侧 ext4_rs，新侧 core，比 (A) Ok/Err；(B) 全盘逐字节。
    /// 旧侧调 `extent_remove_space` 后**不** write_back（与 ext4_rs 既有调用约定一致——
    /// 内部触及 root 时已写 inode；i_blocks 递减是否落盘由内部 write_back 决定，两侧同步）。
    fn diff_remove_step(old_disk: &MemDisk, new_disk: &MemDisk, ino: u32, from: u32, to: u32) {
        let ext4 = ext4_rs::Ext4::open(Arc::new(old_disk.clone()));
        let mut old_ref = ext4.get_inode_ref(ino);
        let old_ret = ext4.extent_remove_space(&mut old_ref, from, to);
        let new_ret = core_extent_remove_space(new_disk, ino, from, to);
        match (&old_ret, &new_ret) {
            (Ok(_), Ok(_)) => {}
            (Err(_), Err(_)) => {}
            _ => panic!(
                "extent_remove_space ok/err mismatch from={from} to={to}: old={:?} new={:?}",
                old_ret.is_ok(),
                new_ret.is_ok()
            ),
        }
        assert_disk_eq(old_disk, new_disk);
    }

    /// 一步 truncate_inode 差分：旧侧 ext4_rs（get_inode_ref → truncate_inode），新侧 core，
    /// 比 (A) Ok/Err；(B) 全盘逐字节（含 i_size/i_blocks——truncate 内部 write_back）。
    fn diff_truncate_step(old_disk: &MemDisk, new_disk: &MemDisk, ino: u32, new_size: u64) {
        let ext4 = ext4_rs::Ext4::open(Arc::new(old_disk.clone()));
        let mut old_ref = ext4.get_inode_ref(ino);
        let old_ret = ext4.truncate_inode(&mut old_ref, new_size);
        let new_ret = core_truncate(new_disk, ino, new_size);
        match (&old_ret, &new_ret) {
            (Ok(_), Ok(_)) => {}
            (Err(_), Err(_)) => {}
            _ => panic!(
                "truncate_inode ok/err mismatch new_size={new_size}: old={:?} new={:?}",
                old_ret.is_ok(),
                new_ret.is_ok()
            ),
        }
        assert_disk_eq(old_disk, new_disk);
    }

    /// 取 inode 的 extent 树 root depth（验证确为 depth>0 深树）。
    fn root_depth(disk: &MemDisk, ino: u32) -> u16 {
        let sb = read_sb(disk);
        let inode = load_inode(disk, &sb, ino).expect("load inode for depth");
        let root = inode.i_block_bytes();
        RawExtentHeader::from_bytes(&root[..size_of::<RawExtentHeader>()]).depth
    }

    /// extent 删除 + truncate 主差分。**两棵树，避开参照侧 ext4_rs 的 panic。**
    ///
    /// **参照侧鲁棒性约束（BUG-14 + BUG-15 + BUG-16，已登记 bug.md）**：
    /// - BUG-14：ext4_rs `balloc_free_blocks`（balloc.rs:672）`inode_blocks -= free_cnt*(bs/512)`
    ///   **无符号减法不做下溢保护** → debug 下溢即 panic（不 saturate）。
    /// - BUG-15：ext4_rs `extent_remove_space` 自叶向上循环（extents.rs:1333）在清空 depth>0 树时
    ///   经 `more_to_rm` 触发**重下钻而 `path[depth]` 从未重载** → 同一节点被**重复释放** →
    ///   单次 free 的块数超过当前 i_blocks → 撞 BUG-14 panic。**精确触发层级（叶层 vs index 层 i<depth）
    ///   为近似、待运行时 trace**（见 bug.md BUG-15 caveat）；但「清空 depth>0 多 extent 树
    ///   （含 truncate-shrink-to-0）必 panic」经 ktest 实证为真——故本差分一律避开清空 depth>0 树。
    /// - BUG-16：ext4_rs `extent_remove_space`（extents.rs:1258/1283）对**空叶**（entries_count==0）
    ///   做 `root_extent_at(entries_count - 1)` / `read_offset_as(...*(entries_count-1))` **无下溢守卫**
    ///   → 缩一棵 entries==0 的树（**稀疏 grow 出来的**、或**已清空再缩**）即 panic。**故差分绝不
    ///   shrink/remove 一棵 entries==0 的树**；要覆盖「regrow 后 shrink」改走 **write-then-shrink**
    ///   （先写真实块使 entries>0，再缩）。「shrink 空/稀疏树」差分推迟（仅源码 review）。
    ///
    /// **core 逐字节复刻 BUG-14/15（不退）；差分只喂不会把 ext4_rs 推进 panic 的输入**：
    /// - **Tree A（depth-0，root i_block 持 ≤4 extent，盘上无 node 块）**：无 node-block 释放路径，
    ///   `more_to_rm` 在 depth-0 循环里**根本不被调**（无 index 层）→ 清叶（shrink-to-0）也安全。
    ///   覆盖大头：middle-split、cross-multi-extent free、truncate grow/shrink-边界/shrink-中部
    ///   （written 尾 RMW）/**shrink-to-0**/同尺寸/空树再 grow。
    /// - **Tree B（depth=1，仅 PARTIAL 删，永不清空叶）**：删/缩**只下探到留 ≥1 extent 在叶**，
    ///   故 `ext_remove_idx` 永不触发、node 块永不释放、i_blocks 永不下溢。覆盖 depth>0 遍历删、
    ///   partial free、**pos==0 first_block 向 root index 传播**。每步断言 depth 仍 >0（叶非空）。
    /// - **差分推迟（仅源码 review 覆盖，非测试）**：depth>0 树的 `ext_remove_idx` + 根塌
    ///   （即清空 depth>0 树）——因 ext4_rs BUG-14/15 重复释放 + panic，**无法**逐字节对拍。
    ///   该路径由 reviewer 逐行核（同 Task 2 depth>0 读、Phase 2 跨组释放的差分推迟先例），
    ///   理由见 bug.md BUG-14/15。
    #[ktest]
    fn extent_delete_truncate_parity() {
        let bs = read_sb(&MemDisk::from_image(EXT4_IMAGE)).block_size();

        // =================================================================
        // Tree A — depth-0（root i_block 持 ≤4 extent，无 node 块 → 清叶亦安全）。
        // =================================================================
        let (base_a, ino_a) = make_base_disk_with_empty_file(EXT4_IMAGE, "dta");
        let old_a = MemDisk::from_image(&base_a);
        let new_a = MemDisk::from_image(&base_a);

        // ① 中间删触发 split（middle-remove + 尾段重插，**不释放块**）：造 3 块宽 extent [10,13)，
        //    删严格内侧 [11,11]（first(10)<11 且 11<10+3-1=12）→ 截前段 + 尾段 reinsert。
        diff_write_step(&old_a, &new_a, ino_a, 10 * bs, &vec![0x99u8; 3 * bs]);
        // depth 仍 0（单 extent）。
        assert_eq!(root_depth(&old_a, ino_a), 0, "A: stays depth-0 after one extent");
        diff_remove_step(&old_a, &new_a, ino_a, 11, 11);

        // ② cross-multi-extent free：在 root 里再写 2 个离散 extent（共 ≤4 槽，depth 不增），
        //    删一段跨多个 extent → 仅释放数据块（depth-0 无 node 块）。
        diff_write_step(&old_a, &new_a, ino_a, 40 * bs, &vec![0xA1u8; 2 * bs]); // [40,42)
        diff_write_step(&old_a, &new_a, ino_a, 60 * bs, &vec![0xA2u8; 2 * bs]); // [60,62)
        assert_eq!(root_depth(&old_a, ino_a), 0, "A: still depth-0 (<=4 root extents)");
        // 删 [11,61] 跨 [10,13) 尾段 + [40,42) + [60,62) 头 → 多 extent 部分/整段释放。
        diff_remove_step(&old_a, &new_a, ino_a, 11, 61);

        // ③ truncate（depth-0 树）：grow 稀疏 / shrink 边界 / shrink 中部（written 尾 RMW）/
        //    shrink-to-0（清空 root 叶，depth 仍 0，无 node 块 → 安全）/ 同尺寸 / 空树再 grow。
        let (base_t, ino_t) = make_base_disk_with_empty_file(EXT4_IMAGE, "dtt");
        let old_t = MemDisk::from_image(&base_t);
        let new_t = MemDisk::from_image(&base_t);
        // 写 3 个连续 extent 段（仍 ≤4 槽，depth-0），共 12 块连续。
        diff_write_step(&old_t, &new_t, ino_t, 0, &vec![0x55u8; 12 * bs]); // [0,12)
        assert_eq!(root_depth(&old_t, ino_t), 0, "T: depth-0 shallow tree");

        // 此刻 entries_count==1（extent [0,12)）。下面每个 shrink 调用进入时 entries>0。
        diff_truncate_step(&old_t, &new_t, ino_t, 20 * bs as u64); // grow 稀疏（entries 仍 1）
        diff_truncate_step(&old_t, &new_t, ino_t, 8 * bs as u64); // shrink 到块边界（进入时 entries=1>0）
        diff_truncate_step(&old_t, &new_t, ino_t, 5 * bs as u64 + 7); // shrink 块中部（written 尾 RMW；entries>0）
        diff_truncate_step(&old_t, &new_t, ino_t, 0); // shrink-to-0（进入时 entries=1>0 → 安全；清后 entries=0）
        assert_eq!(root_depth(&old_t, ino_t), 0, "T: depth-0 after shrink-to-0");
        diff_truncate_step(&old_t, &new_t, ino_t, 0); // 同尺寸 no-op（old==new，提前返回，不删）

        // ⚠️ BUG-16（参照侧）：ext4_rs `extent_remove_space`（extents.rs:1258/1283）对空叶
        //    （entries_count==0）做 `root_extent_at(entries_count-1)` **无下溢守卫** → 缩一棵
        //    entries==0 的树（稀疏 grow 出来的、或已清空的）会 panic。故**绝不 shrink entries==0 树**。
        //    要再覆盖「regrow 后 shrink」改走 **write-then-shrink**：稀疏 grow 后**先写真实块**
        //    （entries>0），再 shrink，安全。「shrink 空/稀疏树」差分推迟（仅源码 review，见 bug.md BUG-16）。
        diff_truncate_step(&old_t, &new_t, ino_t, 3 * bs as u64); // 空树稀疏 grow（entries 仍 0；grow 提前返回，不删 → 安全）
        diff_truncate_step(&old_t, &new_t, ino_t, 3 * bs as u64); // 同尺寸 no-op（安全）
        // write-then-shrink：先写真实块使 entries>0，再 shrink（进入时 entries>0 → 安全）。
        diff_write_step(&old_t, &new_t, ino_t, 0, &vec![0x66u8; 3 * bs]); // 写 [0,3) → entries>0
        assert_eq!(root_depth(&old_t, ino_t), 0, "T: depth-0 after regrow write");
        diff_truncate_step(&old_t, &new_t, ino_t, bs as u64 + 3); // shrink 到块中部（进入时 entries>0 → 安全）
        diff_truncate_step(&old_t, &new_t, ino_t, 0); // shrink-to-0（进入时 entries>0 → 安全）

        // =================================================================
        // Tree B — depth=1，仅 PARTIAL 删（叶永不清空 → ext_remove_idx 永不触发 → 不下溢）。
        // =================================================================
        let (base_b, ino_b) = make_base_disk_with_empty_file(EXT4_IMAGE, "dtb");
        let old_b = MemDisk::from_image(&base_b);
        let new_b = MemDisk::from_image(&base_b);

        // 建 depth=1 树：8 个离散紧凑 extent（每段 2 块、间隔 100）→ 满 root(4) 后
        // ext_grow_indepth → depth=1（8 ≪ 340 叶容量 → 单叶）。
        let build_b: [u32; 8] = [300, 100, 700, 200, 500, 50, 800, 400];
        for (k, &lblk) in build_b.iter().enumerate() {
            diff_write_step(&old_b, &new_b, ino_b, lblk as usize * bs, &vec![0x40 + k as u8; 2 * bs]);
        }
        let d_b = root_depth(&old_b, ino_b);
        assert_eq!(d_b, root_depth(&new_b, ino_b), "B: depth parity");
        assert!(d_b > 0, "B: expected depth>0 tree, got {d_b}");
        assert_disk_eq(&old_b, &new_b);

        // ④ depth>0 partial free：删一段高逻辑块，**只删尾部若干 extent、留 ≥1 在叶**。
        //    删 [750,801] 覆盖 800 段 + 700 段尾——但保留 50/100/200/300/400/500 等多个 extent。
        diff_remove_step(&old_b, &new_b, ino_b, 750, 801);
        assert!(root_depth(&old_b, ino_b) > 0, "B: leaf still non-empty after partial #1 (old)");
        assert!(root_depth(&new_b, ino_b) > 0, "B: leaf still non-empty after partial #1 (new)");

        // ⑤ depth>0 pos==0 first_block 传播：删**叶首 extent 的首块**（最小逻辑块 50 段的
        //    [50,51]）→ 叶 pos==0 → ext_correct_indexes → propagate_first_block_to_ancestors
        //    更新 root index 的 first_block 键。叶仍留 ≥1 extent（100/200/... 还在）。
        diff_remove_step(&old_b, &new_b, ino_b, 50, 51);
        assert!(root_depth(&old_b, ino_b) > 0, "B: leaf still non-empty after pos==0 (old)");
        assert!(root_depth(&new_b, ino_b) > 0, "B: leaf still non-empty after pos==0 (new)");

        // ⑥ depth>0 中间多 extent partial free：删 [150,450] 覆盖 200/300/400 段，
        //    仍保留 100 段 + 500 段 → 叶非空。
        diff_remove_step(&old_b, &new_b, ino_b, 150, 450);
        assert!(root_depth(&old_b, ino_b) > 0, "B: leaf still non-empty after partial #2 (old)");
        assert!(root_depth(&new_b, ino_b) > 0, "B: leaf still non-empty after partial #2 (new)");

        // ⑦ depth>0 truncate partial：缩到 (601)*bs（>500 段尾 502，故 500/100 段保留），
        //    free 高段但叶留 ≥1 extent → ext_remove_idx 不触发。
        diff_truncate_step(&old_b, &new_b, ino_b, 600 * bs as u64);
        assert!(root_depth(&old_b, ino_b) > 0, "B: leaf still non-empty after truncate partial (old)");
        assert!(root_depth(&new_b, ino_b) > 0, "B: leaf still non-empty after truncate partial (new)");
    }

    // =================================================================
    // Phase 3 Task 6：边角差分（深树 depth>1 / 多组 / csum 门控 / 损坏节点 / 乱序插入）。
    //
    // 全部建在前序的两盘差分骨架上（old=ext4_rs、new=core，每盘独立 `MemDisk`、同一份
    // setup 字节）。延续 Task 5 教训：差分只喂 ext4_rs 能优雅处理的输入——深树/乱序只
    // **读 + PARTIAL 删（永不清空叶、永不 empty depth>0 树）**；损坏节点验**两侧同 Err、
    // 都不 panic**。
    // =================================================================

    /// 不做逐步全盘对拍的「建树写」：两盘各跑同一 `write_at`、断言返回字节一致，
    /// **不**每步 `assert_disk_eq`（深树建树要几百步，逐步比对 64M 盘过慢）。两侧跑完全
    /// 相同的序列，终态由调用方一次性 `assert_disk_eq` 兜底——任何分歧终态比对必现。
    fn build_write_step(old_disk: &MemDisk, new_disk: &MemDisk, ino: u32, off: usize, buf: &[u8]) {
        let ext4 = ext4_rs::Ext4::open(Arc::new(old_disk.clone()));
        let old_ret = ext4.write_at(ino, off, buf);
        let new_ret = core_write_at(new_disk, ino, off, buf);
        match (&old_ret, &new_ret) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "build write_at bytes mismatch off={off}"),
            (Err(_), Err(_)) => {}
            _ => panic!("build write_at ok/err mismatch off={off}: old={old_ret:?} new={new_ret:?}"),
        }
    }

    /// 逐 lblock + map_blocks 向量读对拍（不改盘），覆盖 `[0, end)` 全逻辑块 + 尾后一段 hole。
    fn assert_map_read_eq(old_disk: &MemDisk, new_disk: &MemDisk, ino: u32, end: u32) {
        let sb = read_sb(new_disk);
        let ext4 = ext4_rs::Ext4::open(Arc::new(old_disk.clone()));
        let new_inode = load_inode(new_disk, &sb, ino).expect("load inode");
        let old_ref = ext4.get_inode_ref(ino);
        for lblock in 0..end {
            let new_res = get_pblock_idx_state(new_disk, &sb, &new_inode, lblock);
            let old_res = ext4.get_pblock_idx_state(&old_ref, lblock);
            match (new_res, old_res) {
                (Ok(Some((np, nu))), Ok((op, ou))) => {
                    assert_eq!(np, op, "lblock={lblock} pblock");
                    assert_eq!(nu, ou, "lblock={lblock} unwritten");
                }
                (Ok(None), Err(e)) => assert_eq!(
                    e.error(),
                    ext4_rs::Errno::ENOENT,
                    "lblock={lblock}: new hole but old err != ENOENT"
                ),
                (nr, or) => panic!(
                    "lblock={lblock} divergence: new={:?} old_ok={:?}",
                    nr.as_ref().map(|o| o.is_some()),
                    or.is_ok()
                ),
            }
        }
        let ctx = ReadCtx::new(new_disk, &sb);
        let new_mb = map_blocks(&ctx, &new_inode, 0, end).expect("new map_blocks");
        let old_mb = ext4.map_blocks(ino, 0, end).expect("old map_blocks");
        let new_t: Vec<(u32, u64, u32)> =
            new_mb.iter().map(|r| (r.lblock, r.pblock, r.len)).collect();
        let old_t: Vec<(u32, u64, u32)> =
            old_mb.iter().map(|r| (r.lblock, r.pblock, r.len)).collect();
        assert_eq!(new_t, old_t, "map_blocks vector mismatch over [0,{end})");
    }

    /// 场景1（深树 depth≥2）：在 1K-块多组镜像上插入大量**离散、单块、逻辑升序**的 extent，
    /// 逼非根叶填满（1K 叶容量 84）后反复 `create_new_leaf` 充满 root index(4)、再 `ext_grow_indepth`
    /// → depth≥2。补 Task 2 deferred 的 **depth>1 读**覆盖（per-lblock + map_blocks 向量对拍）。
    ///
    /// 删除遵 Task 5 教训：**只做 PARTIAL 删**——对最高一段 extent 做**中间删（middle-split
    /// punch）**：删一段严格落在某 written extent 内侧的逻辑块（`extent_remove_space` 的早返回
    /// `②` 分支：截前段 + 尾段 reinsert，**不进自叶向上的索引删除循环**）。该路径完整下钻
    /// depth-2 的两层 index + 触发 split-reinsert，**绝不清空叶、绝不触 ext_remove_idx/根塌**，
    /// 故不撞 ext4_rs BUG-14/15/16；删后断言 depth 仍 >0。
    /// （depth>0 的清叶/idx 删/根塌差分仍 differential-deferred——见 `extent_delete_truncate_parity`
    /// 与 bug.md BUG-14/15/16；本测试只补 depth>1 的**读** + **split-punch 删**覆盖。）
    #[ktest]
    fn extents_deep_tree_parity() {
        let (base_bytes, ino) = make_base_disk_with_empty_file(EXT4_MULTIGROUP_IMAGE, "deep");
        let old_disk = MemDisk::from_image(&base_bytes);
        let new_disk = MemDisk::from_image(&base_bytes);
        let bs = read_sb(&new_disk).block_size();
        assert_eq!(bs, 1024, "deep tree fixture is the 1K multigroup image");

        // 1K 叶容量 = (1024-12)/12 = 84；root index 容量 4 → 充满 ~4 个叶（≈84*4=336）后
        // 再插一个触发第二次 ext_grow_indepth → depth=2。N=380 留足余量越过阈值。
        // 离散 **3-块** extent（lblock = i*4，每段占 3 块、间隔 1 不可合并），逻辑升序规整生长。
        // 3-块（非单块）是为后续 middle-split 删留「严格内侧」逻辑块。
        const N: u32 = 380;
        for i in 0..N {
            let lblk = (i as usize) * 4;
            build_write_step(&old_disk, &new_disk, ino, lblk * bs, &vec![(i & 0xff) as u8; 3 * bs]);
        }

        // 终态一次性全盘逐字节对拍（覆盖 inode 表 + 全部 node 块 + 叶块 + 数据 + 位图 + GDT + SB）。
        assert_disk_eq(&old_disk, &new_disk);

        // 确认确为 depth≥2 深树（两侧同）。
        let d_old = root_depth(&old_disk, ino);
        let d_new = root_depth(&new_disk, ino);
        assert_eq!(d_old, d_new, "deep: depth parity");
        assert!(d_new >= 2, "deep: expected depth>=2 tree, got {d_new}");

        // depth>1 读对拍：逐 lblock get_pblock_idx_state + map_blocks 向量（含段间/尾后 hole）。
        let read_end = N * 4 + 8;
        assert_map_read_eq(&old_disk, &new_disk, ino, read_end);

        // 写后读回字节对拍（多段，含 hole/已写/跨块）。
        for &(off, len) in &[
            (0usize, 8 * bs),
            (bs, 6 * bs),
            ((N as usize - 2) * 4 * bs, 6 * bs), // 高段附近
        ] {
            assert_read_eq(&old_disk, &new_disk, ino, off, len);
        }

        // PARTIAL 删（middle-split punch，Task 5 安全约束）：删最高一段 extent [top, top+3)
        // 的**严格内侧中间块** [top+1, top+1]——`extent_remove_space` 走 `②` 早返回（截前段
        // + 尾段 reinsert，不进索引删循环、不清叶）。下钻 depth-2 两层 index + split-reinsert。
        let top = (N - 1) * 4;
        diff_remove_step(&old_disk, &new_disk, ino, top + 1, top + 1);
        assert!(root_depth(&old_disk, ino) > 0, "deep: depth stays >0 after split punch (old)");
        assert!(root_depth(&new_disk, ino) > 0, "deep: depth stays >0 after split punch (new)");
        // 删后再读对拍（中间块成 hole，前/尾段仍在）。
        assert_map_read_eq(&old_disk, &new_disk, ino, read_end);
    }

    /// 场景2（多组）：在 1K-块、8 组、first_data_block=1 几何上写/读/truncate 差分。
    /// 验证 1K 块 + first_data_block=1 下 extent 块 csum 与位置正确（4K 单组镜像测不到）；
    /// 物理块分配随段增长自然推进，覆盖跨组以外的 1K 几何专属路径。每步全盘逐字节对拍。
    #[ktest]
    fn extent_multigroup_parity() {
        let (base_bytes, ino) = make_base_disk_with_empty_file(EXT4_MULTIGROUP_IMAGE, "mgf");
        let old_disk = MemDisk::from_image(&base_bytes);
        let new_disk = MemDisk::from_image(&base_bytes);
        let bs = read_sb(&new_disk).block_size();
        assert_eq!(bs, 1024, "multigroup fixture is the 1K image");

        // ① 顺序 append 多块（连续 extent + 预分配尾，1K 块下 extent 块 csum 位置）。
        diff_write_step(&old_disk, &new_disk, ino, 0, &vec![0x11u8; 8 * bs]); // [0,8)
        // ② 稀疏远块写造 hole（跨段，强制独立 extent）。
        diff_write_step(&old_disk, &new_disk, ino, 500 * bs, &vec![0x22u8; 4 * bs]); // [500,504)
        // ③ 跨已有 extent 边界的多块写。
        diff_write_step(&old_disk, &new_disk, ino, 6 * bs, &vec![0x33u8; 6 * bs]); // [6,12)
        // ④ 离散多 extent 逼出 depth>0（验 1K 下 node 块 csum/位置），仍 ≤ 单叶容量。
        for k in 0..10u32 {
            let lblk = 100 + k * 20;
            diff_write_step(&old_disk, &new_disk, ino, lblk as usize * bs, &vec![0x40 + k as u8; bs]);
        }
        assert!(root_depth(&old_disk, ino) > 0, "mg: depth>0 (old)");
        assert!(root_depth(&new_disk, ino) > 0, "mg: depth>0 (new)");

        // 读回逐字节 + map_blocks 向量对拍。
        assert_map_read_eq(&old_disk, &new_disk, ino, 520);
        assert_read_eq(&old_disk, &new_disk, ino, 0, 12 * bs);
        assert_read_eq(&old_disk, &new_disk, ino, 500 * bs, 4 * bs);

        // ⑤ truncate（depth>0、PARTIAL：缩到 (101)*bs，保留低段 [0,12) 与 100 段 → 叶非空、
        //    永不清空 depth>0 叶）。全盘逐字节（含 i_size/i_blocks/位图释放）。
        diff_truncate_step(&old_disk, &new_disk, ino, 101 * bs as u64);
        assert!(root_depth(&old_disk, ino) > 0, "mg: depth stays >0 after partial truncate (old)");
        assert!(root_depth(&new_disk, ino) > 0, "mg: depth stays >0 after partial truncate (new)");
    }

    /// 场景3（csum 门控）：同一写/truncate 序列在 csum-开（`EXT4_IMAGE`）与 csum-关
    /// （`EXT4_NOCSUM_IMAGE`）两镜像各跑一遍，每步两盘全盘对拍。证 RO_COMPAT_METADATA_CSUM
    /// 门控——csum 开时写 extent 块 tail csum、关时不写，且每镜像内 new==old。
    /// （门控正确性由「关-镜像全盘 new==old」捕获：core 若误写 csum，关-镜像即与 old 不符。）
    #[ktest]
    fn extent_csum_gating_parity() {
        for &(image, label) in &[(EXT4_IMAGE, "csum-on"), (EXT4_NOCSUM_IMAGE, "csum-off")] {
            let (base_bytes, ino) = make_base_disk_with_empty_file(image, "csg");
            let old_disk = MemDisk::from_image(&base_bytes);
            let new_disk = MemDisk::from_image(&base_bytes);
            let bs = read_sb(&new_disk).block_size();

            // 写序列：连续段 + 稀疏段 + 离散多 extent 逼出 depth>0（node 块 → 走 csum 门控写路径）。
            diff_write_step(&old_disk, &new_disk, ino, 0, &vec![0x77u8; 4 * bs]); // [0,4)
            diff_write_step(&old_disk, &new_disk, ino, 50 * bs, &vec![0x88u8; 2 * bs]); // [50,52)
            for k in 0..8u32 {
                let lblk = 200 + k * 100;
                diff_write_step(&old_disk, &new_disk, ino, lblk as usize * bs, &vec![0x90 + k as u8; bs]);
            }
            assert!(root_depth(&old_disk, ino) > 0, "{label}: depth>0 (old)");
            assert!(root_depth(&new_disk, ino) > 0, "{label}: depth>0 (new)");

            // 读回对拍（确认 csum 门控不影响数据/映射语义）。
            assert_map_read_eq(&old_disk, &new_disk, ino, 920);

            // truncate PARTIAL（缩到 201*bs，保留 [0,4)+50 段+200 段 → 叶非空）。
            diff_truncate_step(&old_disk, &new_disk, ino, 201 * bs as u64);
            assert!(root_depth(&old_disk, ino) > 0, "{label}: depth stays >0 after truncate (old)");
            assert!(root_depth(&new_disk, ino) > 0, "{label}: depth stays >0 after truncate (new)");
        }
    }

    /// 把 inode `ino` 的 i_block（extent 树根，inode 内偏移 40 起 60 字节）的**头**字节按
    /// 给定 (magic, entries_count) 改坏，**两盘喂同样的坏字节**。定位逻辑同 `read_inode_bytes`。
    fn corrupt_root_header(disk: &MemDisk, sb: &RawSuperblock, ino: u32, magic: u16, entries: u16) {
        let bs = sb.block_size();
        let inode_size = sb.inode_size() as usize;
        let inodes_per_group = sb.inodes_per_group();
        let group = (ino - 1) / inodes_per_group;
        let index = ((ino - 1) % inodes_per_group) as usize;
        let gdt_off = (sb.first_data_block as usize + 1) * bs;
        let desc_size = sb.group_desc_size();
        let mut raw = vec![0u8; desc_size];
        disk.read_at(gdt_off + group as usize * desc_size, raw.as_mut_slice());
        let mut desc_buf = [0u8; 64];
        let take = core::cmp::min(desc_size, 64);
        desc_buf[..take].copy_from_slice(&raw[..take]);
        let desc = RawGroupDescriptor::from_bytes(&desc_buf);
        // i_block 位于 inode 内偏移 40；extent 头前 4 字节 = magic(2) + entries_count(2)。
        let hdr_off = desc.inode_table() as usize * bs + index * inode_size + 40;
        let mut four = [0u8; 4];
        disk.read_at(hdr_off, four.as_mut_slice());
        four[0..2].copy_from_slice(&magic.to_le_bytes());
        four[2..4].copy_from_slice(&entries.to_le_bytes());
        // 经数据-写路径直写这 4 字节（两盘同字节）。
        let writer = DirectMetadataWriter::new(disk.clone(), bs);
        let block = (hdr_off / bs) as Ext4Fsblk;
        let off_in_block = hdr_off % bs;
        let mut full = vec![0u8; bs];
        disk.read_at(block as usize * bs, full.as_mut_slice());
        full[off_in_block..off_in_block + 4].copy_from_slice(&four);
        use crate::fs::ext4::core::metadata_writer::MetadataWriter;
        writer
            .write_metadata_for_handle(0, block, &full)
            .expect("write corrupted root header");
    }

    /// 场景4（损坏节点防御）：把 extent 树**根头**改坏（坏 magic 0xDEAD + entries_count 越界
    /// 9999），**两盘喂同样的坏字节**，对 `find_extent`/`get_pblock_idx_state`/`map_blocks`/
    /// `insert_extent` 验**新旧同样 Err/clamp、都不 panic**。
    ///
    /// PARITY 依据：core `NodeView::valid_entries`（extents.rs:214）与 ext4_rs
    /// `valid_entries_count` 同样在 `entries>capacity` 时返回 None → 读路径降级为 hole；
    /// core `insert_extent` 的 `entries_count>capacity → EIO` 防御（extents.rs:737）逐字复刻
    /// ext4_rs `insert_extent` 的同名 EIO 守卫（ext4_impls/extents.rs:294）。坏 magic 不被读
    /// 路径检查（两侧 `find_extent` 都不验 magic）→ 同样被 entries 越界主导。**两侧均不 panic。**
    ///
    /// **insert_extent 部分单侧验 core**：ext4_rs `insert_extent(&mut, &mut Ext4Extent)` 的
    /// 入参 `Ext4Extent` **未从 ext4_rs 顶层重导出**（不像 `Ext4ExtentHeader`），外部无法命名
    /// 构造之 → 无法对拍调用。故 insert_extent 损坏防御**单侧验 core 优雅 EIO 不 panic**；
    /// 旧侧同名 EIO 守卫由源码 review 覆盖（ext4_impls/extents.rs:294，注释引）。读路径
    /// （find_extent/map_blocks/get_pblock_idx_state）的损坏防御仍**双侧对拍**。
    #[ktest]
    fn extent_corrupted_node_defense() {
        let (base_bytes, ino) = make_base_disk_with_empty_file(EXT4_IMAGE, "cor");
        let old_disk = MemDisk::from_image(&base_bytes);
        let new_disk = MemDisk::from_image(&base_bytes);
        let sb = read_sb(&new_disk);

        // 两盘喂同样的坏字节：坏 magic + 越界 entries_count（depth 保持 0 → 根叶插入路径）。
        corrupt_root_header(&old_disk, &sb, ino, 0xDEAD, 9999);
        corrupt_root_header(&new_disk, &sb, ino, 0xDEAD, 9999);

        // (1) 读路径：get_pblock_idx_state 多个 lblock —— core 优雅返回 Ok(None)（hole），
        //     ext4_rs 返回 ENOENT；两侧都不 panic、语义一致。
        let ext4 = ext4_rs::Ext4::open(Arc::new(old_disk.clone()));
        let new_inode = load_inode(&new_disk, &sb, ino).expect("load inode (corrupted)");
        let old_ref = ext4.get_inode_ref(ino);
        for lblock in [0u32, 1, 5, 100] {
            let new_res = get_pblock_idx_state(&new_disk, &sb, &new_inode, lblock);
            let old_res = ext4.get_pblock_idx_state(&old_ref, lblock);
            match (new_res, old_res) {
                (Ok(None), Err(e)) => assert_eq!(
                    e.error(),
                    ext4_rs::Errno::ENOENT,
                    "corrupted lblock={lblock}: new hole but old err != ENOENT"
                ),
                (Ok(None), Ok(_)) => {
                    panic!("corrupted lblock={lblock}: old unexpectedly mapped a block")
                }
                (nr, or) => panic!(
                    "corrupted lblock={lblock} divergence: new={:?} old_ok={:?}",
                    nr.as_ref().map(|o| o.is_some()),
                    or.is_ok()
                ),
            }
        }

        // (2) find_extent：core 不 panic（返回 fallback 叶节点，无命中）；map_blocks 全 hole。
        let _ = find_extent(&new_disk, &sb, &new_inode, 0).expect("find_extent on corrupted root");
        let ctx = ReadCtx::new(&new_disk, &sb);
        let new_mb = map_blocks(&ctx, &new_inode, 0, 8).expect("map_blocks on corrupted root");
        let old_mb = ext4.map_blocks(ino, 0, 8).expect("old map_blocks on corrupted root");
        let new_t: Vec<(u32, u64, u32)> =
            new_mb.iter().map(|r| (r.lblock, r.pblock, r.len)).collect();
        let old_t: Vec<(u32, u64, u32)> =
            old_mb.iter().map(|r| (r.lblock, r.pblock, r.len)).collect();
        assert_eq!(new_t, old_t, "corrupted map_blocks vector mismatch");

        // (3) 写路径（单侧验 core，见 doc）：insert_extent 在 entries_count(9999)>capacity(4)
        //     时**优雅 EIO、不 panic**。core 侧：从盘重建 WriteCtx/分配器/inode，调 insert_extent。
        let newex = {
            let mut e = RawExtent::default();
            e.first_block = 0;
            e.set_actual_len(1);
            e.store_pblock(1234);
            e
        };
        let (sb2, writer) = new_sb_and_writer(&new_disk);
        let alloc = BlockAllocator::new(sb2, &new_disk, &writer);
        let mut inode = load_inode(&new_disk, &sb2, ino).expect("load inode for insert");
        let ictx = InodeAllocCtx::new(inode.blocks_count());
        let mut adapter = CoreAllocAdapter { alloc, ictx };
        let wctx = WriteCtx::new(&new_disk, &writer, &new_disk, &sb2);
        let new_ins = insert_extent(&wctx, &mut adapter, &mut inode, &newex);
        assert!(new_ins.is_err(), "core insert_extent must EIO on corrupted node");
        assert_eq!(
            new_ins.as_ref().err().map(|e| e.error()),
            Some(Errno::EIO),
            "core insert_extent corrupted-node error must be EIO"
        );
    }

    /// 场景5（乱序插入压 position）：以**逆序 + 随机交错**的逻辑块插入序列建树，
    /// 全盘逐字节对拍 + 逐 lblock read 对拍。压 `first_block`/`position` 在移位插入 /
    /// pos==0 传播 / create_new_leaf 选位下的正确性（report §7 风险点）。
    ///
    /// 删除遵 Task 5 教训：建树后只做 PARTIAL 删（留叶 ≥1 extent、永不清空 depth>0 叶）。
    #[ktest]
    fn extent_out_of_order_insert_parity() {
        let (base_bytes, ino) = make_base_disk_with_empty_file(EXT4_IMAGE, "ooo");
        let old_disk = MemDisk::from_image(&base_bytes);
        let new_disk = MemDisk::from_image(&base_bytes);
        let bs = read_sb(&new_disk).block_size();

        // 乱序逻辑块（逆序 + 交错），每段 1 块、互不相邻（间隔 ≥2，不可合并）→ 每次插入都要
        // 在已存项中**选位移位**，强压 binsearch_pos/insert_pos/first_block 传播。
        let order: [u32; 16] = [
            900, 100, 700, 300, 1100, 50, 1300, 250, 1500, 450, 1700, 650, 1900, 850, 2100, 1050,
        ];
        for (k, &lblk) in order.iter().enumerate() {
            // 逐步全盘对拍（16 步，盘小可承受）——任何 position/first_block 偏差立现。
            diff_write_step(&old_disk, &new_disk, ino, lblk as usize * bs, &vec![(0x10 + k) as u8; bs]);
        }
        // 越过 root(4) → ext_grow_indepth → depth>0（16 ≪ 单叶容量 → 单叶，position 全在叶内）。
        let d = root_depth(&old_disk, ino);
        assert_eq!(d, root_depth(&new_disk, ino), "ooo: depth parity");
        assert!(d > 0, "ooo: expected depth>0, got {d}");
        assert_disk_eq(&old_disk, &new_disk);

        // 逐 lblock + map_blocks 向量读对拍（覆盖全 16 段 + 段间 hole + 尾后 hole）。
        assert_map_read_eq(&old_disk, &new_disk, ino, 2200);

        // PARTIAL 删：删最低逻辑块单段 [50,50]（叶 pos==0 → first_block 向 root index 传播），
        // 叶仍留 15 段 → 永不清空 depth>0 叶。删后再读对拍。
        diff_remove_step(&old_disk, &new_disk, ino, 50, 50);
        assert!(root_depth(&old_disk, ino) > 0, "ooo: depth stays >0 after pos==0 delete (old)");
        assert!(root_depth(&new_disk, ino) > 0, "ooo: depth stays >0 after pos==0 delete (new)");
        assert_map_read_eq(&old_disk, &new_disk, ino, 2200);
    }
}
