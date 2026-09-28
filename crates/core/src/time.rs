//! Clock injection, so state machines advance without waiting on wall-clock time.

use jiff::{SignedDuration, Timestamp};
use std::sync::Mutex;
use std::time::Duration;

/// Nanoseconds in one second.
const NANOS_PER_SECOND: u128 = 1_000_000_000;

/// Supplies the current time to logic that needs to stamp or schedule.
///
/// Injected everywhere rather than read from the system, so tests can drive a state machine
/// through hours of behaviour instantly and deterministically.
pub trait Clock: Send + Sync + 'static {
    /// Returns the current wall-clock time.
    fn now(&self) -> Timestamp;
}

/// Reads the real system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

/// A clock that only moves when a test moves it.
pub struct TestClock {
    now: Mutex<Timestamp>,
}

impl TestClock {
    /// Creates a clock fixed at the given instant.
    pub fn new(start: Timestamp) -> Self {
        Self {
            now: Mutex::new(start),
        }
    }

    /// Creates a clock fixed at the Unix epoch, which is enough for most tests.
    pub fn at_epoch() -> Self {
        Self::new(Timestamp::UNIX_EPOCH)
    }

    /// Moves the clock forward by the given amount.
    pub fn advance(&self, by: Duration) {
        let step = SignedDuration::try_from(by).unwrap_or(SignedDuration::MAX);
        if let Ok(mut guard) = self.now.lock() {
            *guard = guard.checked_add(step).unwrap_or(Timestamp::MAX);
        }
    }
}

impl Default for TestClock {
    fn default() -> Self {
        Self::at_epoch()
    }
}

impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        // A poisoned lock means a test panicked while holding it. Reporting the epoch keeps the
        // failing test's output readable instead of cascading a second panic.
        self.now.lock().map(|t| *t).unwrap_or(Timestamp::UNIX_EPOCH)
    }
}

/// Adds a duration to a timestamp, saturating at the representable maximum.
pub fn saturating_add(at: Timestamp, delta: Duration) -> Timestamp {
    let step = SignedDuration::try_from(delta).unwrap_or(SignedDuration::MAX);
    at.checked_add(step).unwrap_or(Timestamp::MAX)
}

/// Returns how long has elapsed between two timestamps, or zero if `later` precedes `earlier`.
pub fn elapsed(earlier: Timestamp, later: Timestamp) -> Duration {
    let delta = later
        .as_nanosecond()
        .saturating_sub(earlier.as_nanosecond());
    if delta <= 0 {
        return Duration::ZERO;
    }
    let nanos = u128::try_from(delta).unwrap_or(0);
    // Splitting nanoseconds into whole seconds and a remainder is exactly what integer division
    // is for here; the divisor is a constant and cannot be zero.
    #[allow(clippy::integer_division)]
    let secs = u64::try_from(nanos / NANOS_PER_SECOND).unwrap_or(u64::MAX);
    let rem = u32::try_from(nanos % NANOS_PER_SECOND).unwrap_or(0);
    Duration::new(secs, rem)
}
