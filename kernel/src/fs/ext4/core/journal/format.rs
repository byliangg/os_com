// SPDX-License-Identifier: MPL-2.0
//! JBD2 on-disk 格式结构。**全部大端**（与 ext4 元数据的小端相反）——accessor 用 `from_be`。
use ostd::const_assert;

use crate::fs::ext4::core::prelude::*;

/// JBD2 magic（大端存盘）。
pub const JBD2_MAGIC: u32 = 0xC03B3998;

/// journal 超级块块类型 v2（`s_header.h_blocktype`，大端解码后值）。
/// 对应 ext4_rs `JBD2_SUPERBLOCK_V2`。
pub const JBD2_SUPERBLOCK_V2: u32 = 4;

/// descriptor 块类型（`h_blocktype` 逻辑值 1）。对应 ext4_rs `JBD2_DESCRIPTOR_BLOCK`。
pub const JBD2_DESCRIPTOR_BLOCK: u32 = 1;
/// commit 块类型（`h_blocktype` 逻辑值 2）。对应 ext4_rs `JBD2_COMMIT_BLOCK`。
pub const JBD2_COMMIT_BLOCK: u32 = 2;

/// commit 块校验和类型 crc32c（`h_chksum_type`）。对应 ext4_rs `JBD2_CHECKSUM_TYPE_CRC32C`。
pub const JBD2_CHECKSUM_TYPE_CRC32C: u8 = 4;

/// incompat 特性位：64BIT（descriptor tag 携带 64 位块号高半，tag 长 12 字节而非 8）。
/// 对应 ext4_rs `JBD2_FEATURE_INCOMPAT_64BIT`。
pub const JBD2_FEATURE_INCOMPAT_64BIT: u32 = 0x0000_0002;

/// descriptor tag flag：本 tag 的 journaled 块首 4 字节被 escape（写盘置零）。
/// 对应 ext4_rs `JBD2_FLAG_ESCAPE`。
pub const JBD2_FLAG_ESCAPE: u32 = 0x0000_0001;
/// descriptor tag flag：该 tag 与上一个 tag 共用同一 UUID（不另携带 UUID）。
/// 对应 ext4_rs `JBD2_FLAG_SAME_UUID`。
pub const JBD2_FLAG_SAME_UUID: u32 = 0x0000_0002;
/// descriptor tag flag：本 tag 是该 descriptor 块的最后一个 tag。
/// 对应 ext4_rs `JBD2_FLAG_LAST_TAG`。
pub const JBD2_FLAG_LAST_TAG: u32 = 0x0000_0008;

/// journal 超级块大小（字节）；JBD2 SB 占 journal 逻辑块 0 的前 1024 字节。
/// 对应 ext4_rs `JBD2_SUPERBLOCK_SIZE`。
pub const JBD2_SUPERBLOCK_SIZE: usize = 1024;

/// incompat 特性位：CSUM_V2（descriptor/commit/SB 用 crc32c 校验）。对应 ext4_rs 同名常量。
pub const JBD2_FEATURE_INCOMPAT_CSUM_V2: u32 = 0x0000_0008;
/// incompat 特性位：CSUM_V3（64 位块号 tag + 全 32 位 tag csum）。对应 ext4_rs 同名常量。
pub const JBD2_FEATURE_INCOMPAT_CSUM_V3: u32 = 0x0000_0010;

/// JBD2 块通用头（12 字节，大端）。嵌在 superblock / commit / revoke 块开头。
///
/// 可见性 `pub(in crate::fs::ext4::core)`：随 [`RawJournalSuperblock::header`] 一同暴露给
/// core 下的 ktest 差分 harness（避免 `pub` accessor 返回更私有类型的 private-in-public）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub(in crate::fs::ext4::core) struct RawJournalHeader {
    pub h_magic: u32,
    pub h_blocktype: u32,
    pub h_sequence: u32,
}
const_assert!(size_of::<RawJournalHeader>() == 12);

impl RawJournalHeader {
    /// 构造大端块头：magic 固定 `JBD2_MAGIC`，`blocktype`/`sequence` 编码为大端。
    /// PARITY: ext4_rs `JournalHeader::new`（jbd2.rs:45-51）。
    pub fn new(blocktype: u32, sequence: u32) -> Self {
        Self {
            h_magic: JBD2_MAGIC.to_be(),
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
    pub fn is_valid_magic(&self) -> bool {
        self.magic() == JBD2_MAGIC
    }
}

/// JBD2 日志超级块（1024 字节，大端）。含大数组，**不派生 Default**。
///
/// 可见性 `pub(in crate::fs::ext4::core)`：除 `journal` 模块自身外，仅 ktest 差分 harness
/// （`super::super::diff_harness`，core 下的兄弟模块）需要直接读它来对拍 journal 超级块；
/// 其余 `RawJournal*` 内部类型保持 `pub(super)`。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
pub(in crate::fs::ext4::core) struct RawJournalSuperblock {
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
    /// 日志环第一个可用块（紧随逻辑块 0 的超级块）。
    pub fn first(&self) -> u32 {
        u32::from_be(self.s_first)
    }
    pub fn sequence(&self) -> u32 {
        u32::from_be(self.s_sequence)
    }
    /// 当前最老未 checkpoint 事务的起始块；0 表示日志为空（无需恢复）。
    pub fn start(&self) -> u32 {
        u32::from_be(self.s_start)
    }
    /// 环形写入头（下一次 commit 的起始块）。
    pub fn head(&self) -> u32 {
        u32::from_be(self.s_head)
    }
    /// incompat 特性位（大端解码）。
    pub fn feature_incompat(&self) -> u32 {
        u32::from_be(self.s_feature_incompat)
    }
    /// journal UUID（16 字节，原样字节序——既是 csum 种子也用于 SB UUID 校验）。
    pub fn uuid(&self) -> [u8; 16] {
        self.s_uuid
    }
    /// SB 校验和字段（偏移 0xFC，大端解码）。仅 CSUM_V2/V3 特性开时有意义。
    pub fn checksum(&self) -> u32 {
        u32::from_be(self.s_checksum)
    }
    /// 是否开启 JBD2 校验和（CSUM_V2 或 CSUM_V3 任一）。门控 SB/descriptor/commit csum 写读。
    pub fn has_checksum_v2_or_v3(&self) -> bool {
        (self.feature_incompat() & (JBD2_FEATURE_INCOMPAT_CSUM_V2 | JBD2_FEATURE_INCOMPAT_CSUM_V3))
            != 0
    }
    /// 是否开启某 incompat 特性。PARITY: ext4_rs `JournalSuperblock::has_incompat_feature`（jbd2.rs:233-235）。
    pub fn has_incompat_feature(&self, feature: u32) -> bool {
        (self.feature_incompat() & feature) != 0
    }
    /// 置环写入头（`s_head`，大端编码）。PARITY: ext4_rs `set_head`（jbd2.rs:229-231）。
    pub fn set_head(&mut self, head: u32) {
        self.s_head = head.to_be();
    }
    /// 置最老未 checkpoint 事务起点（`s_start`，大端编码）。
    /// PARITY: ext4_rs `set_start`（jbd2.rs:225-227）。
    pub fn set_start(&mut self, start: u32) {
        self.s_start = start.to_be();
    }
    /// 置下一个事务序号。PARITY: ext4_rs `JournalSuperblock::set_sequence`（jbd2.rs:220-223）——
    /// **同时**更新 `s_header.h_sequence` 与 `s_sequence`（两处都大端编码），逐字复刻。
    pub fn set_sequence(&mut self, sequence: u32) {
        self.s_header.h_sequence = sequence.to_be();
        self.s_sequence = sequence.to_be();
    }
    /// 置 SB 校验和字段（`s_checksum`，大端编码）。逻辑值由 [`super::superblock::journal_sb_checksum`] 算。
    /// PARITY: ext4_rs `update_checksum`（jbd2.rs:264-266）`s_checksum = compute_checksum().to_be()`。
    pub fn set_checksum(&mut self, checksum: u32) {
        self.s_checksum = checksum.to_be();
    }
}

/// descriptor 块的 tag（CSUM_V2 / pre-v3，8 字节，大端）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub(super) struct RawJournalBlockTag {
    pub t_blocknr: u32,
    pub t_checksum: u16,
    pub t_flags: u16,
}
const_assert!(size_of::<RawJournalBlockTag>() == 8);

impl RawJournalBlockTag {
    /// 构造大端 8 字节 tag。PARITY: ext4_rs `JournalBlockTag::new`（jbd2.rs:278-284）。
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
    pub fn flags(&self) -> u16 {
        u16::from_be(self.t_flags)
    }
}

/// descriptor 块的 tag（CSUM_V3，64 位块号，16 字节，大端）。字段序与 RawJournalBlockTag 不同。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Default)]
pub(super) struct RawJournalBlockTag3 {
    pub t_blocknr: u32,
    pub t_flags: u32,
    pub t_blocknr_high: u32,
    pub t_checksum: u32,
}
const_assert!(size_of::<RawJournalBlockTag3>() == 16);

impl RawJournalBlockTag3 {
    /// 构造大端 16 字节 tag（64 位块号拆 low/high）。
    /// PARITY: ext4_rs `JournalBlockTag3::new`（jbd2.rs:309-316）。
    pub fn new(blocknr: u64, checksum: u32, flags: u32) -> Self {
        Self {
            t_blocknr: (blocknr as u32).to_be(),
            t_flags: flags.to_be(),
            t_blocknr_high: ((blocknr >> 32) as u32).to_be(),
            t_checksum: checksum.to_be(),
        }
    }
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
pub(super) struct RawCommitBlock {
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
    /// 构造 commit 块（h_chksum_type=CRC32C(4)、h_chksum_size=4，其余零；时间戳留 0）。
    /// PARITY: ext4_rs `CommitBlock::new`（jbd2.rs:344-354）——**逐字复刻**含 h_commit_sec/nsec=0
    /// （ext4_rs 不填真实时间）。`_padding_tail` 是 core 为 Pod u64 对齐显式补的隐式 padding，置 0。
    pub fn new(sequence: u32) -> Self {
        Self {
            h_header: RawJournalHeader::new(JBD2_COMMIT_BLOCK, sequence),
            h_chksum_type: JBD2_CHECKSUM_TYPE_CRC32C,
            h_chksum_size: 4,
            h_padding: [0; 2],
            h_chksum: [0; 8],
            h_commit_sec: 0,
            h_commit_nsec: 0,
            _padding_tail: 0,
        }
    }
    /// 写入 commit 校验和到 `h_chksum[0]`（大端编码）。
    /// PARITY: ext4_rs `CommitBlock::with_checksum`（jbd2.rs:364-367）`h_chksum[0] = checksum.to_be()`。
    pub fn with_checksum(mut self, checksum: u32) -> Self {
        self.h_chksum[0] = checksum.to_be();
        self
    }
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
pub(super) struct RawRevokeBlockHeader {
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
pub(super) struct RawJournalBlockTail {
    pub t_checksum: u32,
}
const_assert!(size_of::<RawJournalBlockTail>() == 4);

impl RawJournalBlockTail {
    /// 构造大端块尾校验和。PARITY: ext4_rs `JournalBlockTail::new`（jbd2.rs:409-413）。
    pub fn new(checksum: u32) -> Self {
        Self {
            t_checksum: checksum.to_be(),
        }
    }
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
