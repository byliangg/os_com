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

/// extent-mapped 文件的最大逻辑块数上界 = `2^32`。
///
/// [对照] ext4 `struct ext4_extent.ee_block` 是 **32 位**逻辑块号（`fs/ext4/ext4_extents.h`），
/// 故 extent 文件最多寻址 `2^32` 个逻辑块（lblock 索引 `[0, 2^32)`）。Linux
/// `ext4_max_size()`（`fs/ext4/super.c`，extent 分支）取 `upper_limit = (1<<32)` 块、
/// `s_maxbytes = upper_limit << blkbits`（再 clamp 到 `MAX_LFS_FILESIZE`）。
const EXT4_MAX_LOGICAL_BLOCKS: u64 = 1u64 << 32;

/// ext4 extent 文件的最大字节大小 = `2^32 * block_size`（= ext4 `s_maxbytes`）。
///
/// **块大小相关**（本核心支持可变块大小，`block_size = 1024 << s_log_block_size`）：
/// - 4 KiB 块：`2^32 * 4096 = 2^44 = 16 TiB`；
/// - 1 KiB 块：`2^32 * 1024 = 2^42 = 4 TiB`。
///
/// 修复 BUG-12 的「附带 ext4_rs bug」：ext4_rs `EXT4_MAX_FILE_SIZE`（consts.rs:43）写死
/// `16 * 1024 * 1024 * 1024` = **16 GiB**（注释却写 16TB，值与注释矛盾），对 ext4 extent
/// 文件是离谱地小、会误拒合法写。这里改为 ext4-spec-correct 的块大小相关上界。
///
/// 一个**恰为** `2^32 * block_size` 字节的文件需要逻辑块 `[0, 2^32)`，索引全在 32 位内、合法；
/// 故守卫判据是 `new_size > max → EFBIG`（严格越界才拒）。三个写入口（write_at /
/// prepare_write_at / allocate_range）与 truncate 增长分支统一用本上界。
fn ext4_max_file_size(block_size: usize) -> u64 {
    EXT4_MAX_LOGICAL_BLOCKS.saturating_mul(block_size as u64)
}

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
    // [对照] ext4 写路径的 EFBIG 上界（修复 BUG-12）：除防 `checked_add` 溢出外，新结尾偏移
    // 超过 extent 文件最大字节大小（`2^32 * block_size`，见 `ext4_max_file_size`）即 EFBIG。
    // 与 prepare_write_at / allocate_range / truncate 增长分支同一道上界。
    let write_end = offset
        .checked_add(write_buf.len())
        .ok_or_else(|| Error::with_message(Errno::EFBIG, "write end overflow"))?;
    if write_end as u64 > ext4_max_file_size(block_size) {
        return Err(Error::with_message(Errno::EFBIG, "file size too large"));
    }
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
    // [对照] ext4 写准备路径的 EFBIG 上界（修复 BUG-12）：除防 `checked_add` 溢出外，新结尾
    // 偏移超过 extent 文件最大字节大小（`2^32 * block_size`）即 EFBIG。与 write_at /
    // allocate_range / truncate 增长分支同一道上界。
    let write_end = offset
        .checked_add(len)
        .ok_or_else(|| Error::with_message(Errno::EFBIG, "write end overflow"))?;
    if write_end as u64 > ext4_max_file_size(block_size) {
        return Err(Error::with_message(Errno::EFBIG, "file size too large"));
    }
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
// - `punch_hole_keep_size`：**真 FALLOC_FL_PUNCH_HOLE | KEEP_SIZE**（Phase 7 Task 6 修 BUG-13）。
//   释放完全落在 `[offset, min(offset+len, file_size))` 内的整块（从 extent 树删 → 物理块经
//   `extent_remove_space` 释放、i_blocks 精确递减、留 hole 读零），只对 head/tail 的**部分覆盖**
//   边界块零写其在范围内的字节（不释放整块——块内尚有别的数据）。i_size 不变（KEEP_SIZE）。
//   返回被零写的字节数（边界块）。详见 §punch 设计。
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
    // [对照] ext4 fallocate 的 EFBIG 上界（BUG-12 统一为块大小相关上限）：range_end 超过
    //   extent 文件最大字节大小（`2^32 * block_size`）即 EFBIG。与 write_at/prepare/truncate 同。
    if range_end as u64 > ext4_max_file_size(block_size) {
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

/// `punch_hole_keep_size`：**真 FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE**。
/// 返回被零写的字节数（仅部分覆盖的边界块）。Phase 7 Task 6 修 BUG-13，判据切 ext4 规范。
///
/// [对照] Linux `ext4_punch_hole`（fs/ext4/inode.c）：对 `[offset, offset+len)`
/// - **完全覆盖的整块** `[align_up(offset), align_down(range_end))` → 从 extent 树删除
///   （`extent_remove_space` 释放物理块、按 512B 单位精确递减 i_blocks、中部删自动分裂 extent）
///   → 这些逻辑块变 hole，读回零（ext4 缺席 extent 读零）；
/// - **部分覆盖的 head/tail 边界块** → **不释放整块**（块内尚有范围外的数据），只把其落在
///   `[offset, range_end)` 内的字节零写（RMW；hole/unwritten 边界本就读零，跳过不分配）。
///
/// i_size **不变**（KEEP_SIZE，punch 永不改文件大小）。i_blocks 恰减去被释放的整块数。
/// 范围零长 / `offset >= file_size` → no-op。只走 extent 映射；legacy 间接映射不在本 Phase 范围。
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
    let block_size = ctx.block_size;
    // KEEP_SIZE：punch 不超过文件末尾——超出部分本就是 hole，无块可释放、无字节可零写。
    let range_end = offset
        .checked_add(len)
        .ok_or_else(|| Error::with_message(Errno::EFBIG, "punch range end overflow"))?
        .min(file_size);
    if offset >= range_end {
        return Ok(0);
    }

    // 完全覆盖的整块（逻辑块号）：[ceil(offset/bs), floor(range_end/bs))。
    // first_full = 第一个起点 >= offset 的块；last_full_excl = 第一个起点 >= range_end 的块。
    let first_full_lblock = offset.div_ceil(block_size);
    let last_full_lblock_excl = range_end / block_size;

    // 1) 释放完全覆盖的整块——从 extent 树删，物理块回收、i_blocks 精确递减、留 hole。
    if first_full_lblock < last_full_lblock_excl {
        let from = u32::try_from(first_full_lblock)
            .map_err(|_| Error::with_message(Errno::EFBIG, "punch lblock start too big"))?;
        // extent_remove_space 收闭区间 [from, to]；to = 最后一个完全覆盖块。
        let to = u32::try_from(last_full_lblock_excl - 1)
            .map_err(|_| Error::with_message(Errno::EFBIG, "punch lblock end too big"))?;
        extents::extent_remove_space(ctx, alloc, inode, from, to)?;
    }

    // 2) 部分覆盖的边界块——只零写在范围内的字节，不释放整块。
    //    head 块覆盖 [offset, min(head_block_end, range_end))；
    //    tail 块覆盖 [max(tail_block_start, offset), range_end)，且 tail 块 != head 块时才单独零。
    let mut zeroed = 0usize;

    // head 部分块：offset 不块对齐 → head 块被部分覆盖。
    if offset % block_size != 0 {
        let head_block_end = (offset / block_size + 1) * block_size;
        let head_zero_end = head_block_end.min(range_end);
        if offset < head_zero_end {
            zeroed += zero_partial_edge_block(ctx, inode, offset, head_zero_end - offset)?;
        }
    }

    // tail 部分块：range_end 不块对齐、且 tail 块严格在 head 块之后（避免对单个边界块重复零写）。
    if range_end % block_size != 0 {
        let tail_block_start = (range_end / block_size) * block_size;
        let tail_zero_start = tail_block_start.max(offset);
        // 仅当 tail 块与 head 块不同（tail 块起点 >= head 块末尾，即 >= first_full 起点）才单独处理；
        // offset 与 range_end 同块时上面 head 分支已覆盖整段，这里跳过。
        if tail_zero_start > offset && tail_zero_start < range_end {
            zeroed += zero_partial_edge_block(ctx, inode, tail_zero_start, range_end - tail_zero_start)?;
        }
    }

    Ok(zeroed)
}

/// 对**单个边界块**内 `[offset, offset+len)` 的字节零写（RMW），`len` 不得跨块。
/// 该块是 hole（缺席 extent）或 unwritten（预分配，读零）→ 跳过、**不分配**（已读零）。
/// written 块 → 读出整块、把范围内字节清零、写回。返回实际零写的字节数（hole/unwritten 返回 0）。
fn zero_partial_edge_block(
    ctx: &WriteCtx,
    inode: &Inode,
    offset: usize,
    len: usize,
) -> Result<usize> {
    if len == 0 {
        return Ok(0);
    }
    let block_size = ctx.block_size;
    let lblock = u32::try_from(offset / block_size)
        .map_err(|_| Error::with_message(Errno::EFBIG, "edge lblock too big"))?;
    let in_block = offset % block_size;

    match extents::get_pblock_idx_state(ctx.reader, ctx.sb, inode, lblock)? {
        // written 块：RMW 把 [in_block, in_block+len) 清零。
        Some((pblock, false)) => {
            let block_offset = usize::try_from(pblock)
                .map_err(|_| Error::with_message(Errno::EFBIG, "edge pblock too big"))?
                .checked_mul(block_size)
                .ok_or_else(|| Error::with_message(Errno::EFBIG, "edge block offset overflow"))?;
            let mut block_data = vec![0u8; block_size];
            ctx.reader.read_at(block_offset, block_data.as_mut_slice());
            block_data[in_block..in_block + len].fill(0);
            ctx.data_writer.write_at(block_offset, block_data.as_slice());
            Ok(len)
        }
        // unwritten 块（预分配、读零）或 hole（缺席 extent，读零）→ 已读零，不动、不分配。
        Some((_, true)) | None => Ok(0),
    }
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
/// - `old < new`（增长）→ EFBIG 守卫（`new_size > ext4_max_file_size`）+ **稀疏** set_size
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
        // [对照] 块大小相关上界（`2^32 * block_size`），与写/fallocate 路径一致（BUG-12）。
        if new_size > ext4_max_file_size(block_size as usize) {
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
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use ostd::prelude::*;

    use alloc::collections::BTreeSet;

    use super::{
        ext4_max_file_size, prepare_write_at, punch_hole_keep_size, read_at, write_at,
        EXT4_INODE_FLAG_EXTENTS,
    };
    use crate::fs::ext4::core::block_group::RawGroupDescriptor;
    use crate::fs::ext4::core::extents::{
        BlockAlloc, RawExtent, RawExtentHeader, WriteCtx, EXTENT_MAGIC,
    };
    use crate::fs::ext4::core::inode::{load_inode, Inode, RawInode};
    use crate::fs::ext4::core::io::{BlockReader, BlockWriter};
    use crate::fs::ext4::core::metadata_writer::MetadataWriter;
    use crate::fs::ext4::core::superblock::RawSuperblock;
    use crate::fs::ext4::core::test_util::EXT4_IMAGE;
    use crate::fs::ext4::core::types::Ext4Fsblk;
    use crate::prelude::Result;
    use crate::prelude::*;

    /// 4 KiB 块、256B inode 的真镜像几何常量（见 EXT4_IMAGE）。
    const EXT_MAGIC: u16 = 0xF30A;

    /// 内存盘：读/数据写/元数据写都打到同一份字节。
    struct MemImage {
        bytes: RefCell<Vec<u8>>,
        block_size: usize,
    }
    impl MemImage {
        fn new(seed: &[u8]) -> Self {
            let sb = RawSuperblock::from_bytes(&seed[1024..2048]);
            MemImage {
                bytes: RefCell::new(seed.to_vec()),
                block_size: sb.block_size(),
            }
        }
    }
    impl BlockReader for MemImage {
        fn read_at(&self, off: usize, out: &mut [u8]) {
            let b = self.bytes.borrow();
            for (i, slot) in out.iter_mut().enumerate() {
                *slot = b.get(off + i).copied().unwrap_or(0);
            }
        }
    }
    impl BlockWriter for MemImage {
        fn write_at(&self, off: usize, data: &[u8]) {
            let mut b = self.bytes.borrow_mut();
            for (i, byte) in data.iter().enumerate() {
                if let Some(slot) = b.get_mut(off + i) {
                    *slot = *byte;
                }
            }
        }
    }
    impl MetadataWriter for MemImage {
        fn write_metadata_for_handle(
            &self,
            _handle_id: u64,
            block: Ext4Fsblk,
            data: &[u8],
        ) -> Result<()> {
            self.write_at(block as usize * self.block_size, data);
            Ok(())
        }
    }

    /// 永不分配的分配器：alloc 入口一律 ENOSPC。EFBIG 上界检查发生在分配之前，故 under-bound
    /// 写会越过尺寸守卫、落到这个分配器（返回 ENOSPC 而非 EFBIG「file size too large」）——
    /// 借此证明尺寸守卫**没有**误拒 under-bound 的合法写。
    struct NoAlloc;
    impl BlockAlloc for NoAlloc {
        fn alloc_one(&mut self, _inode: &mut Inode) -> Result<Ext4Fsblk> {
            Err(Error::with_message(Errno::ENOSPC, "no alloc"))
        }
        fn alloc_batch(
            &mut self,
            _inode: &mut Inode,
            _start_bgid: &mut u32,
            _count: usize,
        ) -> Result<Vec<Ext4Fsblk>> {
            Err(Error::with_message(Errno::ENOSPC, "no alloc"))
        }
        fn free_blocks(&mut self, _inode: &mut Inode, _start: Ext4Fsblk, _count: u32) {}
    }

    /// 定位 inode `ino` 在镜像中的字节偏移（第一组 group descriptor → inode table）。
    fn inode_off(img: &[u8], sb: &RawSuperblock, ino: u32) -> usize {
        let bs = sb.block_size();
        let gd = RawGroupDescriptor::from_bytes(
            &img[(sb.first_data_block as usize + 1) * bs
                ..(sb.first_data_block as usize + 1) * bs + 64],
        );
        gd.inode_table() as usize * bs + (ino as usize - 1) * sb.inode_size() as usize
    }

    /// 在 disk 上把 inode `ino` 初始化成一个空 extent reg 文件（i_size=0），返回加载好的句柄。
    fn seed_empty_reg_inode(disk: &MemImage, sb: &RawSuperblock, ino: u32) -> Inode {
        let off = inode_off(EXT4_IMAGE, sb, ino);
        let mut raw = RawInode::from_bytes(&EXT4_IMAGE[off..off + 156]);
        raw.set_flags(EXT4_INODE_FLAG_EXTENTS);
        raw.set_mode(0x8000); // S_IFREG
        raw.set_size(0);
        // i_block 前 12 字节 = 空 extent header（entries=0, max=4, depth=0）。
        let mut iblock = [0u8; 60];
        iblock[0] = (EXT_MAGIC & 0xff) as u8;
        iblock[1] = (EXT_MAGIC >> 8) as u8;
        iblock[2] = 0; // entries lo
        iblock[3] = 0; // entries hi
        iblock[4] = 4; // max lo
        iblock[5] = 0; // max hi
                       // depth=0, generation=0 已为 0。
        let block: [u32; 15] = Pod::from_bytes(&iblock);
        raw.block = block;
        disk.write_at(off, raw.as_bytes());
        load_inode(disk, sb, ino).unwrap()
    }

    #[ktest]
    fn ext4_max_file_size_is_block_size_aware() {
        // [对照] ext4 s_maxbytes = 2^32 * block_size：4K→16 TiB，1K→4 TiB。
        assert_eq!(ext4_max_file_size(4096), 1u64 << 44, "4K 块上界须为 16 TiB");
        assert_eq!(ext4_max_file_size(1024), 1u64 << 42, "1K 块上界须为 4 TiB");
        // 旧 ext4_rs 的错误值 16 GiB 远小于真上界——确认已不再使用。
        assert!(ext4_max_file_size(4096) > 16u64 * 1024 * 1024 * 1024);
    }

    #[ktest]
    fn write_at_rejects_over_max_with_efbig() {
        // BUG-12：write_at 在 new_size 超过 (2^32 * block_size) 时返回 EFBIG。
        let disk = MemImage::new(EXT4_IMAGE);
        let sb = RawSuperblock::from_bytes(&EXT4_IMAGE[1024..2048]);
        let max = ext4_max_file_size(sb.block_size());

        let mut inode = seed_empty_reg_inode(&disk, &sb, 11);
        let ctx = WriteCtx::new(&disk, &disk, &disk, &sb);
        let mut alloc = NoAlloc;

        // offset 恰为 max、再写 1 字节 → new_size = max + 1 > max → EFBIG。
        let buf = [0xABu8; 1];
        let err = write_at(&ctx, &mut alloc, &mut inode, max as usize, &buf)
            .expect_err("over-max write must fail");
        assert_eq!(err.error(), Errno::EFBIG, "over-max write must be EFBIG");
    }

    #[ktest]
    fn prepare_write_at_rejects_over_max_with_efbig() {
        // BUG-12：prepare_write_at 同样补上 EFBIG 上界。
        let disk = MemImage::new(EXT4_IMAGE);
        let sb = RawSuperblock::from_bytes(&EXT4_IMAGE[1024..2048]);
        let max = ext4_max_file_size(sb.block_size());

        let mut inode = seed_empty_reg_inode(&disk, &sb, 11);
        let ctx = WriteCtx::new(&disk, &disk, &disk, &sb);
        let mut alloc = NoAlloc;

        let err = prepare_write_at(&ctx, &mut alloc, &mut inode, max as usize + 1, 4096)
            .expect_err("over-max prepare must fail");
        assert_eq!(err.error(), Errno::EFBIG, "over-max prepare must be EFBIG");
    }

    #[ktest]
    fn write_under_max_passes_size_guard() {
        // BUG-12 反向守护：修正后的上界**不得**误拒 under-bound 的合法写。
        // 用永不分配的 NoAlloc → 写会越过尺寸守卫、落到分配器返回 ENOSPC（而非 EFBIG）。
        let disk = MemImage::new(EXT4_IMAGE);
        let sb = RawSuperblock::from_bytes(&EXT4_IMAGE[1024..2048]);
        let max = ext4_max_file_size(sb.block_size());

        // 选一个**远在上界内**的大偏移：max - 2*block_size（合法），写一整块。
        let off = (max - 2 * sb.block_size() as u64) as usize;
        let buf = [0xCDu8; 4096];

        let mut inode = seed_empty_reg_inode(&disk, &sb, 11);
        let ctx = WriteCtx::new(&disk, &disk, &disk, &sb);
        let mut alloc = NoAlloc;
        let err = write_at(&ctx, &mut alloc, &mut inode, off, &buf)
            .expect_err("NoAlloc must fail allocation");
        assert_eq!(
            err.error(),
            Errno::ENOSPC,
            "under-max write must pass size guard and fail at allocation, not EFBIG"
        );

        // prepare_write_at 同样越过尺寸守卫。
        let mut inode2 = seed_empty_reg_inode(&disk, &sb, 12);
        let err2 = prepare_write_at(&ctx, &mut alloc, &mut inode2, off, 4096)
            .expect_err("NoAlloc must fail allocation");
        assert_eq!(
            err2.error(),
            Errno::ENOSPC,
            "under-max prepare must pass size guard, not EFBIG"
        );
    }

    // ------------------------------------------------------------------
    // Phase 7 Task 6：真 FALLOC_FL_PUNCH_HOLE | KEEP_SIZE（BUG-13）standalone 测试。
    // 判据切 ext4 规范（非差分；ext4_rs 已删）。校验：
    //   ① 块对齐中部 punch → 完全覆盖块释放（free_blocks_count 增、i_blocks 精确减）、
    //      范围读零、i_size 不变、周边数据完好；
    //   ② 不对齐 punch → 部分边界块只零写在范围内的字节、不释放整块（i_blocks 只减
    //      完全覆盖块）、边界读零；
    //   ③ 单 extent 中部 punch → extent 分裂成两段、树仍有效、两侧读正确。
    // ------------------------------------------------------------------

    /// 真分配器：从高位空闲块池 `free` 取/还块，按 512B 当量增减 i_blocks（saturating，
    /// 与生产 BUG-14 修复语义一致）。`free.len()` 即 free_blocks_count——punch 释放整块后必增。
    struct TrackAlloc {
        block_size: usize,
        free: RefCell<BTreeSet<Ext4Fsblk>>,
    }
    impl TrackAlloc {
        /// 空闲池播入 `[lo, hi)` 高位块（避开低位元数据）。
        fn new(block_size: usize, lo: Ext4Fsblk, hi: Ext4Fsblk) -> Self {
            let mut free = BTreeSet::new();
            for b in lo..hi {
                free.insert(b);
            }
            TrackAlloc {
                block_size,
                free: RefCell::new(free),
            }
        }
        fn free_blocks_count(&self) -> usize {
            self.free.borrow().len()
        }
        fn pop_one(&self) -> Result<Ext4Fsblk> {
            let mut f = self.free.borrow_mut();
            let b = *f
                .iter()
                .next()
                .ok_or_else(|| Error::with_message(Errno::ENOSPC, "track pool empty"))?;
            f.remove(&b);
            Ok(b)
        }
    }
    impl BlockAlloc for TrackAlloc {
        fn alloc_one(&mut self, inode: &mut Inode) -> Result<Ext4Fsblk> {
            let b = self.pop_one()?;
            let inc = self.block_size as u64 / 512;
            let cur = inode.blocks_count();
            inode.set_blocks_count(cur + inc);
            Ok(b)
        }
        fn alloc_batch(
            &mut self,
            inode: &mut Inode,
            _start_bgid: &mut u32,
            count: usize,
        ) -> Result<Vec<Ext4Fsblk>> {
            let mut out = Vec::with_capacity(count);
            for _ in 0..count {
                out.push(self.alloc_one(inode)?);
            }
            Ok(out)
        }
        fn free_blocks(&mut self, inode: &mut Inode, start: Ext4Fsblk, count: u32) {
            for k in 0..count as u64 {
                self.free.borrow_mut().insert(start + k);
            }
            let dec = count as u64 * (self.block_size as u64 / 512);
            let cur = inode.blocks_count();
            inode.set_blocks_count(cur.saturating_sub(dec));
        }
    }

    /// 在 disk 上把 inode `ino` 初始化为「单个 `n_blocks` 块连续 extent」的 reg 文件：
    /// 逻辑块 `[0, n_blocks)` → 物理块 `[pstart, pstart+n_blocks)`，i_size = n_blocks*bs，
    /// i_blocks = n_blocks*(bs/512)。每个数据块填一个可辨识的字节模式（块号+1）。返回句柄。
    fn seed_contig_extent_file(
        disk: &MemImage,
        sb: &RawSuperblock,
        ino: u32,
        n_blocks: u32,
        pstart: Ext4Fsblk,
    ) -> Inode {
        let bs = sb.block_size();
        // 根 i_block：header(entries=1, depth=0) + 单 extent。
        let mut iblock = [0u8; 60];
        iblock[..12]
            .copy_from_slice(RawExtentHeader::new(EXTENT_MAGIC, 1, 4, 0, 0).as_bytes());
        let mut e = RawExtent::default();
        e.first_block = 0;
        e.block_count = n_blocks as u16;
        e.store_pblock(pstart);
        iblock[12..24].copy_from_slice(e.as_bytes());

        let off = inode_off(EXT4_IMAGE, sb, ino);
        let mut raw = RawInode::from_bytes(&EXT4_IMAGE[off..off + 156]);
        raw.set_flags(EXT4_INODE_FLAG_EXTENTS);
        raw.set_mode(0x8000); // S_IFREG
        raw.set_size(n_blocks as u64 * bs as u64);
        let block: [u32; 15] = Pod::from_bytes(&iblock);
        raw.block = block;
        // i_blocks（512B 单位）= n_blocks * (bs/512)；测试规模内 hi 半位恒 0。
        raw.blocks = (n_blocks as u64 * (bs as u64 / 512)) as u32;
        raw.osd2.l_i_blocks_high = 0;
        disk.write_at(off, raw.as_bytes());

        // 每个数据块填可辨识模式（第 i 块全填 (i+1) as u8）。
        for i in 0..n_blocks {
            let pat = vec![(i + 1) as u8; bs];
            disk.write_at((pstart as usize + i as usize) * bs, &pat);
        }

        load_inode(disk, sb, ino).unwrap()
    }

    #[ktest]
    fn punch_aligned_middle_frees_blocks_reads_zero() {
        // [对照] block-aligned 中部 punch → 完全覆盖块释放、范围读零、i_size 不变、周边完好。
        let disk = MemImage::new(EXT4_IMAGE);
        let sb = RawSuperblock::from_bytes(&EXT4_IMAGE[1024..2048]);
        let bs = sb.block_size();
        let ino = 11u32;
        let pstart: Ext4Fsblk = 6000;

        // 4 块连续 extent；空闲池从 7000 起（与数据块不重叠）。
        let mut inode = seed_contig_extent_file(&disk, &sb, ino, 4, pstart);
        let ctx = WriteCtx::new(&disk, &disk, &disk, &sb);
        let mut alloc = TrackAlloc::new(bs, 7000, 8000);

        let i_size_before = inode.size();
        let i_blocks_before = inode.blocks_count();
        let free_before = alloc.free_blocks_count();

        // punch 中部 2 块 [bs, 3*bs)（块对齐）。
        let zeroed = punch_hole_keep_size(&ctx, &mut alloc, &mut inode, bs, 2 * bs).unwrap();
        // 块对齐 punch 无部分边界块 → 不零写任何字节。
        assert_eq!(zeroed, 0, "aligned punch zeroes no edge bytes");

        // i_size 不变（KEEP_SIZE）。
        assert_eq!(inode.size(), i_size_before, "i_size must be unchanged by punch");
        // i_blocks 精确减 2 块。
        assert_eq!(
            inode.blocks_count(),
            i_blocks_before - 2 * (bs as u64 / 512),
            "i_blocks must drop by exactly the 2 freed blocks"
        );
        // free_blocks_count 增 2（物理块 6001、6002 回池）。
        assert_eq!(
            alloc.free_blocks_count(),
            free_before + 2,
            "free_blocks_count must increase by exactly 2"
        );
        assert!(
            alloc.free.borrow().contains(&(pstart + 1)) && alloc.free.borrow().contains(&(pstart + 2)),
            "the two middle physical blocks must be freed"
        );

        // 读：punch 区 [bs, 3*bs) 全零；周边块 0 / 3 数据完好。
        let read_ctx = ctx.read_ctx();
        let mut buf = vec![0xFFu8; 4 * bs];
        let got = read_at(&read_ctx, &inode, 0, &mut buf).unwrap();
        assert_eq!(got, 4 * bs, "full file readable");
        assert!(buf[0..bs].iter().all(|&b| b == 1), "block 0 intact");
        assert!(buf[bs..3 * bs].iter().all(|&b| b == 0), "punched range reads zero");
        assert!(buf[3 * bs..4 * bs].iter().all(|&b| b == 4), "block 3 intact");
    }

    #[ktest]
    fn punch_unaligned_zeroes_partial_edges_frees_only_full() {
        // [对照] 不对齐 punch：部分 head/tail 块只零写范围内字节、不释放；只完全覆盖块被释放。
        let disk = MemImage::new(EXT4_IMAGE);
        let sb = RawSuperblock::from_bytes(&EXT4_IMAGE[1024..2048]);
        let bs = sb.block_size();
        let ino = 12u32;
        let pstart: Ext4Fsblk = 6000;

        // 4 块连续 extent。
        let mut inode = seed_contig_extent_file(&disk, &sb, ino, 4, pstart);
        let ctx = WriteCtx::new(&disk, &disk, &disk, &sb);
        let mut alloc = TrackAlloc::new(bs, 7000, 8000);

        let i_size_before = inode.size();
        let i_blocks_before = inode.blocks_count();
        let free_before = alloc.free_blocks_count();

        // punch 范围 [bs/2, 2.5*bs)：
        //   head 块0 后半 [bs/2, bs) 部分覆盖、块1 完全覆盖、tail 块2 前半 [2*bs, 2.5*bs) 部分覆盖。
        //   完全覆盖块（起点 ceil(bs/2 / bs)=1 .. 终点 floor(2.5*bs / bs)=2）= 仅块1。
        let offset = bs / 2;
        let range_end = 3 * bs - bs / 2; // = 2.5*bs

        let zeroed = punch_hole_keep_size(&ctx, &mut alloc, &mut inode, offset, range_end - offset)
            .unwrap();
        // 两个部分边界块各零写 bs/2 字节。
        assert_eq!(zeroed, bs, "two partial edges each zero bs/2 bytes");

        // i_size 不变。
        assert_eq!(inode.size(), i_size_before, "i_size unchanged");
        // 只释放完全覆盖块（块1）→ i_blocks 减 1 块。
        assert_eq!(
            inode.blocks_count(),
            i_blocks_before - (bs as u64 / 512),
            "i_blocks must drop by exactly 1 fully-covered block"
        );
        assert_eq!(
            alloc.free_blocks_count(),
            free_before + 1,
            "free_blocks_count must increase by exactly 1"
        );
        assert!(
            alloc.free.borrow().contains(&(pstart + 1)),
            "only the fully-covered middle block is freed"
        );
        assert!(
            !alloc.free.borrow().contains(&pstart) && !alloc.free.borrow().contains(&(pstart + 2)),
            "partial edge blocks must NOT be freed"
        );

        // 读：边界块在范围内字节读零、范围外字节保留原模式。
        let read_ctx = ctx.read_ctx();
        let mut buf = vec![0xFFu8; 4 * bs];
        read_at(&read_ctx, &inode, 0, &mut buf).unwrap();
        // 块0：前半 [0, bs/2) 保留模式 1；后半 [bs/2, bs) 读零。
        assert!(buf[0..bs / 2].iter().all(|&b| b == 1), "head block kept prefix intact");
        assert!(buf[bs / 2..bs].iter().all(|&b| b == 0), "head block in-range zeroed");
        // 块1：完全覆盖（hole）读零。
        assert!(buf[bs..2 * bs].iter().all(|&b| b == 0), "fully-covered block reads zero");
        // 块2：前半 [2*bs, 2.5*bs) 读零；后半 [2.5*bs, 3*bs) 保留模式 3。
        assert!(buf[2 * bs..2 * bs + bs / 2].iter().all(|&b| b == 0), "tail block in-range zeroed");
        assert!(
            buf[2 * bs + bs / 2..3 * bs].iter().all(|&b| b == 3),
            "tail block kept suffix intact"
        );
        // 块3：完全在范围外，保留模式 4。
        assert!(buf[3 * bs..4 * bs].iter().all(|&b| b == 4), "block 3 untouched");
    }

    #[ktest]
    fn punch_mid_extent_split_keeps_tree_valid() {
        // [对照] 单 extent 中部 punch → extent 分裂成两段、树仍有效、两侧读正确。
        let disk = MemImage::new(EXT4_IMAGE);
        let sb = RawSuperblock::from_bytes(&EXT4_IMAGE[1024..2048]);
        let bs = sb.block_size();
        let ino = 13u32;
        let pstart: Ext4Fsblk = 6000;

        // 单个 5 块连续 extent，punch 严格在中部的块 2（[2*bs, 3*bs)）→ 内部分裂。
        let mut inode = seed_contig_extent_file(&disk, &sb, ino, 5, pstart);
        let ctx = WriteCtx::new(&disk, &disk, &disk, &sb);
        let mut alloc = TrackAlloc::new(bs, 7000, 8000);

        let i_blocks_before = inode.blocks_count();
        let free_before = alloc.free_blocks_count();

        let zeroed =
            punch_hole_keep_size(&ctx, &mut alloc, &mut inode, 2 * bs, bs).unwrap();
        assert_eq!(zeroed, 0, "aligned single-block punch zeroes no edge bytes");

        // 中部块释放：i_blocks 减 1、free +1，物理块 6002 回池。
        assert_eq!(
            inode.blocks_count(),
            i_blocks_before - (bs as u64 / 512),
            "i_blocks drops by 1 for the split-out punched block"
        );
        assert_eq!(alloc.free_blocks_count(), free_before + 1, "free +1");
        assert!(alloc.free.borrow().contains(&(pstart + 2)), "middle block 2 freed");
        // 两侧块未释放（仍映射）。
        assert!(
            [0u64, 1, 3, 4]
                .iter()
                .all(|&k| !alloc.free.borrow().contains(&(pstart + k))),
            "both sides of the split must remain mapped"
        );

        // 树仍有效：左段 [0,2)、hole 块2、右段 [3,5) 全读正确。
        let read_ctx = ctx.read_ctx();
        let mut buf = vec![0xEEu8; 5 * bs];
        let got = read_at(&read_ctx, &inode, 0, &mut buf).unwrap();
        assert_eq!(got, 5 * bs, "extent tree still walkable after split");
        assert!(buf[0..bs].iter().all(|&b| b == 1), "left side block 0 correct");
        assert!(buf[bs..2 * bs].iter().all(|&b| b == 2), "left side block 1 correct");
        assert!(buf[2 * bs..3 * bs].iter().all(|&b| b == 0), "punched middle reads zero (hole)");
        assert!(buf[3 * bs..4 * bs].iter().all(|&b| b == 4), "right side block 3 correct");
        assert!(buf[4 * bs..5 * bs].iter().all(|&b| b == 5), "right side block 4 correct");
    }
}
