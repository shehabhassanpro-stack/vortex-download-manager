//! Deterministic time abstraction (`Clock` trait — `P1-T01` / `ADR-018`).
//!
//! Pure `std::time` + atomics with zero async or OS I/O dependencies so domain policies,
//! EWMA estimators, and engine backoff schedules are 100% deterministic in tests.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Pure time source for monotonic elapsed measurements and Unix wall-clock timestamps.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Current wall-clock timestamp in milliseconds since Unix epoch (`1970-01-01T00:00:00Z`).
    fn now_unix_ms(&self) -> i64;

    /// Monotonically non-decreasing nanoseconds since clock creation.
    fn monotonic_ns(&self) -> u64;

    /// Monotonically non-decreasing milliseconds since clock creation.
    fn monotonic_ms(&self) -> u64 {
        self.monotonic_ns() / 1_000_000
    }
}

/// Production clock backed by `std::time::SystemTime` and `std::time::Instant`.
#[derive(Debug)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    /// Creates a new [`SystemClock`] anchored at the current monotonic instant.
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_unix_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
    }

    fn monotonic_ns(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}

/// Lock-free deterministic clock for fast, zero-flakiness unit and integration tests.
#[derive(Debug)]
pub struct FakeClock {
    unix_ms: AtomicI64,
    mono_ns: AtomicU64,
}

impl FakeClock {
    /// Creates a [`FakeClock`] starting at the specified Unix epoch millisecond timestamp.
    #[must_use]
    pub const fn new(initial_unix_ms: i64) -> Self {
        Self {
            unix_ms: AtomicI64::new(initial_unix_ms),
            mono_ns: AtomicU64::new(0),
        }
    }

    /// Advances both the monotonic counter and Unix wall clock by `delta_ms` milliseconds.
    pub fn advance_ms(&self, delta_ms: u64) {
        let nanos_to_add = delta_ms.saturating_mul(1_000_000);
        self.mono_ns.fetch_add(nanos_to_add, Ordering::SeqCst);
        let signed_ms = i64::try_from(delta_ms).unwrap_or(i64::MAX);
        self.unix_ms.fetch_add(signed_ms, Ordering::SeqCst);
    }

    /// Advances both clocks by `delta_ns` nanoseconds (updating wall clock by whole ms).
    pub fn advance_ns(&self, delta_ns: u64) {
        let prev = self.mono_ns.fetch_add(delta_ns, Ordering::SeqCst);
        let prev_ms = prev / 1_000_000;
        let next_ms = prev.saturating_add(delta_ns) / 1_000_000;
        let diff_ms = i64::try_from(next_ms.saturating_sub(prev_ms)).unwrap_or(0);
        if diff_ms > 0 {
            self.unix_ms.fetch_add(diff_ms, Ordering::SeqCst);
        }
    }

    /// Explicitly sets the Unix wall-clock timestamp (simulating NTP / user clock skew)
    /// without violating monotonic clock monotonicity.
    pub fn set_unix_ms(&self, unix_ms: i64) {
        self.unix_ms.store(unix_ms, Ordering::SeqCst);
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new(1_764_094_000_000)
    }
}

impl Clock for FakeClock {
    fn now_unix_ms(&self) -> i64 {
        self.unix_ms.load(Ordering::SeqCst)
    }

    fn monotonic_ns(&self) -> u64 {
        self.mono_ns.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_is_monotonic_and_positive() {
        let clock = SystemClock::new();
        let t0 = clock.monotonic_ns();
        let w0 = clock.now_unix_ms();
        let t1 = clock.monotonic_ns();
        assert!(t1 >= t0);
        assert!(w0 > 1_700_000_000_000);
        assert!(SystemClock::default().monotonic_ms() <= 1_000);
    }

    #[test]
    fn fake_clock_advances_deterministically_and_isolates_wall_skew() {
        let clock = FakeClock::default();
        let start_unix = clock.now_unix_ms();
        assert_eq!(clock.monotonic_ns(), 0);

        clock.advance_ms(250);
        assert_eq!(clock.monotonic_ms(), 250);
        assert_eq!(clock.now_unix_ms(), start_unix + 250);

        clock.advance_ns(1_500_000);
        assert_eq!(clock.monotonic_ns(), 251_500_000);
        assert_eq!(clock.now_unix_ms(), start_unix + 251);

        clock.set_unix_ms(1_000);
        assert_eq!(clock.now_unix_ms(), 1_000);
        assert_eq!(clock.monotonic_ns(), 251_500_000);
    }
}
