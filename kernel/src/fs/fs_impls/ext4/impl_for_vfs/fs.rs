// SPDX-License-Identifier: MPL-2.0

//! `FileSystem` trait implementation for `Ext4`.

use aster_block::{BLOCK_SIZE, bio::BioStatus};

use crate::{
    fs::{
        fs_impls::ext4::{Ext4, super_block::MAGIC_NUM},
        utils::NAME_MAX,
        vfs::{
            file_system::{FileSystem, FsEventSubscriberStats, FsFlags, SuperBlock},
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
        // lock is held here, as `commit_and_wait_running` requires. The free
        // counts ride along automatically: every alloc/free journaled the sb
        // and owning-group-descriptor count words into this same running
        // transaction (`SuperBlock::journal_capture` + the descriptor
        // `patch_into` funnel), so committing it here makes them durable AND
        // WAL-consistent with the bitmaps. The direct count RMW in
        // `sync_metadata` is reached only at the unmount quiesce point (journal
        // stopped, log flushed empty); with the journal live it is a no-op, so
        // no count word is ever written around the log.
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
        // `f_blocks` reports usable capacity, not the raw device size: the total
        // block count minus the filesystem's metadata overhead — superblock/GDT
        // copies, bitmaps, inode tables (`SuperBlock::metadata_overhead`), and the
        // journal — matching Linux `ext4_statfs` (`ext4_blocks_count -
        // s_overhead`). Linux caches `s_overhead_clusters` but recomputes it on
        // every non-`bigalloc` mount (`super.c`: it zeroes the cached value unless
        // `bigalloc`); `bigalloc` is not in `RO_COMPAT_SUPP`, so every volume we
        // mount takes the recompute path, which is provably equal to the cached
        // value here.
        let journal_overhead = self
            .journal()
            .map_or(0, |journal| journal.total_log_blocks());
        let overhead = sb.metadata_overhead().saturating_add(journal_overhead);
        let usable_blocks = sb.total_blocks().saturating_sub(overhead);
        // `bavail` excludes the root-reserved blocks (`s_r_blocks_count`), like
        // Linux `ext4_statfs`, so unprivileged `df` sees the space it can use.
        let bavail = sb
            .free_blocks_count()
            .saturating_sub(u64::from(sb.reserved_blocks_count()));
        SuperBlock {
            magic: MAGIC_NUM as u64,
            bsize: BLOCK_SIZE,
            blocks: usize::try_from(usable_blocks).unwrap(),
            bfree: usize::try_from(sb.free_blocks_count()).unwrap(),
            bavail: usize::try_from(bavail).unwrap(),
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

    fn set_fs_flags(&self, _flags: FsFlags, _data: Option<CString>, _ctx: &Context) -> Result<()> {
        // Refuse loudly instead of inheriting the VFS default, which logs a
        // warning and returns `Ok(())` — a fake success that would let
        // `mount -o remount,ro` report a read-only volume while it stayed
        // writable. Ext4 here honors no runtime filesystem-flag change (there is
        // no read-only mount mode yet), so any change is `EOPNOTSUPP`.
        self.refuse_fs_flags_change()
    }

    fn fs_event_subscriber_stats(&self) -> &FsEventSubscriberStats {
        self.fs_event_subscriber_stats()
    }
}
