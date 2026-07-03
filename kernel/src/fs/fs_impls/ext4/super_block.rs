// SPDX-License-Identifier: MPL-2.0

//! On-disk ext4 superblock parsing and the validated in-memory representation.
//!
//! The ext4 superblock shares its first 264 bytes of layout with ext2; the
//! ext4-specific fields live in the trailing reserved area. The group
//! descriptor size (`s_desc_size`) is parsed here so `flex_bg` images mount
//! (their bitmaps/tables come from the descriptor getters, so relocating them
//! needs no geometry change), and the `64BIT` high halves of the block counts
//! (`s_blocks_count_hi` / `s_free_blocks_count_hi`) are spliced at the parse
//! boundary so `> 2^32`-block volumes count correctly. The metadata-checksum
//! fields are parsed when a later P6 task brings that feature; until then only
//! images with checksums disabled mount.

use super::{
    feature::{
        FeatureCompatSet, FeatureIncompatSet, FeatureRoCompatSet, INCOMPAT_SUPP, RO_COMPAT_SUPP,
    },
    journal,
    prelude::*,
};

/// Magic signature (`s_magic`).
pub(super) const MAGIC_NUM: u16 = 0xef53;

/// The main superblock is located at byte 1024 from the start of the device.
pub(super) const SUPER_BLOCK_OFFSET: usize = 1024;

/// The device block that holds the primary superblock. With 4 KiB blocks the
/// superblock lives at byte [`SUPER_BLOCK_OFFSET`] (1024) inside block 0, so
/// journaling it means capturing block 0.
const SUPERBLOCK_BID: Ext4Bid = (SUPER_BLOCK_OFFSET / BLOCK_SIZE) as Ext4Bid;

const SUPER_BLOCK_SIZE: usize = 1024;

/// Classic group-descriptor size in bytes, and the size forced when the `64BIT`
/// feature is absent (Linux `EXT4_MIN_DESC_SIZE`).
const MIN_DESC_SIZE: u16 = 32;

/// Smallest group descriptor a `64BIT` volume may declare (Linux
/// `EXT4_MIN_DESC_SIZE_64BIT`). With `64BIT` set, `s_desc_size` must be at
/// least this — a smaller value contradicts the wide on-disk GDT stride.
const MIN_DESC_SIZE_64BIT: u16 = 64;

/// Widest group descriptor the `64BIT` feature defines (Linux
/// `EXT4_MAX_DESC_SIZE`); the 64-byte descriptor carries the high halves and
/// per-group checksums.
const MAX_DESC_SIZE: u16 = 64;

/// Validated, Rust-typed in-memory representation of the ext4 superblock.
///
/// Counts that the `64BIT` feature would widen are stored as `u64` from the
/// start so enabling that feature later (Phase 6) reads the high halves without
/// changing this type; until then the high halves are zero.
#[derive(Clone, Copy, Debug)]
pub(super) struct SuperBlock {
    inodes_count: u32,
    blocks_count: u64,
    free_blocks_count: u64,
    free_inodes_count: u32,
    first_data_block: Ext4Bid,
    block_size: usize,
    nr_blocks_per_group: u32,
    nr_inodes_per_group: u32,
    // The fields below are decoded for completeness but not yet read by the
    // read-only path; later phases (allocation, journal recovery, checksums)
    // consume them. They are kept live by the accessors and predicates below,
    // each of which carries the `#[expect(dead_code)]` marker until wired in.
    nr_inode_table_blocks_per_group: u32,
    inode_size: usize,
    /// Effective group-descriptor size in bytes (32 or 64), already resolved
    /// from the raw `s_desc_size` sentinel by [`parse_desc_size`].
    desc_size: u16,
    first_ino: u32,
    rev_level: RevLevel,
    state: FsState,
    feature_compat: FeatureCompatSet,
    feature_incompat: FeatureIncompatSet,
    feature_ro_compat: FeatureRoCompatSet,
    uuid: [u8; 16],
    last_orphan: Option<Ext4Ino>,
    reserved_blocks_count: u32,
    journal_ino: u32,
    journal_dev: u32,
}

impl TryFrom<RawSuperBlock> for SuperBlock {
    type Error = Error;

    fn try_from(sb: RawSuperBlock) -> Result<Self> {
        if sb.magic != MAGIC_NUM {
            return_errno_with_message!(Errno::EINVAL, "bad ext4 magic number");
        }

        // Only the 4 KiB block size the page-cache model assumes (page index ==
        // logical block) is supported.
        if sb.log_block_size != 2 {
            return_errno_with_message!(Errno::EINVAL, "unsupported block size (4 KiB only)");
        }
        if sb.log_frag_size != sb.log_block_size {
            return_errno_with_message!(Errno::EINVAL, "invalid fragment size");
        }
        let block_size = BLOCK_SIZE;

        let state = FsState::from_bits_truncate(sb.state);

        let errors_behavior = ErrorsBehavior::try_from(sb.errors)
            .map_err(|_| Error::with_message(Errno::EINVAL, "invalid errors behavior"))?;
        if errors_behavior != ErrorsBehavior::Continue {
            return_errno_with_message!(Errno::EINVAL, "unsupported errors behavior");
        }

        let creator_os = OsId::try_from(sb.creator_os)
            .map_err(|_| Error::with_message(Errno::EINVAL, "invalid creator os"))?;
        if creator_os != OsId::Linux {
            return_errno_with_message!(Errno::EINVAL, "unsupported creator os");
        }

        let rev_level = RevLevel::try_from(sb.rev_level)
            .map_err(|_| Error::with_message(Errno::EINVAL, "invalid revision level"))?;
        let (first_ino, inode_size) = match rev_level {
            RevLevel::GoodOld => (11, 128usize),
            RevLevel::Dynamic => {
                let inode_size = sb.inode_size as usize;
                if inode_size < 128 || inode_size > block_size || !inode_size.is_power_of_two() {
                    return_errno_with_message!(Errno::EINVAL, "invalid inode size");
                }
                (sb.first_ino, inode_size)
            }
        };

        // Reject any incompatible feature this phase cannot honor, rather than
        // silently misreading the volume.
        let unsupported_incompat = sb.feature_incompat & !INCOMPAT_SUPP.bits();
        if unsupported_incompat != 0 {
            return_errno_with_message!(Errno::EINVAL, "unsupported incompatible feature");
        }
        let feature_incompat = FeatureIncompatSet::from_bits_truncate(sb.feature_incompat);
        if !feature_incompat.contains(FeatureIncompatSet::EXTENTS) {
            return_errno_with_message!(Errno::EINVAL, "ext4 image without the extents feature");
        }
        let feature_compat = FeatureCompatSet::from_bits_truncate(sb.feature_compat);

        // Resolve `s_desc_size` at this boundary into the effective descriptor
        // size the group-descriptor decoder strides by: `EXT4_DESC_SIZE =
        // has_64bit ? s_desc_size : 32`. With `64BIT` now supported, a 64-byte
        // descriptor is admitted and decoded wide by `BlockGroup::read_desc`.
        let desc_size = parse_desc_size(
            sb.desc_size,
            feature_incompat.contains(FeatureIncompatSet::IS_64BIT),
        )?;

        // A ro_compat feature we cannot safely *write* (e.g. `METADATA_CSUM`
        // before P6) must not mount writable — our writes would corrupt that
        // feature's invariants for every other implementation. Linux falls
        // back to a read-only mount and refuses `MS_RDWR` with `EROFS`
        // (`ext4_setup_super`); with no read-only mode here yet, refuse the
        // mount the same way. Checked on the raw bits so bits unknown to
        // `FeatureRoCompatSet` are caught too.
        if sb.feature_ro_compat & !RO_COMPAT_SUPP.bits() != 0 {
            return_errno_with_message!(
                Errno::EROFS,
                "unsupported read-only-compatible feature on a writable mount"
            );
        }
        let feature_ro_compat = FeatureRoCompatSet::from_bits_truncate(sb.feature_ro_compat);

        let nr_inodes_per_group = sb.inodes_per_group;
        let nr_blocks_per_group = sb.blocks_per_group;
        if nr_inodes_per_group == 0 || nr_blocks_per_group == 0 {
            return_errno_with_message!(Errno::EINVAL, "invalid group sizes");
        }

        let inodes_per_block = (block_size / inode_size) as u32;
        let max_bits_per_group = (block_size as u32) * 8;
        if nr_inodes_per_group < inodes_per_block || nr_inodes_per_group > max_bits_per_group {
            return_errno_with_message!(Errno::EINVAL, "invalid inodes per group");
        }
        if nr_blocks_per_group > max_bits_per_group {
            return_errno_with_message!(Errno::EINVAL, "blocks per group is too large");
        }

        let nr_inode_table_blocks_per_group = nr_inodes_per_group / inodes_per_block;
        if nr_blocks_per_group <= nr_inode_table_blocks_per_group + 3 {
            return_errno_with_message!(Errno::EINVAL, "blocks per group is too small");
        }

        // Splice the 64-bit block counts at this one parse boundary: the high
        // halves are honored only with the `64BIT` feature, and a non-64bit image
        // carrying a non-zero high half is malformed (rejected inside the helper).
        let is_64bit = feature_incompat.contains(FeatureIncompatSet::IS_64BIT);
        let blocks_count = splice_count_hi(sb.blocks_count, sb.blocks_count_hi, is_64bit)?;
        let free_blocks_count =
            splice_count_hi(sb.free_blocks_count, sb.free_blocks_count_hi, is_64bit)?;

        let first_data_block = sb.first_data_block as u64;
        if blocks_count <= first_data_block + 1 {
            return_errno_with_message!(Errno::EINVAL, "invalid blocks count");
        }
        let nr_block_groups =
            (blocks_count - first_data_block - 1) / nr_blocks_per_group as u64 + 1;

        let max_inodes = nr_block_groups * nr_inodes_per_group as u64;
        let min_inodes = (nr_block_groups - 1) * nr_inodes_per_group as u64;
        let inodes_count = sb.inodes_count as u64;
        if inodes_count <= min_inodes || inodes_count > max_inodes {
            return_errno_with_message!(Errno::EINVAL, "invalid inodes count");
        }
        if free_blocks_count > blocks_count {
            return_errno_with_message!(Errno::EINVAL, "free blocks count exceeds blocks count");
        }
        if sb.free_inodes_count > sb.inodes_count {
            return_errno_with_message!(Errno::EINVAL, "free inodes count exceeds inodes count");
        }

        Ok(Self {
            inodes_count: sb.inodes_count,
            blocks_count,
            free_blocks_count,
            free_inodes_count: sb.free_inodes_count,
            first_data_block,
            block_size,
            nr_blocks_per_group,
            nr_inodes_per_group,
            nr_inode_table_blocks_per_group,
            inode_size,
            desc_size,
            first_ino,
            rev_level,
            state,
            feature_compat,
            feature_incompat,
            feature_ro_compat,
            uuid: sb.uuid,
            // `0 = empty` is the on-disk convention; in memory the head is an
            // `Option` and the sentinel stops at this parse boundary.
            last_orphan: (sb.last_orphan != 0).then_some(sb.last_orphan),
            reserved_blocks_count: sb.reserved_blocks_count,
            journal_ino: sb.journal_ino,
            journal_dev: sb.journal_dev,
        })
    }
}

impl SuperBlock {
    pub(super) const fn block_size(&self) -> usize {
        self.block_size
    }

    pub(super) const fn inode_size(&self) -> usize {
        self.inode_size
    }

    /// Returns the effective group-descriptor size in bytes (`EXT4_DESC_SIZE`):
    /// the raw `s_desc_size` when the `64BIT` feature is set, else the classic
    /// 32. Drives the GDT stride and the 32-vs-64-byte descriptor decode in
    /// [`BlockGroup::load`](super::block_group::BlockGroup).
    pub(super) const fn desc_size(&self) -> u16 {
        self.desc_size
    }

    pub(super) const fn first_ino(&self) -> u32 {
        self.first_ino
    }

    pub(super) const fn nr_inodes_per_group(&self) -> u32 {
        self.nr_inodes_per_group
    }

    pub(super) const fn nr_blocks_per_group(&self) -> u32 {
        self.nr_blocks_per_group
    }

    /// Decreases the free-block counter by `n`, erroring on underflow.
    ///
    /// Takes `&mut self` so that a write guard over `RwMutex<Dirty<SuperBlock>>`
    /// marks the superblock dirty for writeback.
    pub(super) fn dec_free_blocks(&mut self, n: u64) -> Result<()> {
        self.free_blocks_count = self
            .free_blocks_count
            .checked_sub(n)
            .ok_or_else(|| Error::with_message(Errno::EIO, "free block counter underflow"))?;
        Ok(())
    }

    /// Increases the free-block counter by `n`, erroring on overflow past the
    /// total block count.
    pub(super) fn inc_free_blocks(&mut self, n: u64) -> Result<()> {
        let new_count = self
            .free_blocks_count
            .checked_add(n)
            .ok_or_else(|| Error::with_message(Errno::EIO, "free block counter overflow"))?;
        if new_count > self.blocks_count {
            return_errno_with_message!(Errno::EIO, "free block counter exceeds total blocks");
        }
        self.free_blocks_count = new_count;
        Ok(())
    }

    /// Decreases the free-inode counter by one, erroring on underflow.
    ///
    /// Takes `&mut self` so that a write guard over `RwMutex<Dirty<SuperBlock>>`
    /// marks the superblock dirty for writeback.
    pub(super) fn dec_free_inodes(&mut self) -> Result<()> {
        self.free_inodes_count = self
            .free_inodes_count
            .checked_sub(1)
            .ok_or_else(|| Error::with_message(Errno::EIO, "free inode counter underflow"))?;
        Ok(())
    }

    /// Increases the free-inode counter by one, erroring on overflow past the
    /// total inode count.
    pub(super) fn inc_free_inodes(&mut self) -> Result<()> {
        let new_count = self
            .free_inodes_count
            .checked_add(1)
            .ok_or_else(|| Error::with_message(Errno::EIO, "free inode counter overflow"))?;
        if new_count > self.inodes_count {
            return_errno_with_message!(Errno::EIO, "free inode counter exceeds total inodes");
        }
        self.free_inodes_count = new_count;
        Ok(())
    }

    pub(super) const fn nr_inode_table_blocks_per_group(&self) -> u32 {
        self.nr_inode_table_blocks_per_group
    }

    pub(super) const fn first_data_block(&self) -> Ext4Bid {
        self.first_data_block
    }

    pub(super) const fn total_inodes(&self) -> u32 {
        self.inodes_count
    }

    pub(super) const fn total_blocks(&self) -> u64 {
        self.blocks_count
    }

    pub(super) const fn free_blocks_count(&self) -> u64 {
        self.free_blocks_count
    }

    pub(super) const fn free_inodes_count(&self) -> u32 {
        self.free_inodes_count
    }

    /// Returns the head of the on-disk orphan list (`s_last_orphan`), `0` when empty.
    ///
    /// The orphan list threads inodes whose link count reached 0 but whose
    /// deletion has not yet completed (they still hold blocks / a bitmap bit).
    /// Crash recovery walks it from this head, following each inode's `i_dtime`
    /// (reused as the "next" pointer while an inode is on the list), to finish
    /// every interrupted deletion.
    pub(super) const fn last_orphan(&self) -> Option<Ext4Ino> {
        self.last_orphan
    }

    /// Sets the head of the on-disk orphan list (`s_last_orphan`).
    ///
    /// Takes `&mut self` so a write guard over `RwMutex<Dirty<SuperBlock>>` marks
    /// the superblock dirty for writeback; the orphan-list add/remove and the
    /// mount-time recovery scan set it, and it reaches disk through the
    /// captured superblock after-image ([`Self::journal_capture`] patches it
    /// from memory on every capture) or [`Ext4::sync_metadata`](super::fs::Ext4).
    pub(super) fn set_last_orphan(&mut self, head: Option<Ext4Ino>) {
        self.last_orphan = head;
    }

    /// Returns the number of block groups, rounding up the last partial group.
    pub(super) fn nr_block_groups(&self) -> u32 {
        ((self.blocks_count - self.first_data_block - 1) / self.nr_blocks_per_group as u64 + 1)
            as u32
    }

    /// Returns the number of inodes stored per block.
    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(super) const fn inodes_per_block(&self) -> u32 {
        (self.block_size / self.inode_size) as u32
    }

    #[expect(dead_code)]
    pub(super) const fn rev_level(&self) -> RevLevel {
        self.rev_level
    }

    #[expect(dead_code)]
    pub(super) const fn state(&self) -> FsState {
        self.state
    }

    pub(super) const fn uuid(&self) -> &[u8; 16] {
        &self.uuid
    }

    /// Returns the blocks reserved for privileged processes
    /// (`s_r_blocks_count`); `statfs` subtracts them from `bfree` to report
    /// `bavail`.
    pub(super) const fn reserved_blocks_count(&self) -> u32 {
        self.reserved_blocks_count
    }

    /// Returns the inode number of the internal journal (`s_journal_inum`);
    /// the mount contract accepts only the reserved ino 8.
    pub(super) const fn journal_ino(&self) -> u32 {
        self.journal_ino
    }

    /// Returns the external journal's device number (`s_journal_dev`); the
    /// mount contract accepts only `0` (internal journal).
    pub(super) const fn journal_dev(&self) -> u32 {
        self.journal_dev
    }

    /// Captures this superblock's after-image into the operation's transaction
    /// (block 0, RMW at [`SUPER_BLOCK_OFFSET`]), for op-time journaling. A
    /// no-op without a handle.
    ///
    /// Patches **every field the filesystem mutates after mount** — the free
    /// counters straight from `self`, plus `s_last_orphan` — every capture.
    /// This is a single-writer rule, not a convenience: a capture that patched
    /// only "its own" field would leave the others at the seed value, so two
    /// captures patching disjoint fields in different transactions would
    /// clobber each other's committed writes (the B-1 stale-seed class; the
    /// Task 8 first attempt hit exactly this with a counts-only vs.
    /// orphan-only pair). Taking the counters from `&self` makes a
    /// stale/mixed pair unrepresentable — but the caller must hold the
    /// superblock **write** guard across this call and the mutation it
    /// precedes (every call site does; a snapshot taken outside the guard
    /// could patch stale values over a newer capture).
    ///
    /// `last_orphan` stays an explicit parameter: `orphan_add`/`orphan_del`
    /// deliberately capture the *new* head first (capture-fallible) and only
    /// then mutate `self` (infallible), so at capture time `self.last_orphan`
    /// still holds the old value. Every untracked field (label, feature
    /// words, mount counters — changed only at mount time, never under an
    /// operation) survives from the seed. The values are absolute, so
    /// repeated captures converge on the final state.
    pub(super) fn journal_capture(
        &self,
        handle: Option<&journal::Handle>,
        last_orphan: Option<Ext4Ino>,
    ) -> Result<()> {
        let free_inodes = self.free_inodes_count();
        journal::get_write_access(handle, SUPERBLOCK_BID, journal::TriggerType::Superblock)?.patch(
            |buf| {
                let off = SUPER_BLOCK_OFFSET;
                let mut raw =
                    RawSuperBlock::from_bytes(&buf[off..off + size_of::<RawSuperBlock>()]);
                self.write_free_blocks_count(&mut raw);
                raw.free_inodes_count = free_inodes;
                // `0 = empty` is the on-disk convention (encode boundary).
                raw.last_orphan = last_orphan.unwrap_or(0);
                buf[off..off + size_of::<RawSuperBlock>()].copy_from_slice(raw.as_bytes());
            },
        )
    }

    /// Writes the free-block count's low half — and, under `64BIT`, its high half
    /// — into a raw superblock being RMW'd back to disk.
    ///
    /// The single write-side splice boundary for `s_free_blocks_count{,_hi}`,
    /// mirroring the read splice in [`Self::try_from`]. `count as u32` /
    /// `(count >> 32) as u32` is the exact inverse of that read assembly and
    /// lossless as a pair. The high half is emitted only with the `64BIT` feature,
    /// so a non-64bit volume leaves its on-disk `s_free_blocks_count_hi` at the
    /// zero the RMW read back (the 32-byte path stays byte-for-byte unchanged).
    pub(super) fn write_free_blocks_count(&self, raw: &mut RawSuperBlock) {
        let count = self.free_blocks_count;
        raw.free_blocks_count = count as u32;
        if self.feature_incompat.contains(FeatureIncompatSet::IS_64BIT) {
            raw.free_blocks_count_hi = (count >> 32) as u32;
        }
    }

    pub(super) const fn feature_compat(&self) -> FeatureCompatSet {
        self.feature_compat
    }

    pub(super) const fn feature_incompat(&self) -> FeatureIncompatSet {
        self.feature_incompat
    }

    /// Clears the `RECOVER` incompatible flag after the journal has been replayed.
    ///
    /// jbd2 recovery clears the on-disk `INCOMPAT_RECOVER` bit once it has replayed
    /// the log, so a subsequent clean mount sees a consistent volume and does not
    /// re-recover. Takes `&mut self` so a write guard over
    /// `RwMutex<Dirty<SuperBlock>>` marks the superblock dirty for writeback; the
    /// cleared bit reaches disk through [`Ext4::sync_metadata`](super::fs::Ext4).
    pub(super) fn clear_recover(&mut self) {
        self.feature_incompat.remove(FeatureIncompatSet::RECOVER);
    }

    /// Sets the `RECOVER` incompatible flag for the lifetime of a writable
    /// journaled mount (Linux sets it in `ext4_load_journal` and clears it at
    /// clean unmount).
    ///
    /// This is what makes a crash of OUR OWN session recoverable: the bit forces
    /// the next mount to replay the (dirty) log. Without it, a crash while the
    /// device `s_last_orphan` happens to be 0 would skip recovery entirely, and
    /// the first commit of the new session would overwrite the old log —
    /// silently discarding every committed (fsync-acknowledged) transaction.
    pub(super) fn set_recover(&mut self) {
        self.feature_incompat.insert(FeatureIncompatSet::RECOVER);
    }

    #[expect(dead_code)]
    pub(super) const fn feature_ro_compat(&self) -> FeatureRoCompatSet {
        self.feature_ro_compat
    }

    /// Returns whether the volume has a journal that must be replayed before it
    /// can be written (the `RECOVER` bit is set or an orphan list is pending).
    /// `Ext4::open` runs that recovery at mount, before any write.
    pub(super) fn needs_recovery(&self) -> bool {
        self.feature_incompat.contains(FeatureIncompatSet::RECOVER) || self.last_orphan.is_some()
    }
}

/// Resolves and validates the raw `s_desc_size` into the effective
/// group-descriptor size in bytes.
///
/// Mirrors Linux ext4 `super.c`: the on-disk field is honored only with the
/// `64BIT` feature (`EXT4_DESC_SIZE = has_64bit ? s_desc_size : 32`); a `0` on
/// disk means the classic 32-byte descriptor either way. When honored the size
/// must be a power of two within `[MIN_DESC_SIZE, MAX_DESC_SIZE]`.
///
/// The classic-layout branch is deliberately strict: without `64BIT` the
/// descriptor is 32 bytes, so a raw value other than the unset sentinel or the
/// classic size would be silently misread, and we reject it instead.
fn parse_desc_size(raw: u16, has_64bit: bool) -> Result<u16> {
    if !has_64bit {
        // Without the 64bit feature `s_desc_size` is not authoritative: `0`
        // (unset) or the classic `32` both mean 32-byte descriptors, and any
        // other value is a malformed superblock. The sentinel `0` stops here.
        if raw != 0 && raw != MIN_DESC_SIZE {
            return_errno_with_message!(
                Errno::EINVAL,
                "group descriptor size set without the 64bit feature"
            );
        }
        return Ok(MIN_DESC_SIZE);
    }

    // With 64bit, `s_desc_size` is authoritative and MUST describe a 64-bit
    // descriptor. Mirror Linux `super.c` (EXT4_MIN_DESC_SIZE_64BIT): it does NOT
    // fold `0` to a default here and rejects anything below 64. Otherwise a
    // 64bit image whose `s_desc_size` disagrees with its physical 64-byte GDT
    // stride would mount and read every group's descriptor at the wrong offset —
    // the refused-mount → silent-cross-linked-corruption the red-line forbids.
    if raw < MIN_DESC_SIZE_64BIT || raw > MAX_DESC_SIZE || !raw.is_power_of_two() {
        return_errno_with_message!(Errno::EINVAL, "invalid 64bit group descriptor size");
    }
    // `read_desc` strides by this size and decodes the 64-byte layout's high
    // halves; it stays in lockstep with `IS_64BIT ∈ INCOMPAT_SUPP`.
    Ok(raw)
}

/// Splices a superblock 64-bit block count's low and high halves into a `u64`.
///
/// The one read-side boundary for `s_{blocks,free_blocks}_count{,_hi}`
/// (rust_rules #3). `(lo as u64) | ((hi as u64) << 32)` is lossless. The high
/// half is honored only with the `64BIT` feature; a non-64bit image with a
/// non-zero high half is malformed — that field is defined only under `64BIT` —
/// and is rejected rather than silently folded in.
fn splice_count_hi(lo: u32, hi: u32, has_64bit: bool) -> Result<u64> {
    if !has_64bit {
        if hi != 0 {
            return_errno_with_message!(
                Errno::EINVAL,
                "64-bit count high half set without the 64bit feature"
            );
        }
        return Ok(lo as u64);
    }
    Ok((lo as u64) | ((hi as u64) << 32))
}

/// The ext4 revision level (`s_rev_level`).
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, TryFromInt)]
pub(super) enum RevLevel {
    /// Original format with a fixed 128-byte inode.
    GoodOld = 0,
    /// V2 format with a dynamic inode size (ext4 uses this).
    Dynamic = 1,
}

bitflags! {
    /// Filesystem state (`s_state`).
    pub(super) struct FsState: u16 {
        /// Unmounted cleanly.
        const VALID = 1 << 0;
        /// Errors detected.
        const ERROR = 1 << 1;
        /// Orphan inodes are being recovered.
        const ORPHAN = 1 << 2;
    }
}

/// Action taken when an error is detected (`s_errors`).
#[repr(u16)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, TryFromInt)]
pub(super) enum ErrorsBehavior {
    /// Continues execution.
    #[default]
    Continue = 1,
    /// Remounts the filesystem read-only.
    RemountReadonly = 2,
    /// Panics.
    Panic = 3,
}

/// OS that created the filesystem (`s_creator_os`).
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, TryFromInt)]
pub(super) enum OsId {
    Linux = 0,
    Hurd = 1,
    Masix = 2,
    FreeBSD = 3,
    Lites = 4,
}

const_assert!(size_of::<RawSuperBlock>() == SUPER_BLOCK_SIZE);

/// The on-disk superblock structure (exactly 1024 bytes).
///
/// Convert to `SuperBlock` via `TryFrom` for the validated representation.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Pod)]
pub(super) struct RawSuperBlock {
    pub inodes_count: u32,
    pub blocks_count: u32,
    pub reserved_blocks_count: u32,
    pub free_blocks_count: u32,
    pub free_inodes_count: u32,
    pub first_data_block: u32,
    /// The number to left-shift 1024 by to obtain the block size.
    pub log_block_size: u32,
    /// The number to left-shift 1024 by to obtain the fragment size.
    pub log_frag_size: u32,
    pub blocks_per_group: u32,
    pub frags_per_group: u32,
    pub inodes_per_group: u32,
    pub mtime: UnixTime,
    pub wtime: UnixTime,
    pub mnt_count: u16,
    pub max_mnt_count: u16,
    pub magic: u16,
    pub state: u16,
    pub errors: u16,
    pub min_rev_level: u16,
    pub last_check_time: UnixTime,
    pub check_interval: u32,
    pub creator_os: u32,
    pub rev_level: u32,
    pub default_reserved_uid: u16,
    pub default_reserved_gid: u16,
    pub first_ino: u32,
    pub inode_size: u16,
    pub block_group_idx: u16,
    pub feature_compat: u32,
    pub feature_incompat: u32,
    pub feature_ro_compat: u32,
    pub uuid: [u8; 16],
    pub volume_name: Str16,
    pub last_mounted_dir: Str64,
    pub algorithm_usage_bitmap: u32,
    pub prealloc_file_blocks: u8,
    pub prealloc_dir_blocks: u8,
    pub(super) padding1: u16,
    pub journal_uuid: [u8; 16],
    pub journal_ino: u32,
    pub journal_dev: u32,
    pub last_orphan: u32,
    pub hash_seed: [u32; 4],
    pub def_hash_version: u8,
    pub(super) reserved_char_pad: u8,
    /// `s_desc_size`: on-disk group-descriptor size in bytes (offset 0xFE).
    /// Honored only with the `64BIT` feature; `0` means the classic 32-byte
    /// descriptor. Validated at the parse boundary by [`parse_desc_size`].
    pub(super) desc_size: u16,
    pub default_mount_opts: u32,
    pub first_meta_bg: u32,
    /// `s_mkfs_time` (0x108): filesystem creation time.
    pub mkfs_time: UnixTime,
    /// `s_jnl_blocks` (0x10C): backup of the journal inode's block map.
    pub jnl_blocks: [u32; 17],
    /// `s_blocks_count_hi` (0x150): high 32 bits of the total block count.
    /// Honored only with the `64BIT` feature; spliced with `blocks_count` at the
    /// parse boundary ([`SuperBlock::try_from`]).
    pub blocks_count_hi: u32,
    /// `s_r_blocks_count_hi` (0x154): high 32 bits of the reserved block count.
    /// Named for correct field placement; `reserved_blocks_count` stays 32-bit
    /// (a >2^32-block reserve is beyond the supported geometry).
    pub r_blocks_count_hi: u32,
    /// `s_free_blocks_count_hi` (0x158): high 32 bits of the free block count.
    /// Honored only with the `64BIT` feature; spliced with `free_blocks_count` at
    /// the parse boundary and emitted back by [`SuperBlock::write_free_blocks_count`].
    pub free_blocks_count_hi: u32,
    pub(super) reserved: Reserved,
}

/// Reserved padding that fills the on-disk superblock to 1024 bytes. In ext4
/// this region also holds the checksum-seed, mount-option, and metadata-checksum
/// fields, parsed in later phases.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(super) struct Reserved([u32; 169]);

impl Default for Reserved {
    fn default() -> Self {
        Self([0u32; 169])
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::*;

    /// Builds a minimal-feature (extent + filetype) raw superblock for a small
    /// 4 KiB-block image with the given geometry.
    fn minimal_raw(
        blocks_count: u32,
        blocks_per_group: u32,
        inodes_per_group: u32,
    ) -> RawSuperBlock {
        let nr_groups = (blocks_count - 1) / blocks_per_group + 1;
        RawSuperBlock {
            inodes_count: nr_groups * inodes_per_group,
            blocks_count,
            free_blocks_count: 0,
            free_inodes_count: 0,
            first_data_block: 0,
            log_block_size: 2,
            log_frag_size: 2,
            blocks_per_group,
            frags_per_group: blocks_per_group,
            inodes_per_group,
            magic: MAGIC_NUM,
            state: FsState::VALID.bits(),
            errors: ErrorsBehavior::Continue as u16,
            creator_os: OsId::Linux as u32,
            rev_level: RevLevel::Dynamic as u32,
            first_ino: 11,
            inode_size: 256,
            feature_incompat: (FeatureIncompatSet::FILETYPE | FeatureIncompatSet::EXTENTS).bits(),
            feature_ro_compat: FeatureRoCompatSet::SPARSE_SUPER.bits(),
            ..Default::default()
        }
    }

    #[ktest]
    fn parse_minimal_superblock() {
        let raw = minimal_raw(2048, 2048, 256);
        let sb = SuperBlock::try_from(raw).unwrap();
        assert_eq!(sb.block_size(), 4096);
        assert_eq!(sb.inode_size(), 256);
        assert_eq!(sb.first_ino(), 11);
        assert_eq!(sb.inodes_per_block(), 16);
        assert_eq!(sb.nr_inode_table_blocks_per_group(), 16);
        assert_eq!(sb.nr_block_groups(), 1);
        assert!(sb.feature_incompat().contains(FeatureIncompatSet::EXTENTS));
        assert!(!sb.needs_recovery());
    }

    #[ktest]
    fn reject_bad_magic() {
        let mut raw = minimal_raw(2048, 2048, 256);
        raw.magic = 0x1234;
        assert!(SuperBlock::try_from(raw).is_err());
    }

    #[ktest]
    fn reject_non_4k_block() {
        let mut raw = minimal_raw(2048, 2048, 256);
        raw.log_block_size = 0; // 1 KiB
        assert!(SuperBlock::try_from(raw).is_err());
    }

    #[ktest]
    fn reject_unsupported_incompat() {
        // `MMP` is a genuine incompatible feature this implementation does not
        // support (it is not in `INCOMPAT_SUPP`), so it must refuse the mount.
        let mut raw = minimal_raw(2048, 2048, 256);
        raw.feature_incompat |= FeatureIncompatSet::MMP.bits();
        assert!(SuperBlock::try_from(raw).is_err());
    }

    /// A ro_compat feature outside `RO_COMPAT_SUPP` must refuse the (writable)
    /// mount with `EROFS` — writing such a volume would corrupt the feature's
    /// invariants (Linux `ext4_setup_super` parity).
    #[ktest]
    fn reject_unsupported_ro_compat_for_writable_mount() {
        let mut raw = minimal_raw(2048, 2048, 256);
        raw.feature_ro_compat |= FeatureRoCompatSet::METADATA_CSUM.bits();
        let Err(err) = SuperBlock::try_from(raw) else {
            panic!("unsupported ro_compat must not mount writable");
        };
        assert_eq!(err.error(), Errno::EROFS);

        // A bit unknown to `FeatureRoCompatSet` entirely (raw-bits check).
        let mut raw = minimal_raw(2048, 2048, 256);
        raw.feature_ro_compat |= 1 << 12; // RO_COMPAT_READONLY
        assert!(SuperBlock::try_from(raw).is_err());
    }

    #[ktest]
    fn reject_without_extents() {
        let mut raw = minimal_raw(2048, 2048, 256);
        raw.feature_incompat = FeatureIncompatSet::FILETYPE.bits();
        assert!(SuperBlock::try_from(raw).is_err());
    }

    #[ktest]
    fn multi_group_geometry() {
        // 3 full groups of 2048 blocks: blocks_count = 3*2048.
        let raw = minimal_raw(3 * 2048, 2048, 256);
        let sb = SuperBlock::try_from(raw).unwrap();
        assert_eq!(sb.nr_block_groups(), 3);
        assert_eq!(sb.total_blocks(), 3 * 2048);
    }

    /// A `0` on-disk `s_desc_size` (the mkfs default for non-64bit images)
    /// resolves to the classic 32-byte descriptor.
    #[ktest]
    fn desc_size_defaults_to_classic() {
        let raw = minimal_raw(2048, 2048, 256);
        assert_eq!(raw.desc_size, 0);
        let sb = SuperBlock::try_from(raw).unwrap();
        assert_eq!(sb.desc_size(), MIN_DESC_SIZE);
    }

    /// An explicit `s_desc_size == 32` resolves to 32.
    #[ktest]
    fn desc_size_explicit_classic() {
        let mut raw = minimal_raw(2048, 2048, 256);
        raw.desc_size = MIN_DESC_SIZE;
        let sb = SuperBlock::try_from(raw).unwrap();
        assert_eq!(sb.desc_size(), MIN_DESC_SIZE);
    }

    /// Without `64BIT`, any `s_desc_size` other than 0 or 32 is rejected rather
    /// than silently misread by the 32-byte decoder.
    #[ktest]
    fn reject_out_of_range_desc_size() {
        for bad in [16u16, 33, 48, 64, 128] {
            let mut raw = minimal_raw(2048, 2048, 256);
            raw.desc_size = bad;
            let Err(err) = SuperBlock::try_from(raw) else {
                panic!("desc_size {bad} must be rejected without 64bit");
            };
            assert_eq!(err.error(), Errno::EINVAL);
        }
    }

    /// The `EXT4_DESC_SIZE = has_64bit ? s_desc_size : 32` gating, exercised at
    /// the parse boundary directly (the mount-level `IS_64BIT` gate rejects the
    /// feature earlier today, so the 64bit branch is only reachable here).
    #[ktest]
    fn parse_desc_size_gating() {
        // Without 64BIT: 0 and 32 resolve to 32; anything else is rejected.
        assert_eq!(parse_desc_size(0, false).unwrap(), MIN_DESC_SIZE);
        assert_eq!(parse_desc_size(32, false).unwrap(), MIN_DESC_SIZE);
        assert!(parse_desc_size(64, false).is_err());
        assert!(parse_desc_size(48, false).is_err());

        // With 64BIT: `s_desc_size` is authoritative and must be >= 64 (Linux
        // EXT4_MIN_DESC_SIZE_64BIT); 0 and 32 are REJECTED — they would
        // contradict the wide 64-byte GDT stride — as are non-power-of-two and
        // out-of-range. Only 64 is admitted.
        assert_eq!(parse_desc_size(MAX_DESC_SIZE, true).unwrap(), MAX_DESC_SIZE);
        assert!(parse_desc_size(0, true).is_err());
        assert!(parse_desc_size(32, true).is_err());
        assert!(parse_desc_size(48, true).is_err());
        assert!(parse_desc_size(16, true).is_err());
        assert!(parse_desc_size(128, true).is_err());
    }

    /// A `flex_bg` image mounts now that `FLEX_BG` is in `INCOMPAT_SUPP` (the
    /// read side already locates bitmaps/tables through the descriptor getters).
    #[ktest]
    fn accept_flex_bg() {
        let mut raw = minimal_raw(2048, 2048, 256);
        raw.feature_incompat |= FeatureIncompatSet::FLEX_BG.bits();
        let sb = SuperBlock::try_from(raw).unwrap();
        assert!(sb.feature_incompat().contains(FeatureIncompatSet::FLEX_BG));
    }

    /// The `s_*_count_hi` splice boundary: the high half is honored only under
    /// `64BIT`, and a non-64bit high half is rejected rather than folded in. A
    /// genuine `> 2^32`-block image cannot be built here (the fixture's `u32`
    /// `inodes_count` cannot express that geometry), so the wide decode itself is
    /// verified at this boundary.
    #[ktest]
    fn splice_count_hi_gating() {
        // Without 64BIT: the low half passes through; any high half is rejected.
        assert_eq!(splice_count_hi(0x1234, 0, false).unwrap(), 0x1234);
        assert!(splice_count_hi(0, 1, false).is_err());
        assert!(splice_count_hi(5, 7, false).is_err());

        // With 64BIT: `lo | (hi << 32)`, lossless.
        assert_eq!(splice_count_hi(0xDEAD_BEEF, 0, true).unwrap(), 0xDEAD_BEEF);
        assert_eq!(splice_count_hi(0, 1, true).unwrap(), 1u64 << 32);
        assert_eq!(
            splice_count_hi(0x8000_0001, 2, true).unwrap(),
            (2u64 << 32) | 0x8000_0001
        );
    }

    /// A non-64bit image carrying a non-zero `s_blocks_count_hi` or
    /// `s_free_blocks_count_hi` is malformed and rejected at parse time.
    #[ktest]
    fn reject_high_count_without_64bit() {
        let mut raw = minimal_raw(2048, 2048, 256);
        raw.blocks_count_hi = 1;
        assert_eq!(
            SuperBlock::try_from(raw).unwrap_err().error(),
            Errno::EINVAL
        );

        let mut raw = minimal_raw(2048, 2048, 256);
        raw.free_blocks_count_hi = 1;
        assert_eq!(
            SuperBlock::try_from(raw).unwrap_err().error(),
            Errno::EINVAL
        );
    }

    /// A `64BIT` image (feature bit + `s_desc_size == 64`) mounts, records the
    /// wide descriptor size, and round-trips the (here `< 2^32`) free-block count
    /// through the u64 splice.
    #[ktest]
    fn accept_64bit_image() {
        let mut raw = minimal_raw(2048, 2048, 256);
        raw.feature_incompat |= FeatureIncompatSet::IS_64BIT.bits();
        raw.desc_size = MAX_DESC_SIZE;
        raw.free_blocks_count = 500;
        // High halves zero (this small image fits in 32 bits).
        let sb = SuperBlock::try_from(raw).unwrap();
        assert!(sb.feature_incompat().contains(FeatureIncompatSet::IS_64BIT));
        assert_eq!(sb.desc_size(), MAX_DESC_SIZE);
        assert_eq!(sb.free_blocks_count(), 500u64);
        assert_eq!(sb.total_blocks(), 2048u64);
    }
}
