// SPDX-License-Identifier: MPL-2.0
//! JBD2 日志环形空间数学（`[s_first, s_maxlen)` 回绕）。
//!
//! 逐字节/逐语义复刻 ext4_rs `ext4_impls/jbd2/space.rs` 的 `JournalSpace`：
//! `from_superblock`/`free_blocks`/`distance`/`advance`/`advance_head`/`set_tail`
//! 的回绕算术（含算符优先级）。纯内存，不碰盘。
//!
//! 几何字段都是 journal **逻辑块号**（相对 journal 区起点，块 0 = journal 超级块），
//! 取自 [`RawJournalSuperblock`] 的 `s_first`/`s_maxlen`/`s_start`/`s_head`/`s_sequence`。

use super::super::prelude::*;
use super::format::RawJournalSuperblock;

/// JBD2 环形日志空间。`[first, maxlen)` 是可写区间，`first` 紧随逻辑块 0 的超级块。
///
/// 字段语义与 ext4_rs `JournalSpace` 一一对应：`head` = 环写入头（下次 commit 起点），
/// `tail` = 最老未 checkpoint 事务起点。`#[derive(..)]` 同 ext4_rs（Copy 便于差分快照）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::fs::ext4::core) struct JournalSpace {
    first: u32,
    maxlen: u32,
    head: u32,
    tail: u32,
}

impl JournalSpace {
    /// 构造并校验环界。PARITY: ext4_rs `JournalSpace::new`（space.rs:15-31）三关校验：
    /// `first==0 || first>=maxlen` / `head<first || head>=maxlen` / `tail<first || tail>=maxlen`
    /// 任一不满足即 `EINVAL`。
    pub(in crate::fs::ext4::core) fn new(
        first: u32,
        maxlen: u32,
        head: u32,
        tail: u32,
    ) -> Result<Self> {
        // PARITY: ext4_rs space.rs:16-18
        if first == 0 || first >= maxlen {
            return Err(Error::with_message(
                Errno::EINVAL,
                "invalid journal ring bounds",
            ));
        }
        // PARITY: ext4_rs space.rs:19-21
        if head < first || head >= maxlen {
            return Err(Error::with_message(Errno::EINVAL, "invalid journal head"));
        }
        // PARITY: ext4_rs space.rs:22-24
        if tail < first || tail >= maxlen {
            return Err(Error::with_message(Errno::EINVAL, "invalid journal tail"));
        }
        Ok(Self {
            first,
            maxlen,
            head,
            tail,
        })
    }

    /// 从 journal 超级块几何构造。
    /// PARITY: ext4_rs `JournalSpace::from_superblock`（space.rs:33-42）——
    /// `tail = if start==0 { first } else { start }`；`head = if head==0 { tail } else { head }`。
    pub(in crate::fs::ext4::core) fn from_superblock(superblock: &RawJournalSuperblock) -> Result<Self> {
        let first = superblock.first();
        // PARITY: ext4_rs space.rs:35-39
        let tail = if superblock.start() == 0 {
            first
        } else {
            superblock.start()
        };
        // PARITY: ext4_rs space.rs:40
        let head = if superblock.head() == 0 {
            tail
        } else {
            superblock.head()
        };
        Self::new(first, superblock.maxlen(), head, tail)
    }

    pub(in crate::fs::ext4::core) fn first(&self) -> u32 {
        self.first
    }

    pub(in crate::fs::ext4::core) fn maxlen(&self) -> u32 {
        self.maxlen
    }

    pub(in crate::fs::ext4::core) fn head(&self) -> u32 {
        self.head
    }

    pub(in crate::fs::ext4::core) fn tail(&self) -> u32 {
        self.tail
    }

    /// 可用块数 = `maxlen - first`。PARITY: ext4_rs space.rs:60-62。
    pub(in crate::fs::ext4::core) fn usable_blocks(&self) -> u32 {
        self.maxlen - self.first
    }

    /// 已用块数（含回绕）。PARITY: ext4_rs space.rs:64-72——
    /// `head==tail`→0；`head>tail`→`head-tail`；否则 `(maxlen-tail)+(head-first)`。
    pub(in crate::fs::ext4::core) fn used_blocks(&self) -> u32 {
        if self.head == self.tail {
            0
        } else if self.head > self.tail {
            self.head - self.tail
        } else {
            (self.maxlen - self.tail) + (self.head - self.first)
        }
    }

    /// 空闲块数 = `usable - used`（饱和减）。PARITY: ext4_rs space.rs:74-76。
    pub(in crate::fs::ext4::core) fn free_blocks(&self) -> u32 {
        self.usable_blocks().saturating_sub(self.used_blocks())
    }

    /// 把 head 前进 `blocks`（环内回绕），返回新 head。PARITY: ext4_rs space.rs:78-81。
    pub(in crate::fs::ext4::core) fn advance_head(&mut self, blocks: u32) -> u32 {
        self.head = self.advance(self.head, blocks);
        self.head
    }

    /// 把 tail 前进 `blocks`（环内回绕），返回新 tail。PARITY: ext4_rs space.rs:83-86。
    #[allow(dead_code)] // ext4_rs 暴露但 Task 2 差分不直接用；保留以保接口对齐
    pub(in crate::fs::ext4::core) fn advance_tail(&mut self, blocks: u32) -> u32 {
        self.tail = self.advance(self.tail, blocks);
        self.tail
    }

    /// 直接置 tail（带界校验）。PARITY: ext4_rs space.rs:88-94——
    /// `tail<first || tail>=maxlen` 即 `EINVAL`，否则覆盖。
    pub(in crate::fs::ext4::core) fn set_tail(&mut self, tail: u32) -> Result<()> {
        if tail < self.first || tail >= self.maxlen {
            return Err(Error::with_message(Errno::EINVAL, "invalid journal tail"));
        }
        self.tail = tail;
        Ok(())
    }

    /// 环内前进：把 `from` 在 `[first, maxlen)` 内前进 `blocks`。
    /// PARITY: ext4_rs space.rs:96-103——`usable==0`→`first`；否则
    /// `relative = (from - first + blocks % usable) % usable; first + relative`。
    /// **逐字保留算符优先级**：`blocks % usable` 先算，再与 `(from-first)` 相加，再整体 `% usable`。
    pub(in crate::fs::ext4::core) fn advance(&self, from: u32, blocks: u32) -> u32 {
        let usable = self.usable_blocks();
        if usable == 0 {
            return self.first;
        }
        // PARITY: ext4_rs space.rs:101 —— 算符优先级与括号逐字一致。
        let relative = (from - self.first + blocks % usable) % usable;
        self.first + relative
    }

    /// `from`→`to` 的环内距离。PARITY: ext4_rs space.rs:105-113——
    /// `from==to`→0；`to>from`→`to-from`；否则 `(maxlen-from)+(to-first)`。
    pub(in crate::fs::ext4::core) fn distance(&self, from: u32, to: u32) -> u32 {
        if from == to {
            0
        } else if to > from {
            to - from
        } else {
            (self.maxlen - from) + (to - self.first)
        }
    }
}
