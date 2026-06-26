// SPDX-License-Identifier: MPL-2.0
//! Phase 8 move-only split: the per-inode / per-dir correctness-lock helpers relocated verbatim
//! from `fs.rs` — `correctness_lock_for` and the `with_inode_lock(s)` / `with_dir_locks` ordered
//! multi-lock acquisition wrappers (C1 RwMutex correctness locks). No behavior change.

use super::fs::Ext4Fs;
use crate::prelude::*;

impl Ext4Fs {

    /// C1 (Phase 6): per-inode/dir correctness locks are RwMutex. Every
    /// metadata-mutating or page-cache-touching path takes `.write()`
    /// (semantics identical to the previous Mutex); the O_DIRECT
    /// overwrite-write and read paths under `page_cache=0` take `.read()` so
    /// concurrent dio on the same file is no longer serialized across the
    /// device wait (Linux ext4 shared-i_rwsem dio overwrite equivalent).
    pub(super) fn correctness_lock_for(
        table: &Mutex<BTreeMap<u32, Arc<RwMutex<()>>>>,
        ino: u32,
    ) -> Arc<RwMutex<()>> {
        table
            .lock()
            .entry(ino)
            .or_insert_with(|| Arc::new(RwMutex::new(())))
            .clone()
    }

    fn sorted_unique_inos(inos: &[u32]) -> Vec<u32> {
        let mut sorted = Vec::new();
        for &ino in inos {
            if !sorted.contains(&ino) {
                sorted.push(ino);
            }
        }
        sorted.sort_unstable();
        sorted
    }

    pub(super) fn with_inode_locks<T>(&self, inos: &[u32], f: impl FnOnce() -> Result<T>) -> Result<T> {
        let sorted = Self::sorted_unique_inos(inos);
        let locks: Vec<_> = sorted
            .iter()
            .map(|&ino| Self::correctness_lock_for(&self.inode_correctness_locks, ino))
            .collect();
        let mut guards = Vec::with_capacity(locks.len());
        for lock in &locks {
            guards.push(lock.write());
        }
        f()
    }

    pub(super) fn with_inode_lock<T>(&self, ino: u32, f: impl FnOnce() -> Result<T>) -> Result<T> {
        self.with_inode_locks(&[ino], f)
    }

    pub(super) fn with_dir_locks<T>(&self, inos: &[u32], f: impl FnOnce() -> Result<T>) -> Result<T> {
        let sorted = Self::sorted_unique_inos(inos);
        let locks: Vec<_> = sorted
            .iter()
            .map(|&ino| Self::correctness_lock_for(&self.dir_correctness_locks, ino))
            .collect();
        let mut guards = Vec::with_capacity(locks.len());
        for lock in &locks {
            guards.push(lock.write());
        }
        f()
    }

}
