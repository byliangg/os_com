// SPDX-License-Identifier: MPL-2.0
//! Phase-2 算法层差分地基（仅 ktest 构建）。
//!
//! 一张可写内存盘 [`MemDisk`]（底层 `Arc<Mutex<Vec<u8>>>`）同时实现：
//! - `ext4_rs::BlockDevice`：让旧第三方引擎读写它；
//! - core 本地的 [`BlockReader`]：让新安全核心读它；
//! - 直写 [`DirectMetadataWriter`]：实现 core 本地 [`MetadataWriter`]，把元数据
//!   全块镜像**立即**写穿同一份字节（无 JBD2 延迟——overlay 留 Phase 5）。
//!
//! Step 0 的往返自检让新旧共享同一 `Arc`；而 Task 1-6 的**分配器差分用两张独立
//! `MemDisk`**（各自 `from_image` 同一镜像字节、互不污染），跑同一序列后用
//! [`snapshot_meta`] / [`assert_meta_eq`] 逐字节对拍元数据区。两盘设计更强：两侧不可能交叉污染。
//!
//! 注意：core 生产代码只依赖 [`BlockReader`] / [`MetadataWriter`]，对 `ext4_rs`
//! 的桥接**只**出现在本 `#[cfg(ktest)]` 模块里（满足新旧解耦约束）。

use super::io::{BlockReader, BlockWriter};
use super::metadata_writer::MetadataWriter;
use super::prelude::*;
use super::superblock::RawSuperblock;

/// 共享可写内存盘：底层 `Arc<Mutex<Vec<u8>>>`，按字节偏移寻址。
///
/// `Clone` 共享同一 `Arc`——新旧引擎拿到的克隆指向同一份字节。
pub(super) struct MemDisk {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl MemDisk {
    /// 把 `image` 的字节整盘拷进新内存盘。
    pub(super) fn from_image(image: &[u8]) -> Self {
        Self {
            bytes: Arc::new(Mutex::new(image.to_vec())),
        }
    }

    /// 取底层共享字节缓冲（克隆 `Arc`，与本盘指向同一份字节）。
    /// 供后续 Task 1-6 的差分用例把同一份字节同时交给新旧引擎；本 Task 尚无消费者。
    #[allow(dead_code)]
    pub(super) fn backing(&self) -> Arc<Mutex<Vec<u8>>> {
        self.bytes.clone()
    }

    /// 把 `[off, off + len)` 读进 `dst`；越过盘尾的部分填 0。内部统一走锁。
    fn read_into(&self, off: usize, dst: &mut [u8]) {
        if dst.is_empty() {
            return;
        }
        let guard = self.bytes.lock();
        let avail = guard.len().saturating_sub(off);
        let copy = core::cmp::min(avail, dst.len());
        if copy > 0 {
            dst[..copy].copy_from_slice(&guard[off..off + copy]);
        }
        if copy < dst.len() {
            dst[copy..].fill(0);
        }
    }

    /// 把 `data` 写到 `[off, off + data.len())`；越过盘尾的部分丢弃。内部统一走锁。
    fn write_from(&self, off: usize, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let mut guard = self.bytes.lock();
        let avail = guard.len().saturating_sub(off);
        let copy = core::cmp::min(avail, data.len());
        if copy > 0 {
            guard[off..off + copy].copy_from_slice(&data[..copy]);
        }
    }
}

impl Clone for MemDisk {
    fn clone(&self) -> Self {
        Self {
            bytes: self.bytes.clone(),
        }
    }
}

impl ext4_rs::BlockDevice for MemDisk {
    /// 返回从 `offset` 起的一个块（`ext4_rs::BLOCK_SIZE` 字节），盘尾外填 0。
    /// 与集成层 `KernelBlockDeviceAdapter::read_offset` 语义一致。
    fn read_offset(&self, offset: usize) -> Vec<u8> {
        let mut data = vec![0u8; ext4_rs::BLOCK_SIZE];
        self.read_into(offset, data.as_mut_slice());
        data
    }

    fn read_offset_into(&self, offset: usize, out: &mut [u8]) {
        self.read_into(offset, out);
    }

    fn write_offset(&self, offset: usize, data: &[u8]) {
        self.write_from(offset, data);
    }

    // `BlockDevice::sync` 的返回类型用 ext4_rs 内部的 `Result` 别名（私有），
    // 这里写出其等价的具体类型 `core::result::Result<(), ext4_rs::Ext4Error>`。
    fn sync(&self) -> core::result::Result<(), ext4_rs::Ext4Error> {
        Ok(())
    }
}

impl BlockReader for MemDisk {
    fn read_at(&self, off: usize, out: &mut [u8]) {
        self.read_into(off, out);
    }
}

impl BlockWriter for MemDisk {
    /// 写**数据块**（按字节偏移）——与 ext4_rs `write_at` 里 `block_device.write_offset` 等价，
    /// 写穿同一份共享字节（差分两侧同盘对拍）。
    fn write_at(&self, off: usize, data: &[u8]) {
        self.write_from(off, data);
    }
}

/// 全盘逐字节比对：锁两盘底层字节，断言每字节相等；首个差异报盘内 offset + 两侧值。
///
/// Task 3 用它作主检查（B）——extent 树非根块落在数据区，`snapshot_meta_with_inodes`
/// 不覆盖；全盘比对一次性覆盖 inode 表 + extent 块 + 数据块 + 位图 + GDT + SB。
pub(super) fn assert_disk_eq(a: &MemDisk, b: &MemDisk) {
    let ga = a.bytes.lock();
    let gb = b.bytes.lock();
    assert_eq!(
        ga.len(),
        gb.len(),
        "disk length differs ({} vs {})",
        ga.len(),
        gb.len()
    );
    for (i, (x, y)) in ga.iter().zip(gb.iter()).enumerate() {
        if x != y {
            panic!(
                "full-disk mismatch at offset {}: {:#04x} != {:#04x}",
                i, x, y
            );
        }
    }
}

/// 直写元数据写回：实现 core 本地 [`MetadataWriter`]，把全块镜像**立即**写穿
/// 同一份共享字节（无 JBD2 延迟）。`block` 是物理块号，按 `block_size` 折算成字节偏移。
pub(super) struct DirectMetadataWriter {
    disk: MemDisk,
    block_size: usize,
}

impl DirectMetadataWriter {
    pub(super) fn new(disk: MemDisk, block_size: usize) -> Self {
        Self { disk, block_size }
    }
}

impl MetadataWriter for DirectMetadataWriter {
    fn write_metadata_for_handle(
        &self,
        _handle_id: u64,
        block: Ext4Fsblk,
        data: &[u8],
    ) -> Result<()> {
        let off = (block as usize) * self.block_size;
        self.disk.write_from(off, data);
        Ok(())
    }
}

/// 元数据区的一段截取：盘内字节偏移 + 该段字节。
struct MetaRegion {
    disk_off: usize,
    bytes: Vec<u8>,
}

/// 一次「元数据区」的快照：超级块计数/csum 区 + 全部组描述符表 + 每组的块/inode 位图块
/// + 每组的 inode 表整区（Task 0 起追加）。
pub(super) struct MetaSnapshot {
    regions: Vec<MetaRegion>,
}

/// 截取某组的 inode 表整区：`inode_table()*bs` 起、`inodes_per_group*inode_size` 字节。
///
/// 定位辅助（便于报「哪个 inode 字节差」）：组 `group` 的 inode 表首块由该组描述符的
/// `inode_table()` 给出；整区长度 = `inodes_per_group * inode_size`。区段结构对两张同
/// 布局盘一致。本函数与 [`snapshot_meta`] 追加 inode 表区用同一定位逻辑。
pub(super) fn snapshot_inode_table_group(
    disk: &MemDisk,
    sb: &RawSuperblock,
    group: u32,
) -> MetaRegion {
    use super::block_group::RawGroupDescriptor;

    let bs = sb.block_size();
    let desc_size = sb.group_desc_size();
    let inode_size = sb.inode_size() as usize;
    let inodes_per_group = sb.inodes_per_group() as usize;

    // 读该组描述符（GDT 紧跟超级块块），定位 inode 表首块。
    let gdt_off = (sb.first_data_block as usize + 1) * bs;
    let desc_off = gdt_off + (group as usize) * desc_size;
    let mut desc_buf = [0u8; 64];
    let take = core::cmp::min(desc_size, 64);
    let mut raw = vec![0u8; desc_size];
    disk.read_at(desc_off, raw.as_mut_slice());
    desc_buf[..take].copy_from_slice(&raw[..take]);
    let desc = RawGroupDescriptor::from_bytes(&desc_buf);

    let off = (desc.inode_table() as usize) * bs;
    let len = inodes_per_group * inode_size;
    let mut bytes = vec![0u8; len];
    disk.read_at(off, bytes.as_mut_slice());
    MetaRegion {
        disk_off: off,
        bytes,
    }
}

/// 从超级块推导并截取**分配器范围**的元数据区，得到一张可逐字节比对的快照。
///
/// 截取范围（仅分配器会动到的元数据——**不含 inode 表**）：
/// 1. **超级块区**：偏移 1024、长 1024——含 free_blocks/free_inodes 计数与 checksum；
/// 2. **组描述符表（GDT）**：偏移 `(first_data_block + 1) * block_size`，
///    长 `num_groups * group_desc_size`——含每组的 free 计数与位图 csum；
/// 3. **每组两张位图块**：`block_bitmap` / `inode_bitmap` 各一个块大小。
///
/// inode 表**刻意不在此处**：Phase-2 的新核分配器只动位图/计数，i_blocks 仅在内存累加、
/// **不写 inode 表**（见 `balloc.rs`），而旧 ext4_rs 会在分配路径写回 inode；若把 inode 表
/// 并进本快照，分配器差分会在这条已知、刻意的 Phase-2/3 边界上误报。inode 内容进盘的路径
/// （Phase 3 的 inode/extent/文件差分）改用 [`snapshot_meta_with_inodes`]。
///
/// `num_groups = ceil(blocks_count / blocks_per_group)`。位图块偏移逐组从对应组描述符读出
/// （兼容任意布局），故先截 GDT、再据之定位各位图。
pub(super) fn snapshot_meta(disk: &MemDisk, sb: &RawSuperblock) -> MetaSnapshot {
    use super::block_group::RawGroupDescriptor;

    let bs = sb.block_size();
    let desc_size = sb.group_desc_size();
    let blocks_per_group = sb.blocks_per_group as u64;
    let num_groups = if blocks_per_group == 0 {
        0
    } else {
        sb.blocks_count().div_ceil(blocks_per_group) as usize
    };

    let mut regions: Vec<MetaRegion> = Vec::new();

    // 1) 超级块区（计数 + csum）。
    let mut sb_bytes = vec![0u8; 1024];
    disk.read_at(1024, sb_bytes.as_mut_slice());
    regions.push(MetaRegion {
        disk_off: 1024,
        bytes: sb_bytes,
    });

    // 2) 组描述符表（全部组）。
    let gdt_off = (sb.first_data_block as usize + 1) * bs;
    let gdt_len = num_groups * desc_size;
    let mut gdt_bytes = vec![0u8; gdt_len];
    disk.read_at(gdt_off, gdt_bytes.as_mut_slice());

    // 3) 据每组描述符定位块/inode 位图块，各截一个块。
    //    描述符按 `desc_size` 间隔排布；`RawGroupDescriptor::from_bytes` 要求恰 64 字节，
    //    故把该条的 `desc_size` 字节拷进 64 字节零填充缓冲再解析——desc_size==32 时
    //    高 32 字节（含各 _hi 字段）保持 0，恰等于「64bit 特性关」的语义，且绝不跨读下一条。
    let mut bitmap_regions: Vec<MetaRegion> = Vec::new();
    for g in 0..num_groups {
        let mut desc_buf = [0u8; 64];
        let src = &gdt_bytes[g * desc_size..g * desc_size + desc_size];
        let take = core::cmp::min(desc_size, 64);
        desc_buf[..take].copy_from_slice(&src[..take]);
        let desc = RawGroupDescriptor::from_bytes(&desc_buf);
        for blk in [desc.block_bitmap(), desc.inode_bitmap()] {
            let off = (blk as usize) * bs;
            let mut buf = vec![0u8; bs];
            disk.read_at(off, buf.as_mut_slice());
            bitmap_regions.push(MetaRegion {
                disk_off: off,
                bytes: buf,
            });
        }
    }

    regions.push(MetaRegion {
        disk_off: gdt_off,
        bytes: gdt_bytes,
    });
    regions.extend(bitmap_regions);

    MetaSnapshot { regions }
}

/// 在 [`snapshot_meta`]（分配器范围：SB + GDT + 位图）之上，追加**每组 inode 表整区**
/// （`inode_table() * block_size` 起、`inodes_per_group * inode_size` 字节），供 inode /
/// extent / 文件路径差分逐字节对拍 inode 内容（含 inode csum）+ 定位「哪个 inode 字节差」。
///
/// 与 `snapshot_meta` 分开：Phase-2 分配器差分的新核**刻意不写 inode 表**，那些用例必须用
/// 窄的 `snapshot_meta`；inode 内容进盘的路径（Phase 3 起）才用本函数。区段结构对两张同布局
/// 盘一致（同一比对的两侧用同一函数即可）。
pub(super) fn snapshot_meta_with_inodes(disk: &MemDisk, sb: &RawSuperblock) -> MetaSnapshot {
    let blocks_per_group = sb.blocks_per_group as u64;
    let num_groups = if blocks_per_group == 0 {
        0
    } else {
        sb.blocks_count().div_ceil(blocks_per_group) as u32
    };
    let mut snap = snapshot_meta(disk, sb);
    for g in 0..num_groups {
        snap.regions.push(snapshot_inode_table_group(disk, sb, g));
    }
    snap
}

impl MetaSnapshot {
    /// 找首个字节差异，返回 `(盘内 offset, a 字节, b 字节)`；完全一致则 `None`。
    /// 两张快照的区段结构（数量、每段 `disk_off`/长度）必须一致——同一 `MemDisk`
    /// 布局下 `snapshot_meta` 必然如此；不一致即 panic（属测试用法错误）。
    fn first_diff(&self, other: &MetaSnapshot) -> Option<(usize, u8, u8)> {
        assert_eq!(
            self.regions.len(),
            other.regions.len(),
            "meta snapshot region count differs ({} vs {})",
            self.regions.len(),
            other.regions.len()
        );
        for (ra, rb) in self.regions.iter().zip(other.regions.iter()) {
            assert_eq!(
                ra.disk_off, rb.disk_off,
                "meta snapshot region offset differs ({} vs {})",
                ra.disk_off, rb.disk_off
            );
            assert_eq!(
                ra.bytes.len(),
                rb.bytes.len(),
                "meta snapshot region len differs at disk_off {}",
                ra.disk_off
            );
            for (i, (x, y)) in ra.bytes.iter().zip(rb.bytes.iter()).enumerate() {
                if x != y {
                    return Some((ra.disk_off + i, *x, *y));
                }
            }
        }
        None
    }
}

/// 逐字节比对两张快照；不等时 panic 并报**首个差异的盘内 offset + 两侧字节值**。
pub(super) fn assert_meta_eq(a: &MetaSnapshot, b: &MetaSnapshot) {
    if let Some((off, x, y)) = a.first_diff(b) {
        panic!(
            "meta snapshot mismatch at disk offset {}: {:#04x} != {:#04x}",
            off, x, y
        );
    }
}

#[cfg(ktest)]
mod test {
    use alloc::format;

    use ostd::prelude::*;

    use super::{
        assert_disk_eq, assert_meta_eq, snapshot_inode_table_group, snapshot_meta,
        snapshot_meta_with_inodes, DirectMetadataWriter, MemDisk,
    };
    use crate::fs::ext4::core::block_group::RawGroupDescriptor;
    use crate::fs::ext4::core::inode::{inode_checksum, RawInode};
    use crate::fs::ext4::core::io::BlockReader;
    use crate::fs::ext4::core::metadata_writer::MetadataWriter;
    use crate::fs::ext4::core::superblock::RawSuperblock;
    use crate::fs::ext4::core::test_util::EXT4_IMAGE;
    use crate::fs::ext4::core::types::Ext4Fsblk;
    use crate::prelude::*;

    // =================================================================
    // Phase 4 Task 0：目录操作差分地基。
    //
    // 提供：
    // - `DirOp` + `build_dir_populated_image`：用 **ext4_rs** 在 EXT4_IMAGE 克隆盘上跑
    //   一串 mkdir/create，把结果字节交回——后续 dir 差分两盘从同一份「已带目录项」字节起步。
    // - 旧侧（ext4_rs）只读采集 helper：`old_readdir` / `old_lookup`，供 `diff_readdir` /
    //   `diff_lookup` 的旧半部立刻可用。
    // - `diff_readdir` / `diff_lookup` 骨架：旧半部（ext4_rs）现成；新半部（core）由 Task 1
    //   以闭包注入——故 Task 0 标 `#[allow(dead_code)]`、Task 1 接上去掉。
    // =================================================================

    /// 一条建目录树的操作：在 `parent` 下以 `mode` 建子目录 / 子文件。
    /// `mode` 含类型位（mkdir 用 `0o40000|perm`、create 用 `0o100000|perm`）。
    /// 仅 `diff_harness.rs` 的 ktest 模块内消费（Task 1+ 的差分用例同在此模块）。
    #[allow(dead_code)]
    enum DirOp {
        Mkdir {
            parent: u32,
            name: &'static str,
            mode: u16,
        },
        Create {
            parent: u32,
            name: &'static str,
            mode: u16,
        },
    }

    /// 从 `EXT4_IMAGE` 克隆一张内存盘，用 **ext4_rs** 在其上顺序执行 `ops`
    /// （`Mkdir` → `ext4_mkdir_at`，`Create` → `ext4_create_at`），返回结果整盘字节。
    ///
    /// 供后续 dir 差分用例：两侧各 `MemDisk::from_image(&bytes)` 从同一份「已带目录项」
    /// 字节起步，再各跑新旧引擎对拍。任一 op 失败即 panic（builder 用于构造良性 fixture，
    /// 失败属测试用法错误）。
    #[allow(dead_code)]
    fn build_dir_populated_image(ops: &[DirOp]) -> Vec<u8> {
        let disk = MemDisk::from_image(EXT4_IMAGE);
        let ext4 = ext4_rs::Ext4::open(Arc::new(disk.clone()));
        for op in ops {
            match *op {
                DirOp::Mkdir {
                    parent,
                    name,
                    mode,
                } => {
                    ext4.ext4_mkdir_at(parent, name, mode)
                        .unwrap_or_else(|e| panic!("builder mkdir '{name}' failed: {e:?}"));
                }
                DirOp::Create {
                    parent,
                    name,
                    mode,
                } => {
                    ext4.ext4_create_at(parent, name, mode)
                        .unwrap_or_else(|e| panic!("builder create '{name}' failed: {e:?}"));
                }
            }
        }
        disk.backing().lock().clone()
    }

    /// 旧侧（ext4_rs）readdir 采集：在 `disk` 上 `Ext4::open`，读 `dir_ino` 的全部目录项，
    /// 返回 `(name, inode, file_type)` 向量（丢弃 offset；带 next_offset 的对拍用
    /// [`old_readdir_with_next_offset`]）。
    fn old_readdir(disk: &MemDisk, dir_ino: u32) -> Vec<(String, u32, u8)> {
        let ext4 = ext4_rs::Ext4::open(Arc::new(disk.clone()));
        ext4.ext4_readdir_with_offsets(dir_ino)
            .into_iter()
            .map(|(name, ino, _off, ftype)| (name, ino, ftype))
            .collect()
    }

    /// 旧侧（ext4_rs）readdir + **next_offset** 采集：调 `dir_get_entries_with_next_offset`
    /// （`next_offset = iblock*bs + off + rec_len`），逐项映射成 `(name, inode, file_type,
    /// next_offset)`。core `dir_get_entries_with_next_offset` 的逐字节对拍基准。
    /// 注意 ext4_rs 的 `ext4_readdir_with_offsets` 返回的是**项自身**偏移，语义不同；
    /// 这里用底层 `dir_get_entries_with_next_offset`（与 core 同义）才能对 next_offset。
    fn old_readdir_with_next_offset(disk: &MemDisk, dir_ino: u32) -> Vec<(String, u32, u8, usize)> {
        let ext4 = ext4_rs::Ext4::open(Arc::new(disk.clone()));
        ext4.dir_get_entries_with_next_offset(dir_ino)
            .into_iter()
            .map(|(de, next_off)| (de.get_name(), de.inode, de.get_de_type(), next_off))
            .collect()
    }

    /// 旧侧（ext4_rs）lookup 采集：在 `disk` 上 `Ext4::open`，在 `parent` 下查 `name`，
    /// 命中返回 `Ok(inode)`，未命中/出错返回 `Err(ext4_rs::Errno)`（供 `match` 对拍错误码）。
    fn old_lookup(disk: &MemDisk, parent: u32, name: &str) -> core::result::Result<u32, ext4_rs::Errno> {
        let ext4 = ext4_rs::Ext4::open(Arc::new(disk.clone()));
        ext4.ext4_lookup_at(parent, name).map_err(|e| e.error())
    }

    /// 新侧（core）readdir + next_offset 采集：在 `disk` 上建 `ReadCtx`、`load_inode(dir_ino)`，
    /// 调 core `dir_get_entries_with_next_offset`，映射成 `(name, inode, file_type, next_offset)`。
    /// name 经 `String::from_utf8_lossy`（与旧侧 `get_name` 一致）。
    fn core_readdir_next_offset(disk: &MemDisk, dir_ino: u32) -> Vec<(String, u32, u8, usize)> {
        use crate::fs::ext4::core::dir::dir_get_entries_with_next_offset;
        use crate::fs::ext4::core::file::ReadCtx;
        use crate::fs::ext4::core::inode::load_inode;

        let sb = read_sb(disk);
        let ctx = ReadCtx::new(disk, &sb);
        let dir = load_inode(disk, &sb, dir_ino).expect("core load_inode(dir)");
        dir_get_entries_with_next_offset(&ctx, &dir)
            .into_iter()
            .map(|(e, next_off)| {
                (
                    String::from_utf8_lossy(&e.name).into_owned(),
                    e.inode,
                    e.file_type,
                    next_off,
                )
            })
            .collect()
    }

    /// 新侧（core）lookup：建 `ReadCtx`、`load_inode(parent)`，调 core `dir_find_entry`。
    /// 命中→`Ok(inode)`；`Ok(None)`（未命中）→映射 `ext4_rs::Errno::ENOENT`（对拍旧侧）；
    /// core 其它错误（如 EIO）→透传对应 `ext4_rs::Errno`（按 errno 数值映射）。
    fn core_lookup(
        disk: &MemDisk,
        parent: u32,
        name: &str,
    ) -> core::result::Result<u32, ext4_rs::Errno> {
        use crate::fs::ext4::core::dir::dir_find_entry;
        use crate::fs::ext4::core::file::ReadCtx;
        use crate::fs::ext4::core::inode::load_inode;

        let sb = read_sb(disk);
        let ctx = ReadCtx::new(disk, &sb);
        let parent_inode = match load_inode(disk, &sb, parent) {
            Ok(i) => i,
            Err(_) => return Err(ext4_rs::Errno::ENOENT),
        };
        match dir_find_entry(&ctx, &parent_inode, name.as_bytes()) {
            Ok(Some(hit)) => Ok(hit.inode),
            Ok(None) => Err(ext4_rs::Errno::ENOENT),
            Err(e) => Err(map_core_errno(e.error())),
        }
    }

    /// 把 core 侧 `Errno`（kernel）按数值映射到 `ext4_rs::Errno`（仅覆盖目录读路径可能出现的）。
    fn map_core_errno(e: crate::prelude::Errno) -> ext4_rs::Errno {
        use crate::prelude::Errno as K;
        match e {
            K::ENOENT => ext4_rs::Errno::ENOENT,
            K::ENOTDIR => ext4_rs::Errno::ENOTDIR,
            K::EIO => ext4_rs::Errno::EIO,
            _ => ext4_rs::Errno::EINVAL,
        }
    }

    /// readdir 差分：两张独立内存盘（同初始 `image` 字节），旧侧用
    /// `old_readdir_with_next_offset`、新侧由 `core_readdir` 闭包注入（Task 1 接 core
    /// `dir_get_entries_with_next_offset`）。两侧 `(name, inode, file_type, next_offset)`
    /// 向量逐元素相等 + 全盘字节相等（只读，故应不变）。
    fn diff_readdir(
        image: &[u8],
        dir_ino: u32,
        core_readdir: impl FnOnce(&MemDisk, u32) -> Vec<(String, u32, u8, usize)>,
    ) {
        let old_disk = MemDisk::from_image(image);
        let new_disk = MemDisk::from_image(image);
        let old = old_readdir_with_next_offset(&old_disk, dir_ino);
        let new = core_readdir(&new_disk, dir_ino);
        assert_eq!(
            old, new,
            "readdir(dir={dir_ino}) (name,ino,type,next_offset) vector mismatch"
        );
        // 只读操作：两盘字节应保持与初始镜像一致。
        assert_disk_eq(&old_disk, &new_disk);
    }

    /// lookup 差分：两张独立内存盘（同初始 `image` 字节），旧侧用 `old_lookup`、新侧由
    /// `core_lookup` 闭包注入（Task 1 接 core `dir_find_entry`，命中→Ok(inode)、未命中→
    /// 映射 `ext4_rs::Errno::ENOENT`）。两侧成功/失败 + inode 号对齐（`match` 不 expect），
    /// 再全盘字节对拍。
    fn diff_lookup(
        image: &[u8],
        parent: u32,
        name: &str,
        core_lookup: impl FnOnce(&MemDisk, u32, &str) -> core::result::Result<u32, ext4_rs::Errno>,
    ) {
        let old_disk = MemDisk::from_image(image);
        let new_disk = MemDisk::from_image(image);
        let old = old_lookup(&old_disk, parent, name);
        let new = core_lookup(&new_disk, parent, name);
        match (old, new) {
            (Ok(o), Ok(n)) => assert_eq!(o, n, "lookup(parent={parent},name='{name}') inode mismatch"),
            (Err(o), Err(n)) => assert_eq!(
                o, n,
                "lookup(parent={parent},name='{name}') errno mismatch: old {o:?} new {n:?}"
            ),
            (o, n) => panic!(
                "lookup(parent={parent},name='{name}') success/failure divergence: old={o:?} new={n:?}"
            ),
        }
        // 只读操作：两盘字节应保持一致。
        assert_disk_eq(&old_disk, &new_disk);
    }

    /// 从内存盘按超级块布局读出 inode `ino` 的 156 字节原始字节。
    /// 组 = (ino-1)/inodes_per_group；组内序号 = (ino-1)%inodes_per_group。
    fn read_inode_bytes(disk: &MemDisk, sb: &RawSuperblock, ino: u32) -> Vec<u8> {
        let bs = sb.block_size();
        let inode_size = sb.inode_size() as usize;
        let inodes_per_group = sb.inodes_per_group();
        let group = (ino - 1) / inodes_per_group;
        let index = ((ino - 1) % inodes_per_group) as usize;

        let gdt_off = (sb.first_data_block as usize + 1) * bs;
        let desc_size = sb.group_desc_size();
        let mut desc_buf = [0u8; 64];
        let take = core::cmp::min(desc_size, 64);
        let mut raw = vec![0u8; desc_size];
        disk.read_at(gdt_off + group as usize * desc_size, raw.as_mut_slice());
        desc_buf[..take].copy_from_slice(&raw[..take]);
        let desc = RawGroupDescriptor::from_bytes(&desc_buf);

        let off = (desc.inode_table() as usize) * bs + index * inode_size;
        let n = size_of::<RawInode>();
        let mut out = vec![0u8; n];
        disk.read_at(off, out.as_mut_slice());
        out
    }

    /// 读超级块（布局推导用）。
    fn read_sb(disk: &MemDisk) -> RawSuperblock {
        let mut sb_buf = vec![0u8; 1024];
        disk.read_at(1024, sb_buf.as_mut_slice());
        RawSuperblock::from_bytes(&sb_buf)
    }

    /// 往返一致 + 负向能抓差异：证明这张共享内存盘 + snapshot 框架可靠。
    #[ktest]
    fn diff_harness_roundtrip() {
        let disk = MemDisk::from_image(EXT4_IMAGE);

        // 从超级块推导布局（偏移不写死，兼容任意 block_size）。
        let mut sb_buf = vec![0u8; 1024];
        disk.read_at(1024, sb_buf.as_mut_slice());
        let sb = RawSuperblock::from_bytes(&sb_buf);
        assert_eq!(sb.magic(), 0xEF53, "ext4 magic via MemDisk read");
        let bs = sb.block_size();

        // 经 BlockReader 读出第 0 组描述符的字节（GDT 紧跟超级块块）。
        let gdt_off = (sb.first_data_block as usize + 1) * bs;
        let n = size_of::<RawGroupDescriptor>();
        let mut gd0 = vec![0u8; n];
        disk.read_at(gdt_off, gd0.as_mut_slice());
        let desc0 = RawGroupDescriptor::from_bytes(&gd0);
        assert_eq!(desc0.as_bytes(), &gd0[..], "group-0 descriptor round-trip via BlockReader");

        // 直写 MetadataWriter：把同样的字节原样写回同一 offset（块号 = gdt_off / bs）。
        let writer = DirectMetadataWriter::new(disk.clone(), bs);
        let gdt_block = (gdt_off / bs) as u64;
        // gd0 是组描述符表所在块的开头 n 字节；写回需整块镜像，这里读出整块再写整块。
        let mut gdt_full = vec![0u8; bs];
        disk.read_at(gdt_block as usize * bs, gdt_full.as_mut_slice());
        // 整块开头应与单独读出的第 0 描述符一致（同一份共享字节）。
        assert_eq!(&gdt_full[..n], &gd0[..], "GDT block prefix == group-0 descriptor");

        // 正向：原样写回，快照前后必一致。
        let before = snapshot_meta(&disk, &sb);
        writer
            .write_metadata_for_handle(0, gdt_block, &gdt_full)
            .expect("direct metadata write");
        let after = snapshot_meta(&disk, &sb);
        assert_meta_eq(&before, &after);

        // 负向：改组描述符表块的第 0 字节再写回整块，快照必不同（证明 snapshot 真能抓差异）。
        let mut mutated = gdt_full.clone();
        mutated[0] ^= 0xFF;
        writer
            .write_metadata_for_handle(0, gdt_block, &mutated)
            .expect("direct metadata write (mutated)");
        let after_mut = snapshot_meta(&disk, &sb);

        // 必能定位到首个差异，且恰在被改字节的盘内偏移（block 0 字节 → gdt_off）。
        let diff = before.first_diff(&after_mut);
        let (off, x, y) = diff.expect("snapshot must catch a 1-byte change written back");
        assert_eq!(off, gdt_off, "first diff offset = mutated byte offset");
        assert_eq!(y ^ x, 0xFF, "mutated byte differs by the flipped bits");

        // 复原（写回原始整块），确认快照又能回到一致——闭环验证。
        writer
            .write_metadata_for_handle(0, gdt_block, &gdt_full)
            .expect("direct metadata write (restore)");
        let restored = snapshot_meta(&disk, &sb);
        assert_meta_eq(&before, &restored);
    }

    /// inode 元数据 csum 与 ext4_rs 逐字节对拍：对多个真实 inode（#2 根、#11 lost+found），
    /// 新 `inode_checksum`（纯函数）== 旧 `Ext4Inode::get_inode_checksum`。
    #[ktest]
    fn inode_csum_parity() {
        let disk = MemDisk::from_image(EXT4_IMAGE);
        let sb = read_sb(&disk);

        // 旧侧引擎：经 ext4_rs::Ext4::open 取 inode 引用，调其 get_inode_checksum。
        let ext4 = ext4_rs::Ext4::open(Arc::new(disk.clone()));

        for &ino in &[2u32, 11] {
            // 新侧：从盘读 156 字节 → RawInode → 纯函数 inode_checksum（不改 raw）。
            let bytes = read_inode_bytes(&disk, &sb, ino);
            let raw = RawInode::from_bytes(&bytes);
            let new_csum = inode_checksum(&raw, ino, &sb);

            // 旧侧：克隆 Ext4Inode（get_inode_checksum 取 &mut self，会改本地拷贝），算 csum。
            let mut old_inode = ext4.get_inode_ref(ino).inode;
            let old_sb = ext4_rs::Ext4Superblock::from_bytes(&{
                let mut b = vec![0u8; 1024];
                disk.read_at(1024, b.as_mut_slice());
                b
            });
            let old_csum = old_inode.get_inode_checksum(ino, &old_sb);

            assert_eq!(
                new_csum, old_csum,
                "inode #{ino} csum mismatch: new {new_csum:#010x} != old {old_csum:#010x}"
            );
        }
    }

    /// `snapshot_meta_with_inodes` 覆盖 inode 表，能定位 inode 内单字节改动（负向测试）。
    /// 经 `DirectMetadataWriter` 直写 inode 表所在块的一字节（与 GDT 负向测试同手法），
    /// 快照前后必不同，且首差 offset 恰在被改字节；复原后又回到一致。
    #[ktest]
    fn inode_table_snapshot_catches_inode_byte() {
        let disk = MemDisk::from_image(EXT4_IMAGE);
        let sb = read_sb(&disk);
        let bs = sb.block_size();

        // 定位根 inode（#2）所在块与块内偏移。
        let gdt_off = (sb.first_data_block as usize + 1) * bs;
        let desc = RawGroupDescriptor::from_bytes(&{
            let mut b = vec![0u8; 64];
            disk.read_at(gdt_off, b.as_mut_slice());
            b
        });
        let inode_size = sb.inode_size() as usize;
        let itable = desc.inode_table() as usize;
        let ino_off = itable * bs + (2 - 1) * inode_size; // inode #2 字节偏移
        let block = (ino_off / bs) as Ext4Fsblk;
        let off_in_block = ino_off % bs;

        let before = snapshot_meta_with_inodes(&disk, &sb);

        // 读出 inode 表所在整块，翻转 inode #2 第 0 字节再整块写回。
        let writer = DirectMetadataWriter::new(disk.clone(), bs);
        let mut full = vec![0u8; bs];
        disk.read_at(block as usize * bs, full.as_mut_slice());
        let orig = full.clone();
        full[off_in_block] ^= 0xFF;
        writer
            .write_metadata_for_handle(0, block, &full)
            .expect("direct metadata write (mutated inode byte)");
        let after = snapshot_meta_with_inodes(&disk, &sb);

        // 负向：snapshot 必能抓到，且首差 offset 恰是被改字节。
        let (off, x, y) = before
            .first_diff(&after)
            .expect("snapshot must catch a 1-byte change inside an inode");
        assert_eq!(off, ino_off, "first diff offset = mutated inode byte offset");
        assert_eq!(y ^ x, 0xFF, "mutated byte differs by the flipped bits");

        // 辅助定位：snapshot_inode_table_group 截的整区里也含该改动。
        let region = snapshot_inode_table_group(&disk, &sb, 0);
        assert_eq!(region.disk_off, itable * bs, "inode-table region offset");
        assert_ne!(
            region.bytes[ino_off - region.disk_off], orig[off_in_block],
            "inode-table region reflects the mutated byte"
        );

        // 复原 → 快照又一致（闭环）。
        writer
            .write_metadata_for_handle(0, block, &orig)
            .expect("direct metadata write (restore inode block)");
        let restored = snapshot_meta_with_inodes(&disk, &sb);
        assert_meta_eq(&before, &restored);
    }

    /// 最小文件映射差分样例：两张独立 `MemDisk`（同镜像），两侧各 load 同一文件 inode（根 #2）
    /// 并经 `snapshot_meta_with_inodes` 对拍 inode 表一致；再证该 snapshot 能抓 inode 内单字节改动。
    ///
    /// 注：`map_blocks`（extent 映射）是 Task 2，本样例不实现；只验证 (a)
    /// `snapshot_meta_with_inodes` 含 inode 表且新旧一致，(b) 能抓 inode 内单字节改动（负向）。
    #[ktest]
    fn file_map_root_inode_parity() {
        // 两张独立内存盘（各自 from_image 同字节，互不共享 Arc）。
        let old_disk = MemDisk::from_image(EXT4_IMAGE);
        let new_disk = MemDisk::from_image(EXT4_IMAGE);
        let sb = read_sb(&new_disk);

        // 旧侧：经 ext4_rs::Ext4::open 读根 inode 字节；新侧：直接读同 inode 字节。
        // 两者应是同一份磁盘字节（同镜像），故两张盘的 inode 表逐字节相同。
        let ext4 = ext4_rs::Ext4::open(Arc::new(old_disk.clone()));
        let old_root = ext4.get_inode_ref(2).inode;
        let new_root_bytes = read_inode_bytes(&new_disk, &sb, 2);
        let new_root = RawInode::from_bytes(&new_root_bytes);
        // 新侧解析的根 inode 与旧侧关键字段一致（确认两侧读的是同一 inode）。
        assert!(new_root.is_dir(), "root inode is a directory");
        assert_eq!(new_root.size as u64 | ((new_root.size_hi as u64) << 32), {
            (old_root.size as u64) | ((old_root.size_hi as u64) << 32)
        }, "root inode size new == old");

        // (a) inode 包含版 snapshot 含 inode 表，两张同布局盘逐字节一致。
        let old_snap = snapshot_meta_with_inodes(&old_disk, &sb);
        let new_snap = snapshot_meta_with_inodes(&new_disk, &sb);
        assert_meta_eq(&old_snap, &new_snap);

        // (b) 在新侧用 DirectMetadataWriter 直写根 inode 一字节，snapshot 必不同。
        let bs = sb.block_size();
        let gdt_off = (sb.first_data_block as usize + 1) * bs;
        let desc = RawGroupDescriptor::from_bytes(&{
            let mut b = vec![0u8; 64];
            new_disk.read_at(gdt_off, b.as_mut_slice());
            b
        });
        let inode_size = sb.inode_size() as usize;
        let ino_off = desc.inode_table() as usize * bs + (2 - 1) * inode_size;
        let block = (ino_off / bs) as Ext4Fsblk;
        let off_in_block = ino_off % bs;

        let writer = DirectMetadataWriter::new(new_disk.clone(), bs);
        let mut full = vec![0u8; bs];
        new_disk.read_at(block as usize * bs, full.as_mut_slice());
        full[off_in_block] ^= 0xFF;
        writer
            .write_metadata_for_handle(0, block, &full)
            .expect("direct metadata write (mutated root inode)");
        let new_snap_mut = snapshot_meta_with_inodes(&new_disk, &sb);

        let (off, x, y) = old_snap
            .first_diff(&new_snap_mut)
            .expect("extended snapshot must catch a 1-byte inode change across two disks");
        assert_eq!(off, ino_off, "first diff offset = mutated inode byte offset");
        assert_eq!(y ^ x, 0xFF, "mutated byte differs by the flipped bits");
    }

    /// 旧侧 readdir helper 在真实根目录上的基线：`old_readdir(2)` 必含 "." 与 ".."。
    /// 证明 `old_readdir`（`diff_readdir` 的旧半部）对真镜像可用、采得 `(name,ino,type)`。
    #[ktest]
    fn dir_old_readdir_root_baseline() {
        let disk = MemDisk::from_image(EXT4_IMAGE);
        let entries = old_readdir(&disk, 2);
        // '.' / '..' 必在；'.' 指向根自身 (ino 2)、类型为目录 (2)。
        assert!(
            entries.iter().any(|(n, ino, ft)| n == "." && *ino == 2 && *ft == 2),
            "root readdir must contain '.' -> ino 2, type dir; got {entries:?}"
        );
        assert!(
            entries.iter().any(|(n, _ino, ft)| n == ".." && *ft == 2),
            "root readdir must contain '..' (dir); got {entries:?}"
        );
        // 真镜像根目录预置 lost+found（ino 11）。
        assert!(
            entries.iter().any(|(n, ino, _ft)| n == "lost+found" && *ino == 11),
            "root readdir must contain 'lost+found' -> ino 11; got {entries:?}"
        );
    }

    /// builder 往返：用 ext4_rs 在克隆盘上 mkdir "d1" + create "f1"，重开结果字节，
    /// 确认根目录 readdir 现含 "d1"（目录）与 "f1"（文件），且 "." / "lost+found" 基线仍在。
    /// 证明 `build_dir_populated_image` 产出一张可用的「已带目录项」盘。
    #[ktest]
    fn dir_builder_roundtrip() {
        let bytes = build_dir_populated_image(&[
            DirOp::Mkdir {
                parent: 2,
                name: "d1",
                mode: 0o40755,
            },
            DirOp::Create {
                parent: 2,
                name: "f1",
                mode: 0o100644,
            },
        ]);
        let disk = MemDisk::from_image(&bytes);
        let entries = old_readdir(&disk, 2);

        // 新建目录 "d1"：file_type == 2 (dir)。
        let d1 = entries
            .iter()
            .find(|(n, _, _)| n == "d1")
            .unwrap_or_else(|| panic!("builder result must contain 'd1'; got {entries:?}"));
        assert_eq!(d1.2, 2, "'d1' must be a directory entry (file_type 2)");
        assert!(d1.1 >= 12, "'d1' inode {} should be a freshly allocated inode", d1.1);

        // 新建文件 "f1"：file_type == 1 (regular)。
        let f1 = entries
            .iter()
            .find(|(n, _, _)| n == "f1")
            .unwrap_or_else(|| panic!("builder result must contain 'f1'; got {entries:?}"));
        assert_eq!(f1.2, 1, "'f1' must be a regular-file entry (file_type 1)");

        // 基线项仍在：'.' (ino 2) 与 lost+found (ino 11)。
        assert!(
            entries.iter().any(|(n, ino, _)| n == "." && *ino == 2),
            "builder result must still contain '.' -> ino 2; got {entries:?}"
        );
        assert!(
            entries.iter().any(|(n, ino, _)| n == "lost+found" && *ino == 11),
            "builder result must still contain 'lost+found'; got {entries:?}"
        );

        // lookup helper 与 readdir 一致：在 'd1'(目录) 下查 '.' 命中其自身 inode。
        let d1_ino = d1.1;
        let sub = old_lookup(&disk, d1_ino, ".");
        assert_eq!(sub, Ok(d1_ino), "lookup '.' inside 'd1' must resolve to d1's inode");
    }

    // =================================================================
    // Phase 4 Task 1：目录读差分（接上 Task 0 的 diff_readdir / diff_lookup 新侧）。
    // =================================================================

    /// 用 ext4_rs 在 `EXT4_IMAGE` 克隆盘上建一个含**多个目录块**的子目录：先 `mkdir "big"`，
    /// 再在其下 create `n_files` 个文件（格式化短名）。返回结果整盘字节 + 'big' 的 inode 号。
    /// 文件多到逼新建第 2 个目录块（4K 块每块约容 200+ 短名项）。
    fn build_multiblock_dir_image(n_files: usize) -> (Vec<u8>, u32) {
        let disk = MemDisk::from_image(EXT4_IMAGE);
        let ext4 = ext4_rs::Ext4::open(Arc::new(disk.clone()));
        ext4.ext4_mkdir_at(2, "big", 0o40755)
            .unwrap_or_else(|e| panic!("mkdir 'big' failed: {e:?}"));
        let big_ino = ext4
            .ext4_lookup_at(2, "big")
            .unwrap_or_else(|e| panic!("lookup 'big' failed: {e:?}"));
        for i in 0..n_files {
            let name = format!("file_{i:05}");
            ext4.ext4_create_at(big_ino, &name, 0o100644)
                .unwrap_or_else(|e| panic!("create '{name}' failed: {e:?}"));
        }
        (disk.backing().lock().clone(), big_ino)
    }

    /// readdir 根目录差分：core `dir_get_entries_with_next_offset(2)` 与 ext4_rs
    /// `dir_get_entries_with_next_offset(2)` 逐元素 `(name, ino, type, next_offset)` 相等。
    #[ktest]
    fn dir_readdir_root_parity() {
        diff_readdir(EXT4_IMAGE, 2, core_readdir_next_offset);
    }

    /// readdir 多块目录差分：建一个 ≥2 个目录块的子目录（含 '.'/'..'+大量文件），
    /// core vs ext4_rs 逐元素 `(name, ino, type, next_offset)` 相等——覆盖跨块、按 rec_len 走项、
    /// next_offset 含 `iblock*bs` 分量。先确认确实跨块（项数远超单块容量）。
    #[ktest]
    fn dir_readdir_multiblock_parity() {
        let (bytes, big_ino) = build_multiblock_dir_image(300);
        // 确认跨块：旧侧 readdir 项数应远超单块所能容纳（佐证 next_offset 含跨块分量）。
        let probe = old_readdir(&MemDisk::from_image(&bytes), big_ino);
        assert!(
            probe.len() > 200,
            "multiblock dir should hold >200 entries to force a 2nd block; got {}",
            probe.len()
        );
        // 跨块的体现：至少一项的 next_offset >= 一个块大小。
        let sb = read_sb(&MemDisk::from_image(&bytes));
        let bs = sb.block_size();
        let with_off = old_readdir_with_next_offset(&MemDisk::from_image(&bytes), big_ino);
        assert!(
            with_off.iter().any(|(_, _, _, off)| *off >= bs),
            "at least one entry's next_offset must land in block >=1"
        );
        diff_readdir(&bytes, big_ino, core_readdir_next_offset);
    }

    /// lookup 差分：命中（根下 'lost+found' → ino 11）/ 未命中（ENOENT）/ 子目录内（'big' 下
    /// 一个已建文件命中）。全经 `match` Ok/Err 对拍（不 expect）。
    #[ktest]
    fn dir_lookup_parity() {
        // 命中：根目录下 'lost+found'。
        diff_lookup(EXT4_IMAGE, 2, "lost+found", core_lookup);
        // 未命中：根目录下不存在的名字 → 两侧 ENOENT。
        diff_lookup(EXT4_IMAGE, 2, "no-such-name", core_lookup);

        // 子目录内命中：建 'd1'/'f1'，在 'd1' 下查 'f1'。
        let bytes = build_dir_populated_image(&[
            DirOp::Mkdir {
                parent: 2,
                name: "d1",
                mode: 0o40755,
            },
            DirOp::Create {
                parent: 2,
                name: "f1",
                mode: 0o100644,
            },
        ]);
        // 'f1' 建在根下（parent=2）；'d1' 是空目录。先验证根下查 'f1' 命中、'd1' 命中。
        diff_lookup(&bytes, 2, "f1", core_lookup);
        diff_lookup(&bytes, 2, "d1", core_lookup);
        // 子目录内查 '.'（命中其自身）与不存在项（ENOENT）。
        let d1_ino = old_lookup(&MemDisk::from_image(&bytes), 2, "d1").expect("d1 present");
        diff_lookup(&bytes, d1_ino, ".", core_lookup);
        diff_lookup(&bytes, d1_ino, "ghost", core_lookup);
    }
}
