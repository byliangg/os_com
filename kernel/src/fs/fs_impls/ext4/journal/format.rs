// SPDX-License-Identifier: MPL-2.0

//! The JBD2 on-disk journal format.
//!
//! This module defines the byte-for-byte layout of the log that jbd2 (the Linux
//! journaling block device) writes into the journal inode. Our images share this
//! format with Linux exactly, so `e2fsck` and a stock Linux kernel can recover
//! and interpret a journal we wrote, and vice versa.
//!
//! # Big-endian
//!
//! Unlike the ext4 filesystem proper (which is little-endian), **every
//! multi-byte field of the jbd2 on-disk format is big-endian.** Each such field
//! is therefore stored in a [`Be16`]/[`Be32`]/[`Be64`] newtype that keeps the
//! bytes in big-endian order on disk and converts on access; the raw structs are
//! never read as native-endian integers.
//!
//! # Supported features
//!
//! We admit only the journal feature set we can honor end-to-end. Of the jbd2
//! INCOMPAT features, only [`INCOMPAT_REVOKE`] is tolerated at mount — but a
//! log that *actually* contains revoke blocks is refused during recovery with
//! `EUCLEAN` rather than under-replayed (the "revoke gap"; full revoke support
//! is Phase 7). See [`INCOMPAT_SUPP`] for the mask.
//!
//! The descriptor-tag *geometry* of the layout-shaping features is nonetheless
//! modeled: every descriptor builder/walker is parameterized over one
//! [`TagLayout`] derived from the feature bits (parse-once, held by
//! [`JournalSuperblock`]):
//!
//! - [`INCOMPAT_64BIT`] widens block tags with a `t_blocknr_high` word,
//! - [`INCOMPAT_CSUM_V2`]/[`INCOMPAT_CSUM_V3`] change the tag size and reserve
//!   a checksum tail at the end of each descriptor block.
//!
//! The csum v2/v3 machinery is complete on both sides: the journal superblock
//! checksum is verified at the [`JournalSuperblock`] parse boundary (P7a-3),
//! the [`JournalCsumSeed`] + [`DescriptorTag`] helpers verify descriptor-tail,
//! commit-block, and per-tag data checksums during recovery (P7a-3), and the
//! write side stamps every one of them (P7a-4): [`TagWriter::put`] stamps the
//! per-tag data checksum, [`TagWriter::finish`] the descriptor tail,
//! [`JournalCsumSeed::stamp_commit_block`] the commit block, and
//! [`RawJournalSuperblock::stamp_checksum`] the superblock (through the
//! serialization funnel
//! [`JournalGeometry::write_superblock`](super::JournalGeometry::write_superblock)).
//! 64-bit tags and csum v2/v3 are therefore admitted at mount
//! ([`INCOMPAT_SUPP`]).
//! [`INCOMPAT_ASYNC_COMMIT`] (removes the trailing commit-block barrier) and
//! [`INCOMPAT_FAST_COMMIT`] (adds a wholly different fast-commit area) change
//! behavior we do not model and remain rejected.

use core::fmt;

use super::{
    super::{checksum, prelude::*},
    Tid,
};

/// A big-endian `u16` as stored on disk (jbd2 is big-endian, unlike ext4 proper).
///
/// Backed by raw bytes (alignment 1) so it never forces padding into the packed
/// on-disk structs it appears in.
#[repr(C)]
#[derive(Clone, Copy, Default, Pod, PartialEq, Eq)]
pub(super) struct Be16([u8; 2]);

impl Be16 {
    /// Wraps a host-order value for on-disk storage (converts to big-endian).
    pub(super) const fn new(v: u16) -> Self {
        Self(v.to_be_bytes())
    }

    /// Returns the logical (host-order) value.
    pub(super) const fn get(self) -> u16 {
        u16::from_be_bytes(self.0)
    }
}

impl Debug for Be16 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Print the logical value; the raw bytes are big-endian.
        Debug::fmt(&self.get(), f)
    }
}

/// A big-endian `u32` as stored on disk (jbd2 is big-endian, unlike ext4 proper).
///
/// Backed by raw bytes (alignment 1) so it never forces padding into the packed
/// on-disk structs it appears in.
#[repr(C)]
#[derive(Clone, Copy, Default, Pod, PartialEq, Eq)]
pub(super) struct Be32([u8; 4]);

impl Be32 {
    /// Wraps a host-order value for on-disk storage (converts to big-endian).
    pub(super) const fn new(v: u32) -> Self {
        Self(v.to_be_bytes())
    }

    /// Returns the logical (host-order) value.
    pub(super) const fn get(self) -> u32 {
        u32::from_be_bytes(self.0)
    }
}

impl Debug for Be32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Print the logical value; the raw bytes are big-endian.
        Debug::fmt(&self.get(), f)
    }
}

/// A big-endian `u64` as stored on disk (jbd2 is big-endian, unlike ext4 proper).
///
/// Backed by raw bytes (alignment 1) so it never forces padding into the packed
/// on-disk structs it appears in (e.g. the 60-byte commit block's unaligned
/// `h_commit_sec`).
#[repr(C)]
#[derive(Clone, Copy, Default, Pod, PartialEq, Eq)]
pub(super) struct Be64([u8; 8]);

impl Be64 {
    /// Wraps a host-order value for on-disk storage (converts to big-endian).
    pub(super) const fn new(v: u64) -> Self {
        Self(v.to_be_bytes())
    }

    /// Returns the logical (host-order) value.
    pub(super) const fn get(self) -> u64 {
        u64::from_be_bytes(self.0)
    }
}

impl Debug for Be64 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Print the logical value; the raw bytes are big-endian.
        Debug::fmt(&self.get(), f)
    }
}

/// Magic signature at the start of every jbd2 block header (`JBD2_MAGIC_NUMBER`).
pub(super) const JBD2_MAGIC: u32 = 0xC03B_3998;

/// Descriptor block: lists the filesystem blocks that follow in this
/// transaction (`JBD2_DESCRIPTOR_BLOCK`).
pub(super) const BLOCKTYPE_DESCRIPTOR: u32 = 1;
/// Commit block: seals a transaction (`JBD2_COMMIT_BLOCK`).
pub(super) const BLOCKTYPE_COMMIT: u32 = 2;
/// Version-1 journal superblock (`JBD2_SUPERBLOCK_V1`).
pub(super) const BLOCKTYPE_SUPERBLOCK_V1: u32 = 3;
/// Version-2 journal superblock (`JBD2_SUPERBLOCK_V2`).
pub(super) const BLOCKTYPE_SUPERBLOCK_V2: u32 = 4;
/// Revoke block: lists blocks that must not be replayed (`JBD2_REVOKE_BLOCK`).
///
/// Referenced by [`recovery`](super::recovery)'s SCAN pass, which rejects a
/// revoke block met during recovery with `EUCLEAN` (we write none; applying
/// them — full PASS_REVOKE — is Phase 7), so it is live even in non-ktest builds.
pub(super) const BLOCKTYPE_REVOKE: u32 = 5;

/// The journaled block was escaped because it began with [`JBD2_MAGIC`]
/// (`JBD2_FLAG_ESCAPE`).
pub(super) const TAG_FLAG_ESCAPE: u16 = 1;
/// This tag reuses the UUID of the previous tag (`JBD2_FLAG_SAME_UUID`).
pub(super) const TAG_FLAG_SAME_UUID: u16 = 2;
/// The tagged block was deleted (`JBD2_FLAG_DELETED`).
// Defined for jbd2 completeness; unused in Phase 4 (no revoke). Unconditional
// `expect` (not `cfg_attr(not(ktest), ...)`) because it is dead in the ktest
// build too, where the tests do not reference it.
#[expect(dead_code)]
pub(super) const TAG_FLAG_DELETED: u16 = 4;
/// This is the last tag in the descriptor block (`JBD2_FLAG_LAST_TAG`).
pub(super) const TAG_FLAG_LAST_TAG: u16 = 8;

/// The jbd2 v1 accumulated commit-block checksum
/// (`JBD2_FEATURE_COMPAT_CHECKSUM`). Not modeled (a COMPAT bit is safe to
/// ignore by definition) — except alongside csum v2/v3, whose commit-block
/// scheme it contradicts: that combination is refused at parse, as in Linux
/// (journal.c:1407-1413). Cleared when the D-4 mount upgrade enables
/// csum_v3, matching `jbd2_journal_set_features` (fs/jbd2/journal.c:
/// 2349-2353 — v3 supersedes the v1 checksum).
pub(super) const COMPAT_CHECKSUM: u32 = 0x1;

/// Revoke records are present (`JBD2_FEATURE_INCOMPAT_REVOKE`). Tolerated at
/// mount; a revoke block actually met during recovery hard-errors (see the
/// module docs).
pub(super) const INCOMPAT_REVOKE: u32 = 0x1;
/// 64-bit block numbers in block tags (`JBD2_FEATURE_INCOMPAT_64BIT`).
pub(super) const INCOMPAT_64BIT: u32 = 0x2;
/// Commit blocks may be written before their data is durable
/// (`JBD2_FEATURE_INCOMPAT_ASYNC_COMMIT`).
// Defined for jbd2 completeness; dead in both builds (Phase 4 rejects it via
// the INCOMPAT_SUPP mask rather than naming it), so an unconditional `expect`.
#[expect(dead_code)]
pub(super) const INCOMPAT_ASYNC_COMMIT: u32 = 0x4;
/// Version-2 checksums (`JBD2_FEATURE_INCOMPAT_CSUM_V2`).
pub(super) const INCOMPAT_CSUM_V2: u32 = 0x8;
/// Version-3 checksums (`JBD2_FEATURE_INCOMPAT_CSUM_V3`).
pub(super) const INCOMPAT_CSUM_V3: u32 = 0x10;
/// Fast-commit area is present (`JBD2_FEATURE_INCOMPAT_FAST_COMMIT`).
#[expect(dead_code)]
pub(super) const INCOMPAT_FAST_COMMIT: u32 = 0x20;

/// The jbd2 INCOMPAT features we admit at mount, enforced by
/// [`load_geometry`](super::load_geometry) (the mount-side admission gate;
/// [`JournalSuperblock`]'s `TryFrom` parses any layout it can model so the
/// verification paths are testable below the gate).
///
/// 64-bit tags and csum v2/v3 are admitted end-to-end as of P7a-4: the
/// layouts parse (P7a-2), recovery verifies their checksums (P7a-3), and the
/// commit pipeline stamps them (P7a-4), so our own commits round-trip through
/// our own recovery on every admitted layout. [`INCOMPAT_REVOKE`] stays
/// tolerated-not-honored — the feature bit passes, but a revoke block actually
/// met during recovery hard-errors (`EUCLEAN`), the "revoke gap" (full support
/// is P7b). csum_v2 + csum_v3 together prescribe contradictory tag layouts
/// and are still refused at parse ([`TagLayout::from_features`]). Async
/// commit and fast commit change behavior we do not model and are rejected
/// outright.
pub(super) const INCOMPAT_SUPP: u32 =
    INCOMPAT_REVOKE | INCOMPAT_64BIT | INCOMPAT_CSUM_V2 | INCOMPAT_CSUM_V3;

/// The only checksum algorithm jbd2 defines for csum v2/v3
/// (`JBD2_CRC32C_CHKSUM`), named by the journal superblock's
/// `s_checksum_type`. Any other value there is rejected at parse, mirroring
/// Linux `journal_check_superblock` (fs/jbd2/journal.c:1417-1420).
pub(super) const JBD2_CRC32C_CHKSUM: u8 = 4;

/// The 12-byte header shared by every jbd2 log block (`journal_header_t`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(super) struct RawJournalHeader {
    /// [`JBD2_MAGIC`] (`h_magic`).
    pub(super) h_magic: Be32,
    /// One of the `BLOCKTYPE_*` constants (`h_blocktype`).
    pub(super) h_blocktype: Be32,
    /// Transaction id this block belongs to (`h_sequence`).
    pub(super) h_sequence: Be32,
}

impl RawJournalHeader {
    /// Parses the 12-byte header from the head of a log-block buffer.
    ///
    /// The header lives at byte offset 0 of every jbd2 log block; this
    /// reinterprets the first bytes with no device read. The recovery scanner
    /// and the checkpoint reader parse headers through this one definition.
    pub(super) fn parse(block: &[u8; BLOCK_SIZE]) -> Self {
        Self::from_bytes(&block[..size_of::<Self>()])
    }
}

const JOURNAL_HEADER_SIZE: usize = 12;
const_assert!(size_of::<RawJournalHeader>() == JOURNAL_HEADER_SIZE);

/// Fixed-length padding for the 40-word reserved tail of the journal
/// superblock. Derived `Default` does not cover arrays longer than 32, so this
/// newtype supplies a manual one (mirroring `InodeTail`/`Reserved` elsewhere).
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(super) struct SuperblockPadding([Be32; 40]);

impl Default for SuperblockPadding {
    fn default() -> Self {
        Self([Be32::default(); 40])
    }
}

/// Fixed-length padding for the 768-byte `s_users` array of the journal
/// superblock (external-journal client UUIDs; unused here). Supplies a manual
/// `Default` for the same reason as [`SuperblockPadding`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(super) struct SuperblockUsers([u8; 768]);

impl Default for SuperblockUsers {
    fn default() -> Self {
        Self([0u8; 768])
    }
}

/// The on-disk journal superblock (`journal_superblock_t`, exactly 1024 bytes).
///
/// It occupies log block 0. Convert to [`JournalSuperblock`] via `TryFrom` for
/// the validated representation. Field names mirror jbd2's `s_*`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(super) struct RawJournalSuperblock {
    /// Shared header; `h_blocktype` is `BLOCKTYPE_SUPERBLOCK_V1`/`_V2`.
    pub(super) header: RawJournalHeader,
    /// Journal device block size in bytes (`s_blocksize`).
    pub(super) s_blocksize: Be32,
    /// Total number of blocks in the journal (`s_maxlen`).
    pub(super) s_maxlen: Be32,
    /// First log block holding log data, past the superblock (`s_first`).
    pub(super) s_first: Be32,
    /// First transaction id expected on recovery (`s_sequence`).
    pub(super) s_sequence: Be32,
    /// Log block where recovery starts; 0 means clean (`s_start`).
    pub(super) s_start: Be32,
    /// Error number recorded by the journal (`s_errno`).
    pub(super) s_errno: Be32,
    /// Compatible feature bits (`s_feature_compat`).
    pub(super) s_feature_compat: Be32,
    /// Incompatible feature bits (`s_feature_incompat`).
    pub(super) s_feature_incompat: Be32,
    /// Read-only compatible feature bits (`s_feature_ro_compat`).
    pub(super) s_feature_ro_compat: Be32,
    /// Journal UUID (`s_uuid`).
    pub(super) s_uuid: [u8; 16],
    /// Number of filesystems sharing this journal (`s_nr_users`).
    pub(super) s_nr_users: Be32,
    /// Block of the dynamic superblock copy (`s_dynsuper`).
    pub(super) s_dynsuper: Be32,
    /// Limit on the blocks per transaction (`s_max_transaction`).
    pub(super) s_max_transaction: Be32,
    /// Limit on the data blocks per transaction (`s_max_trans_data`).
    pub(super) s_max_trans_data: Be32,
    /// Checksum algorithm for the journal (`s_checksum_type`).
    pub(super) s_checksum_type: u8,
    /// Padding after `s_checksum_type` (`s_padding2`).
    pub(super) s_padding2: [u8; 3],
    /// Number of fast-commit blocks (`s_num_fc_blks`).
    pub(super) s_num_fc_blks: Be32,
    /// Head of the fast-commit list (`s_head`).
    pub(super) s_head: Be32,
    /// Reserved padding to the checksum field (`s_padding`).
    pub(super) s_padding: SuperblockPadding,
    /// crc32c of the superblock (`s_checksum`).
    pub(super) s_checksum: Be32,
    /// External-journal client UUIDs (`s_users`).
    pub(super) s_users: SuperblockUsers,
}

const JOURNAL_SUPERBLOCK_SIZE: usize = 1024;
const_assert!(size_of::<RawJournalSuperblock>() == JOURNAL_SUPERBLOCK_SIZE);

impl RawJournalSuperblock {
    /// The superblock's own crc32c (Linux `jbd2_superblock_csum`,
    /// fs/jbd2/journal.c:118-129): `crc32c(!0, ..)` over the **1024-byte
    /// superblock struct** — `sizeof(journal_superblock_t)`, not the whole
    /// 4 KiB block it occupies — with the `s_checksum` field treated as zero.
    ///
    /// Seeded with `!0` directly, not with the UUID-derived
    /// [`JournalCsumSeed`]: the seed protects log blocks, while this checksum
    /// protects the superblock that *carries* the UUID the seed hashes.
    ///
    /// Verified at the [`JournalSuperblock`] parse boundary when the journal
    /// has csum v2/v3; the write side stamps it on every superblock rewrite
    /// via [`Self::stamp_checksum`] (Linux `jbd2_write_superblock`,
    /// journal.c:1812-1813).
    pub(super) fn checksum(&self) -> u32 {
        const CHECKSUM_OFFSET: usize = core::mem::offset_of!(RawJournalSuperblock, s_checksum);
        // Hash around the s_checksum hole: bytes before it, four zero bytes in
        // its place, bytes after it (crc32c segments chain).
        let bytes = self.as_bytes();
        let head = checksum::crc32c(!0, &bytes[..CHECKSUM_OFFSET]);
        let hole = checksum::crc32c(head, &[0u8; size_of::<Be32>()]);
        checksum::crc32c(hole, &bytes[CHECKSUM_OFFSET + size_of::<Be32>()..])
    }

    /// Stamps `s_checksum` when this superblock's **own** INCOMPAT bits carry
    /// csum v2/v3 — the write-side mirror of the parse-boundary verification.
    ///
    /// Keyed off the bytes being written, not off any parsed state, so the
    /// D-4 mount upgrade — whose raw is one feature set ahead of the parsed
    /// geometry — stamps correctly through the same funnel. A featureless
    /// superblock is left untouched: the v0 byte path stays frozen.
    ///
    /// Every serialization goes through
    /// [`JournalGeometry::write_superblock`](super::JournalGeometry::write_superblock),
    /// which calls this — mirroring Linux, where every journal-superblock
    /// write funnels through `jbd2_write_superblock`, which stamps under csum
    /// v2/v3 (fs/jbd2/journal.c:1812-1813).
    pub(super) fn stamp_checksum(&mut self) {
        if self.s_feature_incompat.get() & (INCOMPAT_CSUM_V2 | INCOMPAT_CSUM_V3) != 0 {
            self.s_checksum = Be32::new(self.checksum());
        }
    }
}

/// The 8-byte head of a non-csum-v3 block tag in a descriptor block
/// (`journal_block_tag_t` truncated after `t_flags`).
///
/// jbd2's `t_blocknr_high` word (present only with [`INCOMPAT_64BIT`]) is
/// deliberately not a field, so this struct stays the 8-byte common prefix: on
/// a 64-bit journal the [`TagLayout`] builder/walker append/read the high word
/// as a separate [`Be32`] right after it (the 12-byte case). A csum-v3 journal
/// uses [`RawJournalBlockTag3`] instead.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(super) struct RawBlockTag {
    /// Low 32 bits of the target filesystem block (`t_blocknr`).
    pub(super) t_blocknr: Be32,
    /// Tag checksum, zero without the csum feature (`t_checksum`).
    pub(super) t_checksum: Be16,
    /// `TAG_FLAG_*` bits (`t_flags`).
    pub(super) t_flags: Be16,
}

const BLOCK_TAG_SIZE: usize = 8;
const_assert!(size_of::<RawBlockTag>() == BLOCK_TAG_SIZE);

/// A 16-byte csum-v3 block tag in a descriptor block (`journal_block_tag3_t`).
///
/// Unlike [`RawBlockTag`], the flags widen to 32 bits and the (full crc32c)
/// checksum moves to a trailing 32-bit word. `t_blocknr`/`t_blocknr_high` sit
/// at the same offsets (0 and 8) as in the 12-byte 64-bit tag, which is what
/// lets Linux read both through one struct. Recovery verifies the checksum
/// ([`DescriptorTag::verify_data_csum`]); the writer stamps it
/// ([`TagWriter::put`]).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(super) struct RawJournalBlockTag3 {
    /// Low 32 bits of the target filesystem block (`t_blocknr`).
    pub(super) t_blocknr: Be32,
    /// `TAG_FLAG_*` bits (`t_flags`); the value fits 16 bits — Linux writes it
    /// through the 8-byte tag's `__be16` at offset 6, which encodes the same
    /// bytes.
    pub(super) t_flags: Be32,
    /// High 32 bits of the target filesystem block (`t_blocknr_high`), zero
    /// unless [`INCOMPAT_64BIT`] is on.
    pub(super) t_blocknr_high: Be32,
    /// Tag checksum (`t_checksum`), stamped by [`TagWriter::put`].
    pub(super) t_checksum: Be32,
}

const BLOCK_TAG3_SIZE: usize = 16;
const_assert!(size_of::<RawJournalBlockTag3>() == BLOCK_TAG3_SIZE);

/// The 16-byte journal UUID that follows a descriptor tag lacking
/// [`TAG_FLAG_SAME_UUID`] — per the writer, exactly the first tag. We carry no
/// journal UUID, so the region is written as zeros and skipped on read.
const TAG_UUID_BYTES: usize = 16;

/// Size of `struct jbd2_journal_block_tail`: one big-endian u32 crc32c of the
/// whole descriptor block, reserved at the *end* of the tag area when
/// [`INCOMPAT_CSUM_V2`] or [`INCOMPAT_CSUM_V3`] is on. Recovery verifies it
/// via [`JournalCsumSeed::verify_block_tail`]; the writer stamps it via
/// [`TagWriter::finish`].
const DESCRIPTOR_TAIL_BYTES: usize = 4;

/// The per-journal csum v2/v3 seed: `crc32c(!0, s_uuid)` of the **journal**
/// superblock's UUID (Linux `j_csum_seed`, derived in
/// `journal_load_superblock`, fs/jbd2/journal.c:1493-1495).
///
/// Deliberately a distinct newtype from the filesystem's
/// [`FsCsumSeed`](checksum::FsCsumSeed): the two hash different superblocks'
/// UUIDs (equal for an internal journal, but distinct semantic layers — an
/// external journal carries its own UUID) and feed entirely different
/// checksum formulas. Collapsing two seed layers into one type was the exact
/// mistake the P6b review split `FsCsumSeed`/`InodeCsumSeed` to fix.
///
/// Derived once at the [`JournalSuperblock`] parse boundary — only when the
/// journal carries csum v2/v3, so *holding* one is the license to verify —
/// and owned by it. The log-block checksum formulas hang off this type: the
/// seed is what turns "bytes + polynomial" into "*this* journal's checksum".
/// The `*_csum` computations are shared by recovery's verification and the
/// commit pipeline's stamping ([`TagWriter::put`]/[`TagWriter::finish`]/
/// [`Self::stamp_commit_block`]), so both sides compute one formula.
#[derive(Clone, Copy, Debug)]
pub(super) struct JournalCsumSeed(u32);

impl JournalCsumSeed {
    /// Derives the seed from the journal superblock's UUID
    /// (`crc32c(!0, uuid)`, fs/jbd2/journal.c:1493-1495).
    fn derive(uuid: &[u8; 16]) -> Self {
        Self(checksum::crc32c(!0, uuid))
    }

    /// Test-only constructor: derives a seed from an arbitrary UUID so
    /// sibling modules' fixtures can exercise the stamping paths without a
    /// parsed superblock. Production code receives a seed only from the
    /// [`JournalSuperblock`] parse boundary.
    #[cfg(ktest)]
    pub(super) fn for_test(uuid: &[u8; 16]) -> Self {
        Self::derive(uuid)
    }

    /// The crc32c of a descriptor-class block with its trailing 4-byte
    /// `jbd2_journal_block_tail` treated as zero — the value the tail stores.
    pub(super) fn block_tail_csum(&self, block: &[u8; BLOCK_SIZE]) -> u32 {
        // Zeroing the tail and hashing the whole block equals hashing the
        // body and then four zero bytes (crc32c segments chain).
        let body = checksum::crc32c(self.0, &block[..BLOCK_SIZE - DESCRIPTOR_TAIL_BYTES]);
        checksum::crc32c(body, &[0u8; DESCRIPTOR_TAIL_BYTES])
    }

    /// Verifies the trailing `jbd2_journal_block_tail` checksum of a
    /// descriptor block (Linux `jbd2_descriptor_block_csum_verify`,
    /// fs/jbd2/recovery.c:179-196): the big-endian crc32c stored at
    /// `BLOCK_SIZE - 4` must equal [`Self::block_tail_csum`].
    ///
    /// Deliberately generic over the block class: revoke blocks end in the
    /// same tail checksum and Linux verifies them through this same function
    /// (recovery.c:836), so P7b's revoke support reuses this helper as is.
    pub(super) fn verify_block_tail(&self, block: &[u8; BLOCK_SIZE]) -> bool {
        let stored = Be32::from_bytes(&block[BLOCK_SIZE - DESCRIPTOR_TAIL_BYTES..]).get();
        stored == self.block_tail_csum(block)
    }

    /// The crc32c of a commit block with its `h_chksum[0]` word treated as
    /// zero — the value that word stores. (`h_chksum_type`/`h_chksum_size`
    /// stay zero under csum v2/v3; they belong to the v1 COMPAT checksum.)
    pub(super) fn commit_block_csum(&self, block: &[u8; BLOCK_SIZE]) -> u32 {
        // Hash around the h_chksum[0] hole, as in `block_tail_csum`.
        let head = checksum::crc32c(self.0, &block[..COMMIT_CHKSUM_OFFSET]);
        let hole = checksum::crc32c(head, &[0u8; size_of::<Be32>()]);
        checksum::crc32c(hole, &block[COMMIT_CHKSUM_OFFSET + size_of::<Be32>()..])
    }

    /// Stamps a commit block's `h_chksum[0]` with [`Self::commit_block_csum`]
    /// — the write-side mirror of [`Self::verify_commit_block`] (Linux
    /// `jbd2_commit_block_csum_set`, fs/jbd2/commit.c:89-103).
    /// `h_chksum_type`/`h_chksum_size` stay zero under csum v2/v3 (they
    /// belong to the v1 COMPAT checksum; pinned by the real-Linux-bytes
    /// vectors below).
    pub(super) fn stamp_commit_block(&self, block: &mut [u8; BLOCK_SIZE]) {
        // `commit_block_csum` hashes around the `h_chksum[0]` hole, so the
        // word's current content never folds into its own checksum.
        let csum = self.commit_block_csum(block);
        block[COMMIT_CHKSUM_OFFSET..COMMIT_CHKSUM_OFFSET + size_of::<Be32>()]
            .copy_from_slice(Be32::new(csum).as_bytes());
    }

    /// Verifies a commit block's checksum (Linux
    /// `jbd2_commit_block_csum_verify`, fs/jbd2/recovery.c:425-441): the
    /// big-endian crc32c stored in `h_chksum[0]` must equal
    /// [`Self::commit_block_csum`].
    pub(super) fn verify_commit_block(&self, block: &[u8; BLOCK_SIZE]) -> bool {
        let stored = Be32::from_bytes(
            &block[COMMIT_CHKSUM_OFFSET..COMMIT_CHKSUM_OFFSET + size_of::<Be32>()],
        )
        .get();
        stored == self.commit_block_csum(block)
    }

    /// The crc32c a descriptor tag stores for its data block (Linux
    /// `jbd2_block_tag_csum_verify`, fs/jbd2/recovery.c:443-461, and the
    /// write side `jbd2_block_tag_csum_set`, fs/jbd2/commit.c:319-340):
    /// `crc32c(seed, be32(tid))` folded over the 4 KiB block **as it sits in
    /// the log**. See [`DescriptorTag::verify_data_csum`] for the
    /// escaped-form contract.
    pub(super) fn data_block_csum(&self, tid: Tid, logged_block: &[u8; BLOCK_SIZE]) -> u32 {
        let seq = checksum::crc32c(self.0, &tid.get().to_be_bytes());
        checksum::crc32c(seq, logged_block)
    }
}

/// Byte offset of a commit block's `h_chksum[0]` — the one checksum word csum
/// v2/v3 uses (Linux stores the whole-block crc32c there and leaves the other
/// seven words zero).
const COMMIT_CHKSUM_OFFSET: usize = core::mem::offset_of!(RawCommitBlock, h_chksum);

/// The descriptor-block tag geometry a journal's INCOMPAT feature bits select
/// — the single source of truth for the byte layout of the tag array, derived
/// once (parse-once, held by [`JournalSuperblock`]) and consumed by the commit
/// writer, the recovery scanner, and the checkpoint/replay applier.
///
/// Tag sizes follow Linux 6.6 `journal_tag_bytes()` (fs/jbd2/journal.c)
/// exactly:
///
/// | features           | tag bytes | descriptor tail |
/// |--------------------|-----------|-----------------|
/// | (none) / revoke    | 8         | 0               |
/// | 64bit              | 12        | 0               |
/// | csum_v2            | 10        | 4               |
/// | csum_v2 + 64bit    | 14        | 4               |
/// | csum_v3 (± 64bit)  | 16        | 4               |
///
/// The odd csum_v2 sizes are jbd2's frozen on-disk quirk: the tag-size
/// function that shipped with csum_v2 over-counted by two bytes, and csum_v3
/// exists to supersede it (Linux commit `db9ee220361d`, "jbd2: fix descriptor
/// block size handling errors with journal_csum"). We reproduce the quirk so a
/// Linux-written csum_v2 log parses (and our own writes stride) exactly as
/// jbd2's would — csum_v2 is admitted since P7a-4, though nothing modern
/// creates it (Linux upgrades v2 requests to v3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TagLayout {
    /// Bytes one tag occupies in the descriptor's tag array (the walk stride,
    /// excluding the 16-byte UUID after a non-`SAME_UUID` tag).
    tag_bytes: usize,
    /// 64-bit journal: tags carry a `t_blocknr_high` word.
    has_blocknr_high: bool,
    /// csum-v3 journal: tags are the 16-byte [`RawJournalBlockTag3`].
    csum_v3: bool,
    /// Bytes reserved at the end of the descriptor block for the
    /// `jbd2_journal_block_tail` checksum (0 or [`DESCRIPTOR_TAIL_BYTES`]).
    descriptor_tail_bytes: usize,
}

impl TagLayout {
    /// Derives the tag geometry from the journal superblock's raw INCOMPAT
    /// feature bits (the parse-once boundary; see the type-level table).
    ///
    /// Total over every feature combination except csum_v2 + csum_v3 together,
    /// which jbd2 itself refuses (`journal_get_superblock`) because the two
    /// prescribe contradictory tag layouts.
    pub(super) fn from_features(feature_incompat: u32) -> Result<Self> {
        let csum_v2 = feature_incompat & INCOMPAT_CSUM_V2 != 0;
        let csum_v3 = feature_incompat & INCOMPAT_CSUM_V3 != 0;
        if csum_v2 && csum_v3 {
            return_errno_with_message!(Errno::EINVAL, "journal enables both csum_v2 and csum_v3");
        }
        let has_blocknr_high = feature_incompat & INCOMPAT_64BIT != 0;

        let tag_bytes = if csum_v3 {
            size_of::<RawJournalBlockTag3>()
        } else {
            // The 8-byte common prefix, plus the high word on a 64-bit
            // journal, plus csum_v2's frozen 2-byte stride quirk (see the
            // type-level docs).
            size_of::<RawBlockTag>()
                + if has_blocknr_high {
                    size_of::<Be32>()
                } else {
                    0
                }
                + if csum_v2 { size_of::<Be16>() } else { 0 }
        };
        let descriptor_tail_bytes = if csum_v2 || csum_v3 {
            DESCRIPTOR_TAIL_BYTES
        } else {
            0
        };

        Ok(Self {
            tag_bytes,
            has_blocknr_high,
            csum_v3,
            descriptor_tail_bytes,
        })
    }

    /// The byte offset of the first tag: right past the 12-byte block header.
    const fn first_tag_offset(&self) -> usize {
        size_of::<RawJournalHeader>()
    }

    /// The end of the usable tag area: the block, minus the reserved
    /// descriptor-tail checksum when the layout carries one. Builder and
    /// walkers bound the tag array by this one value, so neither side can
    /// place or parse a tag inside the tail.
    const fn tag_area_end(&self) -> usize {
        BLOCK_SIZE - self.descriptor_tail_bytes
    }

    /// The exact number of block tags one descriptor block holds under this
    /// layout: `n` tags occupy the 12-byte block header, one 16-byte UUID
    /// after the first tag (every later tag sets [`TAG_FLAG_SAME_UUID`] and
    /// reuses it), and `n` tag strides, so `n` fits iff
    /// `header + UUID + n * tag_bytes` fits the tag area. This is the
    /// per-RUN chunk size of the commit pipeline's descriptor chain (P7a-5:
    /// a transaction's captures split into `tags_per_descriptor`-sized runs,
    /// one descriptor each), and the `t` in
    /// [`Journal::max_credits`](super::Journal::max_credits)'s
    /// whole-transaction footprint bound `n + ceil(n/t) + 1 <= usable ring`.
    pub(super) const fn tags_per_descriptor(&self) -> usize {
        (self.tag_area_end() - self.first_tag_offset() - TAG_UUID_BYTES) / self.tag_bytes
    }

    /// Whether this layout carries csum v2 or v3 (Linux
    /// `jbd2_journal_has_csum_v2or3` on the feature bits the layout was
    /// derived from): tags store a data checksum and descriptor-class blocks
    /// reserve the trailing tail checksum.
    pub(super) const fn has_csum(&self) -> bool {
        self.descriptor_tail_bytes != 0
    }

    /// Splits a filesystem block number into the on-disk
    /// `(t_blocknr, t_blocknr_high)` halves (Linux `write_tag_block`).
    fn split_blocknr(blocknr: Ext4Bid) -> (u32, u32) {
        let bytes = blocknr.to_be_bytes();
        let high = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let low = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        (low, high)
    }

    /// Returns a [`TagWriter`] that takes ownership of a descriptor-block
    /// buffer (zeroed past the 12-byte header), positioned at the first tag —
    /// the write-side counterpart of [`walk`](Self::walk). The writer owns
    /// the cursor, so tag placement can neither skip nor reuse an offset; and
    /// it owns the buffer, so the sealed bytes are only obtainable from
    /// [`finish`](TagWriter::finish) — dropping the writer instead of
    /// finishing it drops the descriptor, so an unsealed (tail-less on a csum
    /// layout) descriptor can never be emitted.
    ///
    /// `tid` is the transaction the tags belong to and `seed` the journal's
    /// csum seed; on a csum layout every [`put`](TagWriter::put) stamps the
    /// tag's data checksum from them and [`finish`](TagWriter::finish) stamps
    /// the descriptor tail.
    ///
    /// # Errors
    ///
    /// `EINVAL` when `seed` presence contradicts the layout: a csum layout
    /// must stamp (its zero checksums would fail our own recovery) and a
    /// plain layout has nothing to stamp with. Both derive from the same
    /// superblock feature bits, so a mismatch is a wiring bug, refused before
    /// any tag is placed — a stamped-layout descriptor without checksums
    /// cannot be emitted. The refusal is what makes [`TagCsumMode`] total:
    /// past this constructor, a csum-mode writer always carries its seed.
    pub(super) fn writer(
        &self,
        block: Box<[u8; BLOCK_SIZE]>,
        tid: Tid,
        seed: Option<JournalCsumSeed>,
    ) -> Result<TagWriter> {
        let mode = match seed {
            None if !self.has_csum() => TagCsumMode::Plain,
            Some(seed) if self.csum_v3 => TagCsumMode::V3(seed),
            Some(seed) if self.has_csum() => TagCsumMode::V2(seed),
            _ => return_errno_with_message!(
                Errno::EINVAL,
                "journal csum seed does not match the tag layout"
            ),
        };
        Ok(TagWriter {
            layout: *self,
            block,
            offset: self.first_tag_offset(),
            tid,
            mode,
        })
    }

    /// Decodes the tag at `offset`. The caller ([`TagWalk`]) has already
    /// bounds-checked `offset + tag_bytes` against the tag area.
    fn decode_tag(&self, descriptor: &[u8; BLOCK_SIZE], offset: usize) -> Result<DescriptorTag> {
        if self.csum_v3 {
            let raw =
                RawJournalBlockTag3::from_bytes(&descriptor[offset..offset + BLOCK_TAG3_SIZE]);
            // Linux reads tag3 flags as the low 16 bits of the 32-bit word
            // (its writer only ever stores a 16-bit value there); nonzero high
            // bits are corruption no writer produces, so reject rather than
            // silently mask.
            let Ok(flags) = u16::try_from(raw.t_flags.get()) else {
                return_errno_with_message!(
                    Errno::EUCLEAN,
                    "journal descriptor tag has malformed flags"
                );
            };
            // A tag3 always carries the `t_blocknr_high` word on disk, but it
            // joins the block number only when the journal has
            // [`INCOMPAT_64BIT`]: Linux `read_tag_block` (fs/jbd2/recovery.c)
            // gates the high word on the 64bit FEATURE for both tag formats.
            // csum_v3 without 64bit is a real combination (a metadata_csum
            // filesystem on a < 16 TiB volume), and stray bytes in the unused
            // word must not redirect the replay.
            let high = if self.has_blocknr_high {
                raw.t_blocknr_high.get()
            } else {
                0
            };
            Ok(DescriptorTag {
                blocknr: Self::join_blocknr(raw.t_blocknr.get(), high),
                flags,
                checksum: Some(TagChecksum::V3(raw.t_checksum.get())),
            })
        } else {
            let raw = RawBlockTag::from_bytes(&descriptor[offset..offset + BLOCK_TAG_SIZE]);
            let high = if self.has_blocknr_high {
                let high_offset = offset + BLOCK_TAG_SIZE;
                Be32::from_bytes(&descriptor[high_offset..high_offset + size_of::<Be32>()]).get()
            } else {
                0
            };
            // The 8-byte tag's `t_checksum` bytes exist on every layout, but
            // they carry a checksum only under csum_v2 — without the feature
            // the field is meaningless zero-fill, not a stored value.
            let checksum = self
                .has_csum()
                .then(|| TagChecksum::V2(raw.t_checksum.get()));
            Ok(DescriptorTag {
                blocknr: Self::join_blocknr(raw.t_blocknr.get(), high),
                flags: raw.t_flags.get(),
                checksum,
            })
        }
    }

    /// Joins the on-disk `(t_blocknr, t_blocknr_high)` halves back into the
    /// filesystem block number (Linux `read_tag_block`); lossless by
    /// construction.
    fn join_blocknr(low: u32, high: u32) -> Ext4Bid {
        Ext4Bid::from(low) | (Ext4Bid::from(high) << 32)
    }

    /// Walks the tag array of a descriptor block under this layout, starting
    /// right after the block header. The one shared reader of the tag
    /// geometry: the recovery scanner (PASS_SCAN), the checkpoint/replay
    /// applier, and the round-trip tests all iterate through it, so a reader
    /// can never drift from the writer ([`TagWriter`]).
    pub(super) fn walk<'a>(&self, descriptor: &'a [u8; BLOCK_SIZE]) -> TagWalk<'a> {
        TagWalk {
            layout: *self,
            descriptor,
            offset: self.first_tag_offset(),
            done: false,
        }
    }
}

/// How a [`TagWriter`] stamps checksums — derived once by
/// [`TagLayout::writer`] from the layout and the (validated) seed, so each
/// csum arm *carries* the seed it stamps with and the plain arm carries
/// nothing: "csum layout without a seed" is unrepresentable past the
/// constructor, and [`TagWriter::put`]/[`TagWriter::finish`] match with no
/// `Option` fallback and no zero-checksum escape arm.
#[derive(Clone, Copy)]
enum TagCsumMode {
    /// No csum feature: tags store no checksum (their `t_checksum` bytes are
    /// the v0 format's meaningless zero-fill) and the descriptor has no tail.
    Plain,
    /// csum_v2: each 8-byte tag stores the LOW 16 bits of its data crc32c,
    /// and [`finish`](TagWriter::finish) stamps the descriptor tail.
    V2(JournalCsumSeed),
    /// csum_v3: each 16-byte tag3 stores the full data crc32c, and
    /// [`finish`](TagWriter::finish) stamps the descriptor tail.
    V3(JournalCsumSeed),
}

/// Serializer for the tag array of one descriptor block (see
/// [`TagLayout::writer`]) — the write-side mirror of [`TagWalk`].
///
/// Owns the write cursor: each [`put`](Self::put) lays one tag down at the
/// current offset and advances past it (and, when the tag lacks
/// [`TAG_FLAG_SAME_UUID`], its 16-byte UUID area), so a caller cannot place a
/// tag at a stale or skipped offset — that misuse does not compile.
///
/// Owns the descriptor buffer: the bytes come back only from
/// [`finish`](Self::finish), which seals the block (stamping the tail on a
/// csum layout), so an unsealed descriptor cannot escape — skipping the seal
/// does not compile either.
pub(super) struct TagWriter {
    layout: TagLayout,
    block: Box<[u8; BLOCK_SIZE]>,
    offset: usize,
    /// The transaction the tags belong to; folded into every tag's data
    /// checksum on a csum layout.
    tid: Tid,
    /// The checksum mode (with its seed, on a csum layout) — see
    /// [`TagCsumMode`].
    mode: TagCsumMode,
}

impl TagWriter {
    /// Serializes one block tag at the cursor into the (zeroed) descriptor
    /// buffer and advances the cursor — past the tag and, when this tag lacks
    /// [`TAG_FLAG_SAME_UUID`], its 16-byte UUID (left as zeros; we carry no
    /// journal UUID).
    ///
    /// `logged_block` is the block **as it will sit in the log** — the
    /// post-escape form when the tag carries [`TAG_FLAG_ESCAPE`]. On a csum
    /// layout the tag's data checksum is stamped from it (Linux
    /// `jbd2_block_tag_csum_set`, fs/jbd2/commit.c:319-340, called on the
    /// escaped `wbuf` copy at commit.c:684-685), exactly the bytes recovery
    /// verifies before restoring the magic head
    /// ([`DescriptorTag::verify_data_csum`]). On a plain layout the bytes are
    /// not read.
    ///
    /// # Errors
    ///
    /// - `EFBIG` when `blocknr` does not fit 32 bits and the layout carries no
    ///   `t_blocknr_high` word: truncating would journal the after-image to
    ///   the wrong block on a > 16 TiB volume, so it is a real error, not a
    ///   debug assert.
    /// - `ENOSPC` when the tag (plus its UUID) would run past the tag area —
    ///   into the reserved descriptor tail, or past the block.
    pub(super) fn put(
        &mut self,
        blocknr: Ext4Bid,
        flags: u16,
        logged_block: &[u8; BLOCK_SIZE],
    ) -> Result<()> {
        let (low, high) = TagLayout::split_blocknr(blocknr);
        if high != 0 && !self.layout.has_blocknr_high {
            return_errno_with_message!(Errno::EFBIG, "journal block number exceeds 32-bit tag");
        }

        let offset = self.offset;
        let mut end = offset + self.layout.tag_bytes;
        if flags & TAG_FLAG_SAME_UUID == 0 {
            end += TAG_UUID_BYTES;
        }
        if end > self.layout.tag_area_end() {
            return_errno_with_message!(Errno::ENOSPC, "journal descriptor tag area is full");
        }

        // Each mode carries exactly what its encoding stamps ([`TagCsumMode`]):
        // no arm falls back on a zero checksum, because the mode a csum
        // layout selects always holds its seed (the constructor's invariant,
        // now structural).
        match self.mode {
            TagCsumMode::V3(seed) => {
                let tag3 = RawJournalBlockTag3 {
                    t_blocknr: Be32::new(low),
                    t_flags: Be32::new(u32::from(flags)),
                    t_blocknr_high: Be32::new(high),
                    t_checksum: Be32::new(seed.data_block_csum(self.tid, logged_block)),
                };
                self.block[offset..offset + BLOCK_TAG3_SIZE].copy_from_slice(tag3.as_bytes());
            }
            TagCsumMode::V2(seed) => {
                // csum_v2 stores only the LOW 16 bits of the crc32c — jbd2's
                // frozen `cpu_to_be16(csum32)` truncation (commit.c:339),
                // taken bytewise from the big-endian encoding (no narrowing
                // cast); the read side compares in the widened domain
                // ([`TagChecksum::V2`]).
                let be = seed.data_block_csum(self.tid, logged_block).to_be_bytes();
                self.put_prefix_tag(offset, low, high, Be16([be[2], be[3]]), flags);
            }
            TagCsumMode::Plain => {
                // The v0/64bit tag's `t_checksum` bytes are the format's
                // meaningless zero-fill (no feature, no stored value) — a
                // genuine on-disk zero, not a stand-in for a missing seed.
                self.put_prefix_tag(offset, low, high, Be16::new(0), flags);
            }
        }

        self.offset = end;
        Ok(())
    }

    /// Serializes one non-tag3 tag at `offset`: the 8-byte [`RawBlockTag`]
    /// prefix, plus the high word on a 64-bit layout. Shared by the csum_v2
    /// and plain arms of [`put`](Self::put), which differ only in the
    /// checksum they store in `t_checksum`.
    fn put_prefix_tag(&mut self, offset: usize, low: u32, high: u32, t_checksum: Be16, flags: u16) {
        let tag = RawBlockTag {
            t_blocknr: Be32::new(low),
            t_checksum,
            t_flags: Be16::new(flags),
        };
        self.block[offset..offset + BLOCK_TAG_SIZE].copy_from_slice(tag.as_bytes());
        if self.layout.has_blocknr_high {
            let high_offset = offset + BLOCK_TAG_SIZE;
            self.block[high_offset..high_offset + size_of::<Be32>()]
                .copy_from_slice(Be32::new(high).as_bytes());
        }
        // csum_v2's 2 stride-padding bytes (the frozen quirk) stay zero:
        // the buffer arrives zeroed, exactly like jbd2's memset-clean
        // descriptor buffer.
    }

    /// Seals the descriptor and hands its bytes back: stamps the trailing
    /// `jbd2_journal_block_tail` checksum on a csum layout (Linux
    /// `jbd2_descriptor_block_csum_set`, fs/jbd2/journal.c:1026-1041, run
    /// once after the LAST tag is placed, commit.c:710-714). A plain layout
    /// reserves no tail and the bytes return unchanged.
    ///
    /// Consumes the writer, and is the ONLY way to get the descriptor out of
    /// it: the tail checksum covers every tag byte, so neither placing a tag
    /// after the seal nor emitting an unsealed descriptor compiles.
    pub(super) fn finish(mut self) -> Box<[u8; BLOCK_SIZE]> {
        let seed = match self.mode {
            TagCsumMode::Plain => return self.block,
            TagCsumMode::V2(seed) | TagCsumMode::V3(seed) => seed,
        };
        let tail = seed.block_tail_csum(&self.block);
        self.block[BLOCK_SIZE - DESCRIPTOR_TAIL_BYTES..]
            .copy_from_slice(Be32::new(tail).as_bytes());
        self.block
    }

    /// Test-only: plants the write cursor at a raw byte `offset`, reaching
    /// tag-area boundary cases a sequential [`put`](Self::put) walk cannot
    /// (its offsets are congruent modulo the tag stride). The constructor's
    /// layout↔mode invariant stays intact — only the cursor moves. Private
    /// to this module: only the format tests below may plant a cursor.
    #[cfg(ktest)]
    fn plant_offset_for_test(&mut self, offset: usize) {
        self.offset = offset;
    }
}

/// The checksum a descriptor tag stores for its data block, in the width its
/// layout prescribes — decoded by [`TagLayout::decode_tag`] alongside the
/// block number so no verifier re-derives tag offsets (the walker yields the
/// stored value; rust_rules "expose intermediate results").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TagChecksum {
    /// csum_v2: the 8-byte tag's 16-bit `t_checksum` holds only the **low 16
    /// bits** of the crc32c — jbd2 truncates via `cpu_to_be16(csum32)`
    /// (fs/jbd2/recovery.c:460 / commit.c:339), a frozen on-disk quirk like
    /// the v2 tag strides.
    V2(u16),
    /// csum_v3: the 16-byte tag3's 32-bit `t_checksum` holds the full crc32c.
    V3(u32),
}

/// One decoded descriptor-block tag: the destination block number (both
/// halves already joined on a 64-bit layout), its flags, and — under csum
/// v2/v3 — the stored data-block checksum, so no caller recomputes offsets or
/// re-splits block numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DescriptorTag {
    blocknr: Ext4Bid,
    flags: u16,
    /// The stored data-block checksum; `None` on a layout without csum v2/v3
    /// (the raw field bytes are zero-fill there, not a stored value).
    checksum: Option<TagChecksum>,
}

impl DescriptorTag {
    /// The tag's destination filesystem block.
    pub(super) const fn blocknr(&self) -> Ext4Bid {
        self.blocknr
    }

    /// The raw `TAG_FLAG_*` bits. Production readers use the semantic
    /// accessors below; the round-trip tests assert the exact bits.
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) const fn flags(&self) -> u16 {
        self.flags
    }

    /// Whether the logged block was escaped ([`TAG_FLAG_ESCAPE`]): its first
    /// four bytes were zeroed in the log and must be restored to
    /// [`JBD2_MAGIC`] on apply.
    pub(super) const fn is_escaped(&self) -> bool {
        self.flags & TAG_FLAG_ESCAPE != 0
    }

    /// Whether this is the last tag of the descriptor ([`TAG_FLAG_LAST_TAG`]).
    pub(super) const fn is_last(&self) -> bool {
        self.flags & TAG_FLAG_LAST_TAG != 0
    }

    /// Whether this tag reuses the previous tag's UUID
    /// ([`TAG_FLAG_SAME_UUID`]), i.e. no UUID follows it on disk.
    const fn reuses_uuid(&self) -> bool {
        self.flags & TAG_FLAG_SAME_UUID != 0
    }

    /// Verifies this tag's stored data-block checksum against the logged
    /// bytes (Linux `jbd2_block_tag_csum_verify`, fs/jbd2/recovery.c:443-461):
    /// `crc32c(seed, be32(tid))` folded over the block **as it sits in the
    /// log** — the possibly ESCAPE-mangled form. Both sides hash the logged
    /// bytes, never the restored ones: the writer checksums the escaped copy
    /// it queues (`jbd2_block_tag_csum_set` on `wbuf`, fs/jbd2/commit.c:684),
    /// and recovery verifies the log block before restoring the magic head
    /// (recovery.c:656 verifies, :686 restores). Callers must therefore pass
    /// the block bytes *before* any escape restoration.
    ///
    /// A tag from a layout without csum v2/v3 verifies vacuously (Linux
    /// returns 1 when `!jbd2_journal_has_csum_v2or3`); callers gate on the
    /// journal's [`JournalCsumSeed`] being present, which derives from the
    /// same feature bits as the tag layout, so the `None` arm never carries a
    /// verification decision on a csum journal.
    pub(super) fn verify_data_csum(
        &self,
        seed: JournalCsumSeed,
        tid: Tid,
        logged_block: &[u8; BLOCK_SIZE],
    ) -> bool {
        let Some(stored) = self.checksum else {
            return true;
        };
        let csum32 = seed.data_block_csum(tid, logged_block);
        match stored {
            // v2 stores only the low half (see [`TagChecksum::V2`]); compare
            // in the wide domain rather than narrowing the computed value.
            TagChecksum::V2(low16) => u32::from(low16) == csum32 & 0xFFFF,
            TagChecksum::V3(full) => full == csum32,
        }
    }
}

/// Iterator over the tags of one descriptor block (see [`TagLayout::walk`]).
///
/// Yields each decoded [`DescriptorTag`] in on-disk order and stops after the
/// [`TAG_FLAG_LAST_TAG`] tag. A tag that would run past the tag area (which
/// excludes the reserved checksum tail), or that fails to decode, yields one
/// `Err` and ends the walk — a malformed descriptor is reported, never
/// panicked on; the recovery scanner maps the error to its log boundary while
/// the checkpoint/replay applier propagates it as corruption.
pub(super) struct TagWalk<'a> {
    layout: TagLayout,
    descriptor: &'a [u8; BLOCK_SIZE],
    offset: usize,
    done: bool,
}

impl Iterator for TagWalk<'_> {
    type Item = Result<DescriptorTag>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if self.offset + self.layout.tag_bytes > self.layout.tag_area_end() {
            self.done = true;
            return Some(Err(Error::with_message(
                Errno::EUCLEAN,
                "journal descriptor tag runs past the tag area",
            )));
        }
        let tag = match self.layout.decode_tag(self.descriptor, self.offset) {
            Ok(tag) => tag,
            Err(e) => {
                self.done = true;
                return Some(Err(e));
            }
        };
        self.offset += self.layout.tag_bytes;
        if !tag.reuses_uuid() {
            self.offset += TAG_UUID_BYTES;
        }
        if tag.is_last() {
            self.done = true;
        }
        Some(Ok(tag))
    }
}

/// The on-disk commit block (`commit_header`, exactly 60 bytes) that seals a
/// transaction.
///
/// Under csum v2/v3 the block's crc32c lives in `h_chksum[0]`: the commit
/// pipeline stamps it ([`JournalCsumSeed::stamp_commit_block`]) and recovery
/// verifies it ([`JournalCsumSeed::verify_commit_block`]); `h_chksum_type` /
/// `h_chksum_size` stay zero (they belong to the v1 COMPAT checksum we do not
/// model).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(super) struct RawCommitBlock {
    /// Shared header; `h_blocktype` is [`BLOCKTYPE_COMMIT`].
    pub(super) header: RawJournalHeader,
    /// Checksum algorithm (`h_chksum_type`).
    pub(super) h_chksum_type: u8,
    /// Checksum size in bytes (`h_chksum_size`).
    pub(super) h_chksum_size: u8,
    /// Padding after the checksum descriptor (`h_padding`).
    pub(super) h_padding: [u8; 2],
    /// Commit-block checksum words (`h_chksum`).
    pub(super) h_chksum: [Be32; 8],
    /// Commit time, seconds (`h_commit_sec`).
    pub(super) h_commit_sec: Be64,
    /// Commit time, nanoseconds (`h_commit_nsec`).
    pub(super) h_commit_nsec: Be32,
}

const COMMIT_BLOCK_SIZE: usize = 60;
const_assert!(size_of::<RawCommitBlock>() == COMMIT_BLOCK_SIZE);

/// A validated, Rust-typed view of the journal superblock.
///
/// Built from [`RawJournalSuperblock`] via `TryFrom`, which rejects a
/// superblock whose magic, block type, block size, geometry, or checksum is
/// bad, or whose feature bits prescribe a layout we cannot model (csum_v2 +
/// csum_v3 together, or either with the v1 COMPAT checksum). Whether the
/// feature set is *admitted* for a real mount is
/// [`load_geometry`](super::load_geometry)'s separate policy gate
/// ([`INCOMPAT_SUPP`]); parse handles every modelable layout so recovery's
/// verification paths are testable below that gate.
///
/// Feature words are read only from a [`BLOCKTYPE_SUPERBLOCK_V2`]
/// superblock: on a V1 they are meaningless bytes (Linux
/// `journal_check_superblock` succeeds before any feature check via
/// `jbd2_format_support_feature`, fs/jbd2/journal.c:1379-1380), so a V1
/// always parses as a featureless v0 journal, whatever its feature words
/// hold.
#[derive(Clone, Copy, Debug)]
pub(super) struct JournalSuperblock {
    /// Total number of log blocks (`s_maxlen`).
    maxlen: u32,
    /// First log block holding log data, past the superblock (`s_first`).
    first: u32,
    /// First transaction id expected on recovery (`s_sequence`).
    sequence: Tid,
    /// Log block where recovery starts; 0 means clean (`s_start`).
    start: u32,
    /// Journal block size in bytes (`s_blocksize`).
    blocksize: u32,
    /// The descriptor-tag geometry the INCOMPAT feature bits select, derived
    /// once here so every walker/builder trusts it (parse-once).
    tag_layout: TagLayout,
    /// The csum v2/v3 seed, present iff the journal carries either checksum
    /// feature — derived once here from `s_uuid` (parse-once), after the
    /// superblock's own checksum proved the UUID trustworthy.
    csum_seed: Option<JournalCsumSeed>,
}

impl TryFrom<RawJournalSuperblock> for JournalSuperblock {
    type Error = Error;

    fn try_from(raw: RawJournalSuperblock) -> Result<Self> {
        if raw.header.h_magic.get() != JBD2_MAGIC {
            return_errno_with_message!(Errno::EINVAL, "bad jbd2 journal superblock magic");
        }

        let blocktype = raw.header.h_blocktype.get();
        if blocktype != BLOCKTYPE_SUPERBLOCK_V2 && blocktype != BLOCKTYPE_SUPERBLOCK_V1 {
            return_errno_with_message!(Errno::EINVAL, "journal superblock has a bad block type");
        }

        let blocksize = raw.s_blocksize.get();
        if blocksize != BLOCK_SIZE as u32 {
            return_errno_with_message!(
                Errno::EINVAL,
                "unsupported journal block size (4 KiB only)"
            );
        }

        let maxlen = raw.s_maxlen.get();
        if maxlen < 2 {
            return_errno_with_message!(Errno::EUCLEAN, "journal is too small");
        }

        let first = raw.s_first.get();
        if first == 0 || first >= maxlen {
            return_errno_with_message!(Errno::EUCLEAN, "journal s_first out of range");
        }

        // Feature words are meaningful only on a V2 superblock: Linux's
        // `journal_check_superblock` succeeds on a V1 BEFORE every feature
        // check (`jbd2_format_support_feature` is false for
        // `BLOCKTYPE_SUPERBLOCK_V1`, fs/jbd2/journal.c:1379-1380), so
        // whatever bytes a V1's feature words carry select nothing: v0 tag
        // geometry, no checksum expectations. Zeroing the bits here makes
        // every feature-derived gate below (the v2×v3 and v1×v2/3 conflicts,
        // the checksum-type demand) vacuous on a V1, exactly Linux's
        // early-return.
        let feature_incompat = if blocktype == BLOCKTYPE_SUPERBLOCK_V2 {
            raw.s_feature_incompat.get()
        } else {
            0
        };

        // Derive the tag geometry before the checksum gate below, mirroring
        // Linux `journal_check_superblock`'s order (fs/jbd2/journal.c:
        // 1399-1405 rejects the contradictory csum_v2 + csum_v3 combination
        // before either bit selects a checksum formula). Note the *admission*
        // gate (INCOMPAT_SUPP) is not here: it is mount policy, enforced by
        // `load_geometry` on the raw feature bits.
        let tag_layout = TagLayout::from_features(feature_incompat)?;

        // The jbd2 v1 COMPAT checksum and csum v2/v3 prescribe contradictory
        // commit-block checksum schemes; refuse the combination in Linux's
        // order — after the v2×v3 refusal, before the checksum-type gate
        // ("Can't have checksum v1 and v2/3 at the same time!",
        // fs/jbd2/journal.c:1407-1413). `has_csum()` is false on a V1
        // superblock (feature bits zeroed above), so its COMPAT word — also
        // meaningless — never trips this.
        if tag_layout.has_csum() && raw.s_feature_compat.get() & COMPAT_CHECKSUM != 0 {
            return_errno_with_message!(
                Errno::EINVAL,
                "journal enables both the v1 and v2/3 checksums"
            );
        }

        // Integrity gate (Linux journal_check_superblock, journal.c:
        // 1416-1433): with csum v2/v3 on, the superblock names its algorithm
        // and carries its own checksum; verify both before trusting any field
        // further, and derive the per-journal seed the log-block verifiers
        // use (journal.c:1493-1495 derives j_csum_seed at the same boundary).
        let csum_seed = if tag_layout.has_csum() {
            if raw.s_checksum_type != JBD2_CRC32C_CHKSUM {
                return_errno_with_message!(Errno::EINVAL, "unsupported journal checksum type");
            }
            if raw.s_checksum.get() != raw.checksum() {
                return_errno_with_message!(Errno::EUCLEAN, "journal superblock checksum mismatch");
            }
            Some(JournalCsumSeed::derive(&raw.s_uuid))
        } else {
            None
        };

        Ok(Self {
            maxlen,
            first,
            // `s_sequence` is the first transaction id; recovery starts here.
            sequence: Tid::new(raw.s_sequence.get()),
            // `s_start` may be nonzero: a dirty journal awaiting recovery. Just
            // record it.
            start: raw.s_start.get(),
            blocksize,
            tag_layout,
            csum_seed,
        })
    }
}

impl JournalSuperblock {
    /// Returns the total number of log blocks (`s_maxlen`).
    pub(super) const fn maxlen(&self) -> u32 {
        self.maxlen
    }

    /// Returns the first log block that holds log data (`s_first`).
    pub(super) const fn first(&self) -> u32 {
        self.first
    }

    /// Returns the first transaction id expected on recovery (`s_sequence`).
    pub(super) const fn sequence(&self) -> Tid {
        self.sequence
    }

    /// Returns the log block where recovery starts; 0 means clean (`s_start`).
    pub(super) const fn start(&self) -> u32 {
        self.start
    }

    /// Returns the journal block size in bytes (`s_blocksize`).
    pub(super) const fn blocksize(&self) -> u32 {
        self.blocksize
    }

    /// Returns the descriptor-tag geometry derived (once, at parse) from the
    /// superblock's INCOMPAT feature bits.
    pub(super) const fn tag_layout(&self) -> TagLayout {
        self.tag_layout
    }

    /// Returns the csum v2/v3 seed, present iff the journal carries either
    /// checksum feature (derived once, at parse, from the superblock UUID).
    pub(super) const fn csum_seed(&self) -> Option<JournalCsumSeed> {
        self.csum_seed
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;

    /// Builds a minimal valid raw journal superblock with the given geometry.
    fn valid_raw(maxlen: u32, first: u32, sequence: u32, start: u32) -> RawJournalSuperblock {
        RawJournalSuperblock {
            header: RawJournalHeader {
                h_magic: Be32::new(JBD2_MAGIC),
                h_blocktype: Be32::new(BLOCKTYPE_SUPERBLOCK_V2),
                h_sequence: Be32::new(0),
            },
            s_blocksize: Be32::new(BLOCK_SIZE as u32),
            s_maxlen: Be32::new(maxlen),
            s_first: Be32::new(first),
            s_sequence: Be32::new(sequence),
            s_start: Be32::new(start),
            s_nr_users: Be32::new(1),
            ..Default::default()
        }
    }

    #[ktest]
    fn be_newtypes_round_trip() {
        assert_eq!(Be16::new(0x1234).get(), 0x1234);
        assert_eq!(Be32::new(0xC03B_3998).get(), 0xC03B_3998);
        assert_eq!(
            Be64::new(0x0123_4567_89AB_CDEF).get(),
            0x0123_4567_89AB_CDEF
        );
        // The stored bytes are byte-swapped relative to host order.
        assert_eq!(Be32::new(0xC03B_3998).as_bytes(), &[0xC0, 0x3B, 0x39, 0x98]);
    }

    /// The on-disk journal superblock must reproduce the ground-truth bytes of a
    /// real `mke2fs`-made journal (physical block 521 of a `-O has_journal`
    /// image): magic, block type, and the leading geometry fields.
    #[ktest]
    fn journal_superblock_ground_truth_bytes() {
        let raw = valid_raw(1024, 1, 1, 0);
        let bytes = raw.as_bytes();
        // h_magic = 0xC03B3998, h_blocktype = 4 (SUPERBLOCK_V2), h_sequence = 0.
        assert_eq!(&bytes[0..4], &[0xC0, 0x3B, 0x39, 0x98]);
        assert_eq!(&bytes[4..8], &[0x00, 0x00, 0x00, 0x04]);
        assert_eq!(&bytes[8..12], &[0x00, 0x00, 0x00, 0x00]);
        // s_blocksize = 4096, s_maxlen = 1024, s_first = 1.
        assert_eq!(&bytes[12..16], &[0x00, 0x00, 0x10, 0x00]);
        assert_eq!(&bytes[16..20], &[0x00, 0x00, 0x04, 0x00]);
        assert_eq!(&bytes[20..24], &[0x00, 0x00, 0x00, 0x01]);
        // s_sequence = 1.
        assert_eq!(&bytes[24..28], &[0x00, 0x00, 0x00, 0x01]);
        // s_nr_users = 1 (offset 64).
        assert_eq!(&bytes[64..68], &[0x00, 0x00, 0x00, 0x01]);
    }

    #[ktest]
    fn parse_valid_journal_superblock() {
        let raw = valid_raw(1024, 1, 7, 3);
        let sb = JournalSuperblock::try_from(raw).unwrap();
        assert_eq!(sb.maxlen(), 1024);
        assert_eq!(sb.first(), 1);
        assert_eq!(sb.sequence(), Tid::new(7));
        // A nonzero `s_start` (a dirty journal awaiting recovery) is accepted.
        assert_eq!(sb.start(), 3);
        assert_eq!(sb.blocksize(), BLOCK_SIZE as u32);
    }

    #[ktest]
    fn reject_bad_magic() {
        let mut raw = valid_raw(1024, 1, 1, 0);
        raw.header.h_magic = Be32::new(0x1234_5678);
        assert!(JournalSuperblock::try_from(raw).is_err());
    }

    #[ktest]
    fn reject_bad_blocktype() {
        let mut raw = valid_raw(1024, 1, 1, 0);
        raw.header.h_blocktype = Be32::new(BLOCKTYPE_DESCRIPTOR);
        assert!(JournalSuperblock::try_from(raw).is_err());
    }

    #[ktest]
    fn accept_superblock_v1_blocktype() {
        let mut raw = valid_raw(1024, 1, 1, 0);
        raw.header.h_blocktype = Be32::new(BLOCKTYPE_SUPERBLOCK_V1);
        assert!(JournalSuperblock::try_from(raw).is_ok());
    }

    #[ktest]
    fn reject_bad_blocksize() {
        let mut raw = valid_raw(1024, 1, 1, 0);
        raw.s_blocksize = Be32::new(1024);
        assert!(JournalSuperblock::try_from(raw).is_err());
    }

    #[ktest]
    fn reject_bad_first() {
        let raw = valid_raw(1024, 0, 1, 0); // s_first == 0
        assert!(JournalSuperblock::try_from(raw).is_err());

        let raw = valid_raw(4, 4, 1, 0); // s_first == s_maxlen
        assert!(JournalSuperblock::try_from(raw).is_err());
    }

    #[ktest]
    fn reject_too_small() {
        let raw = valid_raw(1, 1, 1, 0);
        assert!(JournalSuperblock::try_from(raw).is_err());
    }

    /// Parse (`TryFrom`) accepts every modelable layout — admission is
    /// `load_geometry`'s separate mount gate (which still refuses these; see
    /// the mod.rs test) — but a csum layout must pass the integrity gate:
    /// a named crc32c algorithm and a matching superblock checksum.
    #[ktest]
    fn parse_gates_csum_layouts_on_superblock_integrity() {
        // 64bit alone: no csum feature, parses with no seed.
        let mut raw = valid_raw(1024, 1, 1, 0);
        raw.s_feature_incompat = Be32::new(INCOMPAT_64BIT);
        let sb = JournalSuperblock::try_from(raw).unwrap();
        assert!(sb.csum_seed().is_none());

        // csum_v3 without a checksum type: EINVAL (journal.c:1417-1420).
        let mut raw = valid_raw(1024, 1, 1, 0);
        raw.s_feature_incompat = Be32::new(INCOMPAT_CSUM_V3);
        let err = JournalSuperblock::try_from(raw).unwrap_err();
        assert_eq!(err.error(), Errno::EINVAL);

        // csum_v3 with crc32c named but a wrong stored checksum: EUCLEAN
        // (journal.c:1429-1433).
        raw.s_checksum_type = JBD2_CRC32C_CHKSUM;
        raw.s_checksum = Be32::new(raw.checksum() ^ 1);
        let err = JournalSuperblock::try_from(raw).unwrap_err();
        assert_eq!(err.error(), Errno::EUCLEAN);

        // A correct checksum parses, and the seed appears.
        raw.s_checksum = Be32::new(raw.checksum());
        let sb = JournalSuperblock::try_from(raw).unwrap();
        assert!(sb.csum_seed().is_some());
    }

    /// The jbd2 v1 COMPAT checksum and csum v2/v3 prescribe contradictory
    /// commit-block checksum schemes: Linux refuses the combination ("Can't
    /// have checksum v1 and v2/3 at the same time!", journal.c:1407-1413,
    /// `-EINVAL`). Pinned on an otherwise fully valid csum superblock (named
    /// algorithm, correct self-checksum), so it is THIS gate that refuses,
    /// not the checksum-type or integrity one. `COMPAT_CHECKSUM` alone stays
    /// ignorable — a COMPAT bit by definition.
    #[ktest]
    fn reject_v1_checksum_with_csum_v2or3() {
        for csum_feature in [INCOMPAT_CSUM_V2, INCOMPAT_CSUM_V3] {
            let mut raw = valid_raw(1024, 1, 1, 0);
            raw.s_feature_incompat = Be32::new(csum_feature);
            raw.s_feature_compat = Be32::new(COMPAT_CHECKSUM);
            raw.s_checksum_type = JBD2_CRC32C_CHKSUM;
            raw.s_checksum = Be32::new(raw.checksum());
            let err = JournalSuperblock::try_from(raw).unwrap_err();
            assert_eq!(err.error(), Errno::EINVAL, "incompat {csum_feature:#x}");
        }

        let mut raw = valid_raw(1024, 1, 1, 0);
        raw.s_feature_compat = Be32::new(COMPAT_CHECKSUM);
        assert!(JournalSuperblock::try_from(raw).is_ok());
    }

    /// Feature words are meaningless on a V1-blocktype superblock: Linux's
    /// `journal_check_superblock` succeeds on a V1 BEFORE any feature check
    /// (`jbd2_format_support_feature` is false, journal.c:1379-1380). A V1
    /// with garbage in ALL THREE feature words must parse as a featureless
    /// v0 journal — 8-byte tags, no seed, and none of the feature-derived
    /// refusals (v2×v3, v1×v2/3, checksum type/integrity) in play.
    #[ktest]
    fn v1_blocktype_ignores_feature_words() {
        let mut raw = valid_raw(1024, 1, 1, 0);
        raw.header.h_blocktype = Be32::new(BLOCKTYPE_SUPERBLOCK_V1);
        // On a V2 superblock these would refuse the parse three ways over
        // (v2 + v3 together, v1 + v2/3 together, and no crc32c named).
        raw.s_feature_compat = Be32::new(COMPAT_CHECKSUM | 0xDEAD_0000);
        raw.s_feature_incompat =
            Be32::new(INCOMPAT_CSUM_V2 | INCOMPAT_CSUM_V3 | INCOMPAT_64BIT | 0x40);
        raw.s_feature_ro_compat = Be32::new(0xFFFF_FFFF);

        let sb = JournalSuperblock::try_from(raw).unwrap();
        assert_eq!(sb.tag_layout(), TagLayout::from_features(0).unwrap());
        assert!(sb.csum_seed().is_none());
    }

    /// The superblock checksum covers everything BUT its own field: patching
    /// `s_checksum` leaves `checksum()` unchanged, patching any covered byte
    /// changes it.
    #[ktest]
    fn superblock_checksum_excludes_own_field() {
        let raw = valid_raw(1024, 1, 1, 0);
        let base = raw.checksum();

        let mut stamped = raw;
        stamped.s_checksum = Be32::new(base);
        assert_eq!(stamped.checksum(), base);

        let mut touched = raw;
        touched.s_sequence = Be32::new(2);
        assert_ne!(touched.checksum(), base);
    }

    #[ktest]
    fn accept_revoke_incompat() {
        let mut raw = valid_raw(1024, 1, 1, 0);
        raw.s_feature_incompat = Be32::new(INCOMPAT_REVOKE);
        assert!(JournalSuperblock::try_from(raw).is_ok());
    }

    /// Feature bits → tag geometry, the Linux 6.6 `journal_tag_bytes()` +
    /// `jbd2_journal_has_csum_v2or3` vectors (including csum_v2's frozen
    /// 10/14-byte stride quirk).
    #[ktest]
    fn tag_layout_derivation_vectors() {
        let cases: [(u32, usize, usize); 7] = [
            (0, 8, 0),
            (INCOMPAT_REVOKE, 8, 0), // revoke does not shape tags
            (INCOMPAT_64BIT, 12, 0),
            (INCOMPAT_CSUM_V3, 16, 4),
            (INCOMPAT_CSUM_V3 | INCOMPAT_64BIT, 16, 4),
            (INCOMPAT_CSUM_V2, 10, 4),
            (INCOMPAT_CSUM_V2 | INCOMPAT_64BIT, 14, 4),
        ];
        for (features, tag_bytes, tail) in cases {
            let layout = TagLayout::from_features(features).unwrap();
            assert_eq!(layout.tag_bytes, tag_bytes, "features {features:#x}");
            assert_eq!(layout.descriptor_tail_bytes, tail, "features {features:#x}");
        }

        // csum_v2 + csum_v3 together prescribe contradictory tag layouts and
        // are refused, as in jbd2's `journal_get_superblock`.
        assert!(TagLayout::from_features(INCOMPAT_CSUM_V2 | INCOMPAT_CSUM_V3).is_err());
    }

    /// Single-descriptor capacity per layout: `(tag area − header − UUID) /
    /// tag bytes`, the exact bound `max_credits` builds on. The v0 value is
    /// pinned to the pre-TagLayout constant.
    #[ktest]
    fn tag_layout_capacity_math() {
        let capacity = |features: u32| {
            TagLayout::from_features(features)
                .unwrap()
                .tags_per_descriptor()
        };
        // (4096 - 12 - 16) / 8, exactly the old `tags_per_descriptor`.
        assert_eq!(capacity(0), (BLOCK_SIZE - 12 - 16) / 8);
        assert_eq!(capacity(INCOMPAT_64BIT), (BLOCK_SIZE - 12 - 16) / 12);
        // The csum layouts additionally reserve the 4-byte descriptor tail.
        assert_eq!(capacity(INCOMPAT_CSUM_V3), (BLOCK_SIZE - 4 - 12 - 16) / 16);
        assert_eq!(
            capacity(INCOMPAT_CSUM_V3 | INCOMPAT_64BIT),
            (BLOCK_SIZE - 4 - 12 - 16) / 16
        );
        assert_eq!(capacity(INCOMPAT_CSUM_V2), (BLOCK_SIZE - 4 - 12 - 16) / 10);
        assert_eq!(
            capacity(INCOMPAT_CSUM_V2 | INCOMPAT_64BIT),
            (BLOCK_SIZE - 4 - 12 - 16) / 14
        );
    }

    /// A zeroed data block for the writer calls of tests that do not care
    /// about the logged bytes.
    fn zero_block() -> Box<[u8; BLOCK_SIZE]> {
        Box::new([0u8; BLOCK_SIZE])
    }

    /// The writer demands a seed exactly when the layout carries csum v2/v3:
    /// a csum layout without one could emit unstamped tags (which our own
    /// recovery would refuse), and a plain layout has nothing to stamp with.
    #[ktest]
    fn writer_requires_seed_iff_csum_layout() {
        let v3 = TagLayout::from_features(INCOMPAT_CSUM_V3).unwrap();
        let err = v3
            .writer(zero_block(), Tid::new(1), None)
            .map(|_| ())
            .unwrap_err();
        assert_eq!(err.error(), Errno::EINVAL);

        let v0 = TagLayout::from_features(0).unwrap();
        let err = v0
            .writer(zero_block(), Tid::new(1), Some(linux_seed()))
            .map(|_| ())
            .unwrap_err();
        assert_eq!(err.error(), Errno::EINVAL);
    }

    /// A block number above 32 bits is `EFBIG` on a layout without
    /// `t_blocknr_high`, and round-trips on one with it.
    #[ktest]
    fn put_blocknr_width_is_layout_gated() {
        let wide: Ext4Bid = (1 << 32) | 5;
        let data = zero_block();

        let v0 = TagLayout::from_features(0).unwrap();
        let err = v0
            .writer(zero_block(), Tid::new(1), None)
            .unwrap()
            .put(wide, TAG_FLAG_LAST_TAG, &data)
            .unwrap_err();
        assert_eq!(err.error(), Errno::EFBIG);

        let b64 = TagLayout::from_features(INCOMPAT_64BIT).unwrap();
        let mut writer = b64.writer(zero_block(), Tid::new(1), None).unwrap();
        writer.put(wide, TAG_FLAG_LAST_TAG, &data).unwrap();
        let block = writer.finish();
        let tag = b64.walk(&block).next().unwrap().unwrap();
        assert_eq!(tag.blocknr(), wide);
        assert!(tag.is_last());
    }

    /// 12-byte (64-bit) tags round-trip through the tag writer + the walker,
    /// including a > 32-bit block number, and land at the exact on-disk
    /// offsets (low word, high word, first-tag UUID gap).
    #[ktest]
    fn tag_round_trip_64bit_layout() {
        let layout = TagLayout::from_features(INCOMPAT_64BIT).unwrap();
        let data = zero_block();

        let tags: [(Ext4Bid, u16); 3] = [
            (0x123, 0), // first tag: a 16-byte UUID follows
            ((7 << 32) | 42, TAG_FLAG_SAME_UUID),
            (0xFFFF_FFFF, TAG_FLAG_SAME_UUID | TAG_FLAG_LAST_TAG),
        ];
        let mut writer = layout.writer(zero_block(), Tid::new(1), None).unwrap();
        for (blocknr, flags) in tags {
            writer.put(blocknr, flags, &data).unwrap();
        }
        let block = writer.finish();

        let decoded: Vec<_> = layout.walk(&block).map(|tag| tag.unwrap()).collect();
        assert_eq!(decoded.len(), 3);
        for ((blocknr, flags), tag) in tags.iter().zip(&decoded) {
            assert_eq!(tag.blocknr(), *blocknr);
            assert_eq!(tag.flags(), *flags);
        }

        // On-disk pin: tag 0 spans [12, 24) + UUID [24, 40); tag 1 starts at
        // 40 with t_blocknr = 42 and t_blocknr_high = 7 at offset 48.
        assert_eq!(&block[40..44], &[0, 0, 0, 42]);
        assert_eq!(&block[48..52], &[0, 0, 0, 7]);
    }

    /// 16-byte (csum-v3) tags round-trip with their stamped data checksums at
    /// the tag3 offsets, the widened flags in place, and the sealed
    /// descriptor tail verifying.
    #[ktest]
    fn tag_round_trip_csum_v3_layout() {
        let layout = TagLayout::from_features(INCOMPAT_CSUM_V3 | INCOMPAT_64BIT).unwrap();
        let seed = linux_seed();
        let tid = Tid::new(5);

        let mut d0 = zero_block();
        d0[..4].copy_from_slice(b"DAT0");
        let mut d1 = zero_block();
        d1[..4].copy_from_slice(b"DAT1");

        let mut writer = layout.writer(zero_block(), tid, Some(seed)).unwrap();
        writer.put(0x0102_0304, 0, &d0).unwrap();
        writer
            .put((3 << 32) | 9, TAG_FLAG_SAME_UUID | TAG_FLAG_LAST_TAG, &d1)
            .unwrap();
        let block = writer.finish();

        let decoded: Vec<_> = layout.walk(&block).map(|tag| tag.unwrap()).collect();
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].blocknr(), 0x0102_0304);
        assert_eq!(decoded[1].blocknr(), (3 << 32) | 9);
        assert!(decoded[1].is_last());
        // Each tag verifies against ITS logged block and not the other's.
        assert!(decoded[0].verify_data_csum(seed, tid, &d0));
        assert!(!decoded[0].verify_data_csum(seed, tid, &d1));
        assert!(decoded[1].verify_data_csum(seed, tid, &d1));
        // The sealed descriptor tail verifies over the stamped tags.
        assert!(seed.verify_block_tail(&block));

        // On-disk pin for tag 0 at offset 12: t_blocknr [12,16), t_flags
        // [16,20) (a 32-bit zero here), t_blocknr_high [20,24) (zero),
        // t_checksum [24,28) — the stamped big-endian crc32c.
        assert_eq!(&block[12..16], &[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(&block[16..24], &[0; 8]);
        assert_eq!(
            &block[24..28],
            Be32::new(seed.data_block_csum(tid, &d0)).as_bytes()
        );
        // Tag 1 at 28 + UUID(16) = 44: flags be32 = 0x0000000A, high word 3.
        assert_eq!(&block[44..48], &[0, 0, 0, 9]);
        assert_eq!(&block[48..52], &[0, 0, 0, 0x0A]);
        assert_eq!(&block[52..56], &[0, 0, 0, 3]);
    }

    /// csum_v3 WITHOUT 64bit (a metadata_csum filesystem on a < 16 TiB
    /// volume): the 16-byte tag3 still carries a `t_blocknr_high` word on
    /// disk, but Linux `read_tag_block` (fs/jbd2/recovery.c) joins it only
    /// when the journal has the 64bit FEATURE — the gate is the feature, not
    /// the tag format. A stray nonzero high word must decode to the low word
    /// alone.
    #[ktest]
    fn csum_v3_without_64bit_ignores_stray_blocknr_high() {
        let layout = TagLayout::from_features(INCOMPAT_CSUM_V3).unwrap();
        let mut block = Box::new([0u8; BLOCK_SIZE]);

        let raw = RawJournalBlockTag3 {
            t_blocknr: Be32::new(0x1234),
            t_flags: Be32::new(u32::from(TAG_FLAG_SAME_UUID | TAG_FLAG_LAST_TAG)),
            // Garbage where a 64bit journal would keep the high half.
            t_blocknr_high: Be32::new(0xDEAD_BEEF),
            t_checksum: Be32::new(0),
        };
        let offset = layout.first_tag_offset();
        block[offset..offset + BLOCK_TAG3_SIZE].copy_from_slice(raw.as_bytes());

        let tag = layout.walk(&block).next().unwrap().unwrap();
        assert_eq!(tag.blocknr(), 0x1234);
        assert!(tag.is_last());
    }

    /// csum_v2's frozen 10/14-byte strides round-trip (the two quirk padding
    /// bytes stay zero and the walker steps over them), with the stamped
    /// low-16-bit data checksums verifying end to end.
    #[ktest]
    fn tag_round_trip_csum_v2_quirk_strides() {
        let seed = linux_seed();
        let tid = Tid::new(3);
        let data = linux_data_block();
        for features in [INCOMPAT_CSUM_V2, INCOMPAT_CSUM_V2 | INCOMPAT_64BIT] {
            let layout = TagLayout::from_features(features).unwrap();
            let wide_ok = features & INCOMPAT_64BIT != 0;
            let second: Ext4Bid = if wide_ok { (5 << 32) | 6 } else { 0x600 };

            let mut writer = layout.writer(zero_block(), tid, Some(seed)).unwrap();
            writer.put(0x500, 0, &data).unwrap();
            writer
                .put(second, TAG_FLAG_SAME_UUID | TAG_FLAG_LAST_TAG, &data)
                .unwrap();
            let block = writer.finish();

            let decoded: Vec<_> = layout.walk(&block).map(|tag| tag.unwrap()).collect();
            assert_eq!(decoded.len(), 2, "features {features:#x}");
            assert_eq!(decoded[0].blocknr(), 0x500);
            assert_eq!(decoded[1].blocknr(), second);
            assert!(decoded[1].is_last());
            // The stamped low-16 checksum verifies; a different block fails.
            assert!(decoded[0].verify_data_csum(seed, tid, &data));
            assert!(!decoded[0].verify_data_csum(seed, tid, &zero_block()));
            assert!(seed.verify_block_tail(&block), "features {features:#x}");
        }
    }

    /// Both sides respect the reserved descriptor tail: the writer refuses a
    /// tag that would intrude into it, and a walker on a LAST_TAG-less (all
    /// zeros) descriptor stops with an error before reading the tail as a tag.
    #[ktest]
    fn tag_area_excludes_descriptor_tail() {
        let v3 = TagLayout::from_features(INCOMPAT_CSUM_V3).unwrap();
        let data = zero_block();
        // A 16-byte tag at BLOCK_SIZE - 16 fits the block but overlaps the
        // 4-byte tail: refused. The writer comes from the real constructor
        // (so the layout↔mode invariant holds) and only its cursor is
        // planted (`plant_offset_for_test`): the offsets a sequential writer
        // can reach on this geometry are all congruent mod 16, and this
        // tail-only-overlap one is not among them.
        let mut writer = v3
            .writer(zero_block(), Tid::new(1), Some(linux_seed()))
            .unwrap();
        writer.plant_offset_for_test(BLOCK_SIZE - 16);
        let err = writer
            .put(0x1, TAG_FLAG_SAME_UUID | TAG_FLAG_LAST_TAG, &data)
            .unwrap_err();
        assert_eq!(err.error(), Errno::ENOSPC);
        // One slot earlier (clear of the tail) is accepted. The refused put
        // above did not advance the cursor, so the same writer replants.
        writer.plant_offset_for_test(BLOCK_SIZE - 4 - 16);
        writer
            .put(0x1, TAG_FLAG_SAME_UUID | TAG_FLAG_LAST_TAG, &data)
            .unwrap();

        // Walker bound: an all-zero tag array never sets LAST_TAG, so the walk
        // ends at the tag-area boundary with exactly one error. Every zero tag
        // lacks SAME_UUID, so the stride is tag_bytes + 16.
        let zeroed = Box::new([0u8; BLOCK_SIZE]);
        let count_ok = |layout: &TagLayout| {
            let mut oks = 0usize;
            let mut errs = 0usize;
            for tag in layout.walk(&zeroed) {
                match tag {
                    Ok(_) => oks += 1,
                    Err(_) => errs += 1,
                }
            }
            assert_eq!(errs, 1);
            oks
        };
        // v0: offsets 12 + 24k, valid while 12 + 24k + 8 <= 4096 -> k <= 169.
        let v0 = TagLayout::from_features(0).unwrap();
        assert_eq!(count_ok(&v0), 170);
        // v3: offsets 12 + 32k, valid while 12 + 32k + 16 <= 4092 -> k <= 127.
        assert_eq!(count_ok(&v3), 128);
    }

    // --- P7a-3 gate 3: checksum vectors pinned from a REAL Linux-written
    // journal. Ground truth: a 256 MiB `mke2fs -b 4096 -O metadata_csum,64bit
    // -E lazy_journal_init=0` image, loop-mounted under Linux 6.8 (which
    // upgrades the journal to csum_v3 + 64bit, `s_feature_incompat = 0x12`),
    // dirtied with count-neutral ops (chmod/rename of lost+found), synced,
    // and snapshotted BEFORE unmount. The constants below reproduce, byte for
    // byte, that snapshot's journal superblock, its one transaction's
    // descriptor and commit blocks, and one journaled data block (the root
    // directory block); the reconstructions were diffed against the raw
    // image, and every stored checksum was independently recomputed in
    // Python from the reflected 0x82F63B78 polynomial, before being pinned
    // here. ---

    /// The snapshot filesystem's UUID (`s_uuid` of the journal superblock).
    const LINUX_UUID: [u8; 16] = [
        0x60, 0xDA, 0xC8, 0xE1, 0x3E, 0x5E, 0x42, 0x3F, 0xA7, 0x28, 0xBF, 0x37, 0xE9, 0x1C, 0x87,
        0xBB,
    ];
    /// Linux's `j_csum_seed` for that journal: `crc32c(!0, LINUX_UUID)`.
    const LINUX_SEED: u32 = 0x4A61_B9AA;
    /// The journal superblock's stored `s_checksum`.
    const LINUX_SB_CSUM: u32 = 0x9FD2_29EE;
    /// The transaction's tid (`h_sequence` of its descriptor/commit blocks;
    /// also the journal superblock's `s_sequence` — the log was dirty).
    const LINUX_TID: u32 = 2;
    /// tag0's stored `t_checksum` (data block: an inode-table block, fs block
    /// 37 — not pinned here; tag1's sparser data block is).
    const LINUX_TAG0_CSUM: u32 = 0x1A0C_8FB6;
    /// tag1's stored `t_checksum` (data block: the root directory block, fs
    /// block 4133, pinned in [`linux_data_block`]).
    const LINUX_TAG1_CSUM: u32 = 0x5C00_D228;
    /// The descriptor block's stored tail checksum (at byte 4092).
    const LINUX_DESC_TAIL_CSUM: u32 = 0xE10F_C75F;
    /// The commit block's stored `h_chksum[0]`.
    const LINUX_COMMIT_CSUM: u32 = 0x0850_86F5;

    /// The seed our formula derives — pinned equal to [`LINUX_SEED`] by
    /// [`csum_seed_matches_linux`].
    fn linux_seed() -> JournalCsumSeed {
        JournalCsumSeed::derive(&LINUX_UUID)
    }

    /// The snapshot's journal superblock, field for field (all others zero —
    /// verified byte-identical to the raw image).
    fn linux_journal_superblock() -> RawJournalSuperblock {
        RawJournalSuperblock {
            header: RawJournalHeader {
                h_magic: Be32::new(JBD2_MAGIC),
                h_blocktype: Be32::new(BLOCKTYPE_SUPERBLOCK_V2),
                h_sequence: Be32::new(0),
            },
            s_blocksize: Be32::new(4096),
            s_maxlen: Be32::new(4096),
            s_first: Be32::new(1),
            s_sequence: Be32::new(LINUX_TID),
            s_start: Be32::new(1),
            s_feature_incompat: Be32::new(INCOMPAT_64BIT | INCOMPAT_CSUM_V3),
            s_uuid: LINUX_UUID,
            s_nr_users: Be32::new(1),
            s_checksum_type: JBD2_CRC32C_CHKSUM,
            s_checksum: Be32::new(LINUX_SB_CSUM),
            ..Default::default()
        }
    }

    /// The snapshot's descriptor block (log block 1): header, two tag3 tags —
    /// fs blocks 37 and 4133, a zero journal UUID after the first — and the
    /// stored tail checksum. Everything else zero (verified byte-identical).
    fn linux_descriptor_block() -> Box<[u8; BLOCK_SIZE]> {
        let mut block = Box::new([0u8; BLOCK_SIZE]);
        let header = RawJournalHeader {
            h_magic: Be32::new(JBD2_MAGIC),
            h_blocktype: Be32::new(BLOCKTYPE_DESCRIPTOR),
            h_sequence: Be32::new(LINUX_TID),
        };
        block[..JOURNAL_HEADER_SIZE].copy_from_slice(header.as_bytes());
        let tag0 = RawJournalBlockTag3 {
            t_blocknr: Be32::new(37),
            t_flags: Be32::new(0),
            t_blocknr_high: Be32::new(0),
            t_checksum: Be32::new(LINUX_TAG0_CSUM),
        };
        block[12..28].copy_from_slice(tag0.as_bytes());
        // [28..44): the 16-byte journal UUID after tag0 — Linux wrote zeros.
        let tag1 = RawJournalBlockTag3 {
            t_blocknr: Be32::new(4133),
            t_flags: Be32::new(u32::from(TAG_FLAG_SAME_UUID | TAG_FLAG_LAST_TAG)),
            t_blocknr_high: Be32::new(0),
            t_checksum: Be32::new(LINUX_TAG1_CSUM),
        };
        block[44..60].copy_from_slice(tag1.as_bytes());
        block[BLOCK_SIZE - DESCRIPTOR_TAIL_BYTES..]
            .copy_from_slice(Be32::new(LINUX_DESC_TAIL_CSUM).as_bytes());
        block
    }

    /// The snapshot's commit block (log block 4): header, `h_chksum[0]`, and
    /// the commit timestamp; `h_chksum_type`/`h_chksum_size` are ZERO under
    /// csum v2/v3 (they belong to the v1 COMPAT checksum). Everything else
    /// zero (verified byte-identical).
    fn linux_commit_block() -> Box<[u8; BLOCK_SIZE]> {
        let mut block = Box::new([0u8; BLOCK_SIZE]);
        let mut commit = RawCommitBlock {
            header: RawJournalHeader {
                h_magic: Be32::new(JBD2_MAGIC),
                h_blocktype: Be32::new(BLOCKTYPE_COMMIT),
                h_sequence: Be32::new(LINUX_TID),
            },
            h_commit_sec: Be64::new(1_783_157_811),
            h_commit_nsec: Be32::new(790_793_834),
            ..Default::default()
        };
        commit.h_chksum[0] = Be32::new(LINUX_COMMIT_CSUM);
        block[..size_of::<RawCommitBlock>()].copy_from_slice(commit.as_bytes());
        block
    }

    /// tag1's journaled data block (log block 3 = fs block 4133, the root
    /// directory: `.`, `..`, `lost+found`, and the metadata_csum dir tail),
    /// rebuilt from its sparse nonzero segments (verified byte-identical).
    fn linux_data_block() -> Box<[u8; BLOCK_SIZE]> {
        let segments: &[(usize, &[u8])] = &[
            (0, &[0x02]),
            (4, &[0x0C, 0x00, 0x01, 0x02, 0x2E]),
            (12, &[0x02]),
            (16, &[0x0C, 0x00, 0x02, 0x02, 0x2E, 0x2E]),
            (24, &[0x0B]),
            (28, b"\xDC\x0F\x0A\x02lost+found"),
            (4088, &[0x0C]),
            (4091, &[0xDE, 0x4D, 0x83, 0xA0, 0x4D]),
        ];
        let mut block = Box::new([0u8; BLOCK_SIZE]);
        for (offset, bytes) in segments {
            block[*offset..*offset + bytes.len()].copy_from_slice(bytes);
        }
        block
    }

    /// Our seed derivation reproduces Linux's `j_csum_seed` for the real UUID.
    #[ktest]
    fn csum_seed_matches_linux() {
        assert_eq!(linux_seed().0, LINUX_SEED);
    }

    /// The journal-superblock checksum formula reproduces the stored value on
    /// the real bytes; a bit flip breaks it; and the parse boundary both
    /// accepts the intact superblock (deriving the seed) and refuses the
    /// corrupt one with `EUCLEAN`.
    #[ktest]
    fn superblock_csum_matches_real_linux_bytes() {
        let raw = linux_journal_superblock();
        assert_eq!(raw.checksum(), LINUX_SB_CSUM);

        let sb = JournalSuperblock::try_from(raw).unwrap();
        assert_eq!(sb.csum_seed().unwrap().0, LINUX_SEED);
        assert_eq!(sb.sequence(), Tid::new(LINUX_TID));

        let mut corrupt = raw;
        corrupt.s_uuid[3] ^= 0x10;
        assert_ne!(corrupt.checksum(), LINUX_SB_CSUM);
        assert_eq!(
            JournalSuperblock::try_from(corrupt).unwrap_err().error(),
            Errno::EUCLEAN
        );
    }

    /// The descriptor-tail formula reproduces the stored value on the real
    /// bytes; a bit flip anywhere in the covered area breaks verification.
    #[ktest]
    fn descriptor_tail_csum_matches_real_linux_bytes() {
        let block = linux_descriptor_block();
        let seed = linux_seed();
        assert_eq!(seed.block_tail_csum(&block), LINUX_DESC_TAIL_CSUM);
        assert!(seed.verify_block_tail(&block));

        let mut corrupt = block;
        corrupt[100] ^= 0x40;
        assert!(!seed.verify_block_tail(&corrupt));
    }

    /// The commit-block formula reproduces the stored value on the real
    /// bytes; a bit flip breaks verification.
    #[ktest]
    fn commit_block_csum_matches_real_linux_bytes() {
        let block = linux_commit_block();
        let seed = linux_seed();
        assert_eq!(seed.commit_block_csum(&block), LINUX_COMMIT_CSUM);
        assert!(seed.verify_commit_block(&block));

        let mut corrupt = block;
        corrupt[50] ^= 0x01;
        assert!(!seed.verify_commit_block(&corrupt));
    }

    /// The per-tag data checksum reproduces the stored tag3 value on the real
    /// bytes, end to end through the walker (the tag carries its stored
    /// checksum out of `decode_tag`; nothing re-slices the descriptor): the
    /// right block under the right tid verifies; a flipped bit, the wrong
    /// tid, or the wrong tag all fail.
    #[ktest]
    fn tag3_data_csum_matches_real_linux_bytes() {
        let layout = TagLayout::from_features(INCOMPAT_64BIT | INCOMPAT_CSUM_V3).unwrap();
        let descriptor = linux_descriptor_block();
        let tags: Vec<_> = layout.walk(&descriptor).map(|tag| tag.unwrap()).collect();
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].blocknr(), 37);
        assert_eq!(tags[1].blocknr(), 4133);
        assert_eq!(tags[1].checksum, Some(TagChecksum::V3(LINUX_TAG1_CSUM)));

        let seed = linux_seed();
        let data = linux_data_block();
        assert_eq!(
            seed.data_block_csum(Tid::new(LINUX_TID), &data),
            LINUX_TAG1_CSUM
        );
        assert!(tags[1].verify_data_csum(seed, Tid::new(LINUX_TID), &data));

        // A flipped bit in the data fails.
        let mut corrupt = data.clone();
        corrupt[77] ^= 0x01;
        assert!(!tags[1].verify_data_csum(seed, Tid::new(LINUX_TID), &corrupt));
        // The tid folds into the checksum: the wrong sequence fails.
        assert!(!tags[1].verify_data_csum(seed, Tid::new(LINUX_TID + 1), &data));
        // And this block does not verify against the OTHER tag's checksum.
        assert!(!tags[0].verify_data_csum(seed, Tid::new(LINUX_TID), &data));
    }

    /// csum_v2 semantics (no real artifact — Linux 6.8 writes csum_v3; the
    /// formula is the same crc32c, truncated): the 16-bit tag field the
    /// WRITER stamps must equal the LOW half of the computed crc32c, per
    /// jbd2's `cpu_to_be16(csum32)` truncation (commit.c:339 /
    /// recovery.c:460).
    #[ktest]
    fn tag_csum_v2_stores_low_16_bits() {
        let layout = TagLayout::from_features(INCOMPAT_CSUM_V2).unwrap();
        let seed = linux_seed();
        let tid = Tid::new(9);
        let data = linux_data_block();
        let csum32 = seed.data_block_csum(tid, &data);

        let mut writer = layout.writer(zero_block(), tid, Some(seed)).unwrap();
        writer
            .put(0x321, TAG_FLAG_SAME_UUID | TAG_FLAG_LAST_TAG, &data)
            .unwrap();
        let mut descriptor = writer.finish();
        // The 8-byte tag sits at offset 12, its `t_checksum` at 12 + 4: the
        // stamped bytes are the LOW half of the big-endian crc32c...
        assert_eq!(&descriptor[16..18], &csum32.to_be_bytes()[2..]);
        let tag = layout.walk(&descriptor).next().unwrap().unwrap();
        assert!(tag.verify_data_csum(seed, tid, &data));

        // ...and the HIGH half must not verify (a truncation-direction pin).
        let high = (csum32 >> 16).to_be_bytes();
        descriptor[16..18].copy_from_slice(&high[2..]);
        let tag = layout.walk(&descriptor).next().unwrap().unwrap();
        assert!(!tag.verify_data_csum(seed, tid, &data));
    }

    /// The data checksum covers the block AS LOGGED — the escaped form: the
    /// writer stamps over the zero-headed log copy it is handed, which
    /// verifies against that copy and NOT against the restored original, so
    /// verification must run before the magic head is put back
    /// (recovery.c:656 vs :686).
    #[ktest]
    fn tag_data_csum_covers_escaped_form() {
        let seed = linux_seed();
        let tid = Tid::new(5);

        // A metadata block that begins with the jbd2 magic...
        let mut original = Box::new([0u8; BLOCK_SIZE]);
        original[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
        original[4..8].copy_from_slice(b"REST");
        // ...is escaped in the log: first four bytes zeroed.
        let mut logged = original.clone();
        logged[..4].fill(0);

        let layout = TagLayout::from_features(INCOMPAT_CSUM_V3).unwrap();
        let mut writer = layout.writer(zero_block(), tid, Some(seed)).unwrap();
        writer
            .put(
                0x700,
                TAG_FLAG_ESCAPE | TAG_FLAG_SAME_UUID | TAG_FLAG_LAST_TAG,
                // The writer's contract: the block as it will sit in the log.
                &logged,
            )
            .unwrap();
        let descriptor = writer.finish();

        let tag = layout.walk(&descriptor).next().unwrap().unwrap();
        assert!(tag.is_escaped());
        assert!(tag.verify_data_csum(seed, tid, &logged));
        assert!(!tag.verify_data_csum(seed, tid, &original));
    }

    /// On a layout without csum v2/v3 the tag carries no stored checksum and
    /// verification is vacuous (Linux returns 1 when
    /// `!jbd2_journal_has_csum_v2or3`) — production callers never reach it
    /// there because no seed exists to gate on.
    #[ktest]
    fn tag_without_csum_layout_verifies_vacuously() {
        let layout = TagLayout::from_features(INCOMPAT_64BIT).unwrap();
        let mut writer = layout.writer(zero_block(), Tid::new(1), None).unwrap();
        writer
            .put(0x42, TAG_FLAG_SAME_UUID | TAG_FLAG_LAST_TAG, &zero_block())
            .unwrap();
        let descriptor = writer.finish();
        let tag = layout.walk(&descriptor).next().unwrap().unwrap();
        assert_eq!(tag.checksum, None);
        assert!(tag.verify_data_csum(linux_seed(), Tid::new(1), &linux_data_block()));
    }

    /// The WRITE side reproduces the real Linux artifacts, pinning the
    /// stampers to the same ground truth as the verifiers, independently of
    /// them: [`TagWriter::put`] stamps tag1's exact stored checksum for the
    /// real logged root-directory block, [`RawJournalSuperblock::stamp_checksum`]
    /// reproduces the stored `s_checksum`, and
    /// [`JournalCsumSeed::stamp_commit_block`] reproduces the stored
    /// `h_chksum[0]` byte for byte.
    #[ktest]
    fn write_side_reproduces_real_linux_csums() {
        let seed = linux_seed();
        let tid = Tid::new(LINUX_TID);

        // Tag: stamped over the real logged block. (The tag's position and
        // flags differ from the snapshot's tag1 — only tid + block bytes fold
        // into the data checksum, recovery.c:443-461.)
        let layout = TagLayout::from_features(INCOMPAT_64BIT | INCOMPAT_CSUM_V3).unwrap();
        let mut writer = layout.writer(zero_block(), tid, Some(seed)).unwrap();
        writer
            .put(
                4133,
                TAG_FLAG_SAME_UUID | TAG_FLAG_LAST_TAG,
                &linux_data_block(),
            )
            .unwrap();
        let block = writer.finish();
        let tag = layout.walk(&block).next().unwrap().unwrap();
        assert_eq!(tag.checksum, Some(TagChecksum::V3(LINUX_TAG1_CSUM)));
        // The sealed tail re-verifies over this (differently laid out)
        // descriptor.
        assert!(seed.verify_block_tail(&block));

        // Superblock: strip the stored checksum, restamp, exact value back.
        let mut raw = linux_journal_superblock();
        raw.s_checksum = Be32::new(0);
        raw.stamp_checksum();
        assert_eq!(raw.s_checksum.get(), LINUX_SB_CSUM);

        // Commit block: strip the stored checksum, restamp, byte-identical.
        let mut commit = linux_commit_block();
        commit[COMMIT_CHKSUM_OFFSET..COMMIT_CHKSUM_OFFSET + 4].fill(0);
        seed.stamp_commit_block(&mut commit);
        assert_eq!(commit.as_slice(), linux_commit_block().as_slice());
    }

    /// [`RawJournalSuperblock::stamp_checksum`] keys off the superblock's own
    /// feature bits: a featureless (v0) superblock is left byte-untouched —
    /// the frozen v0 byte path every pre-P7 image rides on.
    #[ktest]
    fn superblock_stamp_only_under_csum_features() {
        let mut raw = valid_raw(1024, 1, 1, 0);
        let before = raw;
        raw.stamp_checksum();
        assert_eq!(raw.as_bytes(), before.as_bytes());
    }
}
