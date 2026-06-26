// SPDX-License-Identifier: MPL-2.0
//! Phase 8 move-only split: the page-cache backend relocated verbatim from `fs.rs` —
//! `Ext4PageCacheState` and `Ext4PageCacheBackend` plus its `PageCacheBackend` impl. The
//! `impl Ext4Fs` page-cache method group stays in `fs.rs` for now (later increment). No behavior change.

use aster_block::bio::BioWaiter;

use ostd::mm::{PAGE_SIZE, VmReader, VmWriter, io_util::HasVmReaderWriter};

use super::fs::Ext4Fs;
use crate::fs::utils::{CachePage, PageCache, PageCacheBackend};
use crate::prelude::*;
use crate::vm::vmo::Vmo;

pub(super) struct Ext4PageCacheState {
    page_cache: PageCache,
    _backend: Arc<Ext4PageCacheBackend>,
}

impl Ext4PageCacheState {
    pub(super) fn new(fs: Weak<Ext4Fs>, ino: u32, capacity: usize) -> Result<Self> {
        let backend = Arc::new(Ext4PageCacheBackend { fs, ino });
        let page_cache = PageCache::with_capacity(capacity, Arc::downgrade(&backend) as _)?;
        Ok(Self {
            page_cache,
            _backend: backend,
        })
    }

    pub(super) fn pages(&self) -> Arc<Vmo> {
        self.page_cache.pages().clone()
    }

    pub(super) fn cached_size(&self) -> usize {
        self.page_cache.pages().size()
    }

    pub(super) fn resize(&self, new_size: usize) -> Result<()> {
        self.page_cache.resize(new_size)
    }

    pub(super) fn evict_range(&self, start: usize, len: usize) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        let end = start.saturating_add(len);
        self.page_cache.evict_range(start..end)?;
        self.decommit_vmo_range(start, end)
    }

    pub(super) fn evict_all(&self, file_size: usize) -> Result<()> {
        self.page_cache.evict_range(0..file_size)?;
        self.decommit_vmo_range(0, file_size)
    }

    /// Writes back every dirty page but keeps the pages resident as clean
    /// (no `decommit`). This is the `fsync`/`sync` safe point: Linux `fsync`
    /// flushes dirty pages yet never drops them, so the working set stays warm
    /// and later reads or sub-page writes hit the cache instead of refilling the
    /// whole file 4KB at a time from the device. Page frames are still released
    /// by `evict_all` (dropping inode state), `discard_all` (truncate) and the
    /// O_DIRECT path (which evicts+discards its own range before touching the
    /// device), so buffered/direct coherency is preserved without decommitting
    /// here.
    pub(super) fn flush_all(&self, file_size: usize) -> Result<()> {
        self.page_cache.evict_range(0..file_size)
    }

    pub(super) fn discard_range(&self, start: usize, len: usize) {
        if len == 0 {
            return;
        }
        let end = start.saturating_add(len);
        self.page_cache.discard_range(start..end);
        let _ = self.decommit_vmo_range(start, end);
    }

    pub(super) fn discard_all(&self) {
        let size = self.page_cache.pages().size();
        self.page_cache.discard_range(0..size);
        let _ = self.decommit_vmo_range(0, size);
    }

    fn decommit_vmo_range(&self, start: usize, end: usize) -> Result<()> {
        let size = self.page_cache.pages().size();
        if start >= size {
            return Ok(());
        }
        self.page_cache.pages().decommit(start..end.min(size))
    }
}

struct Ext4PageCacheBackend {
    fs: Weak<Ext4Fs>,
    ino: u32,
}

impl Ext4PageCacheBackend {
    fn fs(&self) -> Result<Arc<Ext4Fs>> {
        self.fs
            .upgrade()
            .ok_or_else(|| Error::with_message(Errno::EIO, "ext4 fs is dropped"))
    }

    fn page_offset(idx: usize) -> Result<usize> {
        idx.checked_mul(PAGE_SIZE)
            .ok_or_else(|| Error::with_message(Errno::EFBIG, "page cache offset overflow"))
    }
}

impl PageCacheBackend for Ext4PageCacheBackend {
    fn read_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter> {
        let fs = self.fs()?;
        let offset = Self::page_offset(idx)?;
        let mut data = vec![0u8; PAGE_SIZE];
        let file_size = fs.stat(self.ino)?.size as usize;

        if offset < file_size {
            let read_len = PAGE_SIZE.min(file_size - offset);
            fs.read_page_cache_data_at(self.ino, offset, &mut data[..read_len])?;
        }

        frame.writer().write(&mut VmReader::from(data.as_slice()));
        Ok(BioWaiter::new())
    }

    fn write_page_async(&self, idx: usize, frame: &CachePage) -> Result<BioWaiter> {
        let fs = self.fs()?;
        let offset = Self::page_offset(idx)?;
        let file_size = fs.stat(self.ino)?.size as usize;
        // BUG-8 / C4 (writeback size-clamp invariant, made explicit): a dirty page whose start is at
        // or past the on-disk file size is wholly beyond EOF — it can only arise from a truncate that
        // shrank the file below this page. Dropping it is the correct ext4 behavior (those bytes no
        // longer exist on disk), NOT silent data loss. The load-bearing invariant is "a dirty page's
        // VALID content never extends past the current on-disk inode size" — the slow append path
        // updates i_size (prepare) BEFORE dirtying the page, so writeback never persists past-EOF
        // bytes. Any future delalloc change that dirties before updating size MUST re-establish this
        // invariant (or it will corrupt — technical_report C4); the clamp below is its enforcement.
        if offset >= file_size {
            return Ok(BioWaiter::new());
        }

        let write_len = PAGE_SIZE.min(file_size - offset);
        let mut data = vec![0u8; write_len];
        frame
            .reader()
            .read_fallible(&mut VmWriter::from(data.as_mut_slice()).to_fallible())
            .map_err(|(err, _)| Error::from(err))?;
        fs.write_page_cache_data_at(self.ino, offset, data.as_slice())?;
        Ok(BioWaiter::new())
    }

    /// S4 (Phase 6): write a contiguous run of dirty pages in one batch.
    ///
    /// Gathers the run's page contents into a single buffer and writes it with
    /// one `write_page_cache_data_at` call, so the whole run is mapped once,
    /// journaled under one handle and written as coalesced bios (`write_at`
    /// merges contiguous physical blocks) -- instead of one mapping + one JBD2
    /// handle + one 4KB bio per page. Preserves the same size clamp as
    /// `write_page_async` (C4 invariant: never write past the on-disk size).
    fn write_pages_async(&self, start_idx: usize, frames: &[&CachePage]) -> Result<BioWaiter> {
        if frames.is_empty() {
            return Ok(BioWaiter::new());
        }
        let fs = self.fs()?;
        let offset = Self::page_offset(start_idx)?;
        let file_size = fs.stat(self.ino)?.size as usize;
        if offset >= file_size {
            return Ok(BioWaiter::new());
        }
        let run_bytes = frames
            .len()
            .checked_mul(PAGE_SIZE)
            .ok_or_else(|| Error::with_message(Errno::EFBIG, "page cache run overflow"))?;
        let write_len = run_bytes.min(file_size - offset);
        if write_len == 0 {
            return Ok(BioWaiter::new());
        }
        let mut data = vec![0u8; write_len];
        let mut copied = 0;
        for &frame in frames {
            if copied >= write_len {
                break;
            }
            let chunk = PAGE_SIZE.min(write_len - copied);
            frame
                .reader()
                .read_fallible(&mut VmWriter::from(&mut data[copied..copied + chunk]).to_fallible())
                .map_err(|(err, _)| Error::from(err))?;
            copied += chunk;
        }
        fs.write_page_cache_data_at(self.ino, offset, data.as_slice())?;
        Ok(BioWaiter::new())
    }

    fn npages(&self) -> usize {
        let Ok(fs) = self.fs() else {
            return 0;
        };
        fs.stat(self.ino)
            .map(|meta| (meta.size as usize).div_ceil(PAGE_SIZE))
            .unwrap_or(0)
    }
}
