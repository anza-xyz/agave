//! The synchronized virtual clock: a monotonic [`Instant`] anchored to the
//! system wall clock once at startup, plus an atomic offset published by the
//! sync service.

use std::{
    sync::atomic::{AtomicI64, Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

pub struct SyncedClock {
    /// Monotonic anchor; `local_ns` is measured from here.
    base_instant: Instant,
    /// Wall-clock time of `base_instant`, unix ns.
    base_unix_ns: i64,
    /// Cumulative correction published by the sync service.
    offset_ns: AtomicI64,
    /// Test-only artificial skew, folded into the local timebase so it
    /// shifts arrival stamps and pulse scheduling alike.
    skew_ns: i64,
}

impl SyncedClock {
    pub fn new() -> Self {
        Self::with_skew(0)
    }

    /// A clock whose local timebase is shifted by `skew_ns`. Intended for
    /// tests that simulate desynchronized validators.
    pub fn with_skew(skew_ns: i64) -> Self {
        let base_instant = Instant::now();
        let base_unix_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is set after 1970")
            .as_nanos() as i64;
        Self {
            base_instant,
            base_unix_ns,
            offset_ns: AtomicI64::new(0),
            skew_ns,
        }
    }

    /// A point in time expressed in the local timebase: ns of unix time,
    /// advancing at the local oscillator's rate, uncorrected.
    pub fn local_ns(&self, at: Instant) -> i64 {
        let elapsed = at.saturating_duration_since(self.base_instant).as_nanos() as i64;
        self.base_unix_ns
            .saturating_add(elapsed)
            .saturating_add(self.skew_ns)
    }

    /// The current local-timebase time; see [`Self::local_ns`].
    pub fn local_now_ns(&self) -> i64 {
        self.local_ns(Instant::now())
    }

    /// The synchronized time: local timebase plus the cumulative correction.
    pub fn now_ns(&self) -> i64 {
        self.local_now_ns()
            .saturating_add(self.offset_ns.load(Ordering::Relaxed))
    }

    /// Publish the cumulative correction (the sync service's
    /// `cumulative_offset_ns` after each round close).
    pub fn set_offset_ns(&self, offset_ns: i64) {
        self.offset_ns.store(offset_ns, Ordering::Relaxed);
    }

    /// How far the synchronized clock has diverged from the system's wall
    /// clock.
    pub fn offset_vs_system_ns(&self) -> i64 {
        let system_now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is set after 1970")
            .as_nanos() as i64;
        self.now_ns().saturating_sub(system_now_ns)
    }
}

impl Default for SyncedClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    #[test]
    fn offset_shifts_now() {
        let clock = SyncedClock::new();
        let before = clock.now_ns();
        clock.set_offset_ns(5_000_000_000);
        let after = clock.now_ns();
        // 5s offset minus however long the two reads took (< 1s).
        assert!((after - before) > 4_000_000_000);
        assert!((after - before) < 6_000_000_000);
    }

    #[test]
    fn skew_shifts_local_timebase() {
        let a = SyncedClock::with_skew(0);
        let b = SyncedClock::with_skew(250_000_000);
        let at = Instant::now();
        let delta = b.local_ns(at) - a.local_ns(at);
        // Both clocks read the wall clock at construction (< 100ms apart).
        assert!((150_000_000..350_000_000).contains(&delta));
    }

    #[test]
    fn local_ns_is_monotone_in_instant() {
        let clock = SyncedClock::new();
        let t0 = Instant::now();
        let t1 = t0 + std::time::Duration::from_millis(10);
        assert_eq!(clock.local_ns(t1) - clock.local_ns(t0), 10_000_000);
    }
}
