// SPDX-License-Identifier: MPL-2.0
//! Phase 8 move-only split: inode metadata relocated verbatim from `fs.rs` — the `set_inode_*`
//! mutators, `mknod_at`, the atime/mtime/ctime/birth timestamp helpers, the core inode-meta
//! runner/decoder, and `stat`. No behavior change.

use core::sync::atomic::Ordering;

use super::fs::Ext4Fs;
use super::run::{EXT4_RS_RUNTIME_LOCK, JournaledOp};
use super::types::SimpleInodeMeta;
use crate::fs::path::PerMountFlags;
use crate::fs::utils::StatusFlags;
use crate::prelude::*;

impl Ext4Fs {

    pub(super) fn set_inode_times(
        &self,
        ino: u32,
        atime: Option<u32>,
        mtime: Option<u32>,
        ctime: Option<u32>,
    ) -> Result<()> {
        // PARITY (ext4_rs `ext4_set_inode_times`): set each provided field, `None` leaves unchanged.
        self.run_inode_metadata_core(ino, |inode| {
            if let Some(v) = atime {
                inode.raw.atime = v;
            }
            if let Some(v) = mtime {
                inode.raw.mtime = v;
            }
            if let Some(v) = ctime {
                inode.raw.ctime = v;
            }
        })
    }

    pub(super) fn set_inode_mode(&self, ino: u32, mode: u16) -> Result<()> {
        // PARITY (ext4_rs `ext4_set_inode_mode`): keep the type bits (0xF000), replace only the
        // permission bits (0x0FFF) — `(current & 0xF000) | (mode & 0x0FFF)`.
        self.run_inode_metadata_core(ino, |inode| {
            let next = (inode.raw.mode & 0xF000) | (mode & 0x0FFF);
            inode.raw.mode = next;
        })?;
        self.touch_ctime(ino)
    }

    pub(super) fn set_inode_uid(&self, ino: u32, uid: u32) -> Result<()> {
        let uid = u16::try_from(uid)
            .map_err(|_| Error::with_message(Errno::EINVAL, "uid exceeds ext4 uid width"))?;
        // PARITY (ext4_rs `ext4_set_inode_uid`): direct set of the low-16 uid field.
        self.run_inode_metadata_core(ino, |inode| {
            inode.raw.uid = uid;
        })?;
        self.touch_ctime(ino)
    }

    pub(super) fn set_inode_gid(&self, ino: u32, gid: u32) -> Result<()> {
        let gid = u16::try_from(gid)
            .map_err(|_| Error::with_message(Errno::EINVAL, "gid exceeds ext4 gid width"))?;
        // PARITY (ext4_rs `ext4_set_inode_gid`): direct set of the low-16 gid field.
        self.run_inode_metadata_core(ino, |inode| {
            inode.raw.gid = gid;
        })?;
        self.touch_ctime(ino)
    }

    pub(super) fn set_inode_rdev(&self, ino: u32, rdev: u64) -> Result<()> {
        let rdev = u32::try_from(rdev)
            .map_err(|_| Error::with_message(Errno::EINVAL, "rdev exceeds ext4 rdev width"))?;
        // PARITY (ext4_rs `ext4_set_inode_rdev` → `set_faddr`): device id is stored in the
        // (deprecated) i_faddr field, not i_block.
        self.run_inode_metadata_core(ino, |inode| {
            inode.raw.faddr = rdev;
        })?;
        self.touch_ctime(ino)
    }

    pub(super) fn mknod_at(
        &self,
        parent: u32,
        name: &str,
        mode: u16,
        rdev: Option<u64>,
    ) -> Result<u32> {
        let ino = self.create_at(parent, name, mode)?;
        if let Some(rdev) = rdev {
            self.with_inode_lock(ino, || self.set_inode_rdev(ino, rdev))?;
        }
        Ok(ino)
    }

    fn mount_flags(&self) -> PerMountFlags {
        PerMountFlags::from_bits_truncate(self.mount_flags_bits.load(Ordering::Relaxed))
    }

    fn should_consider_atime_update(&self, status_flags: StatusFlags) -> Option<PerMountFlags> {
        if status_flags.contains(StatusFlags::O_NOATIME) {
            return None;
        }

        let mount_flags = self.mount_flags();
        if mount_flags.contains(PerMountFlags::RDONLY)
            || mount_flags.contains(PerMountFlags::NOATIME)
        {
            return None;
        }

        Some(mount_flags)
    }

    pub(super) fn touch_atime(&self, ino: u32, status_flags: StatusFlags) -> Result<()> {
        let Some(mount_flags) = self.should_consider_atime_update(status_flags) else {
            return Ok(());
        };

        let now = Self::now_unix_seconds_u32();
        {
            if self.inode_atime_cache.lock().get(&ino).copied() == Some(now) {
                return Ok(());
            }
        }

        if !mount_flags.contains(PerMountFlags::STRICTATIME) {
            match self.stat(ino) {
                Ok(meta) if meta.atime > meta.mtime && meta.atime > meta.ctime => {
                    // Phase 5: cache the relatime "no atime update needed"
                    // decision for this second so subsequent reads skip the
                    // per-read `stat(ino)` (which re-reads the inode block,
                    // ~31us/read at small bs). A write removes this inode's
                    // atime-cache entry, so a later mtime/ctime bump re-stats.
                    self.inode_atime_cache.lock().insert(ino, now);
                    return Ok(());
                }
                Ok(_) => {}
                Err(err) => {
                    warn!(
                        "ext4: failed to stat inode {} for atime policy: {:?}",
                        ino, err
                    );
                    return Ok(());
                }
            }
        }

        {
            let mut cache = self.inode_atime_cache.lock();
            if cache.get(&ino).copied() == Some(now) {
                return Ok(());
            }
            cache.insert(ino, now);
        }

        if let Err(err) = self.set_inode_times(ino, Some(now), None, None) {
            self.inode_atime_cache.lock().remove(&ino);
            return Err(err);
        }
        Ok(())
    }

    pub(super) fn touch_mtime_ctime(&self, ino: u32) -> Result<()> {
        let now = Self::now_unix_seconds_u32();
        {
            let mut cache = self.inode_mtime_ctime_cache.lock();
            if cache.get(&ino).copied() == Some(now) {
                return Ok(());
            }
            cache.insert(ino, now);
        }
        if let Err(err) = self.set_inode_times(ino, None, Some(now), Some(now)) {
            self.inode_mtime_ctime_cache.lock().remove(&ino);
            return Err(err);
        }
        Ok(())
    }

    pub(super) fn touch_ctime(&self, ino: u32) -> Result<()> {
        let now = Self::now_unix_seconds_u32();
        {
            let mut cache = self.inode_ctime_cache.lock();
            if cache.get(&ino).copied() == Some(now) {
                return Ok(());
            }
            cache.insert(ino, now);
        }
        if let Err(err) = self.set_inode_times(ino, None, None, Some(now)) {
            self.inode_ctime_cache.lock().remove(&ino);
            return Err(err);
        }
        Ok(())
    }

    pub(super) fn touch_birth_times(&self, ino: u32) -> Result<()> {
        let now = Self::now_unix_seconds_u32();
        self.set_inode_times(ino, Some(now), Some(now), Some(now))
    }

    pub(super) fn touch_atime_after_direct_read(&self, ino: u32, status_flags: StatusFlags) -> Result<()> {
        let Some(_) = self.should_consider_atime_update(status_flags) else {
            return Ok(());
        };

        let now = Self::now_unix_seconds_u32();
        {
            let cache = self.inode_direct_read_cache.lock();
            if cache.get(&ino).map(|entry| entry.last_atime_sec) == Some(now) {
                return Ok(());
            }
        }

        self.touch_atime(ino, status_flags)?;

        if let Some(entry) = self.inode_direct_read_cache.lock().get_mut(&ino) {
            entry.last_atime_sec = now;
        }
        Ok(())
    }

    /// Phase 6 Task 3: setattr cutover. Drive an inode-field mutation through **core** instead of
    /// ext4_rs. `mutate` receives the loaded core [`Inode`]; the caller sets the fields it owns
    /// (mode/uid/gid/rdev/times) and this helper writes the inode back + invalidates the meta cache.
    ///
    /// PARITY with the retired `run_inode_metadata_update_with_op`: when a journal driver is present
    /// (`jbd2_runtime` installed at mount), the write goes through the journaled chokepoint
    /// ([`run_journaled_core`] — same lock order, JBD2 handle, `InodeMetadata` op, cache
    /// invalidation as the old `run_journaled_ext4` path). When journal-less, the inode is written
    /// straight to the device via [`CoreCommitMetadataWriter`] (mirroring ext4_rs's no-journal
    /// `write_back_inode` → `write_offset`), then the meta cache is invalidated with the same
    /// generation-bump + clear protocol.
    fn run_inode_metadata_core(
        &self,
        ino: u32,
        mutate: impl FnOnce(&mut super::core::inode::Inode),
    ) -> Result<()> {
        let journal_enabled = self.jbd2_runtime.read().as_ref().is_some();
        if journal_enabled {
            // Journaled: load → mutate → write_back inside the journaled-core chokepoint, which
            // records the inode block image into the active JBD2 transaction (the allocator is
            // unused by setattr but threaded for signature compatibility). The `InodeMetadata { ino }`
            // op drops just this inode from the meta cache (parity with the old path).
            let op = JournaledOp::InodeMetadata { ino };
            self.run_journaled_core(Some(op), ino, |ctx, _alloc, inode| {
                mutate(inode);
                super::core::inode::write_back_inode(ctx.writer, ctx.reader, ctx.sb, inode)
            })
        } else {
            // No-journal: write the inode straight to the device (CoreCommitMetadataWriter does the
            // block-number → device write, bypassing the overlay/defer logic, exactly like ext4_rs's
            // no-journal `write_back_inode`). Same IO-epoch + runtime-lock lifecycle as the old path.
            let io_epoch = self.prepare_ext4_io();
            let runtime_wait_start_ns = Self::monotonic_nanos();
            let runtime_guard = EXT4_RS_RUNTIME_LOCK.lock();
            self.record_ext4_rs_runtime_lock_wait(
                Self::monotonic_nanos().saturating_sub(runtime_wait_start_ns),
            );
            let runtime_hold_start_ns = Self::monotonic_nanos();
            let block_size = self.core_sb.block_size();
            let reader = super::core_adapter::CoreDeviceReader::new(self.journal_io.clone());
            let writer =
                super::core_adapter::CoreCommitMetadataWriter::new(self.adapter.clone(), block_size);
            let result = (|| -> Result<()> {
                let mut inode = super::core::inode::load_inode(&reader, &self.core_sb, ino)?;
                mutate(&mut inode);
                super::core::inode::write_back_inode(&writer, &reader, &self.core_sb, &mut inode)
            })();
            drop(runtime_guard);
            self.record_ext4_rs_runtime_lock_hold(
                Self::monotonic_nanos().saturating_sub(runtime_hold_start_ns),
            );
            let io_result = self.finish_ext4_io(io_epoch);
            // No-journal setattr bypasses run_journaled_*, so invalidate the inode meta cache here
            // too (same generation-bump + clear protocol as the retired path).
            self.meta_cache_generation.fetch_add(1, Ordering::Release);
            self.inode_meta_cache.lock().clear();
            match (result, io_result) {
                (Ok(()), Ok(())) => Ok(()),
                (Err(err), _) => Err(err),
                (Ok(()), Err(err)) => Err(err),
            }
        }
    }

    /// Build the integration `SimpleInodeMeta` DTO from a core `Inode`, byte-identically to ext4_rs
    /// `ext4_stat`.
    ///
    /// Field-by-field mapping (each verified against ext4_rs `Ext4Inode` accessors):
    /// `ino`=`inode.num`; `mode`=`raw.mode()`; `file_type`=`raw.file_type()` (`mode & 0xF000`, same
    /// as ext4_rs `file_type().bits()`); `uid`/`gid`/`atime`/`mtime`/`ctime`/`faddr` are the raw
    /// fields; `nlink`=`raw.links_count()`; `size`=`raw.size()` (`size|size_hi<<32`);
    /// `blocks`=`raw.blocks()` (`blocks|l_i_blocks_high<<32`, equal to ext4_rs `blocks_count()`);
    /// `rdev`=`raw.faddr()`; `flags`=`raw.flags()`.
    fn core_inode_meta(inode: &super::core::inode::Inode) -> SimpleInodeMeta {
        let raw = &inode.raw;
        SimpleInodeMeta {
            ino: inode.num,
            mode: raw.mode(),
            file_type: raw.file_type(),
            uid: raw.uid,
            gid: raw.gid,
            nlink: raw.links_count(),
            size: raw.size(),
            blocks: raw.blocks(),
            atime: raw.atime,
            mtime: raw.mtime,
            ctime: raw.ctime,
            rdev: raw.faddr,
            flags: raw.flags(),
        }
    }

    /// Set second-granularity atime/mtime/ctime on a loaded core inode and write it back.
    ///
    /// PARITY: ext4_rs `ext4_set_inode_times` (set the provided fields, `None` = leave unchanged,
    /// then `write_back_inode`). The write apply closures call this after the data/alloc write, just
    /// as the ext4_rs closures called `ext4_set_inode_times(ino, None, Some(now), Some(now))`.
    pub(super) fn core_set_inode_times(
        ctx: &super::core::extents::WriteCtx,
        inode: &mut super::core::inode::Inode,
        atime: Option<u32>,
        mtime: Option<u32>,
        ctime: Option<u32>,
    ) -> Result<()> {
        if let Some(v) = atime {
            inode.raw.atime = v;
        }
        if let Some(v) = mtime {
            inode.raw.mtime = v;
        }
        if let Some(v) = ctime {
            inode.raw.ctime = v;
        }
        super::core::inode::write_back_inode(ctx.writer, ctx.reader, ctx.sb, inode)
    }

    pub(super) fn stat(&self, ino: u32) -> Result<SimpleInodeMeta> {
        // Fast path: serve from the in-memory metadata cache.
        let gen_before = self.meta_cache_generation.load(Ordering::Acquire);
        if let Some(meta) = self.inode_meta_cache.lock().get(&ino).copied() {
            return Ok(meta);
        }

        // Miss: read the inode from the device once via core and build the same DTO ext4_rs
        // `ext4_stat` returned. `load_inode` validates the inode number (ext4_rs `get_inode_ref`
        // does not), so a structurally-invalid inode now surfaces an error instead of garbage
        // metadata; valid inodes (all the VFS layer ever passes) are byte-identical.
        let meta = self.run_io_read_only(|ctx| {
            let inode = super::core::inode::load_inode(ctx.reader, ctx.sb, ino)?;
            Ok(Self::core_inode_meta(&inode))
        })?;

        // Only cache if no journaled mutation raced our read (generation
        // unchanged), so we never insert a value read across a mutation.
        if self.meta_cache_generation.load(Ordering::Acquire) == gen_before {
            self.inode_meta_cache.lock().insert(ino, meta);
        }
        Ok(meta)
    }
}
