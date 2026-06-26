// SPDX-License-Identifier: MPL-2.0

// Phase 8 move-only split: `impl FileSystem for Ext4Fs` moved here verbatim from `fs.rs`.
use core::sync::atomic::Ordering;

use crate::{
    fs::utils::{FileSystem, FsEventSubscriberStats, Inode, NAME_MAX, SuperBlock},
    prelude::*,
};

use super::super::fs::{EXT4_MAGIC, Ext4Fs};
use super::super::types::EXT4_ROOT_INODE;

impl FileSystem for Ext4Fs {
    fn name(&self) -> &'static str {
        "ext4"
    }

    fn sync(&self) -> Result<()> {
        // Step 4b: after `EXT4_IOC_SHUTDOWN`, sync() is a no-op.  Especially
        // important for NOLOGFLUSH (hard crash simulation) — we must NOT
        // sneak in commits on subsequent unmount/syncfs.  For LOGFLUSH /
        // DEFAULT, the sync was already done as part of the ioctl.
        if self.is_shutdown() {
            return Ok(());
        }
        // BUG-19: reclaim inodes whose last handle closed in an atomic context (deferred by
        // `on_close_file_handle`) before flushing, so their frees are journaled + committed by this
        // sync. `sync()` is sleepable, so the blocking reclaim is safe here.
        self.reclaim_pending_inode_frees();
        self.sync_all_page_caches()?;
        self.flush_pending_jbd2_transactions();
        self.block_device.sync()?;
        self.flush_pending_jbd2_transactions();
        // Phase 5: emit one complete latency-attribution snapshot at the end of
        // a benchmark run (syncfs / unmount). No-op unless ext4fs.phase2_profile=1.
        self.dump_perf_summary();
        Ok(())
    }

    fn root_inode(&self) -> Arc<dyn Inode> {
        self.this().make_inode(EXT4_ROOT_INODE, String::new())
    }

    fn sb(&self) -> SuperBlock {
        // Phase 6 Task 4: read the FROZEN mount-time snapshot `core_sb` (byte-frozen behavior — the
        // old `inner.super_block` was likewise the frozen mount snapshot, so statfs reports mount-time
        // free counts, NOT the live `running_sb` counts). All fields here are immutable geometry or
        // the frozen free counts; identical to what ext4_rs `inner.super_block` returned.
        let ext4_sb = &self.core_sb;
        let block_size = ext4_sb.block_size();
        let blocks = ext4_sb.blocks_count() as usize;
        let bfree = ext4_sb.free_blocks_count().min(usize::MAX as u64) as usize;
        let files = ext4_sb.inodes_count() as usize;
        let ffree = ext4_sb.free_inodes_count() as usize;
        let uuid = ext4_sb.uuid();
        let fsid = u64::from_le_bytes(uuid[..8].try_into().unwrap_or([0u8; 8]));

        SuperBlock {
            magic: EXT4_MAGIC,
            bsize: block_size,
            blocks,
            bfree,
            bavail: bfree,
            files,
            ffree,
            fsid,
            namelen: NAME_MAX,
            frsize: block_size,
            flags: 0,
        }
    }

    fn fs_event_subscriber_stats(&self) -> &FsEventSubscriberStats {
        &self.fs_event_subscriber_stats
    }

    fn set_mount_flags(&self, mount_flags_bits: u32) {
        self.mount_flags_bits
            .store(mount_flags_bits, Ordering::Relaxed);
    }
}
