// SPDX-License-Identifier: MPL-2.0
//! JBD2 on-disk 格式结构。**全部大端**（与 ext4 元数据的小端相反）——accessor 用 `from_be`。
use ostd::const_assert;

use crate::fs::ext4::core::prelude::*;

/// JBD2 magic（大端存盘）。
pub const JBD2_MAGIC: u32 = 0xC03B3998;

/// JBD2 块通用头（12 字节，大端）。嵌在 superblock / commit / revoke 块开头。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub struct RawJournalHeader {
    pub h_magic: u32,
    pub h_blocktype: u32,
    pub h_sequence: u32,
}
const_assert!(size_of::<RawJournalHeader>() == 12);

impl RawJournalHeader {
    pub fn magic(&self) -> u32 {
        u32::from_be(self.h_magic)
    }
    pub fn blocktype(&self) -> u32 {
        u32::from_be(self.h_blocktype)
    }
    pub fn sequence(&self) -> u32 {
        u32::from_be(self.h_sequence)
    }
    pub fn is_valid_magic(&self) -> bool {
        self.magic() == JBD2_MAGIC
    }
}

/// JBD2 日志超级块（1024 字节，大端）。含大数组，**不派生 Default**。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub struct RawJournalSuperblock {
    pub s_header: RawJournalHeader,
    pub s_blocksize: u32,
    pub s_maxlen: u32,
    pub s_first: u32,
    pub s_sequence: u32,
    pub s_start: u32,
    pub s_errno: u32,
    pub s_feature_compat: u32,
    pub s_feature_incompat: u32,
    pub s_feature_ro_compat: u32,
    pub s_uuid: [u8; 16],
    pub s_nr_users: u32,
    pub s_dynsuper: u32,
    pub s_max_transaction: u32,
    pub s_max_trans_data: u32,
    pub s_checksum_type: u8,
    pub s_padding2: [u8; 3],
    pub s_num_fc_blocks: u32,
    pub s_head: u32,
    pub s_padding: [u32; 40],
    pub s_checksum: u32,
    pub s_users: [u8; 16 * 48],
}
const_assert!(size_of::<RawJournalSuperblock>() == 1024);

impl RawJournalSuperblock {
    pub fn header(&self) -> RawJournalHeader {
        self.s_header
    }
    pub fn blocksize(&self) -> u32 {
        u32::from_be(self.s_blocksize)
    }
    pub fn maxlen(&self) -> u32 {
        u32::from_be(self.s_maxlen)
    }
    pub fn sequence(&self) -> u32 {
        u32::from_be(self.s_sequence)
    }
}

/// descriptor 块的 tag（CSUM_V2 / pre-v3，8 字节，大端）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub struct RawJournalBlockTag {
    pub t_blocknr: u32,
    pub t_checksum: u16,
    pub t_flags: u16,
}
const_assert!(size_of::<RawJournalBlockTag>() == 8);

impl RawJournalBlockTag {
    pub fn blocknr(&self) -> u32 {
        u32::from_be(self.t_blocknr)
    }
    pub fn flags(&self) -> u16 {
        u16::from_be(self.t_flags)
    }
}

/// descriptor 块的 tag（CSUM_V3，64 位块号，16 字节，大端）。字段序与 RawJournalBlockTag 不同。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub struct RawJournalBlockTag3 {
    pub t_blocknr: u32,
    pub t_flags: u32,
    pub t_blocknr_high: u32,
    pub t_checksum: u32,
}
const_assert!(size_of::<RawJournalBlockTag3>() == 16);

impl RawJournalBlockTag3 {
    /// 64 位块号（high<<32 | low），均大端。
    pub fn blocknr(&self) -> u64 {
        ((u32::from_be(self.t_blocknr_high) as u64) << 32) | (u32::from_be(self.t_blocknr) as u64)
    }
    pub fn flags(&self) -> u32 {
        u32::from_be(self.t_flags)
    }
}

/// commit 块（大端）。末尾 `_padding_tail` 是为 Pod 显式补齐 u64 引入的隐式 padding。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub struct RawCommitBlock {
    pub h_header: RawJournalHeader,
    pub h_chksum_type: u8,
    pub h_chksum_size: u8,
    pub h_padding: [u8; 2],
    pub h_chksum: [u32; 8],
    pub h_commit_sec: u64,
    pub h_commit_nsec: u32,
    pub _padding_tail: u32,
}
const_assert!(size_of::<RawCommitBlock>() == 64);

impl RawCommitBlock {
    pub fn header(&self) -> RawJournalHeader {
        self.h_header
    }
    pub fn commit_sec(&self) -> u64 {
        u64::from_be(self.h_commit_sec)
    }
    pub fn commit_nsec(&self) -> u32 {
        u32::from_be(self.h_commit_nsec)
    }
}

/// revoke 块头（16 字节，大端）。其后跟变长 revoke 记录（Phase 5 解析）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub struct RawRevokeBlockHeader {
    pub r_header: RawJournalHeader,
    pub r_count: u32,
}
const_assert!(size_of::<RawRevokeBlockHeader>() == 16);

impl RawRevokeBlockHeader {
    pub fn header(&self) -> RawJournalHeader {
        self.r_header
    }
    /// revoke 块已用字节数（大端）。
    pub fn count(&self) -> u32 {
        u32::from_be(self.r_count)
    }
}

/// descriptor / revoke 块尾校验和（4 字节，大端）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub struct RawJournalBlockTail {
    pub t_checksum: u32,
}
const_assert!(size_of::<RawJournalBlockTail>() == 4);

impl RawJournalBlockTail {
    pub fn checksum(&self) -> u32 {
        u32::from_be(self.t_checksum)
    }
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::{
        JBD2_MAGIC, RawCommitBlock, RawJournalBlockTag, RawJournalHeader, RawJournalSuperblock,
        RawRevokeBlockHeader,
    };
    use crate::prelude::*;

    #[ktest]
    fn journal_header_be_roundtrip() {
        // 大端存盘：magic 字节序为 C0 3B 39 98。
        let mut b = [0u8; 12];
        b[0..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
        b[4..8].copy_from_slice(&2u32.to_be_bytes()); // blocktype = commit
        b[8..12].copy_from_slice(&7u32.to_be_bytes()); // sequence
        let h = RawJournalHeader::from_bytes(&b);
        assert_eq!(h.as_bytes(), &b[..]);
        assert_eq!(h.magic(), JBD2_MAGIC, "big-endian magic decode");
        assert!(h.is_valid_magic());
        assert_eq!(h.blocktype(), 2);
        assert_eq!(h.sequence(), 7);
    }

    #[ktest]
    fn journal_blocktag_be() {
        let mut b = [0u8; 8];
        b[0..4].copy_from_slice(&0x1234u32.to_be_bytes());
        b[6..8].copy_from_slice(&0x8u16.to_be_bytes()); // flags = LAST_TAG
        let t = RawJournalBlockTag::from_bytes(&b);
        assert_eq!(t.as_bytes(), &b[..]);
        assert_eq!(t.blocknr(), 0x1234);
        assert_eq!(t.flags(), 0x8);
    }

    #[ktest]
    fn journal_superblock_and_commit_revoke_sizes_and_be() {
        // 在 1024 字节缓冲开头放 BE magic，验证 SB 头大端解码 + round-trip。
        let mut sb = [0u8; 1024];
        sb[0..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
        sb[4..8].copy_from_slice(&4u32.to_be_bytes()); // blocktype = SB v2
        let jsb = RawJournalSuperblock::from_bytes(&sb);
        assert_eq!(jsb.as_bytes(), &sb[..]);
        assert_eq!(jsb.header().magic(), JBD2_MAGIC);
        // commit / revoke 头大端解码。
        let mut cb = [0u8; 64];
        cb[0..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
        let c = RawCommitBlock::from_bytes(&cb);
        assert_eq!(c.as_bytes(), &cb[..]);
        assert_eq!(c.header().magic(), JBD2_MAGIC);
        let mut rb = [0u8; 16];
        rb[0..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
        rb[12..16].copy_from_slice(&24u32.to_be_bytes()); // r_count
        let r = RawRevokeBlockHeader::from_bytes(&rb);
        assert_eq!(r.as_bytes(), &rb[..]);
        assert_eq!(r.header().magic(), JBD2_MAGIC);
        assert_eq!(r.count(), 24);
    }
}
