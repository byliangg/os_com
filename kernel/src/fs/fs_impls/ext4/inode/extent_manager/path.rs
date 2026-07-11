// SPDX-License-Identifier: MPL-2.0

//! Read side of the in-place extent-tree surgery (P9a): a validated view of
//! one external node ([`NodeBuf`]) and the root→leaf search path
//! ([`ExtentPath`]) recording where a lookup landed, so tree mutations can
//! edit exactly the touched nodes instead of re-serializing the whole tree.
//!
//! A path is a cursor private to one [`ExtentTree`](super::tree::ExtentTree)
//! operation: it is created and consumed under the tree's position-③ lock and
//! never cached across calls (cross-call caching is the extent-status-cache
//! work, deliberately out of scope for the surgery).

use super::{
    super::super::{checksum::InodeCsumSeed, fs::Ext4, journal, prelude::*},
    node::{ENTRY_SIZE, Extent, ExtentHeader, ExtentIdx, RawExtent, RawExtentHeader, RawExtentIdx},
};

/// A validated, owned copy of one full-block external extent-tree node (leaf
/// or interior), read through the journal's read funnel.
///
/// The header is checked once at [`read`](Self::read) — the parse boundary for
/// device bytes; accessors thereafter trust it. Mutating editors arrive with
/// the surgery write path (P9a-T2+); until then this is a read-only view.
pub(super) struct NodeBuf {
    bid: Ext4Bid,
    bytes: Box<[u8; BLOCK_SIZE]>,
    header: ExtentHeader,
}

impl NodeBuf {
    /// Reads and validates the external node at `bid`.
    ///
    /// Goes through [`journal::read_metadata_block`] — on a journaled volume a
    /// node's newest bytes may still sit in a journal capture (WAL suppresses
    /// the direct write until checkpoint), so a bare device read here would
    /// walk a stale tree.
    pub(super) fn read(fs: &Ext4, bid: Ext4Bid) -> Result<Self> {
        let journal = fs.journal();
        let bytes =
            journal::read_metadata_block(journal.as_deref(), fs.block_device().as_ref(), bid)?;
        let header = ExtentHeader::try_from(&RawExtentHeader::from_bytes(&bytes[0..ENTRY_SIZE]))?;
        if ENTRY_SIZE * (1 + header.entries() as usize) > BLOCK_SIZE {
            return_errno_with_message!(Errno::EUCLEAN, "extent node entries overrun node");
        }
        // A non-leaf external node must name at least one child: an empty
        // interior is corruption (its phantom entry 0 would drive the descent
        // into unvalidated bytes). An empty external LEAF is legal — an
        // append-split writes a fresh empty leaf, publishes it, then the retry
        // insert reads it back and fills it (T3). The inline root may also be
        // empty, but it never reaches here.
        if !header.is_leaf() && header.entries() == 0 {
            return_errno_with_message!(Errno::EUCLEAN, "interior extent node has no children");
        }
        Ok(Self {
            bid,
            bytes: Box::new(bytes),
            header,
        })
    }

    /// Builds a fresh, empty node at `bid` for a split or a depth growth: a
    /// zeroed block with a full-block-capacity header at `depth`. The caller
    /// allocated `bid` under the handle (zero-seeded capture), and the node
    /// stays unreferenced until an ancestor's index entry publishes it.
    pub(super) fn fresh(bid: Ext4Bid, depth: u16) -> Self {
        let mut bytes = Box::new([0u8; BLOCK_SIZE]);
        let raw = RawExtentHeader {
            magic: super::node::EXTENT_MAGIC,
            entries: 0,
            // Leaf and interior full-block nodes share the same geometry
            // (12-byte entries after a 12-byte header). Lossless: the
            // capacity is 340 at any supported block size.
            max: super::node::NODE_CAPACITY as u16,
            depth,
            generation: 0,
        };
        bytes[0..ENTRY_SIZE].copy_from_slice(raw.as_bytes());
        Self {
            bid,
            bytes,
            header: ExtentHeader::from_trusted(&raw),
        }
    }

    /// Returns the physical block this node was read from (or will be written
    /// to, for a [`fresh`](Self::fresh) node).
    pub(super) const fn bid(&self) -> Ext4Bid {
        self.bid
    }

    /// Returns whether the node has no room for one more entry.
    pub(super) fn is_full(&self) -> bool {
        self.entries() >= self.max_entries() || ENTRY_SIZE * (2 + self.entries()) > BLOCK_SIZE
    }

    /// Returns entry 0's logical-block key (either node kind). Caller
    /// guarantees the node is non-empty.
    pub(super) fn first_key(&self) -> Iblock {
        debug_assert!(self.entries() > 0);
        if self.is_leaf() {
            self.extent_at(0).block()
        } else {
            self.index_at(0).block()
        }
    }

    /// Adopts `n` entries' raw bytes (12-byte slabs, either kind) into this
    /// empty node — the grow step's verbatim copy of the old root's entry
    /// area. Memory-only; the node still needs [`write_back`](Self::write_back).
    pub(super) fn adopt_entries(&mut self, entry_bytes: &[u8], n: usize) {
        debug_assert_eq!(self.entries(), 0);
        debug_assert_eq!(entry_bytes.len(), ENTRY_SIZE * n);
        self.bytes[ENTRY_SIZE..ENTRY_SIZE + entry_bytes.len()].copy_from_slice(entry_bytes);
        self.set_entries(n);
    }

    /// Moves entries `[at..entries)` into the (empty, same-kind) `into` node —
    /// the split's reorganization step. Memory-only; both nodes still need
    /// [`write_back`](Self::write_back).
    pub(super) fn move_upper_into(&mut self, at: usize, into: &mut NodeBuf) {
        debug_assert!(at <= self.entries());
        debug_assert_eq!(into.entries(), 0);
        debug_assert_eq!(into.is_leaf(), self.is_leaf());
        let n = self.entries();
        let start = ENTRY_SIZE * (1 + at);
        let end = ENTRY_SIZE * (1 + n);
        into.bytes[ENTRY_SIZE..ENTRY_SIZE + (end - start)].copy_from_slice(&self.bytes[start..end]);
        into.set_entries(n - at);
        self.set_entries(at);
    }

    /// Inserts index entry `idx` at position `i`, shifting later entries
    /// right. Fails with `ENOSPC` when the node is full (the caller cascades
    /// the split one level up).
    pub(super) fn insert_index_at(&mut self, i: usize, idx: &RawExtentIdx) -> Result<()> {
        debug_assert!(!self.is_leaf() && i <= self.entries());
        let n = self.entries();
        if self.is_full() {
            return_errno_with_message!(Errno::ENOSPC, "extent index node is full");
        }
        let start = ENTRY_SIZE * (1 + i);
        let end = ENTRY_SIZE * (1 + n);
        self.bytes.copy_within(start..end, start + ENTRY_SIZE);
        self.bytes[start..start + ENTRY_SIZE].copy_from_slice(idx.as_bytes());
        self.set_entries(n + 1);
        Ok(())
    }

    /// Returns whether this node is a leaf (depth 0).
    pub(super) const fn is_leaf(&self) -> bool {
        self.header.is_leaf()
    }

    /// Returns this node's depth (0 = leaf).
    pub(super) const fn depth(&self) -> u16 {
        self.header.depth()
    }

    /// Returns the number of live entries.
    pub(super) const fn entries(&self) -> usize {
        self.header.entries() as usize
    }

    /// Decodes leaf entry `i`. Caller guarantees `is_leaf()` and `i < entries()`
    /// (an in-bounds walk over a validated node; a violation is a programming
    /// error, not a disk-corruption path).
    pub(super) fn extent_at(&self, i: usize) -> Extent {
        debug_assert!(self.is_leaf() && i < self.entries());
        let off = ENTRY_SIZE * (1 + i);
        Extent::from(&RawExtent::from_bytes(&self.bytes[off..off + ENTRY_SIZE]))
    }

    /// Decodes index entry `i`. Caller guarantees `!is_leaf()` and `i < entries()`.
    pub(super) fn index_at(&self, i: usize) -> ExtentIdx {
        debug_assert!(!self.is_leaf() && i < self.entries());
        let off = ENTRY_SIZE * (1 + i);
        ExtentIdx::from(&RawExtentIdx::from_bytes(
            &self.bytes[off..off + ENTRY_SIZE],
        ))
    }

    /// Returns the position of the last index entry whose key is `<= iblock`,
    /// `None` when `iblock` precedes the first entry (the walker then descends
    /// into child 0, Linux-style).
    pub(super) fn index_pos(&self, iblock: Iblock) -> Option<usize> {
        last_key_le(self.entries(), |i| self.index_at(i).block(), iblock)
    }

    /// Returns the position of the last leaf entry whose first block is
    /// `<= iblock`, `None` when `iblock` precedes the first entry.
    pub(super) fn leaf_pos(&self, iblock: Iblock) -> Option<usize> {
        last_key_le(self.entries(), |i| self.extent_at(i).block(), iblock)
    }

    // ---- Editors (the surgery write path, P9a-T2 onward) ----
    // Every editor maintains the header/entries-consistent node invariant;
    // an edited node must reach disk through [`write_back`](Self::write_back)'s
    // capture funnel — the edit itself only touches in-memory bytes.

    /// Overwrites leaf entry `i` in place (a merge bump or an unwritten flip).
    pub(super) fn replace_extent_at(&mut self, i: usize, e: &Extent) {
        debug_assert!(self.is_leaf() && i < self.entries());
        let off = ENTRY_SIZE * (1 + i);
        self.bytes[off..off + ENTRY_SIZE].copy_from_slice(RawExtent::from(e).as_bytes());
    }

    /// Inserts leaf entry `e` at position `i`, shifting later entries right.
    /// Fails with `ENOSPC` when the node is full — the caller then takes the
    /// split path (`make_room_for`) and retries.
    pub(super) fn insert_extent_at(&mut self, i: usize, e: &Extent) -> Result<()> {
        debug_assert!(self.is_leaf() && i <= self.entries());
        let n = self.entries();
        if self.is_full() {
            return_errno_with_message!(Errno::ENOSPC, "extent leaf node is full");
        }
        let start = ENTRY_SIZE * (1 + i);
        let end = ENTRY_SIZE * (1 + n);
        self.bytes.copy_within(start..end, start + ENTRY_SIZE);
        self.bytes[start..start + ENTRY_SIZE].copy_from_slice(RawExtent::from(e).as_bytes());
        self.set_entries(n + 1);
        Ok(())
    }

    /// Removes leaf entry `i`, shifting later entries left (a right-neighbor
    /// absorb after a merge).
    pub(super) fn remove_extent_at(&mut self, i: usize) {
        debug_assert!(self.is_leaf() && i < self.entries());
        let n = self.entries();
        let start = ENTRY_SIZE * (1 + i);
        let end = ENTRY_SIZE * (1 + n);
        self.bytes.copy_within(start + ENTRY_SIZE..end, start);
        self.set_entries(n - 1);
    }

    /// Removes index entry `i`, shifting later entries left — the parent-side
    /// step of pruning an emptied child (Linux `ext4_ext_rm_idx`).
    pub(super) fn remove_index_at(&mut self, i: usize) {
        debug_assert!(!self.is_leaf() && i < self.entries());
        let n = self.entries();
        let start = ENTRY_SIZE * (1 + i);
        let end = ENTRY_SIZE * (1 + n);
        self.bytes.copy_within(start + ENTRY_SIZE..end, start);
        self.set_entries(n - 1);
    }

    /// Rewrites index entry `i`'s key, keeping its child pointer — the
    /// `correct_indexes` step after an insert at a child's position 0.
    pub(super) fn set_index_key_at(&mut self, i: usize, key: Iblock) {
        debug_assert!(!self.is_leaf() && i < self.entries());
        let off = ENTRY_SIZE * (1 + i);
        let mut raw = RawExtentIdx::from_bytes(&self.bytes[off..off + ENTRY_SIZE]);
        raw.block = key;
        self.bytes[off..off + ENTRY_SIZE].copy_from_slice(raw.as_bytes());
    }

    /// Patches the edited node back through its journal capture and, without a
    /// live capture (non-journaled volume), writes it to the device — the same
    /// funnel discipline as the rebuild's node writers (WAL: metadata never
    /// precedes its commit). With `metadata_csum` on, the extent-block tail is
    /// recomputed over the edited bytes first (patch-time funnel, P6b D4).
    pub(super) fn write_back(
        &mut self,
        device: &dyn BlockDevice,
        handle: Option<&journal::Handle>,
        csum_seed: Option<InodeCsumSeed>,
    ) -> Result<()> {
        if let Some(seed) = csum_seed {
            super::tree::stamp_extent_tail(self.bytes.as_mut(), seed);
        }
        let access = journal::get_write_access(handle, self.bid)?;
        access.patch(|buf| buf.copy_from_slice(self.bytes.as_ref()))?;
        if !access.is_live() {
            device.write_val(Bid::new(self.bid).to_offset(), self.bytes.as_ref())?;
        }
        Ok(())
    }

    /// Returns this node's entry capacity (`eh_max`), additionally bounded by
    /// the block size at [`read`](Self::read).
    fn max_entries(&self) -> usize {
        self.header.max() as usize
    }

    /// Updates the entry count in both the on-disk header bytes and the cached
    /// decoded header — the single funnel every editor above goes through.
    fn set_entries(&mut self, n: usize) {
        let mut raw = RawExtentHeader::from_bytes(&self.bytes[0..ENTRY_SIZE]);
        // Lossless: bounded by `max_entries()` (a u16) at every growth site.
        raw.entries = n as u16;
        self.bytes[0..ENTRY_SIZE].copy_from_slice(raw.as_bytes());
        self.header = ExtentHeader::from_trusted(&raw);
    }
}

/// Returns the last position in `0..n` whose key (per `key_at_fn`) is
/// `<= iblock`. Entries are sorted ascending by first logical block — the
/// on-disk invariant every node kind shares — so the scan can stop at the
/// first larger key.
pub(super) fn last_key_le(
    n: usize,
    key_at_fn: impl Fn(usize) -> Iblock,
    iblock: Iblock,
) -> Option<usize> {
    let mut chosen = None;
    for i in 0..n {
        if key_at_fn(i) <= iblock {
            chosen = Some(i);
        } else {
            break;
        }
    }
    chosen
}

/// One level of a search path: the external node the walk went through and the
/// entry index it chose there (interior: the child descended into; leaf: see
/// [`Search`] for the covered/insertion-point encoding).
pub(super) struct PathLevel {
    pub(super) node: NodeBuf,
    pub(super) pos: usize,
}

/// The root→leaf cursor of one search: where each level's walk landed.
///
/// `levels` holds the external nodes from the root's child down to the leaf —
/// empty on a depth-0 (inline-root-only) tree, whose landing position is
/// `root_pos` alone.
pub(super) struct ExtentPath {
    /// Entry index chosen at the inline root (on a depth-0 Gap: the insertion
    /// point a new entry would take there).
    pub(super) root_pos: usize,
    pub(super) levels: Vec<PathLevel>,
}

impl ExtentPath {
    /// Returns the leaf level, `None` on a depth-0 tree (the inline root is
    /// itself the leaf).
    pub(super) fn leaf(&self) -> Option<&PathLevel> {
        self.levels.last()
    }
}

/// The complete answer of a tree search (rule: expose what the walk already
/// knows — the miss case carries the insertion point and the predecessor, so
/// an insert needs no second walk and the allocator gets its goal donor).
pub(super) enum Search {
    /// `extent` covers the queried block; the path's landing position points
    /// at it.
    Covered { path: ExtentPath, extent: Extent },
    /// No extent covers the queried block — a hole. The path's landing
    /// position is the insertion point a new extent keyed at the block would
    /// take in the leaf, and `prev` is the in-leaf predecessor (`None` when
    /// the block precedes the leaf's first entry).
    Gap {
        path: ExtentPath,
        prev: Option<Extent>,
    },
}
