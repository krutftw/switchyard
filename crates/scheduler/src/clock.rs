//! Time source.
//!
//! Selection and outcome reporting take the current time as an explicit
//! [`SystemTime`] so tests can drive cooldowns deterministically. The
//! scheduler also owns a [`Clock`] for the introspection calls that have no
//! time parameter and as the recommended source of that explicit time
//! ([`crate::Scheduler::now`]).

use parking_lot::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Milliseconds since the unix epoch; the scheduler's internal time unit.
pub(crate) type Ms = i64;

/// A source of the current time.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> SystemTime;
}

/// The production clock: the system's wall-clock time.
///
/// Callers are free to pass `SystemTime::now()` to `pick` / `report`
/// themselves; this clock reads the very same source, so the explicit times
/// and the introspection calls always agree. A step of the system clock
/// (NTP correction, manual change) shifts running cooldowns by the size of
/// the step and nothing else.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl SystemClock {
    pub fn new() -> Self {
        SystemClock
    }
}

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A clock that only moves when told to. For tests.
#[derive(Debug)]
pub struct ManualClock {
    now: Mutex<SystemTime>,
}

impl ManualClock {
    pub fn new(start: SystemTime) -> Self {
        ManualClock {
            now: Mutex::new(start),
        }
    }

    /// A clock starting `secs` seconds after the unix epoch.
    pub fn at_unix_secs(secs: u64) -> Self {
        ManualClock::new(UNIX_EPOCH + Duration::from_secs(secs))
    }

    /// Moves the clock forward.
    pub fn advance(&self, by: Duration) {
        let mut now = self.now.lock();
        *now = now.checked_add(by).unwrap_or(*now);
    }

    pub fn set(&self, to: SystemTime) {
        *self.now.lock() = to;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        *self.now.lock()
    }
}

/// Converts to unix milliseconds. Times before the epoch clamp to zero.
pub(crate) fn unix_ms(time: SystemTime) -> Ms {
    time.duration_since(UNIX_EPOCH)
        .map(|d| Ms::try_from(d.as_millis()).unwrap_or(Ms::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_moves_only_on_request() {
        let clock = ManualClock::at_unix_secs(1_000);
        assert_eq!(unix_ms(clock.now()), 1_000_000);
        clock.advance(Duration::from_millis(1_500));
        assert_eq!(unix_ms(clock.now()), 1_001_500);
        clock.set(UNIX_EPOCH + Duration::from_secs(5));
        assert_eq!(unix_ms(clock.now()), 5_000);
    }

    #[test]
    fn system_clock_reads_wall_time() {
        let before = unix_ms(SystemTime::now());
        let now = unix_ms(SystemClock::new().now());
        let after = unix_ms(SystemTime::now());
        // Allow for a clock step between the three readings.
        assert!(now >= before - 60_000 && now <= after + 60_000);
    }

    #[test]
    fn pre_epoch_times_clamp_to_zero() {
        assert_eq!(unix_ms(UNIX_EPOCH - Duration::from_secs(10)), 0);
    }
}
