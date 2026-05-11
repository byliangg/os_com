use crate::prelude::*;
use crate::utils::*;

pub const EXT4_FEATURE_COMPAT_HAS_JOURNAL: u32 = 0x0004;

pub const JBD2_MAGIC_NUMBER: u32 = 0xC03B3998;

pub const JBD2_DESCRIPTOR_BLOCK: u32 = 1;
pub const JBD2_COMMIT_BLOCK: u32 = 2;
pub const JBD2_SUPERBLOCK_V1: u32 = 3;
pub const JBD2_SUPERBLOCK_V2: u32 = 4;
pub const JBD2_REVOKE_BLOCK: u32 = 5;

pub const JBD2_FEATURE_COMPAT_CHECKSUM: u32 = 0x0000_0001;

pub const JBD2_FEATURE_INCOMPAT_REVOKE: u32 = 0x0000_0001;
pub const JBD2_FEATURE_INCOMPAT_64BIT: u32 = 0x0000_0002;
pub const JBD2_FEATURE_INCOMPAT_ASYNC_COMMIT: u32 = 0x0000_0004;
pub const JBD2_FEATURE_INCOMPAT_CSUM_V2: u32 = 0x0000_0008;
pub const JBD2_FEATURE_INCOMPAT_CSUM_V3: u32 = 0x0000_0010;
pub const JBD2_FEATURE_INCOMPAT_FAST_COMMIT: u32 = 0x0000_0020;

pub const JBD2_FLAG_ESCAPE: u32 = 0x0000_0001;
pub const JBD2_FLAG_SAME_UUID: u32 = 0x0000_0002;
pub const JBD2_FLAG_DELETED: u32 = 0x0000_0004;
pub const JBD2_FLAG_LAST_TAG: u32 = 0x0000_0008;

pub const JBD2_CHECKSUM_TYPE_CRC32: u8 = 1;
pub const JBD2_CHECKSUM_TYPE_MD5: u8 = 2;
pub const JBD2_CHECKSUM_TYPE_SHA1: u8 = 3;
pub const JBD2_CHECKSUM_TYPE_CRC32C: u8 = 4;

pub const JBD2_CHECKSUM_BYTES: usize = 8;
pub const JBD2_SUPERBLOCK_SIZE: usize = 1024;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JournalHeader {
    h_magic: u32,
    h_blocktype: u32,
    h_sequence: u32,
}

impl JournalHeader {
    pub fn new(blocktype: u32, sequence: u32) -> Self {
        Self {
            h_magic: JBD2_MAGIC_NUMBER.to_be(),
            h_blocktype: blocktype.to_be(),
            h_sequence: sequence.to_be(),
        }
    }

    pub fn magic(&self) -> u32 {
        u32::from_be(self.h_magic)
    }

    pub fn blocktype(&self) -> u32 {
        u32::from_be(self.h_blocktype)
    }

    pub fn sequence(&self) -> u32 {
        u32::from_be(self.h_sequence)
    }

    pub fn set_sequence(&mut self, sequence: u32) {
        self.h_sequence = sequence.to_be();
    }

    pub fn is_valid_magic(&self) -> bool {
        self.magic() == JBD2_MAGIC_NUMBER
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct JournalSuperblock {
    pub s_header: JournalHeader,
    s_blocksize: u32,
    s_maxlen: u32,
    s_first: u32,
    s_sequence: u32,
    s_start: u32,
    s_errno: u32,
    s_feature_compat: u32,
    s_feature_incompat: u32,
    s_feature_ro_compat: u32,
    s_uuid: [u8; 16],
    s_nr_users: u32,
    s_dynsuper: u32,
    s_max_transaction: u32,
    s_max_trans_data: u32,
    s_checksum_type: u8,
    s_padding2: [u8; 3],
    s_num_fc_blocks: u32,
    s_head: u32,
    s_padding: [u32; 40],
    s_checksum: u32,
    s_users: [u8; 16 * 48],
}

impl Default for JournalSuperblock {
    fn default() -> Self {
        Self {
            s_header: JournalHeader::new(JBD2_SUPERBLOCK_V2, 0),
            s_blocksize: 0,
            s_maxlen: 0,
            s_first: 0,
            s_sequence: 0,
            s_start: 0,
            s_errno: 0,
            s_feature_compat: 0,
            s_feature_incompat: 0,
            s_feature_ro_compat: 0,
            s_uuid: [0; 16],
            s_nr_users: 0,
            s_dynsuper: 0,
            s_max_transaction: 0,
            s_max_trans_data: 0,
            s_checksum_type: 0,
            s_padding2: [0; 3],
            s_num_fc_blocks: 0,
            s_head: 0,
            s_padding: [0; 40],
            s_checksum: 0,
            s_users: [0; 16 * 48],
        }
    }
}

impl JournalSuperblock {
    pub fn new_v2(block_size: u32, maxlen: u32, first: u32, sequence: u32, uuid: [u8; 16]) -> Self {
        let mut sb = Self {
            s_header: JournalHeader::new(JBD2_SUPERBLOCK_V2, sequence),
            s_blocksize: block_size.to_be(),
            s_maxlen: maxlen.to_be(),
            s_first: first.to_be(),
            s_sequence: sequence.to_be(),
            s_start: 0,
            s_errno: 0,
            s_feature_compat: 0,
            s_feature_incompat: 0,
            s_feature_ro_compat: 0,
            s_uuid: uuid,
            s_nr_users: 1u32.to_be(),
            s_dynsuper: 0,
            s_max_transaction: 0,
            s_max_trans_data: 0,
            s_checksum_type: JBD2_CHECKSUM_TYPE_CRC32C,
            s_padding2: [0; 3],
            s_num_fc_blocks: 0,
            s_head: first.to_be(),
            s_padding: [0; 40],
            s_checksum: 0,
            s_users: [0; 16 * 48],
        };
        sb.update_checksum();
        sb
    }

    pub fn blocksize(&self) -> u32 {
        u32::from_be(self.s_blocksize)
    }

    pub fn maxlen(&self) -> u32 {
        u32::from_be(self.s_maxlen)
    }

    pub fn first(&self) -> u32 {
        u32::from_be(self.s_first)
    }

    pub fn sequence(&self) -> u32 {
        u32::from_be(self.s_sequence)
    }

    pub fn start(&self) -> u32 {
        u32::from_be(self.s_start)
    }

    pub fn errno(&self) -> u32 {
        u32::from_be(self.s_errno)
    }

    pub fn feature_compat(&self) -> u32 {
        u32::from_be(self.s_feature_compat)
    }

    pub fn feature_incompat(&self) -> u32 {
        u32::from_be(self.s_feature_incompat)
    }

    pub fn feature_ro_compat(&self) -> u32 {
        u32::from_be(self.s_feature_ro_compat)
    }

    pub fn uuid(&self) -> [u8; 16] {
        self.s_uuid
    }

    pub fn nr_users(&self) -> u32 {
        u32::from_be(self.s_nr_users)
    }

    pub fn checksum_type(&self) -> u8 {
        self.s_checksum_type
    }

    pub fn num_fc_blocks(&self) -> u32 {
        u32::from_be(self.s_num_fc_blocks)
    }

    pub fn head(&self) -> u32 {
        u32::from_be(self.s_head)
    }

    pub fn checksum(&self) -> u32 {
        u32::from_be(self.s_checksum)
    }

    pub fn set_sequence(&mut self, sequence: u32) {
        self.s_header.set_sequence(sequence);
        self.s_sequence = sequence.to_be();
    }

    pub fn set_start(&mut self, start: u32) {
        self.s_start = start.to_be();
    }

    pub fn set_head(&mut self, head: u32) {
        self.s_head = head.to_be();
    }

    pub fn has_incompat_feature(&self, feature: u32) -> bool {
        (self.feature_incompat() & feature) != 0
    }

    pub fn blocknr_size(&self) -> usize {
        if self.has_incompat_feature(JBD2_FEATURE_INCOMPAT_64BIT) {
            size_of::<u64>()
        } else {
            size_of::<u32>()
        }
    }

    pub fn to_bytes(&self) -> [u8; JBD2_SUPERBLOCK_SIZE] {
        debug_assert_eq!(size_of::<JournalSuperblock>(), JBD2_SUPERBLOCK_SIZE);
        let mut out = [0u8; JBD2_SUPERBLOCK_SIZE];
        let src = unsafe {
            core::slice::from_raw_parts(
                self as *const _ as *const u8,
                size_of::<JournalSuperblock>(),
            )
        };
        out.copy_from_slice(src);
        out
    }

    pub fn compute_checksum(&self) -> u32 {
        let mut bytes = self.to_bytes();
        bytes[0xFC..0x100].fill(0);
        ext4_crc32c(EXT4_CRC32_INIT, &bytes, bytes.len() as u32)
    }

    pub fn update_checksum(&mut self) {
        self.s_checksum = self.compute_checksum().to_be();
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JournalBlockTag {
    t_blocknr: u32,
    t_checksum: u16,
    t_flags: u16,
}

impl JournalBlockTag {
    pub fn new(blocknr: u32, checksum: u16, flags: u16) -> Self {
        Self {
            t_blocknr: blocknr.to_be(),
            t_checksum: checksum.to_be(),
            t_flags: flags.to_be(),
        }
    }

    pub fn blocknr(&self) -> u32 {
        u32::from_be(self.t_blocknr)
    }

    pub fn checksum(&self) -> u16 {
        u16::from_be(self.t_checksum)
    }

    pub fn flags(&self) -> u16 {
        u16::from_be(self.t_flags)
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JournalBlockTag3 {
    t_blocknr: u32,
    t_flags: u32,
    t_blocknr_high: u32,
    t_checksum: u32,
}

impl JournalBlockTag3 {
    pub fn new(blocknr: u64, checksum: u32, flags: u32) -> Self {
        Self {
            t_blocknr: (blocknr as u32).to_be(),
            t_flags: flags.to_be(),
            t_blocknr_high: ((blocknr >> 32) as u32).to_be(),
            t_checksum: checksum.to_be(),
        }
    }

    pub fn blocknr(&self) -> u64 {
        ((u32::from_be(self.t_blocknr_high) as u64) << 32) | u32::from_be(self.t_blocknr) as u64
    }

    pub fn checksum(&self) -> u32 {
        u32::from_be(self.t_checksum)
    }

    pub fn flags(&self) -> u32 {
        u32::from_be(self.t_flags)
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CommitBlock {
    pub h_header: JournalHeader,
    h_chksum_type: u8,
    h_chksum_size: u8,
    h_padding: [u8; 2],
    h_chksum: [u32; JBD2_CHECKSUM_BYTES],
    h_commit_sec: u64,
    h_commit_nsec: u32,
}

impl CommitBlock {
    pub fn new(sequence: u32) -> Self {
        Self {
            h_header: JournalHeader::new(JBD2_COMMIT_BLOCK, sequence),
            h_chksum_type: JBD2_CHECKSUM_TYPE_CRC32C,
            h_chksum_size: 4,
            h_padding: [0; 2],
            h_chksum: [0; JBD2_CHECKSUM_BYTES],
            h_commit_sec: 0,
            h_commit_nsec: 0,
        }
    }

    pub fn checksum_type(&self) -> u8 {
        self.h_chksum_type
    }

    pub fn checksum_size(&self) -> u8 {
        self.h_chksum_size
    }

    pub fn with_checksum(mut self, checksum: u32) -> Self {
        self.h_chksum[0] = checksum.to_be();
        self
    }

    pub fn checksum_words(&self) -> [u32; JBD2_CHECKSUM_BYTES] {
        self.h_chksum.map(u32::from_be)
    }

    pub fn commit_sec(&self) -> u64 {
        u64::from_be(self.h_commit_sec)
    }

    pub fn commit_nsec(&self) -> u32 {
        u32::from_be(self.h_commit_nsec)
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RevokeBlockHeader {
    pub r_header: JournalHeader,
    r_count: u32,
}

impl RevokeBlockHeader {
    pub fn new(sequence: u32, count: u32) -> Self {
        Self {
            r_header: JournalHeader::new(JBD2_REVOKE_BLOCK, sequence),
            r_count: count.to_be(),
        }
    }

    pub fn count(&self) -> u32 {
        u32::from_be(self.r_count)
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JournalBlockTail {
    t_checksum: u32,
}

impl JournalBlockTail {
    pub fn new(checksum: u32) -> Self {
        Self {
            t_checksum: checksum.to_be(),
        }
    }

    pub fn checksum(&self) -> u32 {
        u32::from_be(self.t_checksum)
    }
}
