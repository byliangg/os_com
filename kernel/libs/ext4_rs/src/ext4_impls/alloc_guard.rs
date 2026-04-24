use spin::Mutex;

use crate::ext4_defs::Ext4Fsblk;
use crate::prelude::*;

static OP_ALLOCATED_BLOCKS: Mutex<BTreeSet<Ext4Fsblk>> = Mutex::new(BTreeSet::new());

pub fn clear_operation_allocated_blocks() {
    OP_ALLOCATED_BLOCKS.lock().clear();
}

pub fn reserve_operation_allocated_block(block: Ext4Fsblk) {
    OP_ALLOCATED_BLOCKS.lock().insert(block);
}

pub fn reserve_operation_allocated_blocks(blocks: &[Ext4Fsblk]) {
    if blocks.is_empty() {
        return;
    }
    let mut guard = OP_ALLOCATED_BLOCKS.lock();
    for block in blocks {
        guard.insert(*block);
    }
}

pub fn is_operation_allocated_block(block: Ext4Fsblk) -> bool {
    OP_ALLOCATED_BLOCKS.lock().contains(&block)
}
