use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

/// Global token-bucket load estimator.
///
/// Each stream increments a global counter. Any thread may periodically
/// update the bucket by sampling the counter over elapsed time. The bucket
/// level is measured in "streams".
///
/// This is a deliberate accuracy/efficiency trade-off: updates are batched
/// to avoid per-stream time calls, which means saturation detection can be
/// delayed by up to the update interval.
pub(crate) struct GlobalLoadTrackerTokenBucket {
    total_streams: AtomicU64,
    /// High bit is used as a lock to serialize updates; remaining bits store nanos.
    last_sample_nanos: AtomicU64,
    last_sample_total: AtomicU64,
    bucket_level_bits: AtomicU64,
    epoch: Instant,
    update_interval_nanos: u64,
    max_streams_per_second: u64,
    burst_capacity: u64,
}

impl GlobalLoadTrackerTokenBucket {
    pub(crate) fn new(
        max_streams_per_second: u64,
        burst_capacity: u64,
        update_interval: Duration,
    ) -> Self {
        assert!(update_interval.as_nanos() > 0, "update_interval must be > 0");
        Self {
            total_streams: AtomicU64::new(0),
            last_sample_nanos: AtomicU64::new(0),
            last_sample_total: AtomicU64::new(0),
            bucket_level_bits: AtomicU64::new((burst_capacity as f64).to_bits()),
            epoch: Instant::now(),
            update_interval_nanos: update_interval.as_nanos() as u64,
            max_streams_per_second,
            burst_capacity,
        }
    }

    /// Increment the global stream counter.
    pub(crate) fn record_stream(&self) {
        self.total_streams.fetch_add(1, Ordering::Relaxed);
    }

    /// Return whether the system appears saturated (bucket level <= 0).
    pub(crate) fn is_saturated(&self) -> bool {
        let now_nanos = self.nanos_since_epoch();
        self.update_if_needed_at(now_nanos);
        self.bucket_level() <= 0.0
    }

    /// Return the current bucket level.
    pub(crate) fn bucket_level(&self) -> f64 {
        f64::from_bits(self.bucket_level_bits.load(Ordering::Relaxed))
    }

    pub(crate) fn burst_capacity(&self) -> u64 {
        self.burst_capacity
    }

    fn nanos_since_epoch(&self) -> u64 {
        self.epoch.elapsed().as_nanos() as u64
    }

    fn update_if_needed_at(&self, now_nanos: u64) {
        // Serialize updates with a lock bit in last_sample_nanos to avoid
        // overlapping updates that can overwrite newer bucket calculations.
        const LOCK_BIT: u64 = 1 << 63;
        const NANO_MASK: u64 = !LOCK_BIT;

        let raw = self.last_sample_nanos.load(Ordering::Relaxed);
        if raw & LOCK_BIT != 0 {
            return;
        }
        let last_nanos = raw & NANO_MASK;
        if now_nanos <= last_nanos {
            return;
        }
        let elapsed_nanos = now_nanos - last_nanos;
        if elapsed_nanos < self.update_interval_nanos {
            return;
        }

        if self
            .last_sample_nanos
            .compare_exchange(
                raw,
                raw | LOCK_BIT,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_err()
        {
            return;
        }

        let total = self.total_streams.load(Ordering::Relaxed);
        let prev_total = self.last_sample_total.swap(total, Ordering::Relaxed);
        let consumed = total.saturating_sub(prev_total) as f64;

        let dt_secs =
            now_nanos.saturating_sub(last_nanos) as f64 / 1_000_000_000.0;
        let refill = self.max_streams_per_second as f64 * dt_secs;

        let current = self.bucket_level();
        let mut level = current + refill - consumed;
        let capacity = self.burst_capacity as f64;
        if level > capacity {
            level = capacity;
        }
        self.bucket_level_bits
            .store(level.to_bits(), Ordering::Relaxed);
        self.last_sample_nanos
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
                g.record_stream();
            }
            now_nanos = now_nanos.saturating_add(interval_nanos);
            g.update_if_needed_at(now_nanos);
        }

        now_nanos
    }

    #[test]
    fn test_bucket_full_when_under_capacity() {
        let interval_ms = 5;
        let g = make_tracker(1_000_000, 50_000, interval_ms);
        let interval_nanos = interval_ms * 1_000_000;

        let end = push_streams(&g, 50_000, 0, 500_000, interval_nanos);
        g.update_if_needed_at(end);

        assert!(
            (g.bucket_level() - g.burst_capacity() as f64).abs() < 100.0,
            "bucket_level={} expected near capacity",
            g.bucket_level()
        );
    }

    #[test]
    fn test_bucket_drains_when_over_capacity() {
        let interval_ms = 5;
        let g = make_tracker(1_000_000, 50_000, interval_ms);
        let interval_nanos = interval_ms * 1_000_000;

        let end = push_streams(&g, 120_000, 0, 2_000_000, interval_nanos);
        g.update_if_needed_at(end);

        assert!(
            g.bucket_level() <= 0.0,
            "should be saturated, level={}",
            g.bucket_level()
        );
    }

    #[test]
    fn test_burst_tolerance() {
        let interval_ms = 5;
        let g = make_tracker(1_000_000, 50_000, interval_ms);
        let interval_nanos = interval_ms * 1_000_000;

        let end = push_streams(&g, 90_000, 0, 2_000_000, interval_nanos);
        g.update_if_needed_at(end);
        assert!(
            g.bucket_level() > 0.0,
            "should not be saturated at ~45ms, level={}",
            g.bucket_level()
        );

        let end = push_streams(&g, 20_000, end, 2_000_000, interval_nanos);
        g.update_if_needed_at(end);
        assert!(
            g.bucket_level() <= 0.0,
            "should be saturated at ~55ms, level={}",
            g.bucket_level()
        );
    }

    #[test]
    fn test_recovery_after_saturation() {
        let interval_ms = 5;
        let g = make_tracker(1_000_000, 50_000, interval_ms);
        let interval_nanos = interval_ms * 1_000_000;

        let end = push_streams(&g, 120_000, 0, 2_000_000, interval_nanos);
        g.update_if_needed_at(end);
        let deficit = -g.bucket_level();
        assert!(deficit > 0.0);

        let recovery_ms = (deficit / 1_000_000.0 * 1000.0).ceil() as u64 + 1;
        let recovery_nanos = end + recovery_ms * 1_000_000;
        g.update_if_needed_at(recovery_nanos);

        assert!(
            g.bucket_level() > 0.0,
            "should recover after {recovery_ms}ms, level={}",
            g.bucket_level()
        );
    }
}
