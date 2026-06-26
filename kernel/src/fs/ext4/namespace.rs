// SPDX-License-Identifier: MPL-2.0
//! Phase 8 move-only split: the directory / namespace path relocated verbatim from `fs.rs` —
//! the dir-entry cache (load/insert/remove/invalidate), lookup, create/mkdir/unlink/rmdir/rename,
//! open-handle tracking + orphan reclaim, and readdir. No behavior change.

use super::caches::{DirEntryCacheEntry, DirLookupCacheResult};
use super::fs::Ext4Fs;
use super::run::JournaledOp;
use super::types::{EXT4_ROOT_INODE, SimpleDirEntry, mode};
use crate::prelude::*;

impl Ext4Fs {

    fn lookup_cache(&self, parent: u32, name: &str) -> DirLookupCacheResult {
        let caches = self.dir_entry_cache.lock();
        let Some(cache) = caches.get(&parent) else {
            return DirLookupCacheResult::Unknown;
        };
        if let Some(entry) = cache.entries.get(name) {
            return DirLookupCacheResult::Hit(entry.ino, entry.offset, entry.de_type);
        }
        if cache.loaded {
            return DirLookupCacheResult::Miss;
        }
        DirLookupCacheResult::Unknown
    }

    fn load_dir_cache_if_needed_locked(&self, parent: u32) -> Result<()> {
        {
            let caches = self.dir_entry_cache.lock();
            if let Some(cache) = caches.get(&parent) {
                if cache.loaded {
                    return Ok(());
                }
            }
        }

        // Phase 6 Task 5a: seed the dir-entry cache via **core**, byte-identically to the retired
        // ext4_rs `ext4_stat` + `ext4_readdir_with_offsets` pair. ext4_rs `ext4_readdir_with_offsets`
        // returns an empty vec for a non-directory inode (`!is_dir`) — the old code gated on
        // `ext4_stat().file_type != S_IFDIR` and raised ENOTDIR; we replicate that guard with the
        // core inode's `is_dir()`. The captured offset is each entry's **own** start byte offset
        // (`iblock*bs + off`), which feeds O(1) rmdir via `dir_remove_entry_at_offset`; core's
        // `dir_get_entries_with_start_offset` computes the identical offset (it differs from the
        // readdir `next_offset` cookie — see that function's PARITY note). DTO mapping mirrors the
        // Task 1 `readdir_locked` cutover exactly:
        //   - name    = `String::from_utf8_lossy(name)` (== ext4_rs `get_name()`)
        //   - ino     = entry inode (unused/inode==0 slots skipped by core, matching ext4_rs)
        //   - offset  = entry-start abs byte offset (== ext4_rs `entry_offset`)
        //   - de_type = dirent file-type byte (core `OwnedDirEntry.file_type` == ext4_rs `get_de_type()`)
        let (is_dir, entries_with_offsets) = self.run_io_dir_read_only_noerr(|ctx| {
            let Ok(dir) = super::core::inode::load_inode(ctx.reader, ctx.sb, parent) else {
                return (false, Vec::new());
            };
            if !dir.is_dir() {
                return (false, Vec::new());
            }
            let entries = super::core::dir::dir_get_entries_with_start_offset(ctx, &dir)
                .into_iter()
                .map(|(entry, start_offset)| {
                    (
                        String::from_utf8_lossy(&entry.name).into_owned(),
                        entry.inode,
                        start_offset as u64,
                        entry.file_type,
                    )
                })
                .collect::<Vec<_>>();
            (true, entries)
        })?;
        if !is_dir {
            return_errno_with_message!(Errno::ENOTDIR, "parent inode is not a directory");
        }

        let mut entry_map = BTreeMap::new();
        for (name, ino, entry_offset, de_type) in entries_with_offsets {
            entry_map.insert(
                name,
                DirEntryCacheEntry {
                    ino,
                    offset: entry_offset,
                    de_type,
                },
            );
        }

        let mut caches = self.dir_entry_cache.lock();
        let cache = caches.entry(parent).or_default();
        if !cache.loaded {
            cache.entries = entry_map;
            cache.loaded = true;
        }
        Ok(())
    }

    /// Insert a cache entry with a known byte offset in the parent directory stream.
    fn cache_insert_entry_with_offset(
        &self,
        parent: u32,
        name: &str,
        child: u32,
        offset: u64,
        de_type: u8,
    ) {
        let mut caches = self.dir_entry_cache.lock();
        let cache = caches.entry(parent).or_default();
        cache.entries.insert(
            name.to_string(),
            DirEntryCacheEntry {
                ino: child,
                offset,
                de_type,
            },
        );
    }

    /// Insert a cache entry when the byte offset is unknown (fallback paths).
    fn cache_insert_entry(&self, parent: u32, name: &str, child: u32, de_type: u8) {
        self.cache_insert_entry_with_offset(parent, name, child, u64::MAX, de_type);
    }

    fn cache_remove_entry(&self, parent: u32, name: &str) {
        let mut caches = self.dir_entry_cache.lock();
        if let Some(cache) = caches.get_mut(&parent) {
            cache.entries.remove(name);
        }
    }

    fn cache_remove_dir(&self, ino: u32) {
        let mut caches = self.dir_entry_cache.lock();
        caches.remove(&ino);
    }

    fn clear_inode_touch_cache(&self, ino: u32) {
        self.drop_page_cache_state(ino);
        self.invalidate_direct_read_cache(ino);
        self.coverage_invalidate(ino);
        self.inode_atime_cache.lock().remove(&ino);
        self.inode_ctime_cache.lock().remove(&ino);
        self.inode_mtime_ctime_cache.lock().remove(&ino);
    }

    fn dirent_type_from_inode_mode(mode: u16) -> u8 {
        // NOTE: the `mode` parameter shadows the imported `mode` module here, so reference the
        // mode-bit constants through their full path `super::types::mode::*`.
        use super::types::mode as ext4_mode;
        let file_type = mode & 0xF000;
        if file_type == ext4_mode::S_IFREG {
            1
        } else if file_type == ext4_mode::S_IFDIR {
            2
        } else if file_type == ext4_mode::S_IFCHR {
            3
        } else if file_type == ext4_mode::S_IFBLK {
            4
        } else if file_type == ext4_mode::S_IFIFO {
            5
        } else if file_type == ext4_mode::S_IFSOCK {
            6
        } else if file_type == ext4_mode::S_IFLNK {
            7
        } else {
            0
        }
    }

    fn loaded_dir_cache_entries(&self, ino: u32) -> Option<Vec<SimpleDirEntry>> {
        let caches = self.dir_entry_cache.lock();
        let cache = caches.get(&ino)?;
        if !cache.loaded {
            return None;
        }
        if cache.entries.values().any(|entry| entry.offset == u64::MAX) {
            return None;
        }

        let mut entries: Vec<_> = cache
            .entries
            .iter()
            .map(|(name, entry)| (entry.offset, name.clone(), *entry))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

        Some(
            entries
                .into_iter()
                .enumerate()
                .map(|(index, (_, name, entry))| SimpleDirEntry {
                    inode: entry.ino,
                    de_type: entry.de_type,
                    name,
                    // The VFS layer only requires a stable, monotonically
                    // increasing cookie for repeated readdir_at calls.
                    next_offset: index.saturating_add(1),
                })
                .collect(),
        )
    }

    fn lookup_at_locked(&self, parent: u32, name: &str) -> Result<u32> {
        self.lookup_at_locked_with_type(parent, name)
            .map(|(ino, _)| ino)
    }

    fn lookup_at_locked_with_type(&self, parent: u32, name: &str) -> Result<(u32, u8)> {
        match self.lookup_cache(parent, name) {
            DirLookupCacheResult::Hit(ino, _, de_type) => return Ok((ino, de_type)),
            DirLookupCacheResult::Miss => {
                return_errno_with_message!(Errno::ENOENT, "entry not found in directory cache");
            }
            DirLookupCacheResult::Unknown => {}
        }

        if self.load_dir_cache_if_needed_locked(parent).is_ok() {
            match self.lookup_cache(parent, name) {
                DirLookupCacheResult::Hit(ino, _, de_type) => return Ok((ino, de_type)),
                DirLookupCacheResult::Miss => {
                    return_errno_with_message!(Errno::ENOENT, "entry not found in directory cache");
                }
                DirLookupCacheResult::Unknown => {}
            }
        }

        // Phase 6 Task 1: single-level directory lookup via core. `core::dir::lookup_at` is a
        // byte-parity reimplementation of ext4_rs `ext4_lookup_at` (cross-block linear scan,
        // ENOENT on miss).
        let ino = self.run_io_dir_read_only(|ctx| {
            let parent_inode = super::core::inode::load_inode(ctx.reader, ctx.sb, parent)?;
            super::core::dir::lookup_at(ctx, &parent_inode, name.as_bytes())
        })?;
        self.cache_insert_entry(parent, name, ino, 0);
        Ok((ino, 0))
    }

    pub(super) fn lookup_at(&self, parent: u32, name: &str) -> Result<u32> {
        let parent_lock = Self::correctness_lock_for(&self.dir_correctness_locks, parent);
        let _parent_guard = parent_lock.write();
        self.lookup_at_locked(parent, name)
    }

    pub(super) fn dir_open(&self, path: &str) -> Result<u32> {
        // Phase 6 Task 1: resolve a path to its inode via core, replicating ext4_rs
        // `ext4_dir_open` == `generic_open(path, root, create=false)`. `generic_open` walks the
        // path from the root inode component-by-component (skipping empty components from
        // consecutive '/'), failing the whole lookup with ENOENT on the first missing component
        // (create=false). We reproduce that exactly with a per-component `core::dir::lookup_at`,
        // re-loading the descended-into directory inode each step. `dir_open` is only called from
        // inode.rs's `..` branch with a well-formed relative parent path (no leading/trailing
        // slash, empty path handled before the call).
        self.run_io_read_only(|ctx| {
            let mut current = EXT4_ROOT_INODE;
            for component in path.split('/') {
                if component.is_empty() {
                    continue;
                }
                let dir = super::core::inode::load_inode(ctx.reader, ctx.sb, current)?;
                current = super::core::dir::lookup_at(ctx, &dir, component.as_bytes())?;
            }
            Ok(current)
        })
    }

    pub(super) fn create_at(&self, parent: u32, name: &str, mode: u16) -> Result<u32> {
        // Drain any inode frees deferred by an atomic-context close (BUG-19). Done before taking any
        // lock so the drain acquires only its own per-ino correctness lock.
        self.reclaim_pending_inode_frees();
        let parent_lock = Self::correctness_lock_for(&self.dir_correctness_locks, parent);
        let _parent_guard = parent_lock.write();
        let op = JournaledOp::Create;
        let ino = self.run_journaled_namespace(Some(op), |nctx| {
            super::core::dir::create_at(nctx, parent, name.as_bytes(), mode)
        })?;
        self.note_inode_allocated(ino);
        let child_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
        let _child_guard = child_lock.write();
        // A freshly allocated inode must not inherit stale VMO/PageCache state
        // from an earlier lifetime with the same inode number. Discard instead
        // of evicting: writing old cached pages through the new inode mapping
        // would corrupt the new file.
        self.discard_page_cache_state(ino);
        self.invalidate_direct_read_cache(ino);
        self.cache_insert_entry(parent, name, ino, Self::dirent_type_from_inode_mode(mode));
        self.cache_remove_dir(ino);
        self.touch_birth_times(ino)?;
        self.touch_mtime_ctime(parent)?;
        Ok(ino)
    }

    pub(super) fn mkdir_at(&self, parent: u32, name: &str, mode: u16) -> Result<u32> {
        self.reclaim_pending_inode_frees();
        let parent_lock = Self::correctness_lock_for(&self.dir_correctness_locks, parent);
        let _parent_guard = parent_lock.write();
        // Ensure the parent directory cache is fully loaded so subsequent existence
        // checks (lookup_cache → Miss) can bypass the O(n) dir_find_entry disk scan.
        // The first call reads the directory once; subsequent calls return immediately.
        let cache_loaded = self.load_dir_cache_if_needed_locked(parent).is_ok();

        if cache_loaded {
            match self.lookup_cache(parent, name) {
                DirLookupCacheResult::Hit(_, _, _) => return_errno!(Errno::EEXIST),
                DirLookupCacheResult::Miss => {
                    // Cache is complete and confirms the name is absent — skip disk scan.
                    let op = JournaledOp::Mkdir;
                    let (ino, dir_byte_offset) = self.run_journaled_namespace(Some(op), |nctx| {
                        super::core::dir::mkdir_unchecked_at(nctx, parent, name.as_bytes(), mode)
                    })?;
                    self.note_inode_allocated(ino);
                    let child_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
                    let _child_guard = child_lock.write();
                    self.cache_insert_entry_with_offset(parent, name, ino, dir_byte_offset, 2);
                    self.cache_remove_dir(ino);
                    self.touch_birth_times(ino)?;
                    self.touch_mtime_ctime(parent)?;
                    return Ok(ino);
                }
                DirLookupCacheResult::Unknown => {}
            }
        }

        // Fallback: cache unavailable — use disk-based existence check.
        let op = JournaledOp::Mkdir;
        let ino = self.run_journaled_namespace(Some(op), |nctx| {
            super::core::dir::mkdir_at(nctx, parent, name.as_bytes(), mode)
        })?;
        self.note_inode_allocated(ino);
        let child_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
        let _child_guard = child_lock.write();
        self.cache_insert_entry(parent, name, ino, 2);
        self.cache_remove_dir(ino);
        self.touch_birth_times(ino)?;
        self.touch_mtime_ctime(parent)?;
        Ok(ino)
    }

    pub(super) fn unlink_at(&self, parent: u32, name: &str) -> Result<()> {
        self.reclaim_pending_inode_frees();
        let parent_lock = Self::correctness_lock_for(&self.dir_correctness_locks, parent);
        let _parent_guard = parent_lock.write();
        let target_ino = self.lookup_at_locked(parent, name)?;
        let target_meta = self.stat(target_ino)?;
        if target_meta.file_type == mode::S_IFDIR {
            return_errno!(Errno::EISDIR);
        }
        let target_lock = Self::correctness_lock_for(&self.inode_correctness_locks, target_ino);
        let _target_guard = target_lock.write();

        let op = JournaledOp::Unlink;
        self.run_journaled_namespace(Some(op), |nctx| {
            super::core::dir::unlink_at(nctx, parent, name.as_bytes())
        })?;
        self.cache_remove_entry(parent, name);
        self.clear_inode_touch_cache(target_ino);
        self.touch_mtime_ctime(parent)?;
        Ok(())
    }

    pub(super) fn on_open_file_handle(&self, ino: u32) {
        let mut open_file_handles = self.open_file_handles.lock();
        *open_file_handles.entry(ino).or_insert(0) += 1;
    }

    pub(super) fn on_close_file_handle(&self, ino: u32) -> Result<()> {
        let last_handle_closed = {
            let mut open_file_handles = self.open_file_handles.lock();
            let Some(count) = open_file_handles.get_mut(&ino) else {
                return Ok(());
            };
            *count = count.saturating_sub(1);
            if *count == 0 {
                open_file_handles.remove(&ino);
                true
            } else {
                false
            }
        };
        // BUG-19 (ext4-spec-correct + atomic-context safe): open-then-unlink-then-close path. When
        // the LAST open handle closes and the file was already unlinked (nlink==0), this close IS the
        // last reference, so the inode + its data blocks must be reclaimed (POSIX: a file held open
        // across unlink is freed only at close).
        //
        // BUT `on_close_file_handle` runs inside `InodeHandle::drop`, which can fire in an ATOMIC
        // context: `dup2`/`dup3` (`do_dup3`) drops the replaced fd's handle while holding the
        // file-table write lock (preempt disabled). The reclaim needs blocking journaled disk I/O
        // (read the inode, truncate blocks, free the bitmap bit) whose task switch would panic there.
        // So we DEFER: only record the ino in `pending_inode_free` here (a cheap, non-blocking insert
        // — uncontended `Mutex::lock` does not sleep, same as the `open_file_handles` insert above),
        // and let `reclaim_pending_inode_frees` perform the real free from the next SLEEPABLE fs op
        // (namespace ops + `sync`). The actual free still re-checks nlink==0 / no-open-handles under
        // the inode correctness lock and is idempotent via the `freed_inodes` guard.
        if last_handle_closed {
            self.pending_inode_free.lock().insert(ino);
        }
        Ok(())
    }

    fn has_open_file_handles(&self, ino: u32) -> bool {
        self.open_file_handles
            .lock()
            .get(&ino)
            .is_some_and(|count| *count > 0)
    }

    /// Drop the "already-freed" marker for `ino` (called right after a successful inode allocation
    /// that may hand out a previously-freed number). Keeps the `freed_inodes` double-free guard from
    /// blocking reclaim of the inode's *next* lifetime. Also drops any stale `pending_inode_free`
    /// entry for the reused number so a deferred free from the inode's PREVIOUS lifetime can never
    /// target the freshly allocated owner (the `nlink != 0` re-check in `cleanup_unlinked_file`
    /// already protects this; clearing here just avoids a wasted reclaim attempt).
    fn note_inode_allocated(&self, ino: u32) {
        self.freed_inodes.lock().remove(&ino);
        self.pending_inode_free.lock().remove(&ino);
    }

    /// BUG-19 fix (ext4-spec-correct): reclaim a regular file whose last reference is gone.
    ///
    /// Mirrors ext2's evict model (`InodeImpl::sync_metadata` frees the inode when
    /// `hard_links()==0`, guarded against double-free): once a regular file is unlinked (nlink==0)
    /// and has no remaining open handles, this releases ALL of its data blocks AND clears its inode
    /// bitmap bit, bumping `s_free_inodes_count`/`s_free_blocks_count` so repeated create/delete no
    /// longer leaks and e2fsck reports zero orphan inodes.
    ///
    /// Both the data-block free (`truncate_inode(0)`) and the inode-bitmap free run inside a SINGLE
    /// `run_journaled_namespace` transaction (`free_inode_on_evict_at`), so the reclaim is
    /// crash-atomic and BOTH free counts are sunk back into the `running_sb` single source of truth
    /// (the namespace chokepoint harvests `post.free_blocks_count()` + `post.free_inodes_count()`).
    /// Lock order unchanged: inode correctness lock → RUNTIME → jbd2 → inner.
    pub(super) fn cleanup_unlinked_file(&self, ino: u32) -> Result<()> {
        let inode_lock = Self::correctness_lock_for(&self.inode_correctness_locks, ino);
        let _inode_guard = inode_lock.write();

        let meta = self.stat(ino)?;
        if meta.nlink != 0 || meta.file_type != mode::S_IFREG {
            return Ok(());
        }
        if self.has_open_file_handles(ino) {
            return Ok(());
        }
        // Double-free guard: if a prior evict (last-ref drop vs last-handle close race) already
        // reclaimed this inode's bitmap bit, the on-disk inode table still reads nlink==0, so we
        // must not free the bitmap bit a second time. The marker is dropped when the number is
        // reallocated (`note_inode_allocated`).
        if self.freed_inodes.lock().contains(&ino) {
            return Ok(());
        }

        self.reset_page_cache_after_truncate(ino, 0)?;
        let op = JournaledOp::Unlink;
        self.run_journaled_namespace(Some(op), |nctx| {
            super::core::dir::free_inode_on_evict_at(nctx, ino)
        })?;
        self.freed_inodes.lock().insert(ino);
        // Drop every cached scrap of the now-freed inode (page cache, direct-read, coverage, time
        // caches) so a later reallocation of this number starts clean.
        self.clear_inode_touch_cache(ino);
        Ok(())
    }

    /// Drain the `pending_inode_free` set, reclaiming each inode whose last open handle closed in an
    /// atomic context (see `on_close_file_handle`). MUST be called only from a SLEEPABLE context —
    /// the per-inode `cleanup_unlinked_file` does blocking journaled disk I/O. Invoked at the head of
    /// every sleepable namespace op (`create_at`/`unlink_at`/`rmdir_at`/`rename_at`/`mkdir_at`) and
    /// from `FileSystem::sync`, so the deferred frees are reclaimed promptly under continued fs
    /// activity — closing the steady-state create/delete leak (e2fsck sees zero orphans) without ever
    /// blocking in the close path.
    ///
    /// Re-entrancy: a pending ino that was already reclaimed (via `cleanup_unlinked`'s sleepable
    /// last-ref path, or a prior drain) is a no-op — `cleanup_unlinked_file` re-checks nlink/handles
    /// and the `freed_inodes` guard. A pending ino that was rescued (relinked, or reused after free)
    /// is likewise skipped by those checks. Errors from one inode do not abort the rest.
    pub(super) fn reclaim_pending_inode_frees(&self) {
        // Snapshot + clear under the lock so the (blocking) frees run without holding it.
        let pending: Vec<u32> = {
            let mut set = self.pending_inode_free.lock();
            if set.is_empty() {
                return;
            }
            core::mem::take(&mut *set).into_iter().collect()
        };
        for ino in pending {
            if let Err(err) = self.cleanup_unlinked_file(ino) {
                warn!(
                    "ext4: deferred reclaim of unlinked inode {} failed: {:?}",
                    ino, err
                );
            }
        }
    }

    pub(super) fn rmdir_at(&self, parent: u32, name: &str) -> Result<()> {
        self.reclaim_pending_inode_frees();
        let parent_lock = Self::correctness_lock_for(&self.dir_correctness_locks, parent);
        let _parent_guard = parent_lock.write();
        let child_ino = self.lookup_at_locked(parent, name)?;
        let child_meta = self.stat(child_ino)?;
        if child_meta.file_type != mode::S_IFDIR {
            return_errno!(Errno::ENOTDIR);
        }
        let child_lock = Self::correctness_lock_for(&self.inode_correctness_locks, child_ino);
        let _child_guard = child_lock.write();

        let entries = self.readdir_locked(child_ino)?;
        let has_real_child = entries
            .iter()
            .any(|entry| entry.name != "." && entry.name != "..");
        if has_real_child {
            return_errno!(Errno::ENOTEMPTY);
        }

        // Retrieve cached byte offset for O(1) parent-dir entry removal.
        let dir_byte_offset = match self.lookup_cache(parent, name) {
            DirLookupCacheResult::Hit(_, offset, _) => offset,
            _ => u64::MAX,
        };

        let op = JournaledOp::Rmdir;
        if dir_byte_offset != u64::MAX {
            self.run_journaled_namespace(Some(op), |nctx| {
                super::core::dir::rmdir_at_fast(nctx, parent, child_ino, dir_byte_offset)
            })?;
        } else {
            self.run_journaled_namespace(Some(op), |nctx| {
                super::core::dir::rmdir_at(nctx, parent, name.as_bytes())
            })?;
        }
        self.cache_remove_entry(parent, name);
        self.cache_remove_dir(child_ino);
        self.clear_inode_touch_cache(child_ino);
        self.touch_mtime_ctime(parent)?;
        Ok(())
    }

    pub(super) fn rename_at(
        &self,
        old_parent: u32,
        old_name: &str,
        new_parent: u32,
        new_name: &str,
    ) -> Result<()> {
        self.reclaim_pending_inode_frees();
        self.with_dir_locks(&[old_parent, new_parent], || {
            if old_parent == new_parent && old_name == new_name {
                return Ok(());
            }

            let (old_ino, old_de_type) = self.lookup_at_locked_with_type(old_parent, old_name)?;
            let overwritten_ino = self.lookup_at_locked(new_parent, new_name).ok();
            let overwritten_is_dir = overwritten_ino
                .and_then(|ino| self.stat(ino).ok().map(|meta| (ino, meta)))
                .map(|(ino, meta)| {
                    (
                        ino,
                        meta.file_type == mode::S_IFDIR,
                    )
                });

            let mut affected_inodes = Vec::new();
            affected_inodes.push(old_ino);
            if let Some(ino) = overwritten_ino {
                affected_inodes.push(ino);
            }

            // The moved inode's own entry cache (if it is a directory) caches a `..` pointing at
            // `old_parent`; a cross-directory move reparents that `..` to `new_parent` inside
            // `core::dir::rename_at`, so its cached children become stale. Invalidate below.
            let old_is_dir = old_de_type == 2; // DE filetype DIR
            let cross_dir = new_parent != old_parent;

            self.with_inode_locks(&affected_inodes, || {
                let op = JournaledOp::Rename;
                self.run_journaled_namespace(Some(op), |nctx| {
                    super::core::dir::rename_at(
                        nctx,
                        old_parent,
                        old_name.as_bytes(),
                        new_parent,
                        new_name.as_bytes(),
                    )
                })?;

                // Cache invalidation: drop the old entry, insert under the new parent.
                self.cache_remove_entry(old_parent, old_name);
                self.cache_insert_entry(new_parent, new_name, old_ino, old_de_type);

                // A moved directory had its `..` rewritten (cross-dir) and changed parents; drop its
                // own cached child set so a later readdir re-reads the reparented `..`.
                if old_is_dir && cross_dir {
                    self.cache_remove_dir(old_ino);
                }

                // An overwritten target: drop its cached children (if a dir) and its name entry, then
                // restore the now-correct mapping (new_name -> old_ino) and scrub its time/page caches.
                if let Some((ino, true)) = overwritten_is_dir {
                    self.cache_remove_dir(ino);
                }
                if let Some(ino) = overwritten_ino {
                    if ino != old_ino {
                        self.cache_remove_entry(new_parent, new_name);
                        self.cache_insert_entry(new_parent, new_name, old_ino, old_de_type);
                        self.clear_inode_touch_cache(ino);
                    }
                }

                self.touch_mtime_ctime(old_parent)?;
                if cross_dir {
                    self.touch_mtime_ctime(new_parent)?;
                }
                self.touch_ctime(old_ino)?;

                Ok(())
            })?;

            // Overwrite reclamation.
            //
            // (1) REGULAR FILE target: `core::dir::rename_at`'s `unlink_file_branch` dropped the
            //     victim's nlink to 0 but freed nothing — the rename path has no VFS
            //     `cleanup_unlinked` hook for the overwritten target, so `mv a b` (b a regular file)
            //     would leak b's inode + data blocks. Reclaim it here via the same evict path used by
            //     unlink. Must be done AFTER `with_inode_locks` releases (its non-reentrant write
            //     guard on the victim ino would deadlock with `cleanup_unlinked_file`'s own inode
            //     correctness lock). The call is self-guarded: it re-checks
            //     `nlink==0 && S_IFREG && !has_open_file_handles` and consults the `freed_inodes`
            //     double-free set, so an open-across-rename target is freed only at its last close
            //     and there is no double-free.
            //
            // (2) EMPTY DIRECTORY target (`mv dir1 dir2`, dir2 an empty dir): NOW handled inside
            //     `core::dir::rename_at` — its directory-overwrite branch calls `finalize_freed_inode`
            //     (clears the inode bitmap bit + bumps free_inodes + decrements used_dirs), closing
            //     the BUG-19-deferred empty-dir-overwrite leak transactionally. No post-lock cleanup
            //     needed for the dir case; we only mark it in `freed_inodes` so a later realloc of the
            //     number is not blocked, and scrub its caches (done above via `cache_remove_dir` +
            //     `clear_inode_touch_cache`).
            if let Some((ino, false)) = overwritten_is_dir
                && ino != old_ino
            {
                self.cleanup_unlinked_file(ino)?;
            }
            if let Some((ino, true)) = overwritten_is_dir
                && ino != old_ino
            {
                // The empty-dir target was already freed in-core; record it in the double-free guard
                // and scrub its remaining caches (page/coverage/time) so a reallocated number starts
                // clean. `note_inode_allocated` drops the marker when the number is handed out again.
                self.freed_inodes.lock().insert(ino);
                self.clear_inode_touch_cache(ino);
            }

            Ok(())
        })
    }

    fn readdir_locked(&self, ino: u32) -> Result<Vec<SimpleDirEntry>> {
        if let Some(entries) = self.loaded_dir_cache_entries(ino) {
            return Ok(entries);
        }

        if self.load_dir_cache_if_needed_locked(ino).is_ok() {
            if let Some(entries) = self.loaded_dir_cache_entries(ino) {
                return Ok(entries);
            }
        }

        // Phase 6 Task 1: enumerate directory entries via core, byte-identically to ext4_rs
        // `ext4_readdir`. ext4_rs returns an empty vec for a non-directory inode (`!is_dir`); core's
        // `dir_get_entries_with_next_offset` does not gate on type, so we replicate the guard here.
        // Each `SimpleDirEntry` mirrors the ext4_rs build one-for-one:
        //   - `inode`     = entry inode (unused/inode==0 slots are skipped by core, matching ext4_rs)
        //   - `de_type`   = dirent file-type byte (core `OwnedDirEntry.file_type` == ext4_rs
        //                   `get_de_type()`, the same union byte)
        //   - `name`      = `String::from_utf8_lossy(name)` (exactly ext4_rs `get_name()`)
        //   - `next_offset` = `iblock*bs + off + rec_len` (core computes the identical cookie)
        self.run_io_dir_read_only_noerr(|ctx| {
            let Ok(dir) = super::core::inode::load_inode(ctx.reader, ctx.sb, ino) else {
                return Vec::new();
            };
            if !dir.is_dir() {
                return Vec::new();
            }
            super::core::dir::dir_get_entries_with_next_offset(ctx, &dir)
                .into_iter()
                .map(|(entry, next_offset)| SimpleDirEntry {
                    inode: entry.inode,
                    de_type: entry.file_type,
                    name: String::from_utf8_lossy(&entry.name).into_owned(),
                    next_offset,
                })
                .collect()
        })
    }

    pub(super) fn readdir(&self, ino: u32) -> Result<Vec<SimpleDirEntry>> {
        let dir_lock = Self::correctness_lock_for(&self.dir_correctness_locks, ino);
        let _dir_guard = dir_lock.write();
        self.readdir_locked(ino)
    }
}
