// SPDX-License-Identifier: MPL-2.0

//! `FileSystem` trait implementation for `Ext4`.

use aster_block::{BLOCK_SIZE, bio::BioStatus};

use crate::{
    fs::{
        fs_impls::ext4::{Ext4, super_block::MAGIC_NUM},
        utils::NAME_MAX,
        vfs::{
            file_system::{FileSystem, FsEventSubscriberStats, SuperBlock},
            inode::Inode,
        },
    },
    prelude::*,
};

impl FileSystem for Ext4 {
    fn name(&self) -> &'static str {
        "ext4"
    }

    fn sync(&self) -> Result<()> {
        // After EXT4_IOC_SHUTDOWN, sync(2) succeeds as a no-op (Linux
        // `ext4_sync_fs` parity): the device is frozen, there is nothing left
        // to promise.
        if self.is_shutdown() {
            return Ok(());
        }
        // Flush every cached inode together with the block-side metadata, then
        // issue a single device barrier. Unmount drives durability through this
        // hook (`Path::unmount` -> `Mount::sync` -> `FileSystem::sync`), so a
        // clean unmount flushes every dirty inode and the bitmap consistently.
        self.sync_all()?;
        // sync(2) is a durability point: the inode writebacks above captured
        // into the running transaction, and returning before that transaction
        // reaches the log would silently drop the metadata on a crash — the
        // sync-then-cut-power baseline every crash harness builds on. No fs
        // lock is held here, as `commit_and_wait_running` requires. (The
        // free-count direct writes inside `sync_metadata` remain the known P7
        // WAL-inversion debt; this closes only the "sync does not wait" half.)
        if let Some(journal) = self.journal() {
            journal.commit_and_wait_running()?;
        }
        if self.block_device().sync()? != BioStatus::Complete {
            return_errno_with_message!(Errno::EIO, "failed to flush block device");
        }
        Ok(())
    }

    fn root_inode(&self) -> Arc<dyn Inode> {
        self.root_inode().unwrap()
    }

    fn sb(&self) -> SuperBlock {
        let sb = self.super_block();
        // `bavail` excludes the root-reserved blocks (`s_r_blocks_count`), like
        // Linux `ext4_statfs`, so unprivileged `df` sees the space it can use.
        let bavail = sb
            .free_blocks_count()
            .saturating_sub(sb.reserved_blocks_count() as u64);
        SuperBlock {
            magic: MAGIC_NUM as u64,
            bsize: BLOCK_SIZE,
            blocks: sb.total_blocks() as usize,
            bfree: sb.free_blocks_count() as usize,
            bavail: bavail as usize,
            files: sb.total_inodes() as usize,
            ffree: sb.free_inodes_count() as usize,
            // The volume UUID's low 64 bits, so mounts are distinguishable
            // (Linux folds the UUID into `f_fsid` too).
            fsid: u64::from_le_bytes(sb.uuid()[..8].try_into().unwrap()),
            namelen: NAME_MAX,
            frsize: BLOCK_SIZE,
            flags: 0,
            container_dev_id: self.container_device_id(),
        }
    }

    fn fs_event_subscriber_stats(&self) -> &FsEventSubscriberStats {
        self.fs_event_subscriber_stats()
    }
}
