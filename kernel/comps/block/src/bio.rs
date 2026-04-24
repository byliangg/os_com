// SPDX-License-Identifier: MPL-2.0

use core::sync::atomic::AtomicU64;

use aster_time::read_monotonic_time;
use align_ext::AlignExt;
use aster_util::mem_obj_slice::Slice;
use bitvec::array::BitArray;
use int_to_c_enum::TryFromInt;
use ostd::{
    Error,
    mm::{
        HasSize, Infallible, USegment, VmReader, VmWriter,
        dma::DmaStream,
        io_util::{HasVmReaderWriter, VmReaderWriterResult},
    },
    sync::{SpinLock, WaitQueue},
};
use spin::Once;

use super::{BlockDevice, id::Sid};
use crate::{BLOCK_SIZE, SECTOR_SIZE, prelude::*};

/// The unit for block I/O.
///
/// Each `Bio` packs the following information:
/// (1) The type of the I/O,
/// (2) The target sectors on the device for doing I/O,
/// (3) The memory locations (`BioSegment`) from/to which data are read/written,
/// (4) The optional callback function that will be invoked when the I/O is completed.
#[derive(Debug)]
pub struct Bio(Arc<BioInner>);

struct ReadBioProfileStats {
    read_bios: AtomicU64,
    read_bytes: AtomicU64,
    read_segments: AtomicU64,
    large_read_bios: AtomicU64,
    large_read_bytes: AtomicU64,
    large_read_segments: AtomicU64,
    queue_wait_ns: AtomicU64,
    dispatch_ns: AtomicU64,
    irq_delivery_ns: AtomicU64,
    irq_reap_ns: AtomicU64,
    resp_sync_ns: AtomicU64,
    device_wait_ns: AtomicU64,
    dma_sync_ns: AtomicU64,
    complete_ns: AtomicU64,
    large_queue_wait_ns: AtomicU64,
    large_dispatch_ns: AtomicU64,
    large_irq_delivery_ns: AtomicU64,
    large_irq_reap_ns: AtomicU64,
    large_resp_sync_ns: AtomicU64,
    large_device_wait_ns: AtomicU64,
    large_dma_sync_ns: AtomicU64,
    large_complete_ns: AtomicU64,
    max_total_ns: AtomicU64,
}

impl ReadBioProfileStats {
    const LOG_INTERVAL_BIOS: u64 = 8_192;
    const LARGE_READ_THRESHOLD_BYTES: u64 = 512 * 1024;

    const fn new() -> Self {
        Self {
            read_bios: AtomicU64::new(0),
            read_bytes: AtomicU64::new(0),
            read_segments: AtomicU64::new(0),
            large_read_bios: AtomicU64::new(0),
            large_read_bytes: AtomicU64::new(0),
            large_read_segments: AtomicU64::new(0),
            queue_wait_ns: AtomicU64::new(0),
            dispatch_ns: AtomicU64::new(0),
            irq_delivery_ns: AtomicU64::new(0),
            irq_reap_ns: AtomicU64::new(0),
            resp_sync_ns: AtomicU64::new(0),
            device_wait_ns: AtomicU64::new(0),
            dma_sync_ns: AtomicU64::new(0),
            complete_ns: AtomicU64::new(0),
            large_queue_wait_ns: AtomicU64::new(0),
            large_dispatch_ns: AtomicU64::new(0),
            large_irq_delivery_ns: AtomicU64::new(0),
            large_irq_reap_ns: AtomicU64::new(0),
            large_resp_sync_ns: AtomicU64::new(0),
            large_device_wait_ns: AtomicU64::new(0),
            large_dma_sync_ns: AtomicU64::new(0),
            large_complete_ns: AtomicU64::new(0),
            max_total_ns: AtomicU64::new(0),
        }
    }

    fn record_read_bio(
        &self,
        bytes: u64,
        segments: u64,
        queue_wait_ns: u64,
        dispatch_ns: u64,
        irq_delivery_ns: u64,
        irq_reap_ns: u64,
        resp_sync_ns: u64,
        device_wait_ns: u64,
        dma_sync_ns: u64,
        complete_ns: u64,
        total_ns: u64,
    ) -> u64 {
        let bios = self.read_bios.fetch_add(1, Ordering::Relaxed) + 1;
        self.read_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.read_segments.fetch_add(segments, Ordering::Relaxed);
        self.queue_wait_ns.fetch_add(queue_wait_ns, Ordering::Relaxed);
        self.dispatch_ns.fetch_add(dispatch_ns, Ordering::Relaxed);
        self.irq_delivery_ns
            .fetch_add(irq_delivery_ns, Ordering::Relaxed);
        self.irq_reap_ns.fetch_add(irq_reap_ns, Ordering::Relaxed);
        self.resp_sync_ns.fetch_add(resp_sync_ns, Ordering::Relaxed);
        self.device_wait_ns.fetch_add(device_wait_ns, Ordering::Relaxed);
        self.dma_sync_ns.fetch_add(dma_sync_ns, Ordering::Relaxed);
        self.complete_ns.fetch_add(complete_ns, Ordering::Relaxed);
        if bytes >= Self::LARGE_READ_THRESHOLD_BYTES {
            self.large_read_bios.fetch_add(1, Ordering::Relaxed);
            self.large_read_bytes.fetch_add(bytes, Ordering::Relaxed);
            self.large_read_segments.fetch_add(segments, Ordering::Relaxed);
            self.large_queue_wait_ns
                .fetch_add(queue_wait_ns, Ordering::Relaxed);
            self.large_dispatch_ns
                .fetch_add(dispatch_ns, Ordering::Relaxed);
            self.large_irq_delivery_ns
                .fetch_add(irq_delivery_ns, Ordering::Relaxed);
            self.large_irq_reap_ns
                .fetch_add(irq_reap_ns, Ordering::Relaxed);
            self.large_resp_sync_ns
                .fetch_add(resp_sync_ns, Ordering::Relaxed);
            self.large_device_wait_ns
                .fetch_add(device_wait_ns, Ordering::Relaxed);
            self.large_dma_sync_ns
                .fetch_add(dma_sync_ns, Ordering::Relaxed);
            self.large_complete_ns
                .fetch_add(complete_ns, Ordering::Relaxed);
        }
        self.update_max_total_ns(total_ns);
        bios
    }

    fn update_max_total_ns(&self, total_ns: u64) {
        let mut current = self.max_total_ns.load(Ordering::Relaxed);
        while total_ns > current {
            match self.max_total_ns.compare_exchange_weak(
                current,
                total_ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    fn reset(&self) {
        self.read_bios.store(0, Ordering::Relaxed);
        self.read_bytes.store(0, Ordering::Relaxed);
        self.read_segments.store(0, Ordering::Relaxed);
        self.large_read_bios.store(0, Ordering::Relaxed);
        self.large_read_bytes.store(0, Ordering::Relaxed);
        self.large_read_segments.store(0, Ordering::Relaxed);
        self.queue_wait_ns.store(0, Ordering::Relaxed);
        self.dispatch_ns.store(0, Ordering::Relaxed);
        self.irq_delivery_ns.store(0, Ordering::Relaxed);
        self.irq_reap_ns.store(0, Ordering::Relaxed);
        self.resp_sync_ns.store(0, Ordering::Relaxed);
        self.device_wait_ns.store(0, Ordering::Relaxed);
        self.dma_sync_ns.store(0, Ordering::Relaxed);
        self.complete_ns.store(0, Ordering::Relaxed);
        self.large_queue_wait_ns.store(0, Ordering::Relaxed);
        self.large_dispatch_ns.store(0, Ordering::Relaxed);
        self.large_irq_delivery_ns.store(0, Ordering::Relaxed);
        self.large_irq_reap_ns.store(0, Ordering::Relaxed);
        self.large_resp_sync_ns.store(0, Ordering::Relaxed);
        self.large_device_wait_ns.store(0, Ordering::Relaxed);
        self.large_dma_sync_ns.store(0, Ordering::Relaxed);
        self.large_complete_ns.store(0, Ordering::Relaxed);
        self.max_total_ns.store(0, Ordering::Relaxed);
    }
}

static READ_BIO_PROFILE_STATS: ReadBioProfileStats = ReadBioProfileStats::new();
const READ_BIO_PROFILE_LOG_ENABLED: bool = false;
const BIO_FLAG_PREFER_FAST_SUBMIT: u32 = 1 << 0;

pub fn reset_read_bio_profile() {
    READ_BIO_PROFILE_STATS.reset();
}

#[inline]
fn monotonic_nanos() -> u64 {
    let duration = read_monotonic_time();
    duration
        .as_secs()
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::from(duration.subsec_nanos()))
}

fn maybe_log_read_bio_profile(bios: u64) {
    if !READ_BIO_PROFILE_LOG_ENABLED {
        return;
    }
    if bios == 0 || bios % ReadBioProfileStats::LOG_INTERVAL_BIOS != 0 {
        return;
    }

    let bytes = READ_BIO_PROFILE_STATS.read_bytes.load(Ordering::Relaxed);
    let segments = READ_BIO_PROFILE_STATS.read_segments.load(Ordering::Relaxed);
    let queue_wait_ns = READ_BIO_PROFILE_STATS.queue_wait_ns.load(Ordering::Relaxed);
    let dispatch_ns = READ_BIO_PROFILE_STATS.dispatch_ns.load(Ordering::Relaxed);
    let irq_delivery_ns = READ_BIO_PROFILE_STATS.irq_delivery_ns.load(Ordering::Relaxed);
    let irq_reap_ns = READ_BIO_PROFILE_STATS.irq_reap_ns.load(Ordering::Relaxed);
    let resp_sync_ns = READ_BIO_PROFILE_STATS.resp_sync_ns.load(Ordering::Relaxed);
    let device_wait_ns = READ_BIO_PROFILE_STATS.device_wait_ns.load(Ordering::Relaxed);
    let dma_sync_ns = READ_BIO_PROFILE_STATS.dma_sync_ns.load(Ordering::Relaxed);
    let complete_ns = READ_BIO_PROFILE_STATS.complete_ns.load(Ordering::Relaxed);
    let large_bios = READ_BIO_PROFILE_STATS.large_read_bios.load(Ordering::Relaxed);
    let large_bytes = READ_BIO_PROFILE_STATS.large_read_bytes.load(Ordering::Relaxed);
    let large_segments = READ_BIO_PROFILE_STATS.large_read_segments.load(Ordering::Relaxed);
    let large_queue_wait_ns = READ_BIO_PROFILE_STATS.large_queue_wait_ns.load(Ordering::Relaxed);
    let large_dispatch_ns = READ_BIO_PROFILE_STATS.large_dispatch_ns.load(Ordering::Relaxed);
    let large_irq_delivery_ns = READ_BIO_PROFILE_STATS
        .large_irq_delivery_ns
        .load(Ordering::Relaxed);
    let large_irq_reap_ns = READ_BIO_PROFILE_STATS
        .large_irq_reap_ns
        .load(Ordering::Relaxed);
    let large_resp_sync_ns = READ_BIO_PROFILE_STATS
        .large_resp_sync_ns
        .load(Ordering::Relaxed);
    let large_device_wait_ns =
        READ_BIO_PROFILE_STATS.large_device_wait_ns.load(Ordering::Relaxed);
    let large_dma_sync_ns = READ_BIO_PROFILE_STATS.large_dma_sync_ns.load(Ordering::Relaxed);
    let large_complete_ns = READ_BIO_PROFILE_STATS.large_complete_ns.load(Ordering::Relaxed);
    let max_total_ns = READ_BIO_PROFILE_STATS.max_total_ns.load(Ordering::Relaxed);

    aster_logger::_print(format_args!(
        concat!(
            "[block-profile] read-bio bios={} bytes={} avg_bytes={} avg_segments_x100={} ",
            "avg_queue_wait_us={} avg_dispatch_us={} avg_device_wait_us={} ",
            "avg_irq_delivery_us={} avg_irq_reap_us={} avg_resp_sync_us={} ",
            "avg_dma_sync_us={} avg_complete_us={} large_bios={} large_avg_bytes={} ",
            "large_avg_segments_x100={} large_avg_queue_wait_us={} ",
            "large_avg_dispatch_us={} large_avg_device_wait_us={} ",
            "large_avg_irq_delivery_us={} large_avg_irq_reap_us={} large_avg_resp_sync_us={} ",
            "large_avg_dma_sync_us={} large_avg_complete_us={} max_total_us={}\n"
        ),
        bios,
        bytes,
        bytes / bios,
        segments.saturating_mul(100) / bios,
        queue_wait_ns / bios / 1_000,
        dispatch_ns / bios / 1_000,
        device_wait_ns / bios / 1_000,
        irq_delivery_ns / bios / 1_000,
        irq_reap_ns / bios / 1_000,
        resp_sync_ns / bios / 1_000,
        dma_sync_ns / bios / 1_000,
        complete_ns / bios / 1_000,
        large_bios,
        if large_bios == 0 { 0 } else { large_bytes / large_bios },
        if large_bios == 0 {
            0
        } else {
            large_segments.saturating_mul(100) / large_bios
        },
        if large_bios == 0 {
            0
        } else {
            large_queue_wait_ns / large_bios / 1_000
        },
        if large_bios == 0 {
            0
        } else {
            large_dispatch_ns / large_bios / 1_000
        },
        if large_bios == 0 {
            0
        } else {
            large_device_wait_ns / large_bios / 1_000
        },
        if large_bios == 0 {
            0
        } else {
            large_irq_delivery_ns / large_bios / 1_000
        },
        if large_bios == 0 {
            0
        } else {
            large_irq_reap_ns / large_bios / 1_000
        },
        if large_bios == 0 {
            0
        } else {
            large_resp_sync_ns / large_bios / 1_000
        },
        if large_bios == 0 {
            0
        } else {
            large_dma_sync_ns / large_bios / 1_000
        },
        if large_bios == 0 {
            0
        } else {
            large_complete_ns / large_bios / 1_000
        },
        max_total_ns / 1_000,
    ));
}

impl Bio {
    /// Constructs a new `Bio`.
    ///
    /// The `type_` describes the type of the I/O.
    /// The `start_sid` is the starting sector id on the device.
    /// The `segments` describes the memory segments.
    /// The `complete_fn` is the optional callback function.
    pub fn new(
        type_: BioType,
        start_sid: Sid,
        segments: Vec<BioSegment>,
        complete_fn: Option<fn(&SubmittedBio)>,
    ) -> Self {
        let nsectors = segments
            .iter()
            .map(|segment| segment.nsectors().to_raw())
            .sum();

        let inner = Arc::new(BioInner {
            type_,
            sid_range: start_sid..start_sid + nsectors,
            sid_offset: AtomicU64::new(0),
            segments,
            complete_fn,
            flags: AtomicU32::new(0),
            status: AtomicU32::new(BioStatus::Init as u32),
            submit_ns: AtomicU64::new(0),
            dequeue_ns: AtomicU64::new(0),
            device_submit_ns: AtomicU64::new(0),
            irq_enter_ns: AtomicU64::new(0),
            used_reaped_ns: AtomicU64::new(0),
            irq_seen_ns: AtomicU64::new(0),
            dma_sync_done_ns: AtomicU64::new(0),
            wait_queue: WaitQueue::new(),
        });
        Self(inner)
    }

    /// Returns the type.
    pub fn type_(&self) -> BioType {
        self.0.type_()
    }

    /// Returns the range of target sectors on the device.
    pub fn sid_range(&self) -> &Range<Sid> {
        self.0.sid_range()
    }

    /// Returns the slice to the memory segments.
    pub fn segments(&self) -> &[BioSegment] {
        self.0.segments()
    }

    /// Returns the status.
    pub fn status(&self) -> BioStatus {
        self.0.status()
    }

    /// Hints the block layer that the bio is worth trying on a fast submit path.
    pub fn prefer_fast_submit(&self) {
        self.0
            .flags
            .fetch_or(BIO_FLAG_PREFER_FAST_SUBMIT, Ordering::Relaxed);
    }

    /// Submits self to the `block_device` asynchronously.
    ///
    /// Returns a `BioWaiter` to the caller to wait for its completion.
    ///
    /// # Panics
    ///
    /// The caller must not submit a `Bio` more than once. Otherwise, a panic shall be triggered.
    pub fn submit(&self, block_device: &dyn BlockDevice) -> Result<BioWaiter, BioEnqueueError> {
        self.0.submit_ns.store(monotonic_nanos(), Ordering::Relaxed);

        // Change the status from "Init" to "Submit".
        let result = self.0.status.compare_exchange(
            BioStatus::Init as u32,
            BioStatus::Submit as u32,
            Ordering::Release,
            Ordering::Relaxed,
        );
        assert!(result.is_ok());

        if let Err(e) = block_device.enqueue(SubmittedBio(self.0.clone())) {
            // Fail to submit, revert the status.
            let result = self.0.status.compare_exchange(
                BioStatus::Submit as u32,
                BioStatus::Init as u32,
                Ordering::Release,
                Ordering::Relaxed,
            );
            assert!(result.is_ok());
            return Err(e);
        }

        Ok(BioWaiter {
            bios: vec![self.0.clone()],
        })
    }

    /// Submits self to the `block_device` and waits for the result synchronously.
    ///
    /// Returns the result status of the `Bio`.
    ///
    /// # Panics
    ///
    /// The caller must not submit a `Bio` more than once. Otherwise, a panic shall be triggered.
    pub fn submit_and_wait(
        &self,
        block_device: &dyn BlockDevice,
    ) -> Result<BioStatus, BioEnqueueError> {
        let waiter = self.submit(block_device)?;
        match waiter.wait() {
            Some(status) => {
                assert!(status == BioStatus::Complete);
                Ok(status)
            }
            None => {
                let status = self.status();
                assert!(status != BioStatus::Complete);
                Ok(status)
            }
        }
    }
}

/// The error type returned when enqueueing the `Bio`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BioEnqueueError {
    /// The request queue is full
    IsFull,
    /// Refuse to enqueue the bio
    Refused,
    /// Too big bio
    TooBig,
}

impl From<BioEnqueueError> for ostd::Error {
    fn from(_error: BioEnqueueError) -> Self {
        ostd::Error::NotEnoughResources
    }
}

/// A waiter for `Bio` submissions.
///
/// This structure holds a list of `Bio` requests and provides functionality to
/// wait for their completion and retrieve their statuses.
#[must_use]
#[derive(Debug)]
pub struct BioWaiter {
    bios: Vec<Arc<BioInner>>,
}

impl BioWaiter {
    /// Constructs a new `BioWaiter` instance with no `Bio` requests.
    pub fn new() -> Self {
        Self { bios: Vec::new() }
    }

    /// Returns the number of `Bio` requests associated with `self`.
    pub fn nreqs(&self) -> usize {
        self.bios.len()
    }

    /// Gets the `index`-th `Bio` request associated with `self`.
    ///
    /// # Panics
    ///
    /// If the `index` is out of bounds, this method will panic.
    pub fn req(&self, index: usize) -> Bio {
        Bio(self.bios[index].clone())
    }

    /// Returns the status of the `index`-th `Bio` request associated with `self`.
    ///
    /// # Panics
    ///
    /// If the `index` is out of bounds, this method will panic.
    pub fn status(&self, index: usize) -> BioStatus {
        self.bios[index].status()
    }

    /// Merges the `Bio` requests from another `BioWaiter` into this one.
    ///
    /// The another `BioWaiter`'s `Bio` requests are appended to the end of
    /// the `Bio` list of `self`, effectively concatenating the two lists.
    pub fn concat(&mut self, mut other: Self) {
        self.bios.append(&mut other.bios);
    }

    /// Returns an iterator for the `Bio` requests associated with `self`.
    pub fn reqs(&self) -> impl Iterator<Item = Bio> {
        self.bios.iter().map(|bio_inner| Bio(bio_inner.clone()))
    }

    /// Waits for the completion of all `Bio` requests.
    ///
    /// This method iterates through each `Bio` in the list, waiting for their
    /// completion.
    ///
    /// The return value is an option indicating whether all the requests in the list
    /// have successfully completed.
    /// On success this value is guaranteed to be equal to `Some(BioStatus::Complete)`.
    pub fn wait(&self) -> Option<BioStatus> {
        let mut ret = Some(BioStatus::Complete);

        for bio in self.bios.iter() {
            let status = bio.wait_queue.wait_until(|| {
                let status = bio.status();
                if status != BioStatus::Submit {
                    Some(status)
                } else {
                    None
                }
            });
            if status != BioStatus::Complete && ret.is_some() {
                ret = None;
            }
        }

        ret
    }

    /// Clears all `Bio` requests in this waiter.
    pub fn clear(&mut self) {
        self.bios.clear();
    }
}

impl Default for BioWaiter {
    fn default() -> Self {
        Self::new()
    }
}

/// A submitted `Bio` object.
///
/// The request queue of block device only accepts a `SubmittedBio` into the queue.
#[derive(Debug, Clone)]
pub struct SubmittedBio(Arc<BioInner>);

impl SubmittedBio {
    /// Returns the type.
    pub fn type_(&self) -> BioType {
        self.0.type_()
    }

    /// Returns the range of target sectors on the device.
    pub fn sid_range(&self) -> &Range<Sid> {
        self.0.sid_range()
    }

    /// Returns the offset of the first sector id.
    pub fn sid_offset(&self) -> u64 {
        self.0.sid_offset.load(Ordering::Relaxed)
    }

    /// Sets the offset of the first sector id.
    pub fn set_sid_offset(&self, offset: u64) {
        self.0.sid_offset.store(offset, Ordering::Relaxed);
    }

    /// Returns the slice to the memory segments.
    pub fn segments(&self) -> &[BioSegment] {
        self.0.segments()
    }

    /// Returns the status.
    pub fn status(&self) -> BioStatus {
        self.0.status()
    }

    pub fn prefers_fast_submit(&self) -> bool {
        self.0.flags.load(Ordering::Relaxed) & BIO_FLAG_PREFER_FAST_SUBMIT != 0
    }

    pub fn mark_dequeued(&self, timestamp_ns: u64) {
        self.0.dequeue_ns.store(timestamp_ns, Ordering::Relaxed);
    }

    pub fn mark_device_submitted(&self, timestamp_ns: u64) {
        self.0.device_submit_ns.store(timestamp_ns, Ordering::Relaxed);
    }

    pub fn mark_irq_entered(&self, timestamp_ns: u64) {
        self.0.irq_enter_ns.store(timestamp_ns, Ordering::Relaxed);
    }

    pub fn mark_used_reaped(&self, timestamp_ns: u64) {
        self.0.used_reaped_ns.store(timestamp_ns, Ordering::Relaxed);
    }

    pub fn mark_irq_seen(&self, timestamp_ns: u64) {
        self.0.irq_seen_ns.store(timestamp_ns, Ordering::Relaxed);
    }

    pub fn mark_dma_sync_done(&self, timestamp_ns: u64) {
        self.0.dma_sync_done_ns.store(timestamp_ns, Ordering::Relaxed);
    }

    /// Completes the `Bio` with the `status` and invokes the callback function.
    ///
    /// When the driver finishes the request for this `Bio`, it will call this method.
    pub fn complete(&self, status: BioStatus) {
        assert!(status != BioStatus::Init && status != BioStatus::Submit);

        // Set the status.
        let result = self.0.status.compare_exchange(
            BioStatus::Submit as u32,
            status as u32,
            Ordering::Release,
            Ordering::Relaxed,
        );
        assert!(result.is_ok());

        if status == BioStatus::Complete && self.type_() == BioType::Read {
            let complete_ns = monotonic_nanos();
            let submit_ns = self.0.submit_ns.load(Ordering::Relaxed);
            let dequeue_ns = self.0.dequeue_ns.load(Ordering::Relaxed);
            let device_submit_ns = self.0.device_submit_ns.load(Ordering::Relaxed);
            let irq_enter_ns = self.0.irq_enter_ns.load(Ordering::Relaxed);
            let used_reaped_ns = self.0.used_reaped_ns.load(Ordering::Relaxed);
            let irq_seen_ns = self.0.irq_seen_ns.load(Ordering::Relaxed);
            let dma_sync_done_ns = self.0.dma_sync_done_ns.load(Ordering::Relaxed);
            let bytes = u64::try_from(self.0.nbytes()).unwrap_or(u64::MAX);
            let segments = u64::try_from(self.0.segments.len()).unwrap_or(u64::MAX);

            let queue_wait_ns = dequeue_ns.saturating_sub(submit_ns);
            let dispatch_ns = device_submit_ns.saturating_sub(dequeue_ns);
            let irq_delivery_ns = irq_enter_ns.saturating_sub(device_submit_ns);
            let irq_reap_ns = used_reaped_ns.saturating_sub(irq_enter_ns);
            let resp_sync_ns = irq_seen_ns.saturating_sub(used_reaped_ns);
            let device_wait_ns = irq_seen_ns.saturating_sub(device_submit_ns);
            let dma_sync_ns = dma_sync_done_ns.saturating_sub(irq_seen_ns);
            let complete_path_ns = complete_ns.saturating_sub(dma_sync_done_ns);
            let total_ns = complete_ns.saturating_sub(submit_ns);

            let bios = READ_BIO_PROFILE_STATS.record_read_bio(
                bytes,
                segments,
                queue_wait_ns,
                dispatch_ns,
                irq_delivery_ns,
                irq_reap_ns,
                resp_sync_ns,
                device_wait_ns,
                dma_sync_ns,
                complete_path_ns,
                total_ns,
            );
            maybe_log_read_bio_profile(bios);
        }

        self.0.wait_queue.wake_all();
        if let Some(complete_fn) = self.0.complete_fn {
            complete_fn(self);
        }
    }
}

/// The common inner part of `Bio`.
struct BioInner {
    /// The type of the I/O
    type_: BioType,
    /// The logical range of target sectors on device
    sid_range: Range<Sid>,
    /// The offset of the first sector id, used to adjust the `sid_range` for partition devices
    sid_offset: AtomicU64,
    /// The memory segments in this `Bio`
    segments: Vec<BioSegment>,
    /// The I/O completion method
    complete_fn: Option<fn(&SubmittedBio)>,
    /// Per-bio submission hints consumed by the block layer.
    flags: AtomicU32,
    /// The I/O status
    status: AtomicU32,
    /// Timestamp when the bio is submitted by the filesystem.
    submit_ns: AtomicU64,
    /// Timestamp when the software request queue dequeues the bio.
    dequeue_ns: AtomicU64,
    /// Timestamp when the driver submits descriptors to the virtqueue.
    device_submit_ns: AtomicU64,
    /// Timestamp when the IRQ handler starts processing the completed request.
    irq_enter_ns: AtomicU64,
    /// Timestamp when the used ring entry is popped and the request is reaped.
    used_reaped_ns: AtomicU64,
    /// Timestamp when the IRQ handler observes completion.
    irq_seen_ns: AtomicU64,
    /// Timestamp after read DMA buffers are synchronized from device.
    dma_sync_done_ns: AtomicU64,
    /// The wait queue for I/O completion
    wait_queue: WaitQueue,
}

impl BioInner {
    pub fn type_(&self) -> BioType {
        self.type_
    }

    pub fn sid_range(&self) -> &Range<Sid> {
        &self.sid_range
    }

    pub fn segments(&self) -> &[BioSegment] {
        &self.segments
    }

    pub fn nbytes(&self) -> usize {
        self.segments.iter().map(BioSegment::nbytes).sum()
    }

    pub fn status(&self) -> BioStatus {
        BioStatus::try_from(self.status.load(Ordering::Relaxed)).unwrap()
    }
}

impl Debug for BioInner {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.debug_struct("BioInner")
            .field("type", &self.type_())
            .field("sid_range", &self.sid_range())
            .field("status", &self.status())
            .field("segments", &self.segments())
            .field("complete_fn", &self.complete_fn)
            .finish()
    }
}

/// The type of `Bio`.
#[derive(Clone, Copy, Debug, PartialEq, TryFromInt)]
#[repr(u8)]
pub enum BioType {
    /// Read sectors from the device.
    Read = 0,
    /// Write sectors into the device.
    Write = 1,
    /// Flush the volatile write cache.
    Flush = 2,
    /// Discard sectors.
    Discard = 3,
}

/// The status of `Bio`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, TryFromInt)]
#[repr(u32)]
pub enum BioStatus {
    /// The initial status for a newly created `Bio`.
    Init = 0,
    /// After a `Bio` is submitted, its status will be changed to "Submit".
    Submit = 1,
    /// The I/O operation has been successfully completed.
    Complete = 2,
    /// The I/O operation is not supported.
    NotSupported = 3,
    /// Insufficient space is available to perform the I/O operation.
    NoSpace = 4,
    /// An error occurred while doing I/O.
    IoError = 5,
}

/// `BioSegment` is the basic memory unit of a block I/O request.
#[derive(Debug, Clone)]
pub struct BioSegment {
    inner: Arc<BioSegmentInner>,
}

/// The inner part of `BioSegment`.
// TODO: Decouple `BioSegmentInner` with DMA-related buffers.
#[derive(Debug)]
struct BioSegmentInner {
    /// Internal DMA slice.
    // TODO: The direction is currently `FromAndToDevice`. Implement compile-time checking.
    dma_slice: Slice<Arc<DmaStream>>,
    direction: BioDirection,
    /// Whether the segment is allocated from the pool.
    from_pool: bool,
}

/// The direction of a bio request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BioDirection {
    /// Read from the backed block device.
    FromDevice,
    /// Write to the backed block device.
    ToDevice,
}

impl BioSegment {
    /// Allocates a new `BioSegment` with the wanted blocks count and
    /// the bio direction.
    pub fn alloc(nblocks: usize, direction: BioDirection) -> Self {
        Self::alloc_inner(nblocks, 0, nblocks * BLOCK_SIZE, direction)
    }

    /// The inner function that do the real segment allocation.
    ///
    /// Support two extended parameters:
    /// 1. `offset_within_first_block`: the offset (in bytes) within the first block.
    /// 2. `len`: the exact length (in bytes) of the wanted segment. (May
    ///    less than `nblocks * BLOCK_SIZE`)
    ///
    /// # Panics
    ///
    /// If the `offset_within_first_block` or `len` is not sector aligned,
    /// this method will panic.
    pub(super) fn alloc_inner(
        nblocks: usize,
        offset_within_first_block: usize,
        len: usize,
        direction: BioDirection,
    ) -> Self {
        let offset = offset_within_first_block;
        assert!(
            is_sector_aligned(offset)
                && offset < BLOCK_SIZE
                && is_sector_aligned(len)
                && offset + len <= nblocks * BLOCK_SIZE
        );

        // The target segment is whether from the pool or newly-allocated
        let bio_segment_inner = target_pool(direction)
            .and_then(|pool| pool.alloc(nblocks, offset, len))
            .unwrap_or_else(|| {
                let dma_stream = DmaStream::alloc_uninit(nblocks, false).unwrap();
                BioSegmentInner {
                    dma_slice: Slice::new(Arc::new(dma_stream), offset..offset + len),
                    direction,
                    from_pool: false,
                }
            });

        Self {
            inner: Arc::new(bio_segment_inner),
        }
    }

    /// Constructs a new `BioSegment` with a given `USegment` and the bio direction.
    pub fn new_from_segment(segment: USegment, direction: BioDirection) -> Self {
        let len = segment.size();
        let dma_stream = DmaStream::map(segment, false).unwrap();
        Self {
            inner: Arc::new(BioSegmentInner {
                dma_slice: Slice::new(Arc::new(dma_stream), 0..len),
                direction,
                from_pool: false,
            }),
        }
    }

    /// Returns the number of bytes.
    pub fn nbytes(&self) -> usize {
        self.inner.dma_slice.size()
    }

    /// Returns the number of sectors.
    pub fn nsectors(&self) -> Sid {
        Sid::from_offset(self.nbytes())
    }

    /// Returns the number of blocks.
    pub fn nblocks(&self) -> usize {
        self.nbytes().align_up(BLOCK_SIZE) / BLOCK_SIZE
    }

    /// Returns the offset (in bytes) within the first block.
    pub fn offset_within_first_block(&self) -> usize {
        self.inner.dma_slice.offset().start % BLOCK_SIZE
    }

    /// Returns the inner DMA slice.
    pub fn inner_dma_slice(&self) -> &Slice<Arc<DmaStream>> {
        &self.inner.dma_slice
    }

    /// Returns the inner DMA object.
    ///
    /// Note that the slicing will be ignored. This is only for testing.
    #[cfg(ktest)]
    pub fn inner_dma(&self) -> &Arc<DmaStream> {
        self.inner.dma_slice.mem_obj()
    }
}

impl HasVmReaderWriter for BioSegment {
    type Types = VmReaderWriterResult;

    fn reader(&self) -> Result<VmReader<'_, Infallible>, Error> {
        if self.inner.direction != BioDirection::FromDevice {
            return Err(Error::AccessDenied);
        }
        self.inner.dma_slice.reader()
    }

    fn writer(&self) -> Result<VmWriter<'_, Infallible>, Error> {
        if self.inner.direction != BioDirection::ToDevice {
            return Err(Error::AccessDenied);
        }
        self.inner.dma_slice.writer()
    }
}

// The timing for free the segment to the pool.
impl Drop for BioSegmentInner {
    fn drop(&mut self) {
        if !self.from_pool {
            return;
        }
        if let Some(pool) = target_pool(self.direction()) {
            pool.free(self);
        }
    }
}

impl BioSegmentInner {
    /// Returns the bio direction.
    fn direction(&self) -> BioDirection {
        self.direction
    }
}

/// A pool of managing segments for block I/O requests.
///
/// Inside the pool, it's a large chunk of `DmaStream` which
/// contains the mapped segment. The allocation/free is done by slicing
/// the `DmaStream`.
// TODO: Use a more advanced allocation algorithm to replace the naive one to improve efficiency.
struct BioSegmentPool {
    pool: Arc<DmaStream>,
    total_blocks: usize,
    direction: BioDirection,
    manager: SpinLock<PoolSlotManager>,
}

/// Manages the free slots in the pool.
struct PoolSlotManager {
    /// A bit array to manage the occupied slots in the pool (Bit
    /// value 1 represents "occupied"; 0 represents "free").
    /// The total size is currently determined by `POOL_DEFAULT_NBLOCKS`.
    occupied: BitArray<[u8; POOL_DEFAULT_NBLOCKS.div_ceil(8)]>,
    /// The first index of all free slots in the pool.
    min_free: usize,
}

impl BioSegmentPool {
    /// Creates a new pool given the bio direction. The total number of
    /// managed blocks is currently set to `POOL_DEFAULT_NBLOCKS`.
    ///
    /// The new pool will be allocated and mapped for later allocation.
    pub fn new(direction: BioDirection) -> Self {
        let total_blocks = POOL_DEFAULT_NBLOCKS;
        let pool = DmaStream::alloc_uninit(total_blocks, false).unwrap();
        let manager = SpinLock::new(PoolSlotManager {
            occupied: BitArray::ZERO,
            min_free: 0,
        });

        Self {
            pool: Arc::new(pool),
            total_blocks,
            direction,
            manager,
        }
    }

    /// Allocates a bio segment with the given count `nblocks`
    /// from the pool.
    ///
    /// Support two extended parameters:
    /// 1. `offset_within_first_block`: the offset (in bytes) within the first block.
    /// 2. `len`: the exact length (in bytes) of the wanted segment. (May
    ///    less than `nblocks * BLOCK_SIZE`)
    ///
    /// If there is no enough space in the pool, this method
    /// will return `None`.
    ///
    /// # Panics
    ///
    /// If the `offset_within_first_block` exceeds the block size, or the `len`
    /// exceeds the total length, this method will panic.
    pub fn alloc(
        &self,
        nblocks: usize,
        offset_within_first_block: usize,
        len: usize,
    ) -> Option<BioSegmentInner> {
        assert!(
            offset_within_first_block < BLOCK_SIZE
                && offset_within_first_block + len <= nblocks * BLOCK_SIZE
        );
        let mut manager = self.manager.lock();
        if nblocks > self.total_blocks - manager.min_free {
            return None;
        }

        // Find the free range
        let (start, end) = {
            let mut start = manager.min_free;
            let mut end = start;
            while end < self.total_blocks && end - start < nblocks {
                if manager.occupied[end] {
                    start = end + 1;
                    end = start;
                } else {
                    end += 1;
                }
            }
            if end - start < nblocks {
                return None;
            }
            (start, end)
        };

        manager.occupied[start..end].fill(true);
        manager.min_free = manager.occupied[end..]
            .iter()
            .position(|i| !i)
            .map(|pos| end + pos)
            .unwrap_or(self.total_blocks);

        let dma_slice = {
            let offset = start * BLOCK_SIZE + offset_within_first_block;
            Slice::new(self.pool.clone(), offset..offset + len)
        };
        let bio_segment = BioSegmentInner {
            dma_slice,
            direction: self.direction,
            from_pool: true,
        };
        Some(bio_segment)
    }

    /// Returns an allocated bio segment to the pool,
    /// free the space. This method is not public and should only
    /// be called automatically by `BioSegmentInner::drop()`.
    ///
    /// # Panics
    ///
    /// If the target bio segment is not allocated from the pool
    /// or not the same direction, this method will panic.
    fn free(&self, bio_segment: &BioSegmentInner) {
        assert!(bio_segment.from_pool && bio_segment.direction() == self.direction);
        let (start, end) = {
            let dma_slice = &bio_segment.dma_slice;
            let start = dma_slice.offset().start.align_down(BLOCK_SIZE) / BLOCK_SIZE;
            let end = dma_slice.offset().end.align_up(BLOCK_SIZE) / BLOCK_SIZE;

            if end <= start || end > self.total_blocks {
                return;
            }
            (start, end)
        };

        let mut manager = self.manager.lock();
        debug_assert!(manager.occupied[start..end].iter().all(|i| *i));
        manager.occupied[start..end].fill(false);
        if start < manager.min_free {
            manager.min_free = start;
        }
    }
}

/// A pool of segments for read bio requests only.
static BIO_SEGMENT_RPOOL: Once<Arc<BioSegmentPool>> = Once::new();
/// A pool of segments for write bio requests only.
static BIO_SEGMENT_WPOOL: Once<Arc<BioSegmentPool>> = Once::new();
/// The default number of blocks in each pool. (16MB each for now)
const POOL_DEFAULT_NBLOCKS: usize = 4096;

/// Initializes the bio segment pool.
pub fn bio_segment_pool_init() {
    BIO_SEGMENT_RPOOL.call_once(|| Arc::new(BioSegmentPool::new(BioDirection::FromDevice)));
    BIO_SEGMENT_WPOOL.call_once(|| Arc::new(BioSegmentPool::new(BioDirection::ToDevice)));
}

/// Gets the target pool with the given `direction`.
fn target_pool(direction: BioDirection) -> Option<&'static Arc<BioSegmentPool>> {
    match direction {
        BioDirection::FromDevice => BIO_SEGMENT_RPOOL.get(),
        BioDirection::ToDevice => BIO_SEGMENT_WPOOL.get(),
    }
}

/// Checks if the given offset is aligned to sector.
pub fn is_sector_aligned(offset: usize) -> bool {
    offset.is_multiple_of(SECTOR_SIZE)
}

/// An aligned unsigned integer number.
///
/// An instance of `AlignedUsize<const N: u16>` is guaranteed to have a value that is a multiple
/// of `N`, a predetermined const value. It is preferable to express an unsigned integer value
/// in type `AlignedUsize<_>` instead of `usize` if the value must satisfy an alignment requirement.
/// This helps readability and prevents bugs.
///
/// # Examples
///
/// ```rust
/// const SECTOR_SIZE: u16 = 512;
///
/// let sector_num = 1234; // The 1234-th sector
/// let sector_offset: AlignedUsize<SECTOR_SIZE> = {
///     let sector_offset = sector_num * (SECTOR_SIZE as usize);
///     AlignedUsize::<SECTOR_SIZE>::new(sector_offset).unwrap()
/// };
/// assert!(sector_offset.value().is_multiple_of(sector_offset.align()));
/// ```
///
/// # Limitation
///
/// Currently, the alignment const value must be expressed in `u16`;
/// it is not possible to use a larger or smaller type.
/// This limitation is inherited from that of Rust's const generics:
/// your code can be generic over the _value_ of a const, but not the _type_ of the const.
/// We choose `u16` because it is reasonably large to represent any alignment value
/// used in practice.
#[derive(Debug, Clone)]
pub struct AlignedUsize<const N: u16>(usize);

impl<const N: u16> AlignedUsize<N> {
    /// Constructs a new instance of aligned integer if the given value is aligned.
    pub fn new(val: usize) -> Option<Self> {
        if val.is_multiple_of(N as usize) {
            Some(Self(val))
        } else {
            None
        }
    }

    /// Returns the value.
    pub fn value(&self) -> usize {
        self.0
    }

    /// Returns the corresponding ID.
    ///
    /// The so-called "ID" of an aligned integer is defined to be `self.value() / self.align()`.
    /// This value is named ID because one common use case is using `Aligned` to express
    /// the byte offset of a sector, block, or page. In this case, the `id` method returns
    /// the ID of the corresponding sector, block, or page.
    pub fn id(&self) -> usize {
        self.value() / self.align()
    }

    /// Returns the alignment.
    pub fn align(&self) -> usize {
        N as usize
    }
}
