use std::{
    sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    time::{Duration, Instant},
};

/// Global load-debt estimator.
///
/// Connections consume tokens via [`acquire`]; time-proportional refills
/// happen lazily on each acquire when the bucket is empty or in debt.
///
/// The bucket intentionally goes negative to represent debt, keeping the
/// system saturated longer — QUIC flow control credit already issued
/// cannot be revoked, so we are conservative about recovery.
///
/// Hysteresis: saturated when bucket ≤ 0, recovered when bucket reaches
/// 90% of capacity. The wide band prevents oscillation near the boundary.
///
pub struct LoadDebtTracker {
    bucket: AtomicI64,
    /// High bit is a lock.
    last_refill_nanos: AtomicU64,
    was_saturated: AtomicBool,
    epoch: Instant,
    refill_interval_nanos: u64,
    max_streams_per_second: u64,
    burst_capacity: i64,
    /// 90% of burst_capacity — exit threshold for saturation hysteresis.
    recovery_threshold: i64,
    transitions_to_saturated: AtomicU64,
    transitions_to_unsaturated: AtomicU64,
    saturated_nanos: AtomicU64,
    saturated_since_nanos: AtomicU64,
}

impl LoadDebtTracker {
    pub(crate) fn new(
        max_streams_per_second: u64,
        burst_capacity: u64,
        refill_interval: Duration,
    ) -> Self {
        assert!(
            refill_interval.as_nanos() > 0,
            "refill_interval must be > 0"
        );
        let burst_capacity = burst_capacity as i64;
        Self {
            bucket: AtomicI64::new(burst_capacity),
            last_refill_nanos: AtomicU64::new(0),
            was_saturated: AtomicBool::new(false),
            epoch: Instant::now(),
            refill_interval_nanos: refill_interval.as_nanos() as u64,
            max_streams_per_second,
            burst_capacity,
            recovery_threshold: burst_capacity * 9 / 10,
            transitions_to_saturated: AtomicU64::new(0),
            transitions_to_unsaturated: AtomicU64::new(0),
            saturated_nanos: AtomicU64::new(0),
            saturated_since_nanos: AtomicU64::new(0),
        }
    }

    /// Consume one token. Triggers refill when bucket hits zero or below.
    pub(crate) fn acquire(&self) {
        let prev = self.bucket.fetch_sub(1, Ordering::Relaxed);
        if prev <= 1 {
            self.try_refill();
        }
    }

    /// Whether the system is saturated (with hysteresis).
    /// Enters when bucket ≤ 0; exits when bucket reaches 90% of capacity.
    /// Logs transitions at warn/info level.
    pub fn is_saturated(&self) -> bool {
        let level = self.bucket.load(Ordering::Relaxed);
        let was_saturated = self.was_saturated.load(Ordering::Relaxed);
        let saturated = if was_saturated {
            if level < self.recovery_threshold {
                self.try_refill();
                self.bucket.load(Ordering::Relaxed) < self.recovery_threshold
            } else {
                false
            }
        } else if level <= 0 {
            self.try_refill();
            self.bucket.load(Ordering::Relaxed) <= 0
        } else {
            false
        };
        let now_nanos = self.nanos_since_epoch();
        let prev = self.was_saturated.swap(saturated, Ordering::Relaxed);
        if saturated && !prev {
            self.transitions_to_saturated
                .fetch_add(1, Ordering::Relaxed);
            self.saturated_since_nanos
                .store(now_nanos, Ordering::Relaxed);
            log::warn!(
                "LoadDebtTracker: system saturated (bucket={})",
                self.bucket.load(Ordering::Relaxed),
            );
        } else if !saturated && prev {
            self.transitions_to_unsaturated
                .fetch_add(1, Ordering::Relaxed);
            let entered = self.saturated_since_nanos.load(Ordering::Relaxed);
            if now_nanos > entered {
                self.saturated_nanos
                    .fetch_add(now_nanos - entered, Ordering::Relaxed);
            }
            log::info!(
                "LoadDebtTracker: system recovered (bucket={})",
                self.bucket.load(Ordering::Relaxed),
            );
        }
        saturated
    }

    /// Return the current bucket level.
    pub fn bucket_level(&self) -> i64 {
        self.bucket.load(Ordering::Relaxed)
    }

    /// Return and reset the number of unsaturated→saturated transitions.
    pub fn take_transitions_to_saturated(&self) -> u64 {
        self.transitions_to_saturated.swap(0, Ordering::Relaxed)
    }

    /// Return and reset the number of saturated→unsaturated transitions.
    pub fn take_transitions_to_unsaturated(&self) -> u64 {
        self.transitions_to_unsaturated.swap(0, Ordering::Relaxed)
    }

    /// Return and reset cumulative nanos spent in saturated state.
    /// Flushes the pending interval if currently saturated.
    pub fn take_saturated_nanos(&self) -> u64 {
        let mut nanos = self.saturated_nanos.swap(0, Ordering::Relaxed);
        if self.was_saturated.load(Ordering::Relaxed) {
            let now = self.nanos_since_epoch();
            let entered = self.saturated_since_nanos.swap(now, Ordering::Relaxed);
            if now > entered {
                nanos += now - entered;
            }
        }
        nanos
    }

    /// Log saturation statistics and reset counters.
    pub fn report_saturation_stats(&self, elapsed: Duration) {
        let to_sat = self.take_transitions_to_saturated();
        let to_unsat = self.take_transitions_to_unsaturated();
        let sat_nanos = self.take_saturated_nanos();
        let elapsed_nanos = elapsed.as_nanos() as u64;
        let duty_pct = if elapsed_nanos > 0 {
            sat_nanos as f64 / elapsed_nanos as f64 * 100.0
        } else {
            0.0
        };
        log::info!(
            "LoadDebtTracker: transitions to_saturated={to_sat} to_unsaturated={to_unsat} \
             saturated_duty={duty_pct:.1}% bucket={}",
            self.bucket_level(),
        );
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

        self.bucket.fetch_add(refill, Ordering::Relaxed);

        // Cap at burst_capacity (racy but bounded and fine for an approximate signal).
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

    // 100 tokens/s, burst=100, refill every 10ms.
    fn simple() -> LoadDebtTracker {
        LoadDebtTracker::new(100, 100, Duration::from_millis(10))
    }

    fn acquire_n(g: &LoadDebtTracker, n: u64) {
        for _ in 0..n {
            g.acquire();
        }
    }

    #[test]
    fn test_starts_not_saturated() {
        let g = simple();
        assert_eq!(g.bucket_level(), 100);
        assert!(!g.is_saturated());
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
    fn test_not_saturated_above_zero() {
        let g = simple();
        acquire_n(&g, 99); // level = 1
        assert!(!g.is_saturated());
    }

    #[test]
    fn test_saturated_at_zero() {
        let g = simple();
        acquire_n(&g, 100); // level = 0
        assert!(g.is_saturated());
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
    fn test_recovery_requires_90_pct_refill() {
        let g = simple(); // burst = 100, recovery_threshold = 90
        acquire_n(&g, 100); // level = 0, saturated
        assert!(g.is_saturated());

        // Refill 50 → level=50: still saturated (below 90%).
        g.refill_at(500_000_000);
        assert_eq!(g.bucket_level(), 50);
        assert!(g.is_saturated());

        // Refill to 90 → at recovery threshold, recovered.
        g.refill_at(900_000_000);
        assert_eq!(g.bucket_level(), 90);
        assert!(!g.is_saturated());
    }
}
