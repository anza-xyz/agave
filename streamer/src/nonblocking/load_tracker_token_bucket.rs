use std::{
    sync::atomic::{AtomicI64, AtomicU64, Ordering},
    time::{Duration, Instant},
};

/// Global token-bucket load estimator.
///
/// Connections consume tokens via [`acquire`]. The system is considered
/// saturated when the bucket level drops below `burst_capacity / 10`.
///
/// Refills are driven by [`acquire`]: when the level drops below half
/// capacity, a time-proportional refill is attempted, capped at
/// `burst_capacity`.
///
/// NOTE: This is intentionally not a generic rate limiter. The bucket can go
/// negative to represent debt after bursts, which keeps the system saturated
/// longer and helps protect downstream pipeline capacity during slot spikes.
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

    /// Consume one token. Triggers a refill attempt when the bucket
    /// drops below `burst_capacity / 10`.
    pub(crate) fn acquire(&self) {
        let prev = self.bucket.fetch_sub(1, Ordering::Relaxed);
        if prev - 1 < self.burst_capacity / 10 {
            self.try_refill();
        }
    }

    /// Return whether the system is saturated.
    ///
    /// The system is saturated when the bucket level is below
    /// `burst_capacity / 10`. When already below that threshold,
    /// a refill is attempted so parked connections can detect recovery
    /// even when no streams are flowing.
    pub fn is_saturated(&self) -> bool {
        let level = self.bucket.load(Ordering::Relaxed);
        if level < self.burst_capacity / 10 {
            self.try_refill();
            self.bucket.load(Ordering::Relaxed) < self.burst_capacity / 10
        } else {
            false
        }
    }

    /// Return the current bucket level.
    pub fn bucket_level(&self) -> i64 {
        self.bucket.load(Ordering::Relaxed)
    }

    fn try_refill(&self) {
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

    // 100 tokens/s, burst=100, refill every 10ms (= 1 token per refill).
    fn simple() -> GlobalLoadTrackerTokenBucket {
        GlobalLoadTrackerTokenBucket::new(100, 100, Duration::from_millis(10))
    }

    fn acquire_n(g: &GlobalLoadTrackerTokenBucket, n: u64) {
        for _ in 0..n {
            g.acquire();
        }
    }

    #[test]
    fn test_starts_not_saturated() {
        let g = simple();
        assert_eq!(g.bucket_level(), 100);
        assert!(!g.is_saturated()); // 100 >= 100/10
    }

    #[test]
    fn test_acquire_decrements() {
        let g = simple();
        acquire_n(&g, 10);
        assert_eq!(g.bucket_level(), 90);
    }

    #[test]
    fn test_goes_negative() {
        let g = simple();
        acquire_n(&g, 150);
        assert_eq!(g.bucket_level(), -50);
    }

    #[test]
    fn test_saturated_below_threshold() {
        let g = simple(); // threshold = 100/10 = 10
        acquire_n(&g, 95); // level = 5 < 10
        assert!(g.is_saturated());
    }

    #[test]
    fn test_not_saturated_at_threshold() {
        let g = simple(); // threshold = 10
        acquire_n(&g, 90); // level = 10, not < 10
        assert!(!g.is_saturated());
    }

    #[test]
    fn test_refill_adds_tokens() {
        let g = simple(); // 100/s, refill interval 10ms
        acquire_n(&g, 100); // level = 0
        // 50ms elapsed at 100/s → refill = 5 tokens
        g.refill_at(50_000_000);
        assert_eq!(g.bucket_level(), 5);
    }

    #[test]
    fn test_refill_from_negative() {
        let g = simple();
        acquire_n(&g, 120); // level = -20
        assert_eq!(g.bucket_level(), -20);
        // 500ms at 100/s → refill = 50
        g.refill_at(500_000_000);
        assert_eq!(g.bucket_level(), 30); // -20 + 50
    }

    #[test]
    fn test_refill_caps_at_burst() {
        let g = simple(); // burst = 100, starts at 100
        // Don't consume anything. Refill after 10s → would add 1000.
        g.refill_at(10_000_000_000);
        assert_eq!(g.bucket_level(), 100); // capped
    }

    #[test]
    fn test_refill_skipped_before_interval() {
        let g = simple(); // interval = 10ms
        acquire_n(&g, 50); // level = 50
        g.refill_at(5_000_000); // 5ms < 10ms interval → no refill
        assert_eq!(g.bucket_level(), 50);
    }

    #[test]
    fn test_recovery_through_refill() {
        let g = simple(); // threshold = 10
        acquire_n(&g, 100); // level = 0, saturated
        assert!(g.is_saturated());

        // Refill 15 tokens (150ms at 100/s) → level = 15 >= 10
        g.refill_at(150_000_000);
        assert!(!g.is_saturated());
    }
}
