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
// Task 2 wires the slot-split / new-entry write path against this constant.
#[allow(dead_code)]
pub(super) const EXT4_DIR_ENTRY_INMEM_SIZE: usize = 264;

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
pub(super) struct RawDirEntryHeader {
    pub inode: u32,
    pub rec_len: u16,
    pub name_len: u8,
    pub file_type: u8,
}
const_assert!(size_of::<RawDirEntryHeader>() == 8);

/// 目录块尾的校验和结构（12 字节）。占用一个普通目录项槽，靠 reserved_ft==0xDE 识别。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub(super) struct RawDirEntryTail {
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
use super::file::ReadCtx;
use super::inode::{write_back_inode, Inode};
use super::superblock::RawSuperblock;

/// `RawDirEntryHeader` 定长头字节数（= ext4_rs `Ext4FakeDirEntry` 的 `size_of`=8）。
const DIR_ENTRY_HEADER_SIZE: usize = 8;

/// 目录块尾校验和结构字节数（= `size_of::<RawDirEntryTail>()`=12）。变长项遍历停于
/// `block_size - DIR_TAIL_SIZE`（tail 区，复刻 ext4_rs `size_of::<Ext4DirEntryTail>()`）。
const DIR_TAIL_SIZE: usize = size_of::<RawDirEntryTail>();

/// RO-compat metadata_csum 特性位（门控目录块 csum）。
/// = ext4_rs `EXT4_FEATURE_RO_COMPAT_METADATA_CSUM`（0x400），与 `extents.rs` 同值。
const RO_COMPAT_METADATA_CSUM: u32 = 0x400;

/// 一条解析出的目录项（借用块字节，零拷贝）。`name` 是块内 `&[u8]` 切片（变长尾，
/// 非 Pod——安全切片即可，无需 from_le_bytes）。
#[derive(Clone, Copy, Debug)]
pub(super) struct DirEntryRef<'a> {
    pub inode: u32,
    pub rec_len: u16,
    pub name_len: u8,
    pub file_type: u8,
    pub name: &'a [u8],
}

/// readdir 返回项：拥有 name 的目录项（脱离块缓冲生命周期）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct OwnedDirEntry {
    pub inode: u32,
    pub file_type: u8,
    pub name: Vec<u8>,
}

/// 跨块查找命中：对齐 ext4_rs `Ext4DirSearchResult`（dentry.inode/pblock_id/offset/prev_offset）。
#[derive(Clone, Copy, Debug)]
pub(super) struct DirSearchHit {
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
pub(super) fn parse_entry<'a>(
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
pub(super) fn dir_find_in_block(
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
pub(super) fn dir_find_entry(
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
pub(super) fn dir_get_entries(ctx: &ReadCtx, dir: &Inode) -> Vec<OwnedDirEntry> {
    dir_enumerate(ctx, dir, false)
        .into_iter()
        .map(|(e, _off)| e)
        .collect()
}

/// 同 [`dir_get_entries`]，但每项附带 readdir 续读用的**下一项绝对字节偏移**
/// `next_offset = iblock*block_size + off + rec_len`。
///
/// PARITY（ext4_rs `dir_get_entries_with_next_offset`，dir.rs:205）。
pub(super) fn dir_get_entries_with_next_offset(
    ctx: &ReadCtx,
    dir: &Inode,
) -> Vec<(OwnedDirEntry, usize)> {
    dir_enumerate(ctx, dir, true)
}

/// 枚举的公共实现：`with_offset=false` 时 next_offset 占位 0（调用方丢弃）。
fn dir_enumerate(ctx: &ReadCtx, dir: &Inode, with_offset: bool) -> Vec<(OwnedDirEntry, usize)> {
    let block_size = ctx.block_size;
    let mut entries: Vec<(OwnedDirEntry, usize)> = Vec::new();
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
                    let next_offset = if with_offset {
                        iblock as usize * block_size + off + rec_len
                    } else {
                        0
                    };
                    entries.push((
                        OwnedDirEntry {
                            inode: de.inode,
                            file_type: de.file_type,
                            name: de.name.to_vec(),
                        },
                        next_offset,
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
pub(super) fn dir_block_csum(
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
pub(super) fn dir_verify_block_csum(sb: &RawSuperblock, block: &[u8], ino_gen: u32) -> bool {
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
pub(super) fn dir_write_entry_bytes(
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

/// 在目录块字节上写 tail.checksum（门控 metadata_csum）。
///
/// PARITY（ext4_rs `dir_set_csum`，dir.rs:252）：`ino_index = parse(block, 0).inode`
/// （**块首项 inode**——块 0 是 "." = 目录自身，块 ≥1 是普通子项 inode，偏离规范但照搬）；
/// 重算 [`dir_block_csum`]（复用 Task 1）；把结果写进 tail 的 checksum 字段
/// （`block[block_size-4 .. block_size]`）。**门控 metadata_csum**：特性关时不写（与 Task 1
/// `dir_verify_block_csum` 同门控；`EXT4_NOCSUM_IMAGE` 覆盖此路径）。
pub(super) fn dir_set_csum(block: &mut [u8], sb: &RawSuperblock, ino_gen: u32, block_size: usize) {
    let has_csum = (sb.features_read_only() & RO_COMPAT_METADATA_CSUM) != 0;
    if !has_csum {
        return;
    }
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
pub(super) fn try_insert_to_existing_block(
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
pub(super) fn insert_to_new_block(
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
pub(super) fn dir_append_block(
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
pub(super) fn dir_add_entry(
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
pub(super) fn dir_add_entry_unchecked(
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
pub(super) fn dir_remove_entry(ctx: &WriteCtx, parent: &mut Inode, name: &[u8]) -> Result<()> {
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
pub(super) fn dir_remove_entry_at_offset(
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
pub(super) fn dir_has_entry(ctx: &ReadCtx, dir: &Inode) -> Result<bool> {
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
pub(super) fn inode_to_dir_entry_type(inode: &Inode) -> u8 {
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

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::{
        dir_block_csum, dir_find_in_block, dir_verify_block_csum, parse_entry, DirEntryFileType,
        RawDirEntryHeader, RawDirEntryTail, DIR_TAIL_MARKER, DIR_TAIL_SIZE,
    };
    use crate::fs::ext4::core::block_group::RawGroupDescriptor;
    use crate::fs::ext4::core::extents::{RawExtent, RawExtentHeader};
    use crate::fs::ext4::core::superblock::RawSuperblock;
    use crate::fs::ext4::core::test_util::{slice_at, EXT4_IMAGE, EXT4_NOCSUM_IMAGE};
    use crate::prelude::*;

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
}
