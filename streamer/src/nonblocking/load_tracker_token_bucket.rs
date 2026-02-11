use std::{
    sync::atomic::{AtomicI64, AtomicU64, Ordering},
    time::{Duration, Instant},
};

/// Global token-bucket load estimator.
///
/// Connections consume tokens via [`acquire`]. The system is considered
/// saturated when the bucket level drops below `burst_capacity / 10`.
///
/// A periodic refill step (driven opportunistically by any caller of
/// [`is_saturated`] or explicitly via [`try_refill`]) adds tokens
/// proportional to elapsed time, capped at `burst_capacity`.
pub struct GlobalLoadTrackerTokenBucket {
    /// Current token count. Connections decrement; refill increments.
    bucket: AtomicI64,
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
            last_refill_nanos: AtomicU64::new(0),
            epoch: Instant::now(),
            refill_interval_nanos: refill_interval.as_nanos() as u64,
            max_streams_per_second,
            burst_capacity: burst_capacity as i64,
        }
    }

    /// Consume one token.
    pub(crate) fn acquire(&self) {
        self.bucket.fetch_sub(1, Ordering::Relaxed);
    }

    /// Return whether the system is saturated.
    ///
    /// The system is saturated when the bucket level drops below
    /// `burst_capacity / 10`.
    pub fn is_saturated(&self) -> bool {
        self.try_refill();
        self.bucket.load(Ordering::Relaxed) < self.burst_capacity / 10
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
    /// each `interval_nanos` boundary. Returns final_nanos.
    fn push_streams(
        g: &GlobalLoadTrackerTokenBucket,
        count: u64,
        start_nanos: u64,
        rate_per_sec: u64,
        interval_nanos: u64,
    ) -> u64 {
        let mut now_nanos = start_nanos;
        let per_interval = rate_per_sec.saturating_mul(interval_nanos) / 1_000_000_000;
        let intervals = count / per_interval.max(1);

        for _ in 0..intervals {
            for _ in 0..per_interval {
                g.acquire();
            }
            now_nanos = now_nanos.saturating_add(interval_nanos);
            g.refill_at(now_nanos);
        }

        now_nanos
    }

    #[test]
    fn test_bucket_full_when_under_capacity() {
        let interval_ms = 5;
        let g = make_tracker(1_000_000, 50_000, interval_ms);
        let interval_nanos = interval_ms * 1_000_000;

        let end = push_streams(&g, 50_000, 0, 500_000, interval_nanos);
        g.refill_at(end);

        assert!(
            (g.bucket_level() - 50_000).abs() < 100,
            "bucket_level={} expected near capacity",
            g.bucket_level()
        );
        assert!(!g.is_saturated());
    }

    #[test]
    fn test_saturated_when_over_capacity() {
        let interval_ms = 5;
        let g = make_tracker(1_000_000, 50_000, interval_ms);
        let interval_nanos = interval_ms * 1_000_000;

        push_streams(&g, 120_000, 0, 2_000_000, interval_nanos);

        assert!(
            g.bucket_level() < g.burst_capacity / 10,
            "bucket should be below threshold, level={}",
            g.bucket_level()
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
        // Threshold = 50K/10 = 5K. Bucket stays at or just above threshold.
        let end = push_streams(&g, 90_000, 0, 2_000_000, interval_nanos);
        g.refill_at(end);
        assert!(
            g.bucket_level() > 0,
            "should not be fully drained at ~45ms, level={}",
            g.bucket_level()
        );

        // 20K more: bucket drains well below threshold → saturated.
        push_streams(&g, 20_000, end, 2_000_000, interval_nanos);
        assert!(
            g.bucket_level() < g.burst_capacity / 10,
            "should be below threshold at ~55ms, level={}",
            g.bucket_level()
        );
    }

    #[test]
    fn test_bucket_goes_negative() {
        let g = make_tracker(1_000_000, 10, 5);

        for _ in 0..20 {
            g.acquire();
        }
        // Bucket can go negative — that's fine
        assert_eq!(g.bucket_level(), -10);
    }

    #[test]
    fn test_recovery_after_saturation() {
        let interval_ms = 5;
        let g = make_tracker(1_000_000, 50_000, interval_ms);
        let interval_nanos = interval_ms * 1_000_000;

        let end = push_streams(&g, 120_000, 0, 2_000_000, interval_nanos);
        assert!(g.bucket_level() < g.burst_capacity / 10, "should be saturated");

        // Refill without consuming. At 1M/s, each 5ms refill adds 5,000 tokens.
        // Threshold = 50K/10 = 5K. Bucket starts near 0 or negative.
        // After a couple of refills it should recover above 5K.
        let mut nanos = end;
        for _ in 0..3 {
            nanos += interval_nanos;
            g.refill_at(nanos);
        }

        assert!(
            g.bucket_level() >= g.burst_capacity / 10,
            "bucket should have recovered above threshold, level={}",
            g.bucket_level()
        );
    }
}
