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

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::{map_blocks, plan_direct_read, prepare_write_at, read_at, write_at, ReadCtx};
    use crate::fs::ext4::core::balloc::{BlockAllocator, InodeAllocCtx};
    use crate::fs::ext4::core::diff_harness::{assert_disk_eq, DirectMetadataWriter, MemDisk};
    use crate::fs::ext4::core::extents::{get_pblock_idx_state, BlockAlloc, RawExtentHeader, WriteCtx};
    use crate::fs::ext4::core::inode::{load_inode, Inode};
    use crate::fs::ext4::core::io::BlockReader;
    use crate::fs::ext4::core::superblock::RawSuperblock;
    use crate::fs::ext4::core::test_util::{EXT4_IMAGE, EXT4_MULTIGROUP_IMAGE};
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
}
