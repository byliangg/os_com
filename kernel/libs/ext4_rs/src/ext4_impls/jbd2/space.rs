use crate::prelude::*;
use crate::return_errno_with_message;

use super::JournalSuperblockState;

#[derive(Debug, Clone, Copy)]
pub struct JournalSpace {
    first: u32,
    maxlen: u32,
    head: u32,
    tail: u32,
}

impl JournalSpace {
    pub fn new(first: u32, maxlen: u32, head: u32, tail: u32) -> Result<Self> {
        if first == 0 || first >= maxlen {
            return_errno_with_message!(Errno::EINVAL, "invalid journal ring bounds");
        }
        if head < first || head >= maxlen {
            return_errno_with_message!(Errno::EINVAL, "invalid journal head");
        }
        if tail < first || tail >= maxlen {
            return_errno_with_message!(Errno::EINVAL, "invalid journal tail");
        }
        Ok(Self {
            first,
            maxlen,
            head,
            tail,
        })
    }

    pub fn from_superblock(superblock: &JournalSuperblockState) -> Result<Self> {
        let first = superblock.first();
        let tail = if superblock.start() == 0 {
            first
        } else {
            superblock.start()
        };
        let head = if superblock.head() == 0 { tail } else { superblock.head() };
        Self::new(first, superblock.maxlen(), head, tail)
    }

    pub fn first(&self) -> u32 {
        self.first
    }

    pub fn maxlen(&self) -> u32 {
        self.maxlen
    }

    pub fn head(&self) -> u32 {
        self.head
    }

    pub fn tail(&self) -> u32 {
        self.tail
    }

    pub fn usable_blocks(&self) -> u32 {
        self.maxlen - self.first
    }

    pub fn used_blocks(&self) -> u32 {
        if self.head == self.tail {
            0
        } else if self.head > self.tail {
            self.head - self.tail
        } else {
            (self.maxlen - self.tail) + (self.head - self.first)
        }
    }

    pub fn free_blocks(&self) -> u32 {
        self.usable_blocks().saturating_sub(self.used_blocks())
    }

    pub fn advance_head(&mut self, blocks: u32) -> u32 {
        self.head = self.advance(self.head, blocks);
        self.head
    }

    pub fn advance_tail(&mut self, blocks: u32) -> u32 {
        self.tail = self.advance(self.tail, blocks);
        self.tail
    }

    pub fn set_tail(&mut self, tail: u32) -> Result<()> {
        if tail < self.first || tail >= self.maxlen {
            return_errno_with_message!(Errno::EINVAL, "invalid journal tail");
        }
        self.tail = tail;
        Ok(())
    }

    pub fn advance(&self, from: u32, blocks: u32) -> u32 {
        let usable = self.usable_blocks();
        if usable == 0 {
            return self.first;
        }
        let relative = (from - self.first + blocks % usable) % usable;
        self.first + relative
    }

    pub fn distance(&self, from: u32, to: u32) -> u32 {
        if from == to {
            0
        } else if to > from {
            to - from
        } else {
            (self.maxlen - from) + (to - self.first)
        }
    }
}
