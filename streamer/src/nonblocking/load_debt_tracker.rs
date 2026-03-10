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
/// This mechanism is intentionally approximate for hot-path cost reasons.
/// It does not provide exact token accounting under contention.
///
/// Approximation details:
/// - concurrent readers/writers can observe transient mismatch between
///   `bucket` and `saturated`;
/// - refill uses integer truncation (`floor`), so each refill update can
///   lose <1 token of fractional credit.
///
/// Fractional-loss bound:
/// - with refill interval `I`, at most `1 / I` refill updates can happen per
///   second, so truncation bias is bounded by `< 1 / I` tokens/second.
/// - relative bias is therefore bounded by `< (1 / I) / max_streams_per_second`.
///
pub struct LoadDebtTracker {
    bucket: AtomicI64,
    /// High bit is a lock.
    last_refill_nanos: AtomicU64,
    /// Needed because 0 < bucket < recovery_threshold is ambiguous without history.
    saturated: AtomicBool,
    epoch: Instant,
    refill_interval_nanos: u64,
    max_streams_per_second: u64,
    burst_capacity: i64,
    recovery_threshold: i64,
}

const LOCK_BIT: u64 = 1 << 63;
const NANO_MASK: u64 = !LOCK_BIT;

impl LoadDebtTracker {
    pub(crate) fn new(
        max_streams_per_second: u64,
        burst_capacity: u64,
        refill_interval: Duration,
    ) -> Self {
        let refill_interval_nanos = refill_interval.as_nanos();
        assert!(refill_interval_nanos > 0, "refill_interval must be > 0");
        assert!(
            burst_capacity <= i64::MAX as u64,
            "burst_capacity must fit i64"
        );
        assert!(
            refill_interval_nanos <= u64::MAX as u128,
            "refill_interval is too large"
        );

        // Require at least 1 whole token of refill per interval to avoid
        // pathological zero-refill loops from integer truncation.
        let refill_per_interval_numerator = (max_streams_per_second as u128)
            .checked_mul(refill_interval_nanos)
            .expect("max_streams_per_second * refill_interval overflow");
        assert!(
            refill_per_interval_numerator >= 1_000_000_000_u128,
            "max_streams_per_second * refill_interval must yield at least 1 token per interval"
        );

        let burst_capacity = burst_capacity as i64;
        Self {
            bucket: AtomicI64::new(burst_capacity),
            last_refill_nanos: AtomicU64::new(0),
            saturated: AtomicBool::new(false),
            epoch: Instant::now(),
            refill_interval_nanos: refill_interval_nanos as u64,
            max_streams_per_second,
            burst_capacity,
            recovery_threshold: burst_capacity * 9 / 10,
        }
    }

    /// Consume one token. Triggers state update when bucket hits zero or below.
    pub(crate) fn acquire(&self) {
        let prev = self.bucket.fetch_sub(1, Ordering::Relaxed);
        if prev <= 1 {
            // Crossing 1 -> 0: force a state check immediately.
            // Already at/below zero: run normal state maintenance.
            self.update_state_inner(prev == 1);
        }
    }

    /// Whether the system is saturated (with hysteresis).
    /// When saturated, probes for recovery so the flag doesn't stay stale
    /// if accepted-stream traffic drops.
    pub fn is_saturated(&self) -> bool {
        let saturated = self.saturated.load(Ordering::Relaxed);
        if saturated {
            self.update_state_inner(false);
        } else if self.bucket.load(Ordering::Relaxed) <= 0 {
            // If debt is visible but the flag is false, force an enter check.
            self.update_state_inner(true);
        }
        self.saturated.load(Ordering::Relaxed)
    }

    /// Return the current bucket level (testing only).
    #[cfg(test)]
    pub fn bucket_level(&self) -> i64 {
        self.bucket.load(Ordering::Relaxed)
    }

    fn nanos_since_epoch(&self) -> u64 {
        self.epoch.elapsed().as_nanos() as u64
    }

    /// Refill the bucket if the interval has elapsed and update saturation
    /// state. All state transitions happen under the bit lock.
    ///
    /// When `force` is false, a cheap elapsed-time pre-check avoids the CAS
    /// unless a refill is actually due. When `force` is true, the pre-check
    /// is skipped so a saturation transition can be detected promptly.
    fn update_state_inner(&self, force: bool) {
        let now_nanos = self.nanos_since_epoch();
        self.update_state_at(now_nanos, force);
    }

    fn update_state_at(&self, now_nanos: u64, force: bool) {
        let raw = self.last_refill_nanos.load(Ordering::Relaxed);
        if raw & LOCK_BIT != 0 {
            return; // another thread holds the lock
        }

        // ── Pre-check: skip CAS when no work is needed ──
        if !force {
            let last_nanos = raw & NANO_MASK;
            if now_nanos.saturating_sub(last_nanos) < self.refill_interval_nanos {
                return;
            }
        }

        // ── Acquire lock ──
        if self
            .last_refill_nanos
            .compare_exchange(raw, raw | LOCK_BIT, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        // ── Refill (only if interval has elapsed) ──
        let last_nanos = raw & NANO_MASK;
        let mut new_timestamp = last_nanos;
        let mut log_enter_saturated = false;
        let mut log_recovered = false;
        let mut log_level = 0_i64;
        let elapsed_nanos = now_nanos.saturating_sub(last_nanos);
        if elapsed_nanos >= self.refill_interval_nanos {
            let dt_secs = elapsed_nanos as f64 / 1_000_000_000.0;
            let refill = (self.max_streams_per_second as f64 * dt_secs) as i64;
            self.bucket.fetch_add(refill, Ordering::Relaxed);

            let level = self.bucket.load(Ordering::Relaxed);
            if level > self.burst_capacity {
                self.bucket.store(self.burst_capacity, Ordering::Relaxed);
            }
            new_timestamp = now_nanos;
        }

        // ── Update saturation state (under lock) ──
        let level = self.bucket.load(Ordering::Relaxed);
        let was_sat = self.saturated.load(Ordering::Relaxed);

        if !was_sat && level <= 0 {
            self.saturated.store(true, Ordering::Relaxed);
            log_enter_saturated = true;
            log_level = level;
        } else if was_sat && level >= self.recovery_threshold {
            self.saturated.store(false, Ordering::Relaxed);
            log_recovered = true;
            log_level = level;
        }

        // ── Release lock ──
        self.last_refill_nanos
            .store(new_timestamp & NANO_MASK, Ordering::Release);

        if log_enter_saturated {
            log::warn!("LoadDebtTracker: system saturated (bucket={log_level})");
        } else if log_recovered {
            log::info!("LoadDebtTracker: system recovered (bucket={log_level})");
        }
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
    fn test_is_saturated_self_heals_missed_enter() {
        let g = simple();
        acquire_n(&g, 100); // level = 0, saturated
        assert!(g.is_saturated());

        // Simulate a missed enter transition: debt is present but flag is false.
        g.saturated.store(false, Ordering::Relaxed);
        assert_eq!(g.bucket_level(), 0);

        // Public API should force a state check and re-enter saturation.
        assert!(g.is_saturated());
    }

    #[test]
    fn test_refill_adds_tokens() {
        let g = simple(); // 100/s, refill interval 10ms
        acquire_n(&g, 100); // level = 0

        // 50ms elapsed at 100/s → refill = 5 tokens
        g.update_state_at(50_000_000, false);
        assert_eq!(g.bucket_level(), 5);
    }

    #[test]
    fn test_refill_from_negative() {
        let g = simple();
        acquire_n(&g, 120); // level = -20
        assert_eq!(g.bucket_level(), -20);
        // 500ms at 100/s → refill = 50
        g.update_state_at(500_000_000, false);
        assert_eq!(g.bucket_level(), 30); // -20 + 50
    }

    #[test]
    fn test_refill_caps_at_burst() {
        let g = simple(); // burst = 100, starts at 100

        // Don't consume anything. Refill after 10s → would add 1000.
        g.update_state_at(10_000_000_000, false);
        assert_eq!(g.bucket_level(), 100); // capped
    }

    #[test]
    fn test_refill_skipped_before_interval() {
        let g = simple(); // interval = 10ms
        acquire_n(&g, 50); // level = 50
        g.update_state_at(5_000_000, false); // 5ms < 10ms interval → no refill
        assert_eq!(g.bucket_level(), 50);
    }

    #[test]
    fn test_recovery_requires_90_pct_refill() {
        let g = simple(); // burst = 100, recovery_threshold = 90
        acquire_n(&g, 100); // level = 0, saturated
        assert!(g.is_saturated());

        // Refill 50 → level=50: still saturated (below 90%).
        g.update_state_at(500_000_000, false);
        assert_eq!(g.bucket_level(), 50);
        assert!(g.is_saturated());

        // Refill to 90 → at recovery threshold, recovered.
        g.update_state_at(900_000_000, false);
        assert_eq!(g.bucket_level(), 90);
        assert!(!g.is_saturated());
    }

    #[test]
    #[should_panic(expected = "must yield at least 1 token per interval")]
    fn test_new_panics_if_refill_is_sub_token_per_interval() {
        // 999 tokens/s over 1ms => 0.999 tokens per interval.
        let _ = LoadDebtTracker::new(999, 100, Duration::from_millis(1));
    }

    #[test]
    #[should_panic(expected = "burst_capacity must fit i64")]
    fn test_new_panics_if_burst_capacity_exceeds_i64() {
        let _ = LoadDebtTracker::new(
            1_000,
            (i64::MAX as u64).saturating_add(1),
            Duration::from_millis(1),
        );
    }

    #[test]
    fn test_concurrent_stress_mismatch_rate() {
        use std::{
            sync::{
                Arc, Barrier,
                atomic::{AtomicU64, Ordering},
            },
            thread,
        };

        const WRITERS: usize = 4;
        const READERS: usize = 2;
        const ITERS: usize = 25_000;
        const MARGIN: i64 = 5;
        const MAX_MISMATCH_PCT: f64 = 0.01;

        let g = Arc::new(LoadDebtTracker::new(20_000, 200, Duration::from_millis(1)));
        let start = Arc::new(Barrier::new(WRITERS + READERS + 1));
        let mismatches = Arc::new(AtomicU64::new(0));
        let samples = Arc::new(AtomicU64::new(0));
        let mut handles = Vec::with_capacity(WRITERS + READERS);

        for _ in 0..WRITERS {
            let g = Arc::clone(&g);
            let start = Arc::clone(&start);
            handles.push(thread::spawn(move || {
                start.wait();
                for i in 0..ITERS {
                    g.acquire();
                    if i % 32 == 0 {
                        let _ = g.is_saturated();
                    }
                }
            }));
        }

        for _ in 0..READERS {
            let g = Arc::clone(&g);
            let start = Arc::clone(&start);
            let mismatches = Arc::clone(&mismatches);
            let samples = Arc::clone(&samples);
            handles.push(thread::spawn(move || {
                start.wait();
                for _ in 0..ITERS {
                    let saturated = g.is_saturated();
                    let level = g.bucket_level();
                    if (level <= -MARGIN && !saturated)
                        || (level >= g.recovery_threshold + MARGIN && saturated)
                    {
                        mismatches.fetch_add(1, Ordering::Relaxed);
                    }
                    samples.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }

        start.wait();
        for h in handles {
            h.join().expect("worker thread should not panic");
        }

        // Force one final state consolidation at a large timestamp.
        g.update_state_at(10_000_000_000, true);

        let raw = g.last_refill_nanos.load(Ordering::Relaxed);
        assert_eq!(raw & LOCK_BIT, 0, "lock bit must not remain set");

        let level = g.bucket_level();
        assert!(
            level <= g.burst_capacity,
            "bucket should not exceed burst capacity: {level}"
        );
        let saturated = g.is_saturated();
        if level <= 0 {
            assert!(saturated, "debt should imply saturated");
        }
        if level >= g.recovery_threshold {
            assert!(!saturated, "recovery threshold should imply unsaturated");
        }

        let mismatches = mismatches.load(Ordering::Relaxed) as f64;
        let samples = samples.load(Ordering::Relaxed) as f64;
        let mismatch_pct = if samples > 0.0 {
            mismatches / samples
        } else {
            0.0
        };
        assert!(
            mismatch_pct <= MAX_MISMATCH_PCT,
            "mismatch rate too high: {mismatch_pct:.4} (mismatches={mismatches}, \
             samples={samples})"
        );
    }

    #[cfg(feature = "shuttle-test")]
    #[test]
    fn shuttle_test_load_debt_tracker_interleavings() {
        use {
            shuttle::{check_random, thread},
            std::sync::{Arc, atomic::Ordering},
        };

        check_random(
            || {
                let g = Arc::new(LoadDebtTracker::new(1_000, 64, Duration::from_millis(1)));

                let h1 = {
                    let g = Arc::clone(&g);
                    thread::spawn(move || {
                        for _ in 0..64 {
                            g.acquire();
                            thread::yield_now();
                        }
                    })
                };

                let h2 = {
                    let g = Arc::clone(&g);
                    thread::spawn(move || {
                        for step in 1..=64_u64 {
                            g.update_state_at(step * 1_000_000, false);
                            let _ = g.is_saturated();
                            thread::yield_now();
                        }
                    })
                };

                let h3 = {
                    let g = Arc::clone(&g);
                    thread::spawn(move || {
                        for _ in 0..64 {
                            let _ = g.is_saturated();
                            thread::yield_now();
                        }
                    })
                };

                h1.join().expect("acquire worker panicked");
                h2.join().expect("state worker panicked");
                h3.join().expect("stats worker panicked");

                g.update_state_at(1_000_000_000, true);

                let raw = g.last_refill_nanos.load(Ordering::Relaxed);
                assert_eq!(raw & LOCK_BIT, 0, "lock bit leaked");

                let level = g.bucket_level();
                assert!(
                    level <= g.burst_capacity,
                    "bucket overflowed past burst capacity: {level}"
                );
                let saturated = g.is_saturated();
                if level <= 0 {
                    assert!(saturated, "debt should imply saturated");
                }
                if level >= g.recovery_threshold {
                    assert!(!saturated, "recovery threshold should imply unsaturated");
                }
            },
            200,
        );
    }
}
