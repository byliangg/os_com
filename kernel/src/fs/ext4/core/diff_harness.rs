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

use super::io::BlockReader;
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

/// 一次「元数据区」的快照：超级块计数/csum 区 + 全部组描述符表 + 每组的块/ inode 位图块。
pub(super) struct MetaSnapshot {
    regions: Vec<MetaRegion>,
}

/// 从超级块推导并截取元数据区，得到一张可逐字节比对的快照。
///
/// 截取范围（覆盖分配器会动到的元数据）：
/// 1. **超级块区**：偏移 1024、长 1024——含 free_blocks/free_inodes 计数与 checksum；
/// 2. **组描述符表（GDT）**：偏移 `(first_data_block + 1) * block_size`，
///    长 `num_groups * group_desc_size`——含每组的 free 计数与位图 csum；
/// 3. **每组两张位图块**：`block_bitmap` / `inode_bitmap` 各一个块大小。
///
/// `num_groups = ceil(blocks_count / blocks_per_group)`。位图块偏移逐组从对应组描述符
/// 读出（兼容任意布局），故先截 GDT、再据之定位各位图。
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
    use ostd::prelude::*;

    use super::{assert_meta_eq, snapshot_meta, DirectMetadataWriter, MemDisk};
    use crate::fs::ext4::core::block_group::RawGroupDescriptor;
    use crate::fs::ext4::core::io::BlockReader;
    use crate::fs::ext4::core::metadata_writer::MetadataWriter;
    use crate::fs::ext4::core::superblock::RawSuperblock;
    use crate::fs::ext4::core::test_util::EXT4_IMAGE;
    use crate::prelude::*;

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
}
