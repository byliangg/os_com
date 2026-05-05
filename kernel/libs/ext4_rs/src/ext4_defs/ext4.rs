use crate::prelude::*;
use spin::{Mutex, MutexGuard};

use super::*;

#[derive(Debug, Clone)]
pub struct SystemZone {
    pub group: u32,
    pub start_blk: u64,
    pub end_blk: u64,
}

pub struct AllocatorBlockGroupLocks {
    block_groups: Vec<Mutex<()>>,
    superblock: Mutex<Ext4Superblock>,
}

impl AllocatorBlockGroupLocks {
    pub fn new(block_group_count: u32, super_block: Ext4Superblock) -> Self {
        let mut block_groups = Vec::with_capacity(block_group_count as usize);
        for _ in 0..block_group_count {
            block_groups.push(Mutex::new(()));
        }

        Self {
            block_groups,
            superblock: Mutex::new(super_block),
        }
    }

    pub fn lock_block_group(&self, bgid: u32) -> MutexGuard<'_, ()> {
        self.block_groups[bgid as usize].lock()
    }

    pub fn lock_superblock_counter(&self) -> MutexGuard<'_, Ext4Superblock> {
        self.superblock.lock()
    }
}

#[derive(Clone)]
pub struct Ext4 {
    pub block_device: Arc<dyn BlockDevice>,
    pub metadata_writer: Arc<dyn MetadataWriter>,
    pub alloc_guard: Arc<dyn OperationAllocGuard>,
    pub allocator_locks: Arc<AllocatorBlockGroupLocks>,
    pub super_block: Ext4Superblock,
    pub system_zone_cache: Option<Vec<SystemZone>>,
    pub inode_table_blocks: Vec<Ext4Fsblk>,
}
