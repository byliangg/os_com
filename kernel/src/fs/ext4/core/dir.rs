// SPDX-License-Identifier: MPL-2.0
use ostd::const_assert;

use super::prelude::*;

/// 目录项尾标识：reserved_ft == 0xDE 表示该槽是目录块尾校验和结构。
const DIR_TAIL_MARKER: u8 = 0xDE;

/// ext4_rs `size_of::<Ext4DirEntry>()` 的值（`#[repr(C)]`：u32+u16+u8+u8+`[u8;255]`
/// → 263 → 对齐到 264）。**这是内存结构尺寸泄漏进磁盘布局的关键 parity 常量**：
/// ext4_rs 的 `try_insert_to_existing_block` / `insert_to_new_block` 用它作
/// ① 插入所需空间下界 `required_len = align4(264 + name.len())`、② 写新项时整 264 字节
/// `copy_to_slice`（name 尾零填到 255 + 对齐）。Task 2 的切槽/写项必须逐字复刻 264。
// PARITY: size_of::<ext4_rs Ext4DirEntry> 泄漏进磁盘布局
// 写路径消费者：dir_write_entry_bytes / try_insert_to_existing_block（264 字节零填项 + 切槽判据）。
// allow(dead_code) 载重：core/dir 暂未接入生产 VFS（仍走 ext4_rs），消费链仅 #[cfg(ktest)] 可达。
#[allow(dead_code)]
pub(in crate::fs::ext4) const EXT4_DIR_ENTRY_INMEM_SIZE: usize = 264;

/// 目录项指向的 inode 类型（filetype 特性下的 file_type 字节）。
/// 安全替换旧实现里的 `union Ext4DirEnInternal`：当成 1 字节 + 按 filetype 解释。
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirEntryFileType {
    Unknown = 0,
    RegFile = 1,
    Dir = 2,
    Chrdev = 3,
    Blkdev = 4,
    Fifo = 5,
    Sock = 6,
    Symlink = 7,
}

impl From<u8> for DirEntryFileType {
    fn from(v: u8) -> Self {
        match v {
            1 => Self::RegFile,
            2 => Self::Dir,
            3 => Self::Chrdev,
            4 => Self::Blkdev,
            5 => Self::Fifo,
            6 => Self::Sock,
            7 => Self::Symlink,
            _ => Self::Unknown,
        }
    }
}

/// ext4 目录项定长头（8 字节，小端）。名字是其后的变长尾（name_len 字节），
/// 不整体 Pod 化变长结构（旧实现含 255 字节 name + union）。变长尾的解析留 Phase 4。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub(in crate::fs::ext4) struct RawDirEntryHeader {
    pub inode: u32,
    pub rec_len: u16,
    pub name_len: u8,
    pub file_type: u8,
}
const_assert!(size_of::<RawDirEntryHeader>() == 8);

/// 目录块尾的校验和结构（12 字节）。占用一个普通目录项槽，靠 reserved_ft==0xDE 识别。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub(in crate::fs::ext4) struct RawDirEntryTail {
    pub reserved_zero1: u32,
    pub rec_len: u16,
    pub reserved_zero2: u8,
    pub reserved_ft: u8,
    pub checksum: u32,
}
const_assert!(size_of::<RawDirEntryTail>() == 12);

impl RawDirEntryHeader {
    pub fn inode(&self) -> u32 {
        self.inode
    }
    pub fn rec_len(&self) -> u16 {
        self.rec_len
    }
    pub fn name_len(&self) -> u8 {
        self.name_len
    }
    pub fn file_type(&self) -> DirEntryFileType {
        DirEntryFileType::from(self.file_type)
    }
    /// inode==0 表示未使用的空槽。
    pub fn is_unused(&self) -> bool {
        self.inode == 0
    }
}

impl RawDirEntryTail {
    /// 是否目录块尾结构（reserved_ft==0xDE 且 inode 字段位置为 0）。
    pub fn is_tail(&self) -> bool {
        self.reserved_ft == DIR_TAIL_MARKER && self.reserved_zero1 == 0
    }
}

// =====================================================================
// Phase 4 Task 1：目录读半部（变长项解析 + 查找 + readdir + 块 csum 读）。
//
// 全部逐字节复刻 ext4_rs `ext4_impls/dir.rs` / `ext4_defs/direntry.rs`：
// - 变长项解析：8B 头 + name_len 字节 name（安全 `&[u8]` 切片，非 Pod）。
// - 防御非对称：查找路径坏 rec_len → EIO；枚举路径坏 rec_len → silent break。
// - 跨块：total_blocks=ceil(size/bs)，逐块 `get_pblock_idx_state`→读→扫。
// - 块 csum：ino_index = 块首项 inode（块 0 是 "." = 目录自身；块 ≥1 是子项 inode——
//   偏离 ext4 规范的 parity quirk，照搬）。
// =====================================================================

use super::crc::{ext4_crc32c, EXT4_CRC32_INIT};
use super::extents::{
    self, get_pblock_idx_state, BlockAlloc, RawExtent, WriteCtx,
};
use super::file::{self, ReadCtx};
use super::ialloc::InodeAllocator;
use super::inode::{init_new_inode, load_inode, write_back_inode, Inode};
use super::io::{BlockReader, BlockWriter};
use super::metadata_writer::MetadataWriter;
use super::superblock::RawSuperblock;

/// `RawDirEntryHeader` 定长头字节数（= ext4_rs `Ext4FakeDirEntry` 的 `size_of`=8）。
const DIR_ENTRY_HEADER_SIZE: usize = 8;

/// 目录块尾校验和结构字节数（= `size_of::<RawDirEntryTail>()`=12）。变长项遍历停于
/// `block_size - DIR_TAIL_SIZE`（tail 区，复刻 ext4_rs `size_of::<Ext4DirEntryTail>()`）。
const DIR_TAIL_SIZE: usize = size_of::<RawDirEntryTail>();

/// RO-compat metadata_csum 特性位（仅**门控目录块 csum 的读侧校验** `dir_verify_block_csum`；
/// **写侧 `dir_set_csum` 无条件写**，不看此位——BUG-22 复刻 ext4_rs 无门控行为）。
/// = ext4_rs `EXT4_FEATURE_RO_COMPAT_METADATA_CSUM`（0x400），与 `extents.rs` 同值。
const RO_COMPAT_METADATA_CSUM: u32 = 0x400;

/// 一条解析出的目录项（借用块字节，零拷贝）。`name` 是块内 `&[u8]` 切片（变长尾，
/// 非 Pod——安全切片即可，无需 from_le_bytes）。
#[derive(Clone, Copy, Debug)]
pub(in crate::fs::ext4) struct DirEntryRef<'a> {
    pub inode: u32,
    pub rec_len: u16,
    pub name_len: u8,
    pub file_type: u8,
    pub name: &'a [u8],
}

/// readdir 返回项：拥有 name 的目录项（脱离块缓冲生命周期）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::fs::ext4) struct OwnedDirEntry {
    pub inode: u32,
    pub file_type: u8,
    pub name: Vec<u8>,
}

/// 跨块查找命中：对齐 ext4_rs `Ext4DirSearchResult`（dentry.inode/pblock_id/offset/prev_offset）。
#[derive(Clone, Copy, Debug)]
pub(in crate::fs::ext4) struct DirSearchHit {
    pub inode: u32,
    pub pblock: Ext4Fsblk,
    pub offset: usize,
    pub prev_offset: usize,
}

/// 安全解析 `block[off..]` 处的一个目录项：8B 头 + `name_len` 字节 name。
///
/// 边界检查：头需 8 字节、name 区 `[off+8, off+8+name_len)` 须落在 `block_size` 内；
/// 越界 → EIO（损坏防御）。ext4_rs 用 `read_offset_as`（unsafe 指针读 264B 整结构）+
/// `&self.name[..name_len]`；这里改为安全切片解析（头 Pod `from_bytes`，name 字节切片）。
pub(in crate::fs::ext4) fn parse_entry<'a>(
    block: &'a [u8],
    off: usize,
    block_size: usize,
) -> Result<DirEntryRef<'a>> {
    if off + DIR_ENTRY_HEADER_SIZE > block_size || off + DIR_ENTRY_HEADER_SIZE > block.len() {
        return Err(Error::with_message(
            Errno::EIO,
            "dir entry header out of block",
        ));
    }
    let h = RawDirEntryHeader::from_bytes(&block[off..off + DIR_ENTRY_HEADER_SIZE]);
    let name_len = h.name_len() as usize;
    let name_start = off + DIR_ENTRY_HEADER_SIZE;
    let name_end = name_start + name_len;
    if name_end > block_size || name_end > block.len() {
        return Err(Error::with_message(
            Errno::EIO,
            "dir entry name out of block",
        ));
    }
    Ok(DirEntryRef {
        inode: h.inode(),
        rec_len: h.rec_len(),
        name_len: h.name_len(),
        file_type: h.file_type,
        name: &block[name_start..name_end],
    })
}

/// 在单个目录块里线性查找名为 `name` 的项。命中返回 `Some((inode, offset, prev_offset))`，
/// 走完块未命中返回 `Ok(None)`。
///
/// PARITY（ext4_rs `dir_find_in_block`，dir.rs:116）：从 offset 0 起，停于
/// `block_size - 12`（tail 区）；`rec_len==0 || rec_len > block_size - off` → **EIO**
/// （查找路径与枚举路径的防御不同：枚举路径 silent break，此处返回错误）；空槽
/// （`inode==0`）跳过；name 用 `&[u8]` 字节比较（name_len 相等且字节相等）。
/// 末尾 ext4_rs 还调 `validate_inode_number`——core 在更上层（namespace）做，此处仅返回 inode。
pub(in crate::fs::ext4) fn dir_find_in_block(
    block: &[u8],
    name: &[u8],
    block_size: usize,
) -> Result<Option<(u32, usize, usize)>> {
    let mut off = 0usize;
    let mut prev_off = 0usize;
    while off < block_size - DIR_TAIL_SIZE {
        let de = parse_entry(block, off, block_size)?;
        let rec_len = de.rec_len as usize;
        // PARITY: dir_find_in_block 坏 rec_len → EIO（dir_get_entries 则 silent break）。
        if rec_len == 0 || rec_len > block_size - off {
            return Err(Error::with_message(
                Errno::EIO,
                "corrupted ext4 dir entry length",
            ));
        }
        // 跳空槽（inode==0），命中名字（name_len 相等且字节相等）。
        if de.inode != 0 && de.name_len as usize == name.len() && de.name == name {
            return Ok(Some((de.inode, off, prev_off)));
        }
        prev_off = off;
        off += rec_len;
    }
    Ok(None)
}

/// 跨目录块查找名为 `name` 的项。命中返回带物理块号的 [`DirSearchHit`]，走完所有块未命中
/// 返回 `Ok(None)`。
///
/// PARITY（ext4_rs `dir_find_entry`，dir.rs:55）：`total_blocks = ceil(dir.size()/bs)`，
/// 逐逻辑块 `get_pblock_idx_state`→读整块→`dir_find_in_block`。
/// 注意 ext4_rs 在调用方先判 `!is_dir → ENOTDIR`（namespace 层 dir_find_entry 开头），
/// 这里不判 is_dir（namespace 编排留 Task 3 复刻）。
///
/// 错误处理逐字复刻 ext4_rs（dir.rs:78-105）：**块映射**（`get_pblock_idx`）的 Err
/// `return Err(e)`（传播）；**块内查找**（`dir_find_in_block`）的结果只看 `r.is_ok()`——
/// 命中即返回，**任何 Err（含未命中 ENOENT 与坏 rec_len 的 EIO）都吞掉、扫下一块**。
/// 走完所有块后 ENOENT（core 用 `Ok(None)` 表达）。
pub(in crate::fs::ext4) fn dir_find_entry(
    ctx: &ReadCtx,
    dir: &Inode,
    name: &[u8],
) -> Result<Option<DirSearchHit>> {
    let block_size = ctx.block_size;
    let total_blocks = dir.size().div_ceil(block_size as u64);
    let mut buf = vec![0u8; block_size];
    let mut iblock = 0u64;
    while iblock < total_blocks {
        // PARITY: 只有块映射错误传播（ext4_rs `get_pblock_idx` 的 Err → `return Err(e)`）。
        // core `get_pblock_idx_state` 用 `Ok(None)` 表示 hole；ext4_rs `get_pblock_idx`
        // 对 i_size 内未映射块报 ENOENT 并传播，故这里把 `Ok(None)` 同样视作映射失败传播
        // ENOENT（有效目录 inode 的 i_size 内无 hole，此路实际不可达——保持 parity 选择）。
        let pblock = match get_pblock_idx_state(ctx.reader, ctx.sb, dir, iblock as Ext4Lblk)? {
            Some((pblock, _unwritten)) => pblock,
            None => {
                return Err(Error::with_message(
                    Errno::ENOENT,
                    "unmapped dir block within i_size",
                ));
            }
        };
        ctx.reader
            .read_at(pblock as usize * block_size, buf.as_mut_slice());
        // PARITY: ext4_rs dir_find_entry swallows dir_find_in_block Err (incl EIO corrupt
        // rec_len) and scans the next block; only block-mapping errors propagate
        // (ext4_impls/dir.rs:55).
        if let Ok(Some((inode, offset, prev_offset))) = dir_find_in_block(&buf, name, block_size) {
            return Ok(Some(DirSearchHit {
                inode,
                pblock,
                offset,
                prev_offset,
            }));
        }
        iblock += 1;
    }
    Ok(None)
}

/// 枚举目录的全部可见项（含 '.'/'..'）。
///
/// PARITY（ext4_rs `dir_get_entries`，dir.rs:155）：逐块，坏 rec_len → **silent break**
/// （跳出本块项循环，不报错，区别于 `dir_find_in_block` 的 EIO）；空槽 `inode==0` 跳过；
/// 停于 `block_size - 12`（tail 区）；`get_pblock_idx_state` 的 Err/None **静默跳过该块**
/// （ext4_rs `if let Ok(fblock) = get_pblock_idx`——错误/未映射都不收任何项、不报错）。
pub(in crate::fs::ext4) fn dir_get_entries(ctx: &ReadCtx, dir: &Inode) -> Vec<OwnedDirEntry> {
    dir_enumerate(ctx, dir)
        .into_iter()
        .map(|(e, _next, _start)| e)
        .collect()
}

/// 同 [`dir_get_entries`]，但每项附带 readdir 续读用的**下一项绝对字节偏移**
/// `next_offset = iblock*block_size + off + rec_len`。
///
/// PARITY（ext4_rs `dir_get_entries_with_next_offset`，dir.rs:205）。
pub(in crate::fs::ext4) fn dir_get_entries_with_next_offset(
    ctx: &ReadCtx,
    dir: &Inode,
) -> Vec<(OwnedDirEntry, usize)> {
    dir_enumerate(ctx, dir)
        .into_iter()
        .map(|(e, next_off, _start)| (e, next_off))
        .collect()
}

/// 同 [`dir_get_entries`]，但每项附带**项自身**起始绝对字节偏移
/// `start_offset = iblock*block_size + off`。
///
/// 区别于 [`dir_get_entries_with_next_offset`]（那是**下一项**偏移，readdir 续读 cookie 用）：
/// 这里给的是该项头部所在的 abs offset，正是 [`dir_remove_entry_at_offset`] 删项所需的偏移。
///
/// PARITY（ext4_rs `ext4_readdir_with_offsets`，simple_interface/mod.rs:591：
/// `entry_offset = iblock*block_size + offset`，项自身偏移）。`dir_get_entries_with_next_offset`
/// 与 `ext4_readdir_with_offsets` 同一遍历、同一 skip/break 规则，仅采集偏移语义不同。
pub(in crate::fs::ext4) fn dir_get_entries_with_start_offset(
    ctx: &ReadCtx,
    dir: &Inode,
) -> Vec<(OwnedDirEntry, usize)> {
    dir_enumerate(ctx, dir)
        .into_iter()
        .map(|(e, _next, start_off)| (e, start_off))
        .collect()
}

/// 枚举的公共实现：每项产出 `(项, next_offset, start_offset)`。
/// `next_offset = iblock*bs + off + rec_len`（下一项偏移，readdir 续读 cookie）；
/// `start_offset = iblock*bs + off`（项自身偏移，删项用）。两者均 O(0) 由游标算出，
/// 始终计算、调用方各取所需。
fn dir_enumerate(ctx: &ReadCtx, dir: &Inode) -> Vec<(OwnedDirEntry, usize, usize)> {
    let block_size = ctx.block_size;
    let mut entries: Vec<(OwnedDirEntry, usize, usize)> = Vec::new();
    // PARITY: ext4_rs dir_get_entries 不判 is_dir（namespace 层判）；非目录其 size 仍按
    // 块遍历——这里同样直接按 size 遍历（差分对拍仅传目录 inode）。
    let total_blocks = dir.size().div_ceil(block_size as u64);
    let mut buf = vec![0u8; block_size];
    let mut iblock = 0u64;
    while iblock < total_blocks {
        // PARITY: get_pblock_idx_state Err/None 静默跳块（ext4_rs `if let Ok(...)`）。
        if let Ok(Some((pblock, _unwritten))) =
            get_pblock_idx_state(ctx.reader, ctx.sb, dir, iblock as Ext4Lblk)
        {
            ctx.reader
                .read_at(pblock as usize * block_size, buf.as_mut_slice());
            let mut off = 0usize;
            while off < block_size - DIR_TAIL_SIZE {
                // 解析失败（越界）等同坏项 → silent break（与 ext4_rs 防御位置一致）。
                let de = match parse_entry(&buf, off, block_size) {
                    Ok(de) => de,
                    Err(_) => break,
                };
                let rec_len = de.rec_len as usize;
                // PARITY: dir_get_entries 坏 rec_len → silent break（非 EIO）。
                if rec_len == 0 || rec_len > block_size - off {
                    break;
                }
                if de.inode != 0 {
                    let start_offset = iblock as usize * block_size + off;
                    let next_offset = start_offset + rec_len;
                    entries.push((
                        OwnedDirEntry {
                            inode: de.inode,
                            file_type: de.file_type,
                            name: de.name.to_vec(),
                        },
                        next_offset,
                        start_offset,
                    ));
                }
                off += rec_len;
            }
        }
        iblock += 1;
    }
    entries
}

/// 计算一个目录块的 crc32c 校验和（不门控——调用方判 metadata_csum）。
///
/// PARITY（ext4_rs `Ext4DirEntry::ext4_dir_get_csum`，direntry.rs:185 / `dir_set_csum`，
/// dir.rs:252）：`ino_index` = **块首项（offset 0）的 inode 字段**（块 0 是 "." = 目录
/// 自身 inode；块 ≥1 是普通子项 inode——偏离 ext4 规范的 parity quirk，照搬，多块目录
/// e2fsck 互操作风险见 bug.md D 段）。序列：
///   c = crc32c(INIT, uuid[..16])
///   c = crc32c(c, ino_index.le4)
///   c = crc32c(c, ino_gen.le4)
///   c = crc32c(c, block[..block_size-12])   // tail 区不入校验
/// 标量 `to_le_bytes`（ino_index/ino_gen 是喂给 crc 的整数，非磁盘结构解析）允许。
pub(in crate::fs::ext4) fn dir_block_csum(
    sb: &RawSuperblock,
    block: &[u8],
    ino_index: u32,
    ino_gen: u32,
) -> u32 {
    let block_size = sb.block_size();
    let data_len = block_size - DIR_TAIL_SIZE;
    let uuid = sb.uuid();
    let mut c = ext4_crc32c(EXT4_CRC32_INIT, &uuid);
    c = ext4_crc32c(c, &ino_index.to_le_bytes());
    c = ext4_crc32c(c, &ino_gen.to_le_bytes());
    // ext4_rs 复制 block[..min(len, data_len)] 进零填缓冲再喂 crc；此处块恒为 block_size
    // 字节，data_len <= block.len()，直接切片等价。
    let take = core::cmp::min(block.len(), data_len);
    if take == data_len {
        c = ext4_crc32c(c, &block[..data_len]);
    } else {
        // 块短于预期：零填到 data_len（复刻 ext4_rs 的 vec![0; data_len] + copy_len）。
        let mut data = vec![0u8; data_len];
        data[..take].copy_from_slice(&block[..take]);
        c = ext4_crc32c(c, &data);
    }
    c
}

/// 校验目录块尾的 csum 是否与重算值一致。**门控 metadata_csum**：特性关时直接返回 true
/// （不校验，复刻 ext4_rs 写侧门控；`EXT4_NOCSUM_IMAGE` 覆盖此路径）。
///
/// `ino_index` 内部取块首项 inode（`parse_entry(block, 0)`），与 `dir_set_csum` 一致；
/// `tail.checksum` 在块 `[block_size-4, block_size)`（tail 内 checksum 字段 @+8）。
pub(in crate::fs::ext4) fn dir_verify_block_csum(sb: &RawSuperblock, block: &[u8], ino_gen: u32) -> bool {
    let has_csum = (sb.features_read_only() & RO_COMPAT_METADATA_CSUM) != 0;
    if !has_csum {
        return true;
    }
    let block_size = sb.block_size();
    // ino_index = 块首项 inode。
    let ino_index = match parse_entry(block, 0, block_size) {
        Ok(de) => de.inode,
        Err(_) => return false,
    };
    let want = dir_block_csum(sb, block, ino_index, ino_gen);
    let tail = RawDirEntryTail::from_bytes(&block[block_size - DIR_TAIL_SIZE..block_size]);
    tail.checksum == want
}

// =====================================================================
// Phase 4 Task 2：目录写半部（切槽插入 / 删除合并 / 块 csum 写 / 追加新块 + CRUD）。
//
// 全部逐字节复刻 ext4_rs `ext4_impls/dir.rs` / `ext4_defs/direntry.rs`。关键 PARITY 雷区：
// - **264/8 不对称**（`try_insert_to_existing_block`）：插入所需空间下界
//   `required_len = align4(264 + name.len())` 用 264（`size_of::<Ext4DirEntry>`，内存结构尺寸
//   泄漏进磁盘布局）；现有项「实际占用」`sz = align4(8 + name_len)` 用 8
//   （`size_of::<Ext4FakeDirEntry>`）。两者不对称是 ext4_rs 的真实算法。
// - **264 字节写**：ext4_rs 写新项经 `copy_to_slice` 拷满 264 字节（8B 头 + name +
//   name[name_len..255] 零填 + 对齐）。core 构造 264 字节零缓冲整体写入，**不是**只写
//   `8+name_len`——否则 `[off+8+name_len .. off+264]` 的零填区字节不等、差分必败。
// - **块 csum ino_index = 块首项 inode**（`dir_set_csum`，复用 Task 1 `dir_block_csum`）。
// - **删项合并：首项删不合并**（`offset==0` 空间不可达，parity）。
// - **dir 块写经 MetadataWriter**（目录块在 ext4 里是元数据，须进 JBD2），不经 data_writer。
//   （`WriteCtx.writer: &dyn MetadataWriter`——调 `write_metadata_for_handle` 经 dyn 对象，
//   无需把 trait 引入本作用域。）
// =====================================================================

/// ext4 inode 类型位掩码（mode & 0xF000）。
const S_IFMT: u16 = 0xF000;
const S_IFIFO: u16 = 0x1000;
const S_IFCHR: u16 = 0x2000;
const S_IFDIR_MODE: u16 = 0x4000;
const S_IFBLK: u16 = 0x6000;
const S_IFLNK: u16 = 0xA000;
const S_IFSOCK: u16 = 0xC000;

/// DE filetype 字节值（与 [`DirEntryFileType`] 同枚举值）。
const DE_REG_FILE: u8 = 1;
const DE_DIR: u8 = 2;
const DE_CHRDEV: u8 = 3;
const DE_BLKDEV: u8 = 4;
const DE_FIFO: u8 = 5;
const DE_SOCK: u8 = 6;
const DE_SYMLINK: u8 = 7;

/// 4 字节对齐向上取整（复刻 ext4_rs 的 `if len%4!=0 { len += 4 - len%4 }`）。
fn align4(len: usize) -> usize {
    if len % 4 != 0 {
        len + (4 - len % 4)
    } else {
        len
    }
}

/// 把一个目录项构造成 **264 字节零填缓冲**并写入 `buf[off..off+264]`。
///
/// PARITY（ext4_rs `Ext4DirEntry::write_entry` + `copy_to_slice`，direntry.rs:171/217）：
/// ext4_rs 的 `Ext4DirEntry` 是 264 字节内存结构（8B 头 + `name:[u8;255]` + 对齐），写项时
/// `copy_to_slice` 整体拷 264 字节（`size_of::<Ext4DirEntry>`），含 name 尾 `[name_len..255]`
/// 的零填 + 1 字节对齐。core 必须逐字复刻：8B 头（inode/rec_len/name_len/file_type）+ name
/// 字节在 `[8..8+name_len]`，其余 `[8+name_len..264]` 保持 0，整 264 字节写入。
/// `// PARITY: 写满 EXT4_DIR_ENTRY_INMEM_SIZE`（264），**不是**只写 `8+name_len`。
///
/// 调用方保证 `off + 264 <= buf.len()`（切槽判据 `required_len >= 264` 已门控）。
pub(in crate::fs::ext4) fn dir_write_entry_bytes(
    buf: &mut [u8],
    off: usize,
    inode: u32,
    rec_len: u16,
    name: &[u8],
    file_type: u8,
) {
    // PARITY: 写满 EXT4_DIR_ENTRY_INMEM_SIZE(264)（内存结构尺寸泄漏进磁盘布局）。
    let mut entry = [0u8; EXT4_DIR_ENTRY_INMEM_SIZE];
    let header = RawDirEntryHeader {
        inode,
        rec_len,
        name_len: name.len() as u8,
        file_type,
    };
    entry[..DIR_ENTRY_HEADER_SIZE].copy_from_slice(header.as_bytes());
    entry[DIR_ENTRY_HEADER_SIZE..DIR_ENTRY_HEADER_SIZE + name.len()].copy_from_slice(name);
    // 其余 [8+name_len..264] 保持 0（复刻 ext4_rs name 尾零填 + 对齐）。
    buf[off..off + EXT4_DIR_ENTRY_INMEM_SIZE].copy_from_slice(&entry);
}

/// 在目录块字节上写 tail.checksum（**无条件写，不门控 metadata_csum**——BUG-22 parity）。
///
/// PARITY（ext4_rs `dir_set_csum`，dir.rs:252）：`ino_index = parse(block, 0).inode`
/// （**块首项 inode**——块 0 是 "." = 目录自身，块 ≥1 是普通子项 inode，偏离规范但照搬）；
/// 重算 [`dir_block_csum`]（复用 Task 1）；把结果写进 tail 的 checksum 字段
/// （`block[block_size-4 .. block_size]`）。**写路径无条件**：ext4_rs 无视 metadata_csum 特性、
/// 关 csum 的盘上也照写（BUG-22），故 core 写路径也无条件写以保 parity。读路径
/// `dir_verify_block_csum` **仍门控**（ext4_rs 读不校验目录 csum，读/写非对称是忠实的）。
pub(in crate::fs::ext4) fn dir_set_csum(block: &mut [u8], sb: &RawSuperblock, ino_gen: u32, block_size: usize) {
    // PARITY: ext4_rs dir_set_csum writes the dir-block csum unconditionally, ignoring the
    // metadata_csum feature gate (ext4_impls/dir.rs:252 + ext4_defs/direntry.rs:185; all 7
    // call sites un-gated) — replicate exactly, even on a metadata_csum-off filesystem; this
    // is logged as BUG-22. (READ-side dir_verify_block_csum keeps its gate: ext4_rs never
    // verifies dir csums on read, so that asymmetry is faithful too.)
    // PARITY: ino_index = 块首项 inode（dir_set_csum 读 block[0] 当 parent_de）。
    let ino_index = match parse_entry(block, 0, block_size) {
        Ok(de) => de.inode,
        Err(_) => return,
    };
    let csum = dir_block_csum(sb, block, ino_index, ino_gen);
    // tail 在 [block_size-12, block_size)；checksum 是 tail 内偏移 +8 的 4 字节 → [block_size-4, block_size)。
    let mut tail = RawDirEntryTail::from_bytes(&block[block_size - DIR_TAIL_SIZE..block_size]);
    tail.checksum = csum;
    block[block_size - DIR_TAIL_SIZE..block_size].copy_from_slice(tail.as_bytes());
}

/// 在一个**已有**目录块里切槽插入新项；成功返回新项的 within-block 偏移。
///
/// PARITY（ext4_rs `try_insert_to_existing_block`，dir.rs:490）：
/// - `required_len = align4(264 + name.len())`（**用 264**）；
/// - 从 offset 0 起遍历，停于 `block_size - 12`（tail 区）；
/// - `rec_len==0 || rec_len > block_size - off` → **EIO**；
/// - 空槽（`inode==0`）`off += rec_len; continue`；
/// - 活动项：`sz = align4(8 + name_len)`（**用 8**），`free_space = rec_len - sz`；
/// - `free_space >= required_len` → 切槽：现有项 rec_len 缩到 `sz`（写 2 字节 @off+4），新项
///   写在 `off+sz`、`rec_len = free_space`（**全部剩余空间**），返回 `off+sz`；
/// - 走完无槽 → **ENOSPC**。
///
/// **264 字节写 parity**：新项经 [`dir_write_entry_bytes`] 写满 264 字节（`required_len >= 264`
/// 保证 `off+sz+264 <= off+sz+free_space <= block_size-12 < block_size`，不会越界）。
/// **现有项缩短的 parity**：ext4_rs 把读出的 264 字节内存结构（仅改 rec_len）整体写回，等价于
/// 只改盘上 `[off+4..off+6]` 这 2 字节（头其余 + name + 尾填字节原样不动）——core 只写这 2 字节，
/// 落盘逐字节一致。
pub(in crate::fs::ext4) fn try_insert_to_existing_block(
    block: &mut [u8],
    name: &[u8],
    child_inode: u32,
    de_type: u8,
    block_size: usize,
) -> Result<usize> {
    // PARITY: required_len 用 EXT4_DIR_ENTRY_INMEM_SIZE(264)，非 8。
    let required_len = align4(EXT4_DIR_ENTRY_INMEM_SIZE + name.len());

    let mut off = 0usize;
    while off < block_size - DIR_TAIL_SIZE {
        let de = parse_entry(block, off, block_size)?;
        let rec_len = de.rec_len as usize;
        if rec_len == 0 || rec_len > block_size - off {
            return Err(Error::with_message(
                Errno::EIO,
                "corrupted ext4 dir entry length",
            ));
        }
        if de.inode == 0 {
            off += rec_len;
            continue;
        }
        // PARITY: 现有项实际占用 sz 用 8（size_of::<Ext4FakeDirEntry>），非 264。
        let sz = align4(DIR_ENTRY_HEADER_SIZE + de.name_len as usize);
        let free_space = rec_len - sz;
        if free_space >= required_len {
            // 现有项 rec_len 缩到 sz：只改 2 字节 @off+4（PARITY: 头其余 + name 不动）。
            block[off + 4..off + 6].copy_from_slice(&(sz as u16).to_le_bytes());
            // 新项写满 264 字节，rec_len = 全部剩余空间 free_space。
            dir_write_entry_bytes(block, off + sz, child_inode, free_space as u16, name, de_type);
            return Ok(off + sz);
        }
        off += rec_len;
    }
    Err(Error::with_message(
        Errno::ENOSPC,
        "No space in block for new entry",
    ))
}

/// 把一个**新分配**的目录块初始化为「首项 + tail」。
///
/// PARITY（ext4_rs `insert_to_new_block`，dir.rs:567）：`el = block_size - 12`；首项写在
/// offset 0、`rec_len = el`（占满整块除 tail），经 [`dir_write_entry_bytes`] 写满 264 字节；
/// tail（reserved_zero1=0, rec_len=12, reserved_zero2=0, reserved_ft=0xDE, checksum=0）写在
/// `block_size-12`；中间 `[264..block_size-12]` 保持 0。（块 csum 由调用方随后 `dir_set_csum` 写。）
pub(in crate::fs::ext4) fn insert_to_new_block(
    block: &mut [u8],
    inode: u32,
    name: &[u8],
    de_type: u8,
    block_size: usize,
) {
    let el = (block_size - DIR_TAIL_SIZE) as u16;
    // 首项写满 264 字节，rec_len = block_size - 12。
    dir_write_entry_bytes(block, 0, inode, el, name, de_type);
    // tail：reserved_ft=0xDE、rec_len=12，其余 0（含 checksum，随后 dir_set_csum 覆盖）。
    let tail = RawDirEntryTail {
        reserved_zero1: 0,
        rec_len: DIR_TAIL_SIZE as u16,
        reserved_zero2: 0,
        reserved_ft: DIR_TAIL_MARKER,
        checksum: 0,
    };
    block[block_size - DIR_TAIL_SIZE..block_size].copy_from_slice(tail.as_bytes());
}

/// 给目录追加一个新数据块；返回 `(物理块号, 逻辑块号)`。
///
/// PARITY（ext4_rs `append_inode_pblk` extent 分支，inode.rs:422）：
/// `iblock = (dir.size() / block_size) as u32`；`pblock = alloc.alloc_one(dir)`；
/// `newex = RawExtent{first_block:iblock, start:pblock, block_count:1}`；
/// `insert_extent(ctx, alloc, dir, &newex)`；`dir.set_size(dir.size() + block_size)`；
/// `write_back_inode(...)`。（core 只支持 extent 目录——真镜像目录均 extent-mapped。）
pub(in crate::fs::ext4) fn dir_append_block(
    ctx: &WriteCtx,
    alloc: &mut dyn BlockAlloc,
    dir: &mut Inode,
) -> Result<(Ext4Fsblk, u32)> {
    let block_size = ctx.block_size;
    let iblock = (dir.size() / block_size as u64) as u32;
    let pblock = alloc.alloc_one(dir)?;
    let mut newex = RawExtent::default();
    newex.first_block = iblock;
    newex.store_pblock(pblock);
    // PARITY: ext4_rs `block_count = min(1, EXT_MAX_BLOCKS - iblock)`；正常 iblock 远小于上界，
    // 故恒为 1（单块 append）。
    newex.set_actual_len(1);
    extents::insert_extent(ctx, alloc, dir, &newex)?;
    dir.set_size(dir.size() + block_size as u64);
    write_back_inode(ctx.writer, ctx.reader, ctx.sb, dir)?;
    Ok((pblock, iblock))
}

/// 读目录的第 `iblock` 个逻辑块的物理块号（映射失败 → ENOENT，复刻 ext4_rs `get_pblock_idx`）。
fn dir_pblock(ctx: &WriteCtx, dir: &Inode, iblock: u32) -> Result<Ext4Fsblk> {
    match get_pblock_idx_state(ctx.reader, ctx.sb, dir, iblock as Ext4Lblk)? {
        Some((pblock, _unwritten)) => Ok(pblock),
        None => Err(Error::with_message(
            Errno::ENOENT,
            "unmapped dir block within i_size",
        )),
    }
}

/// 把目录块整块读进 `block_size` 字节缓冲。
fn read_dir_block(ctx: &WriteCtx, pblock: Ext4Fsblk) -> Vec<u8> {
    let block_size = ctx.block_size;
    let mut buf = vec![0u8; block_size];
    ctx.reader
        .read_at(pblock as usize * block_size, buf.as_mut_slice());
    buf
}

/// 把整块写回盘 + 写块尾 csum。**dir 块经 [`MetadataWriter`]**（目录块是 ext4 元数据），
/// handle_id=0（与 [`write_back_inode`] 约定一致）。
fn write_dir_block(ctx: &WriteCtx, pblock: Ext4Fsblk, block: &mut [u8], ino_gen: u32) -> Result<()> {
    let block_size = ctx.block_size;
    dir_set_csum(block, ctx.sb, ino_gen, block_size);
    // PARITY: dir 块经 MetadataWriter，不经 data_writer（dir 块是元数据）。
    ctx.writer.write_metadata_for_handle(0, pblock, block)
}

/// 给目录 `parent` 添加一个名为 `name`、指向 `child_ino`（类型 `child_ftype`）的项。
///
/// PARITY（ext4_rs `dir_add_entry`，dir.rs:352）：
/// 1. fast-path：末块 `try_insert_to_existing_block`，成功即写块（含 csum）返回；
/// 2. 慢扫：从 iblock 0 起逐块 `try_insert`，成功即写块返回；
/// 3. 无槽：`dir_append_block` 新建块 + `insert_to_new_block` 写首项 + 写块返回。
/// 每次写块经 [`write_dir_block`]（MetadataWriter + csum）。
pub(in crate::fs::ext4) fn dir_add_entry(
    ctx: &WriteCtx,
    alloc: &mut dyn BlockAlloc,
    parent: &mut Inode,
    child_ino: u32,
    child_ftype: u8,
    name: &[u8],
) -> Result<()> {
    let block_size = ctx.block_size;
    let ino_gen = parent.raw.generation();
    let total_blocks = parent.size().div_ceil(block_size as u64);

    // ① fast-path：末块。PARITY: ext4_rs 对末块映射的 ENOENT 视作 None（跳过 fast-path），
    //    其它 Err 传播；core get_pblock_idx_state 用 Ok(None) 表示 hole（同 None 处理）。
    if total_blocks > 0 {
        let last_iblock = (total_blocks - 1) as u32;
        let pblock = get_pblock_idx_state(ctx.reader, ctx.sb, parent, last_iblock as Ext4Lblk)?
            .map(|(pblock, _unwritten)| pblock);
        if let Some(pblock) = pblock {
            let mut block = read_dir_block(ctx, pblock);
            if try_insert_to_existing_block(&mut block, name, child_ino, child_ftype, block_size)
                .is_ok()
            {
                write_dir_block(ctx, pblock, &mut block, ino_gen)?;
                return Ok(());
            }
        }
    }

    // ② 慢扫全块。PARITY: ext4_rs 对 hole 块（ENOENT）`continue` 跳过；其它映射 Err 传播。
    let mut iblock = 0u64;
    while iblock < total_blocks {
        let pblock = get_pblock_idx_state(ctx.reader, ctx.sb, parent, iblock as Ext4Lblk)?
            .map(|(pblock, _unwritten)| pblock);
        if let Some(pblock) = pblock {
            let mut block = read_dir_block(ctx, pblock);
            if try_insert_to_existing_block(&mut block, name, child_ino, child_ftype, block_size)
                .is_ok()
            {
                write_dir_block(ctx, pblock, &mut block, ino_gen)?;
                return Ok(());
            }
        }
        iblock += 1;
    }

    // ③ 无槽：新建块 + 首项。
    let (pblock, _new_iblock) = dir_append_block(ctx, alloc, parent)?;
    let mut block = read_dir_block(ctx, pblock);
    insert_to_new_block(&mut block, child_ino, name, child_ftype, block_size);
    // append 后 generation 不变；用同一 ino_gen。
    write_dir_block(ctx, pblock, &mut block, ino_gen)?;
    Ok(())
}

/// 同 [`dir_add_entry`]，但**只动末块**（O(1)，跳过慢扫）；返回新项的 abs byte offset。
///
/// PARITY（ext4_rs `dir_add_entry_unchecked`，dir.rs:289）：末块 `try_insert` 成功返回
/// `last_iblock*bs + within_offset`；末块满（或目录空）→ `dir_append_block` + `insert_to_new_block`
/// 返回 `new_iblock*bs`。**仅在名字保证不存在 + 目录 append-dominated 时用**（无早块空槽）。
pub(in crate::fs::ext4) fn dir_add_entry_unchecked(
    ctx: &WriteCtx,
    alloc: &mut dyn BlockAlloc,
    parent: &mut Inode,
    child_ino: u32,
    child_ftype: u8,
    name: &[u8],
) -> Result<u64> {
    let block_size = ctx.block_size;
    let ino_gen = parent.raw.generation();
    let total_blocks = parent.size().div_ceil(block_size as u64);

    if total_blocks > 0 {
        let last_iblock = total_blocks - 1;
        let pblock = dir_pblock(ctx, parent, last_iblock as u32)?;
        let mut block = read_dir_block(ctx, pblock);
        if let Ok(within_offset) =
            try_insert_to_existing_block(&mut block, name, child_ino, child_ftype, block_size)
        {
            write_dir_block(ctx, pblock, &mut block, ino_gen)?;
            return Ok(last_iblock * block_size as u64 + within_offset as u64);
        }
    }

    let new_iblock = total_blocks;
    let (pblock, _iblk) = dir_append_block(ctx, alloc, parent)?;
    let mut block = read_dir_block(ctx, pblock);
    insert_to_new_block(&mut block, child_ino, name, child_ftype, block_size);
    write_dir_block(ctx, pblock, &mut block, ino_gen)?;
    Ok(new_iblock * block_size as u64)
}

/// 在目录块字节上删除位于 `offset` 的项：置 inode=0，必要时把空间合并进前驱。
///
/// PARITY（ext4_rs `dir_remove_entry`/`_at_offset` 的合并逻辑，dir.rs:601-636）：
/// - `block[offset..offset+4] = 0`（inode=0 标删）；
/// - `offset != 0` → 从 0 起走项找前驱 `p`（`p + rec_len(p) == offset`），`pred.rec_len +=
///   deleted.rec_len`（写 2 字节 @pred_off+4）；走项遇坏 rec_len（0/越界）或前驱推算不符 → **EIO**；
/// - `offset == 0`（块首项）→ **不合并**（空间不可达，parity）。
///
/// 被删项原 rec_len 在置 inode=0 前先读出（合并进前驱）；与 ext4_rs 用搜索结果 `dentry` 的
/// rec_len 等价（块未变，re-parse 同值）。
fn remove_entry_in_block(block: &mut [u8], offset: usize, block_size: usize) -> Result<()> {
    // 被删项的 rec_len（合并前先读，置 inode=0 不影响 rec_len 字段）。
    let del = parse_entry(block, offset, block_size)?;
    let del_rec_len = del.rec_len;

    // 置 inode=0（标删）。
    block[offset..offset + 4].fill(0);

    // PARITY: 首项（offset==0）不合并。
    if offset != 0 {
        let mut off = 0usize;
        let mut de = parse_entry(block, off, block_size)?;
        let mut de_len = de.rec_len as usize;
        if de_len == 0 || de_len > block_size - off {
            return Err(Error::with_message(
                Errno::EIO,
                "corrupted ext4 dir entry length",
            ));
        }
        // 找直接前驱：`off + de_len < offset` 时前进。
        while off + de_len < offset {
            off += de_len;
            de = parse_entry(block, off, block_size)?;
            de_len = de.rec_len as usize;
            if de_len == 0 || de_len > block_size - off {
                return Err(Error::with_message(
                    Errno::EIO,
                    "corrupted ext4 dir entry length",
                ));
            }
        }
        if de_len + off != offset {
            return Err(Error::with_message(
                Errno::EIO,
                "invalid predecessor calculation",
            ));
        }
        // pred.rec_len += deleted.rec_len（写 2 字节 @off+4）。
        let merged = (de_len as u16).wrapping_add(del_rec_len);
        block[off + 4..off + 6].copy_from_slice(&merged.to_le_bytes());
    }
    Ok(())
}

/// 按名删除目录项（find → inode=0 → 前驱合并 → csum → 写块）。
///
/// PARITY（ext4_rs `dir_remove_entry`，dir.rs:588）：`dir_find_entry` 定位（命中 pblock +
/// within-block offset），在该块上 [`remove_entry_in_block`]，写块（MetadataWriter + csum）。
/// 未命中 → 传播 `dir_find_entry` 的 ENOENT（core 用 `Err(ENOENT)`，对齐 ext4_rs `?`）。
pub(in crate::fs::ext4) fn dir_remove_entry(ctx: &WriteCtx, parent: &mut Inode, name: &[u8]) -> Result<()> {
    let block_size = ctx.block_size;
    let ino_gen = parent.raw.generation();
    let rctx = ctx.read_ctx();
    let hit = dir_find_entry(&rctx, parent, name)?;
    let hit = match hit {
        Some(h) => h,
        None => {
            // PARITY: ext4_rs dir_find_entry 走完未命中 → ENOENT（`?` 传播）。
            return Err(Error::with_message(Errno::ENOENT, "dir search fail"));
        }
    };
    let mut block = read_dir_block(ctx, hit.pblock);
    remove_entry_in_block(&mut block, hit.offset, block_size)?;
    write_dir_block(ctx, hit.pblock, &mut block, ino_gen)?;
    Ok(())
}

/// 按 abs byte offset 删除目录项（不扫全块）。
///
/// PARITY（ext4_rs `dir_remove_entry_at_offset`，dir.rs:647）：`iblock = abs_off / bs`、
/// `offset_in_block = abs_off % bs`；映射 iblock → pblock（失败传播）；在块内
/// [`remove_entry_in_block`]；写块（MetadataWriter + csum）。
pub(in crate::fs::ext4) fn dir_remove_entry_at_offset(
    ctx: &WriteCtx,
    parent: &mut Inode,
    abs_off: u64,
) -> Result<()> {
    let block_size = ctx.block_size;
    let ino_gen = parent.raw.generation();
    let iblock = (abs_off as usize) / block_size;
    let offset_in_block = (abs_off as usize) % block_size;
    let pblock = dir_pblock(ctx, parent, iblock as u32)?;
    let mut block = read_dir_block(ctx, pblock);
    remove_entry_in_block(&mut block, offset_in_block, block_size)?;
    write_dir_block(ctx, pblock, &mut block, ino_gen)?;
    Ok(())
}

/// 目录是否含 '.'/'..' 之外的项（判空：rmdir 用）。
///
/// PARITY（ext4_rs `dir_has_entry`，dir.rs:696）：逐块逐项，坏 rec_len → **EIO**；
/// 先 `off += rec_len`（ext4_rs 在 skip 判定**前**已前进 offset）；空槽（`inode==0`）跳过；
/// '.'/'..' 跳过；遇其它项即 `Ok(true)`；走完 `Ok(false)`。块映射失败传播（`?`）。
/// 注意 ext4_rs 开头判 `!is_dir → ENOTDIR`（namespace 层），core 此处不判（Task 3 编排时已确保是目录）。
pub(in crate::fs::ext4) fn dir_has_entry(ctx: &ReadCtx, dir: &Inode) -> Result<bool> {
    let block_size = ctx.block_size;
    let total_blocks = dir.size().div_ceil(block_size as u64);
    let mut buf = vec![0u8; block_size];
    let mut iblock = 0u64;
    while iblock < total_blocks {
        // PARITY: ext4_rs `get_pblock_idx(&parent, iblock)?` 失败传播。
        let pblock = match get_pblock_idx_state(ctx.reader, ctx.sb, dir, iblock as Ext4Lblk)? {
            Some((pblock, _unwritten)) => pblock,
            None => {
                return Err(Error::with_message(
                    Errno::ENOENT,
                    "unmapped dir block within i_size",
                ));
            }
        };
        ctx.reader
            .read_at(pblock as usize * block_size, buf.as_mut_slice());
        let mut off = 0usize;
        while off < block_size - DIR_TAIL_SIZE {
            let de = parse_entry(&buf, off, block_size)?;
            let rec_len = de.rec_len as usize;
            if rec_len == 0 || rec_len > block_size - off {
                return Err(Error::with_message(
                    Errno::EIO,
                    "corrupted ext4 dir entry length",
                ));
            }
            // PARITY: ext4_rs 在 skip 判定前先 `offset += rec_len`。
            off += rec_len;
            if de.inode == 0 {
                continue;
            }
            // 跳 '.' / '..'。
            if de.name == b"." || de.name == b".." {
                continue;
            }
            return Ok(true);
        }
        iblock += 1;
    }
    Ok(false)
}

/// inode 类型 → DE filetype 字节。
///
/// PARITY（ext4_rs `inode_to_dir_entry_type`，dir.rs:264）：按 `mode & 0xF000` 分派——
/// DIR→2、SYMLINK→7、CHRDEV→3、BLKDEV→4、FIFO→5、SOCK→6、其它（含 REG）→1。
pub(in crate::fs::ext4) fn inode_to_dir_entry_type(inode: &Inode) -> u8 {
    match inode.raw.mode() & S_IFMT {
        S_IFDIR_MODE => DE_DIR,
        S_IFLNK => DE_SYMLINK,
        S_IFCHR => DE_CHRDEV,
        S_IFBLK => DE_BLKDEV,
        S_IFIFO => DE_FIFO,
        S_IFSOCK => DE_SOCK,
        _ => DE_REG_FILE,
    }
}

// =====================================================================
// Phase 4 Task 3：命名空间编排（create / mkdir / unlink / rmdir + lookup）。
//
// 逐字节复刻 ext4_rs `simple_interface/mod.rs`（ext4_create_at/ext4_mkdir_at/...）+
// `ext4_impls/file.rs`（create/create_unchecked/link/link_unchecked）+ `ext4_impls/ext4.rs`
// （unlink）+ `ext4_impls/dir.rs`（dir_remove）。关键 PARITY 雷区：
//
// - **链计数 (H)**：新 inode links_count 起 **0**；create 文件分支 link 后 child=1；mkdir
//   目录分支：写 '.'/'..' + child=2 + **parent nlink++**。'.' / '..' 的 DE filetype = DIR(2)。
// - **create 写回等价化**：ext4_rs `create` 做 `write_back_inode_without_csum(child)` → reload
//   → `link` → `write_back(parent)` + `write_back(child)`。那个「先写无 csum 再 reload」对**最终
//   盘字节不可见**（被末尾 csummed 写覆盖）。差分比**最终**盘面，故 core 直接在内存里把 child
//   构造完、跑 link 逻辑、再各 write_back 一次——最终 inode 表 / dir 块 / 位图 / GDT / SB 字节
//   与 ext4_rs 逐字节一致（write_back 是按 inode 表块 RMW，写序不改最终态；父子若同表块，每次
//   write_back 都重读最新块、各自落对字节）。
// - **unlink（文件，BUG-19 已修，ext4-spec-correct）**：`unlink` 删项 + 调 child nlink
//   （>1 减一否则置 0），**不**在此 free/截块——POSIX 语义：文件可被打开持有，最后一个引用关闭
//   前 inode 须保留。实际 free（截块 + 清 inode 位图位）由集成层 evict 钩子
//   （`Inode::cleanup_unlinked` → `cleanup_unlinked_file` → [`free_inode_on_evict_at`]）在
//   nlink==0 且无打开句柄时做，单事务、走 `run_journaled_namespace`。
// - **rmdir（dir_remove，BUG-19 已修）**：拒 '.'/'..' EINVAL → find ENOENT → 非目录 ENOTDIR →
//   dir_has_entry 非空 ENOTEMPTY → truncate_inode(child,0)（释放子数据块）→ unlink 目录分支
//   （父 nlink-1 + 子 nlink=0 + write_back 两者）→ **清子目录 inode 位图位**（dir 无打开句柄
//   语义，rmdir 即最后引用，立即 free，used_dirs_count 自减）→ write_back(parent)。
// - **SB/GDT/位图一致性**：InodeAllocator 与 BlockAllocator 各自持私有 RawSuperblock 快照，
//   且 `write_superblock` 写**整 1024 字节**——若两者各从陈旧快照写、后写者会用陈旧 free_inodes/
//   free_blocks 覆盖前写者。复刻 ext4_rs 单一权威 `super_block` 的办法：`NamespaceCtx` 持一份
//   **权威 SB**，每次分配前用它构造分配器、分配后把分配器的运行期 SB（`superblock()`）同步回权威
//   SB。这样块分配器看到的 SB 已含 inode 分配的计数变化，落盘的 SB 字节与 ext4_rs 一致。
//   GDT 写是「读整块 → 覆盖该组 64 字节 → 写」，每次操作都 `load_group_desc` 重读盘（含上次写），
//   故同组的 free_inodes / free_blocks / used_dirs 顺序写不互相覆盖；位图块互不相交。
// =====================================================================

use super::balloc::{BlockAllocator, InodeAllocCtx};

/// S_IFDIR mode 位（构造目录 inode 时 `mode | S_IFDIR`）。
const S_IFDIR_FULL: u16 = 0x4000;

/// 命名空间编排上下文：持读 / 元数据写 / 数据写接缝 + **权威可变超级块**。
///
/// 泛型 `R: BlockReader` / `W: MetadataWriter`（与 Phase-2 分配器同形）——`InodeAllocator` /
/// `BlockAllocator` 都要具体 sized 接缝类型，故本上下文也泛型化，由调用方（差分 / P6 集成层）
/// 传具体盘类型（如差分的 `MemDisk`）。数据写接缝 `data_writer` 仅为组装 [`WriteCtx`]（命名空间
/// 写路径只动元数据，不写文件数据块——dir 块走 metadata writer）。
///
/// **权威 SB 是 SB/GDT/位图一致性的关键**：分配 inode（InodeAllocator）与分配 dir 块
/// （BlockAllocator）各持私有 SB 快照并写整 1024 字节——若各从陈旧快照写，后写者会用陈旧
/// free_inodes/free_blocks 覆盖前写者。复刻 ext4_rs 单一权威 `super_block` 的办法：本上下文持
/// 一份 `sb`，**每次分配前据它构造分配器、分配后把分配器运行期 SB（`superblock()`）同步回**。
/// 这样块分配器看到的 SB 已含 inode 分配的计数变化，最终 SB 字节与 ext4_rs 一致。
pub(in crate::fs::ext4) struct NamespaceCtx<'a, R: BlockReader, W: MetadataWriter, D: BlockWriter> {
    reader: &'a R,
    writer: &'a W,
    data_writer: &'a D,
    /// 权威运行期超级块（free_inodes / free_blocks 随分配递减；其余同盘初值）。
    sb: RawSuperblock,
}

/// 把 Phase-2 `BlockAllocator` + `InodeAllocCtx` 适配成写半部要的 [`BlockAlloc`]——与
/// 前差分 harness / file.rs 的 `CoreAllocAdapter` 同套（分配 / 释放后把 i_blocks 同步回 inode）。
struct NamespaceBlockAlloc<'a, R: BlockReader, W: MetadataWriter> {
    alloc: BlockAllocator<'a, R, W>,
    ictx: InodeAllocCtx,
}

impl<'a, R: BlockReader, W: MetadataWriter> BlockAlloc for NamespaceBlockAlloc<'a, R, W> {
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
        inode.set_blocks_count(self.ictx.i_blocks());
    }
}

impl<'a, R: BlockReader, W: MetadataWriter, D: BlockWriter> NamespaceCtx<'a, R, W, D> {
    /// 用读 / 元数据写 / 数据写接缝 + 初始超级块构造。`sb` 应为操作开始时盘上 SB 的快照
    /// （差分两侧从同字节起步）。
    pub(in crate::fs::ext4) fn new(reader: &'a R, writer: &'a W, data_writer: &'a D, sb: RawSuperblock) -> Self {
        Self {
            reader,
            writer,
            data_writer,
            sb,
        }
    }

    /// 当前权威超级块（差分跑完据此对拍）。
    #[allow(dead_code)]
    pub(in crate::fs::ext4) fn superblock(&self) -> &RawSuperblock {
        &self.sb
    }

    /// 从权威 SB 借出只读上下文（读路径用）。
    fn read_ctx(&self) -> ReadCtx<'_> {
        ReadCtx::new(self.reader, &self.sb)
    }

    /// 从权威 SB 借出写上下文（dir 块 / inode 写回路径用）。
    fn write_ctx(&self) -> WriteCtx<'_> {
        WriteCtx::new(self.reader, self.writer, self.data_writer, &self.sb)
    }

    /// 分配一个 inode（按 `is_dir` 走 ialloc 计数分支），返回 1-based inode 号，并把分配器
    /// 运行期 SB 同步回权威 `self.sb`。复刻 ext4_rs `alloc_inode`。
    fn alloc_inode(&mut self, is_dir: bool) -> Result<u32> {
        let mut ialloc = InodeAllocator::new(self.sb, self.reader, self.writer);
        let ino = ialloc.ialloc_alloc_inode(is_dir)?;
        // 关键：把 inode 分配后的运行期 SB（free_inodes-1 等）同步回权威 SB，使后续块分配器
        // 据此构造、落盘 SB 不丢 inode 计数变化。
        self.sb = *ialloc.superblock();
        Ok(ino)
    }

    /// 释放 inode 号 `index`（按 `is_dir` 回退 used_dirs_count），把分配器运行期 SB
    /// （free_inodes+1）同步回权威 `self.sb`。
    ///
    /// [对照] ext4_rs `ialloc_free_inode`（ext4_impls/ialloc.rs:85）+ Linux `ext4_free_inode`
    /// （清 inode 位图位 + 组/SB `free_inodes_count` 自增 + 目录 `used_dirs_count` 自减）。
    /// 与 [`alloc_inode`] 对称：先用权威 SB 造 `InodeAllocator`，释放后回灌运行期 SB，使
    /// 后续操作 / 落盘 SB 字节含本次 free 的计数变化（free_inodes 由集成层 `run_journaled_namespace`
    /// 的 `post.free_inodes_count()` harvest 进 `running_sb`，单一权威不退）。
    fn free_inode(&mut self, index: u32, is_dir: bool) {
        let mut ialloc = InodeAllocator::new(self.sb, self.reader, self.writer);
        ialloc.ialloc_free_inode(index, is_dir);
        self.sb = *ialloc.superblock();
    }

    /// 在 `parent`（已加载）下加一项指向 `child`（已加载），并按 ext4_rs `link` 调链计数。
    ///
    /// PARITY（ext4_rs `link`，ext4_impls/file.rs:470）：
    /// 1. `dir_add_entry(parent, child, name)`（用 child 的 DE filetype）；
    /// 2. **child 是目录**：`dir_add_entry(child, child, ".")`（child 空 → dir_append_block 分配
    ///    首数据块）+ `dir_add_entry(child, parent, "..")`（同块 try_insert）+ child links=2 +
    ///    **parent links += 1**；'.' / '..' 的 DE filetype = DIR(2)；
    /// 3. **child 是文件**：child links += 1（从 0 → 1）。
    ///
    /// 不在此 write_back（由 create / create_unchecked 末尾各 write_back 父 + 子一次）。
    /// 块分配经 [`NamespaceBlockAlloc`]，分配后把运行期 SB 同步回 `self.sb`。
    fn link(&mut self, parent: &mut Inode, child: &mut Inode, name: &[u8]) -> Result<()> {
        let child_ftype = inode_to_dir_entry_type(child);
        let child_is_dir = child.is_dir();
        let child_num = child.num;

        // ① 父目录加项（可能触发父目录新建块——dir_add_entry 内部分配）。
        self.with_block_alloc(parent, |ctx, alloc, parent| {
            dir_add_entry(ctx, alloc, parent, child_num, child_ftype, name)
        })?;

        if child_is_dir {
            // ② child 空 → 写 '.'（dir_append_block 分配 child 首块）+ '..'（同块 try_insert）。
            //    '.' / '..' DE filetype = DIR(2)（child 与 parent 都是目录）。
            let child_ino = child.num;
            let parent_ino = parent.num;
            self.with_block_alloc(child, |ctx, alloc, child| {
                dir_add_entry(ctx, alloc, child, child_ino, DE_DIR, b".")?;
                dir_add_entry(ctx, alloc, child, parent_ino, DE_DIR, b"..")
            })?;
            // ③ child links = 2，parent links += 1（PARITY (H)）。
            child.set_links_count(2);
            let pl = parent.links_count() + 1;
            parent.set_links_count(pl);
        } else {
            // PARITY: 文件分支 child links += 1（0 → 1）。
            let cl = child.links_count() + 1;
            child.set_links_count(cl);
        }
        Ok(())
    }

    /// 同 [`link`] 但父目录加项用 `dir_add_entry_unchecked`（只动末块），返回新项 abs byte offset。
    /// PARITY（ext4_rs `link_unchecked`，ext4_impls/file.rs:562）：'.' / '..' 仍用 dir_add_entry
    /// （恒落 child 首块、无扫描）。
    fn link_unchecked(&mut self, parent: &mut Inode, child: &mut Inode, name: &[u8]) -> Result<u64> {
        let child_ftype = inode_to_dir_entry_type(child);
        let child_is_dir = child.is_dir();

        let child_num = child.num;
        let dir_byte_offset = self.with_block_alloc(parent, |ctx, alloc, parent| {
            dir_add_entry_unchecked(ctx, alloc, parent, child_num, child_ftype, name)
        })?;

        if child_is_dir {
            let child_ino = child.num;
            let parent_ino = parent.num;
            self.with_block_alloc(child, |ctx, alloc, child| {
                dir_add_entry(ctx, alloc, child, child_ino, DE_DIR, b".")?;
                dir_add_entry(ctx, alloc, child, parent_ino, DE_DIR, b"..")
            })?;
            child.set_links_count(2);
            let pl = parent.links_count() + 1;
            parent.set_links_count(pl);
        } else {
            let cl = child.links_count() + 1;
            child.set_links_count(cl);
        }
        Ok(dir_byte_offset)
    }

    /// 在 `inode` 上跑一段需要块分配的写操作 `f`：据权威 SB 造 BlockAllocator + adapter +
    /// WriteCtx，跑 `f`，跑完把块分配器运行期 SB 同步回 `self.sb`（保 free_blocks 一致）。
    fn with_block_alloc<T>(
        &mut self,
        inode: &mut Inode,
        f: impl FnOnce(&WriteCtx, &mut NamespaceBlockAlloc<'_, R, W>, &mut Inode) -> Result<T>,
    ) -> Result<T> {
        let sb = self.sb;
        let alloc = BlockAllocator::new(sb, self.reader, self.writer);
        let ictx = InodeAllocCtx::new(inode.blocks_count());
        let mut adapter = NamespaceBlockAlloc { alloc, ictx };
        let ctx = WriteCtx::new(self.reader, self.writer, self.data_writer, &sb);
        let r = f(&ctx, &mut adapter, inode);
        // 同步运行期 SB（free_blocks 变化）回权威 SB。
        self.sb = *adapter.alloc.superblock();
        r
    }

    /// 把一个 inode 写回盘（据权威 SB；csum 用权威 SB 的 uuid/inode_size）。
    fn write_back(&self, inode: &mut Inode) -> Result<()> {
        write_back_inode(self.writer, self.reader, &self.sb, inode)
    }

    /// 截断一个 inode 到 `new_size`（释放数据块；释放后把运行期 SB 同步回）。
    fn truncate(&mut self, inode: &mut Inode, new_size: u64) -> Result<()> {
        self.with_block_alloc(inode, |ctx, alloc, inode| {
            file::truncate_inode(ctx, alloc, inode, new_size)
        })
    }

    /// 加载一个 inode（据权威 SB）。
    fn load(&self, inode_num: u32) -> Result<Inode> {
        load_inode(self.reader, &self.sb, inode_num)
    }
}

/// 在 `parent`（已加载目录）下查 `name`，命中返回 inode 号，未命中 **ENOENT**。
///
/// PARITY（ext4_rs `ext4_lookup_at`，simple_interface/mod.rs:201）：`dir_find_entry` 命中→
/// `search_result.dentry.inode`；未命中（ext4_rs `dir_find_entry` 走完 ENOENT）→ core 把
/// `Ok(None)` 映射成 `Err(ENOENT)`。
pub(in crate::fs::ext4) fn lookup_at(ctx: &ReadCtx, parent: &Inode, name: &[u8]) -> Result<u32> {
    match dir_find_entry(ctx, parent, name)? {
        Some(hit) => Ok(hit.inode),
        None => Err(Error::with_message(Errno::ENOENT, "dir search fail")),
    }
}

/// 在 `parent_ino` 下创建一个名为 `name`、mode 为 `mode` 的文件 / 节点，返回新 inode 号。
///
/// PARITY（ext4_rs `create`，ext4_impls/file.rs:520）：`create_inode`（alloc inode + init mode/
/// flags/extent header，links=0）→ `link`（文件分支 child=1；若 mode 是目录则走目录分支）→
/// write_back 父 + 子。**create 写回等价化**：ext4_rs 先 write_back_inode_without_csum(child)
/// 再 reload 再 link，core 直接在内存里构造 child 完跑 link、末尾各 write_back 一次——最终盘
/// 字节等价（见模块顶 PARITY 注）。
pub(in crate::fs::ext4) fn create_at<R: BlockReader, W: MetadataWriter, D: BlockWriter>(
    nctx: &mut NamespaceCtx<'_, R, W, D>,
    parent_ino: u32,
    name: &[u8],
    mode: u16,
) -> Result<u32> {
    let mut parent = nctx.load(parent_ino)?;
    // create_inode：alloc inode（is_dir 据 mode 类型位）+ init。
    let is_dir = (mode & S_IFMT) == S_IFDIR_FULL;
    let ino = nctx.alloc_inode(is_dir)?;
    let mut child = init_new_inode(ino, mode, &nctx.sb);

    nctx.link(&mut parent, &mut child, name)?;

    nctx.write_back(&mut parent)?;
    nctx.write_back(&mut child)?;
    Ok(ino)
}

/// 同 [`create_at`] 但用 `link_unchecked`（只动父末块、无扫描），返回 `(新 inode 号, 新项 abs byte offset)`。
///
/// PARITY（ext4_rs `create_unchecked`，ext4_impls/file.rs:543）。
pub(in crate::fs::ext4) fn create_unchecked_at<R: BlockReader, W: MetadataWriter, D: BlockWriter>(
    nctx: &mut NamespaceCtx<'_, R, W, D>,
    parent_ino: u32,
    name: &[u8],
    mode: u16,
) -> Result<(u32, u64)> {
    let mut parent = nctx.load(parent_ino)?;
    let is_dir = (mode & S_IFMT) == S_IFDIR_FULL;
    let ino = nctx.alloc_inode(is_dir)?;
    let mut child = init_new_inode(ino, mode, &nctx.sb);

    let dir_byte_offset = nctx.link_unchecked(&mut parent, &mut child, name)?;

    nctx.write_back(&mut parent)?;
    nctx.write_back(&mut child)?;
    Ok((ino, dir_byte_offset))
}

/// 在 `parent_ino` 下创建子目录 `name`，返回新目录 inode 号。
///
/// PARITY（ext4_rs `ext4_mkdir_at`，simple_interface/mod.rs:243）：先 `dir_find_entry(parent,
/// name)` 查重——命中 → **EEXIST**；否则 `create(parent, name, mode)`——`mode` **原样**传给
/// `create`（ext4_rs **不**在 mkdir 里补 S_IFDIR，靠调用方提供类型位：`ext4_dir_mk` / 集成层
/// 传 `S_IFDIR|perm`）。dir-vs-file 分支由新建 inode 的 `is_dir()`（mode 类型位）驱动，与
/// ext4_rs 一致——正常 mkdir（mode 含 S_IFDIR）走目录分支（写 '.'/'..' + child=2 + 父 nlink++）；
/// 仅畸形调用（mode 无类型位）才回落到常规文件，与 ext4_rs 一致（不再 core 强制建目录）。
pub(in crate::fs::ext4) fn mkdir_at<R: BlockReader, W: MetadataWriter, D: BlockWriter>(
    nctx: &mut NamespaceCtx<'_, R, W, D>,
    parent_ino: u32,
    name: &[u8],
    mode: u16,
) -> Result<u32> {
    // PARITY: 先查重 → EEXIST。
    let parent = nctx.load(parent_ino)?;
    let rctx = nctx.read_ctx();
    if dir_find_entry(&rctx, &parent, name)?.is_some() {
        return Err(Error::with_message(Errno::EEXIST, "directory already exists"));
    }
    drop(rctx);
    drop(parent);
    // PARITY: ext4_rs ext4_mkdir_at passes mode through unchanged; caller supplies S_IFDIR.
    create_at(nctx, parent_ino, name, mode)
}

/// 同 [`mkdir_at`] 但用 `create_unchecked`（无查重、只动父末块），返回 `(新目录 inode 号, abs byte offset)`。
///
/// PARITY（ext4_rs `ext4_mkdir_unchecked_at`，simple_interface/mod.rs:258）：调用方保证 name
/// 不存在（无查重）；`mode` 原样传给 `create_unchecked`（调用方提供 S_IFDIR 类型位）。
pub(in crate::fs::ext4) fn mkdir_unchecked_at<R: BlockReader, W: MetadataWriter, D: BlockWriter>(
    nctx: &mut NamespaceCtx<'_, R, W, D>,
    parent_ino: u32,
    name: &[u8],
    mode: u16,
) -> Result<(u32, u64)> {
    // PARITY: ext4_rs ext4_mkdir_unchecked_at passes mode through unchanged; caller supplies S_IFDIR.
    create_unchecked_at(nctx, parent_ino, name, mode)
}

/// 删除 `parent_ino` 下名为 `name` 的**文件**（目录请用 [`rmdir_at`]）。
///
/// [对照] ext4_rs `ext4_unlink_at` → `unlink`（simple_interface/mod.rs:299 / ext4_impls/ext4.rs:220）
/// + Linux `ext4_unlink`：
/// 1. lookup → 目标 inode；若目标 `is_dir()` → **EISDIR**；
/// 2. `dir_remove_entry(parent, name)` → 文件分支：child links > 1 减一、否则置 0 →
///    `write_back_inode(child)`。
///
/// **BUG-19 已修（ext4-spec-correct）**：unlink 本身**不**截块、**不**清 inode 位图位——这是
/// 正确的 POSIX 语义（文件可被打开持有，最后引用关闭前 inode 须保留，避免 open-unlink 竞态下
/// inode 复用损坏）。当 nlink 落到 0 时，实际回收（`truncate_inode(0)` 释放数据块 + `ialloc`
/// 清位图位 + free_inodes_count 自增）由集成层 evict 钩子 [`free_inode_on_evict_at`] 在最后一个
/// 打开句柄关闭、且确认 nlink==0 时做（单事务，走 `run_journaled_namespace`）。
pub(in crate::fs::ext4) fn unlink_at<R: BlockReader, W: MetadataWriter, D: BlockWriter>(
    nctx: &mut NamespaceCtx<'_, R, W, D>,
    parent_ino: u32,
    name: &[u8],
) -> Result<()> {
    // ① lookup（ext4_unlink_at 先 ext4_lookup_at）。
    let parent_for_lookup = nctx.load(parent_ino)?;
    let rctx = nctx.read_ctx();
    let child_ino = lookup_at(&rctx, &parent_for_lookup, name)?;
    drop(rctx);
    drop(parent_for_lookup);

    // ② 目标是目录 → EISDIR（ext4_unlink_at 在 unlink 前判）。
    let mut child = nctx.load(child_ino)?;
    if child.is_dir() {
        return Err(Error::with_message(Errno::EISDIR, "target is a directory"));
    }

    // ③ unlink 文件分支：dir_remove_entry + nlink 调整 + write_back(child)。
    let mut parent = nctx.load(parent_ino)?;
    {
        let ctx = nctx.write_ctx();
        dir_remove_entry(&ctx, &mut parent, name)?;
    }
    // [对照] 文件 nlink > 1 减一、否则置 0。
    let cl = child.links_count();
    if cl > 1 {
        child.set_links_count(cl - 1);
    } else {
        child.set_links_count(0);
    }
    // BUG-19 已修：unlink 只删项 + 调 nlink，**不**在此截块/free——POSIX 延后回收，nlink==0 的
    // 实际回收在 evict 钩子（[`free_inode_on_evict_at`]）做。仅 write_back(child)。
    nctx.write_back(&mut child)?;
    Ok(())
}

/// 删除 `parent_ino` 下名为 `name` 的**空目录**。
///
/// [对照] ext4_rs `ext4_rmdir_at` = `dir_remove`（ext4_impls/dir.rs:749）+ Linux `ext4_rmdir`：
/// 1. `name == "." | ".."` → **EINVAL**；
/// 2. `dir_find_entry`（未命中 → **ENOENT**）；
/// 3. 载 child；非目录 → **ENOTDIR**；
/// 4. `dir_has_entry(child)` 非空 → **ENOTEMPTY**；
/// 5. `truncate_inode(child, 0)`（释放子数据块）；
/// 6. `unlink(parent, child, name)` 目录分支：父 nlink-1 + 子 nlink=0 + write_back 两者；
/// 7. **BUG-19 已修**：子目录 nlink==0、且目录无打开句柄语义（rmdir 即最后引用）→ 立即清
///    inode 位图位（[`free_inode_on_evict_at`]，is_dir=true：used_dirs_count 自减、free_inodes++）；
/// 8. `dir_remove` 末尾再 `write_back_inode(parent)` 一次（小重复写，最终字节同）。
pub(in crate::fs::ext4) fn rmdir_at<R: BlockReader, W: MetadataWriter, D: BlockWriter>(
    nctx: &mut NamespaceCtx<'_, R, W, D>,
    parent_ino: u32,
    name: &[u8],
) -> Result<()> {
    // ① 拒 '.' / '..'。
    if name == b"." || name == b".." {
        return Err(Error::with_message(Errno::EINVAL, "invalid directory name"));
    }

    // ② find（未命中 ENOENT）。
    let parent_probe = nctx.load(parent_ino)?;
    let rctx = nctx.read_ctx();
    let child_ino = match dir_find_entry(&rctx, &parent_probe, name)? {
        Some(hit) => hit.inode,
        None => return Err(Error::with_message(Errno::ENOENT, "dir search fail")),
    };
    drop(rctx);
    drop(parent_probe);

    // ③ 载 child；非目录 → ENOTDIR。
    let mut child = nctx.load(child_ino)?;
    if !child.is_dir() {
        return Err(Error::with_message(Errno::ENOTDIR, "target is not a directory"));
    }

    // ④ dir_has_entry 非空 → ENOTEMPTY。
    {
        let rctx = nctx.read_ctx();
        if dir_has_entry(&rctx, &child)? {
            return Err(Error::with_message(Errno::ENOTEMPTY, "directory not empty"));
        }
    }

    // ⑤ truncate(child, 0)（释放子数据块）。
    nctx.truncate(&mut child, 0)?;

    // ⑥ unlink 目录分支：dir_remove_entry + 父 nlink-1 + 子 nlink=0 + write_back 两者。
    let mut parent = nctx.load(parent_ino)?;
    unlink_dir_branch(nctx, &mut parent, &mut child, name)?;

    // ⑦ BUG-19 已修：子目录 nlink==0，目录无打开句柄语义（rmdir 即最后引用）→ 立即回收
    //    inode（清位图位 + free_inodes++ + used_dirs_count 自减）。set i_dtime（删除时间戳）。
    finalize_freed_inode(nctx, &mut child)?;

    // ⑧ dir_remove 末尾再 write_back(parent)（小重复写，最终字节同）。
    nctx.write_back(&mut parent)?;
    Ok(())
}

/// 把一个 **nlink 已降到 0** 的 inode 落盘回收：set `i_dtime`（删除时间戳，ext4 标准）→ 清
/// inode 位图位（[`NamespaceCtx::free_inode`]，按 `is_dir` 回退 used_dirs_count）→ write_back
/// （把 nlink=0/dtime 等持久化到 inode 表）。**调用方须保证 `inode.links_count() == 0`** 且数据块
/// 已经 truncate 到 0（否则块泄漏）。`dtime` 时间戳由调用方在 truncate 时已 set 或这里写 0
/// （ext4 仅要求 dtime != 0 标记已删；具体秒值非 csum 相关）。
///
/// [对照] Linux `ext4_free_inode` 的尾段：位图清位 + 计数自增（自减 used_dirs）。core 在
/// inode 写回**前** free 位图（先位图后 inode 表，与 ext4_rs ialloc 写序一致；同事务，
/// 崩溃要么全提交要么全丢弃）。
fn finalize_freed_inode<R: BlockReader, W: MetadataWriter, D: BlockWriter>(
    nctx: &mut NamespaceCtx<'_, R, W, D>,
    inode: &mut Inode,
) -> Result<()> {
    debug_assert_eq!(
        inode.links_count(),
        0,
        "finalize_freed_inode requires nlink==0"
    );
    let is_dir = inode.is_dir();
    // i_dtime：ext4 用非 0 dtime 标记 inode 已删除（e2fsck 据此判孤儿/已删）。这里写一个非 0
    // 哨兵（具体秒值非 csum 相关，集成层若有真实时间会在 truncate/前置步骤 set）。
    if inode.raw.dtime == 0 {
        inode.raw.dtime = 1;
    }
    // 清 inode 位图位 + 组/SB free_inodes++ + 目录 used_dirs--（运行期 SB 同步回权威 SB，
    // 由 run_journaled_namespace 的 post.free_inodes_count() 灌进 running_sb，单一权威）。
    nctx.free_inode(inode.num, is_dir);
    // 把 nlink=0 / dtime 持久化到 inode 表。
    nctx.write_back(inode)?;
    Ok(())
}

/// **evict 钩子核心**：在最后一个打开引用关闭、确认 inode `nlink==0` 后，把该 inode 完整回收
/// （释放数据块 + 清 inode 位图位 + free_inodes++）。集成层 `cleanup_unlinked_file` 在持
/// inode correctness 锁、且 `has_open_file_handles==false` 时经 `run_journaled_namespace`（单事务）
/// 调本函数。
///
/// 这是 BUG-19 的 ext4-spec-correct 修复（普通文件路径）——镜像 ext2 `sync_metadata` 在
/// `hard_links()==0` 时 `resize(0)` + `free_inode` 的 evict 模型（ext2/inode.rs:1361）。
///
/// 步骤：
/// 1. 载 inode；若 `links_count() != 0` → 直接 Ok（被 link 救回/竞态，不回收）；
/// 2. `truncate_inode(inode, 0)` 释放所有数据块；
/// 3. `finalize_freed_inode`：set i_dtime + 清 inode 位图位（is_dir 决定 used_dirs_count）+ write_back。
///
/// 幂等性：集成层用 inode correctness 锁串行化；若本函数已跑过、inode 位图位已清，再次进入会
/// 在步骤 1 仍见 nlink==0 而重复 free。集成层负责只调用一次（`cleanup_unlinked_file` 经
/// `clear_inode_touch_cache` + last-ref/close 单触发；ext2 用 `is_freed` 标志，本路径用集成层
/// 的 last-ref 语义 + 锁保证单次）。
pub(in crate::fs::ext4) fn free_inode_on_evict_at<R: BlockReader, W: MetadataWriter, D: BlockWriter>(
    nctx: &mut NamespaceCtx<'_, R, W, D>,
    ino: u32,
) -> Result<()> {
    let mut inode = nctx.load(ino)?;
    // nlink 非 0：被重新 link 救回（或竞态），不回收。
    if inode.links_count() != 0 {
        return Ok(());
    }
    // 释放所有数据块（i_blocks → 0）。
    nctx.truncate(&mut inode, 0)?;
    // set i_dtime + 清 inode 位图位 + free_inodes++ + write_back。
    finalize_freed_inode(nctx, &mut inode)?;
    Ok(())
}

/// 复刻 ext4_rs `unlink` 的**目录分支**（ext4_impls/ext4.rs:228-244）：`dir_remove_entry(parent,
/// name)` → 父 nlink > 0 减一 → 子 nlink = 0 → `write_back(child)` + `write_back(parent)`。
/// （文件分支在 [`unlink_at`] 内联，目录分支单独抽出供 rmdir 复用。）
fn unlink_dir_branch<R: BlockReader, W: MetadataWriter, D: BlockWriter>(
    nctx: &mut NamespaceCtx<'_, R, W, D>,
    parent: &mut Inode,
    child: &mut Inode,
    name: &[u8],
) -> Result<()> {
    {
        let ctx = nctx.write_ctx();
        dir_remove_entry(&ctx, parent, name)?;
    }
    // PARITY: 父 nlink > 0 减一（rmdir 移除子目录的 '..' 反向链接）。
    let pl = parent.links_count();
    if pl > 0 {
        parent.set_links_count(pl - 1);
    }
    // [对照] 子目录 nlink = 0（'.' + 父向链都没了）。inode 位图位的回收在 rmdir_at 末尾的
    // finalize_freed_inode 做（BUG-19 已修）。
    child.set_links_count(0);
    nctx.write_back(child)?;
    nctx.write_back(parent)?;
    Ok(())
}

/// [对照] ext4_rs `unlink` 的**文件分支**（ext4_impls/ext4.rs:245-258）：`dir_remove_entry(parent,
/// name)` → child nlink > 1 减一、否则置 0 → `write_back(child)`。**不** write_back(parent)
/// （ext4_rs 的文件分支只在 child links>0 时写 child，且不碰 parent）——这是 rename 文件覆盖
/// 路径不写回父的来源。
///
/// **BUG-19 已修**：本分支只删项 + 调 nlink，不在此截块/free；被覆盖文件 nlink 落到 0 时的
/// 实际回收由集成层 evict 钩子（[`free_inode_on_evict_at`]，在最后引用关闭时）做——与
/// `unlink_at` 文件路径同一语义。
fn unlink_file_branch<R: BlockReader, W: MetadataWriter, D: BlockWriter>(
    nctx: &mut NamespaceCtx<'_, R, W, D>,
    parent: &mut Inode,
    child: &mut Inode,
    name: &[u8],
) -> Result<()> {
    {
        let ctx = nctx.write_ctx();
        dir_remove_entry(&ctx, parent, name)?;
    }
    // [对照] 文件 nlink > 1 减一、否则置 0。
    let cl = child.links_count();
    if cl > 1 {
        child.set_links_count(cl - 1);
    } else {
        child.set_links_count(0);
    }
    // BUG-19 已修：只删项 + 调 nlink，**不**在此截块/free——nlink==0 的实际回收由集成层 evict
    // 钩子（[`free_inode_on_evict_at`]）在最后引用关闭时做。仅 write_back(child)，不碰 parent。
    nctx.write_back(child)?;
    Ok(())
}

/// 在 `old_parent` 下把 `old_name` 改名为 `new_name`（**仅同目录**——逐字复刻 ext4_rs
/// `ext4_rename_at`，simple_interface/mod.rs:322）。
///
/// PARITY（BUG-20，已登记 bug.md D 段）：
/// - **仅同目录**：`old_parent != new_parent` → **EXDEV**（无 '..' 重定父——ext4_rs 在跨目录
///   情况直接拒绝，不做父向链调整）；
/// - **'.' / '..' 拒绝**：old / new 任一为 "." / ".." → **EISDIR**；
/// - **同名短路**：`old_name == new_name` → Ok（无盘改动）；
/// - **同 inode 短路**：dest 已存在且 `new_ino == old_ino` → Ok；
/// - **目录覆盖**：dest 是空目录 → `truncate_inode(new, 0)` + `unlink`(目录分支：父 nlink-1 +
///   子 nlink=0 + write_back 子 + write_back 父) + **再显式 write_back(父)**；
/// - **文件覆盖**：dest 是文件 → `unlink`(文件分支：仅 write_back 子) → **不** write_back(父)；
///   （目录覆盖 write_back 父、文件覆盖不写回父——此不对称严格照搬 ext4_rs）；
/// - **末尾不显式 write_back 父**：`dir_remove_entry(old_name)` + `dir_add_entry(new_name,
///   old_ino, old_ftype)` 后不再 write_back 父（仅 `dir_add_entry` 分配新块时其内部写回 i_size）。
///
/// `old_ftype` = old inode 派生的目录项类型（`inode_to_dir_entry_type`）——与 ext4_rs 传
/// `&old_inode_ref` 给 `dir_add_entry`（由 inode 派生类型）一致。
pub(in crate::fs::ext4) fn rename_at<R: BlockReader, W: MetadataWriter, D: BlockWriter>(
    nctx: &mut NamespaceCtx<'_, R, W, D>,
    old_parent: u32,
    old_name: &[u8],
    new_parent: u32,
    new_name: &[u8],
) -> Result<()> {
    // ① old / new 任一为 "." / ".." → EISDIR。
    if old_name == b"." || old_name == b".." || new_name == b"." || new_name == b".." {
        return Err(Error::with_message(
            Errno::EISDIR,
            "rename on . or .. is not allowed",
        ));
    }

    // ② PARITY (BUG-20): 仅同目录 rename——跨目录 → EXDEV，无 '..' 重定父。
    if old_parent != new_parent {
        return Err(Error::with_message(
            Errno::EXDEV,
            "cross-directory rename is not supported",
        ));
    }

    // ③ 同名短路（无盘改动）。
    if old_name == new_name {
        return Ok(());
    }

    // ④ 解析 old：未命中 ENOENT 传播；记 old 是否目录。
    let old_ino = {
        let parent = nctx.load(old_parent)?;
        let rctx = nctx.read_ctx();
        lookup_at(&rctx, &parent, old_name)?
    };
    let old_inode = nctx.load(old_ino)?;
    let old_is_dir = old_inode.is_dir();
    // old_ftype 由 old inode 派生（与 ext4_rs 传 &old_inode_ref 给 dir_add_entry 一致）。
    let old_ftype = inode_to_dir_entry_type(&old_inode);

    // ⑤ dest 已存在？
    let dest = {
        let parent = nctx.load(new_parent)?;
        let rctx = nctx.read_ctx();
        // lookup_at 未命中返回 Err(ENOENT)——dest 不存在等价于 ext4_rs `if let Ok(new_ino) = ...`。
        lookup_at(&rctx, &parent, new_name).ok()
    };
    if let Some(new_ino) = dest {
        // 同 inode 短路（rename 一个名字到指向同 inode 的名字）。
        if new_ino == old_ino {
            return Ok(());
        }
        let mut new_inode = nctx.load(new_ino)?;
        if old_is_dir {
            // old 是目录：dest 必须是空目录，否则报错。
            if !new_inode.is_dir() {
                return Err(Error::with_message(
                    Errno::ENOTDIR,
                    "cannot overwrite non-directory",
                ));
            }
            {
                let rctx = nctx.read_ctx();
                if dir_has_entry(&rctx, &new_inode)? {
                    return Err(Error::with_message(
                        Errno::ENOTEMPTY,
                        "directory not empty",
                    ));
                }
            }
            // truncate dest → unlink(目录分支：父 nlink-1 + 子 nlink=0 + write_back 两者)。
            nctx.truncate(&mut new_inode, 0)?;
            let mut parent = nctx.load(new_parent)?;
            unlink_dir_branch(nctx, &mut parent, &mut new_inode, new_name)?;
            // PARITY: 目录覆盖路径在 unlink 后**再显式** write_back(父)（ext4_rs mod.rs:362）。
            nctx.write_back(&mut parent)?;
        } else {
            // old 是文件：dest 不能是目录。
            if new_inode.is_dir() {
                return Err(Error::with_message(
                    Errno::EISDIR,
                    "cannot overwrite directory",
                ));
            }
            // unlink(文件分支：仅 write_back 子)。
            // PARITY: 文件覆盖路径**不** write_back(父)（ext4_rs mod.rs:367-368 缺该调用）。
            let mut parent = nctx.load(new_parent)?;
            unlink_file_branch(nctx, &mut parent, &mut new_inode, new_name)?;
        }
    }

    // ⑥ 在 old_parent（== new_parent）下删 old_name + 加 new_name（指向 old_ino，类型 old_ftype）。
    // PARITY: 末尾**不**显式 write_back(父)——仅 dir_add_entry 分配新块时其内部写回父 i_size。
    let mut parent = nctx.load(old_parent)?;
    {
        let ctx = nctx.write_ctx();
        dir_remove_entry(&ctx, &mut parent, old_name)?;
    }
    nctx.with_block_alloc(&mut parent, |ctx, alloc, parent| {
        dir_add_entry(ctx, alloc, parent, old_ino, old_ftype, new_name)
    })?;
    Ok(())
}

/// 用预算好的 dir-stream 字节偏移删除空目录（绕过 `dir_find_entry` 扫描）。
///
/// [对照] ext4_rs `ext4_rmdir_at_fast`（simple_interface/mod.rs:266）+ Linux `ext4_rmdir`：
/// 1. 载 parent + child；child 非目录 → **ENOTDIR**；
/// 2. `truncate_inode(child, 0)`（释放子数据块）；
/// 3. `dir_remove_entry_at_offset(parent, dir_byte_offset)`（O(1) 删）；
/// 4. 父 nlink > 0 减一 + 子 nlink = 0 → write_back(child)；
/// 5. **BUG-19 已修**：子目录 nlink==0、目录无打开句柄语义（rmdir 即最后引用）→ 立即清 inode
///    位图位（[`finalize_freed_inode`]，is_dir=true：used_dirs_count 自减、free_inodes++、set
///    i_dtime）——与慢路径 [`rmdir_at`] 步⑦逐位等价。**此为生产热路径**（mkdir/readdir 后
///    dir-entry 偏移已缓存，集成层 `rmdir_at` 优先走本快路径），故 free 必须在此而非仅慢路径；
/// 6. write_back(parent)。
///
/// 注意 ext4_rmdir_at_fast **不**判 dir_has_entry（调用方保证空）、**不**拒 '.'/'..'（按偏移删）。
pub(in crate::fs::ext4) fn rmdir_at_fast<R: BlockReader, W: MetadataWriter, D: BlockWriter>(
    nctx: &mut NamespaceCtx<'_, R, W, D>,
    parent_ino: u32,
    child_ino: u32,
    dir_byte_offset: u64,
) -> Result<()> {
    let mut parent = nctx.load(parent_ino)?;
    let mut child = nctx.load(child_ino)?;

    // ① child 非目录 → ENOTDIR。
    if !child.is_dir() {
        return Err(Error::with_message(Errno::ENOTDIR, "target is not a directory"));
    }

    // ② 释放 child 数据块。
    nctx.truncate(&mut child, 0)?;

    // ③ 按偏移删项。
    {
        let ctx = nctx.write_ctx();
        dir_remove_entry_at_offset(&ctx, &mut parent, dir_byte_offset)?;
    }

    // ④ 父 nlink-1 + 子 nlink=0 → write_back(child)。
    let pl = parent.links_count();
    if pl > 0 {
        parent.set_links_count(pl - 1);
    }
    child.set_links_count(0);
    nctx.write_back(&mut child)?;

    // ⑤ BUG-19 已修：立即回收子目录 inode（清位图位 + free_inodes++ + used_dirs_count 自减 +
    //    set i_dtime）——与 rmdir_at 步⑦一致，生产热路径不漏。
    finalize_freed_inode(nctx, &mut child)?;

    // ⑥ write_back(parent)。
    nctx.write_back(&mut parent)?;
    Ok(())
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::{
        dir_block_csum, dir_find_in_block, dir_verify_block_csum, parse_entry, DirEntryFileType,
        RawDirEntryHeader, RawDirEntryTail, DIR_ENTRY_HEADER_SIZE, DIR_TAIL_MARKER, DIR_TAIL_SIZE,
    };
    use crate::fs::ext4::core::block_group::RawGroupDescriptor;
    use crate::fs::ext4::core::extents::{RawExtent, RawExtentHeader};
    use crate::fs::ext4::core::superblock::RawSuperblock;
    use crate::fs::ext4::core::test_util::{slice_at, EXT4_IMAGE, EXT4_NOCSUM_IMAGE};
    use crate::prelude::*;
    // Disambiguate `Result` (the prelude glob also re-exports an `ostd::Result`); the
    // `MetadataWriter` impl below must use the kernel `crate::prelude::Result`.
    use crate::prelude::Result;

    /// 在原镜像 `img` 字节上定位根目录（ino 2）首数据块的 `[off, off+bs)` 字节，并返回
    /// `(superblock, dir_block_bytes, ino_generation)`。复刻 `dirent_root_first_entry_real_image`
    /// 的定位法（根 inode → extent → 首块），供块解析 / csum ktest 共用。
    fn locate_root_dir_block(img: &[u8]) -> (RawSuperblock, Vec<u8>, u32) {
        let sb = RawSuperblock::from_bytes(&img[1024..2048]);
        let bs = sb.block_size();
        let gd_off = (sb.first_data_block as usize + 1) * bs;
        let gd = RawGroupDescriptor::from_bytes(&img[gd_off..gd_off + 64]);
        let inode_off = gd.inode_table() as usize * bs + (2 - 1) * sb.inode_size() as usize;
        // 根 inode generation（dir 块 csum 第三段种子）——经 RawInode Pod 解析，不裸读字节。
        let raw_inode =
            crate::fs::ext4::core::inode::RawInode::from_bytes(&img[inode_off..inode_off + 156]);
        let ino_gen = raw_inode.generation();
        let ext = RawExtent::from_bytes(&img[inode_off + 40 + 12..inode_off + 40 + 24]);
        let dir_block = ext.start() as usize * bs;
        let block = img[dir_block..dir_block + bs].to_vec();
        (sb, block, ino_gen)
    }

    #[ktest]
    fn dirent_header_handcrafted_roundtrip() {
        let mut b = [0u8; 8];
        b[0..4].copy_from_slice(&2u32.to_le_bytes());
        b[4..6].copy_from_slice(&12u16.to_le_bytes());
        b[6] = 1;
        b[7] = 2;
        let h = RawDirEntryHeader::from_bytes(&b);
        assert_eq!(h.as_bytes(), &b[..]);
        assert_eq!(h.inode(), 2);
        assert_eq!(h.name_len(), 1);
        assert_eq!(h.file_type(), DirEntryFileType::Dir);
    }

    #[ktest]
    fn dirent_filetype_enum() {
        assert_eq!(DirEntryFileType::from(2), DirEntryFileType::Dir);
        assert_eq!(DirEntryFileType::from(1), DirEntryFileType::RegFile);
        assert_eq!(DirEntryFileType::from(99), DirEntryFileType::Unknown);
    }

    #[ktest]
    fn dirent_tail_roundtrip() {
        let mut t = RawDirEntryTail::default();
        t.reserved_ft = DIR_TAIL_MARKER;
        t.rec_len = 12;
        let bytes = t.as_bytes().to_vec();
        let t2 = RawDirEntryTail::from_bytes(&bytes);
        assert_eq!(t2.as_bytes(), &bytes[..]);
        assert!(t2.is_tail());
    }

    #[ktest]
    fn dirent_root_first_entry_real_image() {
        // 根 inode(ino=2) → extent → 首数据块 = 根目录块；首项是 "." 指向 inode 2。
        let sb = RawSuperblock::from_bytes(slice_at(1024, 1024));
        let bs = sb.block_size();
        let gd = RawGroupDescriptor::from_bytes(slice_at((sb.first_data_block as usize + 1) * bs, 64));
        let inode_off = gd.inode_table() as usize * bs + (2 - 1) * sb.inode_size() as usize;
        let eh = RawExtentHeader::from_bytes(slice_at(inode_off + 40, 12));
        assert!(eh.is_valid());
        assert!(eh.entries_count() >= 1);
        let ext = RawExtent::from_bytes(slice_at(inode_off + 40 + 12, 12));
        let dir_block = ext.start() as usize * bs;
        let de = RawDirEntryHeader::from_bytes(slice_at(dir_block, 8));
        assert_eq!(de.as_bytes(), slice_at(dir_block, 8));
        assert_eq!(de.inode(), 2, "root dir first entry '.' -> inode 2");
        assert_eq!(de.name_len(), 1);
        assert_eq!(de.file_type(), DirEntryFileType::Dir);
    }

    /// 真镜像根目录块上：`parse_entry` 解出 '.'(ino 2, name_len 1)、'..'(ino 2, name_len 2)、
    /// 'lost+found'(ino 11) 的变长项；`dir_find_in_block` 命中 'lost+found' 返回其 inode。
    /// 覆盖：变长 name 切片解析、按 rec_len 走项、跳到下一项、按名字节匹配。
    #[ktest]
    fn dir_parse_and_find_in_block() {
        let (sb, block, _gen) = locate_root_dir_block(EXT4_IMAGE);
        let bs = sb.block_size();

        // 首项 "."：ino 2、name_len 1、类型目录、name == b"."。
        let dot = parse_entry(&block, 0, bs).expect("parse '.'");
        assert_eq!(dot.inode, 2, "'.' -> ino 2");
        assert_eq!(dot.name_len, 1);
        assert_eq!(dot.name, b".");
        assert_eq!(dot.file_type, DirEntryFileType::Dir as u8);
        assert!(dot.rec_len >= 12, "'.' rec_len at least 12");

        // 次项 ".."：紧随 '.' 的 rec_len 之后；ino 2、name_len 2、name == b".."。
        let off_dotdot = dot.rec_len as usize;
        let dotdot = parse_entry(&block, off_dotdot, bs).expect("parse '..'");
        assert_eq!(dotdot.inode, 2, "'..' -> ino 2 (root parent is itself)");
        assert_eq!(dotdot.name_len, 2);
        assert_eq!(dotdot.name, b"..");

        // 线性查找 'lost+found' → ino 11（真镜像预置）。命中 offset 在 '.'/'..' 之后。
        let hit = dir_find_in_block(&block, b"lost+found", bs)
            .expect("find lost+found ok")
            .expect("lost+found present");
        let (lf_ino, lf_off, lf_prev) = hit;
        assert_eq!(lf_ino, 11, "'lost+found' -> ino 11");
        assert!(lf_off > off_dotdot, "lost+found after '..'");
        assert!(lf_prev < lf_off, "prev_offset precedes hit offset");

        // 查找不存在的名字 → Ok(None)（走完块未命中，非错误）。
        let miss = dir_find_in_block(&block, b"definitely-absent", bs).expect("find miss ok");
        assert!(miss.is_none(), "absent name must miss (Ok(None))");
    }

    /// 目录块 csum 读 parity：
    /// - csum 开（`EXT4_IMAGE`）：重算的 `dir_block_csum`（ino_index=块首项 inode）== 盘上
    ///   tail.checksum，且 `dir_verify_block_csum` 通过。
    /// - csum 关（`EXT4_NOCSUM_IMAGE`）：门控生效，`dir_verify_block_csum` 直接返回 true
    ///   （不校验盘上字节）。
    #[ktest]
    fn dir_block_csum_parity() {
        // ---- csum 开：EXT4_IMAGE ----
        let (sb, block, ino_gen) = locate_root_dir_block(EXT4_IMAGE);
        let bs = sb.block_size();
        assert!(
            (sb.features_read_only() & 0x400) != 0,
            "EXT4_IMAGE must have metadata_csum on"
        );
        // 块首项 inode = 根 "." = 2（块 0 的 ino_index）。
        let first = parse_entry(&block, 0, bs).expect("parse first entry");
        assert_eq!(first.inode, 2, "block-0 first entry inode is the dir itself");
        let want = dir_block_csum(&sb, &block, first.inode, ino_gen);
        let tail = RawDirEntryTail::from_bytes(&block[bs - DIR_TAIL_SIZE..bs]);
        assert_eq!(
            want, tail.checksum,
            "recomputed dir-block csum == on-disk tail.checksum"
        );
        assert!(
            dir_verify_block_csum(&sb, &block, ino_gen),
            "dir_verify_block_csum must pass on a valid block"
        );

        // ---- csum 关：EXT4_NOCSUM_IMAGE（门控：不校验，恒 true）----
        let (sb_nc, block_nc, gen_nc) = locate_root_dir_block(EXT4_NOCSUM_IMAGE);
        assert!(
            (sb_nc.features_read_only() & 0x400) == 0,
            "EXT4_NOCSUM_IMAGE must have metadata_csum off"
        );
        assert!(
            dir_verify_block_csum(&sb_nc, &block_nc, gen_nc),
            "metadata_csum off: verify must short-circuit to true"
        );
    }

    // =================================================================
    // Phase 4 Task 5：损坏块防御（块级，单侧 core 鲁棒性）。
    //
    // 手工构造坏 `rec_len`（0 / 越界）的目录块缓冲，断言：
    // - 查找路径 `dir_find_in_block` / `parse_entry` → **EIO**（不 panic）；
    // - 名字边界（name_len=255 超长名、name_len=0 空名）正确解析（不 panic）。
    // 这些块**不**喂给 ext4_rs 的 unsafe 路径（避免 UB），故是单侧 core 鲁棒性验证（同 Phase 3
    // 教训：损坏输入只验 core 优雅降级）。枚举路径 `dir_get_entries` 的 silent-break-不-panic
    // 在前差分 harness 的 `dir_get_entries_corrupted_silent_break`（需 ReadCtx）里验。
    // =================================================================

    /// 损坏块防御（单侧 core）：坏 `rec_len`（0 / 越界）→ 查找路径 EIO 不 panic；超长名
    /// （name_len=255）/ 空名（name_len=0）边界正确解析。`#![forbid(unsafe_code)]` 杜绝 UB，
    /// 但越界索引仍会 panic——本测保证 core 用边界检查返回 EIO 而非索引越界。
    #[ktest]
    fn dir_corrupted_block_defense() {
        // 用真镜像取 block_size（4K）；损坏块就地手工构造，不回盘。
        let sb = RawSuperblock::from_bytes(&EXT4_IMAGE[1024..2048]);
        let bs = sb.block_size();

        // 写一个合法目录项头到 `block[off..]`（rec_len/name_len/name 由调用方给）。
        let put = |block: &mut [u8], off: usize, inode: u32, rec_len: u16, name: &[u8]| {
            let h = RawDirEntryHeader {
                inode,
                rec_len,
                name_len: name.len() as u8,
                file_type: DirEntryFileType::RegFile as u8,
            };
            block[off..off + DIR_ENTRY_HEADER_SIZE].copy_from_slice(h.as_bytes());
            block[off + DIR_ENTRY_HEADER_SIZE..off + DIR_ENTRY_HEADER_SIZE + name.len()]
                .copy_from_slice(name);
        };

        // ---- (1) rec_len == 0 → dir_find_in_block 返回 EIO（不 panic）。----
        let mut block = vec![0u8; bs];
        put(&mut block, 0, 12, 0, b"x"); // rec_len 故意为 0（坏）
        let r = dir_find_in_block(&block, b"x", bs);
        assert!(r.is_err(), "rec_len==0 must yield Err (EIO), not a hit/panic");
        assert_eq!(
            r.unwrap_err().error(),
            Errno::EIO,
            "rec_len==0 → EIO on search path"
        );

        // ---- (2) rec_len 越界（> block_size - off）→ dir_find_in_block 返回 EIO。----
        let mut block = vec![0u8; bs];
        put(&mut block, 0, 12, (bs + 4) as u16, b"y"); // rec_len > 整块（越界）
        let r = dir_find_in_block(&block, b"y", bs);
        assert!(r.is_err(), "out-of-range rec_len must yield Err (EIO)");
        assert_eq!(
            r.unwrap_err().error(),
            Errno::EIO,
            "rec_len > block_size-off → EIO on search path"
        );

        // ---- (3) name 区越过块尾（rec_len 合法但 name_len 把 name 推出块）→ parse_entry EIO。----
        // 在块的最后 8 字节起放一个头，name_len=255 → name_end 远超 block_size。
        let mut block = vec![0u8; bs];
        let near_end = bs - DIR_ENTRY_HEADER_SIZE; // 头尚在块内，但 name 必越界
        let h = RawDirEntryHeader {
            inode: 12,
            rec_len: 12,
            name_len: 255,
            file_type: DirEntryFileType::RegFile as u8,
        };
        block[near_end..near_end + DIR_ENTRY_HEADER_SIZE].copy_from_slice(h.as_bytes());
        let pr = parse_entry(&block, near_end, bs);
        assert!(
            pr.is_err(),
            "name running past block end must yield Err (EIO), not OOB index/panic"
        );
        assert_eq!(pr.unwrap_err().error(), Errno::EIO, "name out of block → EIO");

        // ---- (4) 头本身越界（off 使 8B 头放不下）→ parse_entry EIO（不索引越界 panic）。----
        let block = vec![0u8; bs];
        let pr = parse_entry(&block, bs - 4, bs); // 仅剩 4 字节，放不下 8B 头
        assert!(pr.is_err(), "header past block end must yield Err (EIO)");
        assert_eq!(
            pr.unwrap_err().error(),
            Errno::EIO,
            "header out of block → EIO"
        );

        // ---- (5) 超长名（name_len=255）在块内合法布局 → parse_entry 正确解出 255 字节 name。----
        let mut block = vec![0u8; bs];
        // 项总长 = align4(8 + 255) = 264；rec_len 给 264，name 全填 0x41('A')。
        let name = [0x41u8; 255];
        put(&mut block, 0, 12, 264, &name);
        let de = parse_entry(&block, 0, bs).expect("255-byte name must parse");
        assert_eq!(de.name_len, 255, "max name_len 255 parsed");
        assert_eq!(de.name.len(), 255, "name slice length == 255");
        assert!(de.name.iter().all(|&b| b == 0x41), "name bytes intact");

        // ---- (6) 空名（name_len=0）边界 → parse_entry 解出零长 name（不 panic）。----
        let mut block = vec![0u8; bs];
        put(&mut block, 0, 13, 12, b""); // name_len=0
        let de = parse_entry(&block, 0, bs).expect("empty-name entry must parse");
        assert_eq!(de.name_len, 0, "empty name_len 0");
        assert!(de.name.is_empty(), "empty name slice");
        assert_eq!(de.inode, 13, "header fields intact on empty-name entry");

        // ---- (7) 损坏块（首项 rec_len=0）查找一个不存在的名字也只返回 EIO（不 panic）。----
        let mut block = vec![0u8; bs];
        put(&mut block, 0, 14, 0, b"a");
        let r = dir_find_in_block(&block, b"absent", bs);
        assert_eq!(
            r.unwrap_err().error(),
            Errno::EIO,
            "corrupted block + miss still EIO, never panic"
        );
    }

    // =================================================================
    // BUG-19 fix: unlink/rmdir free the inode + data blocks (no leak).
    // Full end-to-end on a writable in-memory image: create → (write) →
    // unlink → free_inode_on_evict; mkdir → rmdir. Asserts the inode
    // bitmap bit clears, SB free_inodes/free_blocks return to baseline,
    // and used_dirs_count is restored. Judged against ext4-spec-correct
    // behavior (NOT differential parity).
    // =================================================================

    use core::cell::RefCell;

    use super::{
        create_at, free_inode_on_evict_at, mkdir_at, mkdir_unchecked_at, rmdir_at, rmdir_at_fast,
        unlink_at, write_back_inode, NamespaceBlockAlloc, NamespaceCtx,
    };
    use crate::fs::ext4::core::balloc::{BlockAllocator, InodeAllocCtx};
    use crate::fs::ext4::core::bitmap::ext4_bmap_bit_find_clr;
    use crate::fs::ext4::core::block_group::GroupGeometry;
    use crate::fs::ext4::core::extents::WriteCtx;
    use crate::fs::ext4::core::file::write_at;
    use crate::fs::ext4::core::inode::load_inode;
    use crate::fs::ext4::core::io::{BlockReader, BlockWriter};
    use crate::fs::ext4::core::metadata_writer::MetadataWriter;
    use crate::fs::ext4::core::types::Ext4Fsblk;

    /// S_IFREG / S_IFDIR full mode bits used when creating test inodes.
    const S_IFREG_TEST: u16 = 0x8000;
    const S_IFDIR_TEST: u16 = 0x4000;

    /// Writable in-memory image: reads, file-data writes, and journaled metadata writes all hit the
    /// same byte buffer (single-thread test, no journal — metadata writes land directly).
    struct MemDisk {
        bytes: RefCell<Vec<u8>>,
        block_size: usize,
    }
    impl MemDisk {
        fn new(seed: &[u8]) -> Self {
            let sb = RawSuperblock::from_bytes(&seed[1024..2048]);
            MemDisk {
                bytes: RefCell::new(seed.to_vec()),
                block_size: sb.block_size(),
            }
        }
        fn read_block(&self, blk: u64) -> Vec<u8> {
            let mut out = alloc::vec![0u8; self.block_size];
            self.read_at(blk as usize * self.block_size, out.as_mut_slice());
            out
        }
    }
    impl BlockReader for MemDisk {
        fn read_at(&self, off: usize, out: &mut [u8]) {
            let b = self.bytes.borrow();
            for (i, slot) in out.iter_mut().enumerate() {
                *slot = b.get(off + i).copied().unwrap_or(0);
            }
        }
    }
    impl BlockWriter for MemDisk {
        fn write_at(&self, off: usize, data: &[u8]) {
            let mut b = self.bytes.borrow_mut();
            for (i, byte) in data.iter().enumerate() {
                if let Some(slot) = b.get_mut(off + i) {
                    *slot = *byte;
                }
            }
        }
    }
    impl MetadataWriter for MemDisk {
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

    /// Read the current on-disk superblock (its free counts mutate as ops run).
    fn read_sb(disk: &MemDisk) -> RawSuperblock {
        let mut buf = [0u8; 1024];
        disk.read_at(1024, &mut buf);
        RawSuperblock::from_bytes(&buf)
    }

    /// Read the group descriptor of the group owning `ino` (re-read from disk so it reflects writes).
    fn group_desc_of_inode(disk: &MemDisk, sb: &RawSuperblock, ino: u32) -> RawGroupDescriptor {
        let bs = sb.block_size();
        let bgid = GroupGeometry::new(sb).get_bgid_of_inode(ino) as usize;
        let desc_size = sb.group_desc_size();
        let dsc_cnt = bs / desc_size;
        let dsc_id = bgid / dsc_cnt;
        let block_id = sb.first_data_block() as usize + dsc_id + 1;
        let off = block_id * bs + (bgid % dsc_cnt) * desc_size;
        let mut buf = [0u8; 64];
        disk.read_at(off, &mut buf);
        RawGroupDescriptor::from_bytes(&buf)
    }

    /// Is the inode-bitmap bit for `ino` (1-based) set on disk?
    fn inode_bit_set(disk: &MemDisk, sb: &RawSuperblock, ino: u32) -> bool {
        let desc = group_desc_of_inode(disk, sb, ino);
        let bmp = disk.read_block(desc.inode_bitmap());
        let idx = GroupGeometry::new(sb).inode_to_bgidx(ino) as usize;
        (bmp[idx / 8] >> (idx % 8)) & 1 == 1
    }

    fn used_dirs_of_inode(disk: &MemDisk, sb: &RawSuperblock, ino: u32) -> u32 {
        group_desc_of_inode(disk, sb, ino).get_used_dirs_count(sb)
    }

    /// Write `len` bytes of data into the regular file `ino` through the core write path (allocates
    /// real data blocks), persisting the grown inode. Mirrors how `NamespaceCtx` drives file writes.
    fn write_file_data(disk: &MemDisk, sb: &mut RawSuperblock, ino: u32, len: usize) {
        let snap = *sb;
        let mut inode = load_inode(disk, &snap, ino).unwrap();
        let alloc = BlockAllocator::new(snap, disk, disk);
        let ictx = InodeAllocCtx::new(inode.blocks_count());
        let mut adapter = NamespaceBlockAlloc { alloc, ictx };
        let buf = alloc::vec![0xABu8; len];
        {
            let ctx = WriteCtx::new(disk, disk, disk, &snap);
            write_at(&ctx, &mut adapter, &mut inode, 0, &buf).unwrap();
        }
        // Sink the post-write SB (free_blocks decreased) and persist the inode (size + i_block tree).
        *sb = *adapter.alloc.superblock();
        write_back_inode(disk, disk, sb, &mut inode).unwrap();
    }

    /// BUG-19 (file): create a regular file, give it data blocks, unlink it (nlink→0), then run the
    /// evict hook. Asserts the inode bitmap bit clears, `free_inodes_count` returns to baseline, and
    /// every data block is released (`free_blocks_count` back to pre-create) — i.e. NO leak.
    #[ktest]
    fn unlink_file_frees_inode_and_blocks() {
        let disk = MemDisk::new(EXT4_IMAGE);
        let base_sb = read_sb(&disk);
        let base_free_inodes = base_sb.free_inodes_count();
        let base_free_blocks = base_sb.free_blocks_count();

        // create under root (ino 2).
        let mut sb = base_sb;
        let ino = {
            let mut nctx = NamespaceCtx::new(&disk, &disk, &disk, sb);
            let ino = create_at(&mut nctx, 2, b"victim", S_IFREG_TEST).unwrap();
            sb = *nctx.superblock();
            ino
        };
        assert!(inode_bit_set(&disk, &sb, ino), "created inode bit must be set");
        assert!(
            sb.free_inodes_count() < base_free_inodes,
            "create must consume a free inode"
        );

        // Give the file real data blocks (two 4K blocks worth) so the block-free is meaningful.
        let data_len = 2 * sb.block_size() + 17;
        write_file_data(&disk, &mut sb, ino, data_len);
        assert!(
            sb.free_blocks_count() < base_free_blocks,
            "writing data must consume free blocks"
        );

        // unlink: removes the entry + drops nlink to 0, but does NOT free yet (POSIX deferral).
        {
            let mut nctx = NamespaceCtx::new(&disk, &disk, &disk, sb);
            unlink_at(&mut nctx, 2, b"victim").unwrap();
            sb = *nctx.superblock();
        }
        let after_unlink = load_inode(&disk, &sb, ino).unwrap();
        assert_eq!(after_unlink.links_count(), 0, "unlink drops nlink to 0");
        assert!(
            inode_bit_set(&disk, &sb, ino),
            "inode bit must still be set right after unlink (freed only at evict)"
        );

        // evict hook (last reference closed): truncate data blocks + free the inode bitmap bit.
        {
            let mut nctx = NamespaceCtx::new(&disk, &disk, &disk, sb);
            free_inode_on_evict_at(&mut nctx, ino).unwrap();
            sb = *nctx.superblock();
        }

        // Inode bitmap bit cleared.
        assert!(
            !inode_bit_set(&disk, &sb, ino),
            "BUG-19 fix: inode bitmap bit must be cleared after evict"
        );
        // i_dtime stamped (ext4 marks deleted inodes with a non-zero dtime).
        let freed = load_inode(&disk, &sb, ino).unwrap();
        assert_ne!(freed.raw.dtime, 0, "freed inode must have i_dtime set");
        assert_eq!(freed.blocks_count(), 0, "all data blocks released (i_blocks==0)");
        // Free counts returned exactly to baseline — no inode and no block leaked.
        assert_eq!(
            sb.free_inodes_count(),
            base_free_inodes,
            "free_inodes_count must return to pre-create baseline (no inode leak)"
        );
        assert_eq!(
            sb.free_blocks_count(),
            base_free_blocks,
            "free_blocks_count must return to pre-create baseline (no block leak)"
        );
    }

    /// BUG-19 (dir): mkdir then rmdir. mkdir consumes one inode + one data block (for `.`/`..`) and
    /// bumps `used_dirs_count`; rmdir must release ALL of that — inode bitmap bit cleared, free
    /// counts back to baseline, and `used_dirs_count` decremented back.
    #[ktest]
    fn rmdir_frees_inode_blocks_and_used_dirs() {
        let disk = MemDisk::new(EXT4_IMAGE);
        let base_sb = read_sb(&disk);
        let base_free_inodes = base_sb.free_inodes_count();
        let base_free_blocks = base_sb.free_blocks_count();
        // used_dirs baseline must be sampled BEFORE mkdir, from the SAME group the test dir will land
        // in. The new dir lands in group 0 alongside root (ino 2), so root's group descriptor is the
        // right pre-mkdir reference (sampling after mkdir would already include its +1 → N==N+1).
        let base_used_dirs = used_dirs_of_inode(&disk, &base_sb, 2);

        let mut sb = base_sb;
        let ino = {
            let mut nctx = NamespaceCtx::new(&disk, &disk, &disk, sb);
            let ino = mkdir_at(&mut nctx, 2, b"sub", S_IFDIR_TEST).unwrap();
            sb = *nctx.superblock();
            ino
        };
        assert!(inode_bit_set(&disk, &sb, ino), "new dir inode bit set");
        // ialloc bumps used_dirs_count for directory allocations.
        let used_dirs_after_mkdir = used_dirs_of_inode(&disk, &sb, ino);
        assert_eq!(
            used_dirs_after_mkdir,
            base_used_dirs + 1,
            "mkdir must bump used_dirs_count (and the test dir must land in group 0 with root)"
        );
        assert!(
            sb.free_blocks_count() < base_free_blocks,
            "mkdir allocates a data block for './..'"
        );

        // rmdir: truncates the dir data block, drops nlinks, and (BUG-19 fix) frees the inode now.
        {
            let mut nctx = NamespaceCtx::new(&disk, &disk, &disk, sb);
            rmdir_at(&mut nctx, 2, b"sub").unwrap();
            sb = *nctx.superblock();
        }

        assert!(
            !inode_bit_set(&disk, &sb, ino),
            "BUG-19 fix: rmdir must clear the directory's inode bitmap bit"
        );
        let freed = load_inode(&disk, &sb, ino).unwrap();
        assert_eq!(freed.links_count(), 0, "rmdir drops dir nlink to 0");
        assert_ne!(freed.raw.dtime, 0, "freed dir inode must have i_dtime set");
        assert_eq!(
            used_dirs_of_inode(&disk, &sb, ino),
            used_dirs_after_mkdir - 1,
            "rmdir must decrement used_dirs_count by 1 (back to the pre-mkdir baseline)"
        );
        assert_eq!(
            used_dirs_of_inode(&disk, &sb, ino),
            base_used_dirs,
            "used_dirs_count back to the pre-mkdir baseline after rmdir"
        );
        assert_eq!(
            sb.free_inodes_count(),
            base_free_inodes,
            "free_inodes_count back to baseline after rmdir (no inode leak)"
        );
        assert_eq!(
            sb.free_blocks_count(),
            base_free_blocks,
            "free_blocks_count back to baseline after rmdir (dir block released)"
        );
    }

    /// BUG-19 (dir, FAST path): the production hot path is `rmdir_at_fast` — after `mkdir`
    /// (`mkdir_unchecked_at` returns a cached dir-entry byte offset) or any readdir/lookup the
    /// integration `Ext4Fs::rmdir_at` routes to `rmdir_at_fast`. This asserts the fast path frees the
    /// directory inode exactly once (bitmap bit cleared, `used_dirs_count` + both free counts back to
    /// baseline) — the slow-path ktest above could pass while this hot path leaked, so cover it here.
    #[ktest]
    fn rmdir_at_fast_frees_inode_blocks_and_used_dirs() {
        let disk = MemDisk::new(EXT4_IMAGE);
        let base_sb = read_sb(&disk);
        let base_free_inodes = base_sb.free_inodes_count();
        let base_free_blocks = base_sb.free_blocks_count();
        // used_dirs baseline sampled BEFORE mkdir from root's group (group 0), where the new dir lands
        // — sampling after mkdir would already include its +1.
        let base_used_dirs = used_dirs_of_inode(&disk, &base_sb, 2);

        // mkdir_unchecked_at returns (ino, real dir-entry byte offset) — exactly what the integration
        // caches and feeds to rmdir_at_fast.
        let mut sb = base_sb;
        let (ino, dir_byte_offset) = {
            let mut nctx = NamespaceCtx::new(&disk, &disk, &disk, sb);
            let r = mkdir_unchecked_at(&mut nctx, 2, b"fastsub", S_IFDIR_TEST).unwrap();
            sb = *nctx.superblock();
            r
        };
        assert!(inode_bit_set(&disk, &sb, ino), "new dir inode bit set");
        let used_dirs_after_mkdir = used_dirs_of_inode(&disk, &sb, ino);
        assert_eq!(
            used_dirs_after_mkdir,
            base_used_dirs + 1,
            "mkdir must bump used_dirs_count (and the test dir must land in group 0 with root)"
        );
        assert!(
            sb.free_blocks_count() < base_free_blocks,
            "mkdir allocates a data block for './..'"
        );

        // FAST-path rmdir by cached offset.
        {
            let mut nctx = NamespaceCtx::new(&disk, &disk, &disk, sb);
            rmdir_at_fast(&mut nctx, 2, ino, dir_byte_offset).unwrap();
            sb = *nctx.superblock();
        }

        assert!(
            !inode_bit_set(&disk, &sb, ino),
            "BUG-19 fix: rmdir_at_fast must clear the directory's inode bitmap bit (hot-path leak)"
        );
        let freed = load_inode(&disk, &sb, ino).unwrap();
        assert_eq!(freed.links_count(), 0, "rmdir_at_fast drops dir nlink to 0");
        assert_ne!(freed.raw.dtime, 0, "freed dir inode must have i_dtime set");
        assert_eq!(
            used_dirs_of_inode(&disk, &sb, ino),
            used_dirs_after_mkdir - 1,
            "rmdir_at_fast must decrement used_dirs_count by 1 (back to the pre-mkdir baseline)"
        );
        assert_eq!(
            used_dirs_of_inode(&disk, &sb, ino),
            base_used_dirs,
            "used_dirs_count back to the pre-mkdir baseline after rmdir_at_fast"
        );
        assert_eq!(
            sb.free_inodes_count(),
            base_free_inodes,
            "free_inodes_count back to baseline after rmdir_at_fast (no inode leak)"
        );
        assert_eq!(
            sb.free_blocks_count(),
            base_free_blocks,
            "free_blocks_count back to baseline after rmdir_at_fast (dir block released)"
        );
    }

    /// Belt-and-braces: the evict hook is a no-op when the inode was rescued (nlink != 0), so a file
    /// that still has links is never freed (guards against a free-on-evict that ignores nlink).
    #[ktest]
    fn evict_is_noop_when_links_nonzero() {
        let disk = MemDisk::new(EXT4_IMAGE);
        let mut sb = read_sb(&disk);
        let ino = {
            let mut nctx = NamespaceCtx::new(&disk, &disk, &disk, sb);
            let ino = create_at(&mut nctx, 2, b"keep", S_IFREG_TEST).unwrap();
            sb = *nctx.superblock();
            ino
        };
        let free_inodes_before = sb.free_inodes_count();
        // nlink is 1 (linked); evict must do nothing.
        {
            let mut nctx = NamespaceCtx::new(&disk, &disk, &disk, sb);
            free_inode_on_evict_at(&mut nctx, ino).unwrap();
            sb = *nctx.superblock();
        }
        assert!(inode_bit_set(&disk, &sb, ino), "evict must not free a still-linked inode");
        assert_eq!(
            sb.free_inodes_count(),
            free_inodes_before,
            "evict on nlink!=0 must not touch free_inodes_count"
        );
        let _ = ext4_bmap_bit_find_clr; // silence unused import if optimized away.
    }
}
