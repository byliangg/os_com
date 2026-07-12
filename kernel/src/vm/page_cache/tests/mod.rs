// SPDX-License-Identifier: MPL-2.0

//! Regression tests for the page-cache subsystem in the
//! concurrency scenarios.

use alloc::vec;

use ostd::{mm::VmIo, prelude::ktest};

use self::utils::{IoCompletion, IoKind, MockPageCacheBackend, wait_until};
use super::{PageCache, PageCacheBackend, VmoCommitError};
use crate::{prelude::*, thread::kernel_thread::ThreadOptions};

mod utils;

/// Creates a page cache with `num_pages` backend pages for test scenarios.
fn new_backend_page_cache(backend: &Arc<MockPageCacheBackend>, num_pages: usize) -> PageCache {
    let backend_dyn: Arc<dyn PageCacheBackend> = backend.clone();
    PageCache::new_with_backend(num_pages * PAGE_SIZE, Arc::downgrade(&backend_dyn)).unwrap()
}

/// Serializes a cold read and a later overwrite with the caller-provided
/// buffered-I/O lock required by the page-cache synchronization model.
#[ktest]
fn concurrent_read_and_write() {
    let backend = MockPageCacheBackend::new(1);
    backend.set_completion(IoKind::Read, IoCompletion::Deferred);

    let old_pattern = vec![0x3c; PAGE_SIZE];
    let new_pattern = vec![0xa5; PAGE_SIZE];
    backend.set_persisted_page_bytes(0, &old_pattern);

    let page_cache = new_backend_page_cache(&backend, 1);
    let io_lock = Arc::new(Mutex::new(()));
    let observed_read_result = Arc::new(Mutex::new(None::<Vec<u8>>));
    let writer_started = Arc::new(Mutex::new(false));
    let writer_finished = Arc::new(Mutex::new(false));

    // Start a cold-page read and hold backend completion so the read keeps the
    // caller-provided buffered-I/O lock while the writer attempts to enter.
    let read_thread = {
        let page_cache = page_cache.clone();
        let io_lock = io_lock.clone();
        let observed_read_result = observed_read_result.clone();
        ThreadOptions::new(move || {
            let _io_guard = io_lock.lock();
            let mut read_buffer = vec![0; PAGE_SIZE];
            page_cache.read_bytes(0, &mut read_buffer).unwrap();
            *observed_read_result.lock() = Some(read_buffer);
        })
        .spawn()
    };

    backend.wait_for_deferred_bios(IoKind::Read, 1);
    assert_eq!(backend.read_count(0), 1);

    // Issue a full-page overwrite against the same range. The page-cache layer
    // relies on this higher-level lock for buffered read/write ordering, so the
    // writer must not enter `write_bytes()` until the read-side critical section
    // has completed.
    let write_thread = {
        let page_cache = page_cache.clone();
        let io_lock = io_lock.clone();
        let writer_started = writer_started.clone();
        let writer_finished = writer_finished.clone();
        let new_pattern = new_pattern.clone();
        ThreadOptions::new(move || {
            *writer_started.lock() = true;
            let _io_guard = io_lock.lock();
            page_cache.write_bytes(0, &new_pattern).unwrap();
            *writer_finished.lock() = true;
        })
        .spawn()
    };

    wait_until(|| *writer_started.lock());
    assert!(!*writer_finished.lock());

    // Finish the backend read. The serialized reader observes the persisted old
    // bytes, and the later writer's overwrite becomes visible afterwards.
    assert!(backend.complete_next_deferred_bio(IoKind::Read, true));
    read_thread.join();
    write_thread.join();

    assert_eq!(&*observed_read_result.lock(), &Some(old_pattern));
    assert!(*writer_finished.lock());
    assert_eq!(backend.read_count(0), 1);

    let mut read_buffer = vec![0; PAGE_SIZE];
    page_cache.read_bytes(0, &mut read_buffer).unwrap();
    assert_eq!(read_buffer, new_pattern);
}

/// Flushes a dirty page while another task re-dirties it, so writeback reaches
/// the backend and the newest dirty bytes are not silently lost.
#[ktest]
fn concurrent_write_and_flush() {
    let backend = MockPageCacheBackend::new(1);
    backend.set_completion(IoKind::Write, IoCompletion::Deferred);

    let page_cache = new_backend_page_cache(&backend, 1);
    let first_dirty_pattern = vec![0x11; PAGE_SIZE];
    let latest_dirty_pattern = vec![0x22; PAGE_SIZE];
    page_cache.write_bytes(0, &first_dirty_pattern).unwrap();

    let flush_result = Arc::new(Mutex::new(None::<Result<()>>));
    let writer_finished = Arc::new(Mutex::new(false));

    // Start writeback and pin it in the deferred state so a concurrent writer
    // can dirty the page again before the first flush completes.
    let flush_thread = {
        let page_cache = page_cache.clone();
        let flush_result = flush_result.clone();
        ThreadOptions::new(move || {
            *flush_result.lock() = Some(page_cache.flush_range(0..PAGE_SIZE));
        })
        .spawn()
    };

    backend.wait_for_deferred_bios(IoKind::Write, 1);

    // Re-dirty the same page while the first writeback is in flight. A later
    // flush must persist this newest version instead of silently dropping it.
    let writer_thread = {
        let page_cache = page_cache.clone();
        let writer_finished = writer_finished.clone();
        let latest_dirty_pattern = latest_dirty_pattern.clone();
        ThreadOptions::new(move || {
            page_cache.write_bytes(0, &latest_dirty_pattern).unwrap();
            *writer_finished.lock() = true;
        })
        .spawn()
    };

    writer_thread.join();
    assert!(*writer_finished.lock());

    // Complete the first writeback, then flush again and confirm the backend
    // eventually stores the latest dirty bytes.
    assert!(backend.complete_next_deferred_bio(IoKind::Write, true));
    flush_thread.join();
    assert!(flush_result.lock().take().unwrap().is_ok());

    backend.set_completion(IoKind::Write, IoCompletion::Immediate);
    page_cache.flush_range(0..PAGE_SIZE).unwrap();

    let mut read_buffer = vec![0; PAGE_SIZE];
    page_cache.read_bytes(0, &mut read_buffer).unwrap();
    assert_eq!(read_buffer, latest_dirty_pattern);
    assert_eq!(backend.write_count(0), 2);
    assert_eq!(backend.persisted_page_bytes(0), latest_dirty_pattern);
}

/// Re-dirties a page while another task runs `flush_range()` and
/// `evict_range()`, ensuring the newest dirty page is kept cached.
#[ktest]
fn concurrent_write_and_evict() {
    let backend = MockPageCacheBackend::new(1);
    backend.set_completion(IoKind::Write, IoCompletion::Deferred);

    let page_cache = new_backend_page_cache(&backend, 1);
    let first_dirty_pattern = vec![0x52; PAGE_SIZE];
    let latest_dirty_pattern = vec![0x7d; PAGE_SIZE];
    page_cache.write_bytes(0, &first_dirty_pattern).unwrap();

    // Race a flush+evict sequence against a new writer after writeback has
    // already started. This checks that a page re-dirtied before eviction
    // stays cached instead of being silently dropped.
    let flush_and_evict_result = Arc::new(Mutex::new(None::<Result<()>>));
    let flush_and_evict_thread = {
        let page_cache = page_cache.clone();
        let flush_and_evict_result = flush_and_evict_result.clone();
        ThreadOptions::new(move || {
            let result = page_cache
                .flush_range(0..PAGE_SIZE)
                .and_then(|()| page_cache.evict_range(0..PAGE_SIZE));
            *flush_and_evict_result.lock() = Some(result);
        })
        .spawn()
    };

    backend.wait_for_deferred_bios(IoKind::Write, 1);

    // Dirty the page again before the first writeback completes. Eviction
    // should leave this newest dirty page resident in cache.
    let writer_thread = {
        let page_cache = page_cache.clone();
        let latest_dirty_pattern = latest_dirty_pattern.clone();
        ThreadOptions::new(move || {
            page_cache.write_bytes(0, &latest_dirty_pattern).unwrap();
        })
        .spawn()
    };

    writer_thread.join();

    // After the first writeback finishes, the flush+evict path should return,
    // but the re-dirtied page must still be readable from cache.
    assert!(backend.complete_next_deferred_bio(IoKind::Write, true));
    flush_and_evict_thread.join();
    assert!(flush_and_evict_result.lock().take().unwrap().is_ok());

    let mut read_buffer = vec![0; PAGE_SIZE];
    page_cache.read_bytes(0, &mut read_buffer).unwrap();
    assert_eq!(read_buffer, latest_dirty_pattern);
    assert_eq!(backend.read_count(0), 0);

    backend.set_completion(IoKind::Write, IoCompletion::Immediate);
    page_cache.flush_range(0..PAGE_SIZE).unwrap();
    assert_eq!(backend.write_count(0), 2);
    assert_eq!(backend.persisted_page_bytes(0), latest_dirty_pattern);
}

/// Commits a page while a concurrent truncate shrinks the page cache, ensuring
/// pages beyond the new size stay inaccessible.
#[ktest]
fn concurrent_commit_and_truncate() {
    let backend = MockPageCacheBackend::new(2);
    backend.set_completion(IoKind::Read, IoCompletion::Deferred);
    backend.set_persisted_page_bytes(1, &[0x9b; PAGE_SIZE]);

    let page_cache = new_backend_page_cache(&backend, 2);
    let commit_second_page_result = Arc::new(Mutex::new(None::<Result<()>>));
    let resize_result = Arc::new(Mutex::new(None::<Result<()>>));
    let resize_started = Arc::new(Mutex::new(false));
    let resize_finished = Arc::new(Mutex::new(false));

    // Commit page 1 and pause its backend read so truncate can shrink the VMO
    // while that commit is still waiting for initialization to finish.
    let commit_thread = {
        let vmo = page_cache.as_vmo().clone();
        let commit_second_page_result = commit_second_page_result.clone();
        ThreadOptions::new(move || {
            *commit_second_page_result.lock() = Some(vmo.commit_on(1).map(|_| ()));
        })
        .spawn()
    };

    backend.wait_for_deferred_bios(IoKind::Read, 1);

    let resize_thread = {
        let page_cache = page_cache.clone();
        let resize_result = resize_result.clone();
        let resize_started = resize_started.clone();
        let resize_finished = resize_finished.clone();
        ThreadOptions::new(move || {
            *resize_started.lock() = true;
            *resize_result.lock() = Some(page_cache.resize(PAGE_SIZE, 2 * PAGE_SIZE));
            *resize_finished.lock() = true;
        })
        .spawn()
    };

    wait_until(|| *resize_started.lock());
    assert!(!*resize_finished.lock());

    // Let the blocked commit finish, then verify the truncated page is no
    // longer accessible through subsequent VMO operations.
    assert!(backend.complete_next_deferred_bio(IoKind::Read, true));
    commit_thread.join();
    resize_thread.join();

    assert!(commit_second_page_result.lock().take().unwrap().is_ok());
    assert!(resize_result.lock().take().unwrap().is_ok());
    assert_eq!(
        page_cache.as_vmo().commit_on(1).unwrap_err().error(),
        Errno::EINVAL
    );

    let mut read_buffer = vec![0; PAGE_SIZE];
    page_cache.read_bytes(PAGE_SIZE, &mut read_buffer).unwrap();
    assert_eq!(read_buffer, vec![0; PAGE_SIZE]);
}

/// Faults the same cold page through `try_commit_page()` and `commit_on()`,
/// verifying that one backend read initializes the page for all waiters.
#[ktest]
fn concurrent_page_faults() {
    let backend = MockPageCacheBackend::new(1);
    backend.set_completion(IoKind::Read, IoCompletion::Deferred);
    backend.set_persisted_page_bytes(0, &[0x6b; PAGE_SIZE]);

    let page_cache = new_backend_page_cache(&backend, 1);
    let vmo = page_cache.as_vmo();
    // The first page-fault style probe should report that backend I/O is
    // needed because the page has not been committed yet.
    assert!(matches!(
        vmo.try_commit_page(0),
        Err(VmoCommitError::NeedIo { index: 0 })
    ));

    let first_commit_finished = Arc::new(Mutex::new(false));
    let second_commit_finished = Arc::new(Mutex::new(false));

    // Start one blocking commit to issue the backend read. Other faulting
    // callers for the same page should observe the in-progress initialization.
    let first_thread = {
        let vmo = vmo.clone();
        let first_commit_finished = first_commit_finished.clone();
        ThreadOptions::new(move || {
            vmo.commit_on(0).unwrap();
            *first_commit_finished.lock() = true;
        })
        .spawn()
    };

    backend.wait_for_deferred_bios(IoKind::Read, 1);
    match vmo.try_commit_page(0) {
        Err(VmoCommitError::WaitUntilInit { index: 0, .. }) => {}
        other => panic!("unexpected page-fault state: {other:?}"),
    }

    // A second blocking commit should join the same initialization instead of
    // submitting a duplicate read BIO for the same page.
    let second_thread = {
        let vmo = vmo.clone();
        let second_commit_finished = second_commit_finished.clone();
        ThreadOptions::new(move || {
            vmo.commit_on(0).unwrap();
            *second_commit_finished.lock() = true;
        })
        .spawn()
    };

    assert_eq!(backend.read_count(0), 1);
    assert!(!*first_commit_finished.lock());
    assert!(!*second_commit_finished.lock());

    // Release the single deferred read and check that both waiters complete
    // and observe the initialized page contents.
    assert!(backend.complete_next_deferred_bio(IoKind::Read, true));
    first_thread.join();
    second_thread.join();

    assert!(*first_commit_finished.lock());
    assert!(*second_commit_finished.lock());
    assert_eq!(backend.read_count(0), 1);

    let mut read_buffer = vec![0; PAGE_SIZE];
    let mut writer = VmWriter::from(read_buffer.as_mut_slice()).to_fallible();
    vmo.read(0, &mut writer).unwrap();
    assert_eq!(read_buffer, vec![0x6b; PAGE_SIZE]);
}

/// Keeps backend reads failing for one page and checks both the initial
/// committer and a later waiter return `EIO` instead of livelocking.
#[ktest]
fn persistent_backend_errors() {
    let backend = MockPageCacheBackend::new(1);
    backend.set_completion(IoKind::Read, IoCompletion::Deferred);

    let page_cache = new_backend_page_cache(&backend, 1);
    let vmo = page_cache.as_vmo();
    let first_error = Arc::new(Mutex::new(None::<Errno>));
    let second_error = Arc::new(Mutex::new(None::<Errno>));

    // Start one commit that blocks on the backend read. A second commit for
    // the same page then goes through `ensure_init()` and should also return
    // promptly once initialization fails.
    let first_thread = {
        let vmo = vmo.clone();
        let first_error = first_error.clone();
        ThreadOptions::new(move || {
            *first_error.lock() = Some(vmo.commit_on(0).unwrap_err().error());
        })
        .spawn()
    };

    backend.wait_for_deferred_bios(IoKind::Read, 1);

    let second_thread = {
        let vmo = vmo.clone();
        let second_error = second_error.clone();
        ThreadOptions::new(move || {
            *second_error.lock() = Some(vmo.commit_on(0).unwrap_err().error());
        })
        .spawn()
    };

    // Fail the first backend read, then fail the retry as well. This checks
    // that both callers get `EIO` back rather than spinning forever.
    assert!(backend.complete_next_deferred_bio(IoKind::Read, false));
    backend.wait_for_deferred_bios(IoKind::Read, 1);
    assert!(backend.complete_next_deferred_bio(IoKind::Read, false));

    first_thread.join();
    second_thread.join();

    assert_eq!(&*first_error.lock(), &Some(Errno::EIO));
    assert_eq!(&*second_error.lock(), &Some(Errno::EIO));
    assert_eq!(backend.read_count(0), 2);
}

/// Delays BIO completion on demand to exercise shared read I/O, writeback
/// hand-off, and waiting for `is_writing_back` to clear.
#[ktest]
fn delayed_io_completion() {
    let backend = MockPageCacheBackend::new(1);
    backend.set_completion(IoKind::Read, IoCompletion::Deferred);
    backend.set_completion(IoKind::Write, IoCompletion::Deferred);

    let persisted_pattern = vec![0x5a; PAGE_SIZE];
    let first_dirty_pattern = vec![0x11; PAGE_SIZE];
    let latest_dirty_pattern = vec![0x33; PAGE_SIZE];
    backend.set_persisted_page_bytes(0, &persisted_pattern);

    let page_cache = new_backend_page_cache(&backend, 1);
    let first_read_result = Arc::new(Mutex::new(None::<Vec<u8>>));
    let second_read_result = Arc::new(Mutex::new(None::<Vec<u8>>));

    // Start two cold-page readers while read completion is delayed. They
    // should share one backend read and both see the initialized page.
    let first_reader = {
        let page_cache = page_cache.clone();
        let first_read_result = first_read_result.clone();
        ThreadOptions::new(move || {
            let mut read_buffer = vec![0; PAGE_SIZE];
            page_cache.read_bytes(0, &mut read_buffer).unwrap();
            *first_read_result.lock() = Some(read_buffer);
        })
        .spawn()
    };
    let second_reader = {
        let page_cache = page_cache.clone();
        let second_read_result = second_read_result.clone();
        ThreadOptions::new(move || {
            let mut read_buffer = vec![0; PAGE_SIZE];
            page_cache.read_bytes(0, &mut read_buffer).unwrap();
            *second_read_result.lock() = Some(read_buffer);
        })
        .spawn()
    };

    backend.wait_for_deferred_bios(IoKind::Read, 1);
    assert_eq!(backend.read_count(0), 1);
    assert!(backend.complete_next_deferred_bio(IoKind::Read, true));

    first_reader.join();
    second_reader.join();

    assert_eq!(&*first_read_result.lock(), &Some(persisted_pattern.clone()));
    assert_eq!(&*second_read_result.lock(), &Some(persisted_pattern));

    // Start one deferred writeback, then dirty the page again before it
    // completes so a second flush must wait for `is_writing_back` to clear.
    page_cache.write_bytes(0, &first_dirty_pattern).unwrap();

    let first_flush_result = Arc::new(Mutex::new(None::<Result<()>>));
    let second_flush_result = Arc::new(Mutex::new(None::<Result<()>>));
    let second_flush_started = Arc::new(Mutex::new(false));
    let second_flush_finished = Arc::new(Mutex::new(false));

    let first_flush_thread = {
        let page_cache = page_cache.clone();
        let first_flush_result = first_flush_result.clone();
        ThreadOptions::new(move || {
            *first_flush_result.lock() = Some(page_cache.flush_range(0..PAGE_SIZE));
        })
        .spawn()
    };

    backend.wait_for_deferred_bios(IoKind::Write, 1);

    page_cache.write_bytes(0, &latest_dirty_pattern).unwrap();

    let second_flush_thread = {
        let page_cache = page_cache.clone();
        let second_flush_result = second_flush_result.clone();
        let second_flush_started = second_flush_started.clone();
        let second_flush_finished = second_flush_finished.clone();
        ThreadOptions::new(move || {
            *second_flush_started.lock() = true;
            *second_flush_result.lock() = Some(page_cache.flush_range(0..PAGE_SIZE));
            *second_flush_finished.lock() = true;
        })
        .spawn()
    };

    wait_until(|| *second_flush_started.lock());
    assert_eq!(backend.write_count(0), 1);
    assert!(!*second_flush_finished.lock());

    // Complete the first and second writebacks one by one. This exercises the
    // ownership hand-off in async writeback and the wait-for-writeback path.
    assert!(backend.complete_next_deferred_bio(IoKind::Write, true));
    backend.wait_for_deferred_bios(IoKind::Write, 1);
    assert_eq!(backend.write_count(0), 2);
    assert!(!*second_flush_finished.lock());

    assert!(backend.complete_next_deferred_bio(IoKind::Write, true));
    first_flush_thread.join();
    second_flush_thread.join();

    assert!(first_flush_result.lock().take().unwrap().is_ok());
    assert!(second_flush_result.lock().take().unwrap().is_ok());
    assert_eq!(backend.persisted_page_bytes(0), latest_dirty_pattern);
}

/// Prefetching a range reads every absent page once, leaves the pages clean
/// (`UpToDate`, not `Dirty`) so a following flush writes nothing back, and
/// makes a later read a cache hit.
#[ktest]
fn prefetch_fills_clean_pages() {
    let backend = MockPageCacheBackend::new(4);
    let patterns = [
        vec![0xa1; PAGE_SIZE],
        vec![0xb2; PAGE_SIZE],
        vec![0xc3; PAGE_SIZE],
        vec![0xd4; PAGE_SIZE],
    ];
    for (idx, pattern) in patterns.iter().enumerate() {
        backend.set_persisted_page_bytes(idx, pattern);
    }
    let page_cache = new_backend_page_cache(&backend, 4);

    // Prefetch the whole file; default (immediate) completion fills the pages.
    page_cache.prefetch_range(0..4 * PAGE_SIZE).unwrap();
    for idx in 0..4 {
        assert_eq!(backend.read_count(idx), 1);
    }

    // The prefetched pages are clean: a flush over the same range writes
    // nothing back to the backend.
    page_cache.flush_range(0..4 * PAGE_SIZE).unwrap();
    for idx in 0..4 {
        assert_eq!(backend.write_count(idx), 0);
    }

    // Contents match the backend, and reading does not trigger another read.
    for (idx, pattern) in patterns.iter().enumerate() {
        let mut read_buffer = vec![0; PAGE_SIZE];
        page_cache
            .read_bytes(idx * PAGE_SIZE, &mut read_buffer)
            .unwrap();
        assert_eq!(&read_buffer, pattern);
        assert_eq!(backend.read_count(idx), 1);
    }
}

/// Prefetching a range whose pages are already cached issues no backend reads.
#[ktest]
fn prefetch_skips_cached_pages() {
    let backend = MockPageCacheBackend::new(2);
    backend.set_persisted_page_bytes(0, &[0x11; PAGE_SIZE]);
    backend.set_persisted_page_bytes(1, &[0x22; PAGE_SIZE]);
    let page_cache = new_backend_page_cache(&backend, 2);

    // Warm both pages, then prefetch the same range: no additional reads.
    let mut read_buffer = vec![0; 2 * PAGE_SIZE];
    page_cache.read_bytes(0, &mut read_buffer).unwrap();
    assert_eq!(backend.read_count(0), 1);
    assert_eq!(backend.read_count(1), 1);

    page_cache.prefetch_range(0..2 * PAGE_SIZE).unwrap();
    assert_eq!(backend.read_count(0), 1);
    assert_eq!(backend.read_count(1), 1);
}

/// Prefetching a range that runs past the page-cache size clamps to the valid
/// region: no panic, and only in-bounds pages are populated.
#[ktest]
fn prefetch_clamps_out_of_bounds_range() {
    let backend = MockPageCacheBackend::new(2);
    backend.set_persisted_page_bytes(0, &[0x33; PAGE_SIZE]);
    backend.set_persisted_page_bytes(1, &[0x44; PAGE_SIZE]);
    // The page cache holds two pages; the prefetch range asks for four.
    let page_cache = new_backend_page_cache(&backend, 2);

    page_cache.prefetch_range(0..4 * PAGE_SIZE).unwrap();
    assert_eq!(backend.read_count(0), 1);
    assert_eq!(backend.read_count(1), 1);

    let mut read_buffer = vec![0; 2 * PAGE_SIZE];
    page_cache.read_bytes(0, &mut read_buffer).unwrap();
    assert_eq!(&read_buffer[..PAGE_SIZE], &[0x33; PAGE_SIZE]);
    assert_eq!(&read_buffer[PAGE_SIZE..], &[0x44; PAGE_SIZE]);
}

/// A prefetch whose backend read fails to submit swallows the error and leaves
/// the page uninitialized, so a later synchronous read re-reads it through the
/// normal commit path and succeeds once the backend recovers.
#[ktest]
fn prefetch_failure_leaves_page_for_retry() {
    let backend = MockPageCacheBackend::new(1);
    backend.set_persisted_page_bytes(0, &[0x55; PAGE_SIZE]);
    // Fail the prefetch's read submission for page 0.
    backend.set_read_submit_failure(0, true);
    let page_cache = new_backend_page_cache(&backend, 1);

    // Prefetch swallows the submission error and returns `Ok`; page 0 is left
    // uninitialized.
    page_cache.prefetch_range(0..PAGE_SIZE).unwrap();

    // Recover the backend; a synchronous read re-reads the page and succeeds.
    backend.set_read_submit_failure(0, false);
    let mut read_buffer = vec![0; PAGE_SIZE];
    page_cache.read_bytes(0, &mut read_buffer).unwrap();
    assert_eq!(read_buffer, vec![0x55; PAGE_SIZE]);
}

/// Batched writeback persists every dirty page byte-for-byte, exactly as the
/// per-page `flush_range` would. The mock backend caps a BIO at one segment, so
/// this exercises the default per-page fan-out of the batched path (the
/// multi-segment run merge is device-specific and covered end-to-end in ext4).
#[ktest]
fn flush_batched_persists_all_dirty_pages() {
    const NPAGES: usize = 5;
    let backend = MockPageCacheBackend::new(NPAGES);
    let page_cache = new_backend_page_cache(&backend, NPAGES);

    let mut written = vec![0u8; NPAGES * PAGE_SIZE];
    for p in 0..NPAGES {
        written[p * PAGE_SIZE..(p + 1) * PAGE_SIZE].fill(0x10 + p as u8);
    }
    page_cache.write_bytes(0, &written).unwrap();

    page_cache
        .flush_range_batched(0..NPAGES * PAGE_SIZE)
        .unwrap();

    // Every page is persisted once, byte-for-byte.
    for p in 0..NPAGES {
        assert_eq!(backend.write_count(p), 1);
        assert_eq!(
            backend.persisted_page_bytes(p),
            written[p * PAGE_SIZE..(p + 1) * PAGE_SIZE].to_vec()
        );
    }

    // The pages are now clean: a second batched flush writes nothing back.
    page_cache
        .flush_range_batched(0..NPAGES * PAGE_SIZE)
        .unwrap();
    for p in 0..NPAGES {
        assert_eq!(backend.write_count(p), 1);
    }
}

/// A clean page between dirty ones is a gap in the collected batch: the batched
/// flush skips it (never writing it) and persists the surrounding dirty pages.
#[ktest]
fn flush_batched_skips_clean_gap() {
    const NPAGES: usize = 5;
    let backend = MockPageCacheBackend::new(NPAGES);
    let page_cache = new_backend_page_cache(&backend, NPAGES);

    // Dirty pages 0, 1, 3, 4 with full-page writes; page 2 is never touched, so
    // it is absent from the cache — a gap the collection skips.
    let pattern = |p: usize| vec![0x20 + p as u8; PAGE_SIZE];
    page_cache.write_bytes(0, &pattern(0)).unwrap();
    page_cache.write_bytes(PAGE_SIZE, &pattern(1)).unwrap();
    page_cache.write_bytes(3 * PAGE_SIZE, &pattern(3)).unwrap();
    page_cache.write_bytes(4 * PAGE_SIZE, &pattern(4)).unwrap();

    page_cache
        .flush_range_batched(0..NPAGES * PAGE_SIZE)
        .unwrap();

    // The gap page was never written; the others persisted correctly.
    assert_eq!(backend.write_count(2), 0);
    assert_eq!(backend.persisted_page_bytes(2), vec![0u8; PAGE_SIZE]);
    for p in [0, 1, 3, 4] {
        assert_eq!(backend.write_count(p), 1);
        assert_eq!(backend.persisted_page_bytes(p), pattern(p));
    }
}

/// A batched flush whose backend write submission fails propagates the error
/// (fsync must not lie), re-dirties the page it could not queue, stops there
/// (later pages stay dirty), and leaves the pages before it persisted — byte
/// for byte the per-page `flush_range` behavior. Recovering the backend and
/// re-flushing then persists everything.
#[ktest]
fn flush_batched_propagates_submit_error_and_redirties() {
    const NPAGES: usize = 3;
    let backend = MockPageCacheBackend::new(NPAGES);
    let page_cache = new_backend_page_cache(&backend, NPAGES);

    let pattern = |p: usize| vec![0x30 + p as u8; PAGE_SIZE];
    for p in 0..NPAGES {
        page_cache.write_bytes(p * PAGE_SIZE, &pattern(p)).unwrap();
    }

    // Fail the write submission for the middle page.
    backend.set_write_submit_failure(1, true);
    let result = page_cache.flush_range_batched(0..NPAGES * PAGE_SIZE);
    assert!(result.is_err());

    // Page 0 (before the failure) is persisted; page 1's data never reached the
    // backend; page 2 (after the failure) was never submitted.
    assert_eq!(backend.write_count(0), 1);
    assert_eq!(backend.persisted_page_bytes(0), pattern(0));
    assert_eq!(backend.persisted_page_bytes(1), vec![0u8; PAGE_SIZE]);
    assert_eq!(backend.write_count(2), 0);

    // Recover and re-flush: the re-dirtied page 1 and the untouched page 2 are
    // both collected and persisted; page 0 is clean, so it is not rewritten.
    backend.set_write_submit_failure(1, false);
    page_cache
        .flush_range_batched(0..NPAGES * PAGE_SIZE)
        .unwrap();
    assert_eq!(backend.write_count(0), 1);
    for p in 0..NPAGES {
        assert_eq!(backend.persisted_page_bytes(p), pattern(p));
    }
}

/// A batched flush over a range with no dirty pages is a no-op: it submits no
/// writes and returns `Ok`.
#[ktest]
fn flush_batched_all_clean_is_noop() {
    const NPAGES: usize = 4;
    let backend = MockPageCacheBackend::new(NPAGES);
    let page_cache = new_backend_page_cache(&backend, NPAGES);

    // Dirty then flush so every page is clean (UpToDate) going in.
    let all = vec![0x7e; NPAGES * PAGE_SIZE];
    page_cache.write_bytes(0, &all).unwrap();
    page_cache
        .flush_range_batched(0..NPAGES * PAGE_SIZE)
        .unwrap();
    for p in 0..NPAGES {
        assert_eq!(backend.write_count(p), 1);
    }

    // A second batched flush over the same clean range writes nothing.
    page_cache
        .flush_range_batched(0..NPAGES * PAGE_SIZE)
        .unwrap();
    for p in 0..NPAGES {
        assert_eq!(backend.write_count(p), 1);
    }
}
