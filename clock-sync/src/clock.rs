//! The synchronized virtual clock: a monotonic [`Instant`] anchored to the
//! system wall clock once at startup, plus an atomic offset published by the
//! sync service.
//!
//! Absolute timestamps carry their timebase in the type — [`LocalNs`] for the
//! uncorrected local timebase, [`SyncedNs`] for the synchronized clock — so
//! the two cannot be mixed. Spans (periods, delays, corrections) are plain
//! `i64` nanoseconds.

use std::{
    sync::atomic::{AtomicI64, Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

/// A point in time in the node's local timebase: unix-anchored ns advancing
/// at the local oscillator's rate, uncorrected. Created by [`SyncedClock`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct LocalNs(i64);

impl LocalNs {
    pub const fn from_ns(ns: i64) -> Self {
        Self(ns)
    }

    pub const fn ns(self) -> i64 {
        self.0
    }

    /// This instant shifted by a span.
    pub fn saturating_add(self, span_ns: i64) -> Self {
        Self(self.0.saturating_add(span_ns))
    }

    /// The span from `earlier` to `self`.
    pub fn saturating_sub(self, earlier: Self) -> i64 {
        self.0.saturating_sub(earlier.0)
    }
}

/// A point in time on the synchronized virtual clock, in unix ns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SyncedNs(i64);

impl SyncedNs {
    pub const fn ns(self) -> i64 {
        self.0
    }

    /// The span from `earlier` to `self`.
    pub fn saturating_sub(self, earlier: Self) -> i64 {
        self.0.saturating_sub(earlier.0)
    }
}

pub struct SyncedClock {
    /// Monotonic anchor; the local timebase is measured from here.
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

    /// A point in time expressed in the local timebase.
    pub fn local(&self, at: Instant) -> LocalNs {
        let elapsed = at.saturating_duration_since(self.base_instant).as_nanos() as i64;
        LocalNs(
            self.base_unix_ns
                .saturating_add(elapsed)
                .saturating_add(self.skew_ns),
        )
    }

    /// The current local-timebase time; see [`Self::local`].
    pub fn local_now(&self) -> LocalNs {
        self.local(Instant::now())
    }

    /// The current synchronized time: local timebase plus the cumulative
    /// correction.
    pub fn now(&self) -> SyncedNs {
        SyncedNs(
            self.local_now()
                .ns()
                .saturating_add(self.offset_ns.load(Ordering::Relaxed)),
        )
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
        self.now().ns().saturating_sub(system_now_ns)
    }
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    #[test]
    fn offset_shifts_now() {
        let clock = SyncedClock::new();
        let before = clock.now();
        clock.set_offset_ns(5_000_000_000);
        let after = clock.now();
        // 5s offset minus however long the two reads took (< 1s).
        assert!(after.saturating_sub(before) > 4_000_000_000);
        assert!(after.saturating_sub(before) < 6_000_000_000);
    }

    #[test]
    fn skew_shifts_local_timebase() {
        let a = SyncedClock::with_skew(0);
        let b = SyncedClock::with_skew(250_000_000);
        let at = Instant::now();
        let delta = b.local(at).saturating_sub(a.local(at));
        // Both clocks read the wall clock at construction (< 100ms apart).
        assert!((150_000_000..350_000_000).contains(&delta));
    }

    #[test]
    fn local_is_monotone_in_instant() {
        let clock = SyncedClock::new();
        let t0 = Instant::now();
        let t1 = t0 + std::time::Duration::from_millis(10);
        assert_eq!(clock.local(t1).saturating_sub(clock.local(t0)), 10_000_000);
    }
}
