//! An injectable clock so leases, backoff and SLAs are testable without sleeping.

use std::sync::atomic::{AtomicI64, Ordering};

/// A source of the current time in epoch milliseconds.
pub trait Clock: Send + Sync {
    /// The current time.
    fn now_ms(&self) -> i64;
}

/// The wall clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        crate::time::system_now_ms()
    }
}

/// A clock that only moves when told to, for tests of expiry and escalation.
#[derive(Debug)]
pub struct ManualClock {
    now: AtomicI64,
}

impl ManualClock {
    /// A manual clock starting at the given time.
    pub fn new(start_ms: i64) -> Self {
        ManualClock {
            now: AtomicI64::new(start_ms),
        }
    }

    /// Moves the clock forward.
    pub fn advance(&self, ms: i64) {
        self.now.fetch_add(ms, Ordering::SeqCst);
    }

    /// Sets the clock.
    pub fn set(&self, ms: i64) {
        self.now.store(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
}
