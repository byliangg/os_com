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
        assert_disk_eq, assert_meta_eq, old_journal_commit, resolve_journal_area,
        snapshot_inode_table_group, snapshot_journal_area, snapshot_meta,
        snapshot_meta_with_inodes, DirectMetadataWriter, JournalMetaWrite, MemDisk,
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
}
