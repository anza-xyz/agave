use std::{
    sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    time::{Duration, Instant},
};

/// Global token-bucket load estimator.
///
/// Connections consume tokens directly via [`acquire`]. When the bucket
/// is empty the call returns `false` and the `saturated` flag is set.
/// The flag clears only after a periodic refill brings the bucket above
/// half its burst capacity, providing hysteresis against oscillation.
///
/// A periodic refill step (driven opportunistically by any caller of
/// [`is_saturated`] or explicitly via [`try_refill`]) adds tokens
/// proportional to elapsed time, capped at `burst_capacity`.
pub struct GlobalLoadTrackerTokenBucket {
    /// Current token count. Connections decrement; refill increments.
    bucket: AtomicI64,
    /// Sticky flag: set on failed acquire, cleared on recovery.
    saturated: AtomicBool,
    /// Nanos since epoch of the last refill. High bit is a lock.
    last_refill_nanos: AtomicU64,
    epoch: Instant,
    refill_interval_nanos: u64,
    max_streams_per_second: u64,
    burst_capacity: i64,
}

impl GlobalLoadTrackerTokenBucket {
    pub(crate) fn new(
        max_streams_per_second: u64,
        burst_capacity: u64,
        refill_interval: Duration,
    ) -> Self {
        assert!(refill_interval.as_nanos() > 0, "refill_interval must be > 0");
        Self {
            bucket: AtomicI64::new(burst_capacity as i64),
            saturated: AtomicBool::new(false),
            last_refill_nanos: AtomicU64::new(0),
            epoch: Instant::now(),
            refill_interval_nanos: refill_interval.as_nanos() as u64,
            max_streams_per_second,
            burst_capacity: burst_capacity as i64,
        }
    }

    /// Consume one token. Returns `true` if a token was available
    /// (not saturated), `false` if the bucket is empty.
    ///
    /// On failure the `saturated` flag is set; it remains set until a
    /// refill brings the bucket above the recovery threshold.
    pub(crate) fn acquire(&self) -> bool {
        let prev = self.bucket.fetch_sub(1, Ordering::Relaxed);
        if prev > 0 {
            true
        } else {
            // Bucket was already at 0 or below — undo the decrement.
            self.bucket.fetch_add(1, Ordering::Relaxed);
            self.saturated.store(true, Ordering::Relaxed);
            false
        }
    }

    /// Return whether the system is saturated.
    ///
    /// The flag is sticky: it is set the moment an [`acquire`] fails and
    /// cleared only when a refill brings the bucket above half of
    /// `burst_capacity`. This hysteresis prevents rapid oscillation at
    /// the saturation boundary.
    pub fn is_saturated(&self) -> bool {
        self.try_refill();
        self.saturated.load(Ordering::Relaxed)
    }

    /// Return the current bucket level.
    pub fn bucket_level(&self) -> i64 {
        self.bucket.load(Ordering::Relaxed)
    }

    /// Attempt to refill the bucket if enough time has elapsed.
    pub(crate) fn try_refill(&self) {
        let now_nanos = self.nanos_since_epoch();
        self.refill_at(now_nanos);
    }

    fn nanos_since_epoch(&self) -> u64 {
        self.epoch.elapsed().as_nanos() as u64
    }

    fn refill_at(&self, now_nanos: u64) {
        const LOCK_BIT: u64 = 1 << 63;
        const NANO_MASK: u64 = !LOCK_BIT;

        let raw = self.last_refill_nanos.load(Ordering::Relaxed);
        if raw & LOCK_BIT != 0 {
            return;
        }
        let last_nanos = raw & NANO_MASK;
        if now_nanos <= last_nanos {
            return;
        }
        let elapsed_nanos = now_nanos - last_nanos;
        if elapsed_nanos < self.refill_interval_nanos {
            return;
        }

        // Try to acquire the refill lock.
        if self
            .last_refill_nanos
            .compare_exchange(raw, raw | LOCK_BIT, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        let dt_secs = elapsed_nanos as f64 / 1_000_000_000.0;
        let refill = (self.max_streams_per_second as f64 * dt_secs) as i64;

        // Atomic add composes correctly with concurrent acquire() calls.
        // No CAS loop needed — the refill lock ensures we're the only refiller.
        self.bucket.fetch_add(refill, Ordering::Relaxed);

        // Cap at burst_capacity. A concurrent acquire() may slip in between
        // the load and store, losing at most 1 token — negligible.
        let level = self.bucket.load(Ordering::Relaxed);
        if level > self.burst_capacity {
            self.bucket.store(self.burst_capacity, Ordering::Relaxed);
        }

        // Clear saturation flag once bucket is above recovery threshold.
        if level > self.burst_capacity / 2 {
            self.saturated.store(false, Ordering::Relaxed);
        }

        // Release lock and store new timestamp.
        self.last_refill_nanos
            .store(now_nanos & NANO_MASK, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tracker(
        max_tps: u64,
        burst_capacity: u64,
        interval_ms: u64,
    ) -> GlobalLoadTrackerTokenBucket {
        GlobalLoadTrackerTokenBucket::new(
            max_tps,
            burst_capacity,
            Duration::from_millis(interval_ms),
        )
    }

    /// Consume `count` tokens at `rate_per_sec`, triggering refills at
    /// each `interval_nanos` boundary. Returns (final_nanos, num_failed).
    fn push_streams(
        g: &GlobalLoadTrackerTokenBucket,
        count: u64,
        start_nanos: u64,
        rate_per_sec: u64,
        interval_nanos: u64,
    ) -> (u64, u64) {
        let mut now_nanos = start_nanos;
        let per_interval = rate_per_sec.saturating_mul(interval_nanos) / 1_000_000_000;
        let intervals = count / per_interval.max(1);
        let mut failed = 0u64;

        for _ in 0..intervals {
            for _ in 0..per_interval {
                if !g.acquire() {
                    failed += 1;
                }
            }
            now_nanos = now_nanos.saturating_add(interval_nanos);
            g.refill_at(now_nanos);
        }

        (now_nanos, failed)
    }

    #[test]
    fn test_bucket_full_when_under_capacity() {
        let interval_ms = 5;
        let g = make_tracker(1_000_000, 50_000, interval_ms);
        let interval_nanos = interval_ms * 1_000_000;

        let (end, failed) = push_streams(&g, 50_000, 0, 500_000, interval_nanos);
        g.refill_at(end);

        assert_eq!(failed, 0, "no acquires should fail under capacity");
        assert!(
            (g.bucket_level() - 50_000).abs() < 100,
            "bucket_level={} expected near capacity",
            g.bucket_level()
        );
        assert!(!g.saturated.load(Ordering::Relaxed));
    }

    #[test]
    fn test_bucket_drains_when_over_capacity() {
        let interval_ms = 5;
        let g = make_tracker(1_000_000, 50_000, interval_ms);
        let interval_nanos = interval_ms * 1_000_000;

        let (_end, failed) = push_streams(&g, 120_000, 0, 2_000_000, interval_nanos);

        assert!(failed > 0, "some acquires should fail over capacity");
        assert!(
            g.saturated.load(Ordering::Relaxed),
            "should be saturated after overload"
        );
    }

    #[test]
    fn test_burst_tolerance() {
        let interval_ms = 5;
        let g = make_tracker(1_000_000, 50_000, interval_ms);
        let interval_nanos = interval_ms * 1_000_000;

        // 90K streams at 2M/s with 1M/s capacity + 50K burst.
        // Net drain per 5ms interval: 10K consumed - 5K refilled = 5K.
        // After 9 intervals (90K streams): bucket ≈ 50K - 9*5K = 5K.
        // All acquires should succeed (burst absorbs the excess).
        let (end, failed) = push_streams(&g, 90_000, 0, 2_000_000, interval_nanos);
        g.refill_at(end);
        assert_eq!(failed, 0, "burst should absorb first 90K");
        assert!(
            g.bucket_level() > 0,
            "should not be drained at ~45ms, level={}",
            g.bucket_level()
        );

        // 20K more: bucket ≈ 5K, needs 10K per interval → acquires fail.
        let (_end, failed) = push_streams(&g, 20_000, end, 2_000_000, interval_nanos);
        assert!(failed > 0, "should see failures after burst exhausted");
        assert!(
            g.saturated.load(Ordering::Relaxed),
            "should be saturated at ~55ms"
        );
    }

    #[test]
    fn test_acquire_returns_false_when_empty() {
        let g = make_tracker(1_000_000, 10, 5);

        for _ in 0..10 {
            assert!(g.acquire(), "should succeed while tokens remain");
        }
        assert!(!g.acquire(), "should fail when bucket is empty");
        assert_eq!(g.bucket_level(), 0);
    }

    #[test]
    fn test_recovery_after_saturation() {
        let interval_ms = 5;
        let g = make_tracker(1_000_000, 50_000, interval_ms);
        let interval_nanos = interval_ms * 1_000_000;

        let (end, _) = push_streams(&g, 120_000, 0, 2_000_000, interval_nanos);
        assert!(g.saturated.load(Ordering::Relaxed), "should be saturated");

        // Refill without consuming. Need bucket > burst_capacity/2 = 25,000
        // to clear the flag. At 1M/s, each 5ms refill adds 5,000 tokens.
        // After 5 intervals (25ms): bucket = 5*5000 = 25,000.
        // After 6 intervals: bucket = 30,000 > 25,000 → flag clears.
        let mut nanos = end;
        for _ in 0..6 {
            nanos += interval_nanos;
            g.refill_at(nanos);
        }

        assert!(
            g.bucket_level() > g.burst_capacity / 2,
            "bucket should have recovered, level={}",
            g.bucket_level()
        );
        assert!(
            !g.saturated.load(Ordering::Relaxed),
            "saturated flag should be cleared after recovery"
        );
    }

    #[test]
    fn test_saturation_flag_sticky_during_overload() {
        // Even though refills add tokens, the flag stays set as long as
        // the bucket stays below the recovery threshold (burst_capacity/2).
        //
        // burst=4000, threshold=2000. Refill per 5ms at 100K/s = 500 tokens.
        // After push_streams at 2x capacity, last refill leaves bucket ≈ 500.
        // Need 3 more refills (1500 more) to reach 2000. 4th gets past it.
        let g = make_tracker(100_000, 4000, 5); // 100K/s, burst=4000
        let interval_nanos = 5 * 1_000_000;

        // Drain at 200K/s → saturate. Per interval: 1000 consumed, 500 refilled.
        // Net -500/interval. Bucket: 4000 → 3500 → ... → 500 after 7 intervals.
        // Interval 8: 500 tokens, 1000 acquires → 500 succeed, 500 fail → saturated.
        let (end, failed) = push_streams(&g, 10_000, 0, 200_000, interval_nanos);
        assert!(failed > 0);
        assert!(g.saturated.load(Ordering::Relaxed));

        // After push_streams, bucket ≈ 500 (from last refill).
        // Each refill adds 500. Threshold = 4000/2 = 2000.
        // +500 → 1000 (still < 2000)
        g.refill_at(end + interval_nanos);
        assert!(
            g.saturated.load(Ordering::Relaxed),
            "flag should still be set: bucket={} threshold={}",
            g.bucket_level(),
            g.burst_capacity / 2,
        );

        // +500 → 1500 (still < 2000)
        g.refill_at(end + 2 * interval_nanos);
        assert!(
            g.saturated.load(Ordering::Relaxed),
            "flag should still be set: bucket={}",
            g.bucket_level(),
        );

        // +500 → 2000 (still not > 2000)
        g.refill_at(end + 3 * interval_nanos);
        assert!(
            g.saturated.load(Ordering::Relaxed),
            "flag should still be set at boundary: bucket={}",
            g.bucket_level(),
        );

        // +500 → 2500 > 2000 → flag clears.
        g.refill_at(end + 4 * interval_nanos);
        assert!(
            !g.saturated.load(Ordering::Relaxed),
            "flag should clear: bucket={}",
            g.bucket_level(),
        );
    }
}
