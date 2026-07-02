// SPDX-License-Identifier: MPL-2.0

//! On-disk ext4 superblock parsing and the validated in-memory representation.
//!
//! The ext4 superblock shares its first 264 bytes of layout with ext2; the
//! ext4-specific fields (64-bit counts, descriptor size, checksum) live in the
//! trailing reserved area and are parsed when P6 brings the features that need
//! them. Until then only minimal-feature images (64-bit, flex_bg, and
//! checksums disabled) mount, so the shared layout suffices.

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

        let blocks_count = sb.blocks_count as u64;
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
        if sb.free_blocks_count > sb.blocks_count {
            return_errno_with_message!(Errno::EINVAL, "free blocks count exceeds blocks count");
        }
        if sb.free_inodes_count > sb.inodes_count {
            return_errno_with_message!(Errno::EINVAL, "free inodes count exceeds inodes count");
        }

        Ok(Self {
            inodes_count: sb.inodes_count,
            blocks_count,
            free_blocks_count: sb.free_blocks_count as u64,
            free_inodes_count: sb.free_inodes_count,
            first_data_block,
            block_size,
            nr_blocks_per_group,
            nr_inodes_per_group,
            nr_inode_table_blocks_per_group,
            inode_size,
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

    /// The head of the on-disk orphan list (`s_last_orphan`), `0` when empty.
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
    /// mount-time recovery scan set it, and it reaches disk through the captured
    /// superblock after-image (`journal_superblock` in `fs.rs` patches it from
    /// memory on every capture) or [`Ext4::sync_metadata`](super::fs::Ext4).
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

    /// Blocks reserved for privileged processes (`s_r_blocks_count`); `statfs`
    /// subtracts them from `bfree` to report `bavail`.
    pub(super) const fn reserved_blocks_count(&self) -> u32 {
        self.reserved_blocks_count
    }

    /// The inode number of the internal journal (`s_journal_inum`); the mount
    /// contract accepts only the reserved ino 8.
    pub(super) const fn journal_ino(&self) -> u32 {
        self.journal_ino
    }

    /// The external journal's device number (`s_journal_dev`); the mount
    /// contract accepts only `0` (internal journal).
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
        let free_blocks = self.free_blocks_count();
        let free_inodes = self.free_inodes_count();
        journal::get_write_access(handle, SUPERBLOCK_BID, journal::TriggerType::Superblock)?.patch(
            |buf| {
                let off = SUPER_BLOCK_OFFSET;
                let mut raw =
                    RawSuperBlock::from_bytes(&buf[off..off + size_of::<RawSuperBlock>()]);
                raw.free_blocks_count = free_blocks as u32;
                raw.free_inodes_count = free_inodes;
                // `0 = empty` is the on-disk convention (encode boundary).
                raw.last_orphan = last_orphan.unwrap_or(0);
                buf[off..off + size_of::<RawSuperBlock>()].copy_from_slice(raw.as_bytes());
            },
        )
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
    pub(super) reserved_word_pad: u16,
    pub default_mount_opts: u32,
    pub first_meta_bg: u32,
    pub(super) reserved: Reserved,
}

/// Reserved padding that fills the on-disk superblock to 1024 bytes. In ext4
/// this region also holds 64-bit counts, descriptor size, and checksum fields,
/// parsed in later phases.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(super) struct Reserved([u32; 190]);

impl Default for Reserved {
    fn default() -> Self {
        Self([0u32; 190])
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
        let mut raw = minimal_raw(2048, 2048, 256);
        raw.feature_incompat |= FeatureIncompatSet::IS_64BIT.bits();
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
}
