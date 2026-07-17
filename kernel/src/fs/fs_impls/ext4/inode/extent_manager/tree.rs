// SPDX-License-Identifier: MPL-2.0

//! [`ExtentTree`] — the validated, mutable extent tree of one inode.
//!
//! The tree root lives inline in the inode's 60-byte `i_block`; interior nodes
//! hold index entries pointing to child blocks read from the device, and leaf
//! nodes hold the extents mapping logical blocks to physical runs. The type
//! owns the root (validated once at construction — the parse-once boundary),
//! the inode's `i_blocks` sector accounting, and the dirty flag; every tree
//! operation is a method, so only a constructed (i.e. proven well-formed) tree
//! can be searched or mutated. Mirrors ext2's `BlockPtrTree`.

use core::ops::ControlFlow;

use super::{
    super::{
        super::{
            checksum::{self, InodeCsumSeed},
            fs::{AllocIntent, Ext4},
            journal,
            prelude::*,
        },
        RAW_BLOCK_PTRS_LEN,
    },
    es::{EsCache, EsCoverage, EsInvalidated},
    node::{
        ENTRY_SIZE, EXTENT_MAGIC, Extent, ExtentHeader, ExtentIdx, ExtentKind, MAX_DEPTH,
        MAX_UNWRITTEN_LEN, MAX_WRITTEN_LEN, NODE_CAPACITY, RawExtent, RawExtentHeader,
        RawExtentIdx,
    },
    path::{self, ExtentPath, NodeBuf, PathLevel, Search},
};

/// Maximum extents in the inline (depth-0) root: the 60-byte `i_block` holds a
/// 12-byte header plus four 12-byte entries.
const INLINE_MAX: usize = 4;

/// Maximum extents in one full-block external leaf node.
const LEAF_MAX: usize = NODE_CAPACITY;

/// Maximum index entries in one full-block external interior node — the same
/// geometry as a leaf, since an index entry is also 12 bytes. A depth-2 tree
/// therefore holds up to `INLINE_MAX × INTERIOR_MAX × LEAF_MAX` extents.
const INTERIOR_MAX: usize = NODE_CAPACITY;

/// 512-byte sectors per filesystem block; the unit `i_blocks` is counted in.
const SECTORS_PER_BLOCK: u64 = (BLOCK_SIZE / SECTOR_SIZE) as u64;

/// One cached external node: the physical block it was read from and a copy of
/// its full-block bytes (already validated at the [`NodeBuf::read`] that filled
/// it, and kept coherent by [`NodeBuf::write_back`]).
struct CachedNode {
    bid: Ext4Bid,
    bytes: Box<[u8; BLOCK_SIZE]>,
}

/// A tiny per-inode cache of recently walked external extent-tree nodes, living
/// inside the position-③ lock content ([`ExtentTree`]).
///
/// One write op descends the same leaf up to four times (`fill_holes`'
/// walk-and-search, then `convert_unwritten`'s walk-and-search), each otherwise
/// a full journal read funnel: a state lock, a `BTreeMap` probe, and a 4 KiB
/// copy. [`NODE_CACHE_SLOTS`] slots hold the last external nodes touched,
/// turning the repeats into a byte clone and — since [`NodeBuf::write_back`]
/// refreshes the slot — keeping the hot nodes resident across ops (steady
/// state: no funnel read). Two slots suffice for one op's own repeats (a
/// depth-2 op alternates interior + leaf), but a workload hopping between
/// several hot leaves thrashes them — the SQLite probe (2026-07-14) measured
/// ~1 miss/op with 2 slots because the database file's hot pages spread across
/// multiple extent leaves — so the capacity is sized to hold that working set.
///
/// # Coherence
///
/// A slot for `bid` always holds exactly the bytes [`NodeBuf::read`] would
/// return for it, or `bid` is absent. Two rules maintain this: every
/// [`NodeBuf::write_back`] [`store`](Self::store)s the node's final bytes, and
/// every free of an external node ([`free_meta_block`], the reused-block rebuild
/// in [`ExtentTree::reserialize`]) [`clear`](Self::clear)s the whole cache
/// (conservative — clearing only costs a refill). A tree metadata block is
/// mutated only by this inode's own `write_back` and freed only through those
/// funnels, so nothing else can desync a slot. `#[cfg(debug_assertions)]` reads
/// (every ktest) cross-check each hit against a fresh funnel read.
///
/// # Concurrency
///
/// The ③ `RwMutex` already serializes writers against all readers; the inner
/// `SpinLock` only guards the reader-vs-reader fill race (a shared-③ lookup may
/// populate a slot). It is a leaf lock — taken and dropped within a single slot
/// probe / store / clear, never held across [`NodeBuf::read`] (which can sleep)
/// or any other lock — so it adds no edge to the lock order at position ③.
///
/// Memory cost: at most [`NODE_CACHE_SLOTS`] 4 KiB blocks (32 KiB) per open
/// inode.
pub(super) struct NodeCache {
    inner: SpinLock<NodeCacheInner>,
}

/// The node-cache capacity. Sized for SQLite's multi-hot-leaf working set (see
/// [`NodeCache`]); the replacement policy (same-`bid` overwrite in place, else
/// round-robin) is capacity-agnostic.
const NODE_CACHE_SLOTS: usize = 8;

struct NodeCacheInner {
    slots: [Option<Box<CachedNode>>; NODE_CACHE_SLOTS],
    /// Round-robin victim when every slot is full and none matches.
    victim: usize,
}

impl NodeCache {
    /// Creates an empty cache. `const` so [`ExtentTree::empty`] stays `const`.
    pub(super) const fn new() -> Self {
        Self {
            inner: SpinLock::new(NodeCacheInner {
                slots: [const { None }; NODE_CACHE_SLOTS],
                victim: 0,
            }),
        }
    }

    /// Returns a clone of the cached bytes for `bid`, or `None` on a miss. The
    /// lock is dropped before the caller reconstructs the node, so it is never
    /// held across a device read.
    fn get(&self, bid: Ext4Bid) -> Option<Box<[u8; BLOCK_SIZE]>> {
        let inner = self.inner.lock();
        for slot in inner.slots.iter() {
            if let Some(cached) = slot
                && cached.bid == bid
            {
                return Some(cached.bytes.clone());
            }
        }
        None
    }

    /// Installs `bytes` for `bid`: overwrites the same-`bid` slot in place if
    /// present, else fills an empty slot, else evicts the round-robin victim.
    pub(super) fn store(&self, bid: Ext4Bid, bytes: &[u8; BLOCK_SIZE]) {
        let mut guard = self.inner.lock();
        let inner = &mut *guard;
        for slot in inner.slots.iter_mut() {
            if let Some(cached) = slot
                && cached.bid == bid
            {
                cached.bytes.copy_from_slice(bytes);
                return;
            }
        }
        for slot in inner.slots.iter_mut() {
            if slot.is_none() {
                *slot = Some(Box::new(CachedNode {
                    bid,
                    bytes: boxed_block(bytes),
                }));
                return;
            }
        }
        let victim = inner.victim;
        inner.slots[victim] = Some(Box::new(CachedNode {
            bid,
            bytes: boxed_block(bytes),
        }));
        inner.victim = (victim + 1) % inner.slots.len();
    }

    /// Drops every slot — the conservative invalidation any node free or
    /// whole-tree rebuild takes (clearing only loses a refill, never coherence).
    fn clear(&self) {
        let mut inner = self.inner.lock();
        inner.slots = [const { None }; NODE_CACHE_SLOTS];
        inner.victim = 0;
    }
}

/// Heap-allocates a block-sized copy of `bytes` without a 4 KiB stack temporary
/// (`Box::new([..])` would build the array on the stack first).
fn boxed_block(bytes: &[u8; BLOCK_SIZE]) -> Box<[u8; BLOCK_SIZE]> {
    let mut boxed = Box::new([0u8; BLOCK_SIZE]);
    boxed.copy_from_slice(bytes);
    boxed
}

/// Reads external node `bid` through `cache`: a hit clones the cached bytes
/// (skipping the journal read funnel), a miss reads through [`NodeBuf::read`]
/// and fills the slot. In debug builds a hit is verified against a fresh funnel
/// read — the coherence net every ktest exercises, since ktest is a debug build.
fn read_node_cached(fs: &Ext4, bid: Ext4Bid, cache: &NodeCache) -> Result<NodeBuf> {
    if let Some(bytes) = cache.get(bid) {
        // A transient funnel error here proves nothing about cache staleness
        // (the miss path would surface the same error as a plain `Err`), so an
        // unverifiable hit is skipped rather than escalated to a panic.
        #[cfg(debug_assertions)]
        if let Ok(fresh) = NodeBuf::read(fs, bid) {
            debug_assert_eq!(
                bytes.as_ref(),
                fresh.bytes(),
                "stale extent-node cache at bid {bid}"
            );
        }
        return Ok(NodeBuf::from_cached(bid, bytes));
    }
    let node = NodeBuf::read(fs, bid)?;
    cache.store(bid, node.bytes());
    Ok(node)
}

/// The validated, mutable extent tree of one inode, plus the `i_blocks`
/// accounting that every tree mutation must keep in step.
///
/// `root` is the inode's 60-byte `i_block` in its on-disk layout, **validated
/// at construction** ([`try_new`](Self::try_new)) and thereafter only rewritten
/// by this type's own mutators — so methods trust it without re-validating
/// (rule: parse once at the boundary). `sector_count` mirrors the inode's
/// `i_blocks` (data + extent-tree metadata, in 512-byte sectors); `dirty`
/// records whether either has changed since the last writeback.
///
/// This struct is the "ExtentTree" lock content at position ③ in the global
/// lock order (report §5.1); [`ExtentManager`](super::ExtentManager) wraps it
/// in the `RwMutex` and delegates.
pub(in crate::fs::fs_impls::ext4::inode) struct ExtentTree {
    root: [u32; RAW_BLOCK_PTRS_LEN],
    sector_count: u64,
    dirty: bool,
    /// The small cache of recently walked external nodes (see [`NodeCache`]).
    /// Interior mutability so `&self` walks may fill it; refreshed by
    /// `write_back` and cleared by any node free, all under the ③ lock.
    node_cache: NodeCache,
    /// The extent-status cache (see [`EsCache`]): the tree's per-block-truth
    /// fact set, the `NodeCache`'s logical-layer sibling. Interior mutability
    /// (an inner leaf `SpinLock` of the same discipline) so `&self` walks may
    /// record facts in passing; every LOGICAL mutator invalidates its span on
    /// entry — the leaf-entry editors demand the [`EsInvalidated`] credential
    /// only invalidation mints — and `reserialize` clears it whole beside the
    /// node cache. `pub(super)` so the `fill_holes` planning walk (R1, in the
    /// manager) can record its visited extents and probe its no-hole fast
    /// path (Q3) — via [`Self::es_cache`], the field itself stays private.
    es_cache: EsCache,
}

impl ExtentTree {
    /// Validates `root`'s extent header once and takes ownership of the tree.
    ///
    /// This is the parse boundary: a bad magic / entry count / depth is
    /// rejected here, and every later method call trusts the root.
    pub(super) fn try_new(root: [u32; RAW_BLOCK_PTRS_LEN], sector_count: u64) -> Result<Self> {
        let header = ExtentHeader::try_from(&RawExtentHeader::from_bytes(
            &root.as_bytes()[0..ENTRY_SIZE],
        ))?;
        // The inline root holds a 12-byte header plus at most `INLINE_MAX`
        // 12-byte entries in the 60-byte `i_block`. `ExtentHeader::try_from`
        // only checks `entries <= max`; bound both here so a crafted image
        // with `entries`/`max` above the inline capacity is rejected at this
        // parse boundary rather than overrunning `i_block` in a later scan
        // (the root scanners read `root.as_bytes()[12*(1+i)..]` with no
        // container length of their own).
        if header.entries() as usize > INLINE_MAX || header.max() as usize > INLINE_MAX {
            return_errno_with_message!(
                Errno::EUCLEAN,
                "inline extent root exceeds i_block capacity"
            );
        }
        Ok(Self {
            root,
            sector_count,
            dirty: false,
            node_cache: NodeCache::new(),
            es_cache: EsCache::new(),
        })
    }

    /// A valid empty tree: a depth-0 header (magic, 0 entries, max 4) followed
    /// by zeros — what a freshly created regular file or directory carries, so
    /// the extent reader sees a well-formed (empty) tree from the first byte.
    pub(in crate::fs::fs_impls::ext4::inode) const fn empty() -> Self {
        let mut root = [0u32; RAW_BLOCK_PTRS_LEN];
        // Each `i_block` word packs two 16-bit fields, little-endian: word 0 is
        // `eh_magic | eh_entries(=0)`, word 1 is `eh_max(=4) | eh_depth(=0)`.
        root[0] = EXTENT_MAGIC as u32;
        root[1] = INLINE_MAX as u32;
        Self {
            root,
            sector_count: 0,
            dirty: false,
            node_cache: NodeCache::new(),
            es_cache: EsCache::new(),
        }
    }

    /// Returns the root in its on-disk 60-byte layout — the serialization
    /// boundary for the inode writeback (`i_block`).
    pub(in crate::fs::fs_impls::ext4::inode) const fn root_bytes(
        &self,
    ) -> &[u32; RAW_BLOCK_PTRS_LEN] {
        &self.root
    }

    /// Returns the inode's `i_blocks` (512-byte sectors) accounting.
    pub(super) const fn sector_count(&self) -> u64 {
        self.sector_count
    }

    /// Returns whether the tree or `i_blocks` has changed since the last
    /// writeback.
    pub(super) const fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Clears the dirty flag after a successful inode writeback.
    ///
    /// This does NOT touch [`es_cache`](Self::es_cache): a writeback changes
    /// no mapping or kind, so every cached fact stays true (per-block truth),
    /// and dropping them here would defeat the write fast paths after every
    /// writeback for nothing.
    pub(super) fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    /// Returns the tree depth (0 = inline leaf, 1 = one level of index blocks,
    /// 2 = two levels). Infallible: the root was validated at construction. Used
    /// to assert tree shape in tests and to size the per-chunk journal credit
    /// estimate for write/truncate/reclaim ([`ExtentManager::root_depth`]).
    pub(super) fn depth(&self) -> u16 {
        self.header().depth()
    }

    /// Returns the root's header, decoded from the trusted
    /// (construction-validated) bytes.
    fn header(&self) -> ExtentHeader {
        ExtentHeader::from_trusted(&RawExtentHeader::from_bytes(
            &self.root.as_bytes()[0..ENTRY_SIZE],
        ))
    }

    /// Walks the tree to find the extent covering `iblock`, returning `None`
    /// for a hole. A thin wrapper over [`find`](Self::find) that drops the
    /// path (the read-only callers need just the verdict).
    pub(super) fn lookup(&self, fs: &Ext4, iblock: Iblock) -> Result<Option<Extent>> {
        match self.find(fs, iblock)? {
            Search::Covered { extent, .. } => Ok(Some(extent)),
            Search::Gap { .. } => Ok(None),
        }
    }

    /// Walks the tree to the leaf landing position for `iblock`: the covering
    /// extent, or the hole's insertion point with its in-leaf predecessor
    /// (see [`Search`]) — the path-recording walker the in-place surgery
    /// (P9a) edits through.
    ///
    /// External nodes are read through the journal funnel ([`NodeBuf::read`]):
    /// on a journaled volume a node's newest bytes may still sit in a journal
    /// capture (WAL suppresses the direct write until checkpoint), so a bare
    /// device read here would walk a stale tree. Each child's depth must step
    /// down by exactly one from its parent's, so a corrupt (loopy or grafted)
    /// tree fails loud instead of walking forever. When `iblock` precedes a
    /// node's first entry the walk descends into child 0 (Linux-style); on a
    /// well-formed tree no leaf under child 0 maps anything below its first
    /// key, so the landing is the same hole answer the old short-circuit gave.
    pub(super) fn find(&self, fs: &Ext4, iblock: Iblock) -> Result<Search> {
        let header = self.header();
        let nr = header.entries() as usize;

        if header.is_leaf() {
            let chosen = path::last_key_le(nr, |i| self.root_extent_at(i).block(), iblock);
            let (pos, landing) = leaf_landing(chosen, |i| self.root_extent_at(i), iblock);
            let path = ExtentPath {
                root_pos: pos,
                levels: Vec::new(),
            };
            return Ok(landing.into_search(path));
        }

        let root_pos =
            path::last_key_le(nr, |i| self.root_index_at(i).block(), iblock).unwrap_or(0);
        let mut next_bid = self.root_index_at(root_pos).leaf();
        let mut levels: Vec<PathLevel> = Vec::with_capacity(header.depth() as usize);

        for expected_depth in (0..header.depth()).rev() {
            let node = read_node_cached(fs, next_bid, &self.node_cache)?;
            if node.depth() != expected_depth {
                return_errno_with_message!(
                    Errno::EUCLEAN,
                    "extent child depth does not step down by one"
                );
            }
            if node.is_leaf() {
                let chosen = node.leaf_pos(iblock);
                let (pos, landing) = leaf_landing(chosen, |i| node.extent_at(i), iblock);
                levels.push(PathLevel { node, pos });
                let path = ExtentPath { root_pos, levels };
                return Ok(landing.into_search(path));
            }
            let pos = node.index_pos(iblock).unwrap_or(0);
            next_bid = node.index_at(pos).leaf();
            levels.push(PathLevel { node, pos });
        }
        // The countdown ends at depth 0, whose node is a leaf and returned above.
        return_errno_with_message!(Errno::EUCLEAN, "extent walk fell through its own depth");
    }

    /// Calls `visit_fn` on each extent overlapping `[range.start, range.end)`
    /// in ascending logical order, descending only the subtrees the range
    /// touches — the bounded replacement for whole-tree flattens on the read
    /// paths. The range is `u64` because a length-derived end (`iblock + len`)
    /// can exceed the 32-bit logical space by up to one extent.
    pub(super) fn walk_range(
        &self,
        fs: &Ext4,
        range: Range<u64>,
        visit_fn: &mut impl FnMut(&Extent) -> ControlFlow<()>,
    ) -> Result<()> {
        if range.start >= range.end {
            return Ok(());
        }
        // Nothing maps at or above 2^32 logical blocks: an out-of-space start
        // has nothing to visit.
        let Ok(start_key) = Iblock::try_from(range.start) else {
            return Ok(());
        };
        let header = self.header();
        let nr = header.entries() as usize;

        if header.is_leaf() {
            for i in 0..nr {
                let e = self.root_extent_at(i);
                if e.block() as u64 >= range.end {
                    break;
                }
                if e.block() as u64 + e.len() as u64 > range.start && visit_fn(&e).is_break() {
                    return Ok(());
                }
            }
            return Ok(());
        }

        let first =
            path::last_key_le(nr, |i| self.root_index_at(i).block(), start_key).unwrap_or(0);
        for i in first..nr {
            let child = self.root_index_at(i);
            if child.block() as u64 >= range.end {
                break;
            }
            if walk_child(
                fs,
                child.leaf(),
                header.depth() - 1,
                &range,
                visit_fn,
                &self.node_cache,
            )?
            .is_break()
            {
                return Ok(());
            }
        }
        Ok(())
    }

    /// The pre-surgery linear walker, kept verbatim as the ktest
    /// cross-verification reference for [`find`](Self::find): both must give
    /// the same covered-extent / hole verdict on every probe of any tree.
    #[cfg(ktest)]
    pub(super) fn lookup_linear(&self, fs: &Ext4, iblock: Iblock) -> Result<Option<Extent>> {
        let root_bytes = self.root.as_bytes();
        let mut next_bid = match search_entries(&self.header(), root_bytes, iblock)? {
            Step::Found(extent) => return Ok(Some(extent)),
            Step::Hole => return Ok(None),
            Step::Descend(bid) => bid,
        };

        let journal = fs.journal();
        let device = fs.block_device().as_ref();
        for _ in 0..MAX_DEPTH {
            let block = journal::read_metadata_block(journal.as_deref(), device, next_bid)?;
            match search_node(&block, iblock)? {
                Step::Found(extent) => return Ok(Some(extent)),
                Step::Hole => return Ok(None),
                Step::Descend(bid) => next_bid = bid,
            }
        }
        return_errno_with_message!(Errno::EUCLEAN, "extent tree deeper than maximum depth");
    }

    /// Walks the right spine of the tree, descending into the LAST index entry
    /// at each level, and returns the path down to the rightmost external
    /// leaf. `None` on a depth-0 tree (the inline root is itself the leaf) or
    /// an empty tree. Each child's depth must step down by exactly one (loop /
    /// graft guard, as in [`find`](Self::find)).
    ///
    /// The tail-truncate spine reads the rightmost leaf, frees its doomed
    /// entries, and (when it empties) prunes upward — re-walking here after
    /// each prune, which reads the just-written-back parent through the
    /// journal funnel.
    fn rightmost_path(&self, fs: &Ext4) -> Result<Option<ExtentPath>> {
        let header = self.header();
        if header.is_leaf() {
            return Ok(None);
        }
        let nr = header.entries() as usize;
        // A well-formed non-leaf root has ≥ 1 child (an empty index root is
        // reset to a depth-0 leaf by the pruning below). A crafted empty index
        // root is corruption — fail loud rather than underflow `nr - 1`
        // (matching `find`'s `unwrap_or(0)` robustness on the same input).
        if nr == 0 {
            return_errno_with_message!(Errno::EUCLEAN, "non-leaf extent root has no children");
        }
        let root_pos = nr - 1;
        let mut next_bid = self.root_index_at(root_pos).leaf();
        let mut levels: Vec<PathLevel> = Vec::with_capacity(header.depth() as usize);
        for expected_depth in (0..header.depth()).rev() {
            let node = read_node_cached(fs, next_bid, &self.node_cache)?;
            if node.depth() != expected_depth {
                return_errno_with_message!(
                    Errno::EUCLEAN,
                    "extent child depth does not step down by one"
                );
            }
            // `NodeBuf::read` rejects an empty interior; a committed tree never
            // keeps an empty external leaf either (a truncate prunes it), so an
            // empty node here is corruption — fail loud, don't underflow `- 1`.
            let Some(pos) = node.entries().checked_sub(1) else {
                return_errno_with_message!(Errno::EUCLEAN, "rightmost extent node is empty");
            };
            if node.is_leaf() {
                levels.push(PathLevel { node, pos });
                return Ok(Some(ExtentPath { root_pos, levels }));
            }
            next_bid = node.index_at(pos).leaf();
            levels.push(PathLevel { node, pos });
        }
        return_errno_with_message!(Errno::EUCLEAN, "extent walk fell through its own depth");
    }

    /// Inserts the extent mapping `[iblock, iblock+len)` → `[pblock,
    /// pblock+len)`, growing `i_blocks` by the `len` data blocks plus the net
    /// metadata-block delta.
    ///
    /// The insert is in-place surgery (P9a): the landing leaf is edited
    /// directly ([`try_insert_in_place`](Self::try_insert_in_place) — a
    /// predecessor merge or a shift-insert with ancestor key correction), and
    /// a full path is reorganized first
    /// ([`make_room_for`](Self::make_room_for): splits, or a depth growth up
    /// to [`MAX_DEPTH`]). Only a depth-0 tree rebuilds: its inline root is
    /// read directly and re-serialized as an inline root or one fresh leaf
    /// (see [`reserialize`](Self::reserialize)). The caller must guarantee
    /// `[iblock, iblock+len)` is currently a hole (the write path only
    /// inserts for unmapped blocks).
    ///
    /// External leaf blocks are reused in place across mutations. Under a
    /// journal handle their reads and writes go through the journal funnels
    /// ([`journal::read_metadata_block`], capture + patch), so WAL order
    /// holds; without one they are read and written directly (Phases 1–3
    /// semantics).
    #[expect(clippy::too_many_arguments)]
    pub(super) fn insert(
        &mut self,
        fs: &Ext4,
        iblock: Iblock,
        pblock: Ext4Bid,
        len: u16,
        kind: ExtentKind,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
    ) -> Result<()> {
        // es: the new mapping's own span. A neighbour merge changes extent
        // boundaries but no block's mapping or kind, so per-block truth owes
        // it no wider span (see the `es` module docs).
        let es = self
            .es_cache
            .invalidate_range(iblock as u64..iblock as u64 + len as u64);
        // Surgery route (T2 fast path + T3 room-making): edit exactly the
        // landing leaf, splitting full nodes or growing the tree a level when
        // the path has no room. Every `make_room_for` strictly adds capacity
        // on the path (a split gives the landing leaf room; a grow adds a
        // level the next round splits), so the retry bound is unreachable
        // except on a corrupt tree — fail loud rather than spin.
        if self.header().depth() > 0 {
            for _ in 0..(MAX_DEPTH as usize + 2) {
                if let InPlaceInsert::Inserted =
                    self.try_insert_in_place(fs, iblock, pblock, len, kind, handle, csum_seed, &es)?
                {
                    // Data blocks only: the leaf was edited in place; any
                    // metadata the room-making allocated was accounted there.
                    self.sector_count = (self.sector_count as i64
                        + len as i64 * SECTORS_PER_BLOCK as i64)
                        .max(0) as u64;
                    self.dirty = true;
                    return Ok(());
                }
                self.make_room_for(fs, iblock, handle, csum_seed, AllocIntent::Normal)?;
            }
            return_errno_with_message!(Errno::EUCLEAN, "extent insert cannot make room");
        }

        // Depth-0: the inline rebuild — memory-only up to INLINE_MAX extents,
        // and the exact, cheap builder of the first external leaf beyond. The
        // root is read directly (≤ INLINE_MAX entries, no device I/O); a
        // depth-0 tree has no external nodes to reuse.
        let mut extents: Vec<Extent> = {
            let n = self.header().entries() as usize;
            (0..n).map(|i| self.root_extent_at(i)).collect()
        };
        extents.push(Extent::new(iblock, len, pblock, kind));
        merge_extents(&mut extents);
        let delta = self.reserialize(fs, &extents, &[], handle, csum_seed, AllocIntent::Normal)?;

        let net_meta = delta.meta_allocated as i64 - delta.meta_freed as i64;
        let added_blocks = len as i64 + net_meta;
        // `.max(0)`: `i_blocks` must never wrap negative-to-huge on a
        // miscounted delta.
        self.sector_count =
            (self.sector_count as i64 + added_blocks * SECTORS_PER_BLOCK as i64).max(0) as u64;
        self.dirty = true;
        Ok(())
    }

    /// Attempts the in-place leaf insert of `[iblock, iblock+len) → pblock`:
    /// merge onto the in-leaf predecessor (absorbing a bridged successor), or
    /// shift-insert into free space, correcting ancestor index keys when the
    /// leaf's first key drops. [`InPlaceInsert::LeafFull`] — tree untouched —
    /// tells the caller to reorganize (`make_room_for`) and retry.
    ///
    /// The caller guarantees the whole range is a hole (the write path plans
    /// from a snapshot under this same ③ lock), so the landing must be a
    /// [`Search::Gap`]; a covered landing means the tree contradicts the plan
    /// — fail loud rather than corrupt.
    #[expect(clippy::too_many_arguments)]
    fn try_insert_in_place(
        &mut self,
        fs: &Ext4,
        iblock: Iblock,
        pblock: Ext4Bid,
        len: u16,
        kind: ExtentKind,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
        es: &EsInvalidated,
    ) -> Result<InPlaceInsert> {
        let e = Extent::new(iblock, len, pblock, kind);
        let Search::Gap { mut path, prev } = self.find(fs, iblock)? else {
            return_errno_with_message!(Errno::EUCLEAN, "extent insert target is already mapped");
        };
        let device = fs.block_device();
        // The caller gated on depth ≥ 1, so the path ends in an external leaf.
        let leaf_level = path.levels.len() - 1;
        let insert_pos = path.levels[leaf_level].pos;

        // Merge onto the predecessor: entry count unchanged, first key
        // unchanged (a predecessor exists, so the position is ≥ 1) — no index
        // correction, one leaf write.
        if let Some(p) = prev
            && can_merge(&p, &e)
        {
            let leaf = &mut path.levels[leaf_level].node;
            let mut merged = merged_pair(&p, &e);
            // The grown run may now bridge flush against the old successor
            // (still named by `insert_pos`: nothing shifted).
            if insert_pos < leaf.entries() {
                let next = leaf.extent_at(insert_pos);
                if can_merge(&merged, &next) {
                    merged = merged_pair(&merged, &next);
                    leaf.remove_extent_at(insert_pos, es);
                }
            }
            leaf.replace_extent_at(insert_pos - 1, &merged, es);
            leaf.write_back(device.as_ref(), handle, csum_seed, Some(&self.node_cache))?;
            return Ok(InPlaceInsert::Inserted);
        }

        // Shift-insert into free space; a full leaf is the rebuild fallback.
        // The capacity check (and the in-buffer edit) precedes every durable
        // write, so a `false` return leaves the tree untouched.
        {
            let leaf = &mut path.levels[leaf_level].node;
            if leaf.insert_extent_at(insert_pos, &e, es).is_err() {
                return Ok(InPlaceInsert::LeafFull);
            }
            // The new run may bridge flush against its successor.
            if insert_pos + 1 < leaf.entries() {
                let next = leaf.extent_at(insert_pos + 1);
                if can_merge(&e, &next) {
                    leaf.replace_extent_at(insert_pos, &merged_pair(&e, &next), es);
                    leaf.remove_extent_at(insert_pos + 1, es);
                }
            }
        }

        // Inserted at the leaf's position 0: the leaf's first key dropped, so
        // the ancestor index keys naming this subtree drop with it (Linux
        // `ext4_ext_correct_indexes`, extents.c:1705 — propagate upward while
        // each level sits at position 0). The ancestors are written BEFORE the
        // leaf: a key alone (old or new) is a valid lower bound either way, so
        // every intermediate error state is a well-formed tree that does NOT
        // yet reference the new data blocks — if any write fails here or at
        // the leaf, the caller's on-error free of those blocks cannot strand a
        // mapped-but-freed extent.
        if insert_pos == 0 {
            self.correct_ancestor_keys(fs, &mut path, leaf_level, iblock, handle, csum_seed)?;
        }

        // The leaf lands last (see above).
        path.levels[leaf_level].node.write_back(
            device.as_ref(),
            handle,
            csum_seed,
            Some(&self.node_cache),
        )?;
        Ok(InPlaceInsert::Inserted)
    }

    /// Rewrites the ancestor index keys naming the subtree under
    /// `path.levels[child_level]` to `new_key`, propagating upward while each
    /// level sits at position 0 (Linux `ext4_ext_correct_indexes`,
    /// extents.c:1705). Needed whenever a node's FIRST key changes — an insert
    /// at position 0 lowers it, a punch that removes or re-keys position 0
    /// raises it — because Linux's read-side validation demands the parent key
    /// EQUAL the child's first key exactly, not merely lower-bound it.
    fn correct_ancestor_keys(
        &mut self,
        fs: &Ext4,
        path: &mut ExtentPath,
        child_level: usize,
        new_key: Iblock,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
    ) -> Result<()> {
        let device = fs.block_device();
        let mut level = child_level;
        loop {
            if level == 0 {
                self.set_root_index_key(path.root_pos, new_key);
                self.dirty = true;
                break;
            }
            let parent = &mut path.levels[level - 1];
            parent.node.set_index_key_at(parent.pos, new_key);
            parent
                .node
                .write_back(device.as_ref(), handle, csum_seed, Some(&self.node_cache))?;
            if parent.pos != 0 {
                break;
            }
            level -= 1;
        }
        Ok(())
    }

    /// Rewrites root index entry `i`'s key, keeping its child pointer. The
    /// root lives in the in-memory `i_block` and reaches disk with the inode
    /// writeback (no block capture of its own), like every other root rewrite.
    fn set_root_index_key(&mut self, i: usize, key: Iblock) {
        let off = ENTRY_SIZE * (1 + i);
        let bytes = self.root.as_mut_bytes();
        let mut raw = RawExtentIdx::from_bytes(&bytes[off..off + ENTRY_SIZE]);
        raw.block = key;
        bytes[off..off + ENTRY_SIZE].copy_from_slice(raw.as_bytes());
    }

    /// Inserts index entry `idx` at root position `i`, shifting later entries
    /// right — in-memory, like every root rewrite. Caller checked the root has
    /// room ([`INLINE_MAX`]).
    fn insert_root_index_at(&mut self, i: usize, idx: &RawExtentIdx) {
        let header = self.header();
        let n = header.entries() as usize;
        debug_assert!(n < INLINE_MAX && i <= n);
        let bytes = self.root.as_mut_bytes();
        let start = ENTRY_SIZE * (1 + i);
        let end = ENTRY_SIZE * (1 + n);
        bytes.copy_within(start..end, start + ENTRY_SIZE);
        bytes[start..start + ENTRY_SIZE].copy_from_slice(idx.as_bytes());
        let mut raw = RawExtentHeader::from_bytes(&bytes[0..ENTRY_SIZE]);
        raw.entries = (n + 1) as u16;
        bytes[0..ENTRY_SIZE].copy_from_slice(raw.as_bytes());
    }

    /// Grows the tree one level deeper (Linux `ext4_ext_grow_indepth`,
    /// extents.c:1311): the root's entire content moves into a freshly
    /// allocated full-block node, and the root becomes a one-entry index
    /// pointing at it. The new node is written (journaled) BEFORE the
    /// in-memory root flips, so an error leaves the old tree fully intact and
    /// only the (rolled-back) allocation touched.
    fn grow_root(
        &mut self,
        fs: &Ext4,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
        intent: AllocIntent,
    ) -> Result<()> {
        let header = self.header();
        // The on-disk format's depth ceiling (every consumer went path-based
        // with the P9a surgery — T3 capped growth at 2 while flatten-based
        // readers remained; T6 lifted it). A tree at MAX_DEPTH holding its
        // full fan-out maps more blocks than the 32-bit logical space, so a
        // well-formed tree never trips this; it guards a corrupt on-disk
        // depth from growing further.
        if header.depth() >= MAX_DEPTH {
            return_errno_with_message!(Errno::ENOSPC, "extent tree would exceed maximum depth");
        }
        let n = header.entries() as usize;
        debug_assert!(n > 0, "only a full root grows, and full is non-empty");
        let root_bytes = self.root.as_bytes();
        let first_key = if header.is_leaf() {
            self.root_extent_at(0).block()
        } else {
            self.root_index_at(0).block()
        };
        // Goal: near the first child (interior root) or first data run (leaf
        // root) for locality.
        let goal = if header.is_leaf() {
            self.root_extent_at(0).start()
        } else {
            self.root_index_at(0).leaf()
        };

        let bid = alloc_meta_block(fs, goal, handle, intent)?;
        let mut node = NodeBuf::fresh(bid, header.depth());
        // The root's entries are a prefix-compatible layout (same 12-byte
        // slabs); copy them verbatim under the full-block header.
        node.adopt_entries(&root_bytes[ENTRY_SIZE..ENTRY_SIZE * (1 + n)], n);
        if let Err(err) = node.write_back(
            fs.block_device().as_ref(),
            handle,
            csum_seed,
            Some(&self.node_cache),
        ) {
            rollback_meta_blocks(fs, &[bid], handle, &self.node_cache);
            return Err(err);
        }

        // Publish: the root becomes a one-entry index one level up (in-memory,
        // infallible; it rides the inode writeback).
        self.write_index_root(&[make_index_entry(first_key, bid)], header.depth() + 1);
        self.sector_count += SECTORS_PER_BLOCK;
        self.dirty = true;
        Ok(())
    }

    /// Makes room on the path to `iblock` so the next in-place edit attempt
    /// succeeds: splits the full nodes along the path (Linux
    /// `ext4_ext_create_new_leaf`/`ext4_ext_split`, extents.c:1398/1052), or
    /// grows the tree a level when the whole path up to the root is full.
    /// Only reorganizes EXISTING entries — the caller's new extent (an
    /// insert's landing, or a convert's in-leaf split) lands afterwards, so
    /// no intermediate state here references new data blocks. The landing may
    /// be a gap (insert) or a covered entry (convert): a split at a covered
    /// position keeps the entry with ≥ 1 free slot beside it either way.
    ///
    /// Write ordering (the always-valid discipline): fresh nodes first (still
    /// unreferenced), then the shrunk old nodes, then the one landing write
    /// (or the in-memory root edit) that publishes the new subtree. Between
    /// the shrink and the publish the moved entries live only in the
    /// unpublished new nodes; if the single publish write fails, a best-effort
    /// restore re-grows the old nodes. The alternative order (publish before
    /// shrink) would expose a both-sides-referenced state whose escape on an
    /// error return double-frees on the next truncate — strictly worse than
    /// this order's worst case (a leak e2fsck reclaims).
    fn make_room_for(
        &mut self,
        fs: &Ext4,
        iblock: Iblock,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
        intent: AllocIntent,
    ) -> Result<()> {
        let (Search::Gap { mut path, .. } | Search::Covered { mut path, .. }) =
            self.find(fs, iblock)?;
        let leaf_level = path.levels.len() - 1;
        if !path.levels[leaf_level].node.is_full() {
            // Spurious call (already room): nothing to do.
            return Ok(());
        }

        // The contiguous run of full nodes from the leaf upward; `land` is the
        // first level with room above them (`None` = the root is the landing).
        let mut top = leaf_level;
        while top > 0 && path.levels[top - 1].node.is_full() {
            top -= 1;
        }
        if top == 0 && self.header().entries() as usize >= INLINE_MAX {
            // Full all the way through the root: grow a level and let the
            // caller's loop retry (the copied-down root usually still needs a
            // split, handled by the next round).
            return self.grow_root(fs, handle, csum_seed, intent);
        }

        let device = fs.block_device();
        // Allocate one fresh node per full level, leaf-first (goal: next to
        // the old leaf). On any failure the fresh blocks roll back; nothing
        // was referenced yet.
        let goal = path.levels[leaf_level].node.bid() + 1;
        let mut fresh: Vec<NodeBuf> = Vec::with_capacity(leaf_level - top + 1);
        let mut fresh_bids: Vec<Ext4Bid> = Vec::with_capacity(fresh.capacity());
        for level in (top..=leaf_level).rev() {
            let bid = match alloc_meta_block(fs, goal, handle, intent) {
                Ok(bid) => bid,
                Err(err) => {
                    rollback_meta_blocks(fs, &fresh_bids, handle, &self.node_cache);
                    return Err(err);
                }
            };
            fresh_bids.push(bid);
            fresh.push(NodeBuf::fresh(bid, path.levels[level].node.depth()));
        }

        // Stage the reorganization in memory, leaf upward. `carry` is the
        // index entry publishing each level's fresh node into the level above.
        // The staging is pure memory: nothing durable happens until the write
        // phase below.
        let mut carry: Option<RawExtentIdx> = None;
        for (i, level) in (top..=leaf_level).rev().enumerate() {
            let at = if level == leaf_level {
                // The leaf splits at the insertion point, but never at 0: a
                // before-first insert (`pos == 0`) keeps the leaf's original
                // first entry in the old node (Linux's `ext4_ext_split` moves
                // from `p_ext + 1`). Splitting at 0 would empty the old leaf
                // and key the fresh node at the same block as the old leaf's
                // unchanged parent index entry — an out-of-order (duplicate-
                // key) index a crash in the retry window could persist, which
                // Linux's `ext4_valid_extent_entries` rejects on read.
                path.levels[level].pos.max(1)
            } else {
                // An interior level keeps its descent child (position `pos`)
                // on the old side.
                path.levels[level].pos + 1
            };
            path.levels[level].node.move_upper_into(at, &mut fresh[i]);
            if let Some(c) = carry.take() {
                // The child carry keys between the old node's kept tail and
                // the fresh node's first moved entry: append to old when it
                // has room, else prepend to a (necessarily empty) fresh node.
                // Both targets are proven to have room (old just passed the
                // `!is_full` check; a full old means it kept every entry, so
                // `at` moved none and fresh is empty) — a full-node `ENOSPC`
                // is unreachable, so the failure would only mean a logic bug,
                // not a runtime condition to leak `fresh_bids` on.
                let old = &mut path.levels[level].node;
                if !old.is_full() {
                    let pos = old.entries();
                    old.insert_index_at(pos, &c)
                        .expect("carry into a non-full interior cannot overflow");
                } else {
                    fresh[i]
                        .insert_index_at(0, &c)
                        .expect("carry into an empty fresh interior cannot overflow");
                }
            }
            // A split that keeps `>= 1` entry in the old node (the `at.max(1)`
            // leaf case and every interior split) gives the fresh node its
            // real first key; a pure-append leaf split (`pos == entries`)
            // leaves the fresh node empty and keys it at the insertion target
            // — a valid lower bound for the extent about to land there, above
            // everything kept below it (Linux keys an append's new leaf the
            // same way).
            let key = if fresh[i].entries() > 0 {
                fresh[i].first_key()
            } else {
                iblock
            };
            carry = Some(make_index_entry(key, fresh[i].bid()));
        }
        let final_carry = carry
            .take()
            .expect("the leaf split always produces a carry");

        // WRITES. Phase 1 — fresh nodes: still unreferenced, so any failure
        // rolls back to a fully intact old tree (the shrinks above live only
        // in this function's buffers).
        for node in fresh.iter_mut() {
            if let Err(err) =
                node.write_back(device.as_ref(), handle, csum_seed, Some(&self.node_cache))
            {
                rollback_meta_blocks(fs, &fresh_bids, handle, &self.node_cache);
                return Err(err);
            }
        }

        // Phase 2 — the shrunk old nodes, then the landing publish. Until the
        // FIRST shrink write lands the disk is still the fully intact old tree
        // (the fresh nodes are written but referenced by nothing), so that one
        // failure rolls back clean by freeing the fresh blocks. Once a shrunk
        // node has landed the moved entries live only in the still-unpublished
        // fresh nodes: the state is irreversible inside this transaction, and
        // any later failure escalates through the P7e error funnel — the
        // journal aborts, the captures never commit, and crash atomicity
        // discards the half-state (Linux `ext4_std_error` posture). A
        // non-journaled volume has no abort; the failure leaves the same
        // torn-metadata exposure every multi-block mutation has there (P1–3).
        let mut shrink_landed = false;
        let publish = (|| -> Result<()> {
            for level in top..=leaf_level {
                path.levels[level].node.write_back(
                    device.as_ref(),
                    handle,
                    csum_seed,
                    Some(&self.node_cache),
                )?;
                shrink_landed = true;
            }
            if top == 0 {
                // The root landing is an in-memory edit: infallible, closing
                // the window with no fallible step after the shrink writes.
                self.insert_root_index_at(path.root_pos + 1, &final_carry);
            } else {
                let landing = &mut path.levels[top - 1];
                let pos = landing.pos + 1;
                landing.node.insert_index_at(pos, &final_carry)?;
                landing.node.write_back(
                    device.as_ref(),
                    handle,
                    csum_seed,
                    Some(&self.node_cache),
                )?;
            }
            Ok(())
        })();
        if let Err(err) = publish {
            if shrink_landed {
                if let Some(h) = handle {
                    h.abort_journal_on_fs_error();
                }
            } else {
                rollback_meta_blocks(fs, &fresh_bids, handle, &self.node_cache);
            }
            return Err(err);
        }

        self.sector_count += fresh_bids.len() as u64 * SECTORS_PER_BLOCK;
        self.dirty = true;
        Ok(())
    }

    /// Converts the unwritten parts of the logical range `[iblock, iblock +
    /// len)` to written, splitting any overlapping unwritten extent so the
    /// written sub-range keeps the same physical mapping. Used by the write
    /// path so data written into preallocated (unwritten) extents becomes
    /// readable.
    ///
    /// Each overlapping unwritten extent is edited in place (P9a-T5), one
    /// boundary per round, each round ONE self-consistent leaf edit (Linux
    /// `ext4_split_extent`'s staged splits): a fully covered extent flips its
    /// kind and coalesces with contiguous same-kind neighbours; a partially
    /// covered one splits inside its leaf, reorganizing a full leaf FIRST
    /// (`make_room_for` — a clean failure point that edits nothing in the
    /// range). An error mid-range therefore leaves a valid tree that still
    /// maps every block, with the conversion simply cut short; it propagates
    /// as a plain error, never a journal abort. Landed flips stay in the
    /// transaction — the write path's error arm registers their ordered-data
    /// flush (the Unwritten-first coupling), see `write_at_once`.
    ///
    /// No data blocks are allocated or freed: the physical mapping is
    /// preserved, so `i_blocks` changes only by the metadata blocks a leaf
    /// reorganization may add (accounted inside `make_room_for`).
    pub(super) fn convert_unwritten(
        &mut self,
        fs: &Ext4,
        iblock: Iblock,
        len: u32,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
    ) -> Result<()> {
        let range_start = iblock;
        let range_end = iblock as u64 + len as u64;
        // Q2 (es-cache): a range already proven all written needs no flip —
        // the whole call is a no-op, so return BEFORE the invalidation below
        // (nothing will change, so no fact may be dropped), without walking
        // or dirtying. `write_at` calls this on every write; a warm cache
        // thus turns the every-write gate's bounded scan into one ordered
        // map probe. The end narrowing is checked: a range spilling past the
        // 32-bit logical space simply misses (nothing maps up there anyway).
        if let Ok(end) = Iblock::try_from(range_end)
            && range_start < end
            && self.es_cache.range_state(range_start, end) == EsCoverage::AllWritten
        {
            // The debug double-read net (`es` module docs, layer 3): re-walk
            // behind the hit — the es lock is already released — and panic on
            // any over-claim. Every ktest is a debug build.
            #[cfg(debug_assertions)]
            self.debug_assert_es_coverage(fs, range_start, end, EsCoverage::AllWritten);
            return Ok(());
        }
        // es: entry-invalidation discipline — a mid-range error then leaves
        // the cache only too empty, never too true. The scan below re-records
        // what it proves (R2) and each landed flip backfills its fact (R3).
        let es = self
            .es_cache
            .invalidate_range(range_start as u64..range_end);

        // Depth-0 (inline root as a leaf, the common small-file shape): there
        // is no external node to edit — rewrite the root in memory, riding the
        // inode writeback. Delegated for the same reason as the punch: edge
        // splits can overflow the inline capacity and re-enter `insert`.
        if self.header().is_leaf() {
            return self.convert_inline_root(fs, range_start, range_end, handle, csum_seed);
        }

        // Convert in place (P9a-T5): scan the range for unwritten extents and
        // edit exactly their covering leaves. `write_at` calls this on EVERY
        // write, so the no-op case (a plain overwrite of written blocks) must
        // stay a bounded probe — the scan below IS that probe: a walk finding
        // nothing returns without touching a node.
        //
        // One boundary per round, each round ONE self-consistent leaf edit
        // (Linux `ext4_split_extent`'s staged splits, extents.c:3311): first a
        // kind-preserving split at the range start, then — next round — a flip
        // or a mid+tail split of the now head-free overlap. A full leaf is
        // reorganized BEFORE the edit (`make_room_for`, a clean failure point
        // that moves entries between nodes without dropping any), so an error
        // anywhere leaves a valid tree that still maps every block: the
        // conversion is cut short and the error propagates, never a journal
        // abort. Landed flips stay in the transaction; the write path's error
        // arm registers their ordered-data flush (the Unwritten-first
        // coupling) — see `write_at_once`.
        let device = fs.block_device();
        let mut edited = false;
        let mut cursor = range_start as u64;
        'scan: while cursor < range_end {
            // The first UNWRITTEN extent overlapping the remaining range.
            let mut hit: Option<Extent> = None;
            let es_cache = &self.es_cache;
            self.walk_range(fs, cursor..range_end, &mut |e| {
                if e.is_unwritten() {
                    hit = Some(*e);
                    return ControlFlow::Break(());
                }
                // R2 (es population): a WRITTEN extent this scan just visited
                // is a proven fact — record it in passing, at zero extra
                // descent, so the next convert over it hits Q2 above. Safe
                // even when a later round edits: this op only flips or splits
                // UNWRITTEN entries (and merges, which preserve per-block
                // truth), so a recorded written fact cannot be falsified
                // within this call.
                es_cache.record(e);
                ControlFlow::Continue(())
            })?;
            let Some(e) = hit else {
                break 'scan;
            };
            edited = true;
            let e_start = e.block();
            let e_end = e_start as u64 + e.len() as u64;
            let ov_start = e_start.max(range_start);
            // Lossless: `e_end` is a mapped extent's end, and the write path's
            // EFBIG gates keep every mapped block below 2^32.
            let ov_end = e_end.min(range_end) as Iblock;

            // The retry only re-lands after a leaf reorganization; the bound
            // is insert's (unreachable except on a corrupt tree).
            for _ in 0..(MAX_DEPTH as usize + 2) {
                let Search::Covered { mut path, extent } = self.find(fs, e_start)? else {
                    return_errno_with_message!(
                        Errno::EUCLEAN,
                        "walked extent vanished under the extent lock"
                    );
                };
                // The walk and this find must name the SAME extent; on a
                // duplicate-keyed or overlapping (corrupt) tree they can
                // disagree, and the edits below would overwrite an innocent
                // entry — fail loud instead.
                if extent.block() != e_start
                    || extent.len() != e.len()
                    || extent.start() != e.start()
                    || !extent.is_unwritten()
                {
                    return_errno_with_message!(
                        Errno::EUCLEAN,
                        "extent walk and path search disagree"
                    );
                }
                let leaf_level = path.levels.len() - 1;
                let pos = path.levels[leaf_level].pos;
                let leaf = &mut path.levels[leaf_level].node;

                if ov_start == e_start && ov_end as u64 == e_end {
                    // Fully covered: flip the kind in place, then coalesce with
                    // the in-leaf neighbours (a freshly written run typically
                    // continues the previously converted one; without the merge
                    // every write chunk would leave one extent behind forever —
                    // the old whole-tree rebuild merged globally).
                    let written = Extent::new(e_start, e.len(), e.start(), ExtentKind::Written);
                    leaf.replace_extent_at(pos, &written, &es);
                    merge_leaf_neighbors(leaf, pos, &es);
                    leaf.write_back(device.as_ref(), handle, csum_seed, Some(&self.node_cache))?;
                    // R3 (es population): the flip just landed — record the
                    // written fact it produced so the NEXT overwrite of these
                    // blocks hits without a walk. Recorded AFTER `write_back`
                    // returned (its node-cache store has unlocked; the two
                    // leaf locks are never held together), and the fact is the
                    // flip's own bounds, not the possibly merged leaf entry:
                    // narrower-than-the-tree is fine under per-block truth,
                    // and skipping the leaf re-read keeps population free.
                    self.es_cache.record(&written);
                    cursor = e_end;
                    continue 'scan;
                }

                // A partial cover splits the entry inside its leaf — one free
                // slot needed; reorganize and re-land when the leaf is full.
                if leaf.is_full() {
                    self.make_room_for(
                        fs,
                        e_start,
                        handle,
                        csum_seed,
                        AllocIntent::MetadataReserve,
                    )?;
                    continue;
                }

                if ov_start > e_start {
                    // Kind-preserving split at the range start: the entry keeps
                    // the unwritten head and the remainder shift-inserts behind
                    // it — both halves land in ONE leaf write, so nothing can
                    // merge them back. The entry's first key is unchanged (no
                    // ancestor correction) and the cursor stays: the next round
                    // lands on the remainder head-free. The `as u16` narrowings
                    // are lossless: each piece lies inside one extent, whose
                    // length is a u16.
                    let head_len = (ov_start - e_start) as u16;
                    let head = Extent::new(e_start, head_len, e.start(), ExtentKind::Unwritten);
                    let remainder = Extent::new(
                        ov_start,
                        (e_end - ov_start as u64) as u16,
                        e.start() + head_len as Ext4Bid,
                        ExtentKind::Unwritten,
                    );
                    leaf.replace_extent_at(pos, &head, &es);
                    leaf.insert_extent_at(pos + 1, &remainder, &es)
                        .expect("a free slot was checked above");
                    leaf.write_back(device.as_ref(), handle, csum_seed, Some(&self.node_cache))?;
                    continue 'scan;
                }

                // No head: the entry becomes the written middle (same first key
                // — no ancestor correction) and the unwritten tail
                // shift-inserts behind it, again ONE leaf write; the middle
                // then coalesces leftward like a full cover. A tail exists
                // here: head-free and tail-free is the fully-covered branch
                // above.
                let mid_len = (ov_end as u64 - ov_start as u64) as u16;
                let mid = Extent::new(ov_start, mid_len, e.start(), ExtentKind::Written);
                let tail = Extent::new(
                    ov_end,
                    (e_end - ov_end as u64) as u16,
                    e.start() + mid_len as Ext4Bid,
                    ExtentKind::Unwritten,
                );
                leaf.replace_extent_at(pos, &mid, &es);
                leaf.insert_extent_at(pos + 1, &tail, &es)
                    .expect("a free slot was checked above");
                merge_leaf_neighbors(leaf, pos, &es);
                leaf.write_back(device.as_ref(), handle, csum_seed, Some(&self.node_cache))?;
                // R3: the flipped middle's own bounds (see the full-cover arm).
                self.es_cache.record(&mid);
                cursor = ov_end as u64;
                continue 'scan;
            }
            return_errno_with_message!(Errno::EUCLEAN, "extent convert cannot make room");
        }

        // The every-write gate: an all-written range must not even dirty (a
        // plain overwrite calls this on every chunk). The scan's R2 records
        // already remembered every written extent it visited, so the next
        // convert over this range short-circuits at Q2 without the walk.
        if edited {
            self.dirty = true;
        }
        Ok(())
    }

    /// The depth-0 conversion: at most [`INLINE_MAX`] inline entries, rewritten
    /// in one in-memory pass — there is no external node, so nothing touches
    /// the journal unless an overflow grows the tree. A partial cover at each
    /// range edge adds one entry apiece, so the converted list can exceed the
    /// inline capacity by up to two; the list then rebuilds as a depth-1 tree
    /// whose fresh leaf lands before the in-memory root flips — atomic under
    /// failure.
    fn convert_inline_root(
        &mut self,
        fs: &Ext4,
        range_start: Iblock,
        range_end: u64,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
    ) -> Result<()> {
        // es: a belt-and-braces re-invalidation (reached only through
        // `convert_unwritten`, which already dropped this span — this guards
        // any future direct caller).
        let es = self
            .es_cache
            .invalidate_range(range_start as u64..range_end);
        let n = self.header().entries() as usize;
        let mut out: Vec<Extent> = Vec::with_capacity(INLINE_MAX + 2);
        let mut edited = false;
        for i in 0..n {
            let e = self.root_extent_at(i);
            let e_start = e.block();
            let e_end = e_start as u64 + e.len() as u64;
            let overlaps = e_end > range_start as u64 && (e_start as u64) < range_end;
            if !e.is_unwritten() || !overlaps {
                if !e.is_unwritten() {
                    // R2 (es population): a written entry this scan visits is
                    // a proven fact — safe to record even when another entry
                    // flips below, since this op never falsifies a WRITTEN
                    // fact (see `convert_unwritten`'s walk).
                    self.es_cache.record(&e);
                }
                out.push(e);
                continue;
            }
            edited = true;
            let ov_start = e_start.max(range_start);
            // Lossless: `e_end` is a mapped extent's end and the write path's
            // EFBIG gates keep every mapped block below 2^32.
            let ov_end = e_end.min(range_end) as Iblock;
            // The `as u16` narrowings are lossless: each piece lies inside one
            // extent, whose length is a u16.
            if ov_start > e_start {
                out.push(Extent::new(
                    e_start,
                    (ov_start - e_start) as u16,
                    e.start(),
                    ExtentKind::Unwritten,
                ));
            }
            out.push(Extent::new(
                ov_start,
                (ov_end as u64 - ov_start as u64) as u16,
                e.start() + (ov_start - e_start) as Ext4Bid,
                ExtentKind::Written,
            ));
            if (ov_end as u64) < e_end {
                out.push(Extent::new(
                    ov_end,
                    (e_end - ov_end as u64) as u16,
                    e.start() + (ov_end - e_start) as Ext4Bid,
                    ExtentKind::Unwritten,
                ));
            }
        }
        // The every-write gate: an all-written range must not even dirty.
        // The R2 records above already remembered every written entry, so
        // the next convert over them short-circuits at Q2 without this scan.
        if !edited {
            return Ok(());
        }
        // A freshly converted run coalesces with its written neighbours —
        // without this every write chunk would leave one extent behind forever.
        merge_extents(&mut out);

        if out.len() > INLINE_MAX {
            // The edge splits pushed past the inline capacity: rebuild as a
            // depth-1 tree from the converted list. The fresh leaf lands
            // BEFORE the in-memory root flips (and the root rides the inode
            // writeback), so any failure leaves the old inline root intact,
            // still mapping every block — rewriting the root first and
            // re-inserting the overflow would strand already-counted runs on
            // a failed insert.
            let delta = self.reserialize(
                fs,
                &out,
                &[],
                handle,
                csum_seed,
                AllocIntent::MetadataReserve,
            )?;
            let net_meta = delta.meta_allocated as i64 - delta.meta_freed as i64;
            self.sector_count =
                (self.sector_count as i64 + net_meta * SECTORS_PER_BLOCK as i64).max(0) as u64;
        } else {
            self.write_inline_leaf_root(&out, &es);
        }
        self.dirty = true;
        Ok(())
    }

    /// Converts the WRITTEN parts of the logical range `[iblock, iblock + len)`
    /// to unwritten (reads-as-zero), splitting any overlapping written extent so
    /// only the covered sub-range flips while its physical mapping stays put. The
    /// mirror image of [`convert_unwritten`](Self::convert_unwritten), used by
    /// `fallocate` ZERO_RANGE to make an already-written middle read back zeros
    /// without freeing or moving its blocks (Linux `ext4_zero_range`'s
    /// `CONVERT_UNWRITTEN` over the block-aligned middle). Holes and
    /// already-unwritten extents are left alone (they read zero already).
    ///
    /// Each overlapping written extent is edited in place, one boundary per
    /// round, each round ONE self-consistent leaf edit — the same staged-split
    /// discipline as `convert_unwritten`: a full leaf is reorganized FIRST
    /// (`make_room_for`, a clean failure point that edits nothing in the range),
    /// so an error mid-range leaves a valid tree that still maps every block with
    /// the conversion simply cut short. No data blocks are allocated or freed;
    /// `i_blocks` changes only by the metadata a leaf reorganization may add.
    pub(super) fn mark_range_unwritten(
        &mut self,
        fs: &Ext4,
        iblock: Iblock,
        len: u32,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
    ) -> Result<()> {
        let range_start = iblock;
        let range_end = iblock as u64 + len as u64;
        // es: written facts in this range are about to be falsified — drop
        // them on entry. No population here: the W→U direction records
        // nothing (a visited written extent may be flipped moments later).
        let es = self
            .es_cache
            .invalidate_range(range_start as u64..range_end);

        // Depth-0 (inline root as a leaf): rewrite the root in memory, delegated
        // like the punch/convert because edge splits can overflow the inline
        // capacity and re-enter `insert`.
        if self.header().is_leaf() {
            return self.mark_inline_root_unwritten(fs, range_start, range_end, handle, csum_seed);
        }

        let device = fs.block_device();
        let mut edited = false;
        let mut cursor = range_start as u64;
        'scan: while cursor < range_end {
            // The first WRITTEN extent overlapping the remaining range.
            let mut hit: Option<Extent> = None;
            self.walk_range(fs, cursor..range_end, &mut |e| {
                if !e.is_unwritten() {
                    hit = Some(*e);
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(())
            })?;
            let Some(e) = hit else {
                break 'scan;
            };
            edited = true;
            let e_start = e.block();
            let e_end = e_start as u64 + e.len() as u64;
            let ov_start = e_start.max(range_start);
            // Lossless: `e_end` is a mapped extent's end, kept below 2^32 by the
            // callers' EFBIG gates.
            let ov_end = e_end.min(range_end) as Iblock;

            for _ in 0..(MAX_DEPTH as usize + 2) {
                let Search::Covered { mut path, extent } = self.find(fs, e_start)? else {
                    return_errno_with_message!(
                        Errno::EUCLEAN,
                        "walked extent vanished under the extent lock"
                    );
                };
                if extent.block() != e_start
                    || extent.len() != e.len()
                    || extent.start() != e.start()
                    || extent.is_unwritten()
                {
                    return_errno_with_message!(
                        Errno::EUCLEAN,
                        "extent walk and path search disagree"
                    );
                }
                let leaf_level = path.levels.len() - 1;
                let pos = path.levels[leaf_level].pos;
                let leaf = &mut path.levels[leaf_level].node;

                if ov_start == e_start && ov_end as u64 == e_end {
                    // Fully covered: flip the kind. A written extent can be a full
                    // `MAX_WRITTEN_LEN` (32768) run, but an unwritten `ee_len`
                    // bias-encodes as `len + MAX_WRITTEN_LEN`, so an unwritten run
                    // must stay at or below `MAX_UNWRITTEN_LEN` (32767) or the sum
                    // wraps the 16-bit field to a bogus zero-length extent on
                    // decode — a silently dropped mapping (Linux caps identically
                    // in `ext4_ext_map_blocks`). Split an over-long run into a
                    // capped head plus a short tail; that needs one free slot, so
                    // reorganize and re-land when the leaf is full.
                    if e.len() > MAX_UNWRITTEN_LEN {
                        if leaf.is_full() {
                            self.make_room_for(
                                fs,
                                e_start,
                                handle,
                                csum_seed,
                                AllocIntent::MetadataReserve,
                            )?;
                            continue;
                        }
                        let head = Extent::new(
                            e_start,
                            MAX_UNWRITTEN_LEN,
                            e.start(),
                            ExtentKind::Unwritten,
                        );
                        let tail = Extent::new(
                            e_start + MAX_UNWRITTEN_LEN as Iblock,
                            e.len() - MAX_UNWRITTEN_LEN,
                            e.start() + MAX_UNWRITTEN_LEN as Ext4Bid,
                            ExtentKind::Unwritten,
                        );
                        leaf.replace_extent_at(pos, &head, &es);
                        leaf.insert_extent_at(pos + 1, &tail, &es)
                            .expect("a free slot was checked above");
                        // Coalesce the head leftward only; `can_merge` caps
                        // unwritten runs at `MAX_UNWRITTEN_LEN`, so the head and
                        // its own tail never re-merge into the wrapping length.
                        merge_leaf_neighbors(leaf, pos, &es);
                        leaf.write_back(
                            device.as_ref(),
                            handle,
                            csum_seed,
                            Some(&self.node_cache),
                        )?;
                        cursor = e_end;
                        continue 'scan;
                    }
                    // Coalesce with in-leaf neighbours (a freshly zeroed run
                    // continues the previous unwritten one).
                    let unwritten = Extent::new(e_start, e.len(), e.start(), ExtentKind::Unwritten);
                    leaf.replace_extent_at(pos, &unwritten, &es);
                    merge_leaf_neighbors(leaf, pos, &es);
                    leaf.write_back(device.as_ref(), handle, csum_seed, Some(&self.node_cache))?;
                    cursor = e_end;
                    continue 'scan;
                }

                // A partial cover splits the entry inside its leaf — one free
                // slot needed; reorganize and re-land when the leaf is full.
                if leaf.is_full() {
                    self.make_room_for(
                        fs,
                        e_start,
                        handle,
                        csum_seed,
                        AllocIntent::MetadataReserve,
                    )?;
                    continue;
                }

                if ov_start > e_start {
                    // Kind-preserving split at the range start: the entry keeps
                    // its written head and the remainder shift-inserts behind it,
                    // both in ONE leaf write; the cursor stays so the next round
                    // lands on the remainder head-free.
                    let head_len = (ov_start - e_start) as u16;
                    let head = Extent::new(e_start, head_len, e.start(), ExtentKind::Written);
                    let remainder = Extent::new(
                        ov_start,
                        (e_end - ov_start as u64) as u16,
                        e.start() + head_len as Ext4Bid,
                        ExtentKind::Written,
                    );
                    leaf.replace_extent_at(pos, &head, &es);
                    leaf.insert_extent_at(pos + 1, &remainder, &es)
                        .expect("a free slot was checked above");
                    leaf.write_back(device.as_ref(), handle, csum_seed, Some(&self.node_cache))?;
                    continue 'scan;
                }

                // No head: the entry becomes the unwritten middle (same first
                // key) and the written tail shift-inserts behind it, ONE leaf
                // write; the middle then coalesces leftward like a full cover.
                let mid_len = (ov_end as u64 - ov_start as u64) as u16;
                let mid = Extent::new(ov_start, mid_len, e.start(), ExtentKind::Unwritten);
                let tail = Extent::new(
                    ov_end,
                    (e_end - ov_end as u64) as u16,
                    e.start() + mid_len as Ext4Bid,
                    ExtentKind::Written,
                );
                leaf.replace_extent_at(pos, &mid, &es);
                leaf.insert_extent_at(pos + 1, &tail, &es)
                    .expect("a free slot was checked above");
                merge_leaf_neighbors(leaf, pos, &es);
                leaf.write_back(device.as_ref(), handle, csum_seed, Some(&self.node_cache))?;
                cursor = ov_end as u64;
                continue 'scan;
            }
            return_errno_with_message!(Errno::EUCLEAN, "extent zero-convert cannot make room");
        }

        if edited {
            self.dirty = true;
        }
        Ok(())
    }

    /// The depth-0 counterpart of [`mark_range_unwritten`](Self::mark_range_unwritten):
    /// rewrites the inline root, flipping the written sub-range to unwritten. A
    /// partial cover at each range edge adds one entry apiece, so the list can
    /// exceed the inline capacity by up to two and then rebuilds as a depth-1
    /// tree (atomic under failure — the fresh leaf lands before the root flips).
    fn mark_inline_root_unwritten(
        &mut self,
        fs: &Ext4,
        range_start: Iblock,
        range_end: u64,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
    ) -> Result<()> {
        // es: a belt-and-braces re-invalidation (reached only through
        // `mark_range_unwritten`, which already dropped this span); no
        // population (W→U).
        let es = self
            .es_cache
            .invalidate_range(range_start as u64..range_end);
        let n = self.header().entries() as usize;
        let mut out: Vec<Extent> = Vec::with_capacity(INLINE_MAX + 2);
        let mut edited = false;
        for i in 0..n {
            let e = self.root_extent_at(i);
            let e_start = e.block();
            let e_end = e_start as u64 + e.len() as u64;
            let overlaps = e_end > range_start as u64 && (e_start as u64) < range_end;
            if e.is_unwritten() || !overlaps {
                out.push(e);
                continue;
            }
            edited = true;
            let ov_start = e_start.max(range_start);
            let ov_end = e_end.min(range_end) as Iblock;
            if ov_start > e_start {
                out.push(Extent::new(
                    e_start,
                    (ov_start - e_start) as u16,
                    e.start(),
                    ExtentKind::Written,
                ));
            }
            // Emit the covered middle as unwritten, splitting it into runs of at
            // most `MAX_UNWRITTEN_LEN`: a full `MAX_WRITTEN_LEN` (32768) written
            // extent would otherwise bias-encode to a wrapped zero-length extent
            // (see `mark_range_unwritten` / `can_merge`).
            let mid_phys_base = e.start() + (ov_start - e_start) as Ext4Bid;
            let mut mid = ov_start;
            while (mid as u64) < ov_end as u64 {
                let run = ((ov_end as u64 - mid as u64).min(MAX_UNWRITTEN_LEN as u64)) as u16;
                out.push(Extent::new(
                    mid,
                    run,
                    mid_phys_base + (mid - ov_start) as Ext4Bid,
                    ExtentKind::Unwritten,
                ));
                mid += run as Iblock;
            }
            if (ov_end as u64) < e_end {
                out.push(Extent::new(
                    ov_end,
                    (e_end - ov_end as u64) as u16,
                    e.start() + (ov_end - e_start) as Ext4Bid,
                    ExtentKind::Written,
                ));
            }
        }
        if !edited {
            return Ok(());
        }
        merge_extents(&mut out);
        if out.len() > INLINE_MAX {
            let delta = self.reserialize(
                fs,
                &out,
                &[],
                handle,
                csum_seed,
                AllocIntent::MetadataReserve,
            )?;
            let net_meta = delta.meta_allocated as i64 - delta.meta_freed as i64;
            self.sector_count =
                (self.sector_count as i64 + net_meta * SECTORS_PER_BLOCK as i64).max(0) as u64;
        } else {
            self.write_inline_leaf_root(&out, &es);
        }
        self.dirty = true;
        Ok(())
    }

    /// Removes the block-aligned logical range `[punch_start, punch_stop)` and
    /// shifts every later extent LEFT by `punch_stop - punch_start` blocks
    /// (`fallocate` COLLAPSE_RANGE — Linux `ext4_collapse_range` /
    /// `ext4_ext_shift_extents(SHIFT_LEFT)`), returning the freed physical data
    /// blocks so the caller can drop them through the revoke/pin funnel.
    ///
    /// Whole-tree, single-transaction rebuild (the caller's atomic contract):
    /// [`flatten`](Self::flatten) reads the tree, the range is removed and the
    /// tail re-keyed IN MEMORY, and [`reserialize`](Self::reserialize) writes the
    /// new tree — fresh nodes before the in-memory root flips, so a failure
    /// leaves the old tree intact (the caller aborts the journal to discard any
    /// reused-node overwrites). The caller's credit gate keeps the rebuild inside
    /// one transaction (a genuinely huge tree is rejected before this runs), so
    /// the crash intermediate state is always all-or-nothing — never a
    /// half-shifted tree.
    ///
    /// `i_blocks` drops by the freed data blocks plus the net metadata delta; the
    /// physical blocks of the surviving extents never move (only their logical
    /// keys shift), so no data relocation or extra allocation is needed.
    ///
    /// Split into a read-only [`plan_collapse_range`](Self::plan_collapse_range)
    /// and a journaled [`apply_collapse_range`](Self::apply_collapse_range) so a
    /// caller runs the plan BEFORE opening a transaction — a malformed-tree read
    /// error then surfaces cleanly instead of aborting the journal. This combined
    /// form is the test convenience; production callers stage plan → `begin_op` →
    /// apply.
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn collapse_range(
        &mut self,
        fs: &Ext4,
        punch_start: Iblock,
        punch_stop: Iblock,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
        data_policy: journal::DataForgetPolicy,
    ) -> Result<()> {
        // es: the left shift falsifies every fact at/past `punch_start` — one
        // whole-tail drop (the apply's `reserialize` clears the rest anyway).
        self.es_cache.invalidate_range(punch_start as u64..u64::MAX);
        let plan = self.plan_collapse_range(fs, punch_start, punch_stop)?;
        self.apply_collapse_range(fs, plan, handle, csum_seed, data_policy)
    }

    /// Plans a COLLAPSE_RANGE with NO journal handle: reads the tree
    /// ([`flatten`](Self::flatten)), computes the post-shift survivors and the
    /// removed window's freed physical runs IN MEMORY, and returns them. Purely a
    /// read — a malformed-tree error surfaces here, before the caller opens a
    /// transaction, so a benign read failure never has to abort the journal.
    pub(super) fn plan_collapse_range(
        &self,
        fs: &Ext4,
        punch_start: Iblock,
        punch_stop: Iblock,
    ) -> Result<CollapsePlan> {
        debug_assert!(punch_start < punch_stop);
        let shift = punch_stop - punch_start;
        let (extents, old_external) = self.flatten(fs)?;

        let mut survivors: Vec<Extent> = Vec::with_capacity(extents.len());
        // The freed physical runs of the removed window, dropped after the tree
        // is rewritten to no longer reference them.
        let mut freed: Vec<(Ext4Bid, u32)> = Vec::new();
        for e in &extents {
            let e_start = e.block();
            let e_end = e_start as u64 + e.len() as u64;
            // Entirely before the removed window: unchanged.
            if e_end <= punch_start as u64 {
                survivors.push(*e);
                continue;
            }
            // Entirely at/after the removed window: shift left.
            if e_start >= punch_stop {
                survivors.push(Extent::new(e_start - shift, e.len(), e.start(), e.kind()));
                continue;
            }
            // Straddles the window: keep the head before `punch_start`, free the
            // covered middle, shift the tail at/after `punch_stop` left.
            if e_start < punch_start {
                let head_len = (punch_start - e_start) as u16;
                survivors.push(Extent::new(e_start, head_len, e.start(), e.kind()));
            }
            let mid_start = e_start.max(punch_start);
            let mid_end = (e_end.min(punch_stop as u64)) as Iblock;
            if mid_end > mid_start {
                freed.push((
                    e.start() + (mid_start - e_start) as Ext4Bid,
                    mid_end - mid_start,
                ));
            }
            if e_end > punch_stop as u64 {
                let tail_start = punch_stop; // e_start < punch_stop here
                let tail_len = (e_end - tail_start as u64) as u16;
                survivors.push(Extent::new(
                    tail_start - shift,
                    tail_len,
                    e.start() + (tail_start - e_start) as Ext4Bid,
                    e.kind(),
                ));
            }
        }
        // Coalesce the boundary: the shifted tail may now abut the kept head of
        // an earlier extent physically and logically.
        merge_extents(&mut survivors);

        Ok(CollapsePlan {
            survivors,
            freed,
            old_external,
        })
    }

    /// Applies a planned COLLAPSE_RANGE under `handle`: frees the removed window's
    /// data blocks and rewrites the tree, all journaled in one transaction. Any
    /// error here means captured journaled writes are in flight, so the caller
    /// aborts. The plan was computed from the same tree the caller has held frozen
    /// (`inner` write lock), so its `old_external` node list is still exact.
    pub(super) fn apply_collapse_range(
        &mut self,
        fs: &Ext4,
        plan: CollapsePlan,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
        data_policy: journal::DataForgetPolicy,
    ) -> Result<()> {
        // es: the plan carries no `punch_start`, and this apply is a
        // whole-tree rebuild whose `reserialize` clears the cache anyway —
        // clear it all at entry (a superset of the shift's `[punch_start,
        // MAX)` span; over-invalidation is the safe direction).
        self.es_cache.clear_all();
        let CollapsePlan {
            survivors,
            freed,
            old_external,
        } = plan;

        // Free the removed window's data blocks (revoke/pin per policy) BEFORE
        // the rebuild reuses metadata nodes; the pins keep a freed block from
        // being grabbed as a fresh metadata node this same transaction.
        let mut freed_data: u64 = 0;
        for &(pblock, count) in &freed {
            fs.free_blocks(data_policy.authorize(pblock, count), handle)?;
            freed_data += count as u64;
        }

        let delta = self.reserialize(
            fs,
            &survivors,
            &old_external,
            handle,
            csum_seed,
            AllocIntent::Normal,
        )?;
        let net_meta = delta.meta_allocated as i64 - delta.meta_freed as i64;
        let removed = freed_data as i64 - net_meta;
        self.sector_count =
            (self.sector_count as i64 - removed * SECTORS_PER_BLOCK as i64).max(0) as u64;
        self.dirty = true;
        Ok(())
    }

    /// Shifts every extent at/after `offset` RIGHT by `len` blocks, opening a
    /// hole `[offset, offset + len)` (`fallocate` INSERT_RANGE — Linux
    /// `ext4_insert_range` / `ext4_ext_shift_extents(SHIFT_RIGHT)`). An extent
    /// straddling `offset` is split so its head stays and its tail shifts.
    ///
    /// Same whole-tree, single-transaction atomic rebuild as
    /// [`collapse_range`](Self::collapse_range) (see that method's crash
    /// contract): no data block is allocated or freed — only the logical keys
    /// move and one straddling extent may split — so `i_blocks` changes solely by
    /// the net metadata delta. The caller grows `i_size` in the SAME transaction.
    ///
    /// Split into read-only [`plan_insert_range`](Self::plan_insert_range) and
    /// journaled [`apply_insert_range`](Self::apply_insert_range) for the same
    /// reason as [`collapse_range`](Self::collapse_range): the SHIFT_RIGHT
    /// overflow `EINVAL` (a benign user error) is decided in the plan, before the
    /// caller opens a transaction, so it never aborts the journal. This combined
    /// form is the test convenience.
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) fn insert_range(
        &mut self,
        fs: &Ext4,
        offset: Iblock,
        len: Iblock,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
    ) -> Result<()> {
        // es: the right shift falsifies every fact at/past `offset`.
        self.es_cache.invalidate_range(offset as u64..u64::MAX);
        let plan = self.plan_insert_range(fs, offset, len)?;
        self.apply_insert_range(fs, plan, handle, csum_seed)
    }

    /// Plans an INSERT_RANGE with NO journal handle: reads the tree, validates the
    /// SHIFT_RIGHT overflow bound, and computes the right-shifted extents IN
    /// MEMORY. The overflow `EINVAL` and any malformed-tree read error surface
    /// here — before the caller opens a transaction — so a benign rejection leaves
    /// the journal untouched.
    pub(super) fn plan_insert_range(
        &self,
        fs: &Ext4,
        offset: Iblock,
        len: Iblock,
    ) -> Result<InsertPlan> {
        debug_assert!(len > 0);
        let (extents, old_external) = self.flatten(fs)?;

        // The shifted last extent must stay inside the 32-bit logical space
        // (Linux `ext4_ext_shift_extents`: `shift > EXT_MAX_BLOCKS - (stop +
        // len)` is `EINVAL`). A `fallocate` KEEP_SIZE preallocation can park
        // extents far past `i_size`, so the caller's size gate alone does not
        // bound this — and without it the shifted-key arithmetic below would
        // wrap. Checked BEFORE any mutation (the "nothing changed" contract).
        if let Some(last) = extents.last()
            && last.block() as u64 + last.len() as u64 + len as u64 > u32::MAX as u64
        {
            return_errno_with_message!(
                Errno::EINVAL,
                "insert range would shift extents past the maximum logical block"
            );
        }

        let mut shifted: Vec<Extent> = Vec::with_capacity(extents.len() + 1);
        for e in &extents {
            let e_start = e.block();
            let e_end = e_start as u64 + e.len() as u64;
            // Entirely before the insert point: unchanged.
            if e_end <= offset as u64 {
                shifted.push(*e);
                continue;
            }
            // Entirely at/after: shift right.
            if e_start >= offset {
                shifted.push(Extent::new(e_start + len, e.len(), e.start(), e.kind()));
                continue;
            }
            // Straddles `offset`: head stays, tail shifts right past the new hole.
            let head_len = (offset - e_start) as u16;
            shifted.push(Extent::new(e_start, head_len, e.start(), e.kind()));
            let tail_len = (e_end - offset as u64) as u16;
            shifted.push(Extent::new(
                offset + len,
                tail_len,
                e.start() + head_len as Ext4Bid,
                e.kind(),
            ));
        }

        Ok(InsertPlan {
            shifted,
            old_external,
        })
    }

    /// Applies a planned INSERT_RANGE under `handle`: rewrites the tree with the
    /// shifted extents, journaled in one transaction. Any error here has captured
    /// journaled writes in flight, so the caller aborts.
    pub(super) fn apply_insert_range(
        &mut self,
        fs: &Ext4,
        plan: InsertPlan,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
    ) -> Result<()> {
        // es: clear whole at entry, like `apply_collapse_range` (the plan
        // carries no `offset`; the rebuild's `reserialize` clears anyway).
        self.es_cache.clear_all();
        let InsertPlan {
            shifted,
            old_external,
        } = plan;
        let delta = self.reserialize(
            fs,
            &shifted,
            &old_external,
            handle,
            csum_seed,
            AllocIntent::Normal,
        )?;
        let net_meta = delta.meta_allocated as i64 - delta.meta_freed as i64;
        self.sector_count =
            (self.sector_count as i64 + net_meta * SECTORS_PER_BLOCK as i64).max(0) as u64;
        self.dirty = true;
        Ok(())
    }

    /// Frees every data block and extent-tree metadata block mapping a logical
    /// region at or beyond `new_size` bytes, rewriting the tree and updating
    /// `i_blocks`, in ONE transaction (the single-transaction truncate path).
    ///
    /// A thin whole-tree wrapper over [`truncate_chunk`](Self::truncate_chunk)
    /// with no credit bound: it frees every doomed extent in place in one
    /// pass. Used by `rollback_write` and the `Inode::resize` fast path,
    /// whose gate already proved the whole truncate fits one transaction.
    pub(super) fn truncate_to_byte_len(
        &mut self,
        fs: &Ext4,
        new_size: usize,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
        data_policy: journal::DataForgetPolicy,
    ) -> Result<()> {
        self.truncate_chunk(fs, new_size, handle, csum_seed, data_policy, None)?;
        Ok(())
    }

    /// One credit-bounded step of shrinking the tree to `new_size` bytes: walks
    /// the right spine, frees the doomed tail extents IN PLACE (P9a-T4a — the
    /// covering leaf entry is trimmed or removed, an emptied leaf and its
    /// emptied ancestors are pruned), and returns the frontier `reached` the
    /// survivor still references.
    ///
    /// `max_credits` is `None` for the whole-tree (single-transaction) path — it
    /// frees every doomed extent in one transaction and returns `keep_blocks`.
    /// `Some(max)` is the chunked, orphan-protected spine: each free is preceded
    /// by a wait-free credit probe reserving one free plus the O(depth) node
    /// write-backs it can trigger, and on [`ExtendOutcome::NeedsRestart`] the
    /// walk STOPS with the progress made so far. The stop returns a `reached`
    /// above `keep_blocks`; the OUTER spine `journal_restart`s (never under this
    /// ③ lock — iron law 1) and calls again, re-walking the right spine of the
    /// smaller tree.
    ///
    /// Per-chunk (not per-truncate) crash red-line: the frees and the in-place
    /// tree edits that drop exactly those extents ride ONE transaction, so a
    /// committed chunk never leaves the tree pointing at a freed (reallocatable)
    /// block (double-alloc) or a freed block the tree still names (leak). Any
    /// failure after the first free aborts the journal (the frees are captured
    /// and irreversible); the freed run's pins (`free_blocks`) release at commit,
    /// after the edit dropped it — no freed-and-reallocated window.
    ///
    /// `data_policy` is the owning inode's revoke rule for its DATA blocks
    /// (Linux `get_default_free_blocks_flags`): directory blocks and
    /// slow-symlink targets are forgotten (revoked) before their free,
    /// regular-file data is not. The tree's own external nodes are always
    /// forgotten ([`free_meta_block`]), independent of the policy.
    pub(super) fn truncate_chunk(
        &mut self,
        fs: &Ext4,
        new_size: usize,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
        data_policy: journal::DataForgetPolicy,
        max_credits: Option<usize>,
    ) -> Result<super::TruncateChunk> {
        // Lossless: callers bound `new_size` by `ensure_size_within_limit` /
        // `max_file_size` (≤ `u32::MAX` logical blocks — see `fs.rs`).
        let keep_blocks = new_size.div_ceil(BLOCK_SIZE) as Iblock;
        // es: everything at/past the keep boundary is (about to be) unmapped;
        // one conservative whole-tail drop also covers a chunked stop's
        // partial progress. This mint serves `truncate_to_byte_len` too (its
        // only work happens here).
        let es = self.es_cache.invalidate_range(keep_blocks as u64..u64::MAX);
        let depth = self.header().depth();
        let free_cost = fs.extent_free_credits();
        // The follow-up credits ONE probed data free may consume, sized to the
        // tree DEPTH (not its size): freeing the extent can empty the rightmost
        // leaf and cascade a prune up to the root, freeing ≤ `depth` metadata
        // blocks (the leaf plus up to `depth - 1` interior parents, each a
        // `free_cost` bitmap/GDT/revoke), then writing back one surviving
        // parent (1 capture) — or, on the terminal step, the boundary leaf (1
        // capture) — and the inode descriptor the chunk transaction carries (1).
        // `free_cost * depth + 2` bounds all of it. This O(depth) headroom is
        // what retires the whole-tree `truncate-EFBIG-floor` debt: a real
        // journal (thousands of credits) always clears `free_cost + this`.
        let node_headroom = free_cost * depth as usize + 2;

        // Depth-0 (inline root as a leaf): at most `INLINE_MAX` extents, so the
        // whole truncate fits any transaction — free the doomed tail and
        // rewrite the inline root in one step, no chunking, no pruning.
        if depth == 0 {
            let mut kept: Vec<Extent> = Vec::with_capacity(INLINE_MAX);
            let mut freed_data: u64 = 0;
            // The free loop runs under the spine's abort discipline: once the
            // first free captures into this transaction, a later failure would
            // commit freed bitmap bits while the (not yet rewritten) root
            // still maps them — freed-but-mapped. Abort instead, like the
            // depth ≥ 1 spine below.
            let frees = (|| -> Result<()> {
                let n = self.header().entries() as usize;
                for i in 0..n {
                    let e = self.root_extent_at(i);
                    let e_end = e.block() as u64 + e.len() as u64;
                    if e_end <= keep_blocks as u64 {
                        kept.push(e);
                    } else if e.block() < keep_blocks {
                        let head_len = (keep_blocks - e.block()) as u16;
                        let auth = data_policy.authorize(
                            e.start() + head_len as Ext4Bid,
                            e.len() as u32 - head_len as u32,
                        );
                        fs.free_blocks(auth, handle)?;
                        freed_data += (e.len() - head_len) as u64;
                        kept.push(Extent::new(e.block(), head_len, e.start(), e.kind()));
                    } else {
                        let auth = data_policy.authorize(e.start(), e.len() as u32);
                        fs.free_blocks(auth, handle)?;
                        freed_data += e.len() as u64;
                    }
                }
                Ok(())
            })();
            if let Err(err) = frees {
                if freed_data > 0
                    && let Some(h) = handle
                {
                    h.abort_journal_on_fs_error();
                }
                return Err(err);
            }
            self.write_inline_leaf_root(&kept, &es);
            let removed = freed_data as i64 * SECTORS_PER_BLOCK as i64;
            debug_assert!(self.sector_count as i64 >= removed);
            self.sector_count = (self.sector_count as i64 - removed).max(0) as u64;
            self.dirty = true;
            let reached = kept
                .iter()
                .map(|e| e.block() + e.len() as Iblock)
                .max()
                .unwrap_or(0);
            return Ok(super::TruncateChunk {
                reached,
                next_bound: free_cost + node_headroom,
            });
        }

        // The honest EFBIG floor: one free plus the pruning/write-back it can
        // trigger must fit a whole transaction, or no restart ever can. O(depth)
        // now — a real journal always clears it (this is the debt's retirement).
        // Only a real free obligation (a rightmost extent above `keep_blocks`)
        // trips it.
        let device = fs.block_device();
        let rightmost_end = {
            let p = self
                .rightmost_path(fs)?
                .expect("depth ≥ 1 has an external leaf");
            let leaf = &p.leaf().expect("rightmost path ends in a leaf").node;
            let n = leaf.entries();
            // A well-formed non-root leaf is non-empty; guard anyway.
            if n == 0 {
                0
            } else {
                let e = leaf.extent_at(n - 1);
                e.block() as u64 + e.len() as u64
            }
        };
        let has_work = rightmost_end > keep_blocks as u64;
        if let Some(max) = max_credits
            && has_work
            && free_cost + node_headroom > max
        {
            return_errno_with_message!(
                Errno::EFBIG,
                "one truncate chunk's free plus node writes exceeds a journal transaction"
            );
        }

        let mut freed_data: u64 = 0;
        let mut freed_meta: u64 = 0;

        // Walk the right spine, freeing doomed tail extents in place. When the
        // rightmost leaf empties, prune it and re-walk to the previous sibling.
        // The loop is bounded: every iteration either frees ≥ 1 extent, prunes
        // ≥ 1 node, hits the survivor boundary, or credit-stops.
        //
        // The whole spine runs inside an abort guard: once the first free
        // captures into this transaction the chunk is irreversible, so any
        // later failure — a node `write_back` (device EIO / OOM), a prune, or
        // an ancestor-key correction — must abort the journal, or the freed
        // data would commit while the tree still maps it (double allocation).
        // The frees themselves self-abort; this catches the metadata writes
        // that follow them. (A non-journaled volume has no abort; the failure
        // leaves the same torn exposure every multi-block mutation has there.)
        let spine = (|| -> Result<Iblock> {
            let mut reached = keep_blocks;
            'chunk: loop {
                let mut path = self
                    .rightmost_path(fs)?
                    .expect("depth ≥ 1 keeps an external leaf until the root resets");
                let leaf_level = path.levels.len() - 1;
                let mut leaf_edited = false;

                // Free doomed entries from this leaf's tail. `Break::Emptied`
                // falls to the prune below; `Break::Done` ends the chunk (the
                // boundary was reached or the credit probe stopped).
                enum Break {
                    Emptied,
                    Done,
                }
                let outcome = loop {
                    let leaf = &path.levels[leaf_level].node;
                    let n = leaf.entries();
                    if n == 0 {
                        break Break::Emptied;
                    }
                    let last = leaf.extent_at(n - 1);
                    let last_end = last.block() as u64 + last.len() as u64;
                    if last_end <= keep_blocks as u64 {
                        // Survivor boundary: this leaf's tail is entirely kept.
                        reached = reached.max(last_end as Iblock);
                        break Break::Done;
                    }
                    // A free is due; probe first (chunked mode).
                    if let Some(h) = handle
                        && max_credits.is_some()
                        && stop_before_free(h, free_cost + node_headroom)?
                    {
                        reached = reached.max(last_end as Iblock);
                        break Break::Done;
                    }
                    if last.block() < keep_blocks {
                        // Straddler: keep the head, free the tail, done.
                        let head_len = (keep_blocks - last.block()) as u16;
                        let tail_len = last.len() - head_len;
                        let auth = data_policy
                            .authorize(last.start() + head_len as Ext4Bid, tail_len as u32);
                        fs.free_blocks(auth, handle)?;
                        freed_data += tail_len as u64;
                        let head = Extent::new(last.block(), head_len, last.start(), last.kind());
                        path.levels[leaf_level]
                            .node
                            .replace_extent_at(n - 1, &head, &es);
                        leaf_edited = true;
                        reached = reached.max(keep_blocks);
                        break Break::Done;
                    }
                    // Fully doomed: free its data and drop it from the leaf.
                    let auth = data_policy.authorize(last.start(), last.len() as u32);
                    fs.free_blocks(auth, handle)?;
                    freed_data += last.len() as u64;
                    path.levels[leaf_level].node.remove_extent_at(n - 1, &es);
                    leaf_edited = true;
                };

                match outcome {
                    Break::Done => {
                        if leaf_edited {
                            path.levels[leaf_level].node.write_back(
                                device.as_ref(),
                                handle,
                                csum_seed,
                                Some(&self.node_cache),
                            )?;
                        }
                        break 'chunk Ok(reached);
                    }
                    Break::Emptied => {
                        // Prune the emptied leaf and every ancestor it empties,
                        // writing each shrunk parent back BEFORE the next
                        // re-walk reads it. A truncate to zero resets the root.
                        freed_meta +=
                            self.prune_emptied_leaf(fs, &mut path, handle, csum_seed, &es)?;
                        if self.header().is_leaf() {
                            break 'chunk Ok(0);
                        }
                        // Re-walk to the new rightmost leaf and continue.
                    }
                }
            }
        })();
        let reached = match spine {
            Ok(reached) => reached,
            Err(err) => {
                if let Some(h) = handle {
                    h.abort_journal_on_fs_error();
                }
                return Err(err);
            }
        };

        let net_removed = freed_data + freed_meta;
        let removed_sectors = net_removed as i64 * SECTORS_PER_BLOCK as i64;
        debug_assert!(self.sector_count as i64 >= removed_sectors);
        self.sector_count = (self.sector_count as i64 - removed_sectors).max(0) as u64;
        self.dirty = true;

        Ok(super::TruncateChunk {
            reached,
            next_bound: free_cost + node_headroom,
        })
    }

    /// Frees the (emptied) leaf `path` ends in and prunes every ancestor its
    /// removal empties, returning the count of metadata blocks freed. Each
    /// level removes ITS path position (the truncate spine's rightmost walk
    /// puts every position at the last entry; a punch can empty any child).
    /// Removing a parent's position 0 raises that parent's first key, so the
    /// grandparents' keys are corrected to keep Linux's exact-first-key
    /// invariant. Each shrunk parent is written back through the journal
    /// funnel BEFORE returning, so a re-walk reads the pruned tree. When the
    /// inline root's last index entry goes, the root resets to an empty
    /// depth-0 leaf (in-memory, riding the inode writeback). Mirrors Linux
    /// `ext4_ext_rm_idx` cascading to `ext4_ext_remove_space`'s root collapse.
    fn prune_emptied_leaf(
        &mut self,
        fs: &Ext4,
        path: &mut ExtentPath,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
        es: &EsInvalidated,
    ) -> Result<u64> {
        let device = fs.block_device();
        let mut freed_meta = 0u64;

        // Free the emptied leaf, then walk up removing each child's index entry;
        // stop at the first parent that stays non-empty.
        let leaf_bid = path.levels[path.levels.len() - 1].node.bid();
        free_meta_block(fs, leaf_bid, handle, &self.node_cache)?;
        freed_meta += 1;

        let mut child_level = path.levels.len() - 1;
        loop {
            if child_level == 0 {
                // The removed node's parent is the inline root.
                let root_entries = self.header().entries() as usize;
                if root_entries <= 1 {
                    // Last child gone: the tree is now empty.
                    self.write_inline_leaf_root(&[], es);
                } else {
                    self.remove_root_index_at(path.root_pos);
                }
                self.dirty = true;
                break;
            }
            let parent_level = child_level - 1;
            let remove_at = path.levels[parent_level].pos;
            let pn = path.levels[parent_level].node.entries();
            if pn <= 1 {
                // Parent empties too: free it and cascade to ITS parent.
                let parent_bid = path.levels[parent_level].node.bid();
                free_meta_block(fs, parent_bid, handle, &self.node_cache)?;
                freed_meta += 1;
                child_level = parent_level;
                continue;
            }
            path.levels[parent_level].node.remove_index_at(remove_at);
            path.levels[parent_level].node.write_back(
                device.as_ref(),
                handle,
                csum_seed,
                Some(&self.node_cache),
            )?;
            // Removing the parent's FIRST child raised its first key; the
            // ancestors' keys must follow exactly (Linux read-side equality).
            if remove_at == 0 {
                let new_key = path.levels[parent_level].node.first_key();
                self.correct_ancestor_keys(fs, path, parent_level, new_key, handle, csum_seed)?;
            }
            break;
        }
        Ok(freed_meta)
    }

    /// Removes root index entry `i`, shifting later entries left (in-memory,
    /// like every root rewrite; it rides the inode writeback).
    fn remove_root_index_at(&mut self, i: usize) {
        let header = self.header();
        let n = header.entries() as usize;
        debug_assert!(!header.is_leaf() && i < n);
        let bytes = self.root.as_mut_bytes();
        let start = ENTRY_SIZE * (1 + i);
        let end = ENTRY_SIZE * (1 + n);
        bytes.copy_within(start + ENTRY_SIZE..end, start);
        let mut raw = RawExtentHeader::from_bytes(&bytes[0..ENTRY_SIZE]);
        raw.entries = (n - 1) as u16;
        bytes[0..ENTRY_SIZE].copy_from_slice(raw.as_bytes());
    }

    /// One credit-bounded step of freeing the mapped blocks in the MIDDLE logical
    /// range `[start_block, end_block)` (a punch-hole), editing the covering
    /// leaves IN PLACE (P9a-T4b). Returns whether doomed blocks remain (the outer
    /// spine restarts and calls again) and the reservation the next chunk's fresh
    /// transaction should start from.
    ///
    /// Unlike [`truncate_chunk`](Self::truncate_chunk) the file size is UNCHANGED
    /// and there is no orphan protection: a crash between chunks leaves some of
    /// the range freed and some still mapped — a partial hole, which is a valid
    /// file state (reads return zero where freed, the original data where not),
    /// so no orphan link is needed to make recovery re-converge. The freed
    /// data/metadata blocks go through the SAME forget/revoke + pin funnels as
    /// truncate (`data_policy` for data, [`free_meta_block`] for tree nodes), so
    /// a replay never resurrects stale data into a reused block and a freed block
    /// is not reallocated before its freeing transaction commits.
    ///
    /// The scan runs low→high; each overlapping extent is edited in place: an
    /// edge-straddling extent is trimmed to its kept head or tail (a raised
    /// first key corrects the ancestor index keys, leaf-write-first for
    /// lower-bound safety), a fully-covered extent is removed (an emptied leaf
    /// and its emptied ancestors are pruned), and an extent spanning BOTH edges
    /// splits into head + tail — same-leaf shift-insert when there is room, else
    /// a trim plus an insert through the split machinery. `max_credits` is `None`
    /// for the whole-range path and `Some(max)` for the chunked spine — the same
    /// O(depth) per-step credit probe and EFBIG floor as truncate.
    ///
    /// Per-chunk (not per-punch) crash red-line, identical to truncate: the frees
    /// and the in-place edits that drop exactly those extents ride one
    /// transaction, and any failure after the first free aborts the journal, so
    /// a committed chunk never leaves the tree naming a freed (reallocatable)
    /// block (double-alloc) or a freed block the tree still names (leak).
    pub(super) fn punch_chunk(
        &mut self,
        fs: &Ext4,
        range: Range<Iblock>,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
        data_policy: journal::DataForgetPolicy,
        max_credits: Option<usize>,
    ) -> Result<super::PunchChunk> {
        let Range {
            start: start_block,
            end: end_block,
        } = range;
        if start_block >= end_block {
            return Ok(super::PunchChunk {
                more: false,
                next_bound: fs.extent_free_credits(),
            });
        }
        // es: the punched range's mappings are about to go; facts outside the
        // span stay (an edge split preserves the neighbouring blocks' truth).
        let es = self
            .es_cache
            .invalidate_range(start_block as u64..end_block as u64);

        let depth = self.header().depth();
        let free_cost = fs.extent_free_credits();
        // The same O(depth) follow-up bound as the truncate spine (a free may
        // empty the leaf and prune a cascade; one boundary write; the inode
        // descriptor)…
        let node_headroom = free_cost * depth as usize + 2;
        // …plus, for the one case that INSERTS (an extent spanning the whole
        // punch range splits into head + tail through the insert machinery),
        // the insert's own per-op worst case. This is `insert_credit_bound`
        // (the `3*(depth+1)+2` split shape), NOT `write_credits` (the smaller
        // `2*(depth+1)` single-map estimate): a spans-both split re-enters the
        // full leaf through `self.insert`, whose worst case is a full-path split
        // plus a depth growth. The inode descriptor rides `node_headroom`'s `+2`,
        // so it is not double-charged here.
        let split_headroom = node_headroom + fs.insert_credit_bound(depth);

        // Depth-0: at most INLINE_MAX entries — punch them in one pass over
        // the inline root (any transaction fits it; a spans-both split may
        // overflow the root and grow the tree through the insert path).
        if depth == 0 {
            return self.punch_inline_root(
                fs,
                start_block,
                end_block,
                handle,
                csum_seed,
                data_policy,
            );
        }

        let mut freed_data: u64 = 0;
        let mut freed_meta: u64 = 0;
        let mut more = false;
        // The reservation the outer spine restarts with: the common per-step
        // bound, raised to a stopped step's own need (a full-leaf spans-both
        // split) so the restarted transaction can run it.
        let mut restart_bound = free_cost + node_headroom;

        // The whole scan runs inside an abort guard, like the truncate spine:
        // once the first free captures into this transaction, any later
        // failure — a node `write_back`, an ancestor-key correction, a prune,
        // or the spans-both tail re-insert — must abort the journal, or the
        // freed data would commit while the tree still maps it.
        let device = fs.block_device();
        let scan = (|| -> Result<()> {
            // Scan low→high: find the first extent overlapping the remaining
            // range, classify, edit in place, continue. (The old spine freed
            // high→low, but every chunk re-scans the whole range, so the
            // direction is immaterial to the contract — `more` just says "call
            // again" — and a partial hole is a valid crash/restart state.)
            let mut cursor = start_block;
            'scan: while (cursor as u64) < end_block as u64 {
                let mut hit: Option<Extent> = None;
                self.walk_range(fs, cursor as u64..end_block as u64, &mut |e| {
                    hit = Some(*e);
                    ControlFlow::Break(())
                })?;
                let Some(e) = hit else {
                    break 'scan; // no mapped extent left in the range
                };
                let e_start = e.block();
                let e_end = e_start as u64 + e.len() as u64;

                let spans_both = e_start < start_block && e_end > end_block as u64;
                // A spans-both split usually fits the SAME leaf (head shrinks
                // in place, the tail shift-inserts beside it): probe the small
                // bound. Only a FULL leaf needs the insert split budget.
                let mut need = free_cost + node_headroom;
                let mut spans_slow = false;
                if spans_both {
                    let Search::Covered {
                        path: probe_path, ..
                    } = self.find(fs, e_start)?
                    else {
                        return_errno_with_message!(
                            Errno::EUCLEAN,
                            "walked extent vanished under the extent lock"
                        );
                    };
                    let leaf = &probe_path.levels[probe_path.levels.len() - 1].node;
                    if leaf.is_full() {
                        spans_slow = true;
                        need = free_cost + split_headroom;
                    }
                }
                // The honest EFBIG floor — O(depth), a real journal always
                // clears it (the whole-tree floor retired with the reserialize).
                if let Some(max) = max_credits
                    && need > max
                {
                    return_errno_with_message!(
                        Errno::EFBIG,
                        "one punch step's free plus node writes exceeds a journal transaction"
                    );
                }
                // Credit-aware early stop (chunked mode): never restart under
                // this ③ lock — report `more` and let the outer spine restart.
                if let Some(h) = handle
                    && max_credits.is_some()
                    && stop_before_free(h, need)?
                {
                    more = true;
                    restart_bound = restart_bound.max(need);
                    break 'scan;
                }

                // The covered middle to free.
                let mid_start = e_start.max(start_block);
                let mid_end = e_end.min(end_block as u64) as Iblock;
                let mid_len = (mid_end - mid_start) as u16; // inside one extent
                let mid_phys = e.start() + (mid_start - e_start) as Ext4Bid;

                // Locate the covering leaf entry for the in-place edit.
                let Search::Covered { mut path, .. } = self.find(fs, e_start)? else {
                    return_errno_with_message!(
                        Errno::EUCLEAN,
                        "walked extent vanished under the extent lock"
                    );
                };
                let leaf_level = path.levels.len() - 1;
                let pos = path.levels[leaf_level].pos;

                if spans_both {
                    let head_len = (start_block - e_start) as u16;
                    let tail_len = (e_end - end_block as u64) as u16;
                    let tail_phys = e.start() + (end_block - e_start) as Ext4Bid;
                    let head = Extent::new(e_start, head_len, e.start(), e.kind());
                    let tail = Extent::new(end_block, tail_len, tail_phys, e.kind());
                    if !spans_slow {
                        // Same-leaf fast path: shrink to the head and
                        // shift-insert the tail beside it — one write.
                        let leaf = &mut path.levels[leaf_level].node;
                        leaf.replace_extent_at(pos, &head, &es);
                        leaf.insert_extent_at(pos + 1, &tail, &es)
                            .expect("probed leaf had room for the split tail");
                        leaf.write_back(
                            device.as_ref(),
                            handle,
                            csum_seed,
                            Some(&self.node_cache),
                        )?;
                    } else {
                        // Full leaf: trim to the head in place; the tail range
                        // is then a hole and re-enters through the ordinary
                        // insert (the split machinery). All in ONE transaction;
                        // the abort guard covers a re-insert failure.
                        path.levels[leaf_level]
                            .node
                            .replace_extent_at(pos, &head, &es);
                        path.levels[leaf_level].node.write_back(
                            device.as_ref(),
                            handle,
                            csum_seed,
                            Some(&self.node_cache),
                        )?;
                        self.insert(
                            fs,
                            end_block,
                            tail_phys,
                            tail_len,
                            e.kind(),
                            handle,
                            csum_seed,
                        )?;
                        // `insert` accounted the tail as NEW data; its blocks
                        // were already counted before the split.
                        self.sector_count = self
                            .sector_count
                            .saturating_sub(tail_len as u64 * SECTORS_PER_BLOCK);
                    }
                    let auth = data_policy.authorize(mid_phys, mid_len as u32);
                    fs.free_blocks(auth, handle)?;
                    freed_data += mid_len as u64;
                    cursor = end_block;
                    continue 'scan;
                }

                // Single-sided straddles and fully-covered extents: one
                // in-place edit, no insert. Free first (probed above), then
                // edit.
                let auth = data_policy.authorize(mid_phys, mid_len as u32);
                fs.free_blocks(auth, handle)?;
                freed_data += mid_len as u64;
                if e_start < start_block {
                    // Straddles the start: keep the head (first key unchanged).
                    let head_len = (start_block - e_start) as u16;
                    let head = Extent::new(e_start, head_len, e.start(), e.kind());
                    path.levels[leaf_level]
                        .node
                        .replace_extent_at(pos, &head, &es);
                    path.levels[leaf_level].node.write_back(
                        device.as_ref(),
                        handle,
                        csum_seed,
                        Some(&self.node_cache),
                    )?;
                } else if e_end > end_block as u64 {
                    // Straddles the end: keep the tail, re-keyed to `end_block`.
                    // A position-0 edit raises the leaf's first key; write the
                    // LEAF first, THEN correct the ancestors — the risen key
                    // goes up only after the leaf actually holds it, so every
                    // intermediate state has an ancestor key that is a valid
                    // lower bound (never above the leaf's real first block).
                    let tail_len = (e_end - end_block as u64) as u16;
                    let tail_phys = e.start() + (end_block - e_start) as Ext4Bid;
                    let tail = Extent::new(end_block, tail_len, tail_phys, e.kind());
                    path.levels[leaf_level]
                        .node
                        .replace_extent_at(pos, &tail, &es);
                    path.levels[leaf_level].node.write_back(
                        device.as_ref(),
                        handle,
                        csum_seed,
                        Some(&self.node_cache),
                    )?;
                    if pos == 0 {
                        self.correct_ancestor_keys(
                            fs, &mut path, leaf_level, end_block, handle, csum_seed,
                        )?;
                    }
                } else {
                    // Fully covered: drop the entry; prune if the leaf empties,
                    // else write the leaf and (on a risen first key) correct
                    // the ancestors AFTER the leaf write (lower-bound-safe
                    // order, as above).
                    path.levels[leaf_level].node.remove_extent_at(pos, &es);
                    if path.levels[leaf_level].node.entries() == 0 {
                        freed_meta +=
                            self.prune_emptied_leaf(fs, &mut path, handle, csum_seed, &es)?;
                    } else {
                        let rose = pos == 0;
                        let new_key = path.levels[leaf_level].node.first_key();
                        path.levels[leaf_level].node.write_back(
                            device.as_ref(),
                            handle,
                            csum_seed,
                            Some(&self.node_cache),
                        )?;
                        if rose {
                            self.correct_ancestor_keys(
                                fs, &mut path, leaf_level, new_key, handle, csum_seed,
                            )?;
                        }
                    }
                }
                cursor = mid_end;
            }
            Ok(())
        })();
        if let Err(err) = scan {
            if let Some(h) = handle {
                h.abort_journal_on_fs_error();
            }
            return Err(err);
        }

        // `i_blocks` drops by the freed data plus the pruned tree nodes.
        let removed_sectors = (freed_data + freed_meta) as i64 * SECTORS_PER_BLOCK as i64;
        debug_assert!(self.sector_count as i64 >= removed_sectors);
        self.sector_count = (self.sector_count as i64 - removed_sectors).max(0) as u64;
        self.dirty = true;

        Ok(super::PunchChunk {
            more,
            next_bound: restart_bound,
        })
    }

    /// The depth-0 punch: at most [`INLINE_MAX`] inline entries, decomposed in
    /// one pass (any transaction holds it). A spans-both split can push the
    /// survivor count past the inline capacity; the list then rebuilds as a
    /// depth-1 tree whose fresh leaf lands before the in-memory root flips —
    /// atomic under failure.
    fn punch_inline_root(
        &mut self,
        fs: &Ext4,
        start_block: Iblock,
        end_block: Iblock,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
        data_policy: journal::DataForgetPolicy,
    ) -> Result<super::PunchChunk> {
        // es: belt-and-braces re-invalidation (reached only through
        // `punch_chunk`, which already dropped this span).
        let es = self
            .es_cache
            .invalidate_range(start_block as u64..end_block as u64);
        let free_cost = fs.extent_free_credits();
        let n = self.header().entries() as usize;

        // Decompose in logical order. A single extent spanning BOTH edges
        // yields head + tail (+1 survivor), so the survivor list can reach
        // `INLINE_MAX + 1`; every other case nets zero or fewer.
        let mut survivors: Vec<Extent> = Vec::with_capacity(INLINE_MAX + 1);
        let mut doomed: Vec<Extent> = Vec::new(); // freed after the root rewrite
        for i in 0..n {
            let e = self.root_extent_at(i);
            let e_start = e.block();
            let e_end = e_start as u64 + e.len() as u64;
            if e_end <= start_block as u64 || e_start as u64 >= end_block as u64 {
                survivors.push(e);
                continue;
            }
            let mid_start = e_start.max(start_block);
            let mid_end = e_end.min(end_block as u64) as Iblock;
            doomed.push(Extent::new(
                mid_start,
                (mid_end - mid_start) as u16,
                e.start() + (mid_start - e_start) as Ext4Bid,
                e.kind(),
            ));
            if e_start < start_block {
                survivors.push(Extent::new(
                    e_start,
                    (start_block - e_start) as u16,
                    e.start(),
                    e.kind(),
                ));
            }
            if e_end > end_block as u64 {
                survivors.push(Extent::new(
                    end_block,
                    (e_end - end_block as u64) as u16,
                    e.start() + (end_block - e_start) as Ext4Bid,
                    e.kind(),
                ));
            }
        }

        // At most one extent overflows the inline root (`survivors` never
        // exceeds `INLINE_MAX + 1`). Rebuild as a depth-1 tree then: the fresh
        // leaf lands BEFORE the in-memory root flips (which rides the inode
        // writeback), so any failure leaves the old inline root intact — the
        // former rewrite-then-re-insert order could strand the peeled tail,
        // still counted but mapped nowhere, on a failed insert.
        debug_assert!(survivors.len() <= INLINE_MAX + 1);
        if survivors.len() > INLINE_MAX {
            let delta =
                self.reserialize(fs, &survivors, &[], handle, csum_seed, AllocIntent::Normal)?;
            let net_meta = delta.meta_allocated as i64 - delta.meta_freed as i64;
            self.sector_count =
                (self.sector_count as i64 + net_meta * SECTORS_PER_BLOCK as i64).max(0) as u64;
        } else {
            self.write_inline_leaf_root(&survivors, &es);
        }
        self.dirty = true;

        let mut freed_data: u64 = 0;
        for d in &doomed {
            let auth = data_policy.authorize(d.start(), d.len() as u32);
            fs.free_blocks(auth, handle)?;
            freed_data += d.len() as u64;
        }
        let removed_sectors = freed_data as i64 * SECTORS_PER_BLOCK as i64;
        debug_assert!(self.sector_count as i64 >= removed_sectors);
        self.sector_count = (self.sector_count as i64 - removed_sectors).max(0) as u64;

        Ok(super::PunchChunk {
            more: false,
            next_bound: free_cost + 2,
        })
    }

    /// Parses the whole tree into a list of leaf extents, also returning the
    /// physical blocks of **every** external node — leaf blocks at depth 1, and
    /// both interior and leaf blocks at depth 2.
    ///
    /// The write/truncate/punch hot paths are all in-place surgery (P9a-T6):
    /// hole planning, the shrink gate, and the depth-0 insert rebuild walk or
    /// read the root directly, never flattening. This survives as the
    /// whole-tree reader for the RARE [`collapse_range`](Self::collapse_range) /
    /// [`insert_range`](Self::insert_range) rebuilds and as the tests'
    /// cross-verification reference; both bound the tree to one journal
    /// transaction, so it still speaks only the depth ≤ 2 shapes (the callers
    /// reject a deeper tree with `EFBIG` before it reaches here — see
    /// `Inode::collapse_range` / `Inode::insert_range`'s depth gate).
    fn flatten(&self, fs: &Ext4) -> Result<(Vec<Extent>, Vec<Ext4Bid>)> {
        let root_bytes = self.root.as_bytes();
        let header = self.header();
        let nr = header.entries() as usize;

        if header.is_leaf() {
            let mut extents = Vec::with_capacity(nr);
            for i in 0..nr {
                let off = ENTRY_SIZE * (1 + i);
                extents.push(Extent::from(&RawExtent::from_bytes(
                    &root_bytes[off..off + ENTRY_SIZE],
                )));
            }
            return Ok((extents, Vec::new()));
        }

        let depth = header.depth();
        if depth != 1 && depth != 2 {
            return_errno_with_message!(
                Errno::EUCLEAN,
                "the test-only flatten reads depth <= 2 shapes only"
            );
        }

        // The root's index entries name the immediate children: leaf blocks at
        // depth 1, interior blocks at depth 2.
        let mut child_bids = Vec::with_capacity(nr);
        for i in 0..nr {
            let off = ENTRY_SIZE * (1 + i);
            let idx = ExtentIdx::from(&RawExtentIdx::from_bytes(
                &root_bytes[off..off + ENTRY_SIZE],
            ));
            child_bids.push(idx.leaf());
        }

        let journal = fs.journal();
        let device = fs.block_device().as_ref();

        if depth == 1 {
            let mut extents = Vec::new();
            for &leaf_bid in &child_bids {
                let block = journal::read_metadata_block(journal.as_deref(), device, leaf_bid)?;
                extents.extend(parse_leaf_node(&block)?);
            }
            return Ok((extents, child_bids));
        }

        // depth == 2: root → interior nodes → leaf nodes. Collect every external
        // block (interior and leaf) into the reuse pool.
        let mut extents = Vec::new();
        let mut external = Vec::new();
        for &interior_bid in &child_bids {
            external.push(interior_bid);
            let interior = journal::read_metadata_block(journal.as_deref(), device, interior_bid)?;
            for leaf_bid in parse_interior_node(&interior)? {
                let leaf = journal::read_metadata_block(journal.as_deref(), device, leaf_bid)?;
                extents.extend(parse_leaf_node(&leaf)?);
                external.push(leaf_bid);
            }
        }
        Ok((extents, external))
    }

    /// Re-serializes `extents` into the on-disk tree, reusing the existing
    /// external metadata blocks where possible and allocating/freeing the
    /// difference.
    ///
    /// Since the P9a surgery this is the TINY-tree builder only: its callers
    /// (the depth-0 insert rebuild and the inline-overflow escapes of convert
    /// and punch) hand it at most `INLINE_MAX + 2` extents — an inline root or
    /// one depth-1 leaf — always with an empty reuse pool. Its write order is
    /// why they use it: fresh nodes land BEFORE the in-memory root flips, so
    /// any failure leaves the old tree fully intact. The larger depth-1/-2
    /// shapes below remain exercised by tests.
    fn reserialize(
        &mut self,
        fs: &Ext4,
        extents: &[Extent],
        old_external: &[Ext4Bid],
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
        intent: AllocIntent,
    ) -> Result<TreeDelta> {
        let device = fs.block_device().as_ref();

        // A whole-tree rebuild reuses the surviving external blocks in place
        // (`write_leaf_node`/`write_interior_node` overwrite them without the
        // cache-refreshing `write_back` funnel) and frees the rest. Drop every
        // cached node up front so no reused block is later read as its stale
        // pre-rebuild self; the reads after this repopulate from the new bytes.
        // The es-cache clears whole at the same choke point: the rebuild
        // rewrites the entire logical layout, so no fact may outlive it.
        self.node_cache.clear();
        let es = self.es_cache.clear_all();

        if extents.len() <= INLINE_MAX {
            self.write_inline_leaf_root(extents, &es);
            // The root no longer references any external block; free them all.
            let mut meta_freed = 0;
            for &bid in old_external {
                free_meta_block(fs, bid, handle, &self.node_cache)?;
                meta_freed += 1;
            }
            return Ok(TreeDelta {
                meta_allocated: 0,
                meta_freed,
            });
        }

        let nr_leaves = extents.len().div_ceil(LEAF_MAX);
        let goal = extents.first().map(|e| e.start()).unwrap_or(0);

        if nr_leaves <= INLINE_MAX {
            // Depth-1: the inline root indexes `nr_leaves` external leaf blocks.
            let reuse = nr_leaves.min(old_external.len());
            let (leaf_bids, newly_allocated) = acquire_meta_blocks(
                fs,
                old_external,
                nr_leaves,
                goal,
                handle,
                &self.node_cache,
                intent,
            )?;

            // Write each leaf node. On failure, roll back the freshly allocated
            // blocks (the in-memory root is not yet updated, so the old tree
            // stays referenced).
            for (chunk, &leaf_bid) in extents.chunks(LEAF_MAX).zip(leaf_bids.iter()) {
                if let Err(err) = write_leaf_node(device, leaf_bid, chunk, handle, csum_seed, &es) {
                    rollback_meta_blocks(fs, &newly_allocated, handle, &self.node_cache);
                    return Err(err);
                }
            }

            // Commit: rewrite the depth-1 index root (in memory, infallible).
            let index_entries: Vec<RawExtentIdx> = extents
                .chunks(LEAF_MAX)
                .zip(leaf_bids.iter())
                .map(|(chunk, &leaf_bid)| make_index_entry(chunk[0].block(), leaf_bid))
                .collect();
            self.write_index_root(&index_entries, 1);

            // Free surplus old external blocks the root no longer references.
            let mut meta_freed = 0;
            for &bid in &old_external[reuse..] {
                free_meta_block(fs, bid, handle, &self.node_cache)?;
                meta_freed += 1;
            }

            return Ok(TreeDelta {
                meta_allocated: newly_allocated.len() as u32,
                meta_freed,
            });
        }

        let nr_interior = nr_leaves.div_ceil(INTERIOR_MAX);
        if nr_interior > INLINE_MAX {
            // Would need a depth-3 tree; the rebuild strategy caps at depth 2.
            return_errno_with_message!(Errno::ENOSPC, "extent tree would exceed depth 2");
        }

        // Depth-2: the inline root indexes `nr_interior` interior nodes, each of
        // which indexes up to `INTERIOR_MAX` leaf blocks. Interior and leaf blocks are
        // interchangeable metadata blocks (each is fully overwritten), so one
        // reuse pool serves both; the first `nr_leaves` become leaves, the rest
        // interiors.
        let total = nr_leaves + nr_interior;
        let reuse = total.min(old_external.len());
        let (blocks, newly_allocated) = acquire_meta_blocks(
            fs,
            old_external,
            total,
            goal,
            handle,
            &self.node_cache,
            intent,
        )?;
        let (leaf_bids, interior_bids) = blocks.split_at(nr_leaves);

        // Write leaves, then interiors. On any failure, roll back the freshly
        // allocated blocks (the in-memory root is not yet updated).
        for (chunk, &leaf_bid) in extents.chunks(LEAF_MAX).zip(leaf_bids.iter()) {
            if let Err(err) = write_leaf_node(device, leaf_bid, chunk, handle, csum_seed, &es) {
                rollback_meta_blocks(fs, &newly_allocated, handle, &self.node_cache);
                return Err(err);
            }
        }

        // One leaf-index entry per leaf, keyed by that leaf's first logical block
        // (the extents are sorted, so the chunk's first entry is its minimum).
        let leaf_index: Vec<RawExtentIdx> = extents
            .chunks(LEAF_MAX)
            .zip(leaf_bids.iter())
            .map(|(chunk, &leaf_bid)| make_index_entry(chunk[0].block(), leaf_bid))
            .collect();

        for (chunk, &interior_bid) in leaf_index.chunks(INTERIOR_MAX).zip(interior_bids.iter()) {
            if let Err(err) = write_interior_node(device, interior_bid, chunk, handle, csum_seed) {
                rollback_meta_blocks(fs, &newly_allocated, handle, &self.node_cache);
                return Err(err);
            }
        }

        // Commit: rewrite the depth-2 index root, one entry per interior node
        // keyed by that interior's first logical block (in memory, infallible).
        let root_index: Vec<RawExtentIdx> = leaf_index
            .chunks(INTERIOR_MAX)
            .zip(interior_bids.iter())
            .map(|(chunk, &interior_bid)| make_index_entry(chunk[0].block, interior_bid))
            .collect();
        self.write_index_root(&root_index, 2);

        // Free surplus old external blocks the tree no longer references.
        let mut meta_freed = 0;
        for &bid in &old_external[reuse..] {
            free_meta_block(fs, bid, handle, &self.node_cache)?;
            meta_freed += 1;
        }

        Ok(TreeDelta {
            meta_allocated: newly_allocated.len() as u32,
            meta_freed,
        })
    }

    /// Rewrites the root as a depth-0 inline leaf (header + up to
    /// [`INLINE_MAX`] extents). A whole-root leaf-entry rewrite, so it demands
    /// the es-cache credential like the per-entry editors (see `path.rs`).
    fn write_inline_leaf_root(&mut self, extents: &[Extent], _es: &EsInvalidated) {
        let bytes = self.root.as_mut_bytes();
        bytes.fill(0);
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: extents.len() as u16,
            max: INLINE_MAX as u16,
            depth: 0,
            generation: 0,
        };
        bytes[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        for (i, ext) in extents.iter().enumerate() {
            let off = ENTRY_SIZE * (1 + i);
            bytes[off..off + ENTRY_SIZE].copy_from_slice(RawExtent::from(ext).as_bytes());
        }
    }

    /// Rewrites the root as an index node at `depth` (1 or 2): a header plus one
    /// index entry per child (an external leaf at depth 1, an interior node at
    /// depth 2). The inline root's capacity is [`INLINE_MAX`] at either depth.
    fn write_index_root(&mut self, entries: &[RawExtentIdx], depth: u16) {
        let bytes = self.root.as_mut_bytes();
        bytes.fill(0);
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: entries.len() as u16,
            max: INLINE_MAX as u16,
            depth,
            generation: 0,
        };
        bytes[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        for (i, idx) in entries.iter().enumerate() {
            let off = ENTRY_SIZE * (1 + i);
            bytes[off..off + ENTRY_SIZE].copy_from_slice(idx.as_bytes());
        }
    }

    /// Counts, in one structural descent of the whole tree, the two shape
    /// inputs the whole-truncate routing gate ([`ExtentManager::plan_shrink`])
    /// reserves against: the real external-node count and how many extents end
    /// past `keep_blocks` (the doomed set).
    ///
    /// The node count is EXACT — every interior node and leaf below the inline
    /// root — not the dense `ceil(extents / fanout)` lower bound a two-level
    /// formula gives. The in-place surgery can leave half-filled leaves (more
    /// nodes than a dense pack) and, with the depth cap lifted, a depth-3+ tree
    /// adds interior levels a two-level formula never counts. Reserving against
    /// the real node count is what keeps the fast/slow gate from under-reserving
    /// and misrouting a big truncate into the single transaction that then
    /// stops mid-truncate on a tiny journal (retiring the ledger debt
    /// `whole-truncate-gate-underestimate`). This is `plan_shrink`'s one whole-
    /// tree walk (it replaced the extent-only scan), so the read cost stays
    /// O(tree) — no extra pass — with O(1) memory (two counters).
    pub(super) fn shrink_shape(&self, fs: &Ext4, keep_blocks: Iblock) -> Result<ShrinkShape> {
        let header = self.header();
        let nr = header.entries() as usize;
        if header.is_leaf() {
            // Depth-0: the extents live in the inline root; no external nodes.
            let mut freed_extents = 0;
            for i in 0..nr {
                let e = self.root_extent_at(i);
                if e.block() as u64 + e.len() as u64 > keep_blocks as u64 {
                    freed_extents += 1;
                }
            }
            return Ok(ShrinkShape {
                external_nodes: 0,
                freed_extents,
            });
        }
        let mut shape = ShrinkShape {
            external_nodes: 0,
            freed_extents: 0,
        };
        for i in 0..nr {
            count_subtree(
                fs,
                self.root_index_at(i).leaf(),
                header.depth() - 1,
                keep_blocks,
                &mut shape,
                &self.node_cache,
            )?;
        }
        Ok(shape)
    }

    /// Read access to the extent-status cache for the manager-side query
    /// (Q3) and population (R1) sites; the field itself stays private.
    pub(super) fn es_cache(&self) -> &EsCache {
        &self.es_cache
    }

    /// Re-walks `[start, end)` read-only and panics when `claim` overstates
    /// the tree — the debug double-read net behind a non-`Unknown` es-cache
    /// verdict (the Q2/Q3 side of the three-layer consistency contract, `es`
    /// module docs) — `AllWritten` demands gap-free coverage with zero unwritten
    /// blocks, `AllMapped` demands gap-free coverage, `Unknown` claims
    /// nothing. The es lock is NOT held here (the verdict was copied out).
    /// A walk error skips the verification instead of failing the caller: a
    /// transient funnel failure proves nothing about staleness, and the
    /// release build would have taken the pure-memory hit unconditionally —
    /// an unverifiable hit is skipped rather than escalated, the same
    /// posture as `read_node_cached`'s net. The query points — Q2
    /// (`convert_unwritten`) and Q3 (`fill_holes`) — run this behind every
    /// hit in debug builds (every ktest is one).
    #[cfg(debug_assertions)]
    pub(super) fn debug_assert_es_coverage(
        &self,
        fs: &Ext4,
        start: Iblock,
        end: Iblock,
        claim: EsCoverage,
    ) {
        if claim == EsCoverage::Unknown {
            return;
        }
        let mut covered_upto = start as u64;
        let mut saw_unwritten = false;
        let walked = self.walk_range(fs, start as u64..end as u64, &mut |e| {
            if e.block() as u64 > covered_upto {
                // A gap: the walk is ordered, so nothing later covers it.
                return ControlFlow::Break(());
            }
            saw_unwritten |= e.is_unwritten();
            covered_upto = covered_upto.max(e.block() as u64 + e.len() as u64);
            ControlFlow::Continue(())
        });
        if walked.is_err() {
            return;
        }
        let fully_mapped = covered_upto >= end as u64;
        assert!(
            fully_mapped && (claim == EsCoverage::AllMapped || !saw_unwritten),
            "stale es-cache: claimed {claim:?} over [{start}, {end}) but the tree disagrees"
        );
    }

    /// Decodes leaf entry `i` of the trusted inline root.
    fn root_extent_at(&self, i: usize) -> Extent {
        let off = ENTRY_SIZE * (1 + i);
        Extent::from(&RawExtent::from_bytes(
            &self.root.as_bytes()[off..off + ENTRY_SIZE],
        ))
    }

    /// Decodes index entry `i` of the trusted inline root.
    fn root_index_at(&self, i: usize) -> ExtentIdx {
        let off = ENTRY_SIZE * (1 + i);
        ExtentIdx::from(&RawExtentIdx::from_bytes(
            &self.root.as_bytes()[off..off + ENTRY_SIZE],
        ))
    }
}

/// One in-place insert attempt's outcome: the landing leaf either took the
/// entry, or is full and the caller must reorganize (`make_room_for`) a path
/// with room and retry — the retry protocol `insert`'s loop drives.
enum InPlaceInsert {
    Inserted,
    LeafFull,
}

/// Where a leaf scan landed, before the path is attached.
enum LeafLanding {
    Covered(Extent),
    Gap(Option<Extent>),
}

impl LeafLanding {
    fn into_search(self, path: ExtentPath) -> Search {
        match self {
            LeafLanding::Covered(extent) => Search::Covered { path, extent },
            LeafLanding::Gap(prev) => Search::Gap { path, prev },
        }
    }
}

/// Resolves a leaf scan into its landing position and verdict: `chosen` is the
/// last entry with first block `<= iblock` (`None` = before the first entry).
/// The returned position is the covering entry on a hit, or the insertion
/// point a new entry keyed at `iblock` would take on a miss.
fn leaf_landing(
    chosen: Option<usize>,
    extent_at_fn: impl Fn(usize) -> Extent,
    iblock: Iblock,
) -> (usize, LeafLanding) {
    match chosen {
        None => (0, LeafLanding::Gap(None)),
        Some(i) => {
            let e = extent_at_fn(i);
            if e.covers(iblock) {
                (i, LeafLanding::Covered(e))
            } else {
                (i + 1, LeafLanding::Gap(Some(e)))
            }
        }
    }
}

/// Visits the extents of the subtree rooted at `bid` that overlap `range`, in
/// ascending order — the recursive child step of [`ExtentTree::walk_range`].
/// `expected_depth` enforces the one-step-down invariant ([`ExtentTree::find`]),
/// which also bounds the recursion at [`MAX_DEPTH`](MAX_DEPTH).
/// The tree-shape inputs the whole-truncate routing gate reserves against,
/// gathered in one structural descent ([`ExtentTree::shrink_shape`]).
pub(super) struct ShrinkShape {
    /// Every external (out-of-inode) node: each interior node and each leaf
    /// below the inline root. Counted exactly, so the credit bound tracks the
    /// tree's real (possibly sparse or depth-3+) shape.
    pub(super) external_nodes: usize,
    /// Extents ending past `keep_blocks`: the doomed set, each free of which may
    /// clear a distinct group's block bitmap and GDT block.
    pub(super) freed_extents: usize,
}

/// The counting companion of [`walk_child`]: adds one subtree's external nodes
/// and its extents ending past `keep_blocks` into `shape`. Descends every node
/// the subtree holds (the whole-range shape the truncate gate reserves against),
/// enforcing the same step-down-by-one depth invariant the read walk does.
fn count_subtree(
    fs: &Ext4,
    bid: Ext4Bid,
    expected_depth: u16,
    keep_blocks: Iblock,
    shape: &mut ShrinkShape,
    cache: &NodeCache,
) -> Result<()> {
    let node = read_node_cached(fs, bid, cache)?;
    if node.depth() != expected_depth {
        return_errno_with_message!(
            Errno::EUCLEAN,
            "extent child depth does not step down by one"
        );
    }
    shape.external_nodes += 1;
    if node.is_leaf() {
        for i in 0..node.entries() {
            let e = node.extent_at(i);
            if e.block() as u64 + e.len() as u64 > keep_blocks as u64 {
                shape.freed_extents += 1;
            }
        }
        return Ok(());
    }
    for i in 0..node.entries() {
        count_subtree(
            fs,
            node.index_at(i).leaf(),
            expected_depth - 1,
            keep_blocks,
            shape,
            cache,
        )?;
    }
    Ok(())
}

fn walk_child(
    fs: &Ext4,
    bid: Ext4Bid,
    expected_depth: u16,
    range: &Range<u64>,
    visit_fn: &mut impl FnMut(&Extent) -> ControlFlow<()>,
    cache: &NodeCache,
) -> Result<ControlFlow<()>> {
    let node = read_node_cached(fs, bid, cache)?;
    if node.depth() != expected_depth {
        return_errno_with_message!(
            Errno::EUCLEAN,
            "extent child depth does not step down by one"
        );
    }
    if node.is_leaf() {
        for i in 0..node.entries() {
            let e = node.extent_at(i);
            if e.block() as u64 >= range.end {
                break;
            }
            if e.block() as u64 + e.len() as u64 > range.start && visit_fn(&e).is_break() {
                return Ok(ControlFlow::Break(()));
            }
        }
        return Ok(ControlFlow::Continue(()));
    }
    // `range.start` fits an `Iblock` (walk_range early-returns otherwise) and
    // only shrinks along the recursion.
    let start_key = range.start as Iblock;
    let first = node.index_pos(start_key).unwrap_or(0);
    for i in first..node.entries() {
        let child = node.index_at(i);
        if child.block() as u64 >= range.end {
            break;
        }
        if walk_child(fs, child.leaf(), expected_depth - 1, range, visit_fn, cache)?.is_break() {
            return Ok(ControlFlow::Break(()));
        }
    }
    Ok(ControlFlow::Continue(()))
}

/// The outcome of searching a single extent-tree node for `iblock` — retained
/// (with the linear walker below) as the ktest cross-verification reference
/// for the path-based [`ExtentTree::find`].
#[cfg(ktest)]
enum Step {
    /// A leaf extent that covers `iblock`.
    Found(Extent),
    /// No extent covers `iblock`: a hole.
    Hole,
    /// An interior node points to a child at this physical block.
    Descend(Ext4Bid),
}

/// Parses and searches one freshly read (untrusted) node — the parse boundary
/// for device bytes.
#[cfg(ktest)]
fn search_node(bytes: &[u8], iblock: Iblock) -> Result<Step> {
    let header = ExtentHeader::try_from(&RawExtentHeader::from_bytes(&bytes[0..ENTRY_SIZE]))?;
    search_entries(&header, bytes, iblock)
}

/// Searches one node's entries for `iblock`, `header` already decoded.
///
/// Entries are sorted by logical block, so the covering entry is the last one
/// whose starting block is `<= iblock`.
#[cfg(ktest)]
fn search_entries(header: &ExtentHeader, bytes: &[u8], iblock: Iblock) -> Result<Step> {
    let nr_entries = header.entries() as usize;

    let entries_end = ENTRY_SIZE * (1 + nr_entries);
    if entries_end > bytes.len() {
        return_errno_with_message!(Errno::EUCLEAN, "extent node entries overrun node");
    }

    if header.is_leaf() {
        let mut covering: Option<Extent> = None;
        for i in 0..nr_entries {
            let off = ENTRY_SIZE * (1 + i);
            let extent = Extent::from(&RawExtent::from_bytes(&bytes[off..off + ENTRY_SIZE]));
            if extent.block() <= iblock {
                covering = Some(extent);
            } else {
                break;
            }
        }
        match covering {
            Some(extent) if extent.covers(iblock) => Ok(Step::Found(extent)),
            _ => Ok(Step::Hole),
        }
    } else {
        let mut chosen: Option<ExtentIdx> = None;
        for i in 0..nr_entries {
            let off = ENTRY_SIZE * (1 + i);
            let idx = ExtentIdx::from(&RawExtentIdx::from_bytes(&bytes[off..off + ENTRY_SIZE]));
            if idx.block() <= iblock {
                chosen = Some(idx);
            } else {
                break;
            }
        }
        match chosen {
            Some(idx) => Ok(Step::Descend(idx.leaf())),
            // `iblock` lies before the first index entry. Linux descends into
            // the first child anyway and then finds no covering extent; we
            // short-circuit to the same answer. On a well-formed tree the two
            // are equivalent (no leaf under index 0 maps anything below its
            // `first_block`); on a corrupt tree the short-circuit is the safer
            // degradation — a hole read instead of chasing a bogus subtree
            // (P1 review item, judged & documented at P5).
            None => Ok(Step::Hole),
        }
    }
}

/// Parses one freshly read (untrusted) external leaf node into its extents —
/// the parse boundary for a depth-0 node's device bytes. Rejects a non-leaf
/// header or an entry count that overruns the block. Used by
/// [`ExtentTree::flatten`].
fn parse_leaf_node(block: &[u8]) -> Result<Vec<Extent>> {
    let header = ExtentHeader::try_from(&RawExtentHeader::from_bytes(&block[0..ENTRY_SIZE]))?;
    if !header.is_leaf() {
        return_errno_with_message!(Errno::EUCLEAN, "external extent leaf is not a leaf node");
    }
    let nr = header.entries() as usize;
    if ENTRY_SIZE * (1 + nr) > block.len() {
        return_errno_with_message!(Errno::EUCLEAN, "extent leaf entries overrun node");
    }
    let mut extents = Vec::with_capacity(nr);
    for i in 0..nr {
        let off = ENTRY_SIZE * (1 + i);
        extents.push(Extent::from(&RawExtent::from_bytes(
            &block[off..off + ENTRY_SIZE],
        )));
    }
    Ok(extents)
}

/// Parses one freshly read (untrusted) external interior node (a depth-2 tree's
/// middle level) into its child leaf-block ids — the parse boundary for a
/// depth-1 node's device bytes. Rejects a node that is not a depth-1 interior or
/// an entry count that overruns the block. Used by [`ExtentTree::flatten`].
fn parse_interior_node(block: &[u8]) -> Result<Vec<Ext4Bid>> {
    let header = ExtentHeader::try_from(&RawExtentHeader::from_bytes(&block[0..ENTRY_SIZE]))?;
    if header.is_leaf() || header.depth() != 1 {
        return_errno_with_message!(
            Errno::EUCLEAN,
            "depth-2 child is not a depth-1 interior node"
        );
    }
    let nr = header.entries() as usize;
    if ENTRY_SIZE * (1 + nr) > block.len() {
        return_errno_with_message!(Errno::EUCLEAN, "extent index entries overrun node");
    }
    let mut leaf_bids = Vec::with_capacity(nr);
    for i in 0..nr {
        let off = ENTRY_SIZE * (1 + i);
        leaf_bids
            .push(ExtentIdx::from(&RawExtentIdx::from_bytes(&block[off..off + ENTRY_SIZE])).leaf());
    }
    Ok(leaf_bids)
}

/// The metadata (index/leaf) blocks a tree mutation allocated and freed;
/// consumed internally by the mutators' `i_blocks` accounting.
struct TreeDelta {
    meta_allocated: u32,
    meta_freed: u32,
}

/// The read-only result of planning a COLLAPSE_RANGE: the survivor extents after
/// the left shift, the removed window's freed physical runs, and the external
/// nodes the rebuild will reuse/free. Produced by
/// [`ExtentTree::plan_collapse_range`] with no journal handle and consumed by
/// [`ExtentTree::apply_collapse_range`]; opaque to the [`ExtentManager`] wrapper
/// and its `Inode` caller, which only carry it between the two phases (hence the
/// through-`inode` visibility).
pub(in crate::fs::fs_impls::ext4::inode) struct CollapsePlan {
    survivors: Vec<Extent>,
    freed: Vec<(Ext4Bid, u32)>,
    old_external: Vec<Ext4Bid>,
}

/// The read-only result of planning an INSERT_RANGE: the right-shifted extents
/// and the external nodes the rebuild will reuse/free. See [`CollapsePlan`] for
/// why planning is split from applying.
pub(in crate::fs::fs_impls::ext4::inode) struct InsertPlan {
    shifted: Vec<Extent>,
    old_external: Vec<Ext4Bid>,
}

/// Returns whether the truncate chunk must stop before the next free: `true`
/// when the handle cannot reserve `need` more credits (one free plus its
/// O(depth) node headroom) in its current transaction even after growing in
/// place. A wait-free probe under the ExtentTree lock ③ (never restarts here
/// — the restart's re-admission may wait, illegal under this lock); the OUTER
/// spine restarts with ③ released.
fn stop_before_free(handle: &journal::Handle, need: usize) -> Result<bool> {
    Ok(journal::try_reserve_next(handle, need)? == journal::ExtendOutcome::NeedsRestart)
}

/// Acquires `count` metadata blocks for an external-node rebuild: reuses the
/// front of `pool` (surviving blocks the mutation will overwrite in place) and
/// allocates the shortfall. On an allocation error the freshly allocated
/// blocks are freed before returning, so no metadata leaks.
///
/// Returns the full block list (`reuse` reused blocks followed by the fresh
/// ones) and, separately, just the freshly allocated blocks — the caller frees
/// those if a later node write fails, since the in-memory root has not yet
/// been pointed at the new layout.
fn acquire_meta_blocks(
    fs: &Ext4,
    pool: &[Ext4Bid],
    count: usize,
    goal: Ext4Bid,
    handle: Option<&journal::Handle>,
    cache: &NodeCache,
    intent: AllocIntent,
) -> Result<(Vec<Ext4Bid>, Vec<Ext4Bid>)> {
    let reuse = count.min(pool.len());
    let mut blocks: Vec<Ext4Bid> = pool[..reuse].to_vec();
    let mut newly_allocated: Vec<Ext4Bid> = Vec::new();
    for _ in reuse..count {
        match alloc_meta_block(fs, goal, handle, intent) {
            Ok(bid) => newly_allocated.push(bid),
            Err(err) => {
                rollback_meta_blocks(fs, &newly_allocated, handle, cache);
                return Err(err);
            }
        }
    }
    blocks.extend_from_slice(&newly_allocated);
    Ok((blocks, newly_allocated))
}

/// Frees blocks allocated during a rebuild that then failed, on a best-effort
/// basis (the mutation is already returning an error).
fn rollback_meta_blocks(
    fs: &Ext4,
    blocks: &[Ext4Bid],
    handle: Option<&journal::Handle>,
    cache: &NodeCache,
) {
    for &bid in blocks {
        // Each free clears the cache — a fresh split node written back (and thus
        // cached) before a later sibling's write failed must not survive here.
        let _ = free_meta_block(fs, bid, handle, cache);
    }
}

/// Builds an index entry keyed by first logical block `block`, pointing at the
/// child node at physical block `child_bid`.
fn make_index_entry(block: Iblock, child_bid: Ext4Bid) -> RawExtentIdx {
    // 48-bit on-disk cap (see `RawExtent::from`); lossless while the no-64bit
    // mount invariant bounds bids below 2^32.
    debug_assert!(child_bid < 1 << 48);
    RawExtentIdx {
        block,
        leaf_lo: child_bid as u32,
        leaf_hi: (child_bid >> 32) as u16,
        unused: 0,
    }
}

/// Allocates one metadata block for an external extent-tree node. `intent`
/// decides whether it may draw the reserve pool: a normal insert's tree growth
/// passes [`AllocIntent::Normal`], a conversion split
/// [`AllocIntent::MetadataReserve`].
fn alloc_meta_block(
    fs: &Ext4,
    goal: Ext4Bid,
    handle: Option<&journal::Handle>,
    intent: AllocIntent,
) -> Result<Ext4Bid> {
    let range = fs.alloc_blocks_with_intent(1, goal, handle, intent)?;
    let bid = range.start;
    // Zero-seed the fresh block's capture now; the capture lives in the
    // running transaction (the credential is proof, not owner), and
    // `write_leaf_node` re-mints its own when it fills the block.
    //
    // If seeding the capture fails (e.g. the transaction is at its credit
    // ceiling), free the block before returning: it is allocated (its bitmap
    // bit set and captured) but referenced by nothing, and the callers' own
    // rollbacks only cover the bids they received — an unfreed one leaks until
    // the next e2fsck. The free rides the same handle (its bitmap after-image
    // is already captured, so it costs no new credit).
    if let Err(err) = journal::get_create_access(handle, bid) {
        let _ = fs.free_blocks(journal::BlockFreeAuth::without_revoke_duty(bid, 1), handle);
        return Err(err);
    }
    Ok(bid)
}

/// Frees one external extent-tree metadata block — the textbook revoke case
/// (Linux `ext4_ext_rm_idx` frees tree nodes with `METADATA | FORGET`,
/// fs/ext4/extents.c:2332). The mint is pure; `free_blocks` discharges the
/// forget effects immediately before the bitmap clear, under the same
/// handle, so the revoke record and the clear commit together — and a
/// rebuild that errors before reaching this block's free leaves its journal
/// state untouched (no revoke for a still-referenced node).
fn free_meta_block(
    fs: &Ext4,
    bid: Ext4Bid,
    handle: Option<&journal::Handle>,
    cache: &NodeCache,
) -> Result<()> {
    // The node leaves the tree and its block becomes reusable at commit; drop
    // every cached node so a later read of a reallocated block can never be
    // served this (or any sibling) node's stale bytes. This is the sole choke
    // point every committed tree-node free passes through — the truncate/punch
    // prune, the `reserialize` surplus, and (via `rollback_meta_blocks`) a
    // failed split's fresh nodes — so clearing here alone covers them all (the
    // cache's second coherence rule).
    cache.clear();
    fs.free_blocks(journal::forget(bid, 1), handle)
}

/// Byte offset of `et_checksum` in a full-block external extent node: a 4-byte
/// tail after the header and `LEAF_MAX`/`INTERIOR_MAX` (== 340) entries. Both
/// node kinds write `eh_max = 340`, so Linux's `EXT4_EXTENT_TAIL_OFFSET`
/// (`12 * (1 + eh_max)`) is fixed at this offset.
const EXTENT_TAIL_OFFSET: usize = ENTRY_SIZE * (1 + LEAF_MAX);

/// Stamps the `metadata_csum` extent-block tail (Linux `ext4_extent_block_csum`):
/// crc32c of the node up to the tail, seeded with the owning inode's seed. Only
/// external (full-block) leaf/interior nodes carry this tail; the inline root is
/// covered by the inode checksum instead.
pub(super) fn stamp_extent_tail(block: &mut [u8], seed: InodeCsumSeed) {
    let csum = checksum::crc32c(seed.get(), &block[..EXTENT_TAIL_OFFSET]);
    block[EXTENT_TAIL_OFFSET..EXTENT_TAIL_OFFSET + size_of::<u32>()]
        .copy_from_slice(&csum.to_le_bytes());
}

/// Serializes `extents` into a full-block external leaf node at `bid` — the
/// rebuild's leaf writer, so it demands the es-cache credential like the
/// per-entry editors (see `path.rs`). Its interior sibling below does not:
/// index entries are structure, not logical facts.
fn write_leaf_node(
    device: &dyn BlockDevice,
    bid: Ext4Bid,
    extents: &[Extent],
    handle: Option<&journal::Handle>,
    csum_seed: Option<InodeCsumSeed>,
    _es: &EsInvalidated,
) -> Result<()> {
    let mut block = [0u8; BLOCK_SIZE];
    let header = RawExtentHeader {
        magic: EXTENT_MAGIC,
        entries: extents.len() as u16,
        max: LEAF_MAX as u16,
        depth: 0,
        generation: 0,
    };
    block[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
    for (i, ext) in extents.iter().enumerate() {
        let off = ENTRY_SIZE * (1 + i);
        block[off..off + ENTRY_SIZE].copy_from_slice(RawExtent::from(ext).as_bytes());
    }
    // With `metadata_csum`, stamp the extent-block tail over the finished node
    // (header + entries) before it is captured/written; a `None` seed (feature
    // off) leaves the block byte-for-byte identical.
    if let Some(seed) = csum_seed {
        stamp_extent_tail(&mut block, seed);
    }
    // The leaf may be REUSED from the previous tree layout (`reserialize`
    // re-fills surviving external leaves in place); only freshly allocated
    // leaves were captured by `alloc_meta_block`. Capture idempotently so the
    // reuse path is journaled too — a fresh leaf's zero-seeded capture is left
    // untouched, a reused leaf gets one here.
    let access = journal::get_write_access(handle, bid)?;
    access.patch(|buf| buf.copy_from_slice(&block))?;
    // Under a live capture the extent block reaches its final location via
    // checkpoint after the transaction commits; suppress the direct write so
    // metadata never precedes its commit (WAL). See
    // `fs::Ext4::write_back_inode_desc`.
    if !access.is_live() {
        device.write_val(Bid::new(bid).to_offset(), &block)?;
    }
    Ok(())
}

/// Serializes `idx_entries` into a full-block external interior (depth-1) node
/// at `bid` — the middle level of a depth-2 tree, whose entries point at leaf
/// blocks. Mirrors [`write_leaf_node`], including its journal-capture idempotency
/// (a block reused as an interior may have been a leaf, and vice versa; the whole
/// block is overwritten either way).
fn write_interior_node(
    device: &dyn BlockDevice,
    bid: Ext4Bid,
    idx_entries: &[RawExtentIdx],
    handle: Option<&journal::Handle>,
    csum_seed: Option<InodeCsumSeed>,
) -> Result<()> {
    let mut block = [0u8; BLOCK_SIZE];
    let header = RawExtentHeader {
        magic: EXTENT_MAGIC,
        entries: idx_entries.len() as u16,
        max: INTERIOR_MAX as u16,
        depth: 1,
        generation: 0,
    };
    block[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
    for (i, idx) in idx_entries.iter().enumerate() {
        let off = ENTRY_SIZE * (1 + i);
        block[off..off + ENTRY_SIZE].copy_from_slice(idx.as_bytes());
    }
    // Stamp the extent-block tail with `metadata_csum` on (see `write_leaf_node`);
    // a `None` seed leaves the block unchanged.
    if let Some(seed) = csum_seed {
        stamp_extent_tail(&mut block, seed);
    }
    // Capture idempotently: a reused block gets its capture here, a freshly
    // allocated one keeps the zero-seeded capture from `alloc_meta_block`.
    let access = journal::get_write_access(handle, bid)?;
    access.patch(|buf| buf.copy_from_slice(&block))?;
    // Under a live capture the block reaches its final location via checkpoint
    // after commit; suppress the direct write so metadata never precedes its
    // commit (WAL). See `write_leaf_node`.
    if !access.is_live() {
        device.write_val(Bid::new(bid).to_offset(), &block)?;
    }
    Ok(())
}

/// Returns whether `right` can coalesce onto the end of `left`: logically and
/// physically contiguous, the same written/unwritten state, and the combined
/// length still encodable. Unwritten extents cap one below the written limit:
/// the length is bias-encoded as `len + MAX_WRITTEN_LEN`, so an unwritten run
/// of `MAX_WRITTEN_LEN` would overflow `ee_len` (Linux uses the distinct
/// `EXT_UNWRITTEN_MAX_LEN = 32767`).
fn can_merge(left: &Extent, right: &Extent) -> bool {
    let max_len = if left.is_unwritten() {
        MAX_WRITTEN_LEN as u32 - 1
    } else {
        MAX_WRITTEN_LEN as u32
    };
    left.block() as u64 + left.len() as u64 == right.block() as u64
        && left.start() + left.len() as u64 == right.start()
        && left.is_unwritten() == right.is_unwritten()
        && left.len() as u32 + right.len() as u32 <= max_len
}

/// Coalesces `left` and `right`; caller proved [`can_merge`].
fn merged_pair(left: &Extent, right: &Extent) -> Extent {
    Extent::new(
        left.block(),
        left.len() + right.len(),
        left.start(),
        left.kind(),
    )
}

/// Coalesces leaf entry `pos` with its immediate in-leaf neighbours when they
/// are logically and physically contiguous and share the same kind — the
/// local counterpart of [`merge_extents`] for an in-place edit (a kind flip or
/// a trim can make a run continuous with a sibling). Merges the right neighbour
/// first (so `pos` stays valid), then the left. Only touches this one leaf;
/// runs split across a leaf boundary stay separate (a benign fragment the old
/// whole-tree rebuild would have merged — acceptable, and rare).
fn merge_leaf_neighbors(leaf: &mut NodeBuf, pos: usize, es: &EsInvalidated) {
    if pos + 1 < leaf.entries() {
        let cur = leaf.extent_at(pos);
        let next = leaf.extent_at(pos + 1);
        if can_merge(&cur, &next) {
            leaf.replace_extent_at(pos, &merged_pair(&cur, &next), es);
            leaf.remove_extent_at(pos + 1, es);
        }
    }
    if pos > 0 {
        let prev = leaf.extent_at(pos - 1);
        let cur = leaf.extent_at(pos);
        if can_merge(&prev, &cur) {
            leaf.replace_extent_at(pos - 1, &merged_pair(&prev, &cur), es);
            leaf.remove_extent_at(pos, es);
        }
    }
}

/// Sorts `extents` by logical block and coalesces runs that are logically and
/// physically contiguous and share the same written/unwritten state.
fn merge_extents(extents: &mut Vec<Extent>) {
    extents.sort_by_key(|e| e.block());
    let mut merged: Vec<Extent> = Vec::with_capacity(extents.len());
    for e in extents.iter() {
        if let Some(last) = merged.last().copied()
            && can_merge(&last, e)
        {
            *merged.last_mut().unwrap() = merged_pair(&last, e);
            continue;
        }
        merged.push(*e);
    }
    *extents = merged;
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;
    use crate::fs::fs_impls::ext4::test_utils::Ext4FixtureBuilder;

    /// Mints an es-cache invalidation credential for tests that drive the
    /// leaf editors directly (no tree in play, hence no real cache to
    /// invalidate — a scratch cache's `clear_all` is the honest stand-in).
    fn es_token() -> EsInvalidated {
        EsCache::new().clear_all()
    }

    /// Builds a validated tree over a depth-0 root (header + extents).
    fn inline_tree(extents: &[RawExtent]) -> ExtentTree {
        let mut block = [0u32; RAW_BLOCK_PTRS_LEN];
        let bytes = block.as_mut_bytes();
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: extents.len() as u16,
            max: 4,
            depth: 0,
            generation: 0,
        };
        bytes[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        for (i, extent) in extents.iter().enumerate() {
            let off = ENTRY_SIZE * (1 + i);
            bytes[off..off + ENTRY_SIZE].copy_from_slice(extent.as_bytes());
        }
        ExtentTree::try_new(block, 0).unwrap()
    }

    #[ktest]
    fn inline_single_extent_lookup() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        // One extent mapping logical 0..4 to physical 100..104.
        let tree = inline_tree(&[RawExtent {
            block: 0,
            len: 4,
            start_hi: 0,
            start_lo: 100,
        }]);

        let mapped = tree.lookup(&f.ext4, 2).unwrap().unwrap();
        assert_eq!(mapped.start(), 100);
        assert_eq!(mapped.block(), 0);

        // Block 4 is beyond the extent: a hole.
        assert!(tree.lookup(&f.ext4, 4).unwrap().is_none());
    }

    #[ktest]
    fn inline_multiple_extents_lookup() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let tree = inline_tree(&[
            RawExtent {
                block: 0,
                len: 2,
                start_hi: 0,
                start_lo: 200,
            },
            RawExtent {
                block: 5,
                len: 3,
                start_hi: 0,
                start_lo: 300,
            },
        ]);

        // Logical 6 → second extent, physical 300 + (6 - 5) = 301.
        let mapped = tree.lookup(&f.ext4, 6).unwrap().unwrap();
        assert_eq!(mapped.start() + (6 - mapped.block()) as u64, 301);

        // Logical 3 falls in the gap between the two extents: a hole.
        assert!(tree.lookup(&f.ext4, 3).unwrap().is_none());
    }

    #[ktest]
    fn empty_root_is_all_holes() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        let tree = ExtentTree::empty();
        assert!(tree.lookup(&f.ext4, 0).unwrap().is_none());
    }

    #[ktest]
    fn rejects_bad_root_magic() {
        let root = [0u32; RAW_BLOCK_PTRS_LEN];
        assert!(ExtentTree::try_new(root, 0).is_err());
    }

    /// Builds a validated tree over a depth-1 index root pointing at a single
    /// external leaf node at physical block `leaf_block`.
    fn index_tree(leaf_block: u32) -> ExtentTree {
        let mut block = [0u32; RAW_BLOCK_PTRS_LEN];
        let bytes = block.as_mut_bytes();
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: 1,
            max: 4,
            depth: 1,
            generation: 0,
        };
        bytes[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        let idx = RawExtentIdx {
            block: 0,
            leaf_lo: leaf_block,
            leaf_hi: 0,
            unused: 0,
        };
        bytes[ENTRY_SIZE..2 * ENTRY_SIZE].copy_from_slice(idx.as_bytes());
        ExtentTree::try_new(block, 0).unwrap()
    }

    /// Builds a full-block external leaf node (depth 0) from `extents`.
    fn leaf_node(extents: &[RawExtent]) -> [u8; BLOCK_SIZE] {
        let mut block = [0u8; BLOCK_SIZE];
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: extents.len() as u16,
            max: ((BLOCK_SIZE / ENTRY_SIZE) - 1) as u16,
            depth: 0,
            generation: 0,
        };
        block[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        for (i, extent) in extents.iter().enumerate() {
            let off = ENTRY_SIZE * (1 + i);
            block[off..off + ENTRY_SIZE].copy_from_slice(extent.as_bytes());
        }
        block
    }

    /// A depth-1 tree (index root → external leaf read from the device) must be
    /// descended into. This exercises the interior-node read path that inline
    /// (depth-0) roots never reach — the real-image counterpart is a fragmented
    /// file whose extents overflow the inline root.
    #[ktest]
    fn descends_into_external_leaf() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();

        let leaf_block = 200u32;
        let leaf = leaf_node(&[
            RawExtent {
                block: 0,
                len: 2,
                start_hi: 0,
                start_lo: 300,
            },
            RawExtent {
                block: 5,
                len: 3,
                start_hi: 0,
                start_lo: 400,
            },
        ]);
        f.write_data_block(leaf_block, &leaf);
        let tree = index_tree(leaf_block);

        // Logical 1 → descend to the leaf → first extent (0..2) → physical 301.
        let m0 = tree.lookup(&f.ext4, 1).unwrap().unwrap();
        assert_eq!(m0.start() + (1 - m0.block()) as u64, 301);
        // Logical 6 → second extent (5..8) → physical 401.
        let m1 = tree.lookup(&f.ext4, 6).unwrap().unwrap();
        assert_eq!(m1.start() + (6 - m1.block()) as u64, 401);
        // Logical 3 → gap between the leaf's extents → hole.
        assert!(tree.lookup(&f.ext4, 3).unwrap().is_none());
        // Logical 100 → beyond all extents → hole.
        assert!(tree.lookup(&f.ext4, 100).unwrap().is_none());
    }

    /// Returns the entry count of the root header.
    fn root_entries(tree: &ExtentTree) -> u16 {
        ExtentHeader::try_from(&RawExtentHeader::from_bytes(
            &tree.root_bytes().as_bytes()[0..ENTRY_SIZE],
        ))
        .unwrap()
        .entries()
    }

    #[ktest]
    fn insert_into_inline_merges_contiguous() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();

        // [0,2) -> 100, then contiguous [2,2) -> 102 must coalesce into [0,4).
        tree.insert(&f.ext4, 0, 100, 2, ExtentKind::Written, None, None)
            .unwrap();
        // Pure data growth: no metadata block was needed.
        assert_eq!(tree.sector_count(), 2 * SECTORS_PER_BLOCK);
        tree.insert(&f.ext4, 2, 102, 2, ExtentKind::Written, None, None)
            .unwrap();

        // Still inline depth-0 with a single merged extent.
        assert_eq!((tree.depth(), root_entries(&tree)), (0, 1));
        assert_eq!(tree.sector_count(), 4 * SECTORS_PER_BLOCK);
        assert!(tree.is_dirty());
        let m = tree.lookup(&f.ext4, 3).unwrap().unwrap();
        assert_eq!(m.start() + (3 - m.block()) as u64, 103);
    }

    #[ktest]
    fn insert_non_contiguous_stays_separate() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();

        tree.insert(&f.ext4, 0, 100, 1, ExtentKind::Written, None, None)
            .unwrap();
        tree.insert(&f.ext4, 5, 200, 1, ExtentKind::Written, None, None)
            .unwrap();

        assert_eq!((tree.depth(), root_entries(&tree)), (0, 2));
        assert_eq!(tree.lookup(&f.ext4, 0).unwrap().unwrap().start(), 100);
        assert_eq!(tree.lookup(&f.ext4, 5).unwrap().unwrap().start(), 200);
        assert!(tree.lookup(&f.ext4, 3).unwrap().is_none());
    }

    #[ktest]
    fn inline_overflow_grows_to_depth1() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();

        // Five non-contiguous extents overflow the 4-entry inline root.
        for k in 0..5u32 {
            tree.insert(
                &f.ext4,
                k * 2,
                100 + k as u64 * 10,
                1,
                ExtentKind::Written,
                None,
                None,
            )
            .unwrap();
        }

        // The root is now a depth-1 index with one external leaf, and
        // `i_blocks` counts the 5 data blocks plus exactly 1 leaf block.
        assert_eq!((tree.depth(), root_entries(&tree)), (1, 1));
        assert_eq!(tree.sector_count(), (5 + 1) * SECTORS_PER_BLOCK);

        // All five mappings are still reachable through the external leaf.
        for k in 0..5u32 {
            let m = tree.lookup(&f.ext4, k * 2).unwrap().unwrap();
            assert_eq!(m.start(), 100 + k as u64 * 10);
        }

        // The allocated leaf block is marked in the block bitmap (e2fsck-clean).
        let leaf_bid = ExtentIdx::from(&RawExtentIdx::from_bytes(
            &tree.root_bytes().as_bytes()[ENTRY_SIZE..2 * ENTRY_SIZE],
        ))
        .leaf();
        let group = f.ext4.block_group(0);
        let metadata = group.metadata();
        assert!(
            metadata
                .block_bitmap
                .is_allocated((leaf_bid - group.first_block()) as u16)
        );
    }

    #[ktest]
    fn insert_into_depth1_reuses_leaf() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        for k in 0..5u32 {
            tree.insert(
                &f.ext4,
                k * 2,
                100 + k as u64 * 10,
                1,
                ExtentKind::Written,
                None,
                None,
            )
            .unwrap();
        }
        assert_eq!(tree.depth(), 1);
        let sectors_before = tree.sector_count();

        // A sixth extent fits the existing leaf: no new metadata block, so
        // `i_blocks` grows by the data block only.
        tree.insert(&f.ext4, 20, 500, 1, ExtentKind::Written, None, None)
            .unwrap();
        assert_eq!(tree.sector_count(), sectors_before + SECTORS_PER_BLOCK);

        assert_eq!(tree.lookup(&f.ext4, 20).unwrap().unwrap().start(), 500);
    }

    /// A file fragmented past the depth-1 capacity (`INLINE_MAX × LEAF_MAX` =
    /// 4 × 340 = 1360 extents) must grow a *depth-2* tree (root → interior node →
    /// leaf nodes), and every mapping must stay reachable through both index
    /// levels. Stride-2 single-block extents never coalesce, so N inserts yield N
    /// distinct extents. 1400 extents need 5 leaves (> `INLINE_MAX`), overflowing
    /// the inline root into one interior node holding 5 leaf-index entries.
    #[ktest]
    fn overflow_past_depth1_grows_to_depth2() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();

        const N: u32 = 1400;
        const DATA_BASE: Ext4Bid = 100_000;
        for k in 0..N {
            tree.insert(
                &f.ext4,
                k * 2,
                DATA_BASE + k as Ext4Bid,
                1,
                ExtentKind::Written,
                None,
                None,
            )
            .unwrap();
        }

        // The inline root is now a depth-2 index.
        assert_eq!(tree.depth(), 2);

        // Every mapping is reachable, spot-checked across all five leaf blocks
        // (each leaf holds 340 extents: 0..340, 340..680, 680..1020, 1020..1360,
        // 1360..1400). The stride-1 gap after each extent stays a hole.
        for k in [0u32, 339, 340, 680, 1020, 1360, 1399] {
            let m = tree.lookup(&f.ext4, k * 2).unwrap().unwrap();
            assert_eq!(m.block(), k * 2);
            assert_eq!(m.start(), DATA_BASE + k as Ext4Bid);
            assert!(tree.lookup(&f.ext4, k * 2 + 1).unwrap().is_none());
        }

        // `i_blocks` counts the N data blocks plus the tree metadata: 5 leaves +
        // 1 interior = 6 blocks. The pure-growth sequence never frees, so the
        // total is exact — and trivially non-negative and above the data count.
        const NR_META: u64 = 6;
        assert_eq!(
            tree.sector_count(),
            (N as u64 + NR_META) * SECTORS_PER_BLOCK
        );
    }

    /// Regression: two contiguous *unwritten* extents whose lengths sum to
    /// `MAX_WRITTEN_LEN` (32768) must NOT coalesce — an unwritten `ee_len` of
    /// 32768 overflows the bias encoding (`len + MAX_WRITTEN_LEN`), so an
    /// unwritten run caps at `EXT_UNWRITTEN_MAX_LEN = 32767`. Written runs of the
    /// same shape may still merge to 32768.
    #[ktest]
    fn merge_caps_unwritten_below_max_len() {
        let half = MAX_WRITTEN_LEN / 2; // 16384

        let mut unwritten = vec![
            Extent::new(0, half, 100, ExtentKind::Unwritten),
            Extent::new(
                half as Iblock,
                half,
                100 + half as Ext4Bid,
                ExtentKind::Unwritten,
            ),
        ];
        merge_extents(&mut unwritten);
        for e in &unwritten {
            assert!(!e.is_unwritten() || e.len() < MAX_WRITTEN_LEN);
        }

        let mut written = vec![
            Extent::new(0, half, 200, ExtentKind::Written),
            Extent::new(
                half as Iblock,
                half,
                200 + half as Ext4Bid,
                ExtentKind::Written,
            ),
        ];
        merge_extents(&mut written);
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].len(), MAX_WRITTEN_LEN);
    }

    /// `stamp_extent_tail` writes crc32c of the node's first `EXTENT_TAIL_OFFSET`
    /// bytes into the 4-byte tail, and any change to the covered region changes
    /// the stamp — the `metadata_csum` extent-block invariant (Linux
    /// `ext4_extent_block_csum`).
    #[ktest]
    fn stamp_extent_tail_matches_crc32c() {
        let seed = checksum::FsCsumSeed::new(0x1357_9bdf).derive_inode(1, 0);
        let mut block = [0u8; BLOCK_SIZE];
        for (i, b) in block.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        // The tail is excluded from its own cover; zero it before stamping.
        block[EXTENT_TAIL_OFFSET..EXTENT_TAIL_OFFSET + 4].fill(0);

        stamp_extent_tail(&mut block, seed);
        let stored = u32::from_le_bytes(
            block[EXTENT_TAIL_OFFSET..EXTENT_TAIL_OFFSET + 4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(
            stored,
            checksum::crc32c(seed.get(), &block[..EXTENT_TAIL_OFFSET])
        );

        // A single-byte change to the covered node re-stamps to a new value.
        block[0] ^= 0xFF;
        stamp_extent_tail(&mut block, seed);
        let restamped = u32::from_le_bytes(
            block[EXTENT_TAIL_OFFSET..EXTENT_TAIL_OFFSET + 4]
                .try_into()
                .unwrap(),
        );
        assert_ne!(stored, restamped);
    }

    /// End-to-end: a seeded depth-1 rebuild writes its external leaf to the
    /// device with a correct extent-block tail checksum; a `None` seed leaves the
    /// tail zeroed. Exercises the `write_leaf_node` compute-on-write hook.
    #[ktest]
    fn seeded_reserialize_stamps_leaf_tail() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();

        let seed = checksum::FsCsumSeed::new(0x0bad_c0de).derive_inode(1, 0);
        let mut tree = ExtentTree::empty();
        // Five disjoint single-block extents overflow the inline root into one
        // external leaf (depth 1).
        for k in 0..5u32 {
            tree.insert(
                &f.ext4,
                k * 2,
                100 + k as Ext4Bid,
                1,
                ExtentKind::Written,
                None,
                Some(seed),
            )
            .unwrap();
        }
        assert_eq!(tree.depth(), 1);

        // The leaf block id lives in the root's single index entry.
        let leaf_bid = ExtentIdx::from(&RawExtentIdx::from_bytes(
            &tree.root_bytes().as_bytes()[ENTRY_SIZE..2 * ENTRY_SIZE],
        ))
        .leaf();

        // No journal handle was used, so the leaf was written straight to disk.
        let mut block = [0u8; BLOCK_SIZE];
        f.disk
            .segment()
            .read_bytes(leaf_bid as usize * BLOCK_SIZE, &mut block)
            .unwrap();
        let stored = u32::from_le_bytes(
            block[EXTENT_TAIL_OFFSET..EXTENT_TAIL_OFFSET + 4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(
            stored,
            checksum::crc32c(seed.get(), &block[..EXTENT_TAIL_OFFSET])
        );
    }

    // ---- P9a-T1: surgery read side (find / walk_range) cross-checks ----

    /// `find` must agree with the pre-surgery linear walker on every probe,
    /// and `walk_range` over the whole space must reproduce the flatten list,
    /// at every tree shape from inline through depth-2 — the path-based read
    /// side is only trusted through this equivalence.
    #[ktest]
    fn find_and_walk_match_linear_reference_across_shapes() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();

        // 1450 single-block extents with one-block gaps (unmergeable) force
        // growth through inline → depth-1 → depth-2 (5 leaves + 1 interior).
        // Checkpoints along the way exercise each shape; unwritten kind every
        // third extent exercises kind fidelity.
        let checkpoints = [3usize, 4, 300, 1400, 1450];
        let mut inserted = 0usize;
        for &target in &checkpoints {
            while inserted < target {
                let i = inserted as u32;
                let kind = if i.is_multiple_of(3) {
                    ExtentKind::Unwritten
                } else {
                    ExtentKind::Written
                };
                tree.insert(
                    &f.ext4,
                    i * 2,
                    100_000 + i as Ext4Bid * 2,
                    1,
                    kind,
                    None,
                    None,
                )
                .unwrap();
                inserted += 1;
            }

            // Probe every logical block up to past the last extent.
            for ib in 0..(inserted as u32 * 2 + 4) {
                let linear = tree.lookup_linear(&f.ext4, ib).unwrap();
                match (linear, tree.find(&f.ext4, ib).unwrap()) {
                    (Some(l), Search::Covered { extent: e, .. }) => {
                        assert_eq!(
                            (l.block(), l.len(), l.start(), l.kind()),
                            (e.block(), e.len(), e.start(), e.kind())
                        );
                    }
                    (None, Search::Gap { .. }) => {}
                    (l, _) => panic!("find/linear disagree at block {ib} (linear: {l:?})"),
                }
            }

            // walk_range over everything == flatten, order and fields.
            let (mut flat, _) = tree.flatten(&f.ext4).unwrap();
            flat.sort_by_key(|e| e.block());
            let mut walked = Vec::new();
            tree.walk_range(&f.ext4, 0..u64::MAX, &mut |e| {
                walked.push(*e);
                ControlFlow::Continue(())
            })
            .unwrap();
            assert_eq!(flat.len(), walked.len());
            for (a, b) in flat.iter().zip(walked.iter()) {
                assert_eq!(
                    (a.block(), a.len(), a.start(), a.kind()),
                    (b.block(), b.len(), b.start(), b.kind())
                );
            }
        }
        assert_eq!(tree.depth(), 2);

        // Bounded walk: exactly the extents overlapping [101, 140) — blocks
        // are even, so extents 51..=69 qualify (block 102..=138).
        let mut seen = Vec::new();
        tree.walk_range(&f.ext4, 101..140, &mut |e| {
            seen.push(e.block());
            ControlFlow::Continue(())
        })
        .unwrap();
        let want: Vec<Iblock> = (51..70).map(|i| i * 2).collect();
        assert_eq!(seen, want);

        // Early break stops the walk mid-tree.
        let mut count = 0;
        tree.walk_range(&f.ext4, 0..u64::MAX, &mut |_| {
            count += 1;
            if count == 3 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .unwrap();
        assert_eq!(count, 3);
    }

    /// A gap's landing carries the insertion point and the in-leaf
    /// predecessor (the allocation-goal donor) — the rule-6 payload inserts
    /// consume without a second walk.
    #[ktest]
    fn find_gap_reports_insertion_point_and_prev() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();

        // Inline root: extents [0,2)→100 and [5,8)→400.
        let tree = inline_tree(&[
            RawExtent {
                block: 0,
                len: 2,
                start_hi: 0,
                start_lo: 100,
            },
            RawExtent {
                block: 5,
                len: 3,
                start_hi: 0,
                start_lo: 400,
            },
        ]);
        match tree.find(&f.ext4, 3).unwrap() {
            Search::Gap { path, prev } => {
                assert!(path.levels.is_empty());
                assert_eq!(path.root_pos, 1); // between the two entries
                assert_eq!(prev.unwrap().block(), 0);
            }
            _ => panic!("block 3 must be a gap"),
        }
        match tree.find(&f.ext4, 6).unwrap() {
            Search::Covered { path, extent } => {
                assert_eq!(path.root_pos, 1);
                assert_eq!(extent.block(), 5);
            }
            _ => panic!("block 6 must be covered"),
        }

        // External leaf via a depth-1 root: same landings, one path level.
        let leaf_block = 200u32;
        let leaf = leaf_node(&[
            RawExtent {
                block: 5,
                len: 3,
                start_hi: 0,
                start_lo: 400,
            },
            RawExtent {
                block: 20,
                len: 1,
                start_hi: 0,
                start_lo: 500,
            },
        ]);
        f.write_data_block(leaf_block, &leaf);
        let tree = index_tree(leaf_block);

        // Before the leaf's first entry: insertion point 0, no predecessor.
        match tree.find(&f.ext4, 2).unwrap() {
            Search::Gap { path, prev } => {
                assert_eq!(path.levels.len(), 1);
                assert_eq!(path.leaf().unwrap().pos, 0);
                assert!(prev.is_none());
            }
            _ => panic!("block 2 must be a gap"),
        }
        // Past the last entry: insertion point = entry count, prev = last.
        match tree.find(&f.ext4, 100).unwrap() {
            Search::Gap { path, prev } => {
                assert_eq!(path.leaf().unwrap().pos, 2);
                assert_eq!(prev.unwrap().block(), 20);
            }
            _ => panic!("block 100 must be a gap"),
        }
    }

    /// The path walker rejects structurally corrupt children loud: a child
    /// whose depth does not step down by one, and a node whose entry count
    /// overruns the block.
    #[ktest]
    fn find_rejects_corrupt_children() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();

        // A depth-1 root must point at leaves; hand it an interior node.
        let bogus_block = 210u32;
        let mut interior = [0u8; BLOCK_SIZE];
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: 0,
            max: INTERIOR_MAX as u16,
            depth: 1,
            generation: 0,
        };
        interior[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        f.write_data_block(bogus_block, &interior);
        let tree = index_tree(bogus_block);
        assert!(tree.find(&f.ext4, 0).is_err());
        let mut visited = 0;
        assert!(
            tree.walk_range(&f.ext4, 0..u64::MAX, &mut |_| {
                visited += 1;
                ControlFlow::Continue(())
            })
            .is_err()
        );
        assert_eq!(visited, 0);

        // An entry count that overruns the 4K node (forged max admits it past
        // the header check) must be rejected at the NodeBuf parse boundary.
        let overrun_block = 211u32;
        let mut overrun = [0u8; BLOCK_SIZE];
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: 400,
            max: 500,
            depth: 0,
            generation: 0,
        };
        overrun[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        f.write_data_block(overrun_block, &overrun);
        let tree = index_tree(overrun_block);
        assert!(tree.find(&f.ext4, 0).is_err());
    }

    /// The every-write convert gate must stay semantics-identical after the
    /// bounded-probe rewrite: a written-only range returns untouched (no
    /// dirtying, no rebuild), an unwritten overlap still converts.
    #[ktest]
    fn convert_gate_bounded_probe_keeps_semantics() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        tree.insert(&f.ext4, 0, 100, 4, ExtentKind::Written, None, None)
            .unwrap();
        tree.insert(&f.ext4, 10, 200, 2, ExtentKind::Unwritten, None, None)
            .unwrap();
        tree.clear_dirty();

        // Written-only range: the gate returns before any rebuild.
        tree.convert_unwritten(&f.ext4, 0, 4, None, None).unwrap();
        assert!(!tree.is_dirty());

        // Unwritten overlap: conversion still runs and splits at the range end.
        tree.convert_unwritten(&f.ext4, 10, 1, None, None).unwrap();
        assert!(tree.is_dirty());
        assert!(!tree.lookup(&f.ext4, 10).unwrap().unwrap().is_unwritten());
        assert!(tree.lookup(&f.ext4, 11).unwrap().unwrap().is_unwritten());
    }

    // ---- P9a-T2: the insert fast path (in-place leaf edits) ----

    /// Decodes root index entry `i`'s key (test-side check of correct_indexes).
    fn root_index_key(tree: &ExtentTree, i: usize) -> Iblock {
        let bytes = tree.root_bytes().as_bytes();
        let off = ENTRY_SIZE * (1 + i);
        ExtentIdx::from(&RawExtentIdx::from_bytes(&bytes[off..off + ENTRY_SIZE])).block()
    }

    /// Probes every block in `0..limit` against the linear reference.
    fn assert_matches_linear(
        f: &crate::fs::fs_impls::ext4::test_utils::Ext4Fixture,
        tree: &ExtentTree,
        limit: Iblock,
    ) {
        for ib in 0..limit {
            let linear = tree.lookup_linear(&f.ext4, ib).unwrap();
            let new = tree.lookup(&f.ext4, ib).unwrap();
            match (linear, new) {
                (Some(l), Some(n)) => assert_eq!(
                    (l.block(), l.len(), l.start(), l.kind()),
                    (n.block(), n.len(), n.start(), n.kind()),
                    "mismatch at block {ib}"
                ),
                (None, None) => {}
                (l, n) => panic!("verdicts differ at block {ib}: linear {l:?} vs path {n:?}"),
            }
        }
    }

    /// The in-place insert fast path on an external leaf: mid-leaf shift
    /// insert, left merge, bridging absorb, and a position-0 insert that
    /// corrects the root index key — each leaving `i_blocks` with a pure-data
    /// delta (no metadata block churn, the fast-path signature) and the whole
    /// tree agreeing with the linear reference.
    #[ktest]
    fn insert_fast_path_edits_leaf_in_place() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();

        // Six unmergeable extents overflow the inline root → depth 1, one leaf.
        for (b, p) in [
            (10, 1000),
            (20, 2000),
            (30, 3000),
            (40, 4000),
            (50, 5000),
            (60, 6000),
        ] {
            tree.insert(&f.ext4, b, p, 2, ExtentKind::Written, None, None)
                .unwrap();
        }
        assert_eq!(tree.depth(), 1);

        // Mid-leaf shift insert (no merge partner): pure-data i_blocks delta.
        let sc = tree.sector_count();
        tree.insert(&f.ext4, 25, 9000, 2, ExtentKind::Written, None, None)
            .unwrap();
        assert_eq!(tree.sector_count(), sc + 2 * SECTORS_PER_BLOCK);
        let m = tree.lookup(&f.ext4, 26).unwrap().unwrap();
        assert_eq!((m.block(), m.len(), m.start()), (25, 2, 9000));

        // Left merge: physically contiguous extension of [10,12)→1000.
        let sc = tree.sector_count();
        tree.insert(&f.ext4, 12, 1002, 2, ExtentKind::Written, None, None)
            .unwrap();
        assert_eq!(tree.sector_count(), sc + 2 * SECTORS_PER_BLOCK);
        let m = tree.lookup(&f.ext4, 13).unwrap().unwrap();
        assert_eq!((m.block(), m.len(), m.start()), (10, 4, 1000));

        // Bridging absorb: two physically consecutive runs with a hole flush
        // between them; filling it fuses all three into one extent.
        tree.insert(&f.ext4, 70, 8000, 2, ExtentKind::Written, None, None)
            .unwrap();
        tree.insert(&f.ext4, 75, 8005, 3, ExtentKind::Written, None, None)
            .unwrap();
        let sc = tree.sector_count();
        tree.insert(&f.ext4, 72, 8002, 3, ExtentKind::Written, None, None)
            .unwrap();
        assert_eq!(tree.sector_count(), sc + 3 * SECTORS_PER_BLOCK);
        let m = tree.lookup(&f.ext4, 77).unwrap().unwrap();
        assert_eq!((m.block(), m.len(), m.start()), (70, 8, 8000));

        // Position-0 insert: the leaf's first key drops from 10 to 4 and the
        // root index key follows (correct_indexes at the root).
        assert_eq!(root_index_key(&tree, 0), 10);
        tree.insert(&f.ext4, 4, 7000, 2, ExtentKind::Written, None, None)
            .unwrap();
        assert_eq!(root_index_key(&tree, 0), 4);
        assert_eq!(tree.lookup(&f.ext4, 4).unwrap().unwrap().start(), 7000);
        // Blocks below the new first key stay holes.
        assert!(tree.lookup(&f.ext4, 3).unwrap().is_none());

        assert_matches_linear(&f, &tree, 90);
    }

    /// The sequential-append pattern (fio seq write, SQLite growth): every
    /// extension left-merges onto the tail extent in place — extent count and
    /// metadata footprint stay constant while only data blocks accrue.
    #[ktest]
    fn insert_fast_path_appends_without_rebuild() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        for (b, p) in [(0, 500), (10, 600), (20, 700), (30, 800), (40, 900)] {
            tree.insert(&f.ext4, b, p, 2, ExtentKind::Written, None, None)
                .unwrap();
        }
        assert_eq!(tree.depth(), 1);

        // 60 contiguous appends onto the tail run [40,42)→900.
        let sc = tree.sector_count();
        for i in 0..60u32 {
            tree.insert(
                &f.ext4,
                42 + i,
                902 + i as Ext4Bid,
                1,
                ExtentKind::Written,
                None,
                None,
            )
            .unwrap();
        }
        // Pure data growth (no metadata churn), one merged tail extent.
        assert_eq!(tree.sector_count(), sc + 60 * SECTORS_PER_BLOCK);
        let m = tree.lookup(&f.ext4, 101).unwrap().unwrap();
        assert_eq!((m.block(), m.len(), m.start()), (40, 62, 900));
        let (extents, _) = tree.flatten(&f.ext4).unwrap();
        assert_eq!(extents.len(), 5);

        assert_matches_linear(&f, &tree, 110);
    }

    // ---- P9a-T3: splits and depth growth ----

    /// Builds a tree of `n` unmergeable single-block extents at blocks
    /// `0,2,4,…` (odd physical parity kills merging) through the ordinary
    /// insert path — ascending appends, so every split is a pure-boundary
    /// (dense) one.
    fn ascending_tree(
        f: &crate::fs::fs_impls::ext4::test_utils::Ext4Fixture,
        n: u32,
    ) -> ExtentTree {
        let mut tree = ExtentTree::empty();
        for i in 0..n {
            tree.insert(
                &f.ext4,
                i * 2,
                200_000 + i as Ext4Bid * 2,
                1,
                ExtentKind::Written,
                None,
                None,
            )
            .unwrap();
        }
        tree
    }

    /// A full leaf split on the append boundary keeps the old leaf dense
    /// (Linux's split-at-insert-point policy): the new leaf starts with just
    /// the appended extent, and the root gains exactly one index entry.
    #[ktest]
    fn split_on_append_keeps_leaves_dense() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let tree_full = ascending_tree(&f, LEAF_MAX as u32);
        assert_eq!((tree_full.depth(), root_entries(&tree_full)), (1, 1));

        let mut tree = tree_full;
        let sc = tree.sector_count();
        // The 341st ascending extent: leaf full → split; new leaf holds it alone.
        tree.insert(
            &f.ext4,
            LEAF_MAX as u32 * 2,
            300_000,
            1,
            ExtentKind::Written,
            None,
            None,
        )
        .unwrap();
        assert_eq!((tree.depth(), root_entries(&tree)), (1, 2));
        // One fresh metadata block plus one data block.
        assert_eq!(tree.sector_count(), sc + 2 * SECTORS_PER_BLOCK);
        match tree.find(&f.ext4, LEAF_MAX as u32 * 2).unwrap() {
            Search::Covered { path, .. } => {
                assert_eq!(path.leaf().unwrap().node.entries(), 1);
            }
            _ => panic!("appended extent must be covered"),
        }
        match tree.find(&f.ext4, 0).unwrap() {
            Search::Covered { path, .. } => {
                assert_eq!(path.leaf().unwrap().node.entries(), LEAF_MAX);
            }
            _ => panic!("old extents must survive the split"),
        }
        assert_matches_linear(&f, &tree, LEAF_MAX as u32 * 2 + 4);
    }

    /// A mid-leaf split moves the tail entries to the new leaf and lands the
    /// extent in the old one; a below-first-key insert into a full leaf moves
    /// everything, lands in the emptied old leaf, and corrects the root key.
    #[ktest]
    fn split_mid_leaf_and_position_zero() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();

        // Mid split: insert into the hole at block 401 (odd = unmapped).
        let mut tree = ascending_tree(&f, LEAF_MAX as u32);
        tree.insert(&f.ext4, 401, 400_000, 1, ExtentKind::Written, None, None)
            .unwrap();
        assert_eq!((tree.depth(), root_entries(&tree)), (1, 2));
        let m = tree.lookup(&f.ext4, 401).unwrap().unwrap();
        assert_eq!(m.start(), 400_000);
        assert_matches_linear(&f, &tree, LEAF_MAX as u32 * 2 + 4);

        // Position-zero split: rebuild dense, then insert below every key.
        // (Blocks start at 2 here so 0..2 is a hole below the first key.)
        let mut tree = ExtentTree::empty();
        for i in 0..LEAF_MAX as u32 {
            tree.insert(
                &f.ext4,
                2 + i * 2,
                500_000 + i as Ext4Bid * 2,
                1,
                ExtentKind::Written,
                None,
                None,
            )
            .unwrap();
        }
        assert_eq!(root_index_key(&tree, 0), 2);
        tree.insert(&f.ext4, 0, 600_000, 1, ExtentKind::Written, None, None)
            .unwrap();
        // The leaf's (and root's) first key dropped to 0.
        assert_eq!(root_index_key(&tree, 0), 0);
        assert_eq!(tree.lookup(&f.ext4, 0).unwrap().unwrap().start(), 600_000);
        assert_matches_linear(&f, &tree, LEAF_MAX as u32 * 2 + 8);
    }

    /// Filling four dense leaves fills the inline root; the next append grows
    /// the tree to depth 2 (the old root's content moves into a fresh interior
    /// node) and the split cascade lands in it.
    #[ktest]
    fn grow_indepth_on_full_root() {
        let f = Ext4FixtureBuilder::new(16384, 256, 16384)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let full = INLINE_MAX as u32 * LEAF_MAX as u32; // 4 × 340 = 1360
        let mut tree = ascending_tree(&f, full);
        assert_eq!((tree.depth(), root_entries(&tree)), (1, INLINE_MAX as u16));

        let sc = tree.sector_count();
        tree.insert(
            &f.ext4,
            full * 2,
            700_000,
            1,
            ExtentKind::Written,
            None,
            None,
        )
        .unwrap();
        // Depth grew; the root now holds the single index entry to the copied
        // old root; the appended extent landed in a fresh dense leaf.
        assert_eq!((tree.depth(), root_entries(&tree)), (2, 1));
        // Two fresh metadata blocks (the copied-down root + the new leaf) plus
        // one data block.
        assert_eq!(tree.sector_count(), sc + 3 * SECTORS_PER_BLOCK);
        let m = tree.lookup(&f.ext4, full * 2).unwrap().unwrap();
        assert_eq!(m.start(), 700_000);

        // Spot probes across the whole space agree with the linear reference
        // (a full probe over 2700+ blocks would dominate the suite runtime).
        for ib in (0..full * 2 + 4).step_by(7) {
            let linear = tree.lookup_linear(&f.ext4, ib).unwrap().map(|e| e.block());
            let path = tree.lookup(&f.ext4, ib).unwrap().map(|e| e.block());
            assert_eq!(linear, path, "mismatch at block {ib}");
        }

        // And the tree keeps absorbing appends after the growth.
        for i in 1..8u32 {
            tree.insert(
                &f.ext4,
                (full + i) * 2,
                700_000 + i as Ext4Bid * 2,
                1,
                ExtentKind::Written,
                None,
                None,
            )
            .unwrap();
        }
        assert_eq!(tree.depth(), 2);
        let m = tree.lookup(&f.ext4, (full + 7) * 2).unwrap().unwrap();
        assert_eq!(m.start(), 700_014);
    }

    /// Splits under a live journal: every touched node (fresh and old) rides
    /// the transaction's captures, and post-flush lookups read the split tree
    /// back consistently through the journal funnel. The commit thread stays
    /// alive — 345 ops outgrow any fixture journal without its space reclaim,
    /// and reads must stay correct across whatever running / committing /
    /// checkpointed mix results.
    #[ktest]
    fn journaled_split_reads_back_consistently() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .with_journal_inode(256)
            .build()
            .unwrap();
        let journal = f.ext4.journal().unwrap();

        let mut tree = ExtentTree::empty();
        let n = LEAF_MAX as u32 + 5; // forces one journaled split near the end
        for i in 0..n {
            let op = f.ext4.begin_op(8).unwrap();
            tree.insert(
                &f.ext4,
                i * 2,
                200_000 + i as Ext4Bid * 2,
                1,
                ExtentKind::Written,
                op.get(),
                None,
            )
            .unwrap();
        }
        assert_eq!((tree.depth(), root_entries(&tree)), (1, 2));

        // Reads through the journal stations see the split tree…
        assert_matches_linear(&f, &tree, n * 2 + 4);
        // …and after commit + checkpoint the device is authoritative and
        // still agrees.
        journal.flush_on_unmount().unwrap();
        assert_matches_linear(&f, &tree, n * 2 + 4);
    }

    // ---- P9a-T3 review findings, pinned as regressions ----

    /// `try_new` rejects an inline root whose `entries`/`max` exceed the
    /// `i_block` capacity (INLINE_MAX). Without the bound the root scanners
    /// would index past the 60-byte `i_block` on a crafted image — a release
    /// slice panic, not the fail-loud EUCLEAN a corrupt mount owes.
    #[ktest]
    fn try_new_rejects_oversized_inline_root() {
        let mut root = [0u32; RAW_BLOCK_PTRS_LEN];
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: 5, // > INLINE_MAX (4): would overrun i_block at entry 4
            max: 5,
            depth: 0,
            generation: 0,
        };
        root.as_mut_bytes()[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        assert!(ExtentTree::try_new(root, 0).is_err());
    }

    /// A depth-1 root pointing at an entries==0 interior node is rejected at
    /// the `NodeBuf::read` parse boundary (a childless interior is corruption);
    /// the walker never reads its phantom entry-0 bytes.
    #[ktest]
    fn find_rejects_empty_interior_child() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048).build().unwrap();
        // A depth-2 root would descend through an interior node; craft that
        // interior as empty (entries=0, depth=1).
        let empty_interior = 220u32;
        let mut block = [0u8; BLOCK_SIZE];
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: 0,
            max: INTERIOR_MAX as u16,
            depth: 1,
            generation: 0,
        };
        block[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        f.write_data_block(empty_interior, &block);

        // A depth-2 index root whose single child is that empty interior.
        let mut rootb = [0u32; RAW_BLOCK_PTRS_LEN];
        let rh = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: 1,
            max: 4,
            depth: 2,
            generation: 0,
        };
        rootb.as_mut_bytes()[0..ENTRY_SIZE].copy_from_slice(rh.as_bytes());
        let idx = RawExtentIdx {
            block: 0,
            leaf_lo: empty_interior,
            leaf_hi: 0,
            unused: 0,
        };
        rootb.as_mut_bytes()[ENTRY_SIZE..2 * ENTRY_SIZE].copy_from_slice(idx.as_bytes());
        let tree = ExtentTree::try_new(rootb, 0).unwrap();
        assert!(tree.find(&f.ext4, 0).is_err());
    }

    /// A before-first insert into a full leaf splits keeping the old leaf's
    /// original first entry (Linux `at.max(1)`): the old leaf never empties
    /// and the fresh node's parent key is strictly greater, so no duplicate
    /// index key is ever produced. The new extent lands in the old leaf and
    /// corrects the root key downward.
    #[ktest]
    fn before_first_split_keeps_old_leaf_nonempty() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        // A full leaf whose first block is 10 (so 0..10 is a before-first hole).
        let mut tree = ExtentTree::empty();
        for i in 0..LEAF_MAX as u32 {
            tree.insert(
                &f.ext4,
                10 + i * 2,
                800_000 + i as Ext4Bid * 2,
                1,
                ExtentKind::Written,
                None,
                None,
            )
            .unwrap();
        }
        assert_eq!((tree.depth(), root_entries(&tree)), (1, 1));
        assert_eq!(root_index_key(&tree, 0), 10);

        tree.insert(&f.ext4, 0, 900_000, 1, ExtentKind::Written, None, None)
            .unwrap();
        // Split happened; the two root index keys are strictly ordered (no
        // duplicate), the first dropped to the new extent's block.
        assert_eq!((tree.depth(), root_entries(&tree)), (1, 2));
        assert_eq!(root_index_key(&tree, 0), 0);
        assert!(root_index_key(&tree, 0) < root_index_key(&tree, 1));
        // Both leaves are non-empty (the old leaf kept its first entry).
        for probe in [0u32, 10] {
            match tree.find(&f.ext4, probe).unwrap() {
                Search::Covered { path, .. } => {
                    assert!(path.leaf().unwrap().node.entries() >= 1);
                }
                _ => panic!("block {probe} must be covered"),
            }
        }
        assert_eq!(tree.lookup(&f.ext4, 0).unwrap().unwrap().start(), 900_000);
        assert_matches_linear(&f, &tree, LEAF_MAX as u32 * 2 + 12);
    }

    // ---- P9a-T4a: in-place tail truncate ----

    /// Like [`ascending_tree`], but the data blocks are REALLY allocated from
    /// the fixture's bitmap — a truncate test frees them, and freeing a
    /// fictional block panics the group lookup. Returns the per-extent
    /// physical blocks for mapping assertions.
    fn ascending_tree_allocated(
        f: &crate::fs::fs_impls::ext4::test_utils::Ext4Fixture,
        n: u32,
    ) -> (ExtentTree, Vec<Ext4Bid>) {
        let mut tree = ExtentTree::empty();
        let mut pblocks = Vec::with_capacity(n as usize);
        for i in 0..n {
            let range = f.ext4.alloc_blocks(1, 0, None).unwrap();
            let pb = range.start;
            tree.insert(&f.ext4, i * 2, pb, 1, ExtentKind::Written, None, None)
                .unwrap();
            pblocks.push(pb);
        }
        (tree, pblocks)
    }

    /// Truncating a depth-2 tree frees the doomed tail extents in place and
    /// prunes the emptied leaves and interior nodes (Linux rm_leaf/rm_idx
    /// cascade), leaving the survivor prefix mapped and the tree still valid.
    #[ktest]
    fn truncate_prunes_depth2_cascade() {
        let f = Ext4FixtureBuilder::new(16384, 256, 16384)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        // Grow to depth 2: > INLINE_MAX × LEAF_MAX = 1360 unmergeable extents.
        let n = INLINE_MAX as u32 * LEAF_MAX as u32 + 200; // 1560
        let (mut tree, pblocks) = ascending_tree_allocated(&f, n);
        assert_eq!(tree.depth(), 2);
        let sc_full = tree.sector_count();

        // Truncate to keep only the first 100 blocks (50 extents at even
        // blocks). The doomed tail spans many leaves and whole interiors.
        let keep = 100usize;
        tree.truncate_to_byte_len(
            &f.ext4,
            keep * BLOCK_SIZE,
            None,
            None,
            journal::DataForgetPolicy::PlainData,
        )
        .unwrap();

        // Survivor prefix reads correctly; the freed tail is holes.
        for b in (0..100u32).step_by(2) {
            let m = tree.lookup(&f.ext4, b).unwrap().unwrap();
            assert_eq!(m.start(), pblocks[(b / 2) as usize]);
        }
        assert!(tree.lookup(&f.ext4, 100).unwrap().is_none());
        assert!(tree.lookup(&f.ext4, n * 2 - 2).unwrap().is_none());
        assert_matches_linear(&f, &tree, 130);
        // i_blocks dropped: 50 survivor data blocks + a handful of surviving
        // metadata nodes, far below the full tree.
        assert!(tree.sector_count() < sc_full);
        assert!(tree.sector_count() >= 50 * SECTORS_PER_BLOCK);
        // The tree stays valid and re-truncatable to zero.
        tree.truncate_to_byte_len(&f.ext4, 0, None, None, journal::DataForgetPolicy::PlainData)
            .unwrap();
        assert_eq!((tree.depth(), root_entries(&tree)), (0, 0));
        assert_eq!(tree.sector_count(), 0);
        assert!(tree.lookup(&f.ext4, 0).unwrap().is_none());
    }

    /// Truncating a depth-1 tree to zero frees every leaf and resets the root
    /// to an empty inline depth-0 node.
    #[ktest]
    fn truncate_to_zero_resets_root() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let (mut tree, _pblocks) = ascending_tree_allocated(&f, LEAF_MAX as u32 + 50); // depth 1
        assert_eq!(tree.depth(), 1);
        tree.truncate_to_byte_len(&f.ext4, 0, None, None, journal::DataForgetPolicy::PlainData)
            .unwrap();
        assert_eq!((tree.depth(), root_entries(&tree)), (0, 0));
        assert_eq!(tree.sector_count(), 0);
        // Reusable: inserts after a full truncate rebuild normally.
        tree.insert(&f.ext4, 0, 5000, 3, ExtentKind::Written, None, None)
            .unwrap();
        let m = tree.lookup(&f.ext4, 1).unwrap().unwrap();
        assert_eq!((m.block(), m.len(), m.start()), (0, 3, 5000));
    }

    /// A chunked (credit-bounded) truncate frees a bounded batch per call and
    /// reports a frontier the outer spine drives down; the survivor after each
    /// chunk stays consistent with the linear reference.
    #[ktest]
    fn truncate_chunk_frontier_and_consistency() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let n = LEAF_MAX as u32 + 100; // depth 1, 2 leaves
        let (mut tree, _pblocks) = ascending_tree_allocated(&f, n);

        // Drive chunks with a tiny per-chunk budget until the frontier reaches
        // keep_blocks, mimicking the outer restart spine (sans journal).
        let keep = 10usize;
        let keep_blocks = keep as Iblock;
        let mut guard = 0;
        loop {
            let chunk = tree
                .truncate_chunk(
                    &f.ext4,
                    keep * BLOCK_SIZE,
                    None,
                    None,
                    journal::DataForgetPolicy::PlainData,
                    Some(64),
                )
                .unwrap();
            // After each chunk the tree is a valid survivor.
            assert_matches_linear(&f, &tree, n * 2 + 4);
            if chunk.reached <= keep_blocks {
                break;
            }
            guard += 1;
            assert!(guard < 10_000, "truncate frontier must converge");
        }
        // Kept prefix survives, tail is holes.
        for b in (0..keep as u32).step_by(2) {
            assert!(tree.lookup(&f.ext4, b).unwrap().is_some());
        }
        assert!(tree.lookup(&f.ext4, keep as u32).unwrap().is_none());
    }

    // ---- P9a-T4b: in-place punch ----

    /// Punching the middle of an inline (depth-0) extent splits it into head +
    /// tail in the root and frees exactly the covered run.
    #[ktest]
    fn punch_inline_middle_splits() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        let range = f.ext4.alloc_blocks(20, 0, None).unwrap();
        let p = range.start;
        let got = (range.end - range.start) as u16;
        assert_eq!(got, 20, "fixture must satisfy a 20-block run");
        tree.insert(&f.ext4, 0, p, 20, ExtentKind::Written, None, None)
            .unwrap();
        let sc = tree.sector_count();

        tree.punch_chunk(
            &f.ext4,
            5..9,
            None,
            None,
            journal::DataForgetPolicy::PlainData,
            None,
        )
        .unwrap();
        let head = tree.lookup(&f.ext4, 4).unwrap().unwrap();
        assert_eq!((head.block(), head.len(), head.start()), (0, 5, p));
        assert!(tree.lookup(&f.ext4, 5).unwrap().is_none());
        assert!(tree.lookup(&f.ext4, 8).unwrap().is_none());
        let tail = tree.lookup(&f.ext4, 9).unwrap().unwrap();
        assert_eq!((tail.block(), tail.len(), tail.start()), (9, 11, p + 9));
        assert_eq!(tree.sector_count(), sc - 4 * SECTORS_PER_BLOCK);
    }

    /// A depth-0 root already holding [`INLINE_MAX`] extents overflows when a
    /// punch splits one of them; the tail re-enters through insert and the
    /// tree grows to depth 1 with every mapping intact.
    #[ktest]
    fn punch_inline_overflow_grows_tree() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        let mut runs = Vec::new();
        for i in 0..INLINE_MAX as u32 {
            let r = f.ext4.alloc_blocks(6, 0, None).unwrap();
            assert_eq!((r.end - r.start) as u16, 6);
            tree.insert(&f.ext4, i * 10, r.start, 6, ExtentKind::Written, None, None)
                .unwrap();
            runs.push(r.start);
        }
        assert_eq!(tree.depth(), 0);

        // Punch the middle of extent #1: head+tail push the root past
        // INLINE_MAX → the tail re-insert grows the tree.
        tree.punch_chunk(
            &f.ext4,
            12..14,
            None,
            None,
            journal::DataForgetPolicy::PlainData,
            None,
        )
        .unwrap();
        assert_eq!(tree.depth(), 1);
        let head = tree.lookup(&f.ext4, 11).unwrap().unwrap();
        assert_eq!((head.block(), head.len()), (10, 2));
        assert!(tree.lookup(&f.ext4, 12).unwrap().is_none());
        let tail = tree.lookup(&f.ext4, 14).unwrap().unwrap();
        assert_eq!(
            (tail.block(), tail.len(), tail.start()),
            (14, 2, runs[1] + 4)
        );
        // Untouched neighbours survive.
        for i in [0u32, 2, 3] {
            let m = tree.lookup(&f.ext4, i * 10 + 1).unwrap().unwrap();
            assert_eq!(m.start(), runs[i as usize]);
        }
        assert_matches_linear(&f, &tree, 40);
    }

    /// Punching the head of a non-first leaf removes its first entries; the
    /// leaf's risen first key must propagate into the parent index (Linux's
    /// exact-first-key invariant), and lookups stay correct.
    #[ktest]
    fn punch_leaf_head_corrects_parent_key() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let n = LEAF_MAX as u32 + 30;
        let (mut tree, _p) = ascending_tree_allocated(&f, n);
        assert_eq!((tree.depth(), root_entries(&tree)), (1, 2));
        let leaf2_first = LEAF_MAX as u32 * 2; // block 680
        assert_eq!(root_index_key(&tree, 1), leaf2_first);

        // Punch the second leaf's first 5 extents: [680, 690).
        tree.punch_chunk(
            &f.ext4,
            leaf2_first..leaf2_first + 10,
            None,
            None,
            journal::DataForgetPolicy::PlainData,
            None,
        )
        .unwrap();
        // The parent key follows the leaf's new first entry exactly.
        assert_eq!(root_index_key(&tree, 1), leaf2_first + 10);
        assert!(tree.lookup(&f.ext4, leaf2_first).unwrap().is_none());
        assert!(tree.lookup(&f.ext4, leaf2_first + 10).unwrap().is_some());
        assert_matches_linear(&f, &tree, n * 2 + 4);
    }

    /// A punch that covers a whole leaf's range empties and prunes it from the
    /// middle of the tree; the sibling leaves survive untouched.
    #[ktest]
    fn punch_empties_and_prunes_middle_leaf() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let n = LEAF_MAX as u32 + 30;
        let (mut tree, _p) = ascending_tree_allocated(&f, n);
        assert_eq!((tree.depth(), root_entries(&tree)), (1, 2));
        let sc = tree.sector_count();

        // Punch the ENTIRE first leaf's range [0, 680).
        tree.punch_chunk(
            &f.ext4,
            0..LEAF_MAX as u32 * 2,
            None,
            None,
            journal::DataForgetPolicy::PlainData,
            None,
        )
        .unwrap();
        assert_eq!((tree.depth(), root_entries(&tree)), (1, 1));
        assert!(tree.lookup(&f.ext4, 0).unwrap().is_none());
        assert!(tree.lookup(&f.ext4, 678).unwrap().is_none());
        assert!(tree.lookup(&f.ext4, LEAF_MAX as u32 * 2).unwrap().is_some());
        // 340 data blocks + the pruned leaf node left i_blocks.
        assert_eq!(
            tree.sector_count(),
            sc - (LEAF_MAX as u64 + 1) * SECTORS_PER_BLOCK
        );
        assert_matches_linear(&f, &tree, n * 2 + 4);
    }

    /// Punching inside one extent of a FULL leaf takes the slow spans-both
    /// path: trim to the head, re-insert the tail through the split machinery
    /// (the leaf splits), every mapping intact.
    #[ktest]
    fn punch_full_leaf_spans_both_splits() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        // 339 singles + one 5-block run = exactly LEAF_MAX entries, depth 1.
        let (mut tree, _p) = ascending_tree_allocated(&f, LEAF_MAX as u32 - 1);
        let big = f.ext4.alloc_blocks(5, 0, None).unwrap();
        assert_eq!((big.end - big.start) as u16, 5);
        let big_block = (LEAF_MAX as u32 - 1) * 2; // 678, covers 678..683
        tree.insert(
            &f.ext4,
            big_block,
            big.start,
            5,
            ExtentKind::Written,
            None,
            None,
        )
        .unwrap();
        assert_eq!((tree.depth(), root_entries(&tree)), (1, 1));
        match tree.find(&f.ext4, big_block).unwrap() {
            Search::Covered { path, .. } => {
                assert_eq!(path.leaf().unwrap().node.entries(), LEAF_MAX)
            }
            _ => panic!("big extent must be mapped"),
        }

        // Punch [679, 681): head 678..679, tail 681..683, leaf FULL → split.
        tree.punch_chunk(
            &f.ext4,
            big_block + 1..big_block + 3,
            None,
            None,
            journal::DataForgetPolicy::PlainData,
            None,
        )
        .unwrap();
        assert_eq!((tree.depth(), root_entries(&tree)), (1, 2));
        let head = tree.lookup(&f.ext4, big_block).unwrap().unwrap();
        assert_eq!(
            (head.block(), head.len(), head.start()),
            (big_block, 1, big.start)
        );
        assert!(tree.lookup(&f.ext4, big_block + 1).unwrap().is_none());
        assert!(tree.lookup(&f.ext4, big_block + 2).unwrap().is_none());
        let tail = tree.lookup(&f.ext4, big_block + 3).unwrap().unwrap();
        assert_eq!(
            (tail.block(), tail.len(), tail.start()),
            (big_block + 3, 2, big.start + 3)
        );
        assert_matches_linear(&f, &tree, big_block + 8);
    }

    /// P9 debt `punch-spans-slow-headroom-shape`: a spans-both punch through a
    /// FULL leaf re-inserts the tail via the split machinery, whose worst case
    /// is `insert_credit_bound` (the `3*(depth+1)+2` split shape), not the
    /// smaller `write_credits`. The per-step EFBIG floor must reserve the larger
    /// bound, so a journal big enough only for the retired (under-counted)
    /// estimate refuses the step up front instead of admitting it and
    /// overrunning mid-punch.
    #[ktest]
    fn punch_spans_slow_floor_reserves_insert_bound() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        // A full depth-1 leaf ending in a 5-block run (the spans-both shape).
        let (mut tree, _p) = ascending_tree_allocated(&f, LEAF_MAX as u32 - 1);
        let big = f.ext4.alloc_blocks(5, 0, None).unwrap();
        let big_block = (LEAF_MAX as u32 - 1) * 2;
        tree.insert(
            &f.ext4,
            big_block,
            big.start,
            5,
            ExtentKind::Written,
            None,
            None,
        )
        .unwrap();
        assert_eq!((tree.depth(), root_entries(&tree)), (1, 1));
        match tree.find(&f.ext4, big_block).unwrap() {
            Search::Covered { path, .. } => {
                assert_eq!(path.leaf().unwrap().node.entries(), LEAF_MAX)
            }
            _ => panic!("big extent must be mapped"),
        }

        // The spans-both step's reservation, both ways (depth 1).
        let depth = 1u16;
        let free_cost = f.ext4.extent_free_credits();
        let node_headroom = free_cost * depth as usize + 2;
        // Retired undercount (2*(depth+1) single-map shape).
        let old_need = free_cost + node_headroom + f.ext4.write_credits(depth);
        // Correct bound (3*(depth+1)+2 split shape).
        let new_need = free_cost + node_headroom + f.ext4.insert_credit_bound(depth);
        assert!(
            new_need > old_need,
            "the fix must raise the reservation: {new_need} vs {old_need}"
        );

        // A journal sized at the OLD need: the retired code admitted the step and
        // would overrun; the fix stops it with a clean EFBIG before any free, so
        // the leaf is left intact. (`PunchChunk` is not `Debug`, so match rather
        // than `unwrap_err`.)
        let err = match tree.punch_chunk(
            &f.ext4,
            big_block + 1..big_block + 3,
            None,
            None,
            journal::DataForgetPolicy::PlainData,
            Some(old_need),
        ) {
            Ok(_) => panic!("a step sized only for the retired estimate must be refused"),
            Err(e) => e,
        };
        assert_eq!(err.error(), Errno::EFBIG);
        match tree.find(&f.ext4, big_block).unwrap() {
            Search::Covered { path, .. } => {
                assert_eq!(path.leaf().unwrap().node.entries(), LEAF_MAX)
            }
            _ => panic!("a floor refusal must leave the leaf intact"),
        }
    }

    // ---- P9a-T5: in-place unwritten→written conversion ----

    /// Converting a whole unwritten extent flips its kind in place (no data
    /// move, no metadata delta) and coalesces with a contiguous written
    /// neighbour — the sequential-write pattern that must not fragment.
    #[ktest]
    fn convert_full_extent_flips_and_merges() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        // A single unwritten run [0,4) @ 100.
        tree.insert(&f.ext4, 0, 100, 4, ExtentKind::Unwritten, None, None)
            .unwrap();
        let (extents, _) = tree.flatten(&f.ext4).unwrap();
        assert_eq!(extents.len(), 1);
        let sc = tree.sector_count();

        // Convert the first half [0,2): partial → head unwritten + written mid.
        tree.convert_unwritten(&f.ext4, 0, 2, None, None).unwrap();
        assert!(!tree.lookup(&f.ext4, 0).unwrap().unwrap().is_unwritten());
        assert!(!tree.lookup(&f.ext4, 1).unwrap().unwrap().is_unwritten());
        assert!(tree.lookup(&f.ext4, 2).unwrap().unwrap().is_unwritten());
        // Now convert [2,4): the flipped middle must MERGE with [0,2) written.
        tree.convert_unwritten(&f.ext4, 2, 2, None, None).unwrap();
        let m = tree.lookup(&f.ext4, 3).unwrap().unwrap();
        assert_eq!((m.block(), m.len(), m.start()), (0, 4, 100));
        assert!(!m.is_unwritten());
        let (extents, _) = tree.flatten(&f.ext4).unwrap();
        assert_eq!(extents.len(), 1, "fully converted run must coalesce to one");
        // Pure kind flips: no i_blocks change (no data or metadata moved).
        assert_eq!(tree.sector_count(), sc);
        assert_matches_linear(&f, &tree, 8);
    }

    /// Converting the MIDDLE of an unwritten extent produces head-unwritten +
    /// written-middle + tail-unwritten, all mapping the same physical run.
    #[ktest]
    fn convert_middle_splits_three_ways() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        tree.insert(&f.ext4, 0, 500, 10, ExtentKind::Unwritten, None, None)
            .unwrap();

        // Convert [3,7): head [0,3)U, mid [3,7)W, tail [7,10)U — same phys.
        tree.convert_unwritten(&f.ext4, 3, 4, None, None).unwrap();
        for b in 0..3 {
            let m = tree.lookup(&f.ext4, b).unwrap().unwrap();
            assert!(m.is_unwritten() && m.start() + (b - m.block()) as u64 == 500 + b as u64);
        }
        for b in 3..7 {
            let m = tree.lookup(&f.ext4, b).unwrap().unwrap();
            assert!(!m.is_unwritten() && m.start() + (b - m.block()) as u64 == 500 + b as u64);
        }
        for b in 7..10 {
            let m = tree.lookup(&f.ext4, b).unwrap().unwrap();
            assert!(m.is_unwritten() && m.start() + (b - m.block()) as u64 == 500 + b as u64);
        }
        let (extents, _) = tree.flatten(&f.ext4).unwrap();
        assert_eq!(extents.len(), 3);
        assert_matches_linear(&f, &tree, 14);
    }

    /// Converting a range that spans a FULL leaf's worth of unwritten extents
    /// edits each covering leaf in place across a depth-1 tree, and the write
    /// path's every-write gate stays a no-op on an all-written range.
    #[ktest]
    fn convert_across_leaves_and_noop_gate() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        // 400 unwritten singletons at even blocks → depth 1, 2 leaves.
        for i in 0..400u32 {
            tree.insert(
                &f.ext4,
                i * 2,
                10_000 + i as Ext4Bid,
                1,
                ExtentKind::Unwritten,
                None,
                None,
            )
            .unwrap();
        }
        assert_eq!(tree.depth(), 1);

        // Convert a swath crossing the leaf boundary (extents ~330..350).
        tree.convert_unwritten(&f.ext4, 660, 40, None, None)
            .unwrap();
        for i in 330..350u32 {
            assert!(!tree.lookup(&f.ext4, i * 2).unwrap().unwrap().is_unwritten());
        }
        // Neighbours untouched.
        assert!(
            tree.lookup(&f.ext4, 329 * 2)
                .unwrap()
                .unwrap()
                .is_unwritten()
        );
        assert!(
            tree.lookup(&f.ext4, 350 * 2)
                .unwrap()
                .unwrap()
                .is_unwritten()
        );
        assert_matches_linear(&f, &tree, 810);

        // The every-write gate: an all-written range converts to a clean no-op
        // (already-written blocks, nothing to flip) without dirtying.
        tree.clear_dirty();
        tree.convert_unwritten(&f.ext4, 660, 40, None, None)
            .unwrap();
        assert!(!tree.is_dirty(), "re-converting a written range is a no-op");
    }

    /// Conversion under a live journal reads back consistently before and
    /// after commit + checkpoint. The tree is depth-1, so the conversions
    /// edit EXTERNAL leaves whose newest bytes travel through the WAL funnel
    /// (the capture suppresses the direct write until checkpoint) — an
    /// inline-root tree would never exercise that path.
    #[ktest]
    fn journaled_convert_reads_back_consistently() {
        crate::time::clocks::init_for_ktest();
        let f = Ext4FixtureBuilder::new(4096, 256, 4096)
            .with_block_bitmap_metadata_marked()
            .with_journal_inode(128)
            .build()
            .unwrap();
        let journal = f.ext4.journal().unwrap();

        let mut tree = ExtentTree::empty();
        {
            let op = f.ext4.begin_op(16).unwrap();
            // Six unmergeable unwritten extents overflow the inline root →
            // depth 1: the conversions below edit an external leaf.
            for (b, p) in [
                (10, 1000),
                (20, 2000),
                (30, 3000),
                (40, 4000),
                (50, 5000),
                (60, 6000),
            ] {
                tree.insert(&f.ext4, b, p, 2, ExtentKind::Unwritten, op.get(), None)
                    .unwrap();
            }
        }
        assert_eq!(tree.depth(), 1);
        {
            let op = f.ext4.begin_op(16).unwrap();
            // A full flip ([20,22)) and a split ([31,32) → head + flipped mid).
            tree.convert_unwritten(&f.ext4, 20, 2, op.get(), None)
                .unwrap();
            tree.convert_unwritten(&f.ext4, 31, 1, op.get(), None)
                .unwrap();
        }
        let check = |tree: &ExtentTree| {
            assert!(!tree.lookup(&f.ext4, 21).unwrap().unwrap().is_unwritten());
            assert!(tree.lookup(&f.ext4, 30).unwrap().unwrap().is_unwritten());
            let m = tree.lookup(&f.ext4, 31).unwrap().unwrap();
            assert!(!m.is_unwritten());
            assert_eq!((m.block(), m.len(), m.start()), (31, 1, 3001));
            assert!(tree.lookup(&f.ext4, 41).unwrap().unwrap().is_unwritten());
        };
        // Through the journal stations (captures live, direct writes
        // suppressed)…
        check(&tree);
        // …and from the device after commit + checkpoint.
        journal.flush_on_unmount().unwrap();
        check(&tree);
    }

    // ---- P9b b-ext: the external-node cache (1a) ----

    /// The cache's first rule (refresh): priming a slot with a read, then
    /// editing that leaf (a `convert_unwritten` whose `write_back` refreshes the
    /// slot), then reading again must observe the WRITTEN bytes. A stale slot
    /// would replay the primed-unwritten node — a data-corruption-class bug this
    /// nails on observable content, independent of the debug coherence net that
    /// also cross-checks every hit.
    #[ktest]
    fn node_cache_refreshes_on_writeback() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        // Five unmergeable unwritten extents overflow the inline root → depth 1,
        // so the conversion edits (and the reads walk) an EXTERNAL leaf — the
        // only node kind the cache holds.
        for (b, p) in [
            (10, 1000),
            (40, 4000),
            (100, 7000),
            (200, 8000),
            (300, 9000),
        ] {
            tree.insert(&f.ext4, b, p, 10, ExtentKind::Unwritten, None, None)
                .unwrap();
        }
        assert_eq!(tree.depth(), 1);

        // Prime: this read walks the covering external leaf into a cache slot.
        assert!(tree.lookup(&f.ext4, 105).unwrap().unwrap().is_unwritten());
        // Edit in place; the leaf's `write_back` must refresh the primed slot.
        tree.convert_unwritten(&f.ext4, 100, 10, None, None)
            .unwrap();
        // The cached slot must now serve the WRITTEN bytes, not the stale ones.
        assert!(!tree.lookup(&f.ext4, 105).unwrap().unwrap().is_unwritten());
        assert_matches_linear(&f, &tree, 320);
    }

    /// A depth-2 descent alternates interior + leaf slots: walking each of the
    /// five leaves under the shared interior keeps that interior resident while
    /// the leaves rotate through the remaining slots (with [`NODE_CACHE_SLOTS`]
    /// = 8 the whole working set stays resident), and every verdict still
    /// matches the linear reference (the debug net cross-checks each hit for
    /// byte equality on top).
    #[ktest]
    fn node_cache_holds_interior_and_leaf_across_depth2() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        const N: u32 = 1400; // > INLINE_MAX × LEAF_MAX (1360) → depth 2
        const DATA_BASE: Ext4Bid = 100_000;
        for k in 0..N {
            tree.insert(
                &f.ext4,
                k * 2,
                DATA_BASE + k as Ext4Bid,
                1,
                ExtentKind::Written,
                None,
                None,
            )
            .unwrap();
        }
        assert_eq!(tree.depth(), 2);

        // Probe one block from each of the five leaves, twice around, so the
        // shared interior is re-hit while the leaf slot turns over. Every probe
        // resolves correctly (a stale interior or leaf slot would diverge here
        // or trip the debug net).
        let leaf_reps = [0u32, 340, 680, 1020, 1360];
        for _ in 0..2 {
            for &k in &leaf_reps {
                let m = tree.lookup(&f.ext4, k * 2).unwrap().unwrap();
                assert_eq!((m.block(), m.start()), (k * 2, DATA_BASE + k as Ext4Bid));
                assert!(tree.lookup(&f.ext4, k * 2 + 1).unwrap().is_none());
            }
        }
    }

    /// The cache's second rule (invalidation): a whole-tree rebuild
    /// (`reserialize`, reached here through `collapse_range`) REUSES external
    /// leaf blocks in place through `write_leaf_node` — a writer that does NOT
    /// pass through the slot-refreshing `write_back` — so the rebuild must clear
    /// the cache, or a later read would replay a reused leaf's pre-rebuild
    /// bytes. Prime a slot with the leaf, collapse, and confirm the shifted
    /// mapping is what the read returns.
    #[ktest]
    fn node_cache_invalidated_by_reserialize_reuse() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let n = LEAF_MAX as u32 + 20; // depth 1, ≥ 2 external leaves
        let (mut tree, pblocks) = ascending_tree_allocated(&f, n);
        assert_eq!(tree.depth(), 1);

        // Prime the cache with the first leaf: block 0 maps `pblocks[0]`.
        assert_eq!(
            tree.lookup(&f.ext4, 0).unwrap().unwrap().start(),
            pblocks[0]
        );
        // Collapse [0,2): reserialize rewrites the reused leaves with shifted
        // keys (logical 0 now maps what was logical 2 = `pblocks[1]`).
        tree.collapse_range(
            &f.ext4,
            0,
            2,
            None,
            None,
            journal::DataForgetPolicy::PlainData,
        )
        .unwrap();
        // The reused leaf's cache slot must not serve its stale pre-shift bytes.
        assert_eq!(
            tree.lookup(&f.ext4, 0).unwrap().unwrap().start(),
            pblocks[1]
        );
        assert_matches_linear(&f, &tree, n * 2);
    }

    /// A depth-1 partial conversion splits inside the external leaf: the
    /// middle of an unwritten extent becomes head-U + mid-W + tail-U in
    /// staged single-leaf edits, preserving the physical mapping block for
    /// block and — with no leaf reorganization — leaving `i_blocks` untouched.
    #[ktest]
    fn depth1_convert_middle_splits_in_leaf() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        // Five unmergeable extents overflow the inline root → depth 1; the
        // target [100, 110) @ 7000 sits mid-leaf.
        for (b, p) in [
            (10, 1000),
            (40, 4000),
            (100, 7000),
            (200, 8000),
            (300, 9000),
        ] {
            tree.insert(&f.ext4, b, p, 10, ExtentKind::Unwritten, None, None)
                .unwrap();
        }
        assert_eq!(tree.depth(), 1);
        let sc = tree.sector_count();

        // Convert [103, 107): a three-way split over the same physical run.
        tree.convert_unwritten(&f.ext4, 103, 4, None, None).unwrap();
        for b in 100u32..110 {
            let m = tree.lookup(&f.ext4, b).unwrap().unwrap();
            assert_eq!(m.is_unwritten(), !(103..107).contains(&b));
            assert_eq!(m.start() + (b - m.block()) as u64, 7000 + (b - 100) as u64);
        }
        // The piece lengths pin the exact split (head 3, mid 4, tail 3).
        assert_eq!(tree.lookup(&f.ext4, 100).unwrap().unwrap().len(), 3);
        assert_eq!(tree.lookup(&f.ext4, 103).unwrap().unwrap().len(), 4);
        assert_eq!(tree.lookup(&f.ext4, 107).unwrap().unwrap().len(), 3);
        // No data or metadata block moved: `i_blocks` is unchanged.
        assert_eq!(tree.sector_count(), sc);
        assert_matches_linear(&f, &tree, 320);

        // Converting the tail then merges it back into the written middle.
        tree.convert_unwritten(&f.ext4, 107, 3, None, None).unwrap();
        let m = tree.lookup(&f.ext4, 106).unwrap().unwrap();
        assert_eq!((m.block(), m.len(), m.start()), (103, 7, 7003));
        assert!(!m.is_unwritten());
        assert_eq!(tree.sector_count(), sc);
        assert_matches_linear(&f, &tree, 320);
    }

    /// A partial conversion inside a FULL leaf reorganizes first
    /// (`make_room_for` on a covered landing — the one new metadata block is
    /// the only `i_blocks` change), then splits the entry in place.
    #[ktest]
    fn convert_in_full_leaf_reorganizes_then_splits() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        // LEAF_MAX unmergeable unwritten runs fill one depth-1 leaf exactly.
        for i in 0..LEAF_MAX as u32 {
            tree.insert(
                &f.ext4,
                i * 4,
                20_000 + (i as Ext4Bid) * 4,
                2,
                ExtentKind::Unwritten,
                None,
                None,
            )
            .unwrap();
        }
        assert_eq!(tree.depth(), 1);
        let sc = tree.sector_count();

        // [601, 602) splits its covering entry [600, 602): the full leaf
        // reorganizes, then the head trim + flip land in place.
        tree.convert_unwritten(&f.ext4, 601, 1, None, None).unwrap();
        let head = tree.lookup(&f.ext4, 600).unwrap().unwrap();
        assert!(head.is_unwritten());
        assert_eq!((head.block(), head.len(), head.start()), (600, 1, 20600));
        let mid = tree.lookup(&f.ext4, 601).unwrap().unwrap();
        assert!(!mid.is_unwritten());
        assert_eq!((mid.block(), mid.len(), mid.start()), (601, 1, 20601));
        // The reorganization allocated exactly one new leaf block.
        assert_eq!(tree.sector_count(), sc + SECTORS_PER_BLOCK);
        assert_matches_linear(&f, &tree, 1400);
    }

    /// `merge_leaf_neighbors` coalesces the edited entry with contiguous
    /// same-kind neighbours on both sides and refuses a kind mismatch (the
    /// length caps live in `can_merge`, pinned separately).
    #[ktest]
    fn merge_leaf_neighbors_coalesces_both_sides_and_respects_kind() {
        let es = es_token();
        let mut leaf = NodeBuf::fresh(999, 0);
        for (i, e) in [
            Extent::new(0, 2, 100, ExtentKind::Written),
            Extent::new(2, 2, 102, ExtentKind::Written),
            Extent::new(4, 2, 104, ExtentKind::Written),
        ]
        .iter()
        .enumerate()
        {
            leaf.insert_extent_at(i, e, &es).unwrap();
        }
        merge_leaf_neighbors(&mut leaf, 1, &es);
        assert_eq!(leaf.entries(), 1);
        let m = leaf.extent_at(0);
        assert_eq!((m.block(), m.len(), m.start()), (0, 6, 100));
        assert!(!m.is_unwritten());

        // A kind mismatch on either side refuses to merge.
        leaf.insert_extent_at(1, &Extent::new(6, 2, 106, ExtentKind::Unwritten), &es)
            .unwrap();
        leaf.insert_extent_at(2, &Extent::new(8, 2, 108, ExtentKind::Written), &es)
            .unwrap();
        merge_leaf_neighbors(&mut leaf, 1, &es);
        assert_eq!(
            leaf.entries(),
            3,
            "unwritten between written must not merge"
        );
    }

    /// Converting the middle of one extent in a FULL inline root overflows
    /// the inline capacity (4 entries + head + tail = 6): the tree rebuilds
    /// as depth-1 atomically (fresh leaf first, in-memory root flip last),
    /// keeping every mapping and counting exactly the one new leaf block.
    #[ktest]
    fn convert_overflowing_inline_root_grows_depth1() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        for (b, p) in [(0, 500), (10, 600), (20, 700), (30, 800)] {
            tree.insert(&f.ext4, b, p, 6, ExtentKind::Unwritten, None, None)
                .unwrap();
        }
        assert_eq!(tree.depth(), 0);
        let sc = tree.sector_count();

        // The middle of [10,16): head + mid + tail push 4 entries → 6.
        tree.convert_unwritten(&f.ext4, 12, 2, None, None).unwrap();
        assert_eq!(tree.depth(), 1);
        assert!(tree.lookup(&f.ext4, 11).unwrap().unwrap().is_unwritten());
        let m = tree.lookup(&f.ext4, 12).unwrap().unwrap();
        assert!(!m.is_unwritten());
        assert_eq!((m.block(), m.len(), m.start()), (12, 2, 602));
        assert!(tree.lookup(&f.ext4, 14).unwrap().unwrap().is_unwritten());
        // The one fresh leaf block is the only `i_blocks` change.
        assert_eq!(tree.sector_count(), sc + SECTORS_PER_BLOCK);
        assert_matches_linear(&f, &tree, 40);
    }

    // ---- P9a-T6: depth cap lifted, flatten retired ----

    /// A crafted depth-3 spine: with the growth cap lifted (T6) every
    /// consumer is path-based, so a tree deeper than the old flatten ceiling
    /// reads, edits, converts, and truncates through three index levels.
    #[ktest]
    fn depth3_spine_reads_edits_converts_and_truncates() {
        let f = Ext4FixtureBuilder::new(4096, 256, 4096)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let device = f.ext4.block_device();
        // Real allocations for the spine nodes and data runs: the truncate
        // below frees them, and the frees must hit genuinely set bitmap bits.
        let alloc = |n: u32| f.ext4.alloc_blocks(n, 0, None).unwrap().start;
        let (data0, data1) = (alloc(1), alloc(1));
        let (leaf_bid, mid_bid, top_bid) = (alloc(1), alloc(1), alloc(1));

        let es = es_token();
        let mut leaf = NodeBuf::fresh(leaf_bid, 0);
        leaf.insert_extent_at(0, &Extent::new(0, 1, data0, ExtentKind::Unwritten), &es)
            .unwrap();
        leaf.insert_extent_at(1, &Extent::new(2, 1, data1, ExtentKind::Written), &es)
            .unwrap();
        leaf.write_back(device.as_ref(), None, None, None).unwrap();
        let mut mid = NodeBuf::fresh(mid_bid, 1);
        mid.insert_index_at(0, &make_index_entry(0, leaf_bid))
            .unwrap();
        mid.write_back(device.as_ref(), None, None, None).unwrap();
        let mut top = NodeBuf::fresh(top_bid, 2);
        top.insert_index_at(0, &make_index_entry(0, mid_bid))
            .unwrap();
        top.write_back(device.as_ref(), None, None, None).unwrap();

        let mut root = [0u32; RAW_BLOCK_PTRS_LEN];
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: 1,
            max: INLINE_MAX as u16,
            depth: 3,
            generation: 0,
        };
        root.as_mut_bytes()[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        root.as_mut_bytes()[ENTRY_SIZE..2 * ENTRY_SIZE]
            .copy_from_slice(make_index_entry(0, top_bid).as_bytes());
        let mut tree = ExtentTree::try_new(root, 5 * SECTORS_PER_BLOCK).unwrap();
        assert_eq!(tree.depth(), 3);

        // Read through three index levels.
        assert!(tree.lookup(&f.ext4, 0).unwrap().unwrap().is_unwritten());
        assert!(tree.lookup(&f.ext4, 1).unwrap().is_none());
        assert!(!tree.lookup(&f.ext4, 2).unwrap().unwrap().is_unwritten());
        assert_matches_linear(&f, &tree, 8);

        // Edit in place at depth 3: insert, then convert.
        let data2 = alloc(1);
        tree.insert(&f.ext4, 4, data2, 1, ExtentKind::Written, None, None)
            .unwrap();
        assert_eq!(tree.lookup(&f.ext4, 4).unwrap().unwrap().start(), data2);
        tree.convert_unwritten(&f.ext4, 0, 1, None, None).unwrap();
        assert!(!tree.lookup(&f.ext4, 0).unwrap().unwrap().is_unwritten());
        assert_eq!(tree.depth(), 3);
        assert_matches_linear(&f, &tree, 8);

        // A truncate to zero prunes the whole three-level spine back to an
        // empty inline root and returns every counted block.
        tree.truncate_to_byte_len(&f.ext4, 0, None, None, journal::DataForgetPolicy::PlainData)
            .unwrap();
        assert_eq!(tree.depth(), 0);
        assert_eq!(tree.sector_count(), 0);
        assert!(tree.lookup(&f.ext4, 0).unwrap().is_none());
    }

    /// P9 debt `whole-truncate-gate-underestimate`: the whole-truncate routing
    /// gate must reserve against the tree's REAL external-node count, not a
    /// dense `ceil(extents / fanout)` lower bound. A depth-3 spine holding only
    /// two extents has THREE external nodes (top + mid + leaf); the retired
    /// dense `external_node_count(2)` returned 0 for a `<= INLINE_MAX`-extent
    /// tree, so `plan_shrink` would under-reserve and misroute this whole
    /// truncate to the single-transaction fast path on a tiny journal.
    #[ktest]
    fn plan_shrink_reserves_for_deep_sparse_tree() {
        let f = Ext4FixtureBuilder::new(4096, 256, 4096)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let device = f.ext4.block_device();
        let alloc = |n: u32| f.ext4.alloc_blocks(n, 0, None).unwrap().start;
        let (data0, data1) = (alloc(1), alloc(1));
        let (leaf_bid, mid_bid, top_bid) = (alloc(1), alloc(1), alloc(1));

        // Two extents (logical 0 and 2), well under INLINE_MAX, hung off a full
        // three-level spine — the shape the dense count is blind to.
        let es = es_token();
        let mut leaf = NodeBuf::fresh(leaf_bid, 0);
        leaf.insert_extent_at(0, &Extent::new(0, 1, data0, ExtentKind::Written), &es)
            .unwrap();
        leaf.insert_extent_at(1, &Extent::new(2, 1, data1, ExtentKind::Written), &es)
            .unwrap();
        leaf.write_back(device.as_ref(), None, None, None).unwrap();
        let mut mid = NodeBuf::fresh(mid_bid, 1);
        mid.insert_index_at(0, &make_index_entry(0, leaf_bid))
            .unwrap();
        mid.write_back(device.as_ref(), None, None, None).unwrap();
        let mut top = NodeBuf::fresh(top_bid, 2);
        top.insert_index_at(0, &make_index_entry(0, mid_bid))
            .unwrap();
        top.write_back(device.as_ref(), None, None, None).unwrap();

        let mut root = [0u32; RAW_BLOCK_PTRS_LEN];
        let header = RawExtentHeader {
            magic: EXTENT_MAGIC,
            entries: 1,
            max: INLINE_MAX as u16,
            depth: 3,
            generation: 0,
        };
        root.as_mut_bytes()[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
        root.as_mut_bytes()[ENTRY_SIZE..2 * ENTRY_SIZE]
            .copy_from_slice(make_index_entry(0, top_bid).as_bytes());

        // The exact structural count sees all three external nodes and both
        // doomed extents; the dense `external_node_count(2)` returned 0.
        let tree = ExtentTree::try_new(root, 5 * SECTORS_PER_BLOCK).unwrap();
        let shape = tree.shrink_shape(&f.ext4, 0).unwrap();
        assert_eq!((shape.external_nodes, shape.freed_extents), (3, 2));

        // `plan_shrink`'s estimate is independent of `max_credits` (that only
        // gates the chunked floor), so compare it against the dense-0 undercount
        // the retired formula produced (no journal here → `revoke_entries_per_block`
        // = 1). The exact count must reserve strictly more, so a tiny journal
        // sized at the dense estimate takes the chunked orphan spine instead of
        // an overrunning single transaction.
        let dense_est = f
            .ext4
            .whole_truncate_credit_bound(0, shape.freed_extents, 1);
        let em = super::super::ExtentManager::try_new(
            root,
            5 * SECTORS_PER_BLOCK,
            Arc::downgrade(&f.ext4),
            5,
            None,
            0,
            journal::DataForgetPolicy::PlainData,
        )
        .unwrap();
        let plan = em.plan_shrink(0, dense_est).unwrap();
        assert!(
            plan.whole_estimate > dense_est,
            "exact node count must overrun the dense-0 undercount: est={} dense={dense_est}",
            plan.whole_estimate,
        );
    }

    /// G9-8 (P9a-a5, the T5 review's ENOSPC pin): a partial conversion whose
    /// leaf reorganization hits a genuinely full disk (injected: the very
    /// next allocation fails) is a CLEAN error — nothing edited, the tree
    /// still maps every block with the target still unwritten. The old
    /// trim-then-reinsert design aborted the whole volume here.
    #[ktest]
    fn convert_in_full_leaf_enospc_is_clean() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        for i in 0..LEAF_MAX as u32 {
            tree.insert(
                &f.ext4,
                i * 4,
                20_000 + (i as Ext4Bid) * 4,
                2,
                ExtentKind::Unwritten,
                None,
                None,
            )
            .unwrap();
        }
        let sc = tree.sector_count();

        // The split needs one metadata block; the injected fault denies it.
        f.ext4.arm_alloc_blocks_enospc(0);
        let err = tree
            .convert_unwritten(&f.ext4, 601, 1, None, None)
            .unwrap_err();
        assert_eq!(err.error(), Errno::ENOSPC);

        // Nothing landed: same accounting, same mapping, still unwritten.
        assert_eq!(tree.sector_count(), sc);
        let m = tree.lookup(&f.ext4, 601).unwrap().unwrap();
        assert!(m.is_unwritten());
        assert_eq!((m.block(), m.len(), m.start()), (600, 2, 20600));
        assert_matches_linear(&f, &tree, 1400);

        // The fault disarmed: the conversion now succeeds.
        tree.convert_unwritten(&f.ext4, 601, 1, None, None).unwrap();
        assert!(!tree.lookup(&f.ext4, 601).unwrap().unwrap().is_unwritten());
    }

    /// A write into a `fallocate`d region on a genuinely full volume: the
    /// unwritten→written conversion splits a full leaf, needing a fresh tree
    /// node, yet the space was already reserved so the write must not fail
    /// `ENOSPC` (xfstests generic/274). The metadata reserve makes that split
    /// succeed where an ordinary allocation — held back from the reserve — is
    /// already `ENOSPC`.
    #[ktest]
    fn convert_split_draws_metadata_reserve_when_full() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        // A single full leaf of unwritten extents (a fallocate'd region): a
        // partial convert splits it and forces one new tree node.
        let mut tree = ExtentTree::empty();
        for i in 0..LEAF_MAX as u32 {
            tree.insert(
                &f.ext4,
                i * 4,
                20_000 + (i as Ext4Bid) * 4,
                2,
                ExtentKind::Unwritten,
                None,
                None,
            )
            .unwrap();
        }

        // Drain the volume with ordinary allocations. Each stops at the reserve
        // floor, so the loop ends with only the reserve left free — the exact
        // state generic/274 reaches by filling the disk from userspace.
        loop {
            match f.ext4.alloc_blocks(4096, 0, None) {
                Ok(_) => {}
                Err(e) => {
                    assert_eq!(e.error(), Errno::ENOSPC);
                    break;
                }
            }
        }

        // The reserve is intact and untouchable by ordinary work: free blocks
        // remain, yet a `Normal` allocation is `ENOSPC` (the reserve is never
        // eaten by ordinary allocations).
        let reserve = f.ext4.super_block().free_blocks_count();
        assert!(reserve > 0);
        assert_eq!(
            f.ext4.alloc_blocks(1, 0, None).unwrap_err().error(),
            Errno::ENOSPC
        );

        // The conversion draws the reserve for its split and succeeds — the
        // write into the preallocated region keeps its no-ENOSPC promise.
        tree.convert_unwritten(&f.ext4, 601, 1, None, None).unwrap();
        assert!(!tree.lookup(&f.ext4, 601).unwrap().unwrap().is_unwritten());
        // The split consumed reserve blocks (free dropped) that the `Normal`
        // path above could never reach.
        assert!(f.ext4.super_block().free_blocks_count() < reserve);
    }

    /// `grow_root` grows past the old depth-2 cap (T6): a full depth-2 root
    /// copies down under a one-entry depth-3 index, and only the on-disk
    /// format ceiling [`MAX_DEPTH`] refuses (grow never dereferences the
    /// children, so the crafted child ids stay untouched).
    #[ktest]
    fn grow_root_past_two_up_to_max_depth() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();

        let craft_full_root = |depth: u16| {
            let mut root = [0u32; RAW_BLOCK_PTRS_LEN];
            let header = RawExtentHeader {
                magic: EXTENT_MAGIC,
                entries: INLINE_MAX as u16,
                max: INLINE_MAX as u16,
                depth,
                generation: 0,
            };
            root.as_mut_bytes()[0..ENTRY_SIZE].copy_from_slice(header.as_bytes());
            for i in 0..INLINE_MAX {
                let e = make_index_entry(i as Iblock * 1000, 100 + i as Ext4Bid);
                root.as_mut_bytes()[ENTRY_SIZE * (1 + i)..ENTRY_SIZE * (2 + i)]
                    .copy_from_slice(e.as_bytes());
            }
            ExtentTree::try_new(root, 0).unwrap()
        };

        // depth 2 → 3: allowed since T6.
        let mut tree = craft_full_root(2);
        tree.grow_root(&f.ext4, None, None, AllocIntent::Normal)
            .unwrap();
        assert_eq!(tree.depth(), 3);
        assert_eq!(tree.header().entries(), 1);
        assert_eq!(root_index_key(&tree, 0), 0);

        // The format ceiling holds: a MAX_DEPTH root refuses to grow.
        let mut deep = craft_full_root(MAX_DEPTH);
        assert_eq!(
            deep.grow_root(&f.ext4, None, None, AllocIntent::Normal)
                .unwrap_err()
                .error(),
            Errno::ENOSPC
        );
    }

    // ---- fallocate ZERO_RANGE (mark_range_unwritten) ----

    /// ZERO_RANGE over a written extent's middle flips the covered sub-range to
    /// unwritten (reads-as-zero) while keeping its physical mapping, splitting
    /// the written head and tail off around it.
    #[ktest]
    fn zero_range_inline_flips_written_middle_to_unwritten() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        tree.insert(&f.ext4, 0, 100, 10, ExtentKind::Written, None, None)
            .unwrap();
        // Zero blocks [3, 7): head [0,3) written, mid [3,7) unwritten, tail [7,10).
        tree.mark_range_unwritten(&f.ext4, 3, 4, None, None)
            .unwrap();

        let head = tree.lookup(&f.ext4, 0).unwrap().unwrap();
        assert!(!head.is_unwritten());
        let mid = tree.lookup(&f.ext4, 5).unwrap().unwrap();
        assert!(mid.is_unwritten());
        // Physical mapping of block 5 preserved (no data moved): 100 + 5.
        assert_eq!(mid.start() + (5 - mid.block()) as Ext4Bid, 105);
        let tail = tree.lookup(&f.ext4, 8).unwrap().unwrap();
        assert!(!tail.is_unwritten());
        assert_eq!(tail.start() + (8 - tail.block()) as Ext4Bid, 108);
        assert_matches_linear(&f, &tree, 12);
    }

    /// A ZERO_RANGE that fully covers a written extent flips it and coalesces
    /// with an adjacent unwritten run (they become one unwritten extent).
    #[ktest]
    fn zero_range_full_extent_coalesces_with_unwritten_neighbor() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        // Unwritten [0,4)@100 then physically contiguous written [4,8)@104.
        tree.insert(&f.ext4, 0, 100, 4, ExtentKind::Unwritten, None, None)
            .unwrap();
        tree.insert(&f.ext4, 4, 104, 4, ExtentKind::Written, None, None)
            .unwrap();
        assert_eq!(root_entries(&tree), 2);
        tree.mark_range_unwritten(&f.ext4, 4, 4, None, None)
            .unwrap();
        // The flipped run merges with its unwritten neighbour into one extent.
        assert_eq!(root_entries(&tree), 1);
        let m = tree.lookup(&f.ext4, 6).unwrap().unwrap();
        assert!(m.is_unwritten());
        assert_eq!((m.block(), m.len(), m.start()), (0, 8, 100));
    }

    /// ZERO_RANGE on a depth-1 tree flips the written extents in the range to
    /// unwritten in place, keeping their mappings; blocks outside stay written.
    #[ktest]
    fn zero_range_depth1_converts_written_run() {
        let f = Ext4FixtureBuilder::new(16384, 256, 16384)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let n = LEAF_MAX as u32 + 50; // depth 1
        let (mut tree, pblocks) = ascending_tree_allocated(&f, n);
        assert_eq!(tree.depth(), 1);
        // Zero span [10, 20): the written extents at even logical blocks flip.
        tree.mark_range_unwritten(&f.ext4, 10, 10, None, None)
            .unwrap();
        let m = tree.lookup(&f.ext4, 10).unwrap().unwrap();
        assert!(m.is_unwritten());
        assert_eq!(m.start(), pblocks[5]); // logical 10 = extent index 5
        // Outside the range stays written.
        assert!(!tree.lookup(&f.ext4, 8).unwrap().unwrap().is_unwritten());
        assert_matches_linear(&f, &tree, n * 2);
    }

    /// The every-op gate: a ZERO_RANGE over an already-unwritten range edits
    /// nothing and leaves the tree clean.
    #[ktest]
    fn zero_range_noop_on_already_unwritten() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        tree.insert(&f.ext4, 0, 100, 8, ExtentKind::Unwritten, None, None)
            .unwrap();
        tree.clear_dirty();
        tree.mark_range_unwritten(&f.ext4, 2, 4, None, None)
            .unwrap();
        assert!(!tree.is_dirty());
        assert_eq!(root_entries(&tree), 1);
    }

    /// Regression (P9b review): a ZERO_RANGE fully covering a `MAX_WRITTEN_LEN`
    /// (32768) written extent in the INLINE root must split the unwritten flip at
    /// `MAX_UNWRITTEN_LEN`. A single unwritten run of 32768 bias-encodes to a
    /// wrapped zero-length `ee_len` (`RawExtent::from` debug_asserts pre-fix).
    #[ktest]
    fn zero_range_full_max_written_extent_splits_inline() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        // One written extent spanning the full MAX_WRITTEN_LEN (physical blocks
        // are notional — `mark_range_unwritten` moves no data).
        tree.insert(
            &f.ext4,
            0,
            100,
            MAX_WRITTEN_LEN,
            ExtentKind::Written,
            None,
            None,
        )
        .unwrap();
        assert_eq!(root_entries(&tree), 1);

        // Flip the whole run to unwritten (would wrap `ee_len` without the split).
        tree.mark_range_unwritten(&f.ext4, 0, MAX_WRITTEN_LEN as u32, None, None)
            .unwrap();

        // Two unwritten extents: a capped head plus a one-block tail, both keeping
        // their physical mapping.
        let (extents, _) = tree.flatten(&f.ext4).unwrap();
        assert_eq!(extents.len(), 2);
        assert_eq!(
            (
                extents[0].block(),
                extents[0].len(),
                extents[0].start(),
                extents[0].is_unwritten()
            ),
            (0, MAX_UNWRITTEN_LEN, 100, true)
        );
        assert_eq!(
            (
                extents[1].block(),
                extents[1].len(),
                extents[1].start(),
                extents[1].is_unwritten()
            ),
            (
                MAX_UNWRITTEN_LEN as Iblock,
                1,
                100 + MAX_UNWRITTEN_LEN as Ext4Bid,
                true
            )
        );
        // A block deep inside the head round-trips through the on-disk encoding.
        let mid = tree.lookup(&f.ext4, 20000).unwrap().unwrap();
        assert!(mid.is_unwritten());
        assert_eq!(mid.start() + (20000 - mid.block()) as Ext4Bid, 100 + 20000);
    }

    /// Regression (P9b review), depth-1 counterpart: a ZERO_RANGE fully covering a
    /// `MAX_WRITTEN_LEN` run inside an EXTERNAL leaf must split the in-place kind
    /// flip at `MAX_UNWRITTEN_LEN` too, or the leaf write-back bias-encodes a
    /// wrapped zero-length `ee_len`.
    #[ktest]
    fn zero_range_full_max_written_extent_splits_depth1() {
        let f = Ext4FixtureBuilder::new(16384, 256, 16384)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        // A full MAX_WRITTEN_LEN written run at logical 0 (physical notional).
        tree.insert(
            &f.ext4,
            0,
            100,
            MAX_WRITTEN_LEN,
            ExtentKind::Written,
            None,
            None,
        )
        .unwrap();
        // Overflow the inline root so the run lands in an external leaf (depth 1).
        for k in 0..INLINE_MAX as u32 + 1 {
            tree.insert(
                &f.ext4,
                40000 + k * 2,
                500 + k as Ext4Bid,
                1,
                ExtentKind::Written,
                None,
                None,
            )
            .unwrap();
        }
        assert_eq!(tree.depth(), 1);

        tree.mark_range_unwritten(&f.ext4, 0, MAX_WRITTEN_LEN as u32, None, None)
            .unwrap();

        // The 32768 run flipped to two capped unwritten extents, mappings kept.
        let head = tree.lookup(&f.ext4, 0).unwrap().unwrap();
        assert_eq!(
            (head.block(), head.len(), head.start(), head.is_unwritten()),
            (0, MAX_UNWRITTEN_LEN, 100, true)
        );
        let tail = tree
            .lookup(&f.ext4, MAX_UNWRITTEN_LEN as u32)
            .unwrap()
            .unwrap();
        assert_eq!(
            (tail.block(), tail.len(), tail.start(), tail.is_unwritten()),
            (
                MAX_UNWRITTEN_LEN as Iblock,
                1,
                100 + MAX_UNWRITTEN_LEN as Ext4Bid,
                true
            )
        );
        // A block inside the head reads its preserved physical mapping.
        let mid = tree.lookup(&f.ext4, 20000).unwrap().unwrap();
        assert!(mid.is_unwritten());
        assert_eq!(mid.start() + (20000 - mid.block()) as Ext4Bid, 100 + 20000);
        // The unrelated small extents survive untouched.
        assert!(!tree.lookup(&f.ext4, 40000).unwrap().unwrap().is_unwritten());
    }

    // ---- fallocate COLLAPSE_RANGE ----

    /// COLLAPSE_RANGE frees the removed window's blocks and shifts the tail left,
    /// closing the logical gap; `i_blocks` and the free count drop by exactly the
    /// freed data blocks.
    #[ktest]
    fn collapse_frees_window_and_shifts_tail_left_inline() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        // Three REAL 2-block runs (the free-count assertion below needs the
        // bitmap to genuinely hold them) at logical 0 / 4 / 8.
        let mut runs = Vec::new();
        for ib in [0u32, 4, 8] {
            let r = f.ext4.alloc_blocks(2, 0, None).unwrap();
            assert_eq!(r.end - r.start, 2);
            tree.insert(&f.ext4, ib, r.start, 2, ExtentKind::Written, None, None)
                .unwrap();
            runs.push(r.start);
        }
        let free_before = f.ext4.super_block().free_blocks_count();
        let sc_before = tree.sector_count();
        // Collapse blocks [4, 8): frees run 1, shifts [8,10) → [4,6) (run 2).
        tree.collapse_range(
            &f.ext4,
            4,
            8,
            None,
            None,
            journal::DataForgetPolicy::PlainData,
        )
        .unwrap();
        assert_eq!(tree.lookup(&f.ext4, 0).unwrap().unwrap().start(), runs[0]);
        let m = tree.lookup(&f.ext4, 4).unwrap().unwrap();
        assert_eq!((m.block(), m.len(), m.start()), (4, 2, runs[2]));
        assert!(tree.lookup(&f.ext4, 8).unwrap().is_none());
        assert_eq!(f.ext4.super_block().free_blocks_count(), free_before + 2);
        assert_eq!(sc_before - tree.sector_count(), 2 * SECTORS_PER_BLOCK);
        assert_matches_linear(&f, &tree, 12);
    }

    /// COLLAPSE_RANGE inside one extent splits off the head, frees the covered
    /// middle, and shifts the tail left — the head and shifted tail stay separate
    /// (a physical gap remains where the middle was freed).
    #[ktest]
    fn collapse_straddling_extent_splits_and_shifts() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        // One REAL 10-block run (the free-count assertion needs the bitmap to
        // genuinely hold it) mapped at logical [0, 10).
        let r = f.ext4.alloc_blocks(10, 0, None).unwrap();
        assert_eq!(r.end - r.start, 10);
        let p = r.start;
        tree.insert(&f.ext4, 0, p, 10, ExtentKind::Written, None, None)
            .unwrap();
        let free_before = f.ext4.super_block().free_blocks_count();
        // Collapse [2,6): head [0,2)@p kept, [2,6)@p+2 freed, tail [6,10)→[2,6)@p+6.
        tree.collapse_range(
            &f.ext4,
            2,
            6,
            None,
            None,
            journal::DataForgetPolicy::PlainData,
        )
        .unwrap();
        let head = tree.lookup(&f.ext4, 0).unwrap().unwrap();
        assert_eq!((head.block(), head.len(), head.start()), (0, 2, p));
        let tail = tree.lookup(&f.ext4, 2).unwrap().unwrap();
        assert_eq!((tail.block(), tail.len(), tail.start()), (2, 4, p + 6));
        assert!(tree.lookup(&f.ext4, 6).unwrap().is_none());
        assert_eq!(f.ext4.super_block().free_blocks_count(), free_before + 4);
        assert_matches_linear(&f, &tree, 12);
    }

    /// COLLAPSE_RANGE on a depth-1 tree rebuilds it (flatten + reserialize) with
    /// the tail shifted left; the survivor stays consistent with the reference.
    #[ktest]
    fn collapse_depth1_rebuilds_and_shifts() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let n = LEAF_MAX as u32 + 50; // depth 1
        let (mut tree, pblocks) = ascending_tree_allocated(&f, n);
        assert_eq!(tree.depth(), 1);
        let free_before = f.ext4.super_block().free_blocks_count();
        let sc_before = tree.sector_count();
        // Collapse [0,2): frees logical-0 block, shifts everything left by 2.
        tree.collapse_range(
            &f.ext4,
            0,
            2,
            None,
            None,
            journal::DataForgetPolicy::PlainData,
        )
        .unwrap();
        assert_eq!(
            tree.lookup(&f.ext4, 0).unwrap().unwrap().start(),
            pblocks[1]
        );
        assert_eq!(
            tree.lookup(&f.ext4, 2).unwrap().unwrap().start(),
            pblocks[2]
        );
        // At least the one freed data block was returned; i_blocks dropped.
        assert!(f.ext4.super_block().free_blocks_count() > free_before);
        assert!(tree.sector_count() < sc_before);
        assert_matches_linear(&f, &tree, n * 2);
    }

    // ---- fallocate INSERT_RANGE ----

    /// INSERT_RANGE opens a hole and shifts the tail right, splitting the extent
    /// straddling the insertion point; no data block moves.
    #[ktest]
    fn insert_opens_hole_and_shifts_tail_right_inline() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        tree.insert(&f.ext4, 0, 100, 4, ExtentKind::Written, None, None)
            .unwrap();
        let sc_before = tree.sector_count();
        // Insert 2 blocks at offset 2: head [0,2)@100, hole [2,4), tail [4,6)@102.
        tree.insert_range(&f.ext4, 2, 2, None, None).unwrap();
        let head = tree.lookup(&f.ext4, 0).unwrap().unwrap();
        assert_eq!((head.block(), head.len(), head.start()), (0, 2, 100));
        assert!(tree.lookup(&f.ext4, 2).unwrap().is_none());
        assert!(tree.lookup(&f.ext4, 3).unwrap().is_none());
        let tail = tree.lookup(&f.ext4, 4).unwrap().unwrap();
        assert_eq!((tail.block(), tail.len(), tail.start()), (4, 2, 102));
        // No data allocated or freed (only logical keys moved).
        assert_eq!(tree.sector_count(), sc_before);
        assert_matches_linear(&f, &tree, 8);
    }

    /// INSERT_RANGE on a depth-1 tree shifts every extent right; the leading
    /// range becomes a hole and the tail keeps its physical blocks.
    #[ktest]
    fn insert_depth1_rebuilds_and_shifts() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let n = LEAF_MAX as u32 + 50; // depth 1
        let (mut tree, pblocks) = ascending_tree_allocated(&f, n);
        assert_eq!(tree.depth(), 1);
        // Insert 4 blocks at offset 0: everything shifts right by 4.
        tree.insert_range(&f.ext4, 0, 4, None, None).unwrap();
        assert!(tree.lookup(&f.ext4, 0).unwrap().is_none());
        assert_eq!(
            tree.lookup(&f.ext4, 4).unwrap().unwrap().start(),
            pblocks[0]
        );
        assert_eq!(
            tree.lookup(&f.ext4, 6).unwrap().unwrap().start(),
            pblocks[1]
        );
        assert_matches_linear(&f, &tree, n * 2 + 8);
    }

    /// INSERT_RANGE whose shift would push the last extent past the 32-bit
    /// logical space is rejected `EINVAL` with the tree untouched (Linux
    /// `ext4_ext_shift_extents`'s SHIFT_RIGHT `EXT_MAX_BLOCKS` guard) — without
    /// it the shifted-key arithmetic would wrap and corrupt the tree.
    #[ktest]
    fn insert_rejects_shift_past_logical_max() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        tree.insert(&f.ext4, 0, 100, 2, ExtentKind::Written, None, None)
            .unwrap();
        // An extent parked near the top of the logical space (a KEEP_SIZE
        // preallocation can create this shape).
        let high = u32::MAX - 4;
        tree.insert(&f.ext4, high, 200, 2, ExtentKind::Unwritten, None, None)
            .unwrap();

        // Shifting right by 4 would push its end (MAX - 2) past u32::MAX.
        let err = tree.insert_range(&f.ext4, 0, 4, None, None).unwrap_err();
        assert_eq!(err.error(), Errno::EINVAL);
        // Nothing changed.
        assert_eq!(tree.lookup(&f.ext4, 0).unwrap().unwrap().start(), 100);
        assert_eq!(tree.lookup(&f.ext4, high).unwrap().unwrap().start(), 200);

        // A shift that exactly reaches the boundary still succeeds.
        tree.insert_range(&f.ext4, 0, 2, None, None).unwrap();
        assert_eq!(
            tree.lookup(&f.ext4, high + 2).unwrap().unwrap().start(),
            200
        );
    }

    // ---- P10-T1b: es-cache population by the convert paths (the knife-2
    // written-hint nails, migrated per the T1 spec §7.2) ----

    /// The no-conversion arm of `convert_unwritten` records every written
    /// extent its scan visited (R2), so the cache answers `AllWritten` over
    /// the covering extent's OWN bounds — and still `Unknown` one block past
    /// them — on the inline (depth-0) path.
    #[ktest]
    fn es_populated_by_noop_convert_inline() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        // A written run [0,8) @ 100 in the inline root. The insert
        // invalidates its own span and records nothing (no R4).
        tree.insert(&f.ext4, 0, 100, 8, ExtentKind::Written, None, None)
            .unwrap();
        assert_eq!(tree.es_cache.range_state(0, 8), EsCoverage::Unknown);

        // Converting an all-written sub-range is a no-op that records the
        // WHOLE visited extent, not just the queried [2,6).
        tree.convert_unwritten(&f.ext4, 2, 4, None, None).unwrap();
        assert_eq!(tree.es_cache.range_state(2, 6), EsCoverage::AllWritten);
        assert_eq!(tree.es_cache.range_state(0, 8), EsCoverage::AllWritten);
        // Past the extent's real boundary the cache claims nothing.
        assert_eq!(tree.es_cache.range_state(0, 9), EsCoverage::Unknown);
    }

    /// Same discipline on an external (depth-1) tree: the no-op convert's
    /// walk records the visited written singleton, bounded to that extent —
    /// a probe spilling into the neighbouring hole stays `Unknown` (the
    /// false-positive guard).
    #[ktest]
    fn es_populated_by_noop_convert_external() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        // Written singletons at even blocks 0,2,4,... → depth 1 with holes
        // between every extent.
        let (mut tree, _pblocks) = ascending_tree_allocated(&f, 400);
        assert_eq!(tree.depth(), 1);

        // Block 20 maps a written [20,21); block 21 is a hole.
        tree.convert_unwritten(&f.ext4, 20, 1, None, None).unwrap();
        assert_eq!(tree.es_cache.range_state(20, 21), EsCoverage::AllWritten);
        assert_eq!(tree.es_cache.range_state(20, 22), EsCoverage::Unknown);
    }

    /// A convert that actually flips (the edited arm) leaves the cache
    /// claiming `AllWritten` only over what genuinely converted — never over
    /// the still-unwritten remainder (the false-positive red line). With the
    /// remainder's own unwritten fact recorded (an R1-style walk), the range
    /// answers the richer `AllMapped` tier instead — the three-state
    /// increment over the binary hint this cache replaced.
    #[ktest]
    fn es_never_claims_written_over_unwritten() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        // External tree: the depth-0 edited arm records nothing (no inline
        // R3), which satisfies the red line only vacuously.
        let mut tree = depth1_written_tree(&f);
        tree.insert(&f.ext4, 60, 2000, 8, ExtentKind::Unwritten, None, None)
            .unwrap();

        // Convert [60,64): a real flip whose R3 claims exactly the flipped
        // half — the unwritten tail is never part of an `AllWritten` answer.
        tree.convert_unwritten(&f.ext4, 60, 4, None, None).unwrap();
        assert_eq!(tree.es_cache.range_state(60, 64), EsCoverage::AllWritten);
        assert_eq!(tree.es_cache.range_state(60, 68), EsCoverage::Unknown);
        assert!(tree.lookup(&f.ext4, 64).unwrap().unwrap().is_unwritten());

        // Record the surviving tail's unwritten fact the way the fill walk
        // (R1) does — visit and record. The range then answers `AllMapped`:
        // fully backed, but still never `AllWritten` over unwritten blocks.
        let es_cache = &tree.es_cache;
        tree.walk_range(&f.ext4, 64..68, &mut |e| {
            es_cache.record(e);
            ControlFlow::Continue(())
        })
        .unwrap();
        assert_eq!(tree.es_cache.range_state(60, 68), EsCoverage::AllMapped);
        assert_eq!(tree.es_cache.range_state(64, 68), EsCoverage::AllMapped);
        assert_eq!(tree.es_cache.range_state(60, 64), EsCoverage::AllWritten);
    }

    // ---- P10-T1a: es-cache invalidation, population, and debug net ----

    /// Builds a depth-1 tree of unmergeable written runs `[b, b+4) @ 1000+b`
    /// for `b` in 0,10,20,30,40 — five inserts overflow the inline root into
    /// one external leaf, so later mutations are in-place surgery (a depth-0
    /// insert would rebuild through `reserialize` and clear the es-cache).
    fn depth1_written_tree(f: &crate::fs::fs_impls::ext4::test_utils::Ext4Fixture) -> ExtentTree {
        let mut tree = ExtentTree::empty();
        for b in [0u32, 10, 20, 30, 40] {
            tree.insert(
                &f.ext4,
                b,
                1000 + b as Ext4Bid,
                4,
                ExtentKind::Written,
                None,
                None,
            )
            .unwrap();
        }
        assert_eq!(tree.depth(), 1);
        tree
    }

    /// An in-leaf merge preserves per-block truth, so a recorded fact about
    /// the merge's left side SURVIVES an adjacent insert (which invalidates
    /// only its own span) and still agrees with the tree block for block —
    /// the es-cache's core increment over the whole-clearing written hint.
    #[ktest]
    fn es_facts_survive_in_leaf_merge_with_per_block_truth() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = depth1_written_tree(&f);

        // Prime: a no-op convert over [20,24) records the visited fact (R2).
        tree.convert_unwritten(&f.ext4, 20, 4, None, None).unwrap();
        assert_eq!(tree.es_cache.range_state(20, 24), EsCoverage::AllWritten);

        // Insert the logically AND physically adjacent [24,25) @ 1024: the
        // leaf merges it onto [20,24) in place, widening the entry to [20,25).
        tree.insert(&f.ext4, 24, 1024, 1, ExtentKind::Written, None, None)
            .unwrap();
        let merged = tree.lookup(&f.ext4, 20).unwrap().unwrap();
        assert_eq!(
            (merged.block(), merged.len(), merged.start()),
            (20, 5, 1020)
        );

        // The recorded fact survived the insert and is still true per block.
        assert_eq!(tree.es_cache.range_state(20, 24), EsCoverage::AllWritten);
        for b in 20u32..24 {
            let m = tree.lookup(&f.ext4, b).unwrap().unwrap();
            assert!(!m.is_unwritten());
            assert_eq!(m.start() + (b - m.block()) as u64, 1000 + b as u64);
        }
        // The freshly inserted block was invalidated, not re-recorded (no R4):
        // `Unknown`, never a manufactured claim.
        assert_eq!(tree.es_cache.range_state(24, 25), EsCoverage::Unknown);
    }

    /// Every logical mutator drops the facts its span falsifies on entry —
    /// and ONLY those: a disjoint fact SURVIVES (the es-cache's increment
    /// over the whole-clearing written hint it replaced). The shift mutators
    /// clear the whole tail, and their applies rebuild the tree
    /// (`reserialize` → `clear_all`), so nothing survives them today —
    /// over-invalidation is always legal; the red line runs the other way.
    #[ktest]
    fn es_invalidated_by_mutators() {
        // Prime written facts [0,8) and [20,24) via one no-op convert (the
        // inline scan R2-records every written entry it passes), run
        // `mutate`, then require `probe` to answer `Unknown` — the strongest
        // claim the cache may make about blocks whose mapping or kind just
        // changed is nothing at all — and `survivor` (when given) to still
        // answer `AllWritten`.
        fn assert_probe_unknown(
            probe: (Iblock, Iblock),
            survivor: Option<(Iblock, Iblock)>,
            mutate: impl FnOnce(&Ext4, &mut ExtentTree),
        ) {
            let f = Ext4FixtureBuilder::new(8192, 256, 8192)
                .with_block_bitmap_metadata_marked()
                .build()
                .unwrap();
            let mut tree = ExtentTree::empty();
            let run = f.ext4.alloc_blocks(8, 0, None).unwrap();
            tree.insert(&f.ext4, 0, run.start, 8, ExtentKind::Written, None, None)
                .unwrap();
            let far = f.ext4.alloc_blocks(4, 0, None).unwrap();
            tree.insert(&f.ext4, 20, far.start, 4, ExtentKind::Written, None, None)
                .unwrap();
            tree.convert_unwritten(&f.ext4, 0, 8, None, None).unwrap();
            assert_eq!(
                tree.es_cache.range_state(0, 8),
                EsCoverage::AllWritten,
                "es primed"
            );
            assert_eq!(
                tree.es_cache.range_state(20, 24),
                EsCoverage::AllWritten,
                "es primed (disjoint fact)"
            );
            mutate(&f.ext4, &mut tree);
            assert_eq!(
                tree.es_cache.range_state(probe.0, probe.1),
                EsCoverage::Unknown,
                "a mutator left an over-strong answer over its span"
            );
            if let Some((s, e)) = survivor {
                assert_eq!(
                    tree.es_cache.range_state(s, e),
                    EsCoverage::AllWritten,
                    "a mutator dropped a fact outside its span"
                );
            }
        }

        // insert: the fresh mapping's own span claims nothing (R4 unbuilt).
        // No survivor probe HERE: a depth-0 insert rebuilds the root through
        // `reserialize`, whose `clear_all` legally drops everything — the
        // in-place (depth-1) insert's disjoint-fact survival is pinned by
        // `es_facts_survive_in_leaf_merge_with_per_block_truth` above.
        assert_probe_unknown((100, 101), None, |fs, tree| {
            let r = fs.alloc_blocks(1, 0, None).unwrap();
            tree.insert(fs, 100, r.start, 1, ExtentKind::Written, None, None)
                .unwrap();
        });
        // mark_range_unwritten: the W→U flip falsifies the written fact
        // (removed whole — entries are never trimmed); [20,24) is untouched.
        assert_probe_unknown((2, 6), Some((20, 24)), |fs, tree| {
            tree.mark_range_unwritten(fs, 2, 4, None, None).unwrap();
        });
        // punch: the range is now a hole; any claim would be false.
        assert_probe_unknown((2, 4), Some((20, 24)), |fs, tree| {
            tree.punch_chunk(
                fs,
                2..4,
                None,
                None,
                journal::DataForgetPolicy::PlainData,
                None,
            )
            .unwrap();
        });
        // truncate: the `[keep, MAX)` tail span drops [20,24); the head
        // fact below the cut survives.
        assert_probe_unknown((20, 24), Some((0, 8)), |fs, tree| {
            tree.truncate_to_byte_len(
                fs,
                16 * BLOCK_SIZE,
                None,
                None,
                journal::DataForgetPolicy::PlainData,
            )
            .unwrap();
        });
        // collapse (shift left): the whole tail from the punch start — and
        // no survivor probe: the apply rebuilds the tree, clearing whole.
        assert_probe_unknown((2, 8), None, |fs, tree| {
            tree.collapse_range(fs, 2, 4, None, None, journal::DataForgetPolicy::PlainData)
                .unwrap();
        });
        // insert_range (shift right): the whole tail from the offset (same
        // whole-tree rebuild, no survivor).
        assert_probe_unknown((2, 8), None, |fs, tree| {
            tree.insert_range(fs, 2, 2, None, None).unwrap();
        });
        // A real convert re-flips [3,4): the pre-flip fact is gone and the
        // inline (depth-0) edited arm records nothing over the flipped span
        // (R3 lives on the external path only) — `Unknown`, never stale;
        // the disjoint [20,24) rides through both root rewrites.
        assert_probe_unknown((3, 4), Some((20, 24)), |fs, tree| {
            tree.mark_range_unwritten(fs, 3, 1, None, None).unwrap();
            tree.convert_unwritten(fs, 3, 1, None, None).unwrap();
        });
    }

    /// R3: a landed unwritten→written flip backfills exactly the written fact
    /// it produced — the full-cover arm records the whole run, the mid arm
    /// records only the flipped middle (the surviving unwritten tail stays
    /// unclaimed), and a staged head split records nothing until its
    /// remainder actually flips.
    #[ktest]
    fn es_r3_backfills_converted_range() {
        let f = Ext4FixtureBuilder::new(8192, 256, 8192)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        // External (depth-1) tree: the inline convert path has no R3 by design.
        let mut tree = depth1_written_tree(&f);

        // Full-cover flip.
        tree.insert(&f.ext4, 50, 2000, 8, ExtentKind::Unwritten, None, None)
            .unwrap();
        tree.convert_unwritten(&f.ext4, 50, 8, None, None).unwrap();
        assert_eq!(tree.es_cache.range_state(50, 58), EsCoverage::AllWritten);
        assert!(!tree.lookup(&f.ext4, 50).unwrap().unwrap().is_unwritten());

        // Mid flip (no head, unwritten tail survives).
        tree.insert(&f.ext4, 70, 3000, 8, ExtentKind::Unwritten, None, None)
            .unwrap();
        tree.convert_unwritten(&f.ext4, 70, 4, None, None).unwrap();
        assert_eq!(tree.es_cache.range_state(70, 74), EsCoverage::AllWritten);
        assert_eq!(tree.es_cache.range_state(74, 78), EsCoverage::Unknown);
        assert_eq!(tree.es_cache.range_state(70, 78), EsCoverage::Unknown);
        assert!(tree.lookup(&f.ext4, 74).unwrap().unwrap().is_unwritten());

        // Head-split round, then the remainder's full-cover flip: only the
        // genuinely converted [76,78) is claimed written.
        tree.convert_unwritten(&f.ext4, 76, 2, None, None).unwrap();
        assert_eq!(tree.es_cache.range_state(76, 78), EsCoverage::AllWritten);
        assert_eq!(tree.es_cache.range_state(74, 76), EsCoverage::Unknown);
        assert_matches_linear(&f, &tree, 100);
    }

    /// The debug double-read net accepts verdicts the tree supports (and
    /// `Unknown` vacuously) — the Q2/Q3-side assertion T1b's query points run
    /// behind every hit.
    #[ktest]
    fn es_debug_net_accepts_true_verdicts() {
        let f = Ext4FixtureBuilder::new(2048, 256, 2048)
            .with_block_bitmap_metadata_marked()
            .build()
            .unwrap();
        let mut tree = ExtentTree::empty();
        tree.insert(&f.ext4, 0, 100, 4, ExtentKind::Written, None, None)
            .unwrap();
        tree.insert(&f.ext4, 4, 200, 4, ExtentKind::Unwritten, None, None)
            .unwrap();

        tree.debug_assert_es_coverage(&f.ext4, 0, 4, EsCoverage::AllWritten);
        tree.debug_assert_es_coverage(&f.ext4, 0, 8, EsCoverage::AllMapped);
        // `Unknown` claims nothing — legal even over a plain hole.
        tree.debug_assert_es_coverage(&f.ext4, 100, 200, EsCoverage::Unknown);
    }
}
