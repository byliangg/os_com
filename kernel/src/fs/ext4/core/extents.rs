// SPDX-License-Identifier: MPL-2.0
use ostd::const_assert;

use super::inode::Inode;
use super::io::BlockReader;
use super::prelude::*;
use super::superblock::RawSuperblock;

const EXTENT_MAGIC: u16 = 0xF30A;
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
