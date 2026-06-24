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

// =====================================================================
// Phase 5 Task 0：JBD2 日志差分地基（仅 ktest）。
//
// 复用 Phase 2-4 的两-MemDisk 模式（旧 ext4_rs vs 新 core，同 journal 镜像、
// 跑同序列、逐字节对拍），把它扩到 journal 区：
//
// - [`resolve_journal_area`]：经 ext4_rs `Jbd2Journal::load` 拿 journal inode 的
//   物理块向量 + journal 超级块几何（first/maxlen/start/head/sequence/blocksize）。
//   两侧差分引擎必须打到**同一组物理 journal 块**——本函数是唯一权威定位。
// - [`snapshot_journal_area`]：按物理块逐块读出 journal 区字节，供 commit 后逐字节对拍。
// - 旧侧驱动 [`old_journal_commit`]（open → JournalRuntime: start_handle →
//   record_metadata_write_for_handle* → stop_handle → prepare_commit →
//   `Jbd2Journal::write_commit_plan`）跑一段最小事务。
// - 崩溃注入 seam [`JournalCrashStage`] + [`old_journal_commit_with_crash`]：包住
//   ext4_rs `write_commit_plan_with_hook` 的 4 个 stage（BeforeDescriptor /
//   BeforeCommitBlock / AfterCommitBlock / AfterSuperblock），供 Task 5 的恢复差分。
// - [`diff_journal_commit`] / [`diff_journal_recover`] 骨架：旧半部现成，新（core）半部
//   留闭包注入——Task 3 接 commit、Task 5 接 recovery，无需返工旧侧。
//
// 注意：JBD2 全大端，journal 超级块在 journal **逻辑块 0**（= physical_blocks[0]）。
// =====================================================================

/// journal 超级块几何（全大端读出后的逻辑值）。块号均为 journal **逻辑块**号
/// （相对 journal 区起点，0 = journal 超级块所在块），经 physical_blocks 折成物理块。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct JournalGeom {
    /// journal 块大小（应 == fs block_size）。
    pub block_size: u32,
    /// 环第一个可用块（journal 逻辑块号；超级块占逻辑块 0，故 first 通常为 1）。
    pub first: u32,
    /// 环可用长度（journal 逻辑块数，含超级块前的保留？见 ext4_rs：maxlen = 总逻辑块）。
    pub maxlen: u32,
    /// 最老未 checkpoint 事务起始块；0 = 日志为空（无需恢复）。
    pub start: u32,
    /// 环写入头（下一次 commit 起始块）；0 表示尚未写过。
    pub head: u32,
    /// 下一个事务序号。
    pub sequence: u32,
}

/// 经 ext4_rs `Jbd2Journal::load` 定位 journal 区：返回 journal inode 的**物理块向量**
/// （`physical_blocks[i]` = journal 逻辑块 i 的 fs 物理块号）+ 几何 [`JournalGeom`]。
///
/// 这是差分两侧（旧 ext4_rs / 新 core）打 journal 的唯一权威定位——两侧用同一向量才能
/// 保证写到**同一组物理块**、对拍才有意义。失败即 panic（fixture 必须带合法 journal inode）。
pub(super) fn resolve_journal_area(disk: &MemDisk) -> (Vec<Ext4Fsblk>, JournalGeom) {
    let ext4 = ext4_rs::Ext4::open(Arc::new(disk.clone()));
    let journal = ext4_rs::Jbd2Journal::load(&ext4)
        .unwrap_or_else(|e| panic!("Jbd2Journal::load failed (fixture must have a journal): {e:?}"));

    let physical_blocks: Vec<Ext4Fsblk> = journal.device.physical_blocks().to_vec();
    let geom = JournalGeom {
        block_size: journal.superblock.block_size(),
        first: journal.superblock.first(),
        maxlen: journal.superblock.maxlen(),
        start: journal.superblock.start(),
        head: journal.superblock.head(),
        sequence: journal.superblock.sequence(),
    };
    (physical_blocks, geom)
}

/// 读出 journal 区字节：按 `physical_blocks` 顺序逐块（`block_size` 字节）读出拼成一段。
///
/// 结果是「journal 逻辑块 0..N 的连续字节镜像」，与物理盘上是否连续无关——故两侧用各自的
/// `physical_blocks`（实为同一组，由 [`resolve_journal_area`] 在同布局盘上解出）读出后可直接
/// 逐字节比较。commit 前后各取一次即可判断「commit 是否写了 journal 区 / 写了哪里」。
pub(super) fn snapshot_journal_area(
    disk: &MemDisk,
    physical_blocks: &[Ext4Fsblk],
    block_size: usize,
) -> Vec<u8> {
    let mut out = vec![0u8; physical_blocks.len() * block_size];
    for (i, &pblock) in physical_blocks.iter().enumerate() {
        let off = (pblock as usize) * block_size;
        disk.read_at(off, &mut out[i * block_size..(i + 1) * block_size]);
    }
    out
}

/// 崩溃注入阶段，一一对应 ext4_rs `JournalCommitWriteStage` 的 4 个 hook 点。
///
/// commit 写序（RED LINE）：写 descriptor+payload（异步）→ **sync 屏障** → 写 commit 块 →
/// 更新 journal SB store → ring advance。这 4 个 stage 是「在某步**之后/之前**剪断」的注入点：
/// - `BeforeDescriptor`：descriptor/payload 尚未写——模拟 commit 完全未开始。
/// - `BeforeCommitBlock`：descriptor/payload 已写、commit 块尚未写——事务**不可** replay。
/// - `AfterCommitBlock`：commit 块已写、SB 尚未更新——事务**可** replay，但 SB 还没指向它。
/// - `AfterSuperblock`：SB 已更新——事务完全持久、可恢复。
///
/// Task 5 用 [`crash_at`] 把某 stage 当「崩溃点」：在该 hook 触发时记下/快照，得到一份
/// 「崩在该 stage」的 journal 区，再喂给两侧 `recover` 对拍。
#[allow(dead_code)] // Task 5 wires the recovery differential that consumes these stages
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum JournalCrashStage {
    BeforeDescriptor,
    BeforeCommitBlock,
    AfterCommitBlock,
    AfterSuperblock,
}

impl JournalCrashStage {
    /// 映射到 ext4_rs 的 stage 枚举（崩溃注入 hook 用）。
    fn to_ext4_rs(self) -> ext4_rs::JournalCommitWriteStage {
        match self {
            JournalCrashStage::BeforeDescriptor => {
                ext4_rs::JournalCommitWriteStage::BeforeDescriptor
            }
            JournalCrashStage::BeforeCommitBlock => {
                ext4_rs::JournalCommitWriteStage::BeforeCommitBlock
            }
            JournalCrashStage::AfterCommitBlock => {
                ext4_rs::JournalCommitWriteStage::AfterCommitBlock
            }
            JournalCrashStage::AfterSuperblock => {
                ext4_rs::JournalCommitWriteStage::AfterSuperblock
            }
        }
    }
}

/// 旧侧驱动用的一条「写元数据块」操作：把 `image`（全块镜像，长度应 == block_size）
/// 记到 journal 逻辑块 `block_nr`（= 在 `JournalRuntime` 里按 `block_nr*block_size` 字节偏移
/// 记录，落进事务的 `BTreeMap<block_nr, JournalBuffer>`，对应 descriptor tag 的 target 块号）。
#[derive(Debug, Clone)]
pub(super) struct JournalMetaWrite {
    pub block_nr: Ext4Fsblk,
    pub image: Vec<u8>,
}

/// 用 ext4_rs 在 `disk` 上跑一段最小 journal 事务并 commit 落盘：
/// `Ext4::open` → `Jbd2Journal::load` → `JournalRuntime::new(block_size, sequence)` →
/// `start_handle` → 每个 [`JournalMetaWrite`] `record_metadata_write_for_handle` →
/// `stop_handle` → `prepare_commit` → `Jbd2Journal::write_commit_plan`。
///
/// 返回 commit 写入的 tid（= 序号）。任一环节失败即 panic（这是构造良性 fixture 的旧侧驱动，
/// 不是被对拍的 core 侧；setup 失败属测试用法错误）。`writes` 不可为空（commit plan 至少 1 块）。
pub(super) fn old_journal_commit(disk: &MemDisk, writes: &[JournalMetaWrite]) -> u32 {
    old_journal_commit_inner(disk, writes, None, || {})
}

/// 同 [`old_journal_commit`]，但在 commit 写序的某 stage 注入「崩溃」：当 ext4_rs 触发
/// `crash_at` 对应的 hook 时，`on_crash` 被调用一次（可在此对 `disk` 快照）。注入**只观测**，
/// 不打断 ext4_rs 后续步骤——「崩在此 stage」的 journal 区 = `on_crash` 触发瞬间的盘字节，
/// 由调用方在 `on_crash` 里截取。返回 commit tid。供 Task 5 造各 stage 的崩溃态。
#[allow(dead_code)] // Task 5 wires the recovery differential (crash-injection seam)
pub(super) fn old_journal_commit_with_crash(
    disk: &MemDisk,
    writes: &[JournalMetaWrite],
    crash_at: JournalCrashStage,
    on_crash: impl FnMut(),
) -> u32 {
    old_journal_commit_inner(disk, writes, Some(crash_at), on_crash)
}

/// [`old_journal_commit`] / [`old_journal_commit_with_crash`] 的公共实现。
/// `crash_at` 为某 stage 时，ext4_rs 触发该 stage 的 hook 会调用一次 `on_crash`（仅观测，
/// 不打断后续步骤）；为 `None` 时 `on_crash` 永不被调用（传 no-op 即可，非 `'static`，
/// 故 `on_crash` 用泛型不装箱——允许借用本地状态对 `disk` 快照）。
fn old_journal_commit_inner(
    disk: &MemDisk,
    writes: &[JournalMetaWrite],
    crash_at: Option<JournalCrashStage>,
    mut on_crash: impl FnMut(),
) -> u32 {
    assert!(
        !writes.is_empty(),
        "old_journal_commit needs ≥1 metadata write (commit plan must have ≥1 block)"
    );

    let ext4 = ext4_rs::Ext4::open(Arc::new(disk.clone()));
    let block_device = ext4.block_device.clone();
    let mut journal = ext4_rs::Jbd2Journal::load(&ext4)
        .unwrap_or_else(|e| panic!("Jbd2Journal::load failed: {e:?}"));
    let block_size = journal.superblock.block_size() as usize;

    // 内存事务状态机：first_tid = journal SB 当前序号（与 ext4_rs commit 写的 plan.tid 对齐）。
    let mut runtime = ext4_rs::JournalRuntime::new(block_size, journal.superblock.sequence());

    let handle = runtime
        .start_handle(writes.len() as u32 + 2, None)
        .expect("start_handle on enabled runtime");
    let handle_id = handle.handle_id();
    for w in writes {
        assert_eq!(
            w.image.len(),
            block_size,
            "metadata image must be a full block ({} bytes), got {}",
            block_size,
            w.image.len()
        );
        // record_metadata_write_for_handle 按 byte offset 记录，block_nr = offset/block_size。
        let offset = (w.block_nr as usize) * block_size;
        let image = w.image.clone();
        runtime.record_metadata_write_for_handle(handle_id, offset, &w.image, move |_| image.clone());
    }
    runtime.stop_handle(handle);

    let plan = runtime
        .prepare_commit()
        .expect("prepare_commit yields a plan after a closed handle with metadata");
    let tid = plan.tid;

    let result = match crash_at {
        None => journal.write_commit_plan(&block_device, &plan),
        Some(stage) => {
            let target = stage.to_ext4_rs();
            journal.write_commit_plan_with_hook(&block_device, &plan, |reached| {
                if reached == target {
                    on_crash();
                }
            })
        }
    };
    result.unwrap_or_else(|e| panic!("write_commit_plan failed: {e:?}"));
    tid
}

/// commit 差分骨架：两张独立 `MemDisk`（同初始 `image` 字节），旧侧用 [`old_journal_commit`]
/// 跑 `writes` 并 commit，新（core）侧由 `core_commit` 闭包注入（**Task 3 wires the core side**）。
/// 跑后对拍 journal 区逐字节 + 两侧返回 tid 一致。
///
/// Task 0 仅提供结构与旧半部；新半部留闭包，Task 3 接 core `write_commit_plan` 时无需返工旧侧。
/// 故 Task 0 暂无调用者 → `#[allow(dead_code)]`。
#[allow(dead_code)] // Task 3 wires the core side (core::journal::commit::write_commit_plan)
pub(super) fn diff_journal_commit(
    image: &[u8],
    writes: &[JournalMetaWrite],
    core_commit: impl FnOnce(&MemDisk, &[JournalMetaWrite]) -> u32,
) {
    let old_disk = MemDisk::from_image(image);
    let new_disk = MemDisk::from_image(image);

    // 两侧用各自盘解出的物理块向量（同布局盘 → 同一组物理块）。
    let (old_blocks, geom) = resolve_journal_area(&old_disk);
    let (new_blocks, _) = resolve_journal_area(&new_disk);
    assert_eq!(old_blocks, new_blocks, "journal physical blocks must match across disks");
    let bs = geom.block_size as usize;

    let old_tid = old_journal_commit(&old_disk, writes);
    let new_tid = core_commit(&new_disk, writes);
    assert_eq!(old_tid, new_tid, "commit tid mismatch: old {old_tid} new {new_tid}");

    let old_area = snapshot_journal_area(&old_disk, &old_blocks, bs);
    let new_area = snapshot_journal_area(&new_disk, &new_blocks, bs);
    assert_journal_area_eq(&old_area, &new_area, &old_blocks, bs);
}

/// recovery 差分骨架：给一份**已崩溃**（某 stage）的初始 `image`，两侧各跑恢复/replay，
/// 对拍 home 区逐字节 + 恢复结果。旧侧由 `old_recover` 闭包（Task 5 接 ext4_rs `recovery`），
/// 新侧由 `core_recover` 闭包（**Task 5 wires the core side**：core `recovery::recover`）。
///
/// Task 0 仅提供结构 + journal-区/全盘对拍工具；两侧 recover 闭包留待 Task 5。
/// 故 Task 0 暂无调用者 → `#[allow(dead_code)]`。
#[allow(dead_code)] // Task 5 wires both recover sides (ext4_rs recovery vs core::journal::recovery)
pub(super) fn diff_journal_recover(
    crashed_image: &[u8],
    old_recover: impl FnOnce(&MemDisk),
    core_recover: impl FnOnce(&MemDisk),
) {
    let old_disk = MemDisk::from_image(crashed_image);
    let new_disk = MemDisk::from_image(crashed_image);
    old_recover(&old_disk);
    core_recover(&new_disk);
    // replay 后 home 区（整盘）逐字节一致。
    assert_disk_eq(&old_disk, &new_disk);
}

/// 造一份「崩在 `crash_at` stage」的盘字节：在 `image` 克隆盘上用 ext4_rs commit-with-hook
/// 跑 `writes`，在 `crash_at` 对应的 hook 触发瞬间快照整盘字节并返回——即「崩溃后的盘」。
///
/// ext4_rs 的崩溃注入是**只观测、不打断**（hook 后 ext4_rs 仍继续写完）；故「崩在某 stage」
/// 的盘 = 该 hook 触发那一刻的字节快照（此刻该 stage 之前的写已落、之后的写未落）。Task 5
/// 用它造各 stage 的恢复输入：
/// - `BeforeDescriptor`：descriptor/payload/commit/SB 全未写——journal 区仍是初始 fixture（空，
///   `s_start==0`），两侧 recover 均不 replay（needs_recovery=false），home 不变。
/// - `BeforeCommitBlock`：descriptor+payload 已写、commit 未写、SB 未更新（`s_start` 仍 0）——
///   SCAN 找不到 commit（needs_recovery 仍 false，因 SB 未指向），不 replay。
/// - `AfterCommitBlock`：commit 已写、SB 未更新（`s_start` 仍 0）——SB 未指向该事务，needs_recovery
///   仍 false，不 replay（事务可 replay 但 SB 没记录它，恢复无从知晓——与真盘崩溃语义一致）。
/// - `AfterSuperblock`：SB 已更新（`s_start` 指向 descriptor）——needs_recovery=true，两侧 replay
///   该事务到 home。
pub(super) fn build_crashed_journal_image(
    image: &[u8],
    writes: &[JournalMetaWrite],
    crash_at: JournalCrashStage,
) -> Vec<u8> {
    let disk = MemDisk::from_image(image);
    let backing = disk.backing();
    let captured: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let captured_cb = captured.clone();
    let on_crash = move || {
        // 首次触发该 stage 时快照整盘（commit plan 单事务、单次触发即足）。
        let mut slot = captured_cb.lock();
        if slot.is_none() {
            *slot = Some(backing.lock().clone());
        }
    };
    old_journal_commit_with_crash(&disk, writes, crash_at, on_crash);
    captured
        .lock()
        .take()
        .expect("crash hook must fire once for the requested stage")
}

/// 旧侧（ext4_rs）恢复驱动：`Ext4::open` → `Jbd2Journal::load` → `journal.recover(&block_device)`。
/// 返回 ext4_rs `JournalRecoveryResult`（差分对拍其字段）。setup 失败即 panic（旧侧是基准）；
/// `recover` 本身的 Ok/Err 由调用方用 `match (old, new)` 对拍——本 helper 返回 `Result` 透传。
#[allow(dead_code)] // consumed by Task 5 recovery differential tests
pub(super) fn old_journal_recover(
    disk: &MemDisk,
) -> core::result::Result<ext4_rs::JournalRecoveryResult, ext4_rs::Ext4Error> {
    let ext4 = ext4_rs::Ext4::open(Arc::new(disk.clone()));
    let block_device = ext4.block_device.clone();
    let mut journal = ext4_rs::Jbd2Journal::load(&ext4)
        .unwrap_or_else(|e| panic!("Jbd2Journal::load failed (recovery fixture): {e:?}"));
    journal.recover(&block_device)
}

/// 新侧（core）恢复驱动：`load_journal_sb` → 建 [`RecoverCtx`]（reader/writer = 同一 `MemDisk`）→
/// core `recovery::recover(&ctx, &mut sb)`。返回 `(core RecoverResult, 重置后的 sb)`，透传 `Result`。
///
/// 注意：core `recover` 把重置后的 SB 既 store 回盘（块 0）又写进 `sb` 出参——差分用出参 `sb`
/// 对拍 `s_start/s_sequence/s_head/checksum`，用整盘对拍盘上 SB 块字节。
#[allow(dead_code)] // consumed by Task 5 recovery differential tests
pub(super) fn core_journal_recover(
    disk: &MemDisk,
) -> Result<(
    crate::fs::ext4::core::journal::recovery::RecoverResult,
    crate::fs::ext4::core::journal::format::RawJournalSuperblock,
)> {
    use crate::fs::ext4::core::journal::recovery::{recover, RecoverCtx};
    use crate::fs::ext4::core::journal::superblock::load_journal_sb;

    let (physical_blocks, geom) = resolve_journal_area(disk);
    let bs = geom.block_size as usize;
    let mut sb = load_journal_sb(disk, &physical_blocks, bs)?;
    let ctx = RecoverCtx {
        physical_blocks: &physical_blocks,
        reader: disk,
        writer: disk,
        block_size: bs,
    };
    let result = recover(&ctx, &mut sb)?;
    Ok((result, sb))
}

/// 逐字节比对两段 journal 区镜像；不等时 panic 并报**首个差异的 journal 逻辑块 + 块内偏移 +
/// 物理块号 + 两侧字节值**（比 `assert_disk_eq` 的盘内 offset 更便于定位 descriptor/tag/commit）。
#[allow(dead_code)] // consumed by diff_journal_commit (Task 3+)
pub(super) fn assert_journal_area_eq(
    a: &[u8],
    b: &[u8],
    physical_blocks: &[Ext4Fsblk],
    block_size: usize,
) {
    assert_eq!(a.len(), b.len(), "journal area length differs ({} vs {})", a.len(), b.len());
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        if x != y {
            let lblock = i / block_size;
            let in_block = i % block_size;
            let pblock = physical_blocks.get(lblock).copied().unwrap_or(0);
            panic!(
                "journal area mismatch at logical block {lblock} (phys {pblock}) byte {in_block}: \
                 {x:#04x} != {y:#04x}"
            );
        }
    }
}

#[cfg(ktest)]
mod test {
    use alloc::format;

    use ostd::prelude::*;

    use super::{
        assert_disk_eq, assert_journal_area_eq, assert_meta_eq, build_crashed_journal_image,
        core_journal_recover, diff_journal_commit, old_journal_commit, old_journal_recover,
        resolve_journal_area, snapshot_inode_table_group, snapshot_journal_area, snapshot_meta,
        snapshot_meta_with_inodes, DirectMetadataWriter, JournalCrashStage, JournalMetaWrite,
        MemDisk,
    };
    use crate::fs::ext4::core::block_group::RawGroupDescriptor;
    use crate::fs::ext4::core::inode::{inode_checksum, RawInode};
    use crate::fs::ext4::core::io::BlockReader;
    use crate::fs::ext4::core::metadata_writer::MetadataWriter;
    use crate::fs::ext4::core::superblock::RawSuperblock;
    use crate::fs::ext4::core::test_util::{
        EXT4_IMAGE, EXT4_MULTIGROUP_IMAGE, EXT4_NOCSUM_IMAGE,
    };
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

    // =================================================================
    // Phase 4 Task 2：目录项 CRUD 写差分（切槽插入 / 删除合并 / 块 csum 写 / 追加新块）。
    //
    // 差分驱动（控制器裁决：经 ext4_rs 公开 dir 法直接对拍）：
    // - OLD：`ext4_rs::Ext4::open` → `get_inode_ref(parent)` → 调 `dir_add_entry` /
    //   `dir_remove_entry` / `dir_remove_entry_at_offset`（均为 `pub fn` on `impl Ext4`）。
    // - NEW：core `dir_add_entry` / `dir_remove_entry` / `dir_remove_entry_at_offset`，同 parent
    //   inode + 同 child ino/type；core 分配器经 `CoreDirAllocAdapter`（Phase-2 `BlockAllocator`
    //   + `InodeAllocCtx`，与 file.rs 差分同一套），WriteCtx 经 `DirectMetadataWriter`。
    // 每步两盘 `assert_disk_eq` 全盘逐字节（dir 块 + inode 表 i_size/links + 位图/SB/extent）。
    // 另含一个 byte-exact 单测（`try_insert` / `insert_to_new_block` 在内存块上跑、对拍手算值）。
    // =================================================================

    use crate::fs::ext4::core::balloc::{BlockAllocator, InodeAllocCtx};
    use crate::fs::ext4::core::dir::{
        dir_add_entry, dir_remove_entry, dir_remove_entry_at_offset, inode_to_dir_entry_type,
        insert_to_new_block, try_insert_to_existing_block, EXT4_DIR_ENTRY_INMEM_SIZE,
    };
    use crate::fs::ext4::core::extents::{BlockAlloc, WriteCtx};
    use crate::fs::ext4::core::inode::{load_inode, Inode};
    use crate::fs::ext4::core::types::Ext4Fsblk as Fsblk;
    // core 写半部用 `crate::prelude::Result`（= core/prelude 的 Result，带 core Error）；
    // 显式（非 glob）引入以消除 `ostd::prelude::*` 同名 `Result` 的歧义（与 file.rs 差分一致）。
    use crate::prelude::Result;

    /// 把 Phase-2 `BlockAllocator` + `InodeAllocCtx` 适配成 core 写半部要的 [`BlockAlloc`]
    /// （与 file.rs 差分里的 `CoreAllocAdapter` 同套：分配后把 i_blocks 同步回 inode）。
    struct CoreDirAllocAdapter<'a, R: BlockReader, W: MetadataWriter> {
        alloc: BlockAllocator<'a, R, W>,
        ictx: InodeAllocCtx,
    }

    impl<'a, R: BlockReader, W: MetadataWriter> BlockAlloc for CoreDirAllocAdapter<'a, R, W> {
        fn alloc_one(&mut self, inode: &mut Inode) -> Result<Fsblk> {
            let blk = self.alloc.balloc_alloc_block(&mut self.ictx, None)?;
            inode.set_blocks_count(self.ictx.i_blocks());
            Ok(blk)
        }
        fn alloc_batch(
            &mut self,
            inode: &mut Inode,
            start_bgid: &mut u32,
            count: usize,
        ) -> Result<Vec<Fsblk>> {
            let v = self
                .alloc
                .balloc_alloc_block_batch(&mut self.ictx, start_bgid, count)?;
            inode.set_blocks_count(self.ictx.i_blocks());
            Ok(v)
        }
        fn free_blocks(&mut self, inode: &mut Inode, start: Fsblk, count: u32) {
            self.alloc.balloc_free_blocks(&mut self.ictx, start, count);
            inode.set_blocks_count(self.ictx.i_blocks());
        }
    }

    /// 旧侧（ext4_rs）`dir_add_entry`：open → get_inode_ref(parent) + get_inode_ref(child) →
    /// `dir_add_entry(&mut parent_ref, &child_ref, name)`。返回 Ok-Err（数值映射），盘字节就地变。
    fn old_dir_add_entry(
        disk: &MemDisk,
        parent: u32,
        child: u32,
        name: &str,
    ) -> core::result::Result<(), ext4_rs::Errno> {
        let ext4 = ext4_rs::Ext4::open(Arc::new(disk.clone()));
        let mut parent_ref = ext4.get_inode_ref(parent);
        let child_ref = ext4.get_inode_ref(child);
        ext4.dir_add_entry(&mut parent_ref, &child_ref, name)
            .map(|_| ())
            .map_err(|e| e.error())
    }

    /// 新侧（core）`dir_add_entry`：从盘重建 SB / 分配器 / WriteCtx / parent inode；child 的
    /// DE filetype 由 core `inode_to_dir_entry_type(child_inode)` 算。返回 Ok-Err（数值映射）。
    fn core_dir_add_entry(
        disk: &MemDisk,
        parent: u32,
        child: u32,
        name: &str,
    ) -> core::result::Result<(), ext4_rs::Errno> {
        let sb = read_sb(disk);
        let bs = sb.block_size();
        let writer = DirectMetadataWriter::new(disk.clone(), bs);
        let alloc = BlockAllocator::new(sb, disk, &writer);
        let mut parent_inode = match load_inode(disk, &sb, parent) {
            Ok(i) => i,
            Err(e) => return Err(map_core_errno(e.error())),
        };
        let child_inode = match load_inode(disk, &sb, child) {
            Ok(i) => i,
            Err(e) => return Err(map_core_errno(e.error())),
        };
        let child_ftype = inode_to_dir_entry_type(&child_inode);
        let ictx = InodeAllocCtx::new(parent_inode.blocks_count());
        let mut adapter = CoreDirAllocAdapter { alloc, ictx };
        let ctx = WriteCtx::new(disk, &writer, disk, &sb);
        dir_add_entry(
            &ctx,
            &mut adapter,
            &mut parent_inode,
            child,
            child_ftype,
            name.as_bytes(),
        )
        .map_err(|e| map_core_errno(e.error()))
    }

    /// 旧侧（ext4_rs）`dir_remove_entry`：open → get_inode_ref(parent) → `dir_remove_entry`。
    fn old_dir_remove_entry(
        disk: &MemDisk,
        parent: u32,
        name: &str,
    ) -> core::result::Result<(), ext4_rs::Errno> {
        let ext4 = ext4_rs::Ext4::open(Arc::new(disk.clone()));
        let mut parent_ref = ext4.get_inode_ref(parent);
        ext4.dir_remove_entry(&mut parent_ref, name)
            .map(|_| ())
            .map_err(|e| e.error())
    }

    /// 新侧（core）`dir_remove_entry`。
    fn core_dir_remove_entry(
        disk: &MemDisk,
        parent: u32,
        name: &str,
    ) -> core::result::Result<(), ext4_rs::Errno> {
        let sb = read_sb(disk);
        let bs = sb.block_size();
        let writer = DirectMetadataWriter::new(disk.clone(), bs);
        let mut parent_inode = match load_inode(disk, &sb, parent) {
            Ok(i) => i,
            Err(e) => return Err(map_core_errno(e.error())),
        };
        let ctx = WriteCtx::new(disk, &writer, disk, &sb);
        dir_remove_entry(&ctx, &mut parent_inode, name.as_bytes())
            .map_err(|e| map_core_errno(e.error()))
    }

    /// 旧侧（ext4_rs）`dir_remove_entry_at_offset`。
    fn old_dir_remove_at_offset(
        disk: &MemDisk,
        parent: u32,
        abs_off: u64,
    ) -> core::result::Result<(), ext4_rs::Errno> {
        let ext4 = ext4_rs::Ext4::open(Arc::new(disk.clone()));
        let mut parent_ref = ext4.get_inode_ref(parent);
        ext4.dir_remove_entry_at_offset(&mut parent_ref, abs_off)
            .map(|_| ())
            .map_err(|e| e.error())
    }

    /// 新侧（core）`dir_remove_entry_at_offset`。
    fn core_dir_remove_at_offset(
        disk: &MemDisk,
        parent: u32,
        abs_off: u64,
    ) -> core::result::Result<(), ext4_rs::Errno> {
        let sb = read_sb(disk);
        let bs = sb.block_size();
        let writer = DirectMetadataWriter::new(disk.clone(), bs);
        let mut parent_inode = match load_inode(disk, &sb, parent) {
            Ok(i) => i,
            Err(e) => return Err(map_core_errno(e.error())),
        };
        let ctx = WriteCtx::new(disk, &writer, disk, &sb);
        dir_remove_entry_at_offset(&ctx, &mut parent_inode, abs_off)
            .map_err(|e| map_core_errno(e.error()))
    }

    /// add-entry 差分一步：两盘从同一 `image` 起步，旧/新各跑 `dir_add_entry(parent, child, name)`，
    /// 比 Ok-Err + 全盘逐字节。返回供链式调用的结果字节（用最新盘面继续下一步）。
    fn diff_add_step(image: &[u8], parent: u32, child: u32, name: &str) -> Vec<u8> {
        let old_disk = MemDisk::from_image(image);
        let new_disk = MemDisk::from_image(image);
        let old_ret = old_dir_add_entry(&old_disk, parent, child, name);
        let new_ret = core_dir_add_entry(&new_disk, parent, child, name);
        match (old_ret, new_ret) {
            (Ok(()), Ok(())) => {}
            (Err(o), Err(n)) => assert_eq!(o, n, "add '{name}' errno mismatch: old {o:?} new {n:?}"),
            (o, n) => panic!("add '{name}' ok/err divergence: old={o:?} new={n:?}"),
        }
        // BUG-21: 归一 ext4_rs 写项的未初始化 padding 字节（仅 rec_len>=264 项的 +263）再全盘对拍。
        let sb = read_sb(&new_disk);
        mask_dirent_padding(&old_disk, &sb, parent);
        mask_dirent_padding(&new_disk, &sb, parent);
        assert_disk_eq(&old_disk, &new_disk);
        new_disk.backing().lock().clone()
    }

    /// remove-by-name 差分一步：旧/新各跑 `dir_remove_entry(parent, name)`，比 Ok-Err + 全盘。
    fn diff_remove_step(image: &[u8], parent: u32, name: &str) -> Vec<u8> {
        let old_disk = MemDisk::from_image(image);
        let new_disk = MemDisk::from_image(image);
        let old_ret = old_dir_remove_entry(&old_disk, parent, name);
        let new_ret = core_dir_remove_entry(&new_disk, parent, name);
        match (old_ret, new_ret) {
            (Ok(()), Ok(())) => {}
            (Err(o), Err(n)) => {
                assert_eq!(o, n, "remove '{name}' errno mismatch: old {o:?} new {n:?}")
            }
            (o, n) => panic!("remove '{name}' ok/err divergence: old={o:?} new={n:?}"),
        }
        // BUG-21: 删项后块尾项 rec_len 可能因合并增长到 >=264（吞并出大槽），同样归一 padding 字节。
        let sb = read_sb(&new_disk);
        mask_dirent_padding(&old_disk, &sb, parent);
        mask_dirent_padding(&new_disk, &sb, parent);
        assert_disk_eq(&old_disk, &new_disk);
        new_disk.backing().lock().clone()
    }

    /// remove-by-offset 差分一步：旧/新各跑 `dir_remove_entry_at_offset(parent, abs_off)`。
    fn diff_remove_at_offset_step(image: &[u8], parent: u32, abs_off: u64) -> Vec<u8> {
        let old_disk = MemDisk::from_image(image);
        let new_disk = MemDisk::from_image(image);
        let old_ret = old_dir_remove_at_offset(&old_disk, parent, abs_off);
        let new_ret = core_dir_remove_at_offset(&new_disk, parent, abs_off);
        match (old_ret, new_ret) {
            (Ok(()), Ok(())) => {}
            (Err(o), Err(n)) => assert_eq!(
                o, n,
                "remove@{abs_off} errno mismatch: old {o:?} new {n:?}"
            ),
            (o, n) => panic!("remove@{abs_off} ok/err divergence: old={o:?} new={n:?}"),
        }
        // BUG-21: 同 diff_remove_step——归一 padding 字节再全盘对拍。
        let sb = read_sb(&new_disk);
        mask_dirent_padding(&old_disk, &sb, parent);
        mask_dirent_padding(&new_disk, &sb, parent);
        assert_disk_eq(&old_disk, &new_disk);
        new_disk.backing().lock().clone()
    }

    /// 归一化 ext4_rs 的 `Ext4DirEntry` 未初始化 padding 泄漏（**BUG-21**）后再对拍。
    ///
    /// BUG-21：`Ext4DirEntry` 是 `#[repr(C)]`，字段共 263 字节、对齐到 **264**；其
    /// `impl Default`（direntry.rs:96）用结构体字面量（**非** `mem::zeroed()`），故结构体**字节
    /// 263 是未初始化的 padding**。ext4_rs 写项经 `copy_to_slice` / `copy_dir_entry_to_array`
    /// 做 `unsafe` 264 字节 `copy_nonoverlapping`，把那个未初始化的栈字节（实测 0x88）泄漏到盘。
    /// core 的 `dir_write_entry_bytes` 写干净零缓冲（字节 263 = 0x00）——**core 才是对的**
    /// （确定性、无信息泄漏，正是重写要修的 bug 之一），core 绝不复刻 UB 垃圾。
    ///
    /// 泄漏字节在 `entry_start + 263`，仅当该位置不被后继项覆盖时存活——精确地：**对目录每块每项，
    /// 若 `rec_len >= 264`（即 padding 字节落在本项自己的槽内、且后继项起点 ≥264 不覆盖它），
    /// `entry_start + 263` 可能是 ext4_rs 垃圾**。rec_len < 264 的项其 +263 被下一项写覆盖、不合格。
    ///
    /// 本 helper 对 `dir_inode` 的每个目录块走项（`parse_entry`），对每个 `rec_len >= 264` 的项把
    /// 块内 `entry_off + 263` 这**一个字节**清零。两盘都调用（core 侧本就是 0、no-op，但对称归一更
    /// 显然正确）。**外科级**：只动这一个字节/项，绝不放宽——其它字节仍逐字节对拍。
    ///
    /// **Fix 2（BUG-21 二阶效应）**：块尾 csum 是对 `block[..bs-12]` 算的，**含** +263 字节；ext4_rs
    /// 把 csum 算在泄漏的 0x88 上、core 算在 0x00 上，故即便两盘都把 +263 清零，盘上 tail.checksum
    /// 仍分叉（实测 `0x8d != 0xbb`）。所以：**某块若清零了 +263 字节、且 metadata_csum 开启**，就在
    /// 归一后的块内容上用 `dir_set_csum` 重算 tail.checksum 写回（两盘都做）。归一后 `block[..bs-12]`
    /// 两盘逐字节相同 → 重算出**同一个** csum → `assert_disk_eq` 过，且块保持 csum-valid（利于后续
    /// e2fsck）。**仅清零过 +263 的块**重算，不碰别的块。
    ///
    /// **BUG-22（无条件重算）**：core 的写侧 `dir_set_csum` 现**无 metadata_csum 门控**（逐字复刻
    /// ext4_rs 无条件写 dir-block csum 的行为）。故本归一在 NOCSUM 镜像下也**必须**无条件重算
    /// tail.csum——否则归一后的内容（+263 清零）与盘上 ext4_rs 写的 stale csum 分叉。`dir_set_csum`
    /// 已无条件写，这里直接调用即正确（两盘归一后同内容 → 同 csum）。
    fn mask_dirent_padding(disk: &MemDisk, sb: &RawSuperblock, dir_inode: u32) {
        use crate::fs::ext4::core::dir::{dir_set_csum, parse_entry};
        use crate::fs::ext4::core::extents::get_pblock_idx_state;

        let bs = sb.block_size();
        let dir = match load_inode(disk, sb, dir_inode) {
            Ok(i) => i,
            Err(_) => return,
        };
        // 仅目录才有目录项槽（防御；调用点都传目录 inode）。
        if !dir.raw.is_dir() {
            return;
        }
        let dir_gen = dir.raw.generation();
        let total_blocks = dir.size().div_ceil(bs as u64);
        let mut buf = vec![0u8; bs];
        let mut iblock = 0u64;
        while iblock < total_blocks {
            // hole / 映射失败的块跳过（与读路径枚举一致，不报错）。
            let pblock = match get_pblock_idx_state(disk, sb, &dir, iblock as u32) {
                Ok(Some((p, _unwritten))) => p,
                _ => {
                    iblock += 1;
                    continue;
                }
            };
            disk.read_at(pblock as usize * bs, buf.as_mut_slice());
            // 在**内存块**上走项 + 清零（避免逐字节回盘），最后整块写回一次。
            let mut masked = false;
            let mut off = 0usize;
            while off + 8 <= bs - 12 {
                let de = match parse_entry(&buf, off, bs) {
                    Ok(de) => de,
                    Err(_) => break,
                };
                let rec_len = de.rec_len as usize;
                if rec_len == 0 || rec_len > bs - off {
                    break;
                }
                // BUG-21: rec_len>=264 → padding 字节 (+263) 落在本项槽内且不被后继覆盖 → 清零。
                if rec_len >= EXT4_DIR_ENTRY_INMEM_SIZE {
                    let leak = off + (EXT4_DIR_ENTRY_INMEM_SIZE - 1);
                    if leak < bs {
                        buf[leak] = 0;
                        masked = true;
                    }
                }
                off += rec_len;
            }
            if masked {
                // Fix 2: 清零过 +263 → 在归一后的内容上重算 tail.csum（dir_set_csum 现无条件写——
                // BUG-22 un-gate，metadata_csum 关也写），再整块写回。两盘 block[..bs-12] 已逐字节相同 → 同 csum。
                dir_set_csum(&mut buf, sb, dir_gen, bs);
                let backing = disk.backing();
                let mut guard = backing.lock();
                let base = pblock as usize * bs;
                if base + bs <= guard.len() {
                    guard[base..base + bs].copy_from_slice(&buf);
                }
            }
            iblock += 1;
        }
    }

    /// 用 ext4_rs 在 `EXT4_IMAGE` 上建若干文件，返回 (盘字节, 各文件 inode 号)。供 CRUD 差分起步。
    fn build_files(names: &[&str]) -> (Vec<u8>, Vec<u32>) {
        let disk = MemDisk::from_image(EXT4_IMAGE);
        let ext4 = ext4_rs::Ext4::open(Arc::new(disk.clone()));
        let mut inos = Vec::new();
        for name in names {
            ext4.ext4_create_at(2, name, 0o100644)
                .unwrap_or_else(|e| panic!("create '{name}' failed: {e:?}"));
            let ino = ext4
                .ext4_lookup_at(2, name)
                .unwrap_or_else(|e| panic!("lookup '{name}' failed: {e:?}"));
            inos.push(ino);
        }
        (disk.backing().lock().clone(), inos)
    }

    /// 切槽插入 byte-exact 单测（无 ext4_rs 依赖）：用 core `insert_to_new_block` 初始化一个新
    /// dir 块（首项 "." rec_len=bs-12），再 core `try_insert_to_existing_block(".." )` 切槽，
    /// 对拍**手算**期望块字节。覆盖 264/8 不对称 + 264B 写 + 现有项 rec_len 缩短只改 2 字节。
    #[ktest]
    fn dir_try_insert_slot_split_parity() {
        let sb = read_sb(&MemDisk::from_image(EXT4_IMAGE));
        let bs = sb.block_size();
        const DE_DIR: u8 = 2;

        // ---- core 侧：新块写 "." → 切槽插 ".." ----
        let mut block = vec![0u8; bs];
        insert_to_new_block(&mut block, 2, b".", DE_DIR, bs);
        // 切槽前：首项 "." rec_len = bs-12。
        assert_eq!(
            u16::from_le_bytes([block[4], block[5]]) as usize,
            bs - 12,
            "'.' initial rec_len = bs-12"
        );
        let off = try_insert_to_existing_block(&mut block, b"..", 2, DE_DIR, bs)
            .expect("slot-split '..' must fit a fresh block");

        // ---- 手算期望 ----
        // sz_dot = align4(8 + name_len(1)) = align4(9) = 12 → 首项缩到 12，".." 落在 off=12。
        let sz_dot = {
            let l = 8 + 1usize;
            (l + 3) & !3
        };
        assert_eq!(sz_dot, 12, "align4(8+1)=12 (用 8，非 264)");
        assert_eq!(off, sz_dot, "new entry within-block offset = sz of '.'");
        // 首项 "." rec_len 缩短到 12；inode/name 不变。
        assert_eq!(
            u16::from_le_bytes([block[4], block[5]]) as usize,
            sz_dot,
            "'.' rec_len shrunk to 12"
        );
        assert_eq!(u32::from_le_bytes([block[0], block[1], block[2], block[3]]), 2, "'.' inode kept");
        assert_eq!(block[6], 1, "'.' name_len kept");
        assert_eq!(&block[8..9], b".", "'.' name kept");
        // 新项 ".."：rec_len = free_space = (bs-12) - 12 = bs-24；inode=2、name_len=2、type=DIR。
        let free_space = (bs - 12) - sz_dot;
        assert_eq!(
            u16::from_le_bytes([block[off + 4], block[off + 5]]) as usize,
            free_space,
            "'..' rec_len = whole remaining free_space (= bs-24)"
        );
        assert_eq!(block[off + 6], 2, "'..' name_len 2");
        assert_eq!(block[off + 7], DE_DIR, "'..' file_type DIR");
        assert_eq!(&block[off + 8..off + 10], b"..", "'..' name bytes");
        // 264B 写 parity：新项的 [off+8+2 .. off+264] 全 0（name 尾零填 + 对齐）。
        assert!(
            block[off + 10..off + EXT4_DIR_ENTRY_INMEM_SIZE].iter().all(|&b| b == 0),
            "264B write: trailing [name_len..264] zero-filled"
        );
    }

    /// 同块连续切槽插入若干短名项：每步 core vs ext4_rs `dir_add_entry` 全盘对拍。
    /// 在根目录（单块、有空间）下连插多个项，逼 try_insert 在同一块里反复切槽。child 复用
    /// 已建文件 inode（项只存 ino+type，无需新分配）。
    #[ktest]
    fn dir_add_entry_same_block_parity() {
        // 先建几个"目标"文件（提供 child inode 号），并在根目录留好空间。
        let (mut img, inos) = build_files(&["src_a", "src_b", "src_c"]);
        // 连续把这些 inode 以新名字插进根目录（同块切槽）。
        img = diff_add_step(&img, 2, inos[0], "link_a");
        img = diff_add_step(&img, 2, inos[1], "link_bb");
        let _ = diff_add_step(&img, 2, inos[2], "link_ccc");
    }

    /// 触发新建 dir 块：把根目录末块塞满（连插大量项）直到 try_insert 失败 → `dir_append_block`
    /// 新建块 + `insert_to_new_block`。对拍 extent/位图/SB/i_size + 新块字节（全盘）。
    #[ktest]
    fn dir_add_entry_new_block_parity() {
        // child inode：复用 lost+found(11)（仅存 ino+type，不分配）。
        const CHILD: u32 = 11;
        let mut img = EXT4_IMAGE.to_vec();
        // 4K 块单根目录块约容 ~250 短名项；插到溢出第一块、逼 append 新块。
        // 逐步全盘对拍（任何 rec_len/分配/extent/i_size 偏差立现）。
        for i in 0..260usize {
            let name = format!("entry_{i:05}");
            img = diff_add_step(&img, 2, CHILD, &name);
        }
        // 佐证确实跨块：旧侧 readdir 项数应远超单块容量。
        let probe = old_readdir(&MemDisk::from_image(&img), 2);
        assert!(
            probe.len() > 250,
            "expected a 2nd dir block (>250 entries); got {}",
            probe.len()
        );
    }

    /// 删中间项（前驱 rec_len 吞并）：建多个文件后删一个**非首项**，core vs ext4_rs 全盘对拍。
    #[ktest]
    fn dir_remove_middle_merge_parity() {
        let (img, _inos) = build_files(&["rm_a", "rm_b", "rm_c", "rm_d"]);
        // 删一个中间文件名（非块首项；前驱合并路径）。
        let _ = diff_remove_step(&img, 2, "rm_b");
        // 再删一个，验证连续删 + 合并。
        let img2 = diff_remove_step(&img, 2, "rm_c");
        let _ = diff_remove_step(&img2, 2, "rm_d");
    }

    /// 删块首项（**不合并**，inode=0 标删）：删根目录块首项 "."（offset 0）。
    /// core 与 ext4_rs 都只置 inode=0、不合并（parity）；全盘对拍。
    #[ktest]
    fn dir_remove_first_entry_parity() {
        // 根目录块首项是 "."（offset 0）。删它走「首项不合并」分支。
        let _ = diff_remove_step(EXT4_IMAGE, 2, ".");
    }

    /// 按 abs offset 删：先用 `dir_get_entries_with_next_offset` 算出某项的 within-block 偏移，
    /// 再两侧 `dir_remove_entry_at_offset` 对拍。删一个中间项（offset != 0，合并路径）。
    #[ktest]
    fn dir_remove_at_offset_parity() {
        let (img, _inos) = build_files(&["off_a", "off_b", "off_c"]);
        // 用旧侧底层枚举（带 next_offset）定位 "off_b" 这一项的**自身** abs offset。
        // next_offset = abs_off_of_entry + rec_len；故该项 abs_off = 前一项的 next_offset。
        let ext4 = ext4_rs::Ext4::open(Arc::new(MemDisk::from_image(&img).clone()));
        let entries = ext4.dir_get_entries_with_next_offset(2);
        // 找 "off_b" 的绝对 offset：它等于其前一项的 next_offset（枚举按块内顺序）。
        let mut abs_off = None;
        let mut prev_next = 0u64;
        for (de, next_off) in &entries {
            if de.get_name() == "off_b" {
                abs_off = Some(prev_next);
                break;
            }
            prev_next = *next_off as u64;
        }
        let abs_off = abs_off.expect("'off_b' present in enumeration");
        assert!(abs_off > 0, "'off_b' is not the first entry (offset>0)");
        let _ = diff_remove_at_offset_step(&img, 2, abs_off);
    }

    // =================================================================
    // Phase 4 Task 3：命名空间编排差分（create / mkdir / unlink / rmdir + lookup）。
    //
    // 差分驱动：两张独立 `MemDisk`（同初始字节），旧侧 `ext4_rs` 的公开命名空间法
    // （ext4_create_at / ext4_mkdir_at / ext4_unlink_at / ext4_rmdir_at），新侧 core 的
    // `NamespaceCtx` + create_at/mkdir_at/unlink_at/rmdir_at。比返回（inode 号 / Ok-Err）+
    // `assert_disk_eq` 全盘逐字节（先对触及的目录 `mask_dirent_padding` 归一 BUG-21 padding）。
    // =================================================================

    use crate::fs::ext4::core::dir::{
        create_at, create_unchecked_at, lookup_at, mkdir_at, mkdir_unchecked_at, rename_at,
        rmdir_at, rmdir_at_fast, unlink_at, NamespaceCtx,
    };
    use crate::fs::ext4::core::file::ReadCtx;

    /// 在 `disk` 上构造 core 命名空间上下文：reader=disk、metadata writer=直写、data writer=disk、
    /// sb=从盘重读。WriteCtx/分配器在 NamespaceCtx 内部据权威 SB 自管。
    ///
    /// 返回 `(NamespaceCtx, writer)`——`writer` 由调用方持有以满足借用期（NamespaceCtx 借它）。
    /// 故调用模式：`let w = DirectMetadataWriter::new(disk.clone(), bs); let mut nctx =
    /// NamespaceCtx::new(disk, &w, disk, sb);`。
    fn make_nctx<'a>(
        disk: &'a MemDisk,
        writer: &'a DirectMetadataWriter,
    ) -> NamespaceCtx<'a, MemDisk, DirectMetadataWriter, MemDisk> {
        let sb = read_sb(disk);
        NamespaceCtx::new(disk, writer, disk, sb)
    }

    /// 旧侧（ext4_rs）create：返回 Ok(inode 号)-Err(数值映射)。
    fn old_create_at(
        disk: &MemDisk,
        parent: u32,
        name: &str,
        mode: u16,
    ) -> core::result::Result<u32, ext4_rs::Errno> {
        let ext4 = ext4_rs::Ext4::open(Arc::new(disk.clone()));
        ext4.ext4_create_at(parent, name, mode).map_err(|e| e.error())
    }

    /// 旧侧（ext4_rs）mkdir：返回 Ok(inode 号)-Err。注意 ext4_rs `ext4_mkdir_at` 传给 `create`
    /// 的 mode **已带** S_IFDIR 类型位（调用方约定）；差分两侧都传 `0o40755`（含 0x4000）。
    fn old_mkdir_at(
        disk: &MemDisk,
        parent: u32,
        name: &str,
        mode: u16,
    ) -> core::result::Result<u32, ext4_rs::Errno> {
        let ext4 = ext4_rs::Ext4::open(Arc::new(disk.clone()));
        ext4.ext4_mkdir_at(parent, name, mode).map_err(|e| e.error())
    }

    /// 旧侧（ext4_rs）unlink：返回 Ok(())-Err。
    fn old_unlink_at(
        disk: &MemDisk,
        parent: u32,
        name: &str,
    ) -> core::result::Result<(), ext4_rs::Errno> {
        let ext4 = ext4_rs::Ext4::open(Arc::new(disk.clone()));
        ext4.ext4_unlink_at(parent, name).map(|_| ()).map_err(|e| e.error())
    }

    /// 旧侧（ext4_rs）rmdir：返回 Ok(())-Err。
    fn old_rmdir_at(
        disk: &MemDisk,
        parent: u32,
        name: &str,
    ) -> core::result::Result<(), ext4_rs::Errno> {
        let ext4 = ext4_rs::Ext4::open(Arc::new(disk.clone()));
        ext4.ext4_rmdir_at(parent, name).map(|_| ()).map_err(|e| e.error())
    }

    /// 新侧（core）create：建 NamespaceCtx → `create_at`。返回 Ok(inode 号)-Err（数值映射）。
    fn core_create_at(
        disk: &MemDisk,
        parent: u32,
        name: &str,
        mode: u16,
    ) -> core::result::Result<u32, ext4_rs::Errno> {
        let bs = read_sb(disk).block_size();
        let writer = DirectMetadataWriter::new(disk.clone(), bs);
        let mut nctx = make_nctx(disk, &writer);
        create_at(&mut nctx, parent, name.as_bytes(), mode).map_err(|e| map_ns_errno(e.error()))
    }

    /// 新侧（core）mkdir：建 NamespaceCtx → `mkdir_at`（core 内部 `| S_IFDIR`）。两侧 mode 都带
    /// S_IFDIR；core `mkdir_at` 再 `| S_IFDIR` 是幂等（0x4000 | 0x4000）。返回 Ok(inode 号)-Err。
    fn core_mkdir_at(
        disk: &MemDisk,
        parent: u32,
        name: &str,
        mode: u16,
    ) -> core::result::Result<u32, ext4_rs::Errno> {
        let bs = read_sb(disk).block_size();
        let writer = DirectMetadataWriter::new(disk.clone(), bs);
        let mut nctx = make_nctx(disk, &writer);
        mkdir_at(&mut nctx, parent, name.as_bytes(), mode).map_err(|e| map_ns_errno(e.error()))
    }

    /// 新侧（core）unlink：建 NamespaceCtx → `unlink_at`。返回 Ok(())-Err。
    fn core_unlink_at(
        disk: &MemDisk,
        parent: u32,
        name: &str,
    ) -> core::result::Result<(), ext4_rs::Errno> {
        let bs = read_sb(disk).block_size();
        let writer = DirectMetadataWriter::new(disk.clone(), bs);
        let mut nctx = make_nctx(disk, &writer);
        unlink_at(&mut nctx, parent, name.as_bytes()).map_err(|e| map_ns_errno(e.error()))
    }

    /// 新侧（core）rmdir：建 NamespaceCtx → `rmdir_at`。返回 Ok(())-Err。
    fn core_rmdir_at(
        disk: &MemDisk,
        parent: u32,
        name: &str,
    ) -> core::result::Result<(), ext4_rs::Errno> {
        let bs = read_sb(disk).block_size();
        let writer = DirectMetadataWriter::new(disk.clone(), bs);
        let mut nctx = make_nctx(disk, &writer);
        rmdir_at(&mut nctx, parent, name.as_bytes()).map_err(|e| map_ns_errno(e.error()))
    }

    /// 旧侧（ext4_rs）rename：返回 Ok(())-Err。注意 `ext4_rename_at` 返回 `Result<usize>`——成功
    /// 时丢弃 usize（EOK），失败时映射 errno。**禁 `.expect()`**：旧侧可能返回
    /// EXDEV / EISDIR / ENOTDIR / ENOTEMPTY / ENOENT。
    fn old_rename_at(
        disk: &MemDisk,
        old_parent: u32,
        old_name: &str,
        new_parent: u32,
        new_name: &str,
    ) -> core::result::Result<(), ext4_rs::Errno> {
        let ext4 = ext4_rs::Ext4::open(Arc::new(disk.clone()));
        ext4.ext4_rename_at(old_parent, old_name, new_parent, new_name)
            .map(|_| ())
            .map_err(|e| e.error())
    }

    /// 新侧（core）rename：建 NamespaceCtx → `rename_at`。返回 Ok(())-Err。
    fn core_rename_at(
        disk: &MemDisk,
        old_parent: u32,
        old_name: &str,
        new_parent: u32,
        new_name: &str,
    ) -> core::result::Result<(), ext4_rs::Errno> {
        let bs = read_sb(disk).block_size();
        let writer = DirectMetadataWriter::new(disk.clone(), bs);
        let mut nctx = make_nctx(disk, &writer);
        rename_at(
            &mut nctx,
            old_parent,
            old_name.as_bytes(),
            new_parent,
            new_name.as_bytes(),
        )
        .map_err(|e| map_ns_errno(e.error()))
    }

    /// rename 差分一步：两盘从 `image` 起，旧/新各 rename，比 Ok-Err + 全盘逐字节（先对所有可能
    /// 被触及的目录 `mask_dirent_padding` 归一 BUG-21 padding——rename 写新目录项，携 ext4_rs 的
    /// padding 泄漏）。`touched` 列出所有需要归一的目录 inode（old_parent / new_parent，以及覆盖
    /// 目录场景里被 truncate 的 dest 目录）。返回新侧结果字节（链式起步）。
    fn diff_rename_step(
        image: &[u8],
        old_parent: u32,
        old_name: &str,
        new_parent: u32,
        new_name: &str,
        touched: &[u32],
    ) -> Vec<u8> {
        let old_disk = MemDisk::from_image(image);
        let new_disk = MemDisk::from_image(image);
        let old_ret = old_rename_at(&old_disk, old_parent, old_name, new_parent, new_name);
        let new_ret = core_rename_at(&new_disk, old_parent, old_name, new_parent, new_name);
        match (old_ret, new_ret) {
            (Ok(()), Ok(())) => {}
            (Err(o), Err(n)) => assert_eq!(
                o, n,
                "rename '{old_name}'->'{new_name}' errno mismatch: old {o:?} new {n:?}"
            ),
            (o, n) => panic!(
                "rename '{old_name}'->'{new_name}' ok/err divergence: old={o:?} new={n:?}"
            ),
        }
        let sb = read_sb(&new_disk);
        for &ino in touched {
            mask_dirent_padding(&old_disk, &sb, ino);
            mask_dirent_padding(&new_disk, &sb, ino);
        }
        assert_disk_eq(&old_disk, &new_disk);
        new_disk.backing().lock().clone()
    }

    /// 扩展 errno 映射（命名空间路径可能出现 EEXIST / ENOTEMPTY / EISDIR / EINVAL / ENOSPC）。
    fn map_ns_errno(e: crate::prelude::Errno) -> ext4_rs::Errno {
        use crate::prelude::Errno as K;
        match e {
            K::ENOENT => ext4_rs::Errno::ENOENT,
            K::ENOTDIR => ext4_rs::Errno::ENOTDIR,
            K::EIO => ext4_rs::Errno::EIO,
            K::EEXIST => ext4_rs::Errno::EEXIST,
            K::ENOTEMPTY => ext4_rs::Errno::ENOTEMPTY,
            K::EISDIR => ext4_rs::Errno::EISDIR,
            K::EXDEV => ext4_rs::Errno::EXDEV,
            K::EINVAL => ext4_rs::Errno::EINVAL,
            K::ENOSPC => ext4_rs::Errno::ENOSPC,
            _ => ext4_rs::Errno::EINVAL,
        }
    }

    /// create 差分一步：两盘从 `image` 起，旧/新各 create，比 inode 号 + 全盘（归一 padding）。
    /// 返回新侧结果字节（链式起步）。
    fn diff_create_step(image: &[u8], parent: u32, name: &str, mode: u16) -> Vec<u8> {
        let old_disk = MemDisk::from_image(image);
        let new_disk = MemDisk::from_image(image);
        let old_ret = old_create_at(&old_disk, parent, name, mode);
        let new_ret = core_create_at(&new_disk, parent, name, mode);
        match (old_ret, new_ret) {
            (Ok(o), Ok(n)) => assert_eq!(o, n, "create '{name}' inode mismatch: old {o} new {n}"),
            (Err(o), Err(n)) => assert_eq!(o, n, "create '{name}' errno mismatch: old {o:?} new {n:?}"),
            (o, n) => panic!("create '{name}' ok/err divergence: old={o:?} new={n:?}"),
        }
        let sb = read_sb(&new_disk);
        mask_dirent_padding(&old_disk, &sb, parent);
        mask_dirent_padding(&new_disk, &sb, parent);
        assert_disk_eq(&old_disk, &new_disk);
        new_disk.backing().lock().clone()
    }

    /// mkdir 差分一步：旧/新各 mkdir，比 inode 号 + 全盘（归一父目录 + 新目录的 padding）。
    fn diff_mkdir_step(image: &[u8], parent: u32, name: &str, mode: u16) -> Vec<u8> {
        let old_disk = MemDisk::from_image(image);
        let new_disk = MemDisk::from_image(image);
        let old_ret = old_mkdir_at(&old_disk, parent, name, mode);
        let new_ret = core_mkdir_at(&new_disk, parent, name, mode);
        let new_dir_ino = match (old_ret, new_ret) {
            (Ok(o), Ok(n)) => {
                assert_eq!(o, n, "mkdir '{name}' inode mismatch: old {o} new {n}");
                Some(o)
            }
            (Err(o), Err(n)) => {
                assert_eq!(o, n, "mkdir '{name}' errno mismatch: old {o:?} new {n:?}");
                None
            }
            (o, n) => panic!("mkdir '{name}' ok/err divergence: old={o:?} new={n:?}"),
        };
        let sb = read_sb(&new_disk);
        // 父目录新增项 + 新目录的 '.'/'..' 块都可能含 padding：归一两者。成功时才有新目录块——
        // 失败（EEXIST）时仅父目录可能变（实际未变）。
        mask_dirent_padding(&old_disk, &sb, parent);
        mask_dirent_padding(&new_disk, &sb, parent);
        if let Some(dino) = new_dir_ino {
            mask_dirent_padding(&old_disk, &sb, dino);
            mask_dirent_padding(&new_disk, &sb, dino);
        }
        assert_disk_eq(&old_disk, &new_disk);
        new_disk.backing().lock().clone()
    }

    /// unlink 差分一步：旧/新各 unlink，比 Ok-Err + 全盘（归一父目录 padding）。
    fn diff_unlink_step(image: &[u8], parent: u32, name: &str) -> Vec<u8> {
        let old_disk = MemDisk::from_image(image);
        let new_disk = MemDisk::from_image(image);
        let old_ret = old_unlink_at(&old_disk, parent, name);
        let new_ret = core_unlink_at(&new_disk, parent, name);
        match (old_ret, new_ret) {
            (Ok(()), Ok(())) => {}
            (Err(o), Err(n)) => assert_eq!(o, n, "unlink '{name}' errno mismatch: old {o:?} new {n:?}"),
            (o, n) => panic!("unlink '{name}' ok/err divergence: old={o:?} new={n:?}"),
        }
        let sb = read_sb(&new_disk);
        mask_dirent_padding(&old_disk, &sb, parent);
        mask_dirent_padding(&new_disk, &sb, parent);
        assert_disk_eq(&old_disk, &new_disk);
        new_disk.backing().lock().clone()
    }

    /// rmdir 差分一步：旧/新各 rmdir，比 Ok-Err + 全盘（归一父目录 padding）。
    fn diff_rmdir_step(image: &[u8], parent: u32, name: &str) -> Vec<u8> {
        let old_disk = MemDisk::from_image(image);
        let new_disk = MemDisk::from_image(image);
        let old_ret = old_rmdir_at(&old_disk, parent, name);
        let new_ret = core_rmdir_at(&new_disk, parent, name);
        match (old_ret, new_ret) {
            (Ok(()), Ok(())) => {}
            (Err(o), Err(n)) => assert_eq!(o, n, "rmdir '{name}' errno mismatch: old {o:?} new {n:?}"),
            (o, n) => panic!("rmdir '{name}' ok/err divergence: old={o:?} new={n:?}"),
        }
        let sb = read_sb(&new_disk);
        mask_dirent_padding(&old_disk, &sb, parent);
        mask_dirent_padding(&new_disk, &sb, parent);
        assert_disk_eq(&old_disk, &new_disk);
        new_disk.backing().lock().clone()
    }

    /// create 文件差分：在根目录建一个常规文件，对拍返回 inode 号 + 全盘（新 inode 表项含
    /// mode/links=1/extent header/csum、父 dir 块新增项、ialloc 位图/SB/GDT）。
    #[ktest]
    fn dir_create_file_parity() {
        let _ = diff_create_step(EXT4_IMAGE, 2, "f1", 0o100644);
        // 连续建多个文件（每步全盘对拍，逼 inode 号递增 + 父目录连续切槽）。
        let mut img = EXT4_IMAGE.to_vec();
        img = diff_create_step(&img, 2, "a", 0o100644);
        img = diff_create_step(&img, 2, "bb", 0o100600);
        let _ = diff_create_step(&img, 2, "ccc", 0o100755);
    }

    /// mkdir 差分：建子目录（新目录 inode links=2 + '.'/'..' 块 + 父 nlink++ + 位图/SB）；
    /// 重复 mkdir → EEXIST；嵌套 mkdir d1 后 mkdir d1/d2。
    #[ktest]
    fn dir_mkdir_parity() {
        // 单个 mkdir。
        let img = diff_mkdir_step(EXT4_IMAGE, 2, "d1", 0o40755);

        // 重复 mkdir "d1" → 两侧 EEXIST（在已建 d1 的盘上）。
        let old_disk = MemDisk::from_image(&img);
        let new_disk = MemDisk::from_image(&img);
        let o = old_mkdir_at(&old_disk, 2, "d1", 0o40755);
        let n = core_mkdir_at(&new_disk, 2, "d1", 0o40755);
        match (o, n) {
            (Err(eo), Err(en)) => assert_eq!(eo, en, "duplicate mkdir errno mismatch"),
            other => panic!("duplicate mkdir must be EEXIST on both; got {other:?}"),
        }
        assert_eq!(o, Err(ext4_rs::Errno::EEXIST), "duplicate mkdir → EEXIST");
        // 失败路径不改盘（两侧都未动）→ 全盘一致。
        assert_disk_eq(&old_disk, &new_disk);

        // 嵌套：在 d1 下 mkdir d2。先取 d1 的 inode 号。
        let d1_ino = old_lookup(&MemDisk::from_image(&img), 2, "d1").expect("d1 present");
        let old_disk = MemDisk::from_image(&img);
        let new_disk = MemDisk::from_image(&img);
        let o = old_mkdir_at(&old_disk, d1_ino, "d2", 0o40755);
        let n = core_mkdir_at(&new_disk, d1_ino, "d2", 0o40755);
        match (o, n) {
            (Ok(oi), Ok(ni)) => assert_eq!(oi, ni, "nested mkdir inode mismatch"),
            other => panic!("nested mkdir must succeed on both; got {other:?}"),
        }
        let sb = read_sb(&new_disk);
        // 触及 d1（父，nlink++ + 新项）与 d2（新目录 '.'/'..'）。
        mask_dirent_padding(&old_disk, &sb, d1_ino);
        mask_dirent_padding(&new_disk, &sb, d1_ino);
        let d2_ino = o.expect("d2 inode");
        mask_dirent_padding(&old_disk, &sb, d2_ino);
        mask_dirent_padding(&new_disk, &sb, d2_ino);
        assert_disk_eq(&old_disk, &new_disk);
    }

    /// unlink 文件差分（含 BUG-19 验证）：create f1 后 unlink；对拍全盘（项删 + 子 nlink 调整；
    /// **inode 位图仍占用、数据块未释放** = BUG-19）。再 unlink 目录 → EISDIR。
    #[ktest]
    fn dir_unlink_file_parity() {
        // 建一个文件 f1（用 ext4_rs builder，两侧从同字节起步）。
        let (img, inos) = build_files(&["f1"]);
        let f1_ino = inos[0];

        // unlink f1：全盘对拍（BUG-19：core 与 ext4_rs 都不 free inode / 不截块）。
        let after = diff_unlink_step(&img, 2, "f1");

        // BUG-19 显式验证：unlink 后 f1 的 inode 位图位仍置位（未释放）。
        let disk = MemDisk::from_image(&after);
        let sb = read_sb(&disk);
        assert!(
            inode_bitmap_bit_set(&disk, &sb, f1_ino),
            "BUG-19: unlinked file inode bitmap bit must remain set (not freed)"
        );
        // f1 inode 仍可加载、links_count 已调整为 0（原 1 → 0）。
        let f1 = crate::fs::ext4::core::inode::load_inode(&disk, &sb, f1_ino).expect("load f1");
        assert_eq!(f1.raw.links_count(), 0, "unlinked file nlink == 0");

        // unlink 一个目录 → EISDIR（两侧）。建 d1，unlink d1。
        let dimg = build_dir_populated_image(&[DirOp::Mkdir {
            parent: 2,
            name: "d1",
            mode: 0o40755,
        }]);
        let old_disk = MemDisk::from_image(&dimg);
        let new_disk = MemDisk::from_image(&dimg);
        let o = old_unlink_at(&old_disk, 2, "d1");
        let n = core_unlink_at(&new_disk, 2, "d1");
        match (o, n) {
            (Err(eo), Err(en)) => assert_eq!(eo, en, "unlink dir errno mismatch"),
            other => panic!("unlink dir must be EISDIR on both; got {other:?}"),
        }
        assert_eq!(o, Err(ext4_rs::Errno::EISDIR), "unlink dir → EISDIR");
        assert_disk_eq(&old_disk, &new_disk);
    }

    /// rmdir 差分：mkdir d1 后 rmdir；对拍（父 nlink-1 + 子 nlink=0 + 子数据块释放 + 项删；
    /// 子 inode 位图仍占用）。+ 删非空目录 ENOTEMPTY + 删 '.'/'..' EINVAL + 删文件用 rmdir → ENOTDIR。
    #[ktest]
    fn dir_rmdir_parity() {
        // mkdir d1（空目录），再 rmdir d1。
        let img = build_dir_populated_image(&[DirOp::Mkdir {
            parent: 2,
            name: "d1",
            mode: 0o40755,
        }]);
        let d1_ino = old_lookup(&MemDisk::from_image(&img), 2, "d1").expect("d1 present");
        let after = diff_rmdir_step(&img, 2, "d1");
        // 子 inode 位图仍占用（BUG-19：rmdir 不 free inode）。
        let disk = MemDisk::from_image(&after);
        let sb = read_sb(&disk);
        assert!(
            inode_bitmap_bit_set(&disk, &sb, d1_ino),
            "BUG-19: rmdir'd dir inode bitmap bit must remain set (not freed)"
        );
        let d1 = crate::fs::ext4::core::inode::load_inode(&disk, &sb, d1_ino).expect("load d1");
        assert_eq!(d1.raw.links_count(), 0, "rmdir'd dir nlink == 0");

        // 删非空目录 → ENOTEMPTY：在 d1 里建一个文件，再 rmdir d1。
        let img2 = build_dir_populated_image(&[
            DirOp::Mkdir {
                parent: 2,
                name: "full",
                mode: 0o40755,
            },
        ]);
        let full_ino = old_lookup(&MemDisk::from_image(&img2), 2, "full").expect("full present");
        // 在 full 下建一个文件（用 ext4_rs builder 续写同盘）。
        let img3 = {
            let disk = MemDisk::from_image(&img2);
            let ext4 = ext4_rs::Ext4::open(Arc::new(disk.clone()));
            ext4.ext4_create_at(full_ino, "inside", 0o100644)
                .expect("create inside full");
            disk.backing().lock().clone()
        };
        let old_disk = MemDisk::from_image(&img3);
        let new_disk = MemDisk::from_image(&img3);
        let o = old_rmdir_at(&old_disk, 2, "full");
        let n = core_rmdir_at(&new_disk, 2, "full");
        match (o, n) {
            (Err(eo), Err(en)) => assert_eq!(eo, en, "rmdir non-empty errno mismatch"),
            other => panic!("rmdir non-empty must be ENOTEMPTY on both; got {other:?}"),
        }
        assert_eq!(o, Err(ext4_rs::Errno::ENOTEMPTY), "rmdir non-empty → ENOTEMPTY");
        assert_disk_eq(&old_disk, &new_disk);

        // 删 '.' / '..' → EINVAL（两侧；用 d1 已删的盘或原镜像均可，取原镜像根目录）。
        for nm in [".", ".."] {
            let old_disk = MemDisk::from_image(EXT4_IMAGE);
            let new_disk = MemDisk::from_image(EXT4_IMAGE);
            let o = old_rmdir_at(&old_disk, 2, nm);
            let n = core_rmdir_at(&new_disk, 2, nm);
            match (o, n) {
                (Err(eo), Err(en)) => assert_eq!(eo, en, "rmdir '{nm}' errno mismatch"),
                other => panic!("rmdir '{nm}' must be EINVAL on both; got {other:?}"),
            }
            assert_eq!(o, Err(ext4_rs::Errno::EINVAL), "rmdir '{nm}' → EINVAL");
            assert_disk_eq(&old_disk, &new_disk);
        }

        // 删文件用 rmdir → ENOTDIR：建 f1，rmdir f1。
        let (fimg, _) = build_files(&["f1"]);
        let old_disk = MemDisk::from_image(&fimg);
        let new_disk = MemDisk::from_image(&fimg);
        let o = old_rmdir_at(&old_disk, 2, "f1");
        let n = core_rmdir_at(&new_disk, 2, "f1");
        match (o, n) {
            (Err(eo), Err(en)) => assert_eq!(eo, en, "rmdir file errno mismatch"),
            other => panic!("rmdir file must be ENOTDIR on both; got {other:?}"),
        }
        assert_eq!(o, Err(ext4_rs::Errno::ENOTDIR), "rmdir file → ENOTDIR");
        assert_disk_eq(&old_disk, &new_disk);
    }

    /// lookup_at 差分：core `lookup_at`（命中 Ok(inode)、未命中 ENOENT）对拍 ext4_rs。
    /// 复用 Task-1 的 `diff_lookup`，但新侧改走 core `lookup_at`（Task-3 的 namespace lookup）。
    #[ktest]
    fn dir_lookup_at_parity() {
        let core_lookup_at =
            |disk: &MemDisk, parent: u32, name: &str| -> core::result::Result<u32, ext4_rs::Errno> {
                let sb = read_sb(disk);
                let ctx = ReadCtx::new(disk, &sb);
                let parent_inode = match crate::fs::ext4::core::inode::load_inode(disk, &sb, parent) {
                    Ok(i) => i,
                    Err(_) => return Err(ext4_rs::Errno::ENOENT),
                };
                match lookup_at(&ctx, &parent_inode, name.as_bytes()) {
                    Ok(ino) => Ok(ino),
                    Err(e) => Err(map_ns_errno(e.error())),
                }
            };
        diff_lookup(EXT4_IMAGE, 2, "lost+found", core_lookup_at);
        diff_lookup(EXT4_IMAGE, 2, "no-such", core_lookup_at);
    }

    /// create_unchecked / mkdir_unchecked + rmdir_at_fast 差分：core unchecked 接口与 checked
    /// 接口落盘一致（unchecked 只动末块、返回 abs offset），且 rmdir_at_fast 按偏移删 == rmdir_at。
    #[ktest]
    fn dir_unchecked_and_fast_parity() {
        // create_unchecked f1 vs ext4_rs ext4_create_at f1：最终盘字节应一致（根目录末块 try_insert）。
        let old_disk = MemDisk::from_image(EXT4_IMAGE);
        let new_disk = MemDisk::from_image(EXT4_IMAGE);
        let old_ret = old_create_at(&old_disk, 2, "uf1", 0o100644);
        let new_ret = {
            let bs = read_sb(&new_disk).block_size();
            let writer = DirectMetadataWriter::new(new_disk.clone(), bs);
            let mut nctx = make_nctx(&new_disk, &writer);
            create_unchecked_at(&mut nctx, 2, b"uf1", 0o100644)
                .map(|(ino, _off)| ino)
                .map_err(|e| map_ns_errno(e.error()))
        };
        match (old_ret, new_ret) {
            (Ok(o), Ok(n)) => assert_eq!(o, n, "create_unchecked inode mismatch"),
            other => panic!("create_unchecked must succeed on both; got {other:?}"),
        }
        let sb = read_sb(&new_disk);
        mask_dirent_padding(&old_disk, &sb, 2);
        mask_dirent_padding(&new_disk, &sb, 2);
        assert_disk_eq(&old_disk, &new_disk);

        // mkdir_unchecked d1 vs ext4_rs ext4_mkdir_at d1：对拍。
        let old_disk = MemDisk::from_image(EXT4_IMAGE);
        let new_disk = MemDisk::from_image(EXT4_IMAGE);
        let old_ret = old_mkdir_at(&old_disk, 2, "ud1", 0o40755);
        let (new_ino, dir_off) = {
            let bs = read_sb(&new_disk).block_size();
            let writer = DirectMetadataWriter::new(new_disk.clone(), bs);
            let mut nctx = make_nctx(&new_disk, &writer);
            mkdir_unchecked_at(&mut nctx, 2, b"ud1", 0o40755).expect("mkdir_unchecked")
        };
        assert_eq!(old_ret, Ok(new_ino), "mkdir_unchecked inode mismatch");
        let sb = read_sb(&new_disk);
        mask_dirent_padding(&old_disk, &sb, 2);
        mask_dirent_padding(&new_disk, &sb, 2);
        mask_dirent_padding(&old_disk, &sb, new_ino);
        mask_dirent_padding(&new_disk, &sb, new_ino);
        assert_disk_eq(&old_disk, &new_disk);

        // rmdir_at_fast(parent, child, dir_off) vs ext4_rs ext4_rmdir_at: 在 mkdir_unchecked 后的
        // new_disk 上跑 core rmdir_at_fast；旧侧用 ext4_rmdir_at 在 old_disk（同状态）上删 ud1。
        let fast_ret = {
            let bs = sb.block_size();
            let writer = DirectMetadataWriter::new(new_disk.clone(), bs);
            let mut nctx = make_nctx(&new_disk, &writer);
            rmdir_at_fast(&mut nctx, 2, new_ino, dir_off).map_err(|e| map_ns_errno(e.error()))
        };
        let old_rm = old_rmdir_at(&old_disk, 2, "ud1");
        match (old_rm, fast_ret) {
            (Ok(()), Ok(())) => {}
            other => panic!("rmdir_at_fast vs ext4_rmdir_at divergence: {other:?}"),
        }
        let sb2 = read_sb(&new_disk);
        mask_dirent_padding(&old_disk, &sb2, 2);
        mask_dirent_padding(&new_disk, &sb2, 2);
        assert_disk_eq(&old_disk, &new_disk);
    }

    // =================================================================
    // Phase 4 Task 4：rename 差分（仅同目录——BUG-20）。
    //
    // 差分驱动 `diff_rename_step`：两张独立 `MemDisk`（同初始字节），旧侧 ext4_rs
    // `ext4_rename_at`、新侧 core `rename_at`，比 Ok-Err + `assert_disk_eq` 全盘逐字节
    // （先对触及的目录 `mask_dirent_padding` 归一 BUG-21 padding——rename 写新目录项携 padding 泄漏）。
    // 全程 `match (old, new)`，**禁 `.expect()`**：旧侧可能返回 EXDEV/EISDIR/ENOTDIR/ENOTEMPTY。
    // =================================================================

    /// 同目录改名（dest 不存在）：rename f1 → f2。对拍全盘（old 项 inode=0 标删 + 前驱合并 +
    /// 新项 f2 指向同 inode + **末尾不显式写回父**的字节态）+ 返回 Ok。
    #[ktest]
    fn dir_rename_same_dir_parity() {
        let (img, _inos) = build_files(&["f1"]);
        // rename f1 → f2（同目录、dest 不存在）。触及目录：根（ino 2）。
        let _ = diff_rename_step(&img, 2, "f1", 2, "f2", &[2]);
    }

    /// 覆盖文件：f1、f2 都存在 → rename f1 → f2 覆盖 f2。
    /// PARITY：文件覆盖路径 `unlink`(文件分支：仅 write_back 子) → **不** write_back(父)。
    #[ktest]
    fn dir_rename_overwrite_file_parity() {
        let (img, _inos) = build_files(&["f1", "f2"]);
        // rename f1 → f2：先 unlink(f2)（文件分支，不写回父）+ 删 f1 项 + 加 f2 项指向 f1 inode。
        let _ = diff_rename_step(&img, 2, "f1", 2, "f2", &[2]);
    }

    /// 覆盖空目录：d1、d2 都是空目录 → rename d1 → d2 覆盖 d2。
    /// PARITY：目录覆盖路径 `truncate_inode(d2, 0)` + `unlink`(目录分支：父 nlink-1 + 子 nlink=0 +
    /// write_back 子 + write_back 父) + **再显式 write_back(父)**。
    #[ktest]
    fn dir_rename_overwrite_empty_dir_parity() {
        let img = build_dir_populated_image(&[
            DirOp::Mkdir {
                parent: 2,
                name: "d1",
                mode: 0o40755,
            },
            DirOp::Mkdir {
                parent: 2,
                name: "d2",
                mode: 0o40755,
            },
        ]);
        // 触及目录：根（ino 2）。d2 被 truncate 到 0（数据块释放），其目录块不再可达 → 不归一 d2、
        // 不归一 d1（d1 内部 '.'/'..' 块未变、且 rename 不重写 d1 自身块——只改根目录项与 d2 inode/块）。
        let _ = diff_rename_step(&img, 2, "d1", 2, "d2", &[2]);
    }

    /// 覆盖非空目录 → ENOTEMPTY：d1 空、d2 含一项 → rename d1 → d2 在 dir_has_entry 判到 d2 非空。
    #[ktest]
    fn dir_rename_overwrite_nonempty_dir_ENOTEMPTY() {
        let img = build_dir_populated_image(&[
            DirOp::Mkdir {
                parent: 2,
                name: "d1",
                mode: 0o40755,
            },
            DirOp::Mkdir {
                parent: 2,
                name: "d2",
                mode: 0o40755,
            },
        ]);
        let d2_ino = old_lookup(&MemDisk::from_image(&img), 2, "d2").expect("d2 present");
        // 在 d2 里建一项使其非空。
        let img2 = {
            let disk = MemDisk::from_image(&img);
            let ext4 = ext4_rs::Ext4::open(Arc::new(disk.clone()));
            ext4.ext4_create_at(d2_ino, "inside", 0o100644)
                .expect("create inside d2");
            disk.backing().lock().clone()
        };
        let old_disk = MemDisk::from_image(&img2);
        let new_disk = MemDisk::from_image(&img2);
        let o = old_rename_at(&old_disk, 2, "d1", 2, "d2");
        let n = core_rename_at(&new_disk, 2, "d1", 2, "d2");
        match (o, n) {
            (Err(eo), Err(en)) => assert_eq!(eo, en, "rename overwrite non-empty errno mismatch"),
            other => panic!("rename overwrite non-empty must be ENOTEMPTY on both; got {other:?}"),
        }
        assert_eq!(
            o,
            Err(ext4_rs::Errno::ENOTEMPTY),
            "rename overwrite non-empty dir → ENOTEMPTY"
        );
        // 失败路径不改盘（两侧都未动）→ 全盘一致。
        assert_disk_eq(&old_disk, &new_disk);
    }

    /// 类型不符：dir → 已存在文件 = ENOTDIR；file → 已存在目录 = EISDIR。
    #[ktest]
    fn dir_rename_type_mismatch() {
        // (a) dir → file：mkdir d1 + create f2 → rename d1 → f2（old 是目录、dest 是文件）= ENOTDIR。
        let img = build_dir_populated_image(&[
            DirOp::Mkdir {
                parent: 2,
                name: "d1",
                mode: 0o40755,
            },
            DirOp::Create {
                parent: 2,
                name: "f2",
                mode: 0o100644,
            },
        ]);
        let old_disk = MemDisk::from_image(&img);
        let new_disk = MemDisk::from_image(&img);
        let o = old_rename_at(&old_disk, 2, "d1", 2, "f2");
        let n = core_rename_at(&new_disk, 2, "d1", 2, "f2");
        match (o, n) {
            (Err(eo), Err(en)) => assert_eq!(eo, en, "rename dir->file errno mismatch"),
            other => panic!("rename dir->file must be ENOTDIR on both; got {other:?}"),
        }
        assert_eq!(o, Err(ext4_rs::Errno::ENOTDIR), "rename dir over file → ENOTDIR");
        assert_disk_eq(&old_disk, &new_disk);

        // (b) file → dir：create f1 + mkdir d2 → rename f1 → d2（old 是文件、dest 是目录）= EISDIR。
        let img2 = build_dir_populated_image(&[
            DirOp::Create {
                parent: 2,
                name: "f1",
                mode: 0o100644,
            },
            DirOp::Mkdir {
                parent: 2,
                name: "d2",
                mode: 0o40755,
            },
        ]);
        let old_disk = MemDisk::from_image(&img2);
        let new_disk = MemDisk::from_image(&img2);
        let o = old_rename_at(&old_disk, 2, "f1", 2, "d2");
        let n = core_rename_at(&new_disk, 2, "f1", 2, "d2");
        match (o, n) {
            (Err(eo), Err(en)) => assert_eq!(eo, en, "rename file->dir errno mismatch"),
            other => panic!("rename file->dir must be EISDIR on both; got {other:?}"),
        }
        assert_eq!(o, Err(ext4_rs::Errno::EISDIR), "rename file over dir → EISDIR");
        assert_disk_eq(&old_disk, &new_disk);
    }

    /// '.' / '..' 拒绝 → EISDIR：old 或 new 任一为 "." / ".."。
    #[ktest]
    fn dir_rename_dot_EISDIR() {
        let (img, _inos) = build_files(&["f1"]);
        // 四种位置都验：old="." / old=".." / new="." / new=".."。
        for (on, nn) in [(".", "f1"), ("..", "f1"), ("f1", "."), ("f1", "..")] {
            let old_disk = MemDisk::from_image(&img);
            let new_disk = MemDisk::from_image(&img);
            let o = old_rename_at(&old_disk, 2, on, 2, nn);
            let n = core_rename_at(&new_disk, 2, on, 2, nn);
            match (o, n) {
                (Err(eo), Err(en)) => assert_eq!(eo, en, "rename '{on}'->'{nn}' errno mismatch"),
                other => panic!("rename '{on}'->'{nn}' must be EISDIR on both; got {other:?}"),
            }
            assert_eq!(
                o,
                Err(ext4_rs::Errno::EISDIR),
                "rename with '.'/'..' → EISDIR"
            );
            assert_disk_eq(&old_disk, &new_disk);
        }
    }

    /// 跨目录 → EXDEV（BUG-20：仅同目录 rename，无 '..' 重定父）。
    #[ktest]
    fn dir_rename_cross_dir_EXDEV() {
        // 建子目录 d1 + 在根建文件 f1 → rename(root, f1, d1, f1moved)：old_parent(2) != new_parent(d1)。
        let img = build_dir_populated_image(&[
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
        let d1_ino = old_lookup(&MemDisk::from_image(&img), 2, "d1").expect("d1 present");
        let old_disk = MemDisk::from_image(&img);
        let new_disk = MemDisk::from_image(&img);
        let o = old_rename_at(&old_disk, 2, "f1", d1_ino, "f1moved");
        let n = core_rename_at(&new_disk, 2, "f1", d1_ino, "f1moved");
        match (o, n) {
            (Err(eo), Err(en)) => assert_eq!(eo, en, "cross-dir rename errno mismatch"),
            other => panic!("cross-dir rename must be EXDEV on both; got {other:?}"),
        }
        assert_eq!(o, Err(ext4_rs::Errno::EXDEV), "cross-directory rename → EXDEV");
        // EXDEV 在任何盘改动前返回 → 两侧未动 → 全盘一致。
        assert_disk_eq(&old_disk, &new_disk);
    }

    /// 同名短路：old_name == new_name → Ok（无盘改动）。
    #[ktest]
    fn dir_rename_same_name_noop() {
        let (img, _inos) = build_files(&["f1"]);
        let old_disk = MemDisk::from_image(&img);
        let new_disk = MemDisk::from_image(&img);
        let o = old_rename_at(&old_disk, 2, "f1", 2, "f1");
        let n = core_rename_at(&new_disk, 2, "f1", 2, "f1");
        match (o, n) {
            (Ok(()), Ok(())) => {}
            other => panic!("same-name rename must be Ok no-op on both; got {other:?}"),
        }
        // 同名短路在比较 old/new 后立即 return Ok，不动盘 → 全盘一致（也与起始 img 一致）。
        assert_disk_eq(&old_disk, &new_disk);
    }

    /// 同 inode 短路：dest 已存在且指向 old 同一 inode → Ok（无盘改动）。
    ///
    /// 构造法：用硬链接让两个名字指向同一 inode。ext4_rs 的命名空间公开法无对外 link 接口，
    /// 但底层 `dir_add_entry(parent, &child_ref, name)` 可在 old_disk 上手工加第二个名字 f2 指向
    /// f1 的 inode（不改 nlink，仅加目录项），从而 lookup(2,"f1")==lookup(2,"f2")。rename f1→f2
    /// 触发 `new_ino == old_ino` 短路返回 Ok。
    #[ktest]
    fn dir_rename_same_inode_noop() {
        let (img, inos) = build_files(&["f1"]);
        let f1_ino = inos[0];
        // 在同一目录里手工再加一项 f2 指向 f1 的 inode（经 ext4_rs 底层 dir_add_entry）。
        let img2 = {
            let disk = MemDisk::from_image(&img);
            let ext4 = ext4_rs::Ext4::open(Arc::new(disk.clone()));
            let mut parent_ref = ext4.get_inode_ref(2);
            let f1_ref = ext4.get_inode_ref(f1_ino);
            ext4.dir_add_entry(&mut parent_ref, &f1_ref, "f2")
                .expect("add second name f2 -> f1 inode");
            ext4.write_back_inode(&mut parent_ref);
            disk.backing().lock().clone()
        };
        // 确认 f1 与 f2 指向同一 inode。
        let probe = MemDisk::from_image(&img2);
        assert_eq!(
            old_lookup(&probe, 2, "f1"),
            old_lookup(&probe, 2, "f2"),
            "f1 and f2 must alias the same inode"
        );
        // rename f1 → f2：dest 存在且 new_ino == old_ino → Ok 短路（无盘改动）。
        let old_disk = MemDisk::from_image(&img2);
        let new_disk = MemDisk::from_image(&img2);
        let o = old_rename_at(&old_disk, 2, "f1", 2, "f2");
        let n = core_rename_at(&new_disk, 2, "f1", 2, "f2");
        match (o, n) {
            (Ok(()), Ok(())) => {}
            other => panic!("same-inode rename must be Ok no-op on both; got {other:?}"),
        }
        assert_disk_eq(&old_disk, &new_disk);
    }

    /// 读出 inode `ino` 在其组 inode 位图里的 bit 是否置位（BUG-19 验证用）。
    fn inode_bitmap_bit_set(disk: &MemDisk, sb: &RawSuperblock, ino: u32) -> bool {
        let bs = sb.block_size();
        let inodes_per_group = sb.inodes_per_group();
        let group = (ino - 1) / inodes_per_group;
        let index_in_group = ((ino - 1) % inodes_per_group) as usize;

        let gdt_off = (sb.first_data_block as usize + 1) * bs;
        let desc_size = sb.group_desc_size();
        let mut desc_buf = [0u8; 64];
        let take = core::cmp::min(desc_size, 64);
        let mut raw = vec![0u8; desc_size];
        disk.read_at(gdt_off + group as usize * desc_size, raw.as_mut_slice());
        desc_buf[..take].copy_from_slice(&raw[..take]);
        let desc = RawGroupDescriptor::from_bytes(&desc_buf);

        let bmp_off = desc.inode_bitmap() as usize * bs;
        let byte_idx = index_in_group / 8;
        let bit_idx = index_in_group % 8;
        let mut byte = [0u8; 1];
        disk.read_at(bmp_off + byte_idx, &mut byte);
        (byte[0] >> bit_idx) & 1 == 1
    }

    // =================================================================
    // Phase 4 Task 5：边角差分（长目录/多块、csum 开关、多组镜像、损坏块防御）。
    //
    // 复用 Task 0–4 全部差分驱动：`diff_create_step`/`diff_mkdir_step`/`diff_unlink_step`
    // （内部已 `match (old, new)` Ok/Err + `mask_dirent_padding` + `assert_disk_eq`），叠
    // `EXT4_NOCSUM_IMAGE`/`EXT4_MULTIGROUP_IMAGE` 两张几何镜像；外加 `dir_get_entries`
    // 损坏块「silent break 不 panic」单侧 core 鲁棒性（损坏输入**不**喂给 ext4_rs 的 unsafe
    // 路径，故不做两侧差分——同 Phase 3 教训）。
    // =================================================================

    /// 在 ext4_rs 上把 `parent` 下塞满 `n` 个短名文件、逼 ≥3 个目录块，返回结果盘字节。
    /// 全程用 ext4_rs builder（两侧后续从同一份字节起步对拍）。任一步失败即 panic（fixture
    /// 构造失败属测试用法错误）。
    fn build_long_dir_image(parent: u32, n: usize) -> Vec<u8> {
        let disk = MemDisk::from_image(EXT4_IMAGE);
        let ext4 = ext4_rs::Ext4::open(Arc::new(disk.clone()));
        for i in 0..n {
            let name = format!("ent_{i:05}");
            ext4.ext4_create_at(parent, &name, 0o100644)
                .unwrap_or_else(|e| panic!("builder create '{name}' failed: {e:?}"));
        }
        disk.backing().lock().clone()
    }

    /// 长目录 / 多块差分：先用 ext4_rs 在根下建足够多文件逼 ≥3 个目录块，确认确实跨 ≥3 块
    /// （旧侧 readdir 的 next_offset 落在块 ≥2），然后跑 create→lookup→readdir→unlink 序列，
    /// 每步 core vs ext4_rs 全盘逐字节对拍。
    ///
    /// **计数从 block_size 推导（不硬编一个偏低的猜测）**：ext4_rs `dir_add_entry` 的 fast-path
    /// 在末块剩余空间 `< required_len = align4(264 + name.len())` 时才 append 新块——故每块实际
    /// 容量受 264-下界影响，4K 块实测约容 ~300 短名项（控制器实测）。取每块容量保守估计
    /// `cap = (bs - 12) / align4(8 + name_len)`，文件数 `4*cap + 16` 给足余量，**可靠**跨 ≥3 块。
    ///
    /// 这同时演练**多块目录块 csum 的 `ino_index = block[0].inode` quirk**（块 ≥1 的块首项是
    /// 普通子文件项 → ino_index 取子项 inode，偏离 ext4 规范；见 bug.md D 段 / dir.rs
    /// `dir_block_csum` PARITY 注）：两引擎对块 ≥1 用**同一** ino_index 公式算 csum，故
    /// `assert_disk_eq` 全盘绿即确认 core 逐字节复刻了该 quirk。控制器据此在 milestone/bug.md
    /// 登记「多块 csum 偏离规范、parity 保住」结论。
    #[ktest]
    fn dir_long_multiblock_parity() {
        // 从 block_size 推导文件数，**可靠**逼 ≥3 个目录块。名字 `ent_NNNNN` 9 字符，
        // 存储 rec_len = align4(8+9)=20；保守每块容量估 (bs-12)/20，文件数取 4*cap+16 给足余量。
        let bs = read_sb(&MemDisk::from_image(EXT4_IMAGE)).block_size();
        let name_slot = {
            let l = 8 + "ent_00000".len();
            (l + 3) & !3
        };
        let cap_per_block = (bs - 12) / name_slot;
        let n_files = cap_per_block * 4 + 16; // 4K 块约 800+，远超 3 块所需
        let img = build_long_dir_image(2, n_files);

        // 佐证确实 ≥3 块：旧侧底层枚举的 next_offset 至少有一项落在块 ≥2（>= 3*bs 即第 3 块内）。
        let probe_disk = MemDisk::from_image(&img);
        let sb = read_sb(&probe_disk);
        let bs = sb.block_size();
        let with_off = old_readdir_with_next_offset(&probe_disk, 2);
        assert!(
            with_off.iter().any(|(_, _, _, off)| *off >= 3 * bs),
            "at least one entry must land in dir block >=2 (3rd block reached); \
             n_files={n_files} entries={} max_next_off={}",
            with_off.len(),
            with_off.iter().map(|(_, _, _, o)| *o).max().unwrap_or(0)
        );

        // 命中目标从实际计数推导（确保存在且落在高块）：near-end / middle 各取一个。
        let near_end = format!("ent_{:05}", n_files - 1);
        let middle = format!("ent_{:05}", n_files / 2);

        // ---- 序列：create 一个新文件（在已多块的根里继续切槽/可能再 append）。----
        let img = diff_create_step(&img, 2, "tail_new", 0o100644);

        // ---- lookup：命中块 >=1 上的项（强制走跨块 dir_find_entry）+ 未命中。----
        diff_lookup(&img, 2, &near_end, core_lookup);
        diff_lookup(&img, 2, &middle, core_lookup);
        diff_lookup(&img, 2, "no-such-ent", core_lookup);

        // ---- readdir：跨 ≥3 块逐元素 (name, ino, type, next_offset) 对拍。----
        diff_readdir(&img, 2, core_readdir_next_offset);

        // ---- unlink：删一个块 >=1 上的中间项（前驱合并路径，跨块） + 删新建的项。----
        let img = diff_unlink_step(&img, 2, &middle);
        let _ = diff_unlink_step(&img, 2, "tail_new");
    }

    /// csum 开关差分（`EXT4_NOCSUM_IMAGE`，metadata_csum 关）：在 csum-off 镜像上跑
    /// create→mkdir→unlink 序列，每步 core vs ext4_rs 全盘逐字节对拍。
    ///
    /// **BUG-22 parity**：ext4_rs 的写侧 `dir_set_csum` **无 metadata_csum 门控**——即便文件系统
    /// 关了 metadata_csum，ext4_rs 仍**无条件**把 dir-block tail.csum 写进盘（ext4_impls/dir.rs:252，
    /// 7 个 call site 全无门控）。core 的写侧 `dir_set_csum` 据此**也已去掉门控、无条件写**（逐字
    /// 复刻）。故本测在 NOCSUM 镜像上 `assert_disk_eq` 全盘绿，正是确认 core 复刻了这个 spec-违反
    /// 的无条件写行为（读侧 `dir_verify_block_csum` 仍门控，与 ext4_rs 不验 dir csum 的读侧对称）。
    #[ktest]
    fn dir_nocsum_parity() {
        // 探针：确认这张镜像确实 metadata_csum 关。
        let probe = MemDisk::from_image(EXT4_NOCSUM_IMAGE);
        let sb = read_sb(&probe);
        assert!(
            (sb.features_read_only() & 0x400) == 0,
            "EXT4_NOCSUM_IMAGE must have metadata_csum off"
        );

        // create f1 → 全盘对拍（core 写父目录块时门控不写 tail csum，与 ext4_rs 一致）。
        let img = diff_create_step(EXT4_NOCSUM_IMAGE, 2, "ncf1", 0o100644);
        // mkdir d1 → 全盘对拍（新目录 '.'/'..' 块同样门控不写 csum）。
        let img = diff_mkdir_step(&img, 2, "ncd1", 0o40755);
        // unlink f1 → 全盘对拍。
        let _ = diff_unlink_step(&img, 2, "ncf1");
    }

    /// 多组镜像差分（`EXT4_MULTIGROUP_IMAGE`：1K 块、8 组、first_data_block=1、64bit、
    /// metadata_csum）：在跨组几何上跑命名空间序列（新 inode / 新目录数据块可能落在非 0 组）。
    /// 每步 core vs ext4_rs 全盘逐字节对拍——覆盖 first_data_block!=0 的布局推导 + 跨组分配。
    #[ktest]
    fn dir_multigroup_parity() {
        // 探针：确认多组几何（first_data_block != 0）。
        let probe = MemDisk::from_image(EXT4_MULTIGROUP_IMAGE);
        let sb = read_sb(&probe);
        assert!(
            sb.first_data_block != 0,
            "EXT4_MULTIGROUP_IMAGE must have first_data_block != 0 (1K-block geometry)"
        );

        // create 一串文件（新 inode 跨组分配） + mkdir（新目录数据块跨组分配），逐步全盘对拍。
        let img = diff_create_step(EXT4_MULTIGROUP_IMAGE, 2, "mgf1", 0o100644);
        let img = diff_create_step(&img, 2, "mgf2", 0o100644);
        let img = diff_mkdir_step(&img, 2, "mgd1", 0o40755);
        // 嵌套：在 mgd1 下再 create（强制在新分配的目录里写项）。
        let mgd1_ino = old_lookup(&MemDisk::from_image(&img), 2, "mgd1").expect("mgd1 present");
        let _ = diff_create_step(&img, mgd1_ino, "inner", 0o100644);
    }

    /// 损坏块防御（**单侧 core 鲁棒性**，不喂 ext4_rs unsafe 路径）：手工构造坏 `rec_len` 的
    /// 目录块缓冲，断言 core 的**枚举路径** `dir_get_entries` **silent break 且不 panic**（与块
    /// 级 `dir_find_in_block`/`parse_entry` 的 EIO 防御互补——后者在 `dir.rs` 的 ktest
    /// `dir_corrupted_block_defense` 里验）。
    ///
    /// 构造法：取 `EXT4_NOCSUM_IMAGE`（门控关，改块无需重算 csum）根目录首块，把首项的
    /// `rec_len` 改成 0（坏），整块写回，再 core `dir_get_entries(root)`——必须返回（可能空/截断）
    /// 而**不 panic**。NEITHER 引擎被喂这块（仅 core 单侧），故无两侧 `assert_disk_eq`。
    #[ktest]
    fn dir_get_entries_corrupted_silent_break() {
        use crate::fs::ext4::core::dir::dir_get_entries;
        use crate::fs::ext4::core::extents::get_pblock_idx_state;
        use crate::fs::ext4::core::file::ReadCtx;
        use crate::fs::ext4::core::inode::load_inode;

        let disk = MemDisk::from_image(EXT4_NOCSUM_IMAGE);
        let sb = read_sb(&disk);
        let bs = sb.block_size();
        let root = load_inode(&disk, &sb, 2).expect("load root");

        // 定位根目录首块物理块号，读出整块，把首项 rec_len 置 0（坏），整块写回。
        let pblock = match get_pblock_idx_state(&disk, &sb, &root, 0) {
            Ok(Some((p, _))) => p,
            other => panic!("root dir block 0 must map; got {other:?}"),
        };
        let base = pblock as usize * bs;
        {
            let backing = disk.backing();
            let mut guard = backing.lock();
            // 首项 rec_len 在块内 [4..6]（u16 le）。置 0 → dir_get_entries 应 silent break。
            guard[base + 4] = 0;
            guard[base + 5] = 0;
        }

        // 关键断言：core 枚举**不 panic**（坏 rec_len → silent break），返回一个向量。
        let ctx = ReadCtx::new(&disk, &sb);
        let entries = dir_get_entries(&ctx, &root);
        // 首项 rec_len=0 立即 break 本块 → 该块无项收集（根仅 1 块时即空）；只要没 panic 即达标。
        assert!(
            entries.is_empty() || entries.iter().all(|e| !e.name.is_empty()),
            "corrupted-block enumeration must not panic and must yield only well-formed entries"
        );
    }

    // =================================================================
    // Phase 5 Task 0：JBD2 日志差分地基 ktest。
    //
    // 注：新-vs-旧的 `journal_sb_load_parity`（core 侧读 journal SB）推迟到 Task 1
    // （core::journal::superblock 实现后）。本 Task 只验证 harness 本身：
    // (a) journal 区可定位且几何合理；(b) 旧侧驱动能跑一个最小 commit、journal 区确实被写、
    //     journal SB s_sequence 推进——证明 harness 能驱动 ext4_rs JBD2。
    // =================================================================

    /// journal 区定位基线：`resolve_journal_area(EXT4_IMAGE)` 解出物理块向量 + 几何，
    /// 并直接从 journal 逻辑块 0（physical_blocks[0]）大端读出 journal 超级块，断言：
    /// magic == 0xC03B3998、s_blocksize == fs block_size、s_maxlen > 0、first 合理、物理块非空。
    #[ktest]
    fn journal_area_resolve_baseline() {
        use crate::fs::ext4::core::journal::format::{RawJournalSuperblock, JBD2_MAGIC};

        let disk = MemDisk::from_image(EXT4_IMAGE);
        let sb = read_sb(&disk);
        let fs_bs = sb.block_size() as u32;

        let (physical_blocks, geom) = resolve_journal_area(&disk);

        // 物理块向量非空，且块数与 maxlen 一致（journal inode 的逻辑块数 = maxlen）。
        assert!(!physical_blocks.is_empty(), "journal physical_blocks must be non-empty");
        assert!(geom.maxlen > 0, "journal s_maxlen must be > 0; got {}", geom.maxlen);
        assert_eq!(
            geom.block_size, fs_bs,
            "journal s_blocksize ({}) must equal fs block_size ({fs_bs})",
            geom.block_size
        );
        // first 是环第一个可用块；超级块占逻辑块 0，故 first 在 (0, maxlen) 内。
        assert!(
            geom.first > 0 && geom.first < geom.maxlen,
            "journal s_first ({}) must be in (0, maxlen={})",
            geom.first,
            geom.maxlen
        );

        // 直接大端读 journal 超级块（journal 逻辑块 0 = 第一个物理块），核对 magic。
        let bs = geom.block_size as usize;
        let mut sb_block = vec![0u8; bs];
        disk.read_at((physical_blocks[0] as usize) * bs, sb_block.as_mut_slice());
        let jsb = RawJournalSuperblock::from_bytes(&sb_block[..1024]);
        assert_eq!(
            jsb.header().magic(),
            JBD2_MAGIC,
            "journal SB magic must be 0xC03B3998 (big-endian at journal block 0)"
        );
        assert_eq!(jsb.blocksize(), geom.block_size, "RawJournalSuperblock blocksize == geom");
        assert_eq!(jsb.maxlen(), geom.maxlen, "RawJournalSuperblock maxlen == geom");
        assert_eq!(jsb.first(), geom.first, "RawJournalSuperblock first == geom");
        assert_eq!(jsb.sequence(), geom.sequence, "RawJournalSuperblock sequence == geom");
    }

    /// 旧侧驱动基线：用 ext4_rs 在 `EXT4_IMAGE` 克隆盘上跑一个最小事务（写 1 个元数据块镜像）
    /// 并 commit，断言 (a) journal 区字节**确实变了**（commit 写了 descriptor/payload/commit/SB）、
    /// (b) journal SB 的 s_sequence **推进了** 1（ext4_rs commit 写序第 ④ 步）。
    /// 证明 [`old_journal_commit`] 能驱动 ext4_rs JBD2——后续 Task 3 的 commit 差分靠它当旧基准。
    #[ktest]
    fn journal_old_driver_commit_advances() {
        let disk = MemDisk::from_image(EXT4_IMAGE);
        let (physical_blocks, geom_before) = resolve_journal_area(&disk);
        let bs = geom_before.block_size as usize;

        // commit 前的 journal 区快照 + 起始序号。
        let before = snapshot_journal_area(&disk, &physical_blocks, bs);
        let seq_before = geom_before.sequence;

        // 一个全块镜像（block_size 字节）的元数据写，记到 journal 逻辑块 1
        // （随便选的 home 块号——Task 0 不验 home 内容，只验 commit 落进 journal 区）。
        let mut image = vec![0u8; bs];
        for (i, b) in image.iter_mut().enumerate() {
            *b = (i & 0xFF) as u8;
        }
        let writes = [JournalMetaWrite { block_nr: 1, image }];

        let tid = old_journal_commit(&disk, &writes);
        assert_eq!(tid, seq_before, "commit tid must equal the SB sequence at commit time");

        // commit 后：journal 区字节应已改变（至少 descriptor + commit + SB 块被写）。
        let after = snapshot_journal_area(&disk, &physical_blocks, bs);
        assert_ne!(before, after, "journal area must change after a commit (commit wrote nothing?)");

        // journal SB s_sequence 推进了（write_commit_plan: update_sequence(seq+1)）。
        let (_, geom_after) = resolve_journal_area(&disk);
        assert_eq!(
            geom_after.sequence,
            seq_before.saturating_add(1),
            "journal SB s_sequence must advance by 1 after commit (before {seq_before}, after {})",
            geom_after.sequence
        );
        // s_start 在原本为空（start==0）时被置为 descriptor 块（commit 写序第 ④ 步）。
        if geom_before.start == 0 {
            assert_ne!(geom_after.start, 0, "empty journal: s_start must be set to the commit's start block");
        }
    }

    // =================================================================
    // Phase 5 Task 1：journal SB load + 4 类 csum 算法差分。
    //
    // - `journal_sb_load_parity`：旧侧 ext4_rs 读 journal SB（经 resolve_journal_area
    //   拿几何）vs 新侧 core `load_journal_sb` 读同一组物理块——对拍 magic/blocksize/
    //   maxlen/first/sequence/start。
    // - `journal_sb_csum_parity`：core `journal_sb_checksum` == 盘上 s_checksum（csum 开）；
    //   nocsum 镜像确认门控（gate 关时 SB s_checksum 不被校验）。
    // - `jbd2_csum_suite_parity`：descriptor-tail / per-tag data / commit 三类 csum 对拍
    //   ext4_rs（用其 public `ext4_crc32c` 跑同一字节序列——ext4_rs 私有 csum 方法不可调，
    //   故按其源码逐字复刻的字节序列 + 同 crc 引擎对拍；core crc 已在 crc.rs 与 ext4_rs 逐位对齐）。
    // =================================================================

    /// journal SB load 差分：旧侧 ext4_rs `JournalSuperblockState`（经 `resolve_journal_area`
    /// 的几何）vs 新侧 core `load_journal_sb`，对拍 magic/blocksize/maxlen/first/sequence/start。
    #[ktest]
    fn journal_sb_load_parity() {
        use crate::fs::ext4::core::journal::format::JBD2_MAGIC;
        use crate::fs::ext4::core::journal::superblock::load_journal_sb;

        for image in [EXT4_IMAGE, EXT4_MULTIGROUP_IMAGE, EXT4_NOCSUM_IMAGE] {
            let disk = MemDisk::from_image(image);
            // 旧侧：ext4_rs 解出物理块 + 几何（= ext4_rs JournalSuperblockState 读出的逻辑值）。
            let (physical_blocks, geom) = resolve_journal_area(&disk);
            let bs = geom.block_size as usize;

            // 新侧：core load_journal_sb 读同一组物理块的逻辑块 0。
            let sb = load_journal_sb(&disk, &physical_blocks, bs)
                .expect("core load_journal_sb on a valid fixture journal");

            assert_eq!(sb.header().magic(), JBD2_MAGIC, "core SB magic");
            assert_eq!(sb.blocksize(), geom.block_size, "s_blocksize: core vs ext4_rs geom");
            assert_eq!(sb.maxlen(), geom.maxlen, "s_maxlen: core vs ext4_rs geom");
            assert_eq!(sb.first(), geom.first, "s_first: core vs ext4_rs geom");
            assert_eq!(sb.sequence(), geom.sequence, "s_sequence: core vs ext4_rs geom");
            assert_eq!(sb.start(), geom.start, "s_start: core vs ext4_rs geom");
        }
    }

    /// journal SB csum 差分 + 门控：csum 开的镜像上 core `journal_sb_checksum` 必 == 盘上
    /// s_checksum（大端解码）；nocsum 镜像上确认 `has_checksum_v2_or_v3()` 关（门控生效，
    /// SB s_checksum 不参与校验）。
    #[ktest]
    fn journal_sb_csum_parity() {
        use crate::fs::ext4::core::journal::superblock::{journal_sb_checksum, load_journal_sb};

        // (a) csum 开的真镜像（EXT4_IMAGE / EXT4_MULTIGROUP_IMAGE）：core 算的 == 盘上 s_checksum。
        for image in [EXT4_IMAGE, EXT4_MULTIGROUP_IMAGE] {
            let disk = MemDisk::from_image(image);
            let (physical_blocks, geom) = resolve_journal_area(&disk);
            let bs = geom.block_size as usize;
            let sb = load_journal_sb(&disk, &physical_blocks, bs).expect("load SB (csum image)");

            // 仅当 SB 自身开启 CSUM_V2/V3 时校验盘上 s_checksum（与 ext4_rs validate 同门控）。
            if sb.has_checksum_v2_or_v3() {
                // 读 journal 逻辑块 0 的前 1024 字节作 SB 镜像，算 csum。
                let mut sb_block = vec![0u8; bs];
                disk.read_at((physical_blocks[0] as usize) * bs, sb_block.as_mut_slice());
                let mut sb_image = [0u8; 1024];
                sb_image.copy_from_slice(&sb_block[..1024]);
                let computed = journal_sb_checksum(&sb_image);
                assert_eq!(
                    computed,
                    sb.checksum(),
                    "core journal_sb_checksum must equal on-disk s_checksum (big-endian) for {image_name}",
                    image_name = if core::ptr::eq(image, EXT4_IMAGE) { "EXT4_IMAGE" } else { "EXT4_MULTIGROUP_IMAGE" },
                );
            }
        }

        // (b) nocsum 镜像：门控应关（has_checksum_v2_or_v3()==false）；s_checksum 不被校验。
        //     对拍 ext4_rs：旧侧 validate 在此门控下不比 s_checksum——故只断门控状态一致。
        let disk = MemDisk::from_image(EXT4_NOCSUM_IMAGE);
        let (physical_blocks, geom) = resolve_journal_area(&disk);
        let bs = geom.block_size as usize;
        let sb = load_journal_sb(&disk, &physical_blocks, bs).expect("load SB (nocsum image)");
        assert!(
            !sb.has_checksum_v2_or_v3(),
            "EXT4_NOCSUM_IMAGE journal SB must have CSUM_V2/V3 gate OFF"
        );
    }

    /// JBD2 csum 套件差分：descriptor-tail / per-tag data / commit 三类 csum，core 必 ==
    /// 按 ext4_rs 源码（mod.rs:309-323/441-447/449-457）逐字复刻的字节序列喂 ext4_rs public
    /// `ext4_crc32c` 的结果（ext4_rs 私有 csum 方法不可外调；core crc 已与 ext4_rs crc 逐位对齐）。
    /// 用真镜像 journal SB 的 UUID 作种子。
    #[ktest]
    fn jbd2_csum_suite_parity() {
        use crate::fs::ext4::core::journal::superblock::{
            commit_block_csum, descriptor_tail_csum, load_journal_sb, tag_data_csum,
        };

        let disk = MemDisk::from_image(EXT4_IMAGE);
        let (physical_blocks, geom) = resolve_journal_area(&disk);
        let bs = geom.block_size as usize;
        let sb = load_journal_sb(&disk, &physical_blocks, bs).expect("load SB for UUID");
        let uuid = sb.uuid();

        // 构造一个 descriptor 块（block_size 字节，tail csum 字段保持零）、一个 commit 块字节
        // （64 字节，h_chksum 区保持零）、一个数据块（block_size 字节）。
        let mut descriptor = vec![0u8; bs];
        for (i, b) in descriptor.iter_mut().enumerate() {
            *b = (i.wrapping_mul(31) & 0xFF) as u8;
        }
        // tail csum 字段在块尾 4 字节——置零（计算时为零，与 ext4_rs 一致）。
        let tail = descriptor.len() - 4;
        descriptor[tail..].fill(0);

        let mut data_block = vec![0u8; bs];
        for (i, b) in data_block.iter_mut().enumerate() {
            *b = (i.wrapping_mul(17).wrapping_add(7) & 0xFF) as u8;
        }

        let commit_bytes = {
            // 64 字节 commit 结构镜像（h_chksum 全零，含其它字段任意——只验算法）。
            let mut c = vec![0u8; 64];
            // 放个大端 magic 头让它像真 commit（不影响算法，仅更接近实战）。
            c[0..4].copy_from_slice(&0xC03B_3998u32.to_be_bytes());
            c
        };

        let sequence: u32 = geom.sequence;

        // 旧侧基准：按 ext4_rs 源码逐字复刻的字节序列 + ext4_rs public crc。
        let old_desc = {
            let mut d = Vec::with_capacity(16 + descriptor.len());
            d.extend_from_slice(&uuid);
            d.extend_from_slice(&descriptor);
            ext4_rs::ext4_crc32c(ext4_rs::EXT4_CRC32_INIT, &d, d.len() as u32)
        };
        let old_tag = {
            let mut d = Vec::with_capacity(16 + 4 + data_block.len());
            d.extend_from_slice(&uuid);
            d.extend_from_slice(&sequence.to_be_bytes());
            d.extend_from_slice(&data_block);
            ext4_rs::ext4_crc32c(ext4_rs::EXT4_CRC32_INIT, &d, d.len() as u32)
        };
        let old_commit = {
            let mut d = Vec::with_capacity(16 + commit_bytes.len());
            d.extend_from_slice(&uuid);
            d.extend_from_slice(&commit_bytes);
            ext4_rs::ext4_crc32c(ext4_rs::EXT4_CRC32_INIT, &d, d.len() as u32)
        };

        // 新侧：core 的 csum 套件。
        let new_desc = descriptor_tail_csum(&uuid, &descriptor);
        let new_tag = tag_data_csum(&uuid, sequence, &data_block);
        let new_commit = commit_block_csum(&uuid, &commit_bytes);

        assert_eq!(new_desc, old_desc, "descriptor-tail csum: core vs ext4_rs recipe");
        assert_eq!(new_tag, old_tag, "per-tag data csum: core vs ext4_rs recipe");
        assert_eq!(new_commit, old_commit, "commit-block csum: core vs ext4_rs recipe");

        // v2 取低 16 位是调用方截断——这里确认截断语义与全 32 位一致。
        assert_eq!((new_tag & 0xFFFF) as u16, (old_tag & 0xFFFF) as u16, "v2 tag low-16 truncation");
    }

    // =================================================================
    // Phase 5 Task 2：JournalSpace 环形空间 + 事务/handle 状态机 → JournalCommitPlan 差分。
    //
    // - `journal_space_ring_parity`：同几何构造 core/ext4_rs `JournalSpace`，跑同一串
    //   advance_head/set_tail（含 WRAP-AROUND：advance 越过 maxlen），对拍
    //   free_blocks/distance/head/tail；再用真镜像几何对拍 from_superblock。
    // - `journal_commit_plan_parity`：同序列（start_handle → record_metadata_write* 含重复
    //   block + 短/超 block_size 镜像 → stop_handle → prepare_commit）喂两侧 JournalRuntime，
    //   对拍 plan.tid + metadata_blocks（block 集合 + 序 + 镜像字节）。`match (old,new)`，
    //   旧侧不 `.expect()`（admission/commit 失败也参与对拍）。
    // =================================================================

    /// JournalSpace 环形数学差分：core `JournalSpace`（`from_superblock`）vs ext4_rs
    /// `JournalSpace`（经 `Jbd2Journal::load(&ext4).space` 取实例——其类型未在 ext4_rs 公开
    /// 命名，但 `Jbd2Journal.space` 字段公开、方法公开，可直驱）。两侧从**同一真镜像**几何起步，
    /// 跑同一串 advance_head/set_tail（含 WRAP-AROUND：advance 量越过 maxlen），逐步对拍
    /// free_blocks/distance/head/tail。
    #[ktest]
    fn journal_space_ring_parity() {
        use crate::fs::ext4::core::journal::space::JournalSpace;
        use crate::fs::ext4::core::journal::superblock::load_journal_sb;

        for image in [EXT4_IMAGE, EXT4_MULTIGROUP_IMAGE, EXT4_NOCSUM_IMAGE] {
            let disk = MemDisk::from_image(image);
            let (physical_blocks, geom) = resolve_journal_area(&disk);
            let bs = geom.block_size as usize;

            // 新侧：core from_superblock（读同一组物理块的逻辑块 0）。
            let sb = load_journal_sb(&disk, &physical_blocks, bs).expect("core load SB for ring");
            let mut new_space = JournalSpace::from_superblock(&sb).expect("core from_superblock");

            // 旧侧：ext4_rs Jbd2Journal::load(&ext4).space（同镜像几何）。类型未公开命名，
            // 故用 `let mut old_space = journal.space;`（类型推断，从不书写其名）。
            let ext4 = ext4_rs::Ext4::open(Arc::new(disk.clone()));
            let journal = ext4_rs::Jbd2Journal::load(&ext4).expect("ext4_rs Jbd2Journal::load");
            let mut old_space = journal.space;

            // from_superblock parity：初始几何 + free_blocks 一致。
            assert_eq!(new_space.first(), old_space.first(), "from_superblock first ({image:?})");
            assert_eq!(new_space.maxlen(), old_space.maxlen(), "from_superblock maxlen");
            assert_eq!(new_space.head(), old_space.head(), "from_superblock head");
            assert_eq!(new_space.tail(), old_space.tail(), "from_superblock tail");
            assert_eq!(new_space.free_blocks(), old_space.free_blocks(), "from_superblock free_blocks");

            // usable = maxlen - first；选若干 advance 量含越过 usable 的回绕（usable+5 / 2*usable）。
            let usable = old_space.maxlen() - old_space.first();
            let advances = [3u32, usable + 5, 7, 2 * usable, 0, usable.saturating_sub(1)];
            for &blocks in &advances {
                let new_h = new_space.advance_head(blocks);
                let old_h = old_space.advance_head(blocks);
                assert_eq!(new_h, old_h, "advance_head({blocks}) head value (wrap-aware)");
                assert_eq!(new_space.head(), old_space.head(), "head after advance_head({blocks})");
                assert_eq!(
                    new_space.free_blocks(),
                    old_space.free_blocks(),
                    "free_blocks after advance_head({blocks})"
                );
                // distance(tail, head) 含回绕分支对拍。
                assert_eq!(
                    new_space.distance(new_space.tail(), new_space.head()),
                    old_space.distance(old_space.tail(), old_space.head()),
                    "distance(tail,head) after advance_head({blocks})"
                );
            }

            // set_tail 串：合法值（first / first+usable/2 / maxlen-1）+ 越界值（0 / maxlen）。
            let first = old_space.first();
            let maxlen = old_space.maxlen();
            let tails = [first, first + usable / 2, maxlen - 1, 0, maxlen];
            for &t in &tails {
                let new_r = new_space.set_tail(t);
                let old_r = old_space.set_tail(t);
                // `match (old, new)` —— Ok/Err 同构；不 `.expect()` 旧侧。
                match (old_r, new_r) {
                    (Ok(()), Ok(())) => {
                        assert_eq!(new_space.tail(), old_space.tail(), "tail after set_tail({t})");
                        assert_eq!(
                            new_space.free_blocks(),
                            old_space.free_blocks(),
                            "free_blocks after set_tail({t})"
                        );
                        assert_eq!(
                            new_space.distance(new_space.tail(), new_space.head()),
                            old_space.distance(old_space.tail(), old_space.head()),
                            "distance after set_tail({t})"
                        );
                    }
                    (Err(oe), Err(ne)) => {
                        assert_eq!(oe.error() as i32, ne.error() as i32, "set_tail({t}) errno");
                    }
                    (o, n) => panic!(
                        "set_tail({t}) Ok/Err shape mismatch: old ok={} new ok={}",
                        o.is_ok(),
                        n.is_ok()
                    ),
                }
            }

            // 直接对拍 distance 的全部三分支（from==to / to>from / 回绕 to<from）。
            let probes = [(first, first), (first, maxlen - 1), (maxlen - 1, first)];
            for &(f, t) in &probes {
                assert_eq!(
                    new_space.distance(f, t),
                    old_space.distance(f, t),
                    "distance({f},{t}) standalone"
                );
            }
        }
    }

    /// JournalCommitPlan 差分（Part A）：同序列喂 core/ext4_rs `JournalRuntime`，对拍 plan.tid +
    /// metadata_blocks（block 集合 + 序 + 镜像字节）。序列含**重复 block**（去重 → 最新镜像）+
    /// **短镜像**（< block_size → 零填）——这两种在两侧 runtime API（core 全块 API vs ext4_rs
    /// offset API at offset 0）产同一 `BTreeMap<block_nr, JournalBuffer>` 结果。
    ///
    /// 注：**超长镜像**（> block_size）的 BUG-8 截断单独在 [`journal_commit_plan_bug8_clamp`]
    /// 验证——ext4_rs 的 runtime offset API 会把超长镜像**跨块切片**（journal.rs:503-540 的
    /// `chunk_len = min(block_size - offset, ..)`），与 core 全块 API 的「单块截断」形状不同，
    /// 故超长项的 parity 在 transaction 级（base-image clamp = BUG-8 的真实触发处）单独对拍。
    #[ktest]
    fn journal_commit_plan_parity() {
        use crate::fs::ext4::core::journal::transaction::JournalRuntime;

        let block_size = 8usize;
        let first_tid = 1u32;

        // (block_nr, image)；BTreeMap 去重 + block_nr 升序 → 期望 plan 顺序 0,1,2。
        let writes: &[(u64, Vec<u8>)] = &[
            (2u64, (10..18u8).collect::<Vec<u8>>()), // 恰好 8 字节
            (0u64, vec![1, 2, 3]),                   // 短：3 < 8 → 零填到 8
            (1u64, (20..28u8).collect::<Vec<u8>>()), // 恰好 8 字节
            (0u64, vec![7, 7, 7, 7, 7]),             // 重复 block 0：覆盖为 5 字节 → 零填到 8
        ];

        // ---- 新侧（core）：全块 API ----
        let new_plan = {
            let mut rt = JournalRuntime::new(block_size, first_tid);
            let hid = rt.start_handle(writes.len() as u32 + 2).expect("core start_handle");
            for (block, image) in writes {
                rt.record_metadata_write(hid, *block, image);
            }
            let tid = rt.stop_handle(hid).expect("core stop_handle returns tid");
            let plan = rt.prepare_commit().expect("core prepare_commit yields plan");
            assert_eq!(plan.tid, tid, "core plan.tid == stop_handle tid");
            plan
        };

        // ---- 旧侧（ext4_rs）：offset API at offset = block_nr*block_size ----
        let old_plan = {
            let mut rt = ext4_rs::JournalRuntime::new(block_size, first_tid);
            // `match` 旧侧 Option（start_handle 可能 None）——不 `.expect()` 假定成功。
            let handle = match rt.start_handle(writes.len() as u32 + 2, None) {
                Some(h) => h,
                None => panic!("ext4_rs start_handle returned None on enabled runtime"),
            };
            let hid = handle.handle_id();
            for (block, image) in writes {
                let offset = (*block as usize) * block_size;
                // load_block 闭包用独立 clone（与传入 `image` 借用不冲突）。短镜像由 load_block 给基底。
                let base = image.clone();
                rt.record_metadata_write_for_handle(hid, offset, image, move |_| base.clone());
            }
            rt.stop_handle(handle);
            match rt.prepare_commit() {
                Some(p) => p,
                None => panic!("ext4_rs prepare_commit yielded None"),
            }
        };

        // ---- 对拍：tid + metadata_blocks（block 集合 + 序 + 镜像字节） ----
        assert_eq!(new_plan.tid, old_plan.tid, "commit plan tid: core vs ext4_rs");
        assert_eq!(
            new_plan.metadata_blocks.len(),
            old_plan.metadata_blocks.len(),
            "metadata_blocks count: core vs ext4_rs"
        );
        for (i, (n, o)) in new_plan
            .metadata_blocks
            .iter()
            .zip(old_plan.metadata_blocks.iter())
            .enumerate()
        {
            assert_eq!(n.block_nr, o.block_nr, "metadata_blocks[{i}] block_nr");
            assert_eq!(
                n.block_data, o.block_data,
                "metadata_blocks[{i}] image bytes (block {})",
                n.block_nr
            );
            assert_eq!(
                n.block_data.len(),
                block_size,
                "metadata_blocks[{i}] full block ({block_size} bytes)"
            );
        }
        // 显式验证去重定序：block 0,1,2 升序、block 0 = 覆盖后最新镜像（零填）。
        let new_blocks: Vec<u64> = new_plan.metadata_blocks.iter().map(|b| b.block_nr).collect();
        assert_eq!(new_blocks, vec![0, 1, 2], "BTreeMap dedup+order: 0,1,2");
        assert_eq!(
            new_plan.metadata_blocks[0].block_data,
            vec![7, 7, 7, 7, 7, 0, 0, 0],
            "block 0 dedup → latest image, zero-padded"
        );
    }

    /// BUG-8 size-clamp 差分（Part B）：base-image clamp 的真实触发处。core 全块 API 把
    /// `full_image` 直接 clamp 到 block_size（短→零填，超长→截断）；ext4_rs 在 transaction 级
    /// 对 `load_block()` 基底做同一 clamp（transaction.rs:154-158），随后 offset-0 的 data 写
    /// 落在块内（不再 re-grow）→ 同一 8 字节结果。直驱 ext4_rs public `JournalTransaction`
    /// 对拍，避开 runtime offset API 的跨块切片形状差异。
    #[ktest]
    fn journal_commit_plan_bug8_clamp() {
        use crate::fs::ext4::core::journal::transaction::JournalRuntime;

        let block_size = 8usize;

        // 三种基底：短(3<8)/恰好(8)/超长(12>8)。core 全块 API 收基底当 full_image 直接 clamp；
        // ext4_rs 在 transaction 级把 load_block() 基底 clamp，再写 0 长 data（不改字节，只验基底 clamp）。
        let bases: &[Vec<u8>] = &[
            vec![1, 2, 3],                    // 短 → 零填 [1,2,3,0,0,0,0,0]
            (40..48u8).collect::<Vec<u8>>(),  // 恰好 8
            vec![9u8; 12],                    // 超长 → 截断 [9;8]
        ];

        for (idx, base) in bases.iter().enumerate() {
            let block_nr = idx as u64;

            // 新侧（core）：record 全块镜像 = base，clamp 到 block_size。
            let new_data = {
                let mut rt = JournalRuntime::new(block_size, 1);
                let hid = rt.start_handle(4).expect("core start_handle");
                rt.record_metadata_write(hid, block_nr, base);
                rt.stop_handle(hid);
                let plan = rt.prepare_commit().expect("core prepare_commit");
                assert_eq!(plan.metadata_blocks.len(), 1, "one block");
                plan.metadata_blocks[0].block_data.clone()
            };

            // 旧侧（ext4_rs）：transaction 级 record_metadata_write，load_block() 返回 base
            // → BUG-8 clamp 基底；data 为空（offset 0, len 0），不触发 re-grow，结果 = clamp 后基底。
            let old_data = {
                let mut tx = ext4_rs::JournalTransaction::new(1);
                let base_for_load = base.clone();
                tx.record_metadata_write(block_nr, 0, &[], block_size, move || base_for_load.clone());
                match tx.buffer(block_nr) {
                    Some(buf) => buf.block_data.clone(),
                    None => panic!("ext4_rs transaction buffer must exist after record"),
                }
            };

            assert_eq!(
                new_data, old_data,
                "BUG-8 base-image clamp: core full-image vs ext4_rs load_block clamp (block {block_nr})"
            );
            assert_eq!(new_data.len(), block_size, "clamped to block_size (block {block_nr})");
        }

        // 直接断 core 的两个 clamp 方向（短零填 / 超长截断）。
        let mut rt = JournalRuntime::new(block_size, 1);
        let hid = rt.start_handle(8).expect("core start_handle");
        rt.record_metadata_write(hid, 0, &[1, 2, 3]); // 短
        rt.record_metadata_write(hid, 1, &[9u8; 12]); // 超长
        rt.stop_handle(hid);
        let plan = rt.prepare_commit().expect("core prepare_commit");
        assert_eq!(
            plan.metadata_blocks[0].block_data,
            vec![1, 2, 3, 0, 0, 0, 0, 0],
            "short image zero-padded to block_size (BUG-8)"
        );
        assert_eq!(
            plan.metadata_blocks[1].block_data,
            vec![9u8; 8],
            "over-long image truncated to block_size (BUG-8)"
        );
    }

    /// soft-credit admission 轮转差分：第一个 handle 预留 1020 块（接近 1024 上限）+ 记一块
    /// 元数据后，第二个 handle（预留 8 → 1020+8 > 1024）触发 admission 轮转：旧 running 移到
    /// prev_running（tid=1），新 running 用下一个 tid（=2）。对拍 core vs ext4_rs 的 running/
    /// prev_running tid + 两侧各自 prepare_commit 的 plan.tid（应先 commit tid=1）。
    /// PARITY: ext4_rs journal.rs 的 `credit_admission_rotates_idle_transaction_before_overflow`。
    #[ktest]
    fn journal_credit_rotation_parity() {
        use crate::fs::ext4::core::journal::transaction::JournalRuntime;

        let block_size = 8usize;
        let first_tid = 1u32;

        // ---- 新侧（core） ----
        let (new_prev_tid, new_run_tid, new_plan_tid) = {
            let mut rt = JournalRuntime::new(block_size, first_tid);
            let h1 = rt.start_handle(1020).expect("core start_handle h1");
            rt.record_metadata_write(h1, 0, &[1u8; 8]); // running 有 buffer → 轮转条件之一
            rt.stop_handle(h1);
            // h2 预留 8：1020 + 8 > 1024 → admission 轮转。
            let h2 = rt.start_handle(8).expect("core start_handle h2");
            let prev_tid = rt.prev_running_transaction().map(|t| t.tid());
            let run_tid = rt.running_transaction().map(|t| t.tid());
            rt.record_metadata_write(h2, 8, &[2u8; 8]);
            rt.stop_handle(h2);
            // prepare_commit 先取 prev_running（tid=1）。
            let plan = rt.prepare_commit().expect("core prepare_commit after rotation");
            (prev_tid, run_tid, plan.tid)
        };

        // ---- 旧侧（ext4_rs） ----
        let (old_prev_tid, old_run_tid, old_plan_tid) = {
            let mut rt = ext4_rs::JournalRuntime::new(block_size, first_tid);
            let h1 = match rt.start_handle(1020, None) {
                Some(h) => h,
                None => panic!("ext4_rs start_handle h1 None"),
            };
            let img1 = vec![1u8; 8];
            let b1 = img1.clone();
            rt.record_metadata_write_for_handle(h1.handle_id(), 0, &b1, move |_| img1.clone());
            rt.stop_handle(h1);
            let h2 = match rt.start_handle(8, None) {
                Some(h) => h,
                None => panic!("ext4_rs start_handle h2 None"),
            };
            let prev_tid = rt.prev_running_transaction().map(|t| t.tid());
            let run_tid = rt.running_transaction().map(|t| t.tid());
            let img2 = vec![2u8; 8];
            let b2 = img2.clone();
            rt.record_metadata_write_for_handle(h2.handle_id(), 8, &b2, move |_| img2.clone());
            rt.stop_handle(h2);
            let plan = match rt.prepare_commit() {
                Some(p) => p,
                None => panic!("ext4_rs prepare_commit after rotation None"),
            };
            (prev_tid, run_tid, plan.tid)
        };

        assert_eq!(new_prev_tid, old_prev_tid, "prev_running tid after rotation");
        assert_eq!(new_run_tid, old_run_tid, "running tid after rotation");
        assert_eq!(new_plan_tid, old_plan_tid, "commit plan tid (prev_running first)");
        // 语义自检：轮转把 tid=1 移到 prev_running、新 running=tid 2、先 commit tid 1。
        assert_eq!(new_prev_tid, Some(1), "rotated transaction is tid 1");
        assert_eq!(new_run_tid, Some(2), "new running is tid 2");
        assert_eq!(new_plan_tid, 1, "prepare_commit drains prev_running (tid 1) first");
    }

    // =================================================================
    // Phase 5 Task 3：commit 落盘（descriptor/tags/payload-escape/单屏障/commit/SB/ring）★RED-LINE★。
    //
    // core 侧驱动 [`core_journal_commit`]：与旧侧 [`old_journal_commit`] 同序列驱动 core 引擎——
    //   resolve_journal_area 拿物理块 + 几何 → core `load_journal_sb` 读 SB → `JournalSpace::from_superblock`
    //   → core `JournalRuntime::new(block_size, sb.sequence())` start/record/stop/prepare_commit
    //   → core `write_commit_plan`（经 `CommitCtx` + `DirectMetadataWriter` 写同一组物理块）→ finish_commit。
    // 用 [`diff_journal_commit`] 把它当 `core_commit` 闭包，跑后 journal 区逐字节 == ext4_rs + tid 一致。
    //
    // - `journal_commit_single_parity`：1 事务、几个 metadata 块。
    // - `journal_commit_escape_parity`：payload 首 4 字节 == 大端 JBD2 magic 的块（escape）。
    // - `journal_commit_csum_gating_parity`：NOCSUM vs csum-on 镜像（三镜像 journal feat_incompat=0，
    //   故 tag/tail/commit csum 两侧都关——与 ext4_rs 门控一致，逐字节对拍仍是有效 parity）。
    // - `journal_commit_multi_ring_parity`：多事务连续 commit 逼 ring 回绕。
    // =================================================================

    /// core 侧 commit 驱动（[`diff_journal_commit`] 的 `core_commit` 闭包）：在 `disk` 上用 core 引擎
    /// 跑 `writes` 的一个事务并 commit 落盘，返回写入的 tid。与 [`old_journal_commit`] 同序列。
    ///
    /// 写序由 core `write_commit_plan` 内部复刻（descriptor+payload → sync 屏障 → commit → SB → ring）。
    /// 这里只负责：定位 journal 区（同 ext4_rs）→ 读 SB → 建 space/runtime → 喂同序列 → 调 emitter。
    fn core_journal_commit(disk: &MemDisk, writes: &[JournalMetaWrite]) -> u32 {
        use crate::fs::ext4::core::journal::commit::{write_commit_plan, CommitCtx};
        use crate::fs::ext4::core::journal::space::JournalSpace;
        use crate::fs::ext4::core::journal::superblock::load_journal_sb;
        use crate::fs::ext4::core::journal::transaction::JournalRuntime;

        let (physical_blocks, geom) = resolve_journal_area(disk);
        let bs = geom.block_size as usize;

        // 读 journal SB（同一组物理块的逻辑块 0），建环空间。
        let mut sb = load_journal_sb(disk, &physical_blocks, bs).expect("core load_journal_sb");
        let mut space = JournalSpace::from_superblock(&sb).expect("core JournalSpace::from_superblock");

        // 内存事务：first_tid = SB 当前序号（与旧侧 old_journal_commit 对齐）。
        let mut runtime = JournalRuntime::new(bs, sb.sequence());
        let hid = runtime
            .start_handle(writes.len() as u32 + 2)
            .expect("core start_handle on enabled runtime");
        for w in writes {
            assert_eq!(
                w.image.len(),
                bs,
                "metadata image must be a full block ({bs} bytes), got {}",
                w.image.len()
            );
            runtime.record_metadata_write(hid, w.block_nr, &w.image);
        }
        runtime.stop_handle(hid);
        let plan = runtime
            .prepare_commit()
            .expect("core prepare_commit yields a plan after a closed handle with metadata");
        let tid = plan.tid;

        // emitter：经 DirectMetadataWriter 写同一组物理块；barrier=None（差分里 sync no-op）。
        let writer = DirectMetadataWriter::new(disk.clone(), bs);
        let ctx = CommitCtx {
            physical_blocks: &physical_blocks,
            writer: &writer,
            barrier: None,
            handle_id: hid,
            block_size: bs,
        };
        let written_tid = write_commit_plan(&ctx, &mut space, &mut sb, &plan)
            .expect("core write_commit_plan");
        assert_eq!(written_tid, tid, "core write_commit_plan returns plan.tid");

        // committing 槽幂等：落盘后 finish_commit 清槽（tid 匹配）。
        assert!(runtime.finish_commit(tid), "core finish_commit clears committing slot");
        assert!(
            runtime.committing_transaction().is_none(),
            "committing slot empty after finish_commit"
        );
        written_tid
    }

    /// 一个全块镜像（block_size 字节），第 i 字节 = `(seed + i) & 0xFF`。
    fn full_block_image(block_size: usize, seed: u8) -> Vec<u8> {
        (0..block_size)
            .map(|i| (seed as usize).wrapping_add(i) as u8)
            .collect()
    }

    /// commit 单事务差分：几个 metadata 块的一个事务，journal 区逐字节 == ext4_rs（descriptor +
    /// tags + payload + commit + SB s_start/s_head/s_sequence + csum）+ 返回 tid 一致。
    #[ktest]
    fn journal_commit_single_parity() {
        for image in [EXT4_IMAGE, EXT4_MULTIGROUP_IMAGE, EXT4_NOCSUM_IMAGE] {
            // 用旧侧解出 block_size 造全块镜像（多镜像块大小不同：4096 / 1024）。
            let probe = MemDisk::from_image(image);
            let (_, geom) = resolve_journal_area(&probe);
            let bs = geom.block_size as usize;

            // 三个不同 home 块号的 metadata 写（升序 + 乱序混入，验 BTreeMap 定序）。
            let writes = [
                JournalMetaWrite { block_nr: 21, image: full_block_image(bs, 0x10) },
                JournalMetaWrite { block_nr: 5, image: full_block_image(bs, 0x40) },
                JournalMetaWrite { block_nr: 13, image: full_block_image(bs, 0x90) },
            ];

            diff_journal_commit(image, &writes, core_journal_commit);
        }
    }

    /// commit escape 差分：含一个 payload 首 4 字节 == 大端 JBD2 magic 的块——两侧都把写盘那 4 字节
    /// 零填 + tag 置 ESCAPE（csum 按原始数据），journal 区逐字节一致。
    #[ktest]
    fn journal_commit_escape_parity() {
        use crate::fs::ext4::core::journal::format::JBD2_MAGIC;

        for image in [EXT4_IMAGE, EXT4_NOCSUM_IMAGE] {
            let probe = MemDisk::from_image(image);
            let (_, geom) = resolve_journal_area(&probe);
            let bs = geom.block_size as usize;

            // escape 命中块：首 4 字节 = magic 大端，其余非零。
            let mut escape_img = full_block_image(bs, 0x77);
            escape_img[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());

            let writes = [
                JournalMetaWrite { block_nr: 9, image: escape_img },
                JournalMetaWrite { block_nr: 17, image: full_block_image(bs, 0x33) },
            ];

            diff_journal_commit(image, &writes, core_journal_commit);
        }
    }

    /// commit csum 门控差分：NOCSUM（journal csum 关）vs csum-on 镜像各自 commit；两侧引擎按同一
    /// 门控产 journal 区（三镜像 journal feat_incompat=0 → tag/tail/commit csum 均关），逐字节一致。
    #[ktest]
    fn journal_commit_csum_gating_parity() {
        for image in [EXT4_NOCSUM_IMAGE, EXT4_IMAGE] {
            let probe = MemDisk::from_image(image);
            let (_, geom) = resolve_journal_area(&probe);
            let bs = geom.block_size as usize;

            let writes = [
                JournalMetaWrite { block_nr: 7, image: full_block_image(bs, 0x05) },
                JournalMetaWrite { block_nr: 8, image: full_block_image(bs, 0xC0) },
            ];

            diff_journal_commit(image, &writes, core_journal_commit);
        }
    }

    /// commit 多事务连续 parity：在同一盘上**连续 commit 多个事务**，每次 commit 后 journal 区
    /// 逐字节 == ext4_rs（含 s_head 沿环推进、s_start 首事务后固定、s_sequence 每事务 +1）。
    ///
    /// 每个事务用各自两盘对拍（旧/新从**同一前序态**起步、跑一个事务、对拍），再用旧侧引擎把
    /// 共享盘真正推进到下一轮起点——串起来即「多事务连续 commit」的逐字节 parity，且能定位是哪个
    /// 事务出的差异。事务数控制在 free 空间内（本差分不 checkpoint，tail 固定在 first，故 head 不能
    /// 物理越过 maxlen——真实物理回绕需 checkpoint 推进 tail，留 Task 5/6；ring `advance` 的环算术
    /// 回绕已由 Task 2 `journal_space_ring_parity` 直接覆盖）。
    #[ktest]
    fn journal_commit_multi_sequential_parity() {
        let base_image: &[u8] = EXT4_IMAGE;
        let probe = MemDisk::from_image(base_image);
        let (_, geom) = resolve_journal_area(&probe);
        let bs = geom.block_size as usize;

        let mut disk_bytes = base_image.to_vec();
        let payloads_per_txn = 4usize; // 每事务 6 块（4 payload + descriptor + commit）。
        let txns = 6u32; // 6 事务 * 6 块 = 36 块 << usable(1023)，留足空间。

        for round in 0..txns {
            let writes: Vec<JournalMetaWrite> = (0..payloads_per_txn)
                .map(|k| JournalMetaWrite {
                    block_nr: (100 + round * 16 + k as u32) as u64,
                    image: full_block_image(bs, (round as u8).wrapping_mul(7).wrapping_add(k as u8)),
                })
                .collect();

            // 从当前盘字节对拍一个事务（diff_journal_commit 内部各建两盘 from_image）。
            diff_journal_commit(&disk_bytes, &writes, core_journal_commit);

            // 推进共享盘：用旧侧引擎在 disk_bytes 上真正 commit 这个事务，得到下一轮起点。
            let advance_disk = MemDisk::from_image(&disk_bytes);
            let _ = old_journal_commit(&advance_disk, &writes);
            disk_bytes = advance_disk.backing().lock().clone();
        }

        // 收尾自检：sequence 每事务 +1；start 在首事务后固定（非 0）。
        let final_disk = MemDisk::from_image(&disk_bytes);
        let (_, geom_final) = resolve_journal_area(&final_disk);
        assert_eq!(
            geom_final.sequence,
            geom.sequence + txns,
            "sequence advanced by one per committed txn ({txns} txns)"
        );
        assert_ne!(geom_final.start, 0, "s_start set after first commit and kept");
    }

    // =================================================================
    // Phase 5 Task 4：revoke 记录 + revoke 表（复刻 BUG-5 revoke 从不写盘）。
    //
    // - `journal_revoke_no_disk_parity`：含 revoke 的序列后，两侧（旧 ext4_rs / 新 core）的
    //   journal 区都**无** revoke 块（blocktype==5）= BUG-5 parity（都不写盘）+ journal 区
    //   逐字节一致；revoke 表内存语义对拍（删 checkpoint buffer 数 + 表记录）。
    // =================================================================

    /// 扫一段 journal 区镜像，数其中 blocktype == JBD2_REVOKE_BLOCK(5) 的块数。
    /// 每个 journal 逻辑块开头是 12B 大端 `RawJournalHeader`；magic 合法且 blocktype==5 即计一块。
    fn count_revoke_blocks(area: &[u8], block_size: usize) -> usize {
        use crate::fs::ext4::core::journal::format::{RawJournalHeader, JBD2_REVOKE_BLOCK};

        let hdr_len = size_of::<RawJournalHeader>();
        let mut count = 0usize;
        let mut off = 0usize;
        while off + block_size <= area.len() {
            if block_size >= hdr_len {
                let header = RawJournalHeader::from_bytes(&area[off..off + hdr_len]);
                if header.is_valid_magic() && header.blocktype() == JBD2_REVOKE_BLOCK {
                    count += 1;
                }
            }
            off += block_size;
        }
        count
    }

    /// revoke parity（BUG-5）：含 revoke 的序列后，两侧 journal 区**无** revoke 块（type=5）——
    /// 新旧引擎都不写盘——且 journal 区逐字节一致；revoke 表/checkpoint-删-buffer 内存语义对拍。
    ///
    /// 步骤：
    /// 1. 用 [`diff_journal_commit`] 跑一个事务（旧 ext4_rs / 新 core 各自盘），它已断言 journal 区
    ///    逐字节 == + tid ==。commit 后两侧 journal 区都只含 descriptor/payload/commit/SB，无 type=5。
    /// 2. 各自快照 journal 区，断言 `count_revoke_blocks == 0`（BUG-5：都不写 revoke 块）。
    /// 3. 内存语义对拍：两侧各建 runtime → commit → finish_commit 把事务推入 checkpoint 队列 →
    ///    `revoke_checkpoint_metadata_block(blk)` → 对拍「删到的 checkpoint buffer 数」一致 +
    ///    被 revoke 的块确实从 checkpoint 事务删掉。core 侧额外验 revoke 表记录该块。
    #[ktest]
    fn journal_revoke_no_disk_parity() {
        use crate::fs::ext4::core::journal::revoke::{CheckpointTransaction, RevokeRuntime};
        use crate::fs::ext4::core::journal::transaction::JournalRuntime;

        for image in [EXT4_IMAGE, EXT4_NOCSUM_IMAGE] {
            let probe = MemDisk::from_image(image);
            let (probe_blocks, geom) = resolve_journal_area(&probe);
            let bs = geom.block_size as usize;

            // 一个事务的若干 metadata 写（升序 + 含一个稍后被 revoke 的块号 13）。
            let writes = [
                JournalMetaWrite { block_nr: 13, image: full_block_image(bs, 0x10) },
                JournalMetaWrite { block_nr: 5, image: full_block_image(bs, 0x40) },
                JournalMetaWrite { block_nr: 21, image: full_block_image(bs, 0x90) },
            ];

            // ---- (1)+(2) 盘上 BUG-5 parity：commit 后两侧 journal 区无 revoke 块 + 逐字节一致 ----
            // diff_journal_commit 内部各建两盘、对拍 journal 区逐字节 + tid；这里再各跑一次落到我们
            // 持有的盘上，单独验「无 type=5 块」（diff_journal_commit 不暴露其内部盘）。
            diff_journal_commit(image, &writes, core_journal_commit);

            let old_disk = MemDisk::from_image(image);
            let new_disk = MemDisk::from_image(image);
            let old_tid = old_journal_commit(&old_disk, &writes);
            let new_tid = core_journal_commit(&new_disk, &writes);
            assert_eq!(old_tid, new_tid, "revoke-seq commit tid mismatch");

            let old_area = snapshot_journal_area(&old_disk, &probe_blocks, bs);
            let new_area = snapshot_journal_area(&new_disk, &probe_blocks, bs);
            assert_eq!(
                count_revoke_blocks(&old_area, bs),
                0,
                "BUG-5: ext4_rs journal area must contain NO revoke block (type=5) [{image:?}]"
            );
            assert_eq!(
                count_revoke_blocks(&new_area, bs),
                0,
                "BUG-5: core journal area must contain NO revoke block (type=5) [{image:?}]"
            );
            // 两侧 journal 区逐字节一致（含「都没写 revoke 块」这一事实）。
            assert_journal_area_eq(&old_area, &new_area, &probe_blocks, bs);

            // ---- (3) 内存 revoke 语义对拍：删 checkpoint buffer 数一致 ----
            let revoke_block = 13u64; // 上面 writes 里出现，故 checkpoint 事务持有它。

            // 旧侧（ext4_rs）：commit → finish_commit 入 checkpoint_list → revoke_checkpoint_metadata_block。
            let old_removed = {
                let mut rt = ext4_rs::JournalRuntime::new(bs, 1);
                let h = match rt.start_handle(writes.len() as u32 + 2, None) {
                    Some(h) => h,
                    None => panic!("ext4_rs start_handle None on enabled runtime"),
                };
                let hid = h.handle_id();
                for w in &writes {
                    let off = (w.block_nr as usize) * bs;
                    let base = w.image.clone();
                    rt.record_metadata_write_for_handle(hid, off, &w.image, move |_| base.clone());
                }
                rt.stop_handle(h);
                let plan = match rt.prepare_commit() {
                    Some(p) => p,
                    None => panic!("ext4_rs prepare_commit None after closed handle"),
                };
                // finish_commit 把事务推入 checkpoint_list（start_block/next_head 任意，本检查不依赖环位置）。
                assert!(rt.finish_commit(plan.tid, 1, 5), "ext4_rs finish_commit → checkpoint_list");
                assert_eq!(rt.checkpoint_depth(), 1, "ext4_rs one checkpoint txn");
                rt.revoke_checkpoint_metadata_block(revoke_block)
            };

            // 新侧（core）：同序列 commit（内存 plan）→ 把 plan 推入 RevokeRuntime checkpoint 队列 →
            // revoke_checkpoint_metadata_block。core 的内存事务半部（JournalRuntime）无 checkpoint_list，
            // 故 revoke 用独立 RevokeRuntime（语义逐字对齐 ext4_rs revoke_checkpoint_metadata_block）。
            let (new_removed, new_table_revoked, new_buffer_gone) = {
                let mut rt = JournalRuntime::new(bs, 1);
                let hid = rt.start_handle(writes.len() as u32 + 2).expect("core start_handle");
                for w in &writes {
                    rt.record_metadata_write(hid, w.block_nr, &w.image);
                }
                rt.stop_handle(hid);
                let plan = rt.prepare_commit().expect("core prepare_commit");
                let tid = plan.tid;

                let mut revoke_rt = RevokeRuntime::new();
                revoke_rt.push_checkpoint(CheckpointTransaction::new(
                    tid,
                    plan.metadata_blocks
                        .iter()
                        .map(|b| (b.block_nr, b.block_data.clone())),
                ));
                assert_eq!(revoke_rt.checkpoint_depth(), 1, "core one checkpoint txn");
                let removed = revoke_rt.revoke_checkpoint_metadata_block(revoke_block);
                let table_revoked = revoke_rt.table().is_revoked(revoke_block);
                let buffer_gone = !revoke_rt
                    .checkpoint_transaction(tid)
                    .expect("core checkpoint txn present")
                    .has_buffer(revoke_block);
                (removed, table_revoked, buffer_gone)
            };

            assert_eq!(
                new_removed, old_removed,
                "revoke_checkpoint_metadata_block removed-count: core vs ext4_rs [{image:?}]"
            );
            assert_eq!(new_removed, 1, "block 13 removed from exactly one checkpoint txn");
            assert!(new_table_revoked, "core revoke table records the revoked block");
            assert!(new_buffer_gone, "core checkpoint buffer for revoked block deleted");
        }
    }

    // =================================================================
    // Phase 5 Task 5：recovery / replay 三趟差分（★RED-LINE★）。
    //
    // 设计（grounded against ext4_rs recovery.rs）：
    // - 用崩溃注入 seam（`build_crashed_journal_image`）造各 stage 的崩溃态盘字节。
    // - 两侧各跑 recover：旧 `old_journal_recover`（ext4_rs `Jbd2Journal::recover`），
    //   新 `core_journal_recover`（core `journal::recovery::recover`）。
    // - **对拍（显式，brief Task-0 note）**：
    //   (a) HOME 目标块逐字节相等（replay 真正落到的 fs 块）——直接 read_at(block*bs) 比对；
    //   (b) 重置后的 journal SB 字段 s_start / s_sequence / s_head + s_checksum 相等；
    //   (c) `RecoverResult` 字段（transactions_replayed / metadata_blocks_replayed /
    //       revoked_blocks / last_sequence）相等；
    //   (d) **整盘** `assert_disk_eq` 作为兜底——因 (b) 已先确认两侧把 SB 重置成字节相同
    //       （replay 会改写 s_start/s_sequence/s_head/csum），整盘对拍不会被良性 SB 分歧误报。
    // - csum：ext4_rs recovery **全程不校 crc**（仅 magic+blocktype+commit.seq==desc.seq）——
    //   故「坏 csum」不改变 recovery 行为；`journal_recover_badcsum_parity` 改坏 commit
    //   块（magic / seq），那才是 ext4_rs 真正的「事务无效→停 replay」判据，两侧同停。
    // - 崩溃采样**非穷举**（report §10.3）：4 stage 采样，不假称穷举。
    // - e2fsck scoping：replay 后 HOME 字节 == ext4_rs HOME 字节即 parity gate；真「dump
    //   replayed MemDisk → 宿主 e2fsck」不在 ktest 内可行（QEMU/宿主侧），是 P6 端到端项——
    //   见 task-5-report.md。这里**不**跑 e2fsck / QEMU。
    // =================================================================

    /// 读 `disk` 上 home 块 `block`（block_size 字节）。
    fn read_home_block(disk: &MemDisk, block: Ext4Fsblk, block_size: usize) -> Vec<u8> {
        let mut buf = vec![0u8; block_size];
        disk.read_at((block as usize) * block_size, buf.as_mut_slice());
        buf
    }

    /// 对拍重置后的 journal SB 字段（两侧均已 replay 并重置）。
    /// 旧侧从盘上 journal 块 0 解 ext4_rs SB；新侧用 core `recover` 的出参 SB（== 盘上）。
    fn assert_reset_sb_eq(
        old_disk: &MemDisk,
        old_blocks: &[Ext4Fsblk],
        new_sb: &crate::fs::ext4::core::journal::format::RawJournalSuperblock,
        block_size: usize,
    ) {
        use crate::fs::ext4::core::journal::format::RawJournalSuperblock;
        use crate::fs::ext4::core::journal::format::JBD2_SUPERBLOCK_SIZE;

        let sb_pblock = old_blocks[0];
        let mut sb_block = vec![0u8; block_size];
        old_disk.read_at((sb_pblock as usize) * block_size, sb_block.as_mut_slice());
        let old_sb = RawJournalSuperblock::from_bytes(&sb_block[..JBD2_SUPERBLOCK_SIZE]);

        assert_eq!(old_sb.start(), new_sb.start(), "reset s_start mismatch");
        assert_eq!(old_sb.sequence(), new_sb.sequence(), "reset s_sequence mismatch");
        assert_eq!(old_sb.head(), new_sb.head(), "reset s_head mismatch");
        assert_eq!(old_sb.checksum(), new_sb.checksum(), "reset s_checksum mismatch");
        // 头 sequence（set_sequence 同改 s_header.h_sequence）也对拍。
        assert_eq!(
            old_sb.header().sequence(),
            new_sb.header().sequence(),
            "reset s_header.h_sequence mismatch"
        );
    }

    /// 一个全块镜像（block_size 字节），第 i 字节 = `(seed + i) & 0xFF`。
    fn recover_block_image(block_size: usize, seed: u8) -> Vec<u8> {
        (0..block_size)
            .map(|i| (seed as usize).wrapping_add(i) as u8)
            .collect()
    }

    /// 跑一个 stage 的恢复差分，返回 `(old_blocks, bs, home_blocks)` 供调用方做额外断言。
    /// 内部：造崩溃态 → 两侧 recover → 对拍 (a) HOME 块 (b) 重置 SB (c) RecoverResult (d) 整盘。
    /// `expect_replayed`：该 stage 是否应有事务被 replay（HOME 块是否应被写）。
    fn run_recover_stage(
        image: &[u8],
        writes: &[JournalMetaWrite],
        stage: JournalCrashStage,
        expect_replayed: bool,
    ) {
        let probe = MemDisk::from_image(image);
        let (probe_blocks, geom) = resolve_journal_area(&probe);
        let bs = geom.block_size as usize;

        let crashed = build_crashed_journal_image(image, writes, stage);

        // pristine 盘（未崩溃、未恢复）——对照 home 块的「原始」字节（不应被 replay 改写时用）。
        let pristine = MemDisk::from_image(image);

        let old_disk = MemDisk::from_image(&crashed);
        let new_disk = MemDisk::from_image(&crashed);

        // 旧 / 新各跑 recover；用 `match (old, new)` 对拍 Ok/Err（绝不 .expect 假定旧侧成功）。
        let old_res = old_journal_recover(&old_disk);
        let new_res = core_journal_recover(&new_disk);
        match (&old_res, &new_res) {
            (Ok(old), Ok((new, new_sb))) => {
                // (c) RecoverResult 字段对拍。
                assert_eq!(
                    old.transactions_replayed, new.transactions_replayed,
                    "transactions_replayed mismatch [{image:?} {stage:?}]"
                );
                assert_eq!(
                    old.metadata_blocks_replayed, new.metadata_blocks_replayed,
                    "metadata_blocks_replayed mismatch [{image:?} {stage:?}]"
                );
                assert_eq!(
                    old.revoked_blocks, new.revoked_blocks,
                    "revoked_blocks mismatch [{image:?} {stage:?}]"
                );
                assert_eq!(
                    old.last_sequence, new.last_sequence,
                    "last_sequence mismatch [{image:?} {stage:?}]"
                );

                // (a) HOME 目标块逐字节相等。
                for w in writes {
                    let old_home = read_home_block(&old_disk, w.block_nr, bs);
                    let new_home = read_home_block(&new_disk, w.block_nr, bs);
                    assert_eq!(
                        old_home, new_home,
                        "HOME block {} bytes differ between engines [{image:?} {stage:?}]",
                        w.block_nr
                    );
                    if expect_replayed {
                        // replay 应把 journaled 镜像写到 home。
                        assert_eq!(
                            new_home, w.image,
                            "replayed HOME block {} != journaled image [{image:?} {stage:?}]",
                            w.block_nr
                        );
                    } else {
                        // 未 commit / SB 未指向 → 不 replay；home 仍是 pristine 字节。
                        let orig = read_home_block(&pristine, w.block_nr, bs);
                        assert_eq!(
                            new_home, orig,
                            "non-replayed HOME block {} must be unchanged [{image:?} {stage:?}]",
                            w.block_nr
                        );
                    }
                }

                // 该 stage 是否真有事务被 replay 的不变量。
                if expect_replayed {
                    assert!(
                        new.transactions_replayed >= 1,
                        "expected ≥1 replayed txn [{image:?} {stage:?}]"
                    );
                    assert!(
                        new.metadata_blocks_replayed >= 1,
                        "expected ≥1 replayed metadata block [{image:?} {stage:?}]"
                    );
                } else {
                    assert_eq!(
                        new.transactions_replayed, 0,
                        "expected 0 replayed txn [{image:?} {stage:?}]"
                    );
                    assert_eq!(
                        new.metadata_blocks_replayed, 0,
                        "expected 0 replayed metadata block [{image:?} {stage:?}]"
                    );
                }

                // (b) 重置后 journal SB 字段对拍（两侧都重置，replay 与否都 store）。
                assert_reset_sb_eq(&old_disk, &probe_blocks, new_sb, bs);

                // (d) 兜底：整盘逐字节相等（已先确认 SB 重置字节同 → 不误报）。
                assert_disk_eq(&old_disk, &new_disk);
            }
            (Err(_), Err(_)) => { /* 两侧同样失败也算 parity（本测试 fixture 不应触发） */ }
            (o, n) => panic!(
                "recover Ok/Err divergence [{image:?} {stage:?}]: old={:?} new_is_ok={}",
                o.as_ref().map(|_| ()),
                n.is_ok()
            ),
        }
    }

    /// AfterSuperblock（已 commit、SB 已指向）→ 两侧 replay → HOME 逐字节 + SB 重置 + RecoverResult
    /// 一致。**核心 happy-path recovery parity。**
    #[ktest]
    fn journal_recover_replay_parity() {
        for image in [EXT4_IMAGE, EXT4_MULTIGROUP_IMAGE, EXT4_NOCSUM_IMAGE] {
            let probe = MemDisk::from_image(image);
            let (_, geom) = resolve_journal_area(&probe);
            let bs = geom.block_size as usize;

            // 三个 home 块（升序 + 乱序混入，验 BTreeMap 定序在 replay 序里也保持）。
            let writes = [
                JournalMetaWrite { block_nr: 21, image: recover_block_image(bs, 0x11) },
                JournalMetaWrite { block_nr: 5, image: recover_block_image(bs, 0x55) },
                JournalMetaWrite { block_nr: 13, image: recover_block_image(bs, 0x99) },
            ];

            run_recover_stage(image, &writes, JournalCrashStage::AfterSuperblock, true);
        }
    }

    /// 4 崩溃 stage 覆盖：BeforeDescriptor / BeforeCommitBlock / AfterCommitBlock（均 SB 未指向 →
    /// needs_recovery=false → 不 replay）/ AfterSuperblock（SB 已指向 → replay）。各 stage replay
    /// 后 HOME 逐字节 + SB + RecoverResult 一致。
    #[ktest]
    fn journal_recover_crash_stage_parity() {
        for image in [EXT4_IMAGE, EXT4_NOCSUM_IMAGE] {
            let probe = MemDisk::from_image(image);
            let (_, geom) = resolve_journal_area(&probe);
            let bs = geom.block_size as usize;

            let writes = [
                JournalMetaWrite { block_nr: 13, image: recover_block_image(bs, 0x20) },
                JournalMetaWrite { block_nr: 5, image: recover_block_image(bs, 0x40) },
                JournalMetaWrite { block_nr: 21, image: recover_block_image(bs, 0x60) },
            ];

            // 未 commit / SB 未指向 → recover 不 replay（home 不变）。
            run_recover_stage(image, &writes, JournalCrashStage::BeforeDescriptor, false);
            run_recover_stage(image, &writes, JournalCrashStage::BeforeCommitBlock, false);
            run_recover_stage(image, &writes, JournalCrashStage::AfterCommitBlock, false);
            // SB 已指向 → replay。
            run_recover_stage(image, &writes, JournalCrashStage::AfterSuperblock, true);
        }
    }

    /// 坏 csum parity：造一个已 commit 态（AfterSuperblock）的盘，再**破坏 commit 块**（改其序号，
    /// 使 commit.sequence != descriptor.sequence）——这是 ext4_rs recovery 唯一的「事务无效→停」
    /// 判据（recovery 不校 crc）。两侧 SCAN 都在该事务前停（视为未 commit），不 replay；HOME 逐字节
    /// 一致（都没写）。验证「坏 csum/坏 commit 停 replay 一致」。
    #[ktest]
    fn journal_recover_badcsum_parity() {
        use crate::fs::ext4::core::journal::format::{RawJournalHeader, JBD2_COMMIT_BLOCK};

        for image in [EXT4_IMAGE, EXT4_NOCSUM_IMAGE] {
            let probe = MemDisk::from_image(image);
            let (probe_blocks, geom) = resolve_journal_area(&probe);
            let bs = geom.block_size as usize;

            let writes = [
                JournalMetaWrite { block_nr: 13, image: recover_block_image(bs, 0x33) },
                JournalMetaWrite { block_nr: 21, image: recover_block_image(bs, 0x77) },
            ];

            // 造已 commit + SB 指向的盘。
            let mut crashed = build_crashed_journal_image(image, &writes, JournalCrashStage::AfterSuperblock);

            // 定位 commit 块（journal 逻辑块）：descriptor=s_start，commit=descriptor + N(payload) + 1。
            // 这里事务的 commit 在 journal 区里：descriptor 块后跟 writes.len() 个 payload，再 commit。
            // 解出当前 s_start（崩溃态的 SB 已指向 descriptor）。
            let crashed_disk = MemDisk::from_image(&crashed);
            let (_, crashed_geom) = resolve_journal_area(&crashed_disk);
            let start = crashed_geom.start;
            assert_ne!(start, 0, "AfterSuperblock crash must leave s_start != 0");
            // commit 逻辑块 = start + writes.len() + 1（descriptor + payloads + commit）。
            // journal 环在该事务范围内不回绕（fixture maxlen 远大于 N+2），故线性偏移成立。
            let commit_logical = start + writes.len() as u32 + 1;
            let commit_phys = probe_blocks[commit_logical as usize];
            let commit_off = (commit_phys as usize) * bs;

            // 破坏 commit 块：读出 12B 头，把 h_sequence 改成一个不匹配的值，写回。
            // recovery 据 commit.sequence == descriptor.sequence 判事务有效——改坏即「该事务无效」。
            let mut hdr_bytes = vec![0u8; size_of::<RawJournalHeader>()];
            crashed_disk.read_at(commit_off, hdr_bytes.as_mut_slice());
            let hdr = RawJournalHeader::from_bytes(&hdr_bytes);
            assert_eq!(
                hdr.blocktype(),
                JBD2_COMMIT_BLOCK,
                "located block must be the commit block [{image:?}]"
            );
            // 改 h_sequence（偏移 8..12，大端）成一个绝不匹配 descriptor.sequence 的值。
            let bad_seq = hdr.sequence().wrapping_add(0x5A5A_5A5A).wrapping_add(1);
            let corrupt = RawJournalHeader::new(JBD2_COMMIT_BLOCK, bad_seq);
            crashed[commit_off..commit_off + size_of::<RawJournalHeader>()]
                .copy_from_slice(corrupt.as_bytes());

            // 现在两侧 recover：commit.seq != descriptor.seq → SCAN 视该事务未 commit → 不 replay。
            let pristine = MemDisk::from_image(image);
            let old_disk = MemDisk::from_image(&crashed);
            let new_disk = MemDisk::from_image(&crashed);

            let old_res = old_journal_recover(&old_disk);
            let new_res = core_journal_recover(&new_disk);
            match (&old_res, &new_res) {
                (Ok(old), Ok((new, new_sb))) => {
                    assert_eq!(
                        old.transactions_replayed, new.transactions_replayed,
                        "badcsum transactions_replayed mismatch [{image:?}]"
                    );
                    assert_eq!(
                        new.transactions_replayed, 0,
                        "corrupt commit → 0 replayed txns [{image:?}]"
                    );
                    assert_eq!(
                        old.metadata_blocks_replayed, new.metadata_blocks_replayed,
                        "badcsum metadata_blocks_replayed mismatch [{image:?}]"
                    );
                    // HOME 块未被写（都 stop 在坏 commit 前）：两侧 + pristine 三者一致。
                    for w in &writes {
                        let old_home = read_home_block(&old_disk, w.block_nr, bs);
                        let new_home = read_home_block(&new_disk, w.block_nr, bs);
                        let orig = read_home_block(&pristine, w.block_nr, bs);
                        assert_eq!(old_home, new_home, "badcsum HOME {} engine mismatch [{image:?}]", w.block_nr);
                        assert_eq!(new_home, orig, "badcsum HOME {} must be unchanged [{image:?}]", w.block_nr);
                    }
                    assert_reset_sb_eq(&old_disk, &probe_blocks, new_sb, bs);
                    assert_disk_eq(&old_disk, &new_disk);
                }
                (Err(_), Err(_)) => {}
                (o, n) => panic!(
                    "badcsum recover Ok/Err divergence [{image:?}]: old={:?} new_is_ok={}",
                    o.as_ref().map(|_| ()),
                    n.is_ok()
                ),
            }
        }
    }

    // =================================================================
    // Phase 5 Task 6：边角差分 + 收口。
    //
    // 在 Task 1-5 已有的 commit/recover/space/revoke 差分之上补边角，硬化对拍面。
    // 全部 **test-only**——无生产改动（core recovery 的 parse 路径已全程 bounds-checked：
    // `raw.get(..)` / `try_into().ok()?` / `checked_mul`，对坏/截断输入返回 None/Err，
    // 不会 panic；本节坐实这一点而非引入新代码）。
    //
    // - `journal_tag_form_used_parity`：三镜像 journal SB 的 feature_incompat 实测——确认它们
    //   用的是 **8 字节 v2-form tag**（CSUM_V2/V3/64BIT 位全关），断言 recovery 用的 `tag_length`
    //   走 8 字节路径。**不**伪造镜像没有的特性（v3 16B / 64BIT 12B 路径的解码已由 recovery.rs
    //   的 read_descriptor_tag 单测覆盖类型层；此处只 ground 实测镜像走的那条）。
    // - `journal_escape_recover_parity`：escape 块（payload 首 4B==大端 magic）commit 后再 recover——
    //   两侧 replay 把 home 还原成**原始**字节（含 magic 头），HOME 逐字节 + 整盘 parity。
    //   补上 Task 3 escape（只验 commit 写盘 4B 零填）缺的「replay 还原」半程。
    // - `journal_empty_no_recovery_parity`：pristine 镜像（s_start==0）——两侧 needs_recovery=false、
    //   recover 不 replay、journal 区逐字节不变；整盘不变（no-op）。
    // - `journal_recover_corrupt_descriptor_parity`：AfterSuperblock 崩溃态（s_start!=0）后**破坏
    //   descriptor 块头**（坏 magic / 坏 blocktype）——两侧 SCAN 把它当无效事务、优雅停（0 replay），
    //   HOME 不变 + 整盘 parity。坐实「坏头不 panic、停得一致」。
    // - `journal_recover_corrupt_input_core_graceful`：把若干结构性坏/截断 journal 区喂给 core
    //   `recover`——core 必返回 Ok/Err（**绝不 panic**），且不 replay 越界块。ext4_rs 在同输入也优雅
    //   则双侧对拍；本测试聚焦 core 健壮性（无生产改动地坐实 bounds-checked 解析）。
    // =================================================================

    /// 实测三镜像 journal SB 的 tag-form（feature_incompat），断言它们都走 **8 字节 v2-form tag**。
    /// recovery 的 `tag_length`：CSUM_V3→16 / 64BIT→12 / 否则 8。三镜像 feat_incompat=0 → 全 8B。
    /// 这是 grounded 断言（不伪造特性）：v3/64BIT 解码路径由类型层单测覆盖，此处坐实镜像走的那条。
    #[ktest]
    fn journal_tag_form_used_parity() {
        use crate::fs::ext4::core::journal::format::{
            JBD2_FEATURE_INCOMPAT_64BIT, JBD2_FEATURE_INCOMPAT_CSUM_V2,
            JBD2_FEATURE_INCOMPAT_CSUM_V3,
        };
        use crate::fs::ext4::core::journal::superblock::load_journal_sb;

        for image in [EXT4_IMAGE, EXT4_MULTIGROUP_IMAGE, EXT4_NOCSUM_IMAGE] {
            let disk = MemDisk::from_image(image);
            let (physical_blocks, geom) = resolve_journal_area(&disk);
            let bs = geom.block_size as usize;
            let sb = load_journal_sb(&disk, &physical_blocks, bs).expect("load journal SB");

            let feat = sb.feature_incompat();
            // 三镜像实测：journal feature_incompat 不含 CSUM_V2/V3/64BIT → 8 字节 v2-form tag。
            assert_eq!(
                feat & (JBD2_FEATURE_INCOMPAT_CSUM_V2
                    | JBD2_FEATURE_INCOMPAT_CSUM_V3
                    | JBD2_FEATURE_INCOMPAT_64BIT),
                0,
                "image journal must use 8-byte v2-form tags (no CSUM_V2/V3/64BIT) [{image:?}], \
                 feature_incompat={feat:#010x}"
            );
            assert!(
                !sb.has_checksum_v2_or_v3(),
                "journal csum gate must be OFF on these images [{image:?}]"
            );
            assert!(
                !sb.has_incompat_feature(JBD2_FEATURE_INCOMPAT_64BIT),
                "journal 64BIT must be OFF on these images [{image:?}]"
            );
        }
    }

    /// escape 块的 commit + recover 全程 parity：payload 首 4 字节 == 大端 JBD2 magic 的块，
    /// commit 写盘时两侧都把那 4 字节零填 + tag 置 ESCAPE（Task 3 已验 commit 半程）；**本测试补
    /// recover 半程**——AfterSuperblock 崩溃态后两侧 replay，escape 块还原成**原始**字节（含 magic 头），
    /// HOME 逐字节 + 整盘 parity。
    #[ktest]
    fn journal_escape_recover_parity() {
        use crate::fs::ext4::core::journal::format::JBD2_MAGIC;

        for image in [EXT4_IMAGE, EXT4_NOCSUM_IMAGE] {
            let probe = MemDisk::from_image(image);
            let (_, geom) = resolve_journal_area(&probe);
            let bs = geom.block_size as usize;

            // 一个 escape 命中块（首 4B = 大端 magic，其余非零）+ 一个普通块。
            let mut escape_img = recover_block_image(bs, 0x77);
            escape_img[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
            let writes = [
                JournalMetaWrite { block_nr: 9, image: escape_img.clone() },
                JournalMetaWrite { block_nr: 17, image: recover_block_image(bs, 0x33) },
            ];

            // 造已 commit + SB 指向的盘，两侧 recover → replay → HOME 逐字节 + 整盘 parity
            // （run_recover_stage 内部对 replayed 块断言 new_home == w.image，即还原回原始字节）。
            run_recover_stage(image, &writes, JournalCrashStage::AfterSuperblock, true);
        }
    }

    /// 空 journal（pristine 镜像，s_start==0）no-op parity：两侧 needs_recovery=false、recover 不 replay、
    /// journal 区逐字节不变、整盘不变。坐实 `needs_recovery == (s_start != 0)`（PARITY recovery.rs:28-30）。
    #[ktest]
    fn journal_empty_no_recovery_parity() {
        use crate::fs::ext4::core::journal::recovery::needs_recovery;
        use crate::fs::ext4::core::journal::superblock::load_journal_sb;

        for image in [EXT4_IMAGE, EXT4_MULTIGROUP_IMAGE, EXT4_NOCSUM_IMAGE] {
            let probe = MemDisk::from_image(image);
            let (probe_blocks, geom) = resolve_journal_area(&probe);
            let bs = geom.block_size as usize;

            // pristine 镜像的 journal SB：s_start==0 → 不需恢复。
            let pristine_sb =
                load_journal_sb(&probe, &probe_blocks, bs).expect("load pristine journal SB");
            assert_eq!(pristine_sb.start(), 0, "pristine image journal s_start must be 0 [{image:?}]");
            assert!(
                !needs_recovery(&pristine_sb),
                "pristine journal must not need recovery [{image:?}]"
            );

            // 起点字节快照（journal 区 + 整盘）。
            let pre_journal = snapshot_journal_area(&probe, &probe_blocks, bs);
            let pre_disk = probe.backing().lock().clone();

            // 两侧 recover：均应为 no-op（0 replay），用 `match (old,new)` 对拍。
            let old_disk = MemDisk::from_image(image);
            let new_disk = MemDisk::from_image(image);
            let old_res = old_journal_recover(&old_disk);
            let new_res = core_journal_recover(&new_disk);
            match (&old_res, &new_res) {
                (Ok(old), Ok((new, _new_sb))) => {
                    assert_eq!(old.transactions_replayed, 0, "old empty recover 0 txn [{image:?}]");
                    assert_eq!(new.transactions_replayed, 0, "core empty recover 0 txn [{image:?}]");
                    assert_eq!(
                        new.metadata_blocks_replayed, 0,
                        "core empty recover 0 metadata blocks [{image:?}]"
                    );
                    assert_eq!(new.last_sequence, None, "core empty recover last_sequence None [{image:?}]");
                }
                (o, n) => panic!(
                    "empty recover Ok/Err divergence [{image:?}]: old={:?} new_is_ok={}",
                    o.as_ref().map(|_| ()),
                    n.is_ok()
                ),
            }

            // journal 区 + 整盘逐字节不变（no-op：needs_recovery=false 早返，不动盘）。
            let post_journal_new = snapshot_journal_area(&new_disk, &probe_blocks, bs);
            assert_eq!(
                post_journal_new, pre_journal,
                "core empty recover must leave journal area unchanged [{image:?}]"
            );
            assert_eq!(
                new_disk.backing().lock().clone(),
                pre_disk,
                "core empty recover must leave whole disk unchanged [{image:?}]"
            );
            // 旧侧（ext4_rs）also no-op：整盘字节与新侧一致（兜底，两侧都没动盘）。
            assert_disk_eq(&old_disk, &new_disk);
        }
    }

    /// 坏 descriptor 头防御 parity：AfterSuperblock 崩溃态（s_start!=0、needs_recovery=true）后破坏
    /// **descriptor 块头**——分别测「坏 magic」「坏 blocktype（改成非 DESCRIPTOR）」两种坏法。两侧 SCAN
    /// 在 `read_header` / blocktype 检查处把它当无效事务，优雅停（0 replay，**不 panic**），HOME 不变 +
    /// 整盘 parity + SB 仍被重置一致。
    #[ktest]
    fn journal_recover_corrupt_descriptor_parity() {
        use crate::fs::ext4::core::journal::format::{
            RawJournalHeader, JBD2_COMMIT_BLOCK, JBD2_DESCRIPTOR_BLOCK,
        };

        // corruption kind：0 = 坏 magic（清头 4 字节）；1 = 坏 blocktype（改成 COMMIT，非 DESCRIPTOR）。
        for image in [EXT4_IMAGE, EXT4_NOCSUM_IMAGE] {
            for corrupt_kind in 0u8..2 {
                let probe = MemDisk::from_image(image);
                let (probe_blocks, geom) = resolve_journal_area(&probe);
                let bs = geom.block_size as usize;

                let writes = [
                    JournalMetaWrite { block_nr: 13, image: recover_block_image(bs, 0x44) },
                    JournalMetaWrite { block_nr: 21, image: recover_block_image(bs, 0x88) },
                ];

                // 已 commit + SB 指向的盘。
                let mut crashed =
                    build_crashed_journal_image(image, &writes, JournalCrashStage::AfterSuperblock);

                // 定位 descriptor 块（崩溃态 s_start 指向它）。
                let crashed_disk = MemDisk::from_image(&crashed);
                let (_, crashed_geom) = resolve_journal_area(&crashed_disk);
                let start = crashed_geom.start;
                assert_ne!(start, 0, "AfterSuperblock crash must set s_start != 0 [{image:?}]");
                let desc_phys = probe_blocks[start as usize];
                let desc_off = (desc_phys as usize) * bs;

                // 确认它本是合法 descriptor。
                let mut hdr_bytes = vec![0u8; size_of::<RawJournalHeader>()];
                crashed_disk.read_at(desc_off, hdr_bytes.as_mut_slice());
                let hdr = RawJournalHeader::from_bytes(&hdr_bytes);
                assert_eq!(
                    hdr.blocktype(),
                    JBD2_DESCRIPTOR_BLOCK,
                    "located block must be the descriptor [{image:?}]"
                );

                // 破坏它。
                match corrupt_kind {
                    0 => {
                        // 坏 magic：头 4 字节清零（is_valid_magic 失败 → read_header=None）。
                        crashed[desc_off..desc_off + 4].fill(0);
                    }
                    _ => {
                        // 坏 blocktype：magic 保留、blocktype 改成 COMMIT（非 DESCRIPTOR）。
                        let corrupt = RawJournalHeader::new(JBD2_COMMIT_BLOCK, hdr.sequence());
                        crashed[desc_off..desc_off + size_of::<RawJournalHeader>()]
                            .copy_from_slice(corrupt.as_bytes());
                    }
                }

                let pristine = MemDisk::from_image(image);
                let old_disk = MemDisk::from_image(&crashed);
                let new_disk = MemDisk::from_image(&crashed);

                // 两侧 recover：坏 descriptor → SCAN 视无效 → 优雅停（不 panic），0 replay。
                let old_res = old_journal_recover(&old_disk);
                let new_res = core_journal_recover(&new_disk);
                match (&old_res, &new_res) {
                    (Ok(old), Ok((new, new_sb))) => {
                        assert_eq!(
                            old.transactions_replayed, new.transactions_replayed,
                            "corrupt-desc transactions_replayed mismatch [{image:?} kind={corrupt_kind}]"
                        );
                        assert_eq!(
                            new.transactions_replayed, 0,
                            "corrupt descriptor → 0 replayed txns [{image:?} kind={corrupt_kind}]"
                        );
                        assert_eq!(
                            old.metadata_blocks_replayed, new.metadata_blocks_replayed,
                            "corrupt-desc metadata_blocks_replayed mismatch [{image:?} kind={corrupt_kind}]"
                        );
                        // HOME 块未被写：两侧 + pristine 三者一致。
                        for w in &writes {
                            let old_home = read_home_block(&old_disk, w.block_nr, bs);
                            let new_home = read_home_block(&new_disk, w.block_nr, bs);
                            let orig = read_home_block(&pristine, w.block_nr, bs);
                            assert_eq!(
                                old_home, new_home,
                                "corrupt-desc HOME {} engine mismatch [{image:?} kind={corrupt_kind}]",
                                w.block_nr
                            );
                            assert_eq!(
                                new_home, orig,
                                "corrupt-desc HOME {} must be unchanged [{image:?} kind={corrupt_kind}]",
                                w.block_nr
                            );
                        }
                        assert_reset_sb_eq(&old_disk, &probe_blocks, new_sb, bs);
                        assert_disk_eq(&old_disk, &new_disk);
                    }
                    (Err(_), Err(_)) => { /* 两侧同样失败也算 parity */ }
                    (o, n) => panic!(
                        "corrupt-desc recover Ok/Err divergence [{image:?} kind={corrupt_kind}]: \
                         old={:?} new_is_ok={}",
                        o.as_ref().map(|_| ()),
                        n.is_ok()
                    ),
                }
            }
        }
    }

    /// core 对结构性坏 / 截断 journal 区的健壮性：直接在 SB 上写一个**指向坏内容的 s_start**
    /// （needs_recovery=true 但 descriptor 区是垃圾 / 全零 / 越界状），喂给 core `recover`——
    /// core 必返回 `Ok`/`Err`（**绝不 panic**），且不把任何块 replay 到 home（无有效事务）。
    /// 这坐实 core recovery 的 parse 路径全程 bounds-checked（`raw.get(..)`/`try_into().ok()?`/
    /// `checked_mul`），是 forbid(unsafe) 下的健壮性下限——无生产改动。
    ///
    /// ext4_rs 在同输入也优雅时双侧对拍（needs_recovery / 0 replay）；本测试主验 core 侧。
    #[ktest]
    fn journal_recover_corrupt_input_core_graceful() {
        use crate::fs::ext4::core::journal::format::{RawJournalSuperblock, JBD2_SUPERBLOCK_SIZE};
        use crate::fs::ext4::core::journal::recovery::{needs_recovery, recover, RecoverCtx};
        use crate::fs::ext4::core::journal::superblock::{journal_sb_checksum, load_journal_sb};

        for image in [EXT4_IMAGE, EXT4_NOCSUM_IMAGE] {
            let probe = MemDisk::from_image(image);
            let (probe_blocks, geom) = resolve_journal_area(&probe);
            let bs = geom.block_size as usize;
            let usable = geom.maxlen.saturating_sub(geom.first);

            // s_start 候选：环内合法但内容是垃圾的位置 + 一个接近 maxlen 边界的位置。
            // （s_start 必须在 [first, maxlen) 内 JournalSpace::from_superblock 才不报错；
            //  本测试要的是「needs_recovery=true 但 descriptor 是垃圾」→ 走完 SCAN/parse 路径。）
            let mut starts = vec![geom.first];
            if usable > 2 {
                starts.push(geom.first + 1);
                starts.push(geom.maxlen - 1);
            }

            for &bad_start in &starts {
                // 取 pristine 盘字节，改写 journal SB：s_start=bad_start（needs_recovery=true），
                // 重算 SB csum；再把整个 descriptor 区填成垃圾（0xEE），制造坏/无效事务输入。
                let mut bytes = image.to_vec();
                let sb_pblock = probe_blocks[0] as usize;
                let sb_off = sb_pblock * bs;

                // 改 SB。
                let mut sb = load_journal_sb(&probe, &probe_blocks, bs).expect("load SB");
                sb.set_start(bad_start);
                let mut sb_image = [0u8; JBD2_SUPERBLOCK_SIZE];
                sb_image.copy_from_slice(sb.as_bytes());
                let csum = journal_sb_checksum(&sb_image);
                sb.set_checksum(csum);
                bytes[sb_off..sb_off + JBD2_SUPERBLOCK_SIZE].copy_from_slice(sb.as_bytes());

                // 把所有非-SB 的 journal 物理块填成垃圾（0xEE）——descriptor 区全是无效头。
                for &pblock in probe_blocks.iter().skip(1) {
                    let off = (pblock as usize) * bs;
                    bytes[off..off + bs].fill(0xEE);
                }

                // ---- core 侧：必不 panic，返回 Ok/Err，且 0 replay ----
                let new_disk = MemDisk::from_image(&bytes);
                let mut new_sb = load_journal_sb(&new_disk, &probe_blocks, bs)
                    .expect("load corrupted SB (magic/version still valid)");
                assert!(
                    needs_recovery(&new_sb),
                    "bad_start={bad_start} must trigger needs_recovery [{image:?}]"
                );
                let ctx = RecoverCtx {
                    physical_blocks: &probe_blocks,
                    reader: &new_disk,
                    writer: &new_disk,
                    block_size: bs,
                };
                // recover 必须**收敛**（Ok/Err），不 panic；垃圾 descriptor → 0 有效事务。
                match recover(&ctx, &mut new_sb) {
                    Ok(res) => {
                        assert_eq!(
                            res.transactions_replayed, 0,
                            "garbage descriptor → 0 replayed txns [{image:?} start={bad_start}]"
                        );
                        assert_eq!(
                            res.metadata_blocks_replayed, 0,
                            "garbage descriptor → 0 replayed metadata blocks [{image:?} start={bad_start}]"
                        );
                    }
                    Err(_) => { /* 优雅 Err 也是 graceful（无 panic）——同样可接受 */ }
                }

                // ---- 旧侧（ext4_rs）：同输入也优雅时双侧对拍 0 replay（透传 Result，不 expect）----
                let old_disk = MemDisk::from_image(&bytes);
                let old_res = old_journal_recover(&old_disk);
                if let Ok(old) = old_res {
                    assert_eq!(
                        old.transactions_replayed, 0,
                        "ext4_rs garbage descriptor → 0 replayed txns [{image:?} start={bad_start}]"
                    );
                }
                // 留意 `RawJournalSuperblock` 不直接对拍盘 SB（两侧都重置成空），本测试只验 graceful。
                let _ = RawJournalSuperblock::from_bytes(&bytes[sb_off..sb_off + JBD2_SUPERBLOCK_SIZE]);
            }
        }
    }
}
