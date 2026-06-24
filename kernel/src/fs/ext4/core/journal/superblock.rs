// SPDX-License-Identifier: MPL-2.0
//! JBD2 日志超级块 load/store + JBD2 校验和套件（全大端）。
//!
//! 逐字节复刻 ext4_rs `ext4_impls/jbd2/superblock.rs`（SB load/store/validate）与
//! `ext4_impls/jbd2/mod.rs` 的 4 类 crc32c 校验（descriptor-tail / per-tag data /
//! commit / SB），及 `ext4_defs/jbd2.rs` 的 `JournalSuperblock::compute_checksum`。
//!
//! **JBD2 全大端**：SB 经 Pod `from_bytes`/`from_be` 读，构造经 `as_bytes`/`to_be` 写；
//! 喂 crc 的标量（UUID 16 字节、sequence u32→4 大端字节、整块/整 SB 切片）是字节序列
//! 编码而非 on-disk 结构解析——**不用** `from_le_bytes`。
//!
//! 校验和：crc32c（[`ext4_crc32c`]），初值 [`EXT4_CRC32_INIT`]=0xFFFFFFFF，**不取反**，
//! 种子为 journal UUID（SB 的 `s_uuid`，16 字节）。

use super::super::crc::{ext4_crc32c, EXT4_CRC32_INIT};
use super::super::io::BlockReader;
use super::super::metadata_writer::MetadataWriter;
use super::super::prelude::*;
use super::format::{
    RawJournalSuperblock, JBD2_MAGIC, JBD2_SUPERBLOCK_SIZE, JBD2_SUPERBLOCK_V2,
};

/// SB `s_checksum` 字段所在的字节区间 [0xFC, 0x100)。compute_checksum 时该 4 字节置零。
/// PARITY: ext4_rs `JournalSuperblock::compute_checksum`（jbd2.rs:258-262）`bytes[0xFC..0x100].fill(0)`。
const SB_CHECKSUM_RANGE: core::ops::Range<usize> = 0xFC..0x100;

/// 读 journal **逻辑块 0**（= `physical_blocks[0]`）的前 1024 字节为 [`RawJournalSuperblock`]，
/// 校验 magic（大端 0xC03B3998）+ blocktype == SUPERBLOCK_V2(4)。
///
/// PARITY: ext4_rs `JournalSuperblockState::load`（superblock.rs:13-29）读 journal device 块 0
/// 的前 `JBD2_SUPERBLOCK_SIZE` 字节 → 解析 → `validate` 的 magic/blocktype 两关；core 侧只做
/// load + magic/version 两关（block_size/maxlen/first/start 等几何关由调用方/Task 2 用 geom 校验，
/// 此处保持「能 load + 算法正确」即可）。`physical_blocks[0] * block_size` 折算字节偏移。
pub(in crate::fs::ext4::core) fn load_journal_sb(
    reader: &dyn BlockReader,
    physical_blocks: &[Ext4Fsblk],
    block_size: usize,
) -> Result<RawJournalSuperblock> {
    let sb_pblock = *physical_blocks.first().ok_or_else(|| {
        Error::with_message(Errno::EINVAL, "journal has no physical blocks")
    })?;
    if block_size < JBD2_SUPERBLOCK_SIZE {
        return Err(Error::with_message(
            Errno::EINVAL,
            "journal block size smaller than JBD2 superblock",
        ));
    }
    // 读整块，取前 1024 字节解析（journal SB 占块 0 的前 1024 字节）。
    let mut block = vec![0u8; block_size];
    reader.read_at((sb_pblock as usize) * block_size, block.as_mut_slice());
    let sb = RawJournalSuperblock::from_bytes(&block[..JBD2_SUPERBLOCK_SIZE]);

    // PARITY: ext4_rs validate（superblock.rs:36-41）—— magic 大端 0xC03B3998 + blocktype==V2。
    let header = sb.header();
    if header.magic() != JBD2_MAGIC {
        return Err(Error::with_message(Errno::EINVAL, "invalid JBD2 magic"));
    }
    if header.blocktype() != JBD2_SUPERBLOCK_V2 {
        return Err(Error::with_message(
            Errno::EINVAL,
            "unsupported JBD2 superblock version",
        ));
    }
    Ok(sb)
}

/// 把 `sb` 写回 journal **逻辑块 0**（= `physical_blocks[0]`）。
///
/// PARITY: ext4_rs `JournalSuperblockState::store`（superblock.rs:155-162）`device.write_block(.., 0, bytes)`。
/// 经 `MetadataWriter::write_metadata_for_handle`（差分里 home 直写同一份字节）落 SB 的 1024 字节
/// 到块 0 的前 1024 字节。注意：调用方负责在改字段后重算 csum（见 [`journal_sb_checksum`]）——
/// 本函数只负责落盘当前镜像（store 不重算，与 ext4_rs 一致）。
#[allow(dead_code)] // Task 3 (commit) wires SB store back to journal block 0 after ring advance
pub(in crate::fs::ext4::core) fn store_journal_sb(
    writer: &dyn MetadataWriter,
    handle_id: u64,
    physical_blocks: &[Ext4Fsblk],
    sb: &RawJournalSuperblock,
) -> Result<()> {
    let sb_pblock = *physical_blocks.first().ok_or_else(|| {
        Error::with_message(Errno::EINVAL, "journal has no physical blocks")
    })?;
    // SB 是 1024 字节；ext4_rs `to_bytes` 写恰 1024 字节到块 0 前段。
    writer.write_metadata_for_handle(handle_id, sb_pblock, sb.as_bytes())
}

/// 计算 journal 超级块校验和（`s_checksum` 字段值，未做大端编码）。
///
/// PARITY: ext4_rs `JournalSuperblock::compute_checksum`（jbd2.rs:258-262）——
/// 拷贝 1024 字节 SB 镜像 → 把 `[0xFC..0x100]`（s_checksum 字段）置零 →
/// `ext4_crc32c(EXT4_CRC32_INIT, &image, image.len())`。**init 0xFFFFFFFF 不取反**。
/// 返回值是逻辑 csum；写盘时调用方再 `.to_be()` 存入 `s_checksum`。
pub(in crate::fs::ext4::core) fn journal_sb_checksum(sb_bytes: &[u8; JBD2_SUPERBLOCK_SIZE]) -> u32 {
    let mut image = *sb_bytes;
    image[SB_CHECKSUM_RANGE].fill(0); // PARITY: 置零 s_checksum 字段再算（含其自身）
    ext4_crc32c(EXT4_CRC32_INIT, &image) // PARITY: init=0xFFFFFFFF, NOT inverted
}

/// descriptor 块尾校验和（`JournalBlockTail::t_checksum` 的逻辑值）。
///
/// PARITY: ext4_rs `build_descriptor_block` csum（mod.rs:309-323）——
/// `crc32c(EXT4_CRC32_INIT, UUID(16) ++ 整 descriptor 块)`，其中 descriptor 块的 tail csum
/// 字段在计算时**仍为零**（调用方先零填 tail、算完再写回）。种子=journal UUID。
pub(in crate::fs::ext4::core) fn descriptor_tail_csum(uuid: &[u8; 16], descriptor_block: &[u8]) -> u32 {
    let mut csum_data = Vec::with_capacity(16 + descriptor_block.len());
    csum_data.extend_from_slice(uuid); // PARITY: 种子 = journal UUID（大端无关，原样 16 字节）
    csum_data.extend_from_slice(descriptor_block); // PARITY: 整块，tail csum 字段置零时算
    ext4_crc32c(EXT4_CRC32_INIT, &csum_data) // PARITY: init=0xFFFFFFFF, NOT inverted
}

/// 单个 journaled 数据块的 per-tag 校验和（v2 取低 16 位 / v3 用全 32 位，由调用方截断）。
///
/// PARITY: ext4_rs `data_block_checksum`（mod.rs:441-447）——
/// `crc32c(EXT4_CRC32_INIT, UUID(16) ++ sequence(4 大端字节) ++ 未 escape 的原始块数据)`。
/// `sequence.to_be_bytes()` 是标量整数的大端字节编码（**非** on-disk 结构解析，允许）。
/// 注意喂的是**原始**块（escape 还原前）——见 ext4_rs escape 在 csum 之后才置零首 4 字节。
pub(in crate::fs::ext4::core) fn tag_data_csum(uuid: &[u8; 16], sequence: u32, orig_block_data: &[u8]) -> u32 {
    let mut csum_data = Vec::with_capacity(16 + 4 + orig_block_data.len());
    csum_data.extend_from_slice(uuid); // PARITY: 种子 = journal UUID
    csum_data.extend_from_slice(&sequence.to_be_bytes()); // PARITY: seq 大端 4 字节
    csum_data.extend_from_slice(orig_block_data); // PARITY: 原始（未 escape）块数据
    ext4_crc32c(EXT4_CRC32_INIT, &csum_data) // PARITY: init=0xFFFFFFFF, NOT inverted
}

/// commit 块校验和（`CommitBlock::h_chksum[0]` 的逻辑值）。
///
/// PARITY: ext4_rs `commit_block_checksum`（mod.rs:449-457）——
/// `crc32c(EXT4_CRC32_INIT, UUID(16) ++ CommitBlock 结构字节)`，其中 `h_chksum[0]`
/// 在计算时**仍为零**（先建 commit、算 csum、再 `with_checksum` 写回 h_chksum[0]）。
/// `commit_block` 应为 `size_of::<RawCommitBlock>()`（64 字节）的结构字节，**非**整块。
pub(in crate::fs::ext4::core) fn commit_block_csum(uuid: &[u8; 16], commit_block: &[u8]) -> u32 {
    let mut csum_data = Vec::with_capacity(16 + commit_block.len());
    csum_data.extend_from_slice(uuid); // PARITY: 种子 = journal UUID
    csum_data.extend_from_slice(commit_block); // PARITY: CommitBlock 字节，h_chksum[0] 置零时算
    ext4_crc32c(EXT4_CRC32_INIT, &csum_data) // PARITY: init=0xFFFFFFFF, NOT inverted
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::*;

    use super::super::format::JBD2_SUPERBLOCK_SIZE;
    use super::{
        commit_block_csum, descriptor_tail_csum, journal_sb_checksum, tag_data_csum,
    };
    use crate::prelude::*;

    /// `journal_sb_checksum` 在置零 [0xFC..0x100) 之外的语义自检（不依赖镜像）：
    /// 改写校验和字段自身不影响结果，改写其它字节会影响结果。
    #[ktest]
    fn journal_sb_checksum_zeros_csum_field() {
        let mut sb = [0u8; JBD2_SUPERBLOCK_SIZE];
        for (i, b) in sb.iter_mut().enumerate() {
            *b = (i & 0xFF) as u8;
        }
        let base = journal_sb_checksum(&sb);
        // 改 s_checksum 字段（0xFC..0x100）不应改变结果（计算前已置零）。
        let mut sb2 = sb;
        sb2[0xFC] ^= 0xFF;
        sb2[0xFF] ^= 0xAA;
        assert_eq!(base, journal_sb_checksum(&sb2), "s_checksum field must be excluded");
        // 改其它字节必改变结果。
        let mut sb3 = sb;
        sb3[0] ^= 0x01;
        assert_ne!(base, journal_sb_checksum(&sb3), "other bytes must affect csum");
    }

    /// 校验和套件的形状自检：UUID 种子参与、seq 大端参与、与裸 crc 不同（不取反由 crc.rs 保证）。
    #[ktest]
    fn csum_suite_seeds_observed() {
        use super::super::super::crc::{ext4_crc32c, EXT4_CRC32_INIT};

        let uuid = [0x11u8; 16];
        let block = vec![0xABu8; 64];
        // descriptor-tail = crc(UUID ++ block)；与 crc(block) 不同（UUID 真参与）。
        let with_uuid = descriptor_tail_csum(&uuid, &block);
        let no_uuid = ext4_crc32c(EXT4_CRC32_INIT, &block);
        assert_ne!(with_uuid, no_uuid, "UUID must seed descriptor-tail csum");

        // per-tag：不同 sequence 必产不同 csum（seq 大端真参与）。
        let c1 = tag_data_csum(&uuid, 1, &block);
        let c2 = tag_data_csum(&uuid, 2, &block);
        assert_ne!(c1, c2, "sequence must affect per-tag data csum");

        // commit：UUID ++ commit 字节。
        let commit = vec![0u8; 64];
        let cc = commit_block_csum(&uuid, &commit);
        let cc_no_uuid = ext4_crc32c(EXT4_CRC32_INIT, &commit);
        assert_ne!(cc, cc_no_uuid, "UUID must seed commit csum");
    }
}
