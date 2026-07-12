// SPDX-License-Identifier: MPL-2.0

use core::ops::Range;

use aster_block::BLOCK_SIZE;
use bitvec::{
    order::Lsb0,
    slice::BitSlice,
    view::{AsBits, AsMutBits},
};

use crate::prelude::*;

/// A disk I/O-friendly bitmap for ID management (e.g., block or inode IDs).
///
/// An ID bitmap has the same size as a block, i.e., `BLOCK_SIZE`.
/// Each bit in the bitmap represents one ID:
/// bit 0 at the i-th position of the bitmap means the i-th ID is free
/// and bit 1 means the ID is in use.
/// As such, the bitmap can contain contain at most `BLOCK_SIZE` * 8 bits/IDs.
#[derive(Clone)]
pub struct IdBitmap {
    buf: Box<[u8]>,
    first_available_id: u16,
    len: u16,
}

impl IdBitmap {
    /// Creates a new ID bitmap out of a given buffer, whose first `len`-bits represent valid IDs.
    ///
    /// # Panics
    ///
    /// This method panics if `len` is greater than [`IdBitmap::capacity()`].
    pub fn from_buf(buf: Box<[u8]>, len: u16) -> Self {
        assert!(len <= Self::capacity());
        let mut bitmap = Self {
            buf,
            first_available_id: 0,
            len,
        };

        let bit_slice = bitmap.bit_slice();
        bitmap.first_available_id = (0..len).find(|&i| !bit_slice[i as usize]).unwrap_or(len);
        bitmap
    }

    /// Returns the length of the ID bitmap, i.e., the maximum number of IDs.
    #[expect(unused)]
    pub const fn len(&self) -> u16 {
        self.len
    }

    /// Returns the capacity of the ID bitmap.
    ///
    /// The capacity is the size of the underlying buffer in bits.
    pub const fn capacity() -> u16 {
        BLOCK_SIZE as u16 * 8
    }

    /// Returns a reference to the underlying buffer of `BLOCK_SIZE` bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    fn bit_slice(&self) -> &BitSlice<u8, Lsb0> {
        &self.buf.as_bits()[..self.len as usize]
    }

    fn bit_slice_mut(&mut self) -> &mut BitSlice<u8, Lsb0> {
        &mut self.buf.as_mut_bits()[..self.len as usize]
    }

    /// Returns true if the `id` is allocated.
    ///
    /// # Panics
    ///
    /// If the `id` is out of bounds, this method will panic.
    pub fn is_allocated(&self, id: u16) -> bool {
        self.bit_slice()[id as usize]
    }

    /// Allocates and returns a new `id`.
    ///
    /// If allocation is not possible, it returns `None`.
    pub fn alloc(&mut self) -> Option<u16> {
        if self.first_available_id < self.len {
            let id = self.first_available_id;
            self.bit_slice_mut().set(id as usize, true);

            let bit_slice = self.bit_slice();
            self.first_available_id = (id + 1..self.len)
                .find(|&i| !bit_slice[i as usize])
                .unwrap_or(self.len);

            Some(id)
        } else {
            None
        }
    }

    /// Allocates a consecutive range of new `id`s.
    ///
    /// The `count` is the number of consecutive `id`s to allocate. If it is 0, return `None`.
    ///
    /// If allocation is not possible, it returns `None`.
    pub fn alloc_consecutive(&mut self, count: u16) -> Option<Range<u16>> {
        if count == 0 {
            return None;
        }

        let end = self.first_available_id.checked_add(count)?;
        if end > self.len {
            return None;
        }

        // Scan the bitmap from the position `first_available_id`
        // for the first `count` number of consecutive 0's.
        let allocated_range = {
            // Invariance: all bits within `curr_range` are 0's.
            let bit_slice = self.bit_slice();
            let mut curr_range = self.first_available_id..self.first_available_id + 1;
            while curr_range.len() < count as usize && curr_range.end < self.len {
                if !bit_slice[curr_range.end as usize] {
                    curr_range.end += 1;
                } else {
                    curr_range = curr_range.end + 1..curr_range.end + 1;
                }
            }

            if curr_range.len() < count as usize {
                return None;
            }

            curr_range
        };

        // Set every bit to 1 within the allocated range.
        let bit_slice_mut = self.bit_slice_mut();
        for id in allocated_range.clone() {
            bit_slice_mut.set(id as usize, true);
        }

        // In case we need to update `first_available_id`.
        let bit_slice = self.bit_slice();
        if bit_slice[self.first_available_id as usize] {
            self.first_available_id = (allocated_range.end..self.len)
                .find(|&i| !bit_slice[i as usize])
                .map_or(self.len, |i| i);
        }

        Some(allocated_range)
    }

    /// Allocates the first `count`-long run of free IDs at or after `hint` —
    /// goal-directed first fit. Returns `None` when `[hint, len)` holds no
    /// such run: the request is neither wrapped below `hint` (the caller's
    /// ring loop owns moving on) nor shrunk (a downsized run would defeat the
    /// contiguity the hint asks for).
    pub fn alloc_consecutive_from(&mut self, hint: u16, count: u16) -> Option<Range<u16>> {
        if count == 0 || hint >= self.len {
            return None;
        }
        // Everything below `first_available_id` is allocated, so the scan may
        // fast-forward to it when the hint lies below.
        let start = hint.max(self.first_available_id);
        let allocated_range = {
            let bit_slice = self.bit_slice();
            // Invariant: all bits within `curr_range` are 0's.
            let mut curr_range = start..start;
            while curr_range.len() < count as usize && curr_range.end < self.len {
                if !bit_slice[curr_range.end as usize] {
                    curr_range.end += 1;
                } else {
                    curr_range = curr_range.end + 1..curr_range.end + 1;
                }
            }
            if curr_range.len() < count as usize {
                return None;
            }
            curr_range
        };
        self.set_allocated(allocated_range)
    }

    /// Allocates the longest run of free IDs anywhere in the bitmap, stopping
    /// early at `cap` — the fallback pass when no group holds the full
    /// request: take the best piece available instead of rescanning one
    /// group with halved counts. Returns `None` when no ID is free.
    pub fn alloc_longest_run(&mut self, cap: u16) -> Option<Range<u16>> {
        if cap == 0 {
            return None;
        }
        let best = {
            let bit_slice = self.bit_slice();
            let mut best: Option<Range<u16>> = None;
            let mut curr = self.first_available_id..self.first_available_id;
            loop {
                if curr.end < self.len && !bit_slice[curr.end as usize] {
                    curr.end += 1;
                    if curr.len() >= cap as usize {
                        best = Some(curr);
                        break;
                    }
                    continue;
                }
                if !curr.is_empty() && best.as_ref().is_none_or(|b| curr.len() > b.len()) {
                    best = Some(curr.clone());
                }
                if curr.end >= self.len {
                    break;
                }
                curr = curr.end + 1..curr.end + 1;
            }
            best?
        };
        self.set_allocated(best)
    }

    /// Allocates exactly `[at, at + count)`, or `None` when any bit in the
    /// range is already set (or out of bounds) — the pin-hold and
    /// preallocation-carve primitive: the caller names the exact bits.
    pub fn alloc_exact_at(&mut self, at: u16, count: u16) -> Option<Range<u16>> {
        if count == 0 {
            return None;
        }
        let end = at.checked_add(count)?;
        if end > self.len {
            return None;
        }
        {
            let bit_slice = self.bit_slice();
            if (at..end).any(|i| bit_slice[i as usize]) {
                return None;
            }
        }
        self.set_allocated(at..end)
    }

    /// Marks `range` allocated and maintains the `first_available_id`
    /// invariant — the shared tail of every range allocator above.
    fn set_allocated(&mut self, range: Range<u16>) -> Option<Range<u16>> {
        let bit_slice_mut = self.bit_slice_mut();
        for id in range.clone() {
            bit_slice_mut.set(id as usize, true);
        }
        let bit_slice = self.bit_slice();
        if bit_slice[self.first_available_id as usize] {
            self.first_available_id = (range.end..self.len)
                .find(|&i| !bit_slice[i as usize])
                .map_or(self.len, |i| i);
        }
        Some(range)
    }

    /// Releases the allocated `id`.
    ///
    /// # Panics
    ///
    /// If the `id` is out of bounds, this method will panic.
    pub fn free(&mut self, id: u16) {
        debug_assert!(self.bit_slice()[id as usize]);

        self.bit_slice_mut().set(id as usize, false);
        if id < self.first_available_id {
            self.first_available_id = id;
        }
    }

    /// Releases the consecutive range of allocated `id`s.
    ///
    /// # Panics
    ///
    /// If the `range` is out of bounds, this method will panic.
    pub fn free_consecutive(&mut self, range: Range<u16>) {
        if range.is_empty() {
            return;
        }

        let range_start = range.start;
        let bit_slice_mut = self.bit_slice_mut();
        for id in range {
            debug_assert!(bit_slice_mut[id as usize]);
            bit_slice_mut.set(id as usize, false);
        }

        if range_start < self.first_available_id {
            self.first_available_id = range_start
        }
    }
}

impl Debug for IdBitmap {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.debug_struct("IdBitMap")
            .field("len", &self.len)
            .field("first_available_id", &self.first_available_id)
            .finish()
    }
}

#[cfg(ktest)]
mod test {
    use alloc::vec;

    use aster_block::BLOCK_SIZE;
    use ostd::prelude::ktest;

    use super::IdBitmap;

    #[ktest]
    fn bitmap_alloc_out_of_bounds() {
        let buf = vec![0; BLOCK_SIZE].into_boxed_slice();

        let capacity = BLOCK_SIZE as u16 * 8;
        let mut bitmap = IdBitmap::from_buf(buf, capacity);

        for _ in 0..capacity {
            assert!(bitmap.alloc().is_some());
        }

        // Allocating one more ID should fail since the
        // bitmap's `first_available_id` + `count` is out of bounds.
        assert!(bitmap.alloc_consecutive(1).is_none());
    }

    #[ktest]
    fn alloc_consecutive_from_is_goal_directed_and_never_shrinks() {
        let mut bm = IdBitmap::from_buf(vec![0; BLOCK_SIZE].into_boxed_slice(), 64);
        // Occupy [10, 20) so runs exist on both sides of a mid hint.
        assert_eq!(bm.alloc_consecutive_from(10, 10), Some(10..20));

        // Goal-directed: from 16 the run lands after the occupied span, not
        // in the (larger) free head below the hint.
        assert_eq!(bm.alloc_consecutive_from(16, 4), Some(20..24));
        // Never wraps below the hint and never shrinks: a request larger
        // than the tail fails outright even though the head could hold it.
        assert_eq!(bm.alloc_consecutive_from(30, 40), None);
        // The head stays reachable through a low hint.
        assert_eq!(bm.alloc_consecutive_from(0, 10), Some(0..10));
        // `first_available_id` stayed exact throughout: the next single
        // allocation lands in the lowest hole.
        assert_eq!(bm.alloc(), Some(24));
    }

    #[ktest]
    fn alloc_longest_run_takes_the_best_piece() {
        let mut bm = IdBitmap::from_buf(vec![0; BLOCK_SIZE].into_boxed_slice(), 64);
        // Carve free runs of 3, 8, and 5: [0,3) [6,14) [20,25), rest occupied.
        assert!(bm.alloc_consecutive_from(3, 3).is_some()); // [3,6)
        assert!(bm.alloc_consecutive_from(14, 6).is_some()); // [14,20)
        assert!(bm.alloc_consecutive_from(25, 39).is_some()); // [25,64)

        // A capped request early-stops at the first run that satisfies it…
        assert_eq!(bm.alloc_longest_run(3), Some(0..3));
        // …and an uncappable one takes the longest available piece.
        assert_eq!(bm.alloc_longest_run(16), Some(6..14));
        assert_eq!(bm.alloc_longest_run(16), Some(20..25));
        // Nothing free → None.
        assert_eq!(bm.alloc_longest_run(1), None);
    }
}
