// SPDX-License-Identifier: MPL-2.0

//! Small helpers shared across the ext4 module.
//!
//! - `Dirty` — a wrapper that tracks whether its inner value has been mutated,
//!   for writeback scheduling.
//! - `now` — reads the real-time coarse clock.

use super::prelude::*;
use crate::prelude::warn;

/// A value with dirty tracking.
pub(super) struct Dirty<T: Debug> {
    value: T,
    dirty: bool,
}

impl<T: Debug> Dirty<T> {
    /// Creates a new `Dirty` value without setting the dirty flag.
    pub(super) fn new(val: T) -> Dirty<T> {
        Dirty {
            value: val,
            dirty: false,
        }
    }

    /// Returns whether the value is dirty.
    pub(super) fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Clears the dirty flag.
    pub(super) fn clear_dirty(&mut self) {
        self.dirty = false;
    }
}

impl<T: Debug> Deref for Dirty<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T: Debug> DerefMut for Dirty<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.dirty = true;
        &mut self.value
    }
}

impl<T: Debug> Drop for Dirty<T> {
    fn drop(&mut self) {
        if self.is_dirty() {
            warn!("dropped while dirty: {:?}", self.value);
        }
    }
}

impl<T: Debug> Debug for Dirty<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let tag = if self.dirty { "Dirty" } else { "Clean" };
        write!(f, "[{}] {:?}", tag, self.value)
    }
}

/// Returns the current time.
pub(super) fn now() -> Duration {
    crate::time::clocks::RealTimeCoarseClock::get().read_time()
}
