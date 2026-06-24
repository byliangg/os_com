// SPDX-License-Identifier: MPL-2.0
use ostd::const_assert;

use super::crc::{ext4_crc32c, EXT4_CRC32_INIT};
use super::inode::{write_back_inode, Inode};
use super::io::{BlockReader, BlockWriter};
use super::metadata_writer::MetadataWriter;
use super::prelude::*;
use super::superblock::RawSuperblock;

pub(super) const EXTENT_MAGIC: u16 = 0xF30A;
const EXT4_EXTENT_HEADER_SIZE: usize = 12;
const EXT4_EXTENT_SIZE: usize = 12;
/// RO-compat metadata_csum 特性位（门控 extent 块 csum）。
/// = ext4_rs `EXT4_FEATURE_RO_COMPAT_METADATA_CSUM`（0x400）。
const RO_COMPAT_METADATA_CSUM: u32 = 0x400;
/// 写态 extent 合并长度上限。= ext4_rs `EXT_INIT_MAX_LEN`（consts.rs:24）。
const EXT_INIT_MAX_LEN: u16 = 32768;
/// block_count 高于此值表示 unwritten extent（实际长度 = block_count - 此值）。
/// = ext4_rs `EXT_INIT_MAX_LEN`（consts.rs:24）= `EXT_INIT_MAX_LEN`。
const UNWRITTEN_MAX_LEN: u16 = 32768;

/// extent 树节点头（12 字节，小端）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub(super) struct RawExtentHeader {
    pub magic: u16,
    pub entries_count: u16,
    pub max_entries_count: u16,
    pub depth: u16,
    pub generation: u32,
}
const_assert!(size_of::<RawExtentHeader>() == 12);

/// extent 树内部索引项（12 字节）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub(super) struct RawExtentIndex {
    pub first_block: u32,
    pub leaf_lo: u32,
    pub leaf_hi: u16,
    pub padding: u16,
}
const_assert!(size_of::<RawExtentIndex>() == 12);

/// extent 叶子项（12 字节）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub(super) struct RawExtent {
    pub first_block: u32,
    pub block_count: u16,
    pub start_hi: u16,
    pub start_lo: u32,
}
const_assert!(size_of::<RawExtent>() == 12);

/// 非根 extent 块尾的校验和（4 字节）。
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub(super) struct RawExtentTail {
    pub et_checksum: u32,
}
const_assert!(size_of::<RawExtentTail>() == 4);

impl RawExtentHeader {
    pub fn magic(&self) -> u16 {
        self.magic
    }
    pub fn entries_count(&self) -> u16 {
        self.entries_count
    }
    pub fn depth(&self) -> u16 {
        self.depth
    }
    /// 是否合法 extent 头（magic == 0xF30A）。
    pub fn is_valid(&self) -> bool {
        self.magic == EXTENT_MAGIC
    }
}

impl RawExtentIndex {
    pub fn first_block(&self) -> u32 {
        self.first_block
    }
    /// 指向的下层块号（leaf_lo | leaf_hi<<32）。
    pub fn leaf(&self) -> Ext4Fsblk {
        (self.leaf_lo as u64) | ((self.leaf_hi as u64) << 32)
    }
}

impl RawExtent {
    pub fn first_block(&self) -> u32 {
        self.first_block
    }
    /// 是否 unwritten extent（block_count 高位标志）。
    pub fn is_unwritten(&self) -> bool {
        self.block_count > UNWRITTEN_MAX_LEN
    }
    /// 实际覆盖块数（去掉 unwritten 标志）。
    pub fn len(&self) -> u16 {
        if self.block_count > UNWRITTEN_MAX_LEN {
            self.block_count - UNWRITTEN_MAX_LEN
        } else {
            self.block_count
        }
    }
    /// 物理起始块号（start_lo | start_hi<<32）。
    pub fn start(&self) -> Ext4Fsblk {
        (self.start_lo as u64) | ((self.start_hi as u64) << 32)
    }

    // ------------------------------------------------------------------
    // 写半部 setter（逐位对齐 ext4_rs `Ext4Extent` 的同名方法）。
    // ------------------------------------------------------------------

    /// 写物理起始块号（lo = pblock&0xffffffff，hi = pblock>>32）。
    /// [对照] ext4_rs `Ext4Extent::store_pblock`（ext4_defs/extents.rs:527）。
    pub fn store_pblock(&mut self, pblock: Ext4Fsblk) {
        self.start_lo = (pblock & 0xffff_ffff) as u32;
        self.start_hi = (pblock >> 32) as u16;
    }

    /// 设实际长度（直写 block_count，不带 unwritten 标志）。
    /// [对照] ext4_rs `Ext4Extent::set_actual_len`（ext4_defs/extents.rs:547）。
    pub fn set_actual_len(&mut self, len: u16) {
        self.block_count = len;
    }

    /// 标记 unwritten（block_count |= EXT_INIT_MAX_LEN）。
    /// [对照] ext4_rs `Ext4Extent::mark_unwritten`（ext4_defs/extents.rs:552）。
    pub fn mark_unwritten(&mut self) {
        self.block_count |= EXT_INIT_MAX_LEN;
    }

    /// 标记 written（保留实际长度，去掉 unwritten 标志）。
    /// [对照] ext4_rs `Ext4Extent::mark_written`（ext4_defs/extents.rs:557）。
    pub fn mark_written(&mut self) {
        self.block_count = self.len();
    }
}

impl RawExtentIndex {
    /// 写指向的下层块号（leaf_lo | leaf_hi<<32），padding 不动。
    /// [对照] ext4_rs `Ext4ExtentIndex::store_pblock`（ext4_defs/extents.rs:502）。
    pub fn store_pblock(&mut self, pblock: Ext4Fsblk) {
        self.leaf_lo = (pblock & 0xffff_ffff) as u32;
        self.leaf_hi = (pblock >> 32) as u16;
    }
}

impl RawExtentHeader {
    /// 构造新节点头。[对照] ext4_rs `Ext4ExtentHeader::new`（ext4_defs/extents.rs）。
    pub fn new(magic: u16, entries: u16, max_entries: u16, depth: u16, generation: u32) -> Self {
        Self {
            magic,
            entries_count: entries,
            max_entries_count: max_entries,
            depth,
            generation,
        }
    }
}

// =====================================================================
// extent 树读半部（Phase 3 Task 2）。
//
// 安全复刻 ext4_rs 的 extent 遍历（`ext4_impls/extents.rs:find_extent`、
// `ext4_defs/extents.rs` 的 binsearch、`ext4_impls/inode.rs:get_pblock_idx_inner`）。
// ext4_rs 用 `transmute::<&[u32;15],&[u8;60]>` 取根 60 字节、用裸指针
// `load_from_u8`/`load_from_u32` 解析节点项；core 一律改 Pod `from_bytes`：
// 根用 `inode.i_block_bytes()`（60 字节），子节点块经 [`BlockReader`] 读出后切片。
//
// 节点项的盘上布局对「根（[u32;15]）」与「内部块（Vec<u8>）」**逐字节一致**——
// 根的 `[u32;15]` 即 60 字节小端，与内部块前缀同布局；ext4_rs 之所以分两套
// `load_from_u32`/`load_from_u8` 只因载体类型不同，字节语义相同。故 core 把两者
// 统一成「一段字节 + entries_count」，binsearch/取项一律走字节偏移 `12 + pos*12`，
// 与 ext4_rs 逐字节等价（差分会暴露任何偏差）。
// =====================================================================

/// extent 树节点的只读字节视图：节点头 + 紧随其后的项区字节。
///
/// `bytes` 是「该节点的完整字节」（根=60 字节 i_block；内部=block_size 字节块），
/// 头占前 12 字节，第 `pos` 项位于 `12 + pos * 12`。
struct NodeView<'a> {
    header: RawExtentHeader,
    bytes: &'a [u8],
    /// 该节点可容纳的最大项数（按项大小算，与 ext4_rs `entry_capacity` 一致）。
    capacity: usize,
}

impl<'a> NodeView<'a> {
    /// 解析节点头并算容量。`is_root` 为真时容量按 60 字节根算（=(60-12)/12=4），
    /// 否则按整段字节长算（=(len-12)/12）。对应 ext4_rs `entry_capacity`。
    fn new(bytes: &'a [u8], is_root: bool) -> Self {
        let header = RawExtentHeader::from_bytes(&bytes[..size_of::<RawExtentHeader>()]);
        // PARITY: ext4_rs `entry_capacity`（ext4_defs/extents.rs:281）——
        //   根 = (15*4 - 12)/entry_size；内部 = (data.len() - 12)/entry_size。
        //   extent/index 项均 12 字节，故两套容量公式同形，统一成 (len-12)/12。
        let span = if is_root {
            15 * size_of::<u32>()
        } else {
            bytes.len()
        };
        let capacity = span.saturating_sub(size_of::<RawExtentHeader>()) / size_of::<RawExtent>();
        Self {
            header,
            bytes,
            capacity,
        }
    }

    /// 有效项数（与 ext4_rs `valid_entries_count` 一致）：
    /// entries==0 → None；entries>capacity → None；否则 Some(entries)。
    fn valid_entries(&self) -> Option<usize> {
        let entries = self.header.entries_count as usize;
        if entries == 0 {
            return None;
        }
        if entries > self.capacity {
            return None;
        }
        Some(entries)
    }

    /// 取第 `pos` 个 extent 叶项（字节偏移 12 + pos*12）。越界 → None。
    /// 对应 ext4_rs `ExtentNode::get_extent`。
    fn get_extent(&self, pos: usize) -> Option<RawExtent> {
        let entries = self.valid_entries()?;
        if pos >= entries {
            return None;
        }
        let off = size_of::<RawExtentHeader>() + pos * size_of::<RawExtent>();
        let end = off.checked_add(size_of::<RawExtent>())?;
        if end > self.bytes.len() {
            return None;
        }
        Some(RawExtent::from_bytes(&self.bytes[off..end]))
    }

    /// 取第 `pos` 个 index 项（字节偏移 12 + pos*12）。越界 → EIO。
    /// 对应 ext4_rs `ExtentNode::get_index`（越界返回 EIO）。
    fn get_index(&self, pos: usize) -> Result<RawExtentIndex> {
        let Some(entries) = self.valid_entries() else {
            return Err(Error::with_message(
                Errno::EIO,
                "invalid extent index entries_count",
            ));
        };
        if pos >= entries {
            return Err(Error::with_message(
                Errno::EIO,
                "extent index position out of range",
            ));
        }
        let off = size_of::<RawExtentHeader>() + pos * size_of::<RawExtentIndex>();
        let end = off
            .checked_add(size_of::<RawExtentIndex>())
            .filter(|&e| e <= self.bytes.len())
            .ok_or_else(|| Error::with_message(Errno::EIO, "extent index offset out of bounds"))?;
        Ok(RawExtentIndex::from_bytes(&self.bytes[off..end]))
    }

    /// 找 first_block <= lblock 的最右一项的位置（空节点 → None）。
    /// 逐位复刻 ext4_rs `binsearch_extent_pos`（ext4_defs/extents.rs:309）。
    fn binsearch_pos(&self, lblock: Ext4Lblk) -> Option<usize> {
        let entries = self.valid_entries()?;
        let mut l = 0usize;
        let mut r = entries;
        while l < r {
            let m = l + (r - l) / 2;
            // 用 extent 视图读 first_block（index/extent 的 first_block 同在偏移 0）。
            let off = size_of::<RawExtentHeader>() + m * size_of::<RawExtent>();
            let first_block = RawExtent::from_bytes(&self.bytes[off..off + size_of::<RawExtent>()])
                .first_block();
            // PARITY: ext4_rs `l < first_block ? r=m : l=m+1`（含 lblock < first_block）。
            if lblock < first_block {
                r = m;
            } else {
                l = m + 1;
            }
        }
        Some(l.saturating_sub(1))
    }

    /// 找包含 lblock 的 extent。对应 ext4_rs `binsearch_extent`（ext4_defs/extents.rs:351）。
    fn binsearch_extent(&self, lblock: Ext4Lblk) -> Option<(RawExtent, usize)> {
        let pos = self.binsearch_pos(lblock)?;
        let ext = self.get_extent(pos)?;
        let ext_len = ext.len() as u32;
        let ext_end = ext.first_block().checked_add(ext_len)?;
        if lblock >= ext.first_block() && lblock < ext_end {
            Some((ext, pos))
        } else {
            None
        }
    }

    /// 找最接近 lblock 的 index 位置。对应 ext4_rs `binsearch_idx`（ext4_defs/extents.rs:365）。
    /// index 与 extent 的 first_block 同在偏移 0，二分逻辑同 `binsearch_pos`。
    fn binsearch_idx(&self, lblock: Ext4Lblk) -> Option<usize> {
        self.binsearch_pos(lblock)
    }
}

/// extent 树遍历路径中的一层节点（语义对齐 ext4_rs `ExtentPathNode`）。
///
/// 控制器歧义裁决 #1：core 不必逐字段同名 ext4_rs；要钉死的是**遍历序**与最终
/// 解析出的 `(extent, pblock)`。这里保留 `extent`/`pblock`，让三个消费者
/// （`get_pblock_idx_state`/`map_blocks`/`read_at`）对叶节点做与 ext4_rs **同样**的
/// 区间再校验。`pblock_of_node == 0` 表示该节点是 i_block 根（沿用 ext4_rs 约定）。
pub(super) struct ExtentPathNode {
    /// 该层节点头。
    pub header: RawExtentHeader,
    /// 内部层选中的 index（叶层为 None）。
    pub index: Option<RawExtentIndex>,
    /// 叶层选中的 extent（内部层为 None）。
    pub extent: Option<RawExtent>,
    /// 选中项在本节点内的位置。
    pub position: usize,
    /// 本层解析出的物理块号：内部层=子节点块号；叶层=映射出的数据块号（命中时）。
    pub pblock: Ext4Fsblk,
    /// 本节点自身所在的物理块号（0 表示根在 i_block）。
    pub pblock_of_node: usize,
}

/// extent 树搜索路径（语义对齐 ext4_rs `SearchPath`）。逐层 push 节点，
/// 末项即 `find_extent` 的解析结果（叶层节点）。
pub(super) struct SearchPath {
    pub path: Vec<ExtentPathNode>,
}

impl SearchPath {
    fn new() -> Self {
        SearchPath { path: Vec::new() }
    }

    /// 末层节点（=解析结果），对齐 ext4_rs `path.path.last()`。
    pub fn last(&self) -> Option<&ExtentPathNode> {
        self.path.last()
    }
}

/// 在 extent 树中查找 `lblock` 的路径。逐字节复刻 ext4_rs
/// `Ext4::find_extent`（ext4_impls/extents.rs:183）。
///
/// 流程：从 inode 的 60 字节 i_block 根起（ext4_rs 用 `transmute<&[u32;15],&[u8;60]>`，
/// core 改 `inode.i_block_bytes()` 安全取），按 `header.depth` 下行——每层
/// `binsearch_idx` 选 index、`get_index` 取子块号、经 [`BlockReader`] 读子块、递归；
/// 到 depth==0 用 `binsearch_extent` 建叶节点。无匹配 index → ENOENT；叶层未命中
/// 则回退建一个带 `binsearch_pos` 处 extent、`pblock=0` 的节点（与 ext4_rs 一致，
/// 消费者会对该 fallback extent 做区间再校验、最终判为 hole）。
///
/// 子节点块读：ext4_rs 走 `block_device.read_offset(blk*bs)`（返回一个块字节，
/// 不足/超出 resize/truncate 到 block_size）；core 用 `BlockReader::read_at` 读满
/// `block_size` 字节进缓冲，等价。
pub(super) fn find_extent(
    reader: &dyn BlockReader,
    sb: &RawSuperblock,
    inode: &Inode,
    lblock: Ext4Lblk,
) -> Result<SearchPath> {
    let block_size = sb.block_size();
    let mut search_path = SearchPath::new();

    // 根：i_block 的 60 字节（安全 Pod，替代 ext4_rs 的 transmute 根）。
    let root_bytes = inode.i_block_bytes();
    let mut node = NodeView::new(&root_bytes, true);
    let mut depth = node.header.depth;

    // 子节点块字节缓冲（下行循环里复用；持有 Vec 以满足借用期）。
    let mut child_buf: Vec<u8> = Vec::new();
    let mut pblock_of_node: usize = 0;

    // depth > 0：逐层下行。
    while depth > 0 {
        match node.binsearch_idx(lblock) {
            Some(pos) => {
                let index = node.get_index(pos)?;
                let next_block = index.leaf();
                let next_block_usize = usize::try_from(next_block).map_err(|_| {
                    Error::with_message(
                        Errno::EIO,
                        "extent index points to block out of usize range",
                    )
                })?;

                search_path.path.push(ExtentPathNode {
                    header: node.header,
                    index: Some(index),
                    extent: None,
                    position: pos,
                    pblock: next_block,
                    pblock_of_node,
                });

                // 读子块满 block_size 字节（ext4_rs read_offset 的 resize/truncate 等价）。
                child_buf = vec![0u8; block_size];
                reader.read_at(next_block_usize * block_size, child_buf.as_mut_slice());
                node = NodeView::new(&child_buf, false);
                depth -= 1;
                pblock_of_node = next_block_usize;
            }
            None => {
                return Err(Error::with_message(Errno::ENOENT, "Extentindex not found"));
            }
        }
    }

    // depth == 0：叶层。
    if let Some((extent, pos)) = node.binsearch_extent(lblock) {
        // PARITY: pblock = lblock - first_block + extent.start()（ext4_impls/extents.rs:244）。
        let pblock = lblock as u64 - extent.first_block() as u64 + extent.start();
        search_path.path.push(ExtentPathNode {
            header: node.header,
            index: None,
            extent: Some(extent),
            position: pos,
            pblock,
            pblock_of_node,
        });
        Ok(search_path)
    } else {
        // PARITY: 未命中时仍 push 一个 fallback 节点（extent = binsearch_pos 处的项、pblock=0）；
        //   消费者会对该 extent 做 [first, first+len) 区间再校验，lblock 不在其内 → hole。
        let mut fallback_pos = 0usize;
        let mut fallback_extent = None;
        if let Some(pos) = node.binsearch_pos(lblock) {
            fallback_pos = pos;
            fallback_extent = node.get_extent(pos);
        }
        search_path.path.push(ExtentPathNode {
            header: node.header,
            index: None,
            extent: fallback_extent,
            position: fallback_pos,
            pblock: 0,
            pblock_of_node,
        });
        Ok(search_path)
    }
}

/// 逻辑块 `lblock` → `(物理块号, 是否 unwritten)`，extent 映射。
/// 逐字节复刻 ext4_rs `get_pblock_idx_inner` 的 extent 分支
/// （ext4_impls/inode.rs:255），返回 `Ok(None)` 表示 hole（对齐 ext4_rs 的 ENOENT）。
///
/// 流程：`find_extent` → 取末层 extent → 若 `lblock ∈ [first, first+actual_len)`：
/// `fblock = extent.start() + (lblock - first)`，`fblock >= blocks_count → EIO`，
/// 返回 `Some((fblock, is_unwritten))`；否则 hole → `Ok(None)`。
///
/// PARITY（allow_refresh 省略）：ext4_rs `get_pblock_idx_inner` 带 `allow_refresh`——
/// 末层未命中或路径缺失时，重载 inode（`get_inode_ref`）再比对、若 inode 被并发改过则
/// 用新视图重试一次。该重试仅在 inode 于两次查找间被并发改写时才触发；core 的读路径在
/// 共享 guard 下、单线程差分中 inode 不会并发变更，故**有意不复刻**该 stale-inode 重载重试
/// （控制器歧义裁决 #2）——它在 inode 未被并发改写时是 no-op，差分等价。
pub(super) fn get_pblock_idx_state(
    reader: &dyn BlockReader,
    sb: &RawSuperblock,
    inode: &Inode,
    lblock: Ext4Lblk,
) -> Result<Option<(Ext4Fsblk, bool)>> {
    let path = find_extent(reader, sb, inode, lblock)?;
    if let Some(node) = path.last() {
        if let Some(extent) = node.extent {
            let ext_start = extent.first_block();
            let ext_len = extent.len() as u32;
            if let Some(ext_end) = ext_start.checked_add(ext_len) {
                if lblock >= ext_start && lblock < ext_end {
                    let fblock = extent.start() + (lblock - ext_start) as u64;
                    // PARITY: fblock >= blocks_count → EIO（ext4_impls/inode.rs:279）。
                    if fblock >= sb.blocks_count() {
                        return Err(Error::with_message(
                            Errno::EIO,
                            "mapped block out of range",
                        ));
                    }
                    return Ok(Some((fblock, extent.is_unwritten())));
                }
            }
        }
    }
    // hole：ext4_rs 返回 ENOENT；core 用 None 表达。
    Ok(None)
}

// =====================================================================
// extent 树写半部（Phase 3 Task 3）。
//
// 安全复刻 ext4_rs 写半部（`ext4_impls/extents.rs`：insert_extent / can_merge /
// merge_extent / insert_new_extent / create_new_leaf / ext_grow_indepth /
// convert_unwritten_span + extent 块 csum）。ext4_rs 的写半部用裸指针把
// `inode.block: [u32;15]` 当 `*mut Ext4ExtentHeader`/`*mut Ext4Extent` 改、用
// `from_raw_parts` 拼节点字节、`copy_within` 移位——core 一律改 Pod
// `from_bytes`/`as_bytes` 在「一段字节 + 字节偏移 12 + pos*12」上读写，逐字节等价。
//
// **parity-first 限制（每条 `// PARITY`）：**
// - insert_extent 只合并 found extent、不与邻居合并；
// - 无真正节点内分裂：满叶 → create_new_leaf（兄弟叶，父在 root）/ ext_grow_indepth；
// - insert extent at nonroot → ENOTSUP；split leaf with full non-root parent → ENOTSUP；
// - corrupted node（entries_count > capacity）→ EIO；
// - 合并长上限：written 32768、unwritten 32767。
// =====================================================================

/// 写上下文：读 + 元数据写 + 数据块写 + 可变 SB（分配器持有），core **不获取全局锁**。
///
/// `sb` 是只读几何视图（块大小 / 块数等稳定字段）；分配的「运行期权威 SB」由注入的
/// 分配上下文（Phase 2 `BlockAllocator`）自持。extent 树块写、inode 写回经 `writer`
/// （[`MetadataWriter`]）；文件数据块写经 `data_writer`（[`BlockWriter`]）。
pub(super) struct WriteCtx<'a> {
    pub reader: &'a dyn BlockReader,
    pub writer: &'a dyn MetadataWriter,
    pub data_writer: &'a dyn BlockWriter,
    pub sb: &'a RawSuperblock,
    pub block_size: usize,
}

impl<'a> WriteCtx<'a> {
    pub(super) fn new(
        reader: &'a dyn BlockReader,
        writer: &'a dyn MetadataWriter,
        data_writer: &'a dyn BlockWriter,
        sb: &'a RawSuperblock,
    ) -> Self {
        let block_size = sb.block_size();
        Self {
            reader,
            writer,
            data_writer,
            sb,
            block_size,
        }
    }

    /// 从写上下文借出只读上下文（读路径复用，避免重复持 reader/sb）。
    pub(super) fn read_ctx(&self) -> super::file::ReadCtx<'_> {
        super::file::ReadCtx {
            reader: self.reader,
            sb: self.sb,
            block_size: self.block_size,
        }
    }
}

/// 分配回调：写半部要分配新块（extent 兄弟叶 / 加深用单块、数据块用批量），但 core 不持
/// 全局分配器单例。把两类分配抽象成本回调，由调用方（差分 / 集成层）注入 Phase-2
/// `BlockAllocator` 的 `balloc_alloc_block(None)` / `balloc_alloc_block_batch(...)`。
///
/// **关键（ambiguity #4）**：两类分配必须背靠**同一个**分配器实例（同一份运行期 SB +
/// 位图状态），否则 tree-block 与 data-block 的 free 计数 / 位图会分叉、落盘字节不一致。
/// 故合成单 trait、单 `&mut dyn` 传参（一个对象同持两入口），不拆两个 `&mut dyn`。
pub(super) trait BlockAlloc {
    /// 分配一个块（无 goal），对应 ext4_rs `balloc_alloc_block(None)`（extent 树块用）。
    /// 入参 `inode` 让实现把 i_blocks（512B 单位）累加写回 `inode.raw.blocks`——与 ext4_rs
    /// 在共享 `Ext4InodeRef` 上累加 `blocks_count` 等价，使后续 `write_back_inode` 落对值。
    fn alloc_one(&mut self, inode: &mut Inode) -> Result<Ext4Fsblk>;
    /// 跨组批量分配（部分成功），对应 ext4_rs `balloc_alloc_block_batch`（数据块用）。
    /// `start_bgid` 是起扫组游标（命中后回写），由调用方按 `initial_write_alloc_bgid` 算好。
    fn alloc_batch(
        &mut self,
        inode: &mut Inode,
        start_bgid: &mut u32,
        count: usize,
    ) -> Result<Vec<Ext4Fsblk>>;
    /// 释放从 `start` 起的 `count` 个连续块，对应 ext4_rs `balloc_free_blocks`（删除路径用）。
    /// 入参 `inode` 让实现把释放后的 i_blocks（512B 单位）写回 `inode.raw.blocks`——与 ext4_rs
    /// 在共享 `Ext4InodeRef` 上递减 `blocks_count` 等价，使后续 `write_back_inode` 落对值。
    /// **关键（ambiguity #3）**：释放必须背靠分配同一实例（同一运行期 SB + 位图状态），故
    /// 三个分配/释放入口都挂在同一 trait、由同一 `&mut dyn` 持有。
    fn free_blocks(&mut self, inode: &mut Inode, start: Ext4Fsblk, count: u32);
}

/// 节点容量（项数）：root=(60-12)/12=4，非 root=(bs-12)/12。
/// [对照] ext4_rs insert_extent 防御段的 `node_capacity`（extents.rs:289-293）。
fn node_capacity(block_size: usize, at_root: bool) -> usize {
    if at_root {
        (15 * 4 - EXT4_EXTENT_HEADER_SIZE) / EXT4_EXTENT_SIZE
    } else {
        (block_size - EXT4_EXTENT_HEADER_SIZE) / EXT4_EXTENT_SIZE
    }
}

/// 从一段节点字节读第 `pos` 个 extent（字节偏移 12 + pos*12）。
fn read_extent_at(bytes: &[u8], pos: usize) -> RawExtent {
    let off = EXT4_EXTENT_HEADER_SIZE + pos * EXT4_EXTENT_SIZE;
    RawExtent::from_bytes(&bytes[off..off + EXT4_EXTENT_SIZE])
}

/// 把第 `pos` 个 extent 写进节点字节（字节偏移 12 + pos*12）。
fn write_extent_at(bytes: &mut [u8], pos: usize, ex: &RawExtent) {
    let off = EXT4_EXTENT_HEADER_SIZE + pos * EXT4_EXTENT_SIZE;
    bytes[off..off + EXT4_EXTENT_SIZE].copy_from_slice(ex.as_bytes());
}

/// 把第 `pos` 个 index 写进节点字节（字节偏移 12 + pos*12）。
fn write_index_at(bytes: &mut [u8], pos: usize, idx: &RawExtentIndex) {
    let off = EXT4_EXTENT_HEADER_SIZE + pos * EXT4_EXTENT_SIZE;
    bytes[off..off + EXT4_EXTENT_SIZE].copy_from_slice(idx.as_bytes());
}

/// 读节点头并写回（在节点字节上原地更新前 12 字节）。
fn write_header(bytes: &mut [u8], header: &RawExtentHeader) {
    bytes[..EXT4_EXTENT_HEADER_SIZE].copy_from_slice(header.as_bytes());
}

/// extent 块 tail 偏移：`12 + max_entries_count*12`（从盘上读到的头算，不写死）。
/// [对照] ext4_rs `ext4_extent_tail_offset`（extents.rs:1805）。
fn extent_tail_offset(header: &RawExtentHeader) -> usize {
    EXT4_EXTENT_HEADER_SIZE + (header.max_entries_count as usize) * EXT4_EXTENT_SIZE
}

/// 在**非根** extent 块字节上设置 tail csum（门控 metadata_csum）。**root 无 tail/无 csum**。
/// [对照] ext4_rs `set_extent_block_checksum_in_block`（extents.rs:79）+
/// `calculate_extent_block_checksum`（extents.rs:1770）。
///
/// csum = crc32c(uuid → inum(le4) → generation(le4) → block[..tail_offset])，写在 tail 处 4 字节。
fn set_extent_block_checksum_in_block(
    ctx: &WriteCtx,
    inode: &Inode,
    block: &mut [u8],
) -> Result<()> {
    let has_csum = (ctx.sb.features_read_only() & RO_COMPAT_METADATA_CSUM) != 0;
    if !has_csum {
        return Ok(());
    }
    let header = RawExtentHeader::from_bytes(&block[..EXT4_EXTENT_HEADER_SIZE]);
    // PARITY: ext4_rs 此处 magic != EXT4_EXTENT_MAGIC → EINVAL。
    if header.magic != EXTENT_MAGIC {
        return Err(Error::with_message(Errno::EINVAL, "Invalid extent magic"));
    }
    let tail_offset = extent_tail_offset(&header);
    let uuid = ctx.sb.uuid();
    let mut c = ext4_crc32c(EXT4_CRC32_INIT, &uuid);
    c = ext4_crc32c(c, &inode.num.to_le_bytes());
    c = ext4_crc32c(c, &inode.raw.generation().to_le_bytes());
    c = ext4_crc32c(c, &block[..tail_offset]);
    block[tail_offset..tail_offset + 4].copy_from_slice(&c.to_le_bytes());
    Ok(())
}

/// 读一个 extent 树块满 block_size 字节（ext4_rs read_offset 的 resize/truncate 等价）。
fn load_tree_block(ctx: &WriteCtx, pblock: usize) -> Vec<u8> {
    let mut buf = vec![0u8; ctx.block_size];
    ctx.reader.read_at(pblock * ctx.block_size, buf.as_mut_slice());
    buf
}

/// 把一个 extent 树块经 [`MetadataWriter`] 整块写回（对齐 ext4_rs `sync_blk_to_disk`）。
fn sync_tree_block(ctx: &WriteCtx, pblock: usize, block: &[u8]) -> Result<()> {
    ctx.writer
        .write_metadata_for_handle(0, pblock as Ext4Fsblk, block)
}

/// 两 extent 能否合并。逐位复刻 ext4_rs `can_merge`（extents.rs:467）。
///
/// PARITY 合并长上限：written `EXT_INIT_MAX_LEN=32768`；unwritten `EXT_INIT_MAX_LEN-1=32767`
/// （合并两 unwritten 到恰 32768 会丢 unwritten 标志、把未初始化块当 written 暴露）。
fn can_merge(ex1: &RawExtent, ex2: &RawExtent) -> bool {
    if ex1.is_unwritten() != ex2.is_unwritten() {
        return false;
    }
    let ext1_len = ex1.len() as usize;
    let ext2_len = ex2.len() as usize;
    // 逻辑连续。
    if ex1.first_block() + ext1_len as u32 != ex2.first_block() {
        return false;
    }
    // PARITY: 合并长上限（unwritten 取 32767）。
    let max_merged_len = if ex1.is_unwritten() {
        (EXT_INIT_MAX_LEN - 1) as usize
    } else {
        EXT_INIT_MAX_LEN as usize
    };
    if ext1_len + ext2_len > max_merged_len {
        return false;
    }
    // 物理连续。
    ex1.start() + ext1_len as u64 == ex2.start()
}

/// 合并：把 `right` 并入 `left`（left 长 += right 长，保留 unwritten 态）。
/// 逐位复刻 ext4_rs `merge_extent`（extents.rs:502）——只更新 `left` 局部值；
/// 非根（max_entries_count > 4）时再把合并结果写回该叶块的 `position` 槽并 sync。
fn merge_extent(
    ctx: &WriteCtx,
    inode: &Inode,
    leaf: &ExtentPathNode,
    left: &mut RawExtent,
    right: &RawExtent,
) -> Result<()> {
    let unwritten = left.is_unwritten();
    let len = left.len() + right.len();
    left.set_actual_len(len);
    if unwritten {
        left.mark_unwritten();
    }
    // PARITY: ext4_rs 仅在 max_entries_count > 4（即非根叶）时回写盘上叶块；
    //   root（max=4）只改 inode.block 局部、由 insert_extent 调用方 root_extent_mut_at 写回。
    if leaf.header.max_entries_count > 4 {
        let block_no = leaf.pblock_of_node;
        let mut block = load_tree_block(ctx, block_no);
        // 在盘块上重读该槽（与 ext4_rs 一致：load_offset_as_mut 后再合并一次）。
        let pos = leaf.position;
        let mut slot = read_extent_at(&block, pos);
        let unwritten = slot.is_unwritten();
        let len = slot.len() + right.len();
        slot.set_actual_len(len);
        if unwritten {
            slot.mark_unwritten();
        }
        write_extent_at(&mut block, pos, &slot);
        set_extent_block_checksum_in_block(ctx, inode, &mut block)?;
        sync_tree_block(ctx, block_no, &block)?;
    }
    Ok(())
}

/// 把一个新 extent 插入 extent 树。逐字节复刻 ext4_rs `insert_extent`（extents.rs:271）。
///
/// 流程：find_extent 找槽 → 防御（entries > capacity → EIO）→ 空节点 → insert_new_extent
/// → 命中 extent 且 can_merge → merge_extent（root 时写回 inode 槽）→ 否则 entries<max
/// 移位插入、满 → create_new_leaf。**不与邻居合并（PARITY）**。
pub(super) fn insert_extent(
    ctx: &WriteCtx,
    alloc: &mut dyn BlockAlloc,
    inode: &mut Inode,
    newex: &RawExtent,
) -> Result<()> {
    let newex_first_block = newex.first_block();
    let path = find_extent(ctx.reader, ctx.sb, inode, newex_first_block)?;
    // ext4_rs `search_path.depth` = 叶在 path 中的下标 = path.len()-1。
    let depth = path.path.len() - 1;
    let node = &path.path[depth];

    let at_root = node.pblock_of_node == 0;
    let header = node.header;

    // PARITY: corrupted node（entries_count > capacity）→ EIO（防御，extents.rs:289-303）。
    let capacity = node_capacity(ctx.block_size, at_root);
    if header.entries_count as usize > capacity {
        return Err(Error::with_message(Errno::EIO, "corrupted extent node"));
    }

    // 空节点：直接插。
    if header.entries_count == 0 {
        insert_new_extent(ctx, alloc, inode, &path, depth, newex)?;
        return Ok(());
    }

    // 命中 found extent 且可合并 → merge_extent（PARITY: 只合并 found，不碰邻居）。
    if let Some(ex) = node.extent {
        let mut ex = ex;
        if can_merge(&ex, newex) {
            merge_extent(ctx, inode, node, &mut ex, newex)?;
            if at_root {
                // root：把合并结果写回 inode i_block 的 position 槽。
                let mut root = inode.i_block_bytes_vec();
                write_extent_at(&mut root, node.position, &ex);
                inode.set_i_block_bytes(&root);
            }
            return Ok(());
        }
        // PARITY: 不与左右邻居合并——fall through 走常规插入。
    }

    // 有空位移位插入，满则 create_new_leaf。
    if header.entries_count < header.max_entries_count {
        insert_new_extent(ctx, alloc, inode, &path, depth, newex)?;
    } else {
        create_new_leaf(ctx, alloc, inode, &path, depth, newex)?;
    }
    Ok(())
}

/// 把新 extent 插入指定节点（root 或非根叶）。复刻 ext4_rs `insert_new_extent`（extents.rs:538）。
fn insert_new_extent(
    ctx: &WriteCtx,
    alloc: &mut dyn BlockAlloc,
    inode: &mut Inode,
    path: &SearchPath,
    depth: usize,
    newex: &RawExtent,
) -> Result<()> {
    let node = &path.path[depth];
    let header = node.header;

    if depth == 0 {
        // ---- 插入 root ----
        let mut root = inode.i_block_bytes_vec();
        // 空节点：写槽 position、entries +=1，write_back。
        if header.entries_count == 0 {
            write_extent_at(&mut root, node.position, newex);
            let mut h = RawExtentHeader::from_bytes(&root[..EXT4_EXTENT_HEADER_SIZE]);
            h.entries_count += 1;
            write_header(&mut root, &h);
            inode.set_i_block_bytes(&root);
            write_back_inode(ctx.writer, ctx.reader, ctx.sb, inode)?;
            return Ok(());
        }
        // root 满 → 加深后重插。
        if header.entries_count == header.max_entries_count {
            ext_grow_indepth(ctx, alloc, inode)?;
            return insert_extent(ctx, alloc, inode, newex);
        }
        // 非空：按 key 序选插入位。
        let insert_pos = if let Some(cur) = node.extent {
            if newex.first_block() < cur.first_block() {
                node.position
            } else {
                node.position + 1
            }
        } else {
            node.position + 1
        };
        let entries = header.entries_count as usize;
        if insert_pos < entries {
            for i in (insert_pos..entries).rev() {
                let moved = read_extent_at(&root, i);
                write_extent_at(&mut root, i + 1, &moved);
            }
        }
        write_extent_at(&mut root, insert_pos, newex);
        let mut h = RawExtentHeader::from_bytes(&root[..EXT4_EXTENT_HEADER_SIZE]);
        h.entries_count += 1;
        write_header(&mut root, &h);
        inode.set_i_block_bytes(&root);
        write_back_inode(ctx.writer, ctx.reader, ctx.sb, inode)?;
        return Ok(());
    }

    // ---- 插入非根叶 ----
    let insert_pos = if let Some(cur) = node.extent {
        if newex.first_block() < cur.first_block() {
            node.position
        } else {
            node.position + 1
        }
    } else {
        node.position + 1
    };
    let node_block = node.pblock_of_node;
    let mut block = load_tree_block(ctx, node_block);
    let entries_count = {
        let h = RawExtentHeader::from_bytes(&block[..EXT4_EXTENT_HEADER_SIZE]);
        h.entries_count as usize
    };
    if insert_pos < entries_count {
        let src = EXT4_EXTENT_HEADER_SIZE + insert_pos * EXT4_EXTENT_SIZE;
        let dst = EXT4_EXTENT_HEADER_SIZE + (insert_pos + 1) * EXT4_EXTENT_SIZE;
        let bytes_to_move = (entries_count - insert_pos) * EXT4_EXTENT_SIZE;
        block.copy_within(src..src + bytes_to_move, dst);
    }
    write_extent_at(&mut block, insert_pos, newex);
    {
        let mut h = RawExtentHeader::from_bytes(&block[..EXT4_EXTENT_HEADER_SIZE]);
        h.entries_count += 1;
        write_header(&mut block, &h);
    }
    set_extent_block_checksum_in_block(ctx, inode, &mut block)?;
    sync_tree_block(ctx, node_block, &block)?;
    if insert_pos == 0 {
        propagate_first_block_to_ancestors(ctx, inode, path, depth, newex.first_block())?;
    }
    Ok(())
}

/// 把 `first_block` 沿祖先 index 向上传播。复刻 ext4_rs `propagate_first_block_to_ancestors`
/// （extents.rs:152）+ `update_index_first_block_in_node`（extents.rs:105）。
fn propagate_first_block_to_ancestors(
    ctx: &WriteCtx,
    inode: &mut Inode,
    path: &SearchPath,
    mut child_level: usize,
    first_block: u32,
) -> Result<()> {
    while child_level > 0 {
        let parent_level = child_level - 1;
        let parent_node = &path.path[parent_level];
        let parent_pos = parent_node.position;
        update_index_first_block_in_node(ctx, inode, parent_node, parent_pos, first_block)?;
        if parent_pos != 0 {
            break;
        }
        child_level = parent_level;
    }
    Ok(())
}

/// 更新某节点第 `pos` 个 index 的 first_block。复刻 ext4_rs `update_index_first_block_in_node`。
fn update_index_first_block_in_node(
    ctx: &WriteCtx,
    inode: &mut Inode,
    node: &ExtentPathNode,
    pos: usize,
    first_block: u32,
) -> Result<()> {
    if node.pblock_of_node == 0 {
        // root index 节点（在 inode body）。
        let entries = node.header.entries_count as usize;
        if pos >= entries {
            return Err(Error::with_message(
                Errno::EINVAL,
                "root index position out of range",
            ));
        }
        let mut root = inode.i_block_bytes_vec();
        let off = EXT4_EXTENT_HEADER_SIZE + pos * EXT4_EXTENT_SIZE;
        let mut idx = RawExtentIndex::from_bytes(&root[off..off + EXT4_EXTENT_SIZE]);
        idx.first_block = first_block;
        write_index_at(&mut root, pos, &idx);
        inode.set_i_block_bytes(&root);
        write_back_inode(ctx.writer, ctx.reader, ctx.sb, inode)?;
        return Ok(());
    }
    // 非根内部块。
    let mut block = load_tree_block(ctx, node.pblock_of_node);
    let entries = {
        let h = RawExtentHeader::from_bytes(&block[..EXT4_EXTENT_HEADER_SIZE]);
        h.entries_count as usize
    };
    if pos >= entries {
        return Err(Error::with_message(
            Errno::EINVAL,
            "index position out of range",
        ));
    }
    let off = EXT4_EXTENT_HEADER_SIZE + pos * EXT4_EXTENT_SIZE;
    let mut idx = RawExtentIndex::from_bytes(&block[off..off + EXT4_EXTENT_SIZE]);
    idx.first_block = first_block;
    write_index_at(&mut block, pos, &idx);
    set_extent_block_checksum_in_block(ctx, inode, &mut block)?;
    sync_tree_block(ctx, node.pblock_of_node, &block)?;
    Ok(())
}

/// 满叶分裂：分配兄弟叶（depth>0）、装 1 项、往父插 index。复刻 ext4_rs
/// `create_new_leaf`（extents.rs:654）。**PARITY: 父在 root 才支持；满非根父 → ENOTSUP。**
fn create_new_leaf(
    ctx: &WriteCtx,
    alloc: &mut dyn BlockAlloc,
    inode: &mut Inode,
    path: &SearchPath,
    depth: usize,
    newex: &RawExtent,
) -> Result<()> {
    if depth > 0 {
        let parent = &path.path[depth - 1];
        let new_leaf_block = alloc.alloc_one(inode)?;
        // 装兄弟叶：头（1 项）+ newex，清零余下，写 csum，sync。
        let mut leaf = vec![0u8; ctx.block_size];
        let leaf_header = RawExtentHeader::new(
            EXTENT_MAGIC,
            1,
            ((ctx.block_size - EXT4_EXTENT_HEADER_SIZE) / EXT4_EXTENT_SIZE) as u16,
            0,
            0,
        );
        write_header(&mut leaf, &leaf_header);
        write_extent_at(&mut leaf, 0, newex);
        set_extent_block_checksum_in_block(ctx, inode, &mut leaf)?;
        sync_tree_block(ctx, new_leaf_block as usize, &leaf)?;

        let insert_pos = if let Some(cur_idx) = parent.index {
            if newex.first_block() < cur_idx.first_block() {
                parent.position
            } else {
                parent.position + 1
            }
        } else {
            parent.position + 1
        };

        if parent.pblock_of_node == 0 {
            // 父是 root index 节点。
            let root_header = root_extent_header(inode);
            if root_header.entries_count >= root_header.max_entries_count {
                // root index 满 → 加深重插。
                ext_grow_indepth(ctx, alloc, inode)?;
                return insert_extent(ctx, alloc, inode, newex);
            }
            let parent_entries = root_header.entries_count as usize;
            let mut root = inode.i_block_bytes_vec();
            if insert_pos < parent_entries {
                for i in (insert_pos..parent_entries).rev() {
                    let off = EXT4_EXTENT_HEADER_SIZE + i * EXT4_EXTENT_SIZE;
                    let moved = RawExtentIndex::from_bytes(&root[off..off + EXT4_EXTENT_SIZE]);
                    write_index_at(&mut root, i + 1, &moved);
                }
            }
            let mut new_index = RawExtentIndex::default();
            new_index.first_block = newex.first_block();
            new_index.store_pblock(new_leaf_block);
            new_index.padding = 0;
            write_index_at(&mut root, insert_pos, &new_index);
            let mut h = RawExtentHeader::from_bytes(&root[..EXT4_EXTENT_HEADER_SIZE]);
            h.entries_count += 1;
            write_header(&mut root, &h);
            inode.set_i_block_bytes(&root);
            write_back_inode(ctx.writer, ctx.reader, ctx.sb, inode)?;
            return Ok(());
        }

        // 父是非根内部块。
        let mut parent_block = load_tree_block(ctx, parent.pblock_of_node);
        let (parent_entries, parent_max) = {
            let h = RawExtentHeader::from_bytes(&parent_block[..EXT4_EXTENT_HEADER_SIZE]);
            (h.entries_count as usize, h.max_entries_count as usize)
        };
        // PARITY: 满非根父 → ENOTSUP（extents.rs:757-762）。
        if parent_entries >= parent_max {
            return Err(Error::with_message(
                Errno::EOPNOTSUPP,
                "split leaf with full non-root parent is not supported",
            ));
        }
        if insert_pos < parent_entries {
            let src = EXT4_EXTENT_HEADER_SIZE + insert_pos * EXT4_EXTENT_SIZE;
            let dst = EXT4_EXTENT_HEADER_SIZE + (insert_pos + 1) * EXT4_EXTENT_SIZE;
            let bytes_to_move = (parent_entries - insert_pos) * EXT4_EXTENT_SIZE;
            parent_block.copy_within(src..src + bytes_to_move, dst);
        }
        let mut new_index = RawExtentIndex::default();
        new_index.first_block = newex.first_block();
        new_index.store_pblock(new_leaf_block);
        new_index.padding = 0;
        write_index_at(&mut parent_block, insert_pos, &new_index);
        {
            let mut h = RawExtentHeader::from_bytes(&parent_block[..EXT4_EXTENT_HEADER_SIZE]);
            h.entries_count += 1;
            write_header(&mut parent_block, &h);
        }
        set_extent_block_checksum_in_block(ctx, inode, &mut parent_block)?;
        sync_tree_block(ctx, parent.pblock_of_node, &parent_block)?;
        if insert_pos == 0 {
            propagate_first_block_to_ancestors(ctx, inode, path, depth - 1, newex.first_block())?;
        }
        return Ok(());
    }

    // depth == 0：树满 → 加深后重插。
    ext_grow_indepth(ctx, alloc, inode)?;
    insert_extent(ctx, alloc, inode, newex)
}

/// 树加深：根内容搬进新块、根变 1 项 index 节点、depth+1。复刻 ext4_rs
/// `ext_grow_indepth`（extents.rs:815）。
fn ext_grow_indepth(
    ctx: &WriteCtx,
    alloc: &mut dyn BlockAlloc,
    inode: &mut Inode,
) -> Result<()> {
    let new_block = alloc.alloc_one(inode)?;
    let mut new_blk = vec![0u8; ctx.block_size];

    let root = inode.i_block_bytes_vec();
    let old_root_header = RawExtentHeader::from_bytes(&root[..EXT4_EXTENT_HEADER_SIZE]);
    let old_depth = old_root_header.depth;
    let old_entries_count = old_root_header.entries_count;

    // 第一个子项的逻辑块号（depth==0 取 extent[0].first_block，否则 index[0].first_block）。
    let first_logical_block = if old_entries_count > 0 {
        let off = EXT4_EXTENT_HEADER_SIZE; // pos 0
        // extent 与 index 的 first_block 同在偏移 0，统一用 Pod 视图读（不用 from_le_bytes）。
        RawExtent::from_bytes(&root[off..off + size_of::<RawExtent>()]).first_block()
    } else {
        0
    };

    // 新块头：保留 old_depth，max 按整块算。
    let new_header = RawExtentHeader::new(
        EXTENT_MAGIC,
        old_entries_count,
        ((ctx.block_size - EXT4_EXTENT_HEADER_SIZE) / EXT4_EXTENT_SIZE) as u16,
        old_depth,
        0,
    );
    write_header(&mut new_blk, &new_header);
    // 搬根的项区（12 起，old_entries_count*12 字节）。
    if old_entries_count > 0 {
        let sz = old_entries_count as usize * EXT4_EXTENT_SIZE;
        new_blk[EXT4_EXTENT_HEADER_SIZE..EXT4_EXTENT_HEADER_SIZE + sz]
            .copy_from_slice(&root[EXT4_EXTENT_HEADER_SIZE..EXT4_EXTENT_HEADER_SIZE + sz]);
    }
    set_extent_block_checksum_in_block(ctx, inode, &mut new_blk)?;
    sync_tree_block(ctx, new_block as usize, &new_blk)?;

    // 根变 1 项 index 节点：在**原根头**上改 magic/entries=1/max=4/depth+1（保留 generation），
    //   再清项区、写首 index——与 ext4_rs `root_extent_header_mut()` 就地改 + `write_bytes` 清
    //   extent 区逐字节一致（generation 字段不被触碰）。
    let mut new_root = vec![0u8; root.len()];
    let mut root_header = old_root_header;
    root_header.magic = EXTENT_MAGIC;
    root_header.entries_count = 1;
    root_header.max_entries_count = 4;
    root_header.depth = old_depth + 1;
    write_header(&mut new_root, &root_header);
    // 清根项区（write_bytes 0），再写首 index。
    let mut first_index = RawExtentIndex::default();
    first_index.first_block = first_logical_block;
    first_index.store_pblock(new_block);
    write_index_at(&mut new_root, 0, &first_index);
    inode.set_i_block_bytes(&new_root);
    write_back_inode(ctx.writer, ctx.reader, ctx.sb, inode)?;
    Ok(())
}

/// 读 inode root 的 extent 头（前 12 字节）。
fn root_extent_header(inode: &Inode) -> RawExtentHeader {
    let root = inode.i_block_bytes();
    RawExtentHeader::from_bytes(&root[..EXT4_EXTENT_HEADER_SIZE])
}

/// 在叶节点上做就地编辑（+ 至多一项删除）。复刻 ext4_rs `rewrite_leaf_entries`（extents.rs:938）。
///
/// `edits` 的 pos 指删除前布局；删除（若有）在编辑之后应用。**调用方不得改 pos 0 的
/// first_block**（那需要祖先 index 更新，本 helper 不做）。
fn rewrite_leaf_entries(
    ctx: &WriteCtx,
    inode: &mut Inode,
    node: &ExtentPathNode,
    edits: &[(usize, RawExtent)],
    remove_pos: Option<usize>,
) -> Result<()> {
    let entries = node.header.entries_count as usize;
    for (pos, _) in edits {
        if *pos >= entries {
            return Err(Error::with_message(Errno::EIO, "leaf entry edit out of range"));
        }
    }
    if let Some(pos) = remove_pos {
        if pos == 0 || pos >= entries {
            return Err(Error::with_message(Errno::EIO, "leaf entry removal out of range"));
        }
    }

    if node.pblock_of_node == 0 {
        // root 叶。
        let mut root = inode.i_block_bytes_vec();
        for (pos, ex) in edits {
            write_extent_at(&mut root, *pos, ex);
        }
        if let Some(pos) = remove_pos {
            for i in pos + 1..entries {
                let moved = read_extent_at(&root, i);
                write_extent_at(&mut root, i - 1, &moved);
            }
            // 末项清零。
            write_extent_at(&mut root, entries - 1, &RawExtent::default());
            let mut h = RawExtentHeader::from_bytes(&root[..EXT4_EXTENT_HEADER_SIZE]);
            h.entries_count -= 1;
            write_header(&mut root, &h);
        }
        inode.set_i_block_bytes(&root);
        write_back_inode(ctx.writer, ctx.reader, ctx.sb, inode)?;
        return Ok(());
    }

    // 非根叶块。
    let mut block = load_tree_block(ctx, node.pblock_of_node);
    for (pos, ex) in edits {
        write_extent_at(&mut block, *pos, ex);
    }
    if let Some(pos) = remove_pos {
        if entries > pos + 1 {
            let src = EXT4_EXTENT_HEADER_SIZE + (pos + 1) * EXT4_EXTENT_SIZE;
            let dst = EXT4_EXTENT_HEADER_SIZE + pos * EXT4_EXTENT_SIZE;
            let bytes_to_move = (entries - pos - 1) * EXT4_EXTENT_SIZE;
            block.copy_within(src..src + bytes_to_move, dst);
        }
        let last = EXT4_EXTENT_HEADER_SIZE + (entries - 1) * EXT4_EXTENT_SIZE;
        block[last..last + EXT4_EXTENT_SIZE].fill(0);
        let mut h = RawExtentHeader::from_bytes(&block[..EXT4_EXTENT_HEADER_SIZE]);
        h.entries_count -= 1;
        write_header(&mut block, &h);
    }
    set_extent_block_checksum_in_block(ctx, inode, &mut block)?;
    sync_tree_block(ctx, node.pblock_of_node, &block)?;
    Ok(())
}

/// 把覆盖 `from` 的 unwritten extent 的前段 `[from, min(ee, max_end))` 转 written。
/// 复刻 ext4_rs `convert_unwritten_span`（extents.rs:1032）。返回转换后首个逻辑块。
///
/// 三形态：① from==es 且左邻 written 且连续 → 左并（grow left + shrink/drop E）；
/// ② from==es → E 原地变 written 片，余下 unwritten 尾 re-insert；
/// ③ from>es → E 原地缩成 unwritten 头，written 片 + unwritten 尾 insert。
pub(super) fn convert_unwritten_span(
    ctx: &WriteCtx,
    alloc: &mut dyn BlockAlloc,
    inode: &mut Inode,
    from: Ext4Lblk,
    max_end: Ext4Lblk,
) -> Result<Ext4Lblk> {
    let path = find_extent(ctx.reader, ctx.sb, inode, from)?;
    let depth = path.path.len() - 1;
    let node = &path.path[depth];
    let Some(ex) = node.extent else {
        return Err(Error::with_message(
            Errno::EIO,
            "no extent covers unwritten lblock",
        ));
    };

    let es = ex.first_block();
    let ext_len = ex.len() as u32;
    let ee = es
        .checked_add(ext_len)
        .ok_or_else(|| Error::with_message(Errno::EIO, "extent end overflow"))?;
    if !ex.is_unwritten() || from < es || from >= ee || max_end <= from {
        return Err(Error::with_message(
            Errno::EIO,
            "convert target is not an unwritten mapping",
        ));
    }
    let ep = ex.start();
    let cov_end = ee.min(max_end);
    let cov_len = cov_end - from;
    let pos = node.position;

    // ① 左并 fast path。
    if from == es && pos > 0 {
        let left = read_leaf_extent_at(ctx, inode, node, pos - 1)?;
        let left_len = left.len() as u32;
        let mergeable = !left.is_unwritten()
            && left.first_block().checked_add(left_len) == Some(es)
            && left.start() + left_len as u64 == ep
            && left_len + cov_len <= EXT_INIT_MAX_LEN as u32;
        if mergeable {
            let mut new_left = left;
            new_left.set_actual_len((left_len + cov_len) as u16);
            if cov_end == ee {
                rewrite_leaf_entries(ctx, inode, node, &[(pos - 1, new_left)], Some(pos))?;
            } else {
                let mut new_cur = ex;
                new_cur.first_block = cov_end;
                new_cur.store_pblock(ep + cov_len as u64);
                new_cur.set_actual_len((ee - cov_end) as u16);
                new_cur.mark_unwritten();
                rewrite_leaf_entries(
                    ctx,
                    inode,
                    node,
                    &[(pos - 1, new_left), (pos, new_cur)],
                    None,
                )?;
            }
            return Ok(cov_end);
        }
    }

    // ② from == es：E 原地变 written，尾 re-insert。
    if from == es {
        let mut written_piece = ex;
        written_piece.set_actual_len(cov_len as u16);
        written_piece.mark_written();
        rewrite_leaf_entries(ctx, inode, node, &[(pos, written_piece)], None)?;

        if cov_end < ee {
            let mut tail = RawExtent::default();
            tail.first_block = cov_end;
            tail.store_pblock(ep + cov_len as u64);
            tail.set_actual_len((ee - cov_end) as u16);
            tail.mark_unwritten();
            insert_extent(ctx, alloc, inode, &tail)?;
        }
        return Ok(cov_end);
    }

    // ③ from > es：E 缩成 unwritten 头，written 片 + unwritten 尾 insert。
    let mut head = ex;
    head.set_actual_len((from - es) as u16);
    head.mark_unwritten();
    rewrite_leaf_entries(ctx, inode, node, &[(pos, head)], None)?;

    let mut written_piece = RawExtent::default();
    written_piece.first_block = from;
    written_piece.store_pblock(ep + (from - es) as u64);
    written_piece.set_actual_len(cov_len as u16);
    insert_extent(ctx, alloc, inode, &written_piece)?;

    if cov_end < ee {
        let mut tail = RawExtent::default();
        tail.first_block = cov_end;
        tail.store_pblock(ep + (cov_end - es) as u64);
        tail.set_actual_len((ee - cov_end) as u16);
        tail.mark_unwritten();
        insert_extent(ctx, alloc, inode, &tail)?;
    }
    Ok(cov_end)
}

/// 读叶节点第 `pos` 个 extent（root 从 inode i_block，非根从盘块）。
/// 对应 ext4_rs convert 路径里 `root_extent_at` / `get_extent_from_node` 的取项。
fn read_leaf_extent_at(
    ctx: &WriteCtx,
    inode: &Inode,
    node: &ExtentPathNode,
    pos: usize,
) -> Result<RawExtent> {
    if node.pblock_of_node == 0 {
        let root = inode.i_block_bytes();
        return Ok(read_extent_at(&root, pos));
    }
    let block = load_tree_block(ctx, node.pblock_of_node);
    Ok(read_extent_at(&block, pos))
}

// =====================================================================
// extent 树删除半部（Phase 3 Task 5）。
//
// 安全复刻 ext4_rs 删除半部（`ext4_impls/extents.rs`：extent_remove_space /
// ext_remove_leaf / ext_remove_idx / ext_remove_index_block / ext_remove_blocks /
// ext_correct_indexes / more_to_rm）。ext4_rs 用裸指针把 `inode.block:[u32;15]` 当
// `*mut Ext4ExtentHeader`/`*mut Ext4Extent` 改、用 `Block::load_inode_root_block`
// transmute 取根 60 字节、`read_offset_as_mut` 改盘块项、`copy_from_slice`/`fill(0)`
// 压实——core 一律改 Pod `from_bytes`/`as_bytes` 在「一段字节 + 字节偏移 12 + pos*12」
// 上读写，逐字节等价（差分 `assert_disk_eq` 全盘对拍暴露任何偏差）。
//
// **parity-first 限制（每条 `// PARITY`）：**
// - `extent_remove_space` 只 `find_extent(from)` 一次，跨循环复用同一（渐旧的）path，
//   节点块在 `ext_remove_leaf`/`more_to_rm` 内从盘**重读**（与 ext4_rs 一致）；
// - extent 中间删（first_block<from && to<first_block+actual_len-1）→ 截短 + 尾段 reinsert；
// - 叶压实空 → `ext_remove_idx` 删父 index + 释放 index 块；末根项 → 根塌回空叶；
// - pos==0 first_block 传播经已有 `propagate_first_block_to_ancestors`（Task 3）；
// - `more_to_rm` 读 last_index 用 `12*pos`（缺头偏移）——ext4_rs 既有怪癖，原样复刻。
// =====================================================================

/// 删除 `[from, to]`（逻辑块闭区间）覆盖的 extent 树空间。逐字节复刻 ext4_rs
/// `extent_remove_space`（ext4_impls/extents.rs:1180）。
///
/// 流程：① `find_extent(from)`；② **extent 中间删**（found extent 完全包住 [from,to] 内侧）：
/// 把 found 截短到 `from-first_block`、构造尾段 newex(`first_block=to+1`) → `insert_extent`；
/// ③ 否则从叶（`i=depth`）向上 `i=depth..=0` 循环：叶层 `ext_remove_leaf`（压实 + 释放），
/// 索引层据 `more_to_rm` 决定下钻 / 上溯，空索引 → `ext_remove_idx`。
pub(super) fn extent_remove_space(
    ctx: &WriteCtx,
    alloc: &mut dyn BlockAlloc,
    inode: &mut Inode,
    from: Ext4Lblk,
    to: Ext4Lblk,
) -> Result<()> {
    let block_size = ctx.block_size;
    let mut search_path = find_extent(ctx.reader, ctx.sb, inode, from)?;

    // PARITY: ext4_rs `search_path.depth` = 下钻层数 = path.len()-1（叶在 path 中的下标）。
    let depth = search_path.path.len() - 1;

    // ② extent 中间删：found extent 严格包住 [from, to] 内侧（first_block < from
    //    且 to < first_block + actual_len - 1）→ 截前段 + 尾段 reinsert（不释放块）。
    if let Some(mut ex) = search_path.path[depth].extent {
        let ee_block = ex.first_block();
        let actual_len = ex.len() as u32;
        if ee_block < from && to < ee_block + actual_len - 1 {
            let mut newex = RawExtent::default();
            let unwritten = ex.is_unwritten();
            let block_count = ex.block_count;
            // 尾段物理起块 = (to+1 - ee_block) + ex.pblock。
            let newblock = to + 1 - ee_block + ex.start() as u32;
            // PARITY: ext4_rs 直写 block_count（不经 set_actual_len 去 unwritten 标志），
            //   随后若 unwritten 再 mark_unwritten——两步与下方一致。
            ex.block_count = (from as u16).wrapping_sub(ee_block as u16);
            if unwritten {
                ex.mark_unwritten();
            }
            newex.first_block = to + 1;
            newex.block_count = (ee_block + block_count as u32 - 1 - to) as u16;
            newex.start_lo = newblock;
            newex.start_hi = ((newblock as u64) >> 32) as u16;

            // PARITY: ext4_rs 截短后只构造 newex 再 insert_extent；前段 ex 的截短由
            //   insert_extent 路径里对 found extent 的处理落盘？—— 否，ext4_rs **未**回写
            //   截短后的 ex（它只改了局部 `ex`，未写回 inode/块）。复刻同一行为：不回写 ex，
            //   仅 insert 尾段。truncate 主调路径对该分支不依赖（中间删仅 punch 才走）。
            insert_extent(ctx, alloc, inode, &newex)?;
            return Ok(());
        }
    }

    // ③ 自叶向上逐层删。
    let mut i = depth as isize;
    while i >= 0 {
        if i as usize == depth {
            // ---- 叶层（i == depth）----
            let node_pblock = search_path.path[i as usize].pblock_of_node;
            let header = search_path.path[i as usize].header;
            let entries_count = header.entries_count;

            let (first_ex, last_ex) = if node_pblock == 0 {
                // 根叶：从 inode i_block 取首/末 extent。
                let root = inode.i_block_bytes();
                (
                    read_extent_at(&root, 0),
                    read_extent_at(&root, entries_count as usize - 1),
                )
            } else {
                // 非根叶：从盘块取首/末 extent。
                let block = load_tree_block(ctx, node_pblock);
                (
                    read_extent_at(&block, 0),
                    read_extent_at(&block, entries_count as usize - 1),
                )
            };

            let mut leaf_from = first_ex.first_block();
            let mut leaf_to = last_ex.first_block() + last_ex.len() as u32 - 1;
            if leaf_from < from {
                leaf_from = from;
            }
            if leaf_to > to {
                leaf_to = to;
            }
            ext_remove_leaf(ctx, alloc, inode, &mut search_path, leaf_from, leaf_to)?;

            i -= 1;
            continue;
        }

        // ---- 索引层（i < depth）----
        let header = search_path.path[i as usize].header;
        if more_to_rm(ctx, &search_path.path[i as usize], to) {
            // 下钻到子节点。
            i += 1;
        } else {
            if i > 0 && header.entries_count == 0 {
                ext_remove_idx(ctx, alloc, inode, &mut search_path, i as u16 - 1)?;
            }
            if i - 1 < 0 {
                break;
            }
            i -= 1;
        }
    }

    Ok(())
}

/// 压实叶节点存活 extent、释放被删块、pos==0 时传播 first_block。逐字节复刻 ext4_rs
/// `ext_remove_leaf`（ext4_impls/extents.rs:1358）。
fn ext_remove_leaf(
    ctx: &WriteCtx,
    alloc: &mut dyn BlockAlloc,
    inode: &mut Inode,
    path: &mut SearchPath,
    from: Ext4Lblk,
    to: Ext4Lblk,
) -> Result<()> {
    // PARITY: ext4_rs 用 `inode.root_header_depth()` 作叶在 path 中的下标（depth）。
    let depth = root_extent_header(inode).depth as usize;
    let header = path.path[depth].header;

    let pos = path.path[depth].position;
    let entry_count = header.entries_count;

    let node_pblock = path.path[depth].pblock_of_node;
    let is_root = node_pblock == 0;

    // 节点字节：根=60 字节 i_block；非根=整块。
    let mut node_bytes = if is_root {
        inode.i_block_bytes_vec()
    } else {
        load_tree_block(ctx, node_pblock)
    };

    // PARITY: 防御 corrupted 节点——entries_count 超物理容量时 clamp（ext4_rs:1417-1431）。
    let capacity = node_bytes
        .len()
        .saturating_sub(EXT4_EXTENT_HEADER_SIZE)
        / EXT4_EXTENT_SIZE;
    let mut entry_count = entry_count;
    if entry_count as usize > capacity {
        entry_count = capacity as u16;
    }

    let mut write_pos = pos;
    for idx in pos..entry_count as usize {
        let mut ex = read_extent_at(&node_bytes, idx);

        // PARITY: 零长 extent（actual_len==0）是垃圾槽——直接压实掉（ext4_rs:1442）。
        if ex.len() == 0 {
            continue;
        }

        if ex.first_block() > to {
            if write_pos != idx {
                let moved = ex;
                write_extent_at(&mut node_bytes, write_pos, &moved);
            }
            write_pos += 1;
            continue;
        }

        let end = ex.first_block() + ex.len() as u32 - 1;
        if end < from {
            if write_pos != idx {
                let moved = ex;
                write_extent_at(&mut node_bytes, write_pos, &moved);
            }
            write_pos += 1;
            continue;
        }

        let mut kept = None;
        if ex.first_block() < from {
            // 删 [from, min(end,to)]，保留前段 [first_block, from)。
            let remove_from = from;
            let remove_to = end.min(to);
            ext_remove_blocks(ctx, alloc, inode, &ex, remove_from, remove_to)?;
            let unwritten = ex.is_unwritten();
            ex.block_count = (from - ex.first_block()) as u16;
            if unwritten {
                ex.mark_unwritten();
            }
            kept = Some(ex);
        } else if end > to {
            // 删 [first_block, to]，保留尾段 [to+1, end]。
            let remove_from = ex.first_block();
            let remove_to = to;
            ext_remove_blocks(ctx, alloc, inode, &ex, remove_from, remove_to)?;
            let unwritten = ex.is_unwritten();
            let new_start = to + 1;
            let new_pblock = ex.start() + (new_start - ex.first_block()) as u64;
            ex.first_block = new_start;
            ex.store_pblock(new_pblock);
            ex.block_count = (end - to) as u16;
            if unwritten {
                ex.mark_unwritten();
            }
            kept = Some(ex);
        } else {
            // 整 extent 被删 [first_block, end]。
            let remove_from = ex.first_block();
            ext_remove_blocks(ctx, alloc, inode, &ex, remove_from, end)?;
        }

        if let Some(kept_extent) = kept {
            write_extent_at(&mut node_bytes, write_pos, &kept_extent);
            write_pos += 1;
        }
    }

    // 清空压实后多出的尾槽。
    for idx in write_pos..entry_count as usize {
        let off = EXT4_EXTENT_HEADER_SIZE + idx * EXT4_EXTENT_SIZE;
        node_bytes[off..off + EXT4_EXTENT_SIZE].fill(0);
    }

    let new_entry_count = write_pos as u16;
    // 更新本节点头 entries_count。
    {
        let mut h = RawExtentHeader::from_bytes(&node_bytes[..EXT4_EXTENT_HEADER_SIZE]);
        h.entries_count = new_entry_count;
        write_header(&mut node_bytes, &h);
    }

    if is_root {
        inode.set_i_block_bytes(&node_bytes[..60]);
        write_back_inode(ctx.writer, ctx.reader, ctx.sb, inode)?;
    } else {
        set_extent_block_checksum_in_block(ctx, inode, &mut node_bytes)?;
        sync_tree_block(ctx, node_pblock, &node_bytes)?;
    }

    // pos==0 且仍有存活 extent → 修正祖先 index 的 first_block 键。
    if pos == 0 && new_entry_count > 0 {
        let first_extent = read_extent_at(&node_bytes, 0);
        ext_correct_indexes(ctx, inode, path, depth, first_extent.first_block())?;
    }

    // 叶空 → 从上层 index 块删本叶；否则若 depth>0 推进父 position。
    if new_entry_count == 0 {
        if path.path[depth].pblock_of_node == 0 {
            return Ok(());
        }
        ext_remove_idx(ctx, alloc, inode, path, depth as u16 - 1)?;
    } else if depth > 0 {
        path.path[depth - 1].position += 1;
    }

    Ok(())
}

/// 释放某 index 项指向的 index 块（1 块）。复刻 ext4_rs `ext_remove_index_block`（:1556）。
fn ext_remove_index_block(
    ctx: &WriteCtx,
    alloc: &mut dyn BlockAlloc,
    inode: &mut Inode,
    index: &RawExtentIndex,
) -> Result<()> {
    let block_to_free = index.leaf();
    alloc.free_blocks(inode, block_to_free, 1);
    // PARITY: 与 `ext_remove_blocks` 同理——ext4_rs `balloc_free_blocks` 在释放后立即
    //   write_back_inode 落 i_blocks。core `balloc_free_blocks` 不落盘，故此处补写回，
    //   使 index 块释放的 i_blocks 递减也持久化（否则根塌路径会丢这次落盘）。
    write_back_inode(ctx.writer, ctx.reader, ctx.sb, inode)?;
    Ok(())
}

/// 删父 index + 释放 index 块；末根项时根塌回空叶。逐字节复刻 ext4_rs
/// `ext_remove_idx`（ext4_impls/extents.rs:1563）。`depth` 是**父**层在 path 中的下标。
fn ext_remove_idx(
    ctx: &WriteCtx,
    alloc: &mut dyn BlockAlloc,
    inode: &mut Inode,
    path: &mut SearchPath,
    depth: u16,
) -> Result<()> {
    let i = depth as usize;
    let mut header = path.path[i].header;

    // 要删的 index（其 leaf 块号待释放）。
    let removed_index = path.path[i].index.ok_or_else(|| {
        Error::with_message(Errno::EIO, "ext_remove_idx: missing index in path node")
    })?;

    let node_pblock = path.path[i].pblock_of_node;
    let is_root = node_pblock == 0;

    let mut node_bytes = if is_root {
        inode.i_block_bytes_vec()
    } else {
        load_tree_block(ctx, node_pblock)
    };

    // 非末项 → 后续 index 前移压实，尾部清零。
    if path.path[i].position != header.entries_count as usize - 1 {
        let start_pos = EXT4_EXTENT_HEADER_SIZE + path.path[i].position * EXT4_EXTENT_SIZE;
        let end_pos = EXT4_EXTENT_HEADER_SIZE + (header.entries_count as usize) * EXT4_EXTENT_SIZE;
        // [start_pos+12, end_pos) → [start_pos, …)。
        let src = start_pos + EXT4_EXTENT_SIZE;
        let remaining_size = end_pos - src;
        node_bytes.copy_within(src..end_pos, start_pos);
        let empty_start = start_pos + remaining_size;
        node_bytes[empty_start..end_pos].fill(0);
    }

    // entries_count -= 1，写回头。
    header.entries_count -= 1;
    {
        let mut h = RawExtentHeader::from_bytes(&node_bytes[..EXT4_EXTENT_HEADER_SIZE]);
        h.entries_count = header.entries_count;
        write_header(&mut node_bytes, &h);
    }

    if is_root {
        // PARITY: 末根 index 被删 → 根塌回空叶（magic/entries=0/max=4/depth=0/gen=0，清项区），
        //   write_back，再释放该 index 块，返回（ext4_rs:1628-1644）。
        if header.entries_count == 0 {
            let mut h = RawExtentHeader::from_bytes(&node_bytes[..EXT4_EXTENT_HEADER_SIZE]);
            h.magic = EXTENT_MAGIC;
            h.entries_count = 0;
            h.max_entries_count = 4;
            h.depth = 0;
            h.generation = 0;
            write_header(&mut node_bytes, &h);
            // 清 [12, 60) 项区（ext4_rs `write_bytes(extents_ptr, 0, 60-12)`）。
            node_bytes[EXT4_EXTENT_HEADER_SIZE..60].fill(0);
            inode.set_i_block_bytes(&node_bytes[..60]);
            write_back_inode(ctx.writer, ctx.reader, ctx.sb, inode)?;
            ext_remove_index_block(ctx, alloc, inode, &removed_index)?;
            return Ok(());
        }
        inode.set_i_block_bytes(&node_bytes[..60]);
        write_back_inode(ctx.writer, ctx.reader, ctx.sb, inode)?;
    } else {
        set_extent_block_checksum_in_block(ctx, inode, &mut node_bytes)?;
        sync_tree_block(ctx, node_pblock, &node_bytes)?;
    }

    // 释放被删 index 指向的块。
    ext_remove_index_block(ctx, alloc, inode, &removed_index)?;

    // pos==0 且仍有项 → 修正祖先 index 的 first_block 键。
    if path.path[i].position == 0 && header.entries_count > 0 {
        let first_block = if is_root {
            // 根的首 index（u32 偏移 3 = 字节 12）。
            RawExtentIndex::from_bytes(&node_bytes[EXT4_EXTENT_HEADER_SIZE..EXT4_EXTENT_HEADER_SIZE + EXT4_EXTENT_SIZE])
                .first_block()
        } else {
            RawExtentIndex::from_bytes(&node_bytes[EXT4_EXTENT_HEADER_SIZE..EXT4_EXTENT_HEADER_SIZE + EXT4_EXTENT_SIZE])
                .first_block()
        };
        ext_correct_indexes(ctx, inode, path, i, first_block)?;
    }

    Ok(())
}

/// 子节点首项变更后修正祖先 index 的 first_block 键。复刻 ext4_rs
/// `ext_correct_indexes`（ext4_impls/extents.rs:1678）——`child_level>0` 时经已有
/// `propagate_first_block_to_ancestors`（Task 3）向上传播。
fn ext_correct_indexes(
    ctx: &WriteCtx,
    inode: &mut Inode,
    path: &SearchPath,
    child_level: usize,
    first_block: u32,
) -> Result<()> {
    if child_level > 0 {
        propagate_first_block_to_ancestors(ctx, inode, path, child_level, first_block)?;
    }
    Ok(())
}

/// 释放 extent 覆盖的物理块 `[from, to]`（逻辑闭区间）。逐字节复刻 ext4_rs
/// `ext_remove_blocks`（ext4_impls/extents.rs:1691）——含越界/非法区间防御（PARITY），
/// i_blocks 由 `free_blocks` 内部按 512B 单位递减。
fn ext_remove_blocks(
    ctx: &WriteCtx,
    alloc: &mut dyn BlockAlloc,
    inode: &mut Inode,
    ex: &RawExtent,
    from: Ext4Lblk,
    to: Ext4Lblk,
) -> Result<()> {
    // PARITY: 非法区间防御（to<from / from<first_block 会下溢长度）→ 跳过（ext4_rs:1702-1708）。
    if to < from || from < ex.first_block() {
        return Ok(());
    }
    let len = to - from + 1;
    let num = from - ex.first_block();
    let start: u32 = ex.start() as u32 + num;
    let total = ctx.sb.blocks_count();
    // PARITY: 越界释放防御（start+len > blocks_count）→ 跳过（ext4_rs:1713-1719）。
    if (start as u64) + (len as u64) > total {
        return Ok(());
    }
    alloc.free_blocks(inode, start as Ext4Fsblk, len);
    // PARITY: ext4_rs `balloc_free_blocks`（balloc.rs:684-687）在每次释放后**立即**
    //   `write_back_inode`（落 i_blocks 到 inode 表）。core 的 `balloc_free_blocks` 只在
    //   `InodeAllocCtx` 内累减、刻意不落盘（Phase-2 设计），故必须在此把递减后的 i_blocks
    //   写回 inode 表——否则只 sync_tree_block 的非根叶删除路径会丢这次 i_blocks 落盘，
    //   导致盘上 i_blocks 偏高（under-decrement）。这与 ext4_rs「每次 free 都 write_back」逐次对齐。
    write_back_inode(ctx.writer, ctx.reader, ctx.sb, inode)?;
    Ok(())
}

/// 索引层是否还有要删的子节点。逐字节复刻 ext4_rs `more_to_rm`（ext4_impls/extents.rs:1723）。
fn more_to_rm(ctx: &WriteCtx, node: &ExtentPathNode, to: Ext4Lblk) -> bool {
    let header = node.header;

    // 无兄弟。
    if header.entries_count == 1 {
        return false;
    }

    let pos = node.position;
    if pos > header.entries_count as usize - 1 {
        return false;
    }

    if let Some(index) = node.index {
        let last_index_pos = header.entries_count as usize - 1;
        let block = load_tree_block(ctx, node.pblock_of_node);
        // PARITY: ext4_rs 读 last_index 用 `size_of::<Ext4ExtentIndex>() * last_index_pos`
        //   （= 12*pos，**缺 12 字节头偏移**）——既有怪癖，原样复刻（差分对拍兜底）。
        let off = EXT4_EXTENT_SIZE * last_index_pos;
        let last_index = if off + EXT4_EXTENT_SIZE <= block.len() {
            RawExtentIndex::from_bytes(&block[off..off + EXT4_EXTENT_SIZE])
        } else {
            RawExtentIndex::default()
        };

        if node.position > last_index_pos || index.first_block() > last_index.first_block() {
            return false;
        }

        if index.first_block() > to {
            return false;
        }
    }

    true
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::{EXTENT_MAGIC, RawExtent, RawExtentHeader};
    use crate::fs::ext4::core::block_group::RawGroupDescriptor;
    use crate::fs::ext4::core::superblock::RawSuperblock;
    use crate::fs::ext4::core::test_util::slice_at;
    use crate::prelude::*;

    #[ktest]
    fn extent_header_handcrafted_roundtrip() {
        let mut bytes = [0u8; 12];
        bytes[0..2].copy_from_slice(&EXTENT_MAGIC.to_le_bytes());
        bytes[2..4].copy_from_slice(&1u16.to_le_bytes());
        bytes[4..6].copy_from_slice(&4u16.to_le_bytes());
        let h = RawExtentHeader::from_bytes(&bytes);
        assert_eq!(h.as_bytes(), &bytes[..]);
        assert!(h.is_valid());
        assert_eq!(h.entries_count(), 1);
    }

    #[ktest]
    fn extent_unwritten_flag() {
        let mut e = RawExtent::default();
        e.block_count = 32768 + 5;
        assert!(e.is_unwritten());
        assert_eq!(e.len(), 5);
        e.block_count = 10;
        assert!(!e.is_unwritten());
        assert_eq!(e.len(), 10);
    }

    #[ktest]
    fn extent_root_header_real_image() {
        // 根 inode（ino=2）的 i_block 前 12 字节是 extent 头（根目录 extent-mapped）。
        let sb = RawSuperblock::from_bytes(slice_at(1024, 1024));
        let bs = sb.block_size();
        let gd = RawGroupDescriptor::from_bytes(slice_at((sb.first_data_block as usize + 1) * bs, 64));
        let inode_off = gd.inode_table() as usize * bs + (2 - 1) * sb.inode_size() as usize;
        // i_block 位于 inode 内偏移 40 起。
        let h_bytes = slice_at(inode_off + 40, 12);
        let h = RawExtentHeader::from_bytes(h_bytes);
        assert_eq!(h.as_bytes(), h_bytes);
        assert!(h.is_valid(), "root i_block must start with extent header magic 0xF30A");
        let old = ext4_rs::Ext4ExtentHeader::from_bytes(h_bytes);
        assert_eq!(h.magic, old.magic);
        assert_eq!(h.entries_count, old.entries_count);
        assert_eq!(h.depth, old.depth);
    }
}
