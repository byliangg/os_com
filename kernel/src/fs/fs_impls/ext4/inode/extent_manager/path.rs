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
    super::super::{fs::Ext4, journal, prelude::*},
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
        Ok(Self {
            bid,
            bytes: Box::new(bytes),
            header,
        })
    }

    /// Returns the physical block this node was read from (the capture key of
    /// the surgery write-back, P9a-T2).
    #[expect(dead_code)]
    pub(super) const fn bid(&self) -> Ext4Bid {
        self.bid
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
    // The path payload's production consumer is the surgery write path
    // (P9a-T2 `insert_at` edits exactly the nodes the path names); T1 builds
    // the read side and cross-verifies it in ktest.
    #[expect(dead_code)]
    pub(super) node: NodeBuf,
    #[cfg_attr(not(ktest), expect(dead_code))]
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
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) root_pos: usize,
    pub(super) levels: Vec<PathLevel>,
}

impl ExtentPath {
    /// Returns the leaf level, `None` on a depth-0 tree (the inline root is
    /// itself the leaf).
    #[cfg_attr(not(ktest), expect(dead_code))]
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
    Covered {
        // The paths' production consumer is the surgery write path (P9a-T2).
        #[cfg_attr(not(ktest), expect(dead_code))]
        path: ExtentPath,
        extent: Extent,
    },
    /// No extent covers the queried block — a hole. The path's landing
    /// position is the insertion point a new extent keyed at the block would
    /// take in the leaf, and `prev` is the in-leaf predecessor (`None` when
    /// the block precedes the leaf's first entry).
    Gap {
        #[cfg_attr(not(ktest), expect(dead_code))]
        path: ExtentPath,
        #[cfg_attr(not(ktest), expect(dead_code))]
        prev: Option<Extent>,
    },
}
