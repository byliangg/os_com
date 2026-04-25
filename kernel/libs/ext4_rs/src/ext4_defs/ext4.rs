use crate::prelude::*;

use super::*;

#[derive(Debug, Clone)]
pub struct SystemZone {
    pub group: u32,
    pub start_blk: u64,
    pub end_blk: u64,
}

#[derive(Clone)]
pub struct Ext4 {
    pub block_device: Arc<dyn BlockDevice>,
    pub metadata_writer: Arc<dyn MetadataWriter>,
    pub alloc_guard: Arc<dyn OperationAllocGuard>,
    pub super_block: Ext4Superblock,
    pub system_zone_cache: Option<Vec<SystemZone>>,
    pub inode_table_blocks: Vec<Ext4Fsblk>,
}
