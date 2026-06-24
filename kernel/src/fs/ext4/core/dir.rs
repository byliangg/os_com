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
use super::extents::get_pblock_idx_state;
use super::file::ReadCtx;
use super::inode::Inode;
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
