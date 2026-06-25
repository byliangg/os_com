// SPDX-License-Identifier: MPL-2.0
use ostd::const_assert;

use super::block_group::RawGroupDescriptor;
use super::crc::{ext4_crc32c, EXT4_CRC32_INIT};
use super::io::BlockReader;
use super::metadata_writer::MetadataWriter;
use super::prelude::*;
use super::superblock::RawSuperblock;

/// `i_flags` 中的 extent 标志位（INODE uses extents）。
/// [对照] ext4_rs `EXT4_INODE_FLAG_EXTENTS`（ext4_defs/consts.rs:21）= `0x00080000`。
/// core 自定义同值常量（`as u32`），不依赖 ext4_rs。
pub(in crate::fs::ext4) const EXT4_INODE_FLAG_EXTENTS: u32 = 0x0008_0000;

/// ext4 on-disk inode 的 OS-dependent #2 区（Linux 变体，12 字节）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub(in crate::fs::ext4) struct RawOsd2 {
    pub l_i_blocks_high: u16,
    pub l_i_file_acl_high: u16,
    pub l_i_uid_high: u16,
    pub l_i_gid_high: u16,
    pub l_i_checksum_lo: u16,
    pub l_i_reserved: u16,
}

/// ext4 on-disk inode（156 字节，小端）。逐字段镜像磁盘布局。
/// base 128B（mode..osd2）+ extra-isize 28B（i_extra_isize..i_version_hi）。
/// block:[u32;15] 兼作 extent 树根（接缝4，本阶段只保字节、不解释）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub(in crate::fs::ext4) struct RawInode {
    pub mode: u16,
    pub uid: u16,
    pub size: u32,
    pub atime: u32,
    pub ctime: u32,
    pub mtime: u32,
    pub dtime: u32,
    pub gid: u16,
    pub links_count: u16,
    pub blocks: u32,
    pub flags: u32,
    pub osd1: u32,
    pub block: [u32; 15],
    pub generation: u32,
    pub file_acl: u32,
    pub size_hi: u32,
    pub faddr: u32,
    pub osd2: RawOsd2,
    pub i_extra_isize: u16,
    pub i_checksum_hi: u16,
    pub i_ctime_extra: u32,
    pub i_mtime_extra: u32,
    pub i_atime_extra: u32,
    pub i_crtime: u32,
    pub i_crtime_extra: u32,
    pub i_version_hi: u32,
}

const_assert!(size_of::<RawInode>() == 156);

const S_IFMT: u16 = 0xF000;
const S_IFDIR: u16 = 0x4000;

impl RawInode {
    pub fn mode(&self) -> u16 {
        self.mode
    }
    pub fn links_count(&self) -> u16 {
        self.links_count
    }
    /// 写 links_count（委托同名字段）。
    /// [对照] ext4_rs `Ext4Inode::set_links_count`（ext4_defs/inode.rs:143）。
    pub fn set_links_count(&mut self, links_count: u16) {
        self.links_count = links_count;
    }
    /// 写 mode（含类型位 + 权限位）。
    /// [对照] ext4_rs `Ext4Inode::set_mode`（ext4_defs/inode.rs:78）。
    pub fn set_mode(&mut self, mode: u16) {
        self.mode = mode;
    }
    /// 写 inode 标志位（含 extent 标志）。
    /// [对照] ext4_rs `Ext4Inode::set_flags`（ext4_defs/inode.rs:164）。
    pub fn set_flags(&mut self, flags: u32) {
        self.flags = flags;
    }
    /// 写 i_extra_isize（128B+ inode 的额外尺寸）。
    /// [对照] ext4_rs `Ext4Inode::set_i_extra_isize`（ext4_defs/inode.rs:228）。
    pub fn set_i_extra_isize(&mut self, i_extra_isize: u16) {
        self.i_extra_isize = i_extra_isize;
    }
    /// 文件类型位（mode & 0xF000）。
    /// [对照] ext4_rs `Ext4Inode::file_type` 取 `mode & EXT4_INODE_MODE_TYPE_MASK`。
    pub fn file_type(&self) -> u16 {
        self.mode & S_IFMT
    }
    /// 文件大小（size | size_hi<<32）。
    pub fn size(&self) -> u64 {
        (self.size as u64) | ((self.size_hi as u64) << 32)
    }
    /// 已分配块数（blocks | osd2.l_i_blocks_high<<32）。
    pub fn blocks(&self) -> u64 {
        (self.blocks as u64) | ((self.osd2.l_i_blocks_high as u64) << 32)
    }
    /// i_block 原始 60 字节（extent/间接的解释留 Phase 3）。
    pub fn i_block(&self) -> [u32; 15] {
        self.block
    }
    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }

    /// inode 标志位（含 extent 标志）。
    /// [对照] ext4_rs `Ext4Inode::flags`（ext4_defs/inode.rs:160）。
    pub fn flags(&self) -> u32 {
        self.flags
    }
    /// inode generation（crc32c 种子之一）。
    /// [对照] ext4_rs `Ext4Inode::generation`（ext4_defs/inode.rs:184）。
    pub fn generation(&self) -> u32 {
        self.generation
    }
    /// 写文件大小（lo = size&0xffffffff，hi = size>>32）。
    /// [对照] ext4_rs `Ext4Inode::set_size`（ext4_defs/inode.rs:94）。
    pub fn set_size(&mut self, size: u64) {
        self.size = (size & 0xffff_ffff) as u32;
        self.size_hi = (size >> 32) as u32;
    }
}

/// inode 元数据校验和（crc32c）——**纯函数**，逐字节复刻 ext4_rs
/// `Ext4Inode::get_inode_checksum`（`ext4_defs/inode.rs:425`）。
///
/// 计算前在**本地拷贝**上把两个 csum 字段清零（`osd2.l_i_checksum_lo` / `i_checksum_hi`），
/// **不**改入参 `raw`；要写回 lo/hi 用 [`write_inode_checksum_into`]。
///
/// 步骤（与 ext4_rs 一一对应）：
/// 1. 本地拷贝清零 csum lo/hi；
/// 2. `c = crc32c(INIT, uuid)`（16 字节）；
/// 3. `c = crc32c(c, inode_id.to_le_bytes())`（4 字节小端）；
/// 4. `c = crc32c(c, generation.to_le_bytes())`（4 字节小端）；
/// 5. 把 inode 的**前 0x9c = 156 字节**拷进 256 字节零缓冲（其余 100 字节保持 0）；
/// 6. `c = crc32c(c, &raw_data[..inode_size])`（覆盖 156 真实 + 100 零字节）；
/// 7. `if inode_size == 128 { c &= 0xFFFF }`。
pub(in crate::fs::ext4) fn inode_checksum(raw: &RawInode, inode_id: u32, sb: &RawSuperblock) -> u32 {
    let inode_size = sb.inode_size() as usize;

    // 1) 本地拷贝（不动入参 raw），在拷贝上清零 csum lo/hi——与 ext4_rs 在算前
    //    把 `osd2.l_i_checksum_lo`/`i_checksum_hi` 置 0 一致（否则把旧 csum 算进去）。
    let mut work = *raw;
    work.osd2.l_i_checksum_lo = 0;
    work.i_checksum_hi = 0;

    // 2) crc32c(INIT, uuid)（16 字节）。
    let uuid = sb.uuid();
    let mut c = ext4_crc32c(EXT4_CRC32_INIT, &uuid);
    // 3) inode_id（4 字节小端）。
    c = ext4_crc32c(c, &inode_id.to_le_bytes());
    // 4) generation（4 字节小端）。
    c = ext4_crc32c(c, &work.generation.to_le_bytes());

    // 5) PARITY: replicate ext4_rs behavior, fix deferred (roadmap §5)
    //    ext4_rs `copy_to_slice`(inode.rs:417) 只拷 0x9c=156 字节进 256 字节零缓冲——
    //    这是实现细节（非 ext4 规范的整 inode），逐字节复刻。`RawInode` 恰 156 字节，
    //    故 `as_bytes()`（156 字节）即那 0x9c 字节；其余 100 字节保持 0。
    let mut raw_data = [0u8; 0x100];
    let work_bytes = work.as_bytes();
    raw_data[..work_bytes.len()].copy_from_slice(work_bytes);

    // 6) PARITY: replicate ext4_rs behavior, fix deferred (roadmap §5)
    //    crc 覆盖 `&raw_data[..inode_size]`——长度是 `sb.inode_size()`（本镜像=256），
    //    即 156 真实字节 + 100 零字节，而非仅 156。
    c = ext4_crc32c(c, &raw_data[..inode_size]);

    // 7) 128B inode 只有 lo 半。
    if inode_size == 128 {
        c &= 0xFFFF;
    }
    c
}

/// 算出 csum 后写回 `raw` 的 lo/hi 字段，对齐 ext4_rs `set_inode_checksum`（`inode.rs:461`）：
/// `osd2.l_i_checksum_lo = c & 0xFFFF`；`if inode_size > 128 { i_checksum_hi = c >> 16 }`。
pub(in crate::fs::ext4) fn write_inode_checksum_into(raw: &mut RawInode, inode_id: u32, sb: &RawSuperblock) {
    let c = inode_checksum(raw, inode_id, sb);
    raw.osd2.l_i_checksum_lo = (c & 0xFFFF) as u16;
    if sb.inode_size() > 128 {
        raw.i_checksum_hi = (c >> 16) as u16;
    }
}

/// inode 逻辑句柄：盘上 [`RawInode`] + inode 号。对齐 ext2 `InodeDesc`↔`RawInode` 与
/// ext4_rs `Ext4InodeRef { inode_num, inode }`。逻辑访问器委托 [`RawInode`]，不重复定义
/// `size()`/`blocks()` 等已有 accessor。
pub(in crate::fs::ext4) struct Inode {
    pub raw: RawInode,
    pub num: u32,
}

impl Inode {
    /// inode 标志位（委托 [`RawInode::flags`]）。
    pub(in crate::fs::ext4) fn flags(&self) -> u32 {
        self.raw.flags()
    }
    /// 文件大小（委托 [`RawInode::size`]）。
    pub(in crate::fs::ext4) fn size(&self) -> u64 {
        self.raw.size()
    }
    /// 写文件大小（委托 [`RawInode::set_size`]）。
    pub(in crate::fs::ext4) fn set_size(&mut self, size: u64) {
        self.raw.set_size(size);
    }
    /// inode generation（委托 [`RawInode::generation`]）。
    /// 对称访问器，当前 core 路径用 `raw.generation()`，本包装暂无调用者（保留以备目录/属性路径）。
    #[allow(dead_code)]
    pub(in crate::fs::ext4) fn generation(&self) -> u32 {
        self.raw.generation()
    }
    /// 是否 extent 映射：`(flags() & EXT4_INODE_FLAG_EXTENTS) != 0`。
    /// [对照] ext4_rs `Ext4::inode_uses_extents`（ext4_impls/inode.rs:14）。
    pub(in crate::fs::ext4) fn uses_extents(&self) -> bool {
        (self.flags() & EXT4_INODE_FLAG_EXTENTS) != 0
    }

    /// i_block 的 60 字节（小端）——extent 树根字节视图。
    ///
    /// 安全替代 ext4_rs `find_extent` 里的 `transmute::<&[u32;15], &[u8;60]>`
    /// （ext4_impls/extents.rs:188）：`[u32;15]` 是 Pod，`as_bytes()` 给出其
    /// 60 字节小端镜像，与 transmute 逐字节等价。
    pub(in crate::fs::ext4) fn i_block_bytes(&self) -> [u8; 60] {
        let block = self.raw.i_block();
        let mut out = [0u8; 60];
        out.copy_from_slice(block.as_bytes());
        out
    }

    /// i_block 的 60 字节作 `Vec`（写半部要可变 buffer 做移位/改 extent）。
    pub(in crate::fs::ext4) fn i_block_bytes_vec(&self) -> Vec<u8> {
        self.i_block_bytes().to_vec()
    }

    /// 把 60 字节写回 `raw.block: [u32;15]`（安全 Pod，替代 ext4_rs 裸指针改 i_block）。
    /// `bytes` 必须恰 60 字节（一个 [u32;15] 的字节镜像）。
    pub(in crate::fs::ext4) fn set_i_block_bytes(&mut self, bytes: &[u8]) {
        debug_assert_eq!(bytes.len(), 60, "i_block must be 60 bytes");
        let block: [u32; 15] = Pod::from_bytes(bytes);
        self.raw.block = block;
    }

    /// 写 i_blocks（512B 单位）：lo = blocks & 0xffffffff，hi = blocks >> 32。
    /// [对照] ext4_rs `Ext4Inode::set_blocks_count`（i_blocks 累加用）。
    pub(in crate::fs::ext4) fn set_blocks_count(&mut self, blocks: u64) {
        self.raw.blocks = (blocks & 0xffff_ffff) as u32;
        self.raw.osd2.l_i_blocks_high = (blocks >> 32) as u16;
    }

    /// 读 i_blocks（512B 单位，委托 [`RawInode::blocks`]）。
    pub(in crate::fs::ext4) fn blocks_count(&self) -> u64 {
        self.raw.blocks()
    }

    /// 链接计数（委托 [`RawInode::links_count`]）。
    pub(in crate::fs::ext4) fn links_count(&self) -> u16 {
        self.raw.links_count()
    }
    /// 写链接计数（委托 [`RawInode::set_links_count`]）。
    pub(in crate::fs::ext4) fn set_links_count(&mut self, n: u16) {
        self.raw.set_links_count(n);
    }
    /// 文件类型位（mode & 0xF000，委托 [`RawInode::file_type`]）。
    pub(in crate::fs::ext4) fn file_type(&self) -> u16 {
        self.raw.file_type()
    }
    /// 是否目录（委托 [`RawInode::is_dir`]）。
    pub(in crate::fs::ext4) fn is_dir(&self) -> bool {
        self.raw.is_dir()
    }
}

/// 构造一个**新分配** inode 的逻辑句柄（in-memory），逐字节复刻 ext4_rs `create_inode`
/// （ext4_impls/file.rs:589）：从全 0 的 [`RawInode`] 起，按 `inode_mode` 设 mode、按
/// `inode_size > 128` 设 i_extra_isize，再按类型（dir/reg）设 EXTENTS flag + 初始化空
/// extent header。links_count 起 **0**（ext4_rs `Ext4Inode::default()`；link 时再 +1/=2）。
///
/// PARITY 要点：
/// - `mode = file_type_bits(inode_mode & 0xF000，非 dir/reg 归一为 REG 的语义见下) | (inode_mode & 0x0FFF)`。
///   ext4_rs：`InodeFileType::from_bits(inode_mode & 0xF000)`，无法识别的类型位回落 S_IFREG
///   （`bits()`=0x8000）；core 用 [`normalize_file_type_bits`] 复刻同一回落。
/// - extra_isize：仅 `inode_size > 128` 时设为 SB 的 `want_extra_isize`。
/// - dir 或 reg：flags = EXTENTS(0x80000)，extent header = {magic:0xF30A, entries:0, max:4,
///   depth:0, generation:0} 写在 i_block 前 12 字节；其余 i_block 字节保持 0。其它类型 flags=0。
pub(in crate::fs::ext4) fn init_new_inode(inode_num: u32, inode_mode: u16, sb: &RawSuperblock) -> Inode {
    const EXT4_INODE_MODE_TYPE_MASK: u16 = 0xF000;
    const EXT4_INODE_MODE_PERM_MASK: u16 = 0x0FFF;
    const S_IFREG: u16 = 0x8000;
    const S_IFDIR_TY: u16 = 0x4000;
    const EXT4_GOOD_OLD_INODE_SIZE: u16 = 128;

    let mut raw = RawInode::default();

    // PARITY: ext4_rs `InodeFileType::from_bits(inode_mode & TYPE_MASK)`，未知类型位 → S_IFREG。
    let file_type_bits = normalize_file_type_bits(inode_mode & EXT4_INODE_MODE_TYPE_MASK);
    let is_dir = file_type_bits == S_IFDIR_TY;
    let is_reg = file_type_bits == S_IFREG;

    // PARITY: 保留调用方权限位，仅归一类型位。
    let file_mode = file_type_bits | (inode_mode & EXT4_INODE_MODE_PERM_MASK);
    raw.set_mode(file_mode);

    // PARITY: inode_size > 128 时设 i_extra_isize = SB want_extra_isize。
    if sb.inode_size() > EXT4_GOOD_OLD_INODE_SIZE {
        raw.set_i_extra_isize(sb.want_extra_isize);
    }

    if is_dir || is_reg {
        raw.set_flags(EXT4_INODE_FLAG_EXTENTS);
        extent_tree_init_into(&mut raw);
    } else {
        raw.set_flags(0);
    }

    Inode {
        raw,
        num: inode_num,
    }
}

/// 把 `mode & 0xF000` 归一为 ext4_rs `InodeFileType::from_bits` 的语义：识别的类型位原样
/// 返回，未知类型位回落 S_IFREG（0x8000）。复刻 `create_inode` 里 `from_bits(...).unwrap_or(S_IFREG)`。
fn normalize_file_type_bits(type_bits: u16) -> u16 {
    const S_IFIFO: u16 = 0x1000;
    const S_IFCHR: u16 = 0x2000;
    const S_IFDIR_TY: u16 = 0x4000;
    const S_IFBLK: u16 = 0x6000;
    const S_IFREG: u16 = 0x8000;
    const S_IFLNK: u16 = 0xA000;
    const S_IFSOCK: u16 = 0xC000;
    match type_bits {
        S_IFIFO | S_IFCHR | S_IFDIR_TY | S_IFBLK | S_IFREG | S_IFLNK | S_IFSOCK => type_bits,
        _ => S_IFREG,
    }
}

/// 在 `raw.block`（i_block 的 [u32;15]）前 12 字节写入空 extent 根头——安全 Pod 等价
/// ext4_rs `Ext4Inode::extent_tree_init`（ext4_defs/inode.rs:384，原用裸指针写 header）。
///
/// PARITY：header = {magic:0xF30A, entries_count:0, max_entries_count:4, depth:0, generation:0}；
/// i_block 其余字节（`init_new_inode` 从全 0 起步）保持 0。
fn extent_tree_init_into(raw: &mut RawInode) {
    use super::extents::{RawExtentHeader, EXTENT_MAGIC};
    let header = RawExtentHeader::new(EXTENT_MAGIC, 0, 4, 0, 0);
    let mut i_block = [0u8; 60];
    i_block.copy_from_slice(raw.block.as_bytes());
    i_block[..size_of::<RawExtentHeader>()].copy_from_slice(header.as_bytes());
    let block: [u32; 15] = Pod::from_bytes(&i_block);
    raw.block = block;
}

/// 从盘读第 `group` 组的组描述符（GDT 紧跟超级块块），与 Phase-2 分配器
/// [`super::balloc::BlockAllocator::load_group_desc`] / the former differential harness `snapshot_inode_table_group`
/// 同一定位逻辑：`block_id = first_data_block + group/dsc_cnt + 1`、块内偏移
/// `(group % dsc_cnt) * desc_size`，读满 64 字节后解析（desc_size==64 的真镜像下与 ext4_rs 一致）。
fn load_group_desc(reader: &dyn BlockReader, sb: &RawSuperblock, group: u32) -> RawGroupDescriptor {
    let bs = sb.block_size();
    let desc_size = sb.group_desc_size();
    let dsc_cnt = bs / desc_size;
    let dsc_id = group as usize / dsc_cnt;
    let first_data_block = sb.first_data_block() as usize;
    let block_id = first_data_block + dsc_id + 1;
    let offset_in_block = (group as usize % dsc_cnt) * desc_size;
    let off = block_id * bs + offset_in_block;
    let mut buf = [0u8; 64];
    reader.read_at(off, &mut buf);
    RawGroupDescriptor::from_bytes(&buf)
}

/// inode `inode_num` 在盘上的字节偏移。
/// [对照] ext4_rs `Ext4::inode_disk_pos`（ext4_impls/inode.rs:173）：
/// 组 `(inode_num-1)/inodes_per_group`、组内序号 `(inode_num-1)%inodes_per_group`、
/// `inode_table_blk * block_size + index * inode_size`（**inode_size 来自 SB，非
/// `size_of::<RawInode>()`=156**——本镜像 inode_size=256）。
///
/// ext4_rs 读 `self.inode_table_blocks[group]`（启动期缓存）；该缓存即各组描述符的
/// `get_inode_table_blk_num()`（ext4_impls/ext4.rs:90-98），故此处直接从盘读该组描述符
/// 取 `inode_table()`，字节等价、且不引入全局缓存/单例。
pub(in crate::fs::ext4) fn inode_disk_pos(
    reader: &dyn BlockReader,
    sb: &RawSuperblock,
    inode_num: u32,
) -> usize {
    let block_size = sb.block_size();
    let inodes_per_group = sb.inodes_per_group();
    let inode_size = sb.inode_size() as usize;
    let group = (inode_num - 1) / inodes_per_group;
    let index = (inode_num - 1) % inodes_per_group;
    let desc = load_group_desc(reader, sb, group);
    let inode_table_blk = desc.inode_table() as usize;
    inode_table_blk * block_size + index as usize * inode_size
}

/// inode 号合法性校验（0 或 > inodes_count → EIO）。
/// [对照] ext4_rs `Ext4::validate_inode_number`（ext4_impls/inode.rs:205）。
fn validate_inode_number(sb: &RawSuperblock, inode_num: u32) -> Result<()> {
    if inode_num == 0 || inode_num > sb.inodes_count() {
        return Err(Error::with_message(Errno::EIO, "invalid inode number"));
    }
    Ok(())
}

/// 从盘加载 inode `inode_num`：定位 → 读 inode 所在对齐块 → `RawInode::from_bytes`。
/// [对照] ext4_rs `Ext4::get_inode_ref`（ext4_impls/inode.rs:213）——先 `validate_inode_number`，
/// 再读 `inode_disk_pos` 所在对齐块、取块内偏移处的 inode 镜像。core 用注入的
/// `&dyn BlockReader`（不持全局盘）。
pub(in crate::fs::ext4) fn load_inode(
    reader: &dyn BlockReader,
    sb: &RawSuperblock,
    inode_num: u32,
) -> Result<Inode> {
    validate_inode_number(sb, inode_num)?;
    let block_size = sb.block_size();
    let pos = inode_disk_pos(reader, sb, inode_num);
    // 读 inode 所在对齐块（与 write_back_inode 同形状）。`RawInode` 占 156 字节，
    // inode_size 整除 block_size 时不会跨块；从块内偏移取 156 字节解析。
    let block_offset = pos / block_size * block_size;
    let offset_in_block = pos - block_offset;
    let mut block = vec![0u8; block_size];
    reader.read_at(block_offset, block.as_mut_slice());
    let n = size_of::<RawInode>();
    let raw = RawInode::from_bytes(&block[offset_in_block..offset_in_block + n]);
    Ok(Inode {
        raw,
        num: inode_num,
    })
}

/// 把 inode 写回盘：先算 csum 写回 lo/hi（[`write_inode_checksum_into`]），再
/// RMW inode 所在整块——读出整块、覆盖该 inode 的 `inode_size` 字节、整块经
/// [`MetadataWriter`] 写。
///
/// [对照] ext4_rs `Ext4::write_back_inode`（ext4_impls/inode.rs:242）= `set_inode_checksum`
/// 再 `write_inode_image`（:185）。
///
/// **BUG-10 已修（read-modify-write 保留尾区）**：`RawInode` 建模 inode 的前 156 字节
/// （base 128B + extra-isize 字段直到 `i_version_hi`，`size_of::<RawInode>()==156`）；
/// 256 字节 inode 的尾区 `[156, inode_size)` 是 core **不建模**的区域（ext4 的内联 xattr /
/// 未来字段所在）。ext4_rs `write_inode_image` 每次写回都把该尾区 `fill(0)` **抹零**——这是数据
/// 丢失（销毁盘上未建模的 inode 尾字节）。core 改为 ext4-correct 的 RMW：读出 inode 所在整块、
/// **只覆盖** core 管理的 `RawInode`（156 字节）区，尾区 `[156, inode_size)` 保留盘上原值
/// （不 own 的字节不动）。整块 RMW 同时保留同块内**其它 inode** 的字节。
pub(in crate::fs::ext4) fn write_back_inode(
    writer: &dyn MetadataWriter,
    reader: &dyn BlockReader,
    sb: &RawSuperblock,
    inode: &mut Inode,
) -> Result<()> {
    // 1) 先算 csum 写回 lo/hi（对齐 ext4_rs set_inode_checksum 先于 write_inode_image）。
    write_inode_checksum_into(&mut inode.raw, inode.num, sb);

    let block_size = sb.block_size();
    let inode_size = sb.inode_size() as usize;
    let pos = inode_disk_pos(reader, sb, inode.num);
    let block_offset = pos / block_size * block_size;
    let offset_in_block = pos - block_offset;

    // 2) RMW：读出 inode 所在整块。
    let mut block = vec![0u8; block_size];
    reader.read_at(block_offset, block.as_mut_slice());

    // 3) 只覆盖 core 管理的 `RawInode`（156 字节）区，尾区 `[156, inode_size)` 经 RMW 保留盘上
    //    原值——[对照] 不抹零未建模的 inode 尾字节（ext4 内联 xattr / 未来字段），修 BUG-10。
    //    `inode_size < 156`（128B inode，无 extra-isize）时只拷盘上容得下的字节。
    let raw_bytes = inode.raw.as_bytes();
    let copy_len = core::cmp::min(inode_size, raw_bytes.len());
    block[offset_in_block..offset_in_block + copy_len].copy_from_slice(&raw_bytes[..copy_len]);

    // 4) 整块经 MetadataWriter 写（handle_id 占位 0，与 Phase-2 分配器一致）。
    let block_id = (block_offset / block_size) as Ext4Fsblk;
    writer.write_metadata_for_handle(0, block_id, &block)
}

#[cfg(ktest)]
mod test {
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use ostd::prelude::*;

    use super::{inode_disk_pos, load_inode, write_back_inode, RawInode};
    use crate::fs::ext4::core::io::BlockReader;
    use crate::fs::ext4::core::metadata_writer::MetadataWriter;
    use crate::fs::ext4::core::superblock::RawSuperblock;
    use crate::fs::ext4::core::test_util::EXT4_IMAGE;
    use crate::fs::ext4::core::types::Ext4Fsblk;
    use crate::prelude::Result;
    use crate::prelude::*;

    /// 内存盘：读 + 元数据写都打到同一份字节。
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
        fn read_bytes(&self, off: usize, len: usize) -> Vec<u8> {
            let b = self.bytes.borrow();
            (0..len).map(|i| b.get(off + i).copied().unwrap_or(0)).collect()
        }
        fn write_bytes(&self, off: usize, data: &[u8]) {
            let mut b = self.bytes.borrow_mut();
            for (i, byte) in data.iter().enumerate() {
                if let Some(slot) = b.get_mut(off + i) {
                    *slot = *byte;
                }
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
    impl MetadataWriter for MemImage {
        fn write_metadata_for_handle(
            &self,
            _handle_id: u64,
            block: Ext4Fsblk,
            data: &[u8],
        ) -> Result<()> {
            self.write_bytes(block as usize * self.block_size, data);
            Ok(())
        }
    }

    #[ktest]
    fn write_back_inode_preserves_unmanaged_tail() {
        // BUG-10：256B inode 的尾区 [156, inode_size) 是 core 不建模区（ext4 内联 xattr / 未来字段）。
        // write_back_inode 必须**保留**这些字节、不抹零。
        let disk = MemImage::new(EXT4_IMAGE);
        let sb = RawSuperblock::from_bytes(&EXT4_IMAGE[1024..2048]);
        let inode_size = sb.inode_size() as usize;
        assert_eq!(inode_size, 256, "本镜像 inode_size 应为 256");
        let raw_len = size_of::<RawInode>();
        assert_eq!(raw_len, 156, "RawInode 应建模 156 字节");

        let ino = 11u32;
        let pos = inode_disk_pos(&disk, &sb, ino);

        // 在 inode 槽的尾区 [156, 256) 播一段可辨识的非零模式（模拟盘上既存的内联 xattr）。
        let tail_pattern: Vec<u8> =
            (0..(inode_size - raw_len)).map(|i| (0xA0 + (i & 0x3f)) as u8).collect();
        disk.write_bytes(pos + raw_len, &tail_pattern);

        // 加载 → 改一个 core 管理的字段（i_size）→ 写回。
        let mut inode = load_inode(&disk, &sb, ino).unwrap();
        inode.set_size(123456);
        write_back_inode(&disk, &disk, &sb, &mut inode).unwrap();

        // 断言①：尾区字节**原封不动**（这正是 BUG-10 的抹零会破坏的）。
        let tail_after = disk.read_bytes(pos + raw_len, inode_size - raw_len);
        assert_eq!(
            tail_after, tail_pattern,
            "inode 尾区 [156,256) 必须被保留，不得抹零"
        );

        // 断言②：core 管理区确实被更新（i_size 写进盘上 RawInode）。
        let reloaded = load_inode(&disk, &sb, ino).unwrap();
        assert_eq!(reloaded.size(), 123456, "core 管理的 i_size 必须落盘");
    }
}
