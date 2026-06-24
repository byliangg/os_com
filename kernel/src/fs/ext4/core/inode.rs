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
pub(super) const EXT4_INODE_FLAG_EXTENTS: u32 = 0x0008_0000;

/// ext4 on-disk inode 的 OS-dependent #2 区（Linux 变体，12 字节）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub(super) struct RawOsd2 {
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
pub(super) struct RawInode {
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
pub(super) fn inode_checksum(raw: &RawInode, inode_id: u32, sb: &RawSuperblock) -> u32 {
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
#[allow(dead_code)]
pub(super) fn write_inode_checksum_into(raw: &mut RawInode, inode_id: u32, sb: &RawSuperblock) {
    let c = inode_checksum(raw, inode_id, sb);
    raw.osd2.l_i_checksum_lo = (c & 0xFFFF) as u16;
    if sb.inode_size() > 128 {
        raw.i_checksum_hi = (c >> 16) as u16;
    }
}

/// inode 逻辑句柄：盘上 [`RawInode`] + inode 号。对齐 ext2 `InodeDesc`↔`RawInode` 与
/// ext4_rs `Ext4InodeRef { inode_num, inode }`。逻辑访问器委托 [`RawInode`]，不重复定义
/// `size()`/`blocks()` 等已有 accessor。
pub(super) struct Inode {
    pub raw: RawInode,
    pub num: u32,
}

impl Inode {
    /// inode 标志位（委托 [`RawInode::flags`]）。
    pub(super) fn flags(&self) -> u32 {
        self.raw.flags()
    }
    /// 文件大小（委托 [`RawInode::size`]）。
    pub(super) fn size(&self) -> u64 {
        self.raw.size()
    }
    /// 写文件大小（委托 [`RawInode::set_size`]）。
    #[allow(dead_code)]
    pub(super) fn set_size(&mut self, size: u64) {
        self.raw.set_size(size);
    }
    /// inode generation（委托 [`RawInode::generation`]）。
    #[allow(dead_code)]
    pub(super) fn generation(&self) -> u32 {
        self.raw.generation()
    }
    /// 是否 extent 映射：`(flags() & EXT4_INODE_FLAG_EXTENTS) != 0`。
    /// [对照] ext4_rs `Ext4::inode_uses_extents`（ext4_impls/inode.rs:14）。
    pub(super) fn uses_extents(&self) -> bool {
        (self.flags() & EXT4_INODE_FLAG_EXTENTS) != 0
    }

    /// i_block 的 60 字节（小端）——extent 树根字节视图。
    ///
    /// 安全替代 ext4_rs `find_extent` 里的 `transmute::<&[u32;15], &[u8;60]>`
    /// （ext4_impls/extents.rs:188）：`[u32;15]` 是 Pod，`as_bytes()` 给出其
    /// 60 字节小端镜像，与 transmute 逐字节等价。
    pub(super) fn i_block_bytes(&self) -> [u8; 60] {
        let block = self.raw.i_block();
        let mut out = [0u8; 60];
        out.copy_from_slice(block.as_bytes());
        out
    }
}

/// 从盘读第 `group` 组的组描述符（GDT 紧跟超级块块），与 Phase-2 分配器
/// [`super::balloc::BlockAllocator::load_group_desc`] / `diff_harness::snapshot_inode_table_group`
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
#[allow(dead_code)]
pub(super) fn inode_disk_pos(
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
pub(super) fn load_inode(
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
/// **PARITY（BUG-10）**：`RawInode` 建模 156 字节，本镜像 `inode_size`=256。ext4_rs
/// `write_inode_image` 把 `Ext4Inode`（同样 156 字节）按 `min(inode_size, sizeof)`=156 拷入
/// inode 槽，再把余下 `[156, inode_size)` 用 `fill(0)` **清零**——即 inode 尾 100 字节落盘恒为
/// 0，**不是**保留盘上原值。core 逐字节复刻：覆盖 156 真实字节后同样把尾 100 字节清零（而非
/// 依赖 RMW 保留）。整块用 RMW 是为了保留**同块内其它 inode** 的字节，不是为了保留本 inode 的
/// 尾区。语义上这丢弃了未建模的 inode 尾字节（标准 mkfs 镜像该区本就是 0，故无感）；登记
/// 根目录 `bug.md` BUG-10，迁移后评估。
pub(super) fn write_back_inode(
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

    // 3) 覆盖该 inode 的 inode_size 字节：前 156 = RawInode 镜像，余下 [156, inode_size) 清零。
    //    PARITY: ext4_rs write_inode_image 拷 min(inode_size, sizeof Ext4Inode)=156 字节后，
    //    把 [156, inode_size) `fill(0)`——故这 100 字节落盘恒为 0，不是「保留盘上原值」。
    //    为逐字节一致，core 同样把尾部清零，而非依赖 RMW 保留原值。
    let raw_bytes = inode.raw.as_bytes();
    let copy_len = core::cmp::min(inode_size, raw_bytes.len());
    block[offset_in_block..offset_in_block + copy_len].copy_from_slice(&raw_bytes[..copy_len]);
    if inode_size > copy_len {
        block[offset_in_block + copy_len..offset_in_block + inode_size].fill(0);
    }

    // 4) 整块经 MetadataWriter 写（handle_id 占位 0，与 Phase-2 分配器一致）。
    let block_id = (block_offset / block_size) as Ext4Fsblk;
    writer.write_metadata_for_handle(0, block_id, &block)
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::{load_inode, write_back_inode, Inode, RawInode, EXT4_INODE_FLAG_EXTENTS};
    use crate::fs::ext4::core::block_group::RawGroupDescriptor;
    use crate::fs::ext4::core::diff_harness::{
        assert_meta_eq, snapshot_meta_with_inodes, DirectMetadataWriter, MemDisk,
    };
    use crate::fs::ext4::core::io::BlockReader;
    use crate::fs::ext4::core::superblock::RawSuperblock;
    use crate::fs::ext4::core::test_util::{slice_at, EXT4_IMAGE};
    use crate::prelude::*;

    /// 从内存盘读超级块（布局推导）。
    fn read_sb(disk: &MemDisk) -> RawSuperblock {
        let mut buf = vec![0u8; 1024];
        disk.read_at(1024, buf.as_mut_slice());
        RawSuperblock::from_bytes(&buf)
    }

    #[ktest]
    fn inode_roundtrip_handcrafted() {
        let mut bytes = [0u8; 156];
        bytes[0..2].copy_from_slice(&0o100644u16.to_le_bytes()); // i_mode
        bytes[4..8].copy_from_slice(&4096u32.to_le_bytes()); // i_size_lo
        let raw = RawInode::from_bytes(&bytes);
        assert_eq!(raw.as_bytes(), &bytes[..]);
        assert_eq!(raw.mode(), 0o100644);
        assert_eq!(raw.size(), 4096);
    }

    #[ktest]
    fn inode_root_diff_old_real_image() {
        // 定位根 inode（ino=2）：组0描述符的 inode_table 块 + (2-1)*inode_size。
        let sb = RawSuperblock::from_bytes(slice_at(1024, 1024));
        let bs = sb.block_size();
        let gdt_off = (sb.first_data_block as usize + 1) * bs;
        let gd = RawGroupDescriptor::from_bytes(slice_at(gdt_off, 64));
        let inode_size = sb.inode_size() as usize;
        let itable = gd.inode_table() as usize;
        let off = itable * bs + (2 - 1) * inode_size;
        let n = size_of::<RawInode>();
        let bytes = slice_at(off, n);
        let raw = RawInode::from_bytes(bytes);
        assert_eq!(raw.as_bytes(), bytes, "round-trip");
        // 根 inode 必是目录。
        assert!(raw.is_dir(), "root inode is a directory");
        // 全字段对拍旧实现（Ext4Inode 全字段 pub）。
        let old = ext4_rs::Ext4Inode::from_bytes(bytes);
        assert_eq!(raw.mode, old.mode, "mode");
        assert_eq!(raw.uid, old.uid, "uid");
        assert_eq!(raw.size, old.size, "size_lo");
        assert_eq!(raw.gid, old.gid, "gid");
        assert_eq!(raw.links_count, old.links_count, "links_count");
        assert_eq!(raw.blocks, old.blocks, "blocks");
        assert_eq!(raw.flags, old.flags, "flags");
        assert_eq!(raw.block, old.block, "i_block[15]");
        assert_eq!(raw.generation, old.generation, "generation");
        assert_eq!(raw.size_hi, old.size_hi, "size_hi");
    }

    /// inode load→改无害字段→write_back 后，inode 表字节（含 csum）与 ext4_rs 逐字节一致。
    ///
    /// 两张独立 `MemDisk`（同镜像，互不共享 Arc）：
    /// - 旧侧：`ext4_rs::Ext4::open` → `get_inode_ref(2)` → `atime += 1` → `write_back_inode`；
    /// - 新侧：`load_inode(2)` → `atime += 1`（同改）→ `write_back_inode`。
    /// 两盘 `snapshot_meta_with_inodes`（含 inode 表 + csum）逐字节对拍。
    /// 这正是 Task-0 csum parity 的下游验证：唯一盘面 delta 在 inode #2，且 csum 必须一致。
    #[ktest]
    fn inode_load_writeback_parity() {
        const INO: u32 = 2;

        // 两张独立内存盘（各自 from_image 同字节，互不共享 Arc）。
        let old_disk = MemDisk::from_image(EXT4_IMAGE);
        let new_disk = MemDisk::from_image(EXT4_IMAGE);
        let sb = read_sb(&new_disk);

        // 旧侧：经 ext4_rs 改 atime+1 再 write_back_inode。
        let ext4 = ext4_rs::Ext4::open(Arc::new(old_disk.clone()));
        let mut old_ref = ext4.get_inode_ref(INO);
        let old_atime = old_ref.inode.atime();
        old_ref.inode.set_atime(old_atime.wrapping_add(1));
        ext4.write_back_inode(&mut old_ref);

        // 新侧：load_inode → 同改 atime+1 → write_back_inode（注入 reader + writer）。
        let bs = sb.block_size();
        let writer = DirectMetadataWriter::new(new_disk.clone(), bs);
        let mut inode = load_inode(&new_disk, &sb, INO).expect("load_inode #2");
        // 确认两侧读到同一初值（同镜像）。
        assert_eq!(inode.raw.atime, old_atime, "new load atime == old initial atime");
        inode.raw.atime = inode.raw.atime.wrapping_add(1);
        write_back_inode(&writer, &new_disk, &sb, &mut inode).expect("write_back_inode #2");

        // 两盘元数据（含 inode 表 + csum）逐字节一致。
        let old_snap = snapshot_meta_with_inodes(&old_disk, &sb);
        let new_snap = snapshot_meta_with_inodes(&new_disk, &sb);
        assert_meta_eq(&old_snap, &new_snap);
    }

    /// 派发谓词 `uses_extents()` 与 ext4_rs `inode_uses_extents` 一致：
    /// - 真镜像 inode #2（flags 含 EXTENTS）→ true，两侧一致；
    /// - 构造的 legacy-flag inode（清 EXTENTS 位）→ false，两侧一致。
    ///
    /// 只验证**谓词**（不触发任一映射分支的 `unimplemented!`，按控制器歧义裁决 #1）。
    #[ktest]
    fn dispatch_predicate_parity() {
        let disk = MemDisk::from_image(EXT4_IMAGE);
        let sb = read_sb(&disk);

        // 真镜像 inode #2：新侧 uses_extents() 应与旧侧 inode_uses_extents 一致。
        let ext4 = ext4_rs::Ext4::open(Arc::new(disk.clone()));
        let old_ref = ext4.get_inode_ref(2);
        let old_uses_extents =
            (old_ref.inode.flags() & EXT4_INODE_FLAG_EXTENTS) != 0;

        let inode = load_inode(&disk, &sb, 2).expect("load_inode #2");
        assert_eq!(
            inode.uses_extents(),
            old_uses_extents,
            "uses_extents(#2) parity with ext4_rs inode_uses_extents"
        );
        // 真镜像（INCOMPAT_EXTENTS 开）下根 inode 应走 extent。
        assert!(inode.uses_extents(), "real image inode #2 uses extents");

        // 构造 legacy-flag inode（清 EXTENTS 位）：两侧谓词同为 false。
        let mut legacy = Inode {
            raw: inode.raw,
            num: 2,
        };
        legacy.raw.flags &= !EXT4_INODE_FLAG_EXTENTS;
        let mut old_legacy = old_ref.inode;
        old_legacy.set_flags(old_legacy.flags() & !EXT4_INODE_FLAG_EXTENTS);
        let old_legacy_uses = (old_legacy.flags() & EXT4_INODE_FLAG_EXTENTS) != 0;
        assert_eq!(
            legacy.uses_extents(),
            old_legacy_uses,
            "uses_extents(legacy) parity"
        );
        assert!(!legacy.uses_extents(), "legacy-flag inode does not use extents");
    }
}
