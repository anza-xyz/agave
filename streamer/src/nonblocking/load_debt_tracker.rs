use {
    solana_svm_type_overrides::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    std::time::{Duration, Instant},
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
    #[cfg(test)]
    manual_time_nanos: Option<solana_svm_type_overrides::sync::Arc<AtomicU64>>,
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
            #[cfg(test)]
            manual_time_nanos: None,
            refill_interval_nanos: refill_interval_nanos as u64,
            max_streams_per_second,
            burst_capacity,
            recovery_threshold: burst_capacity * 9 / 10,
        }
    }

    #[cfg(test)]
    fn new_with_manual_time(
        max_streams_per_second: u64,
        burst_capacity: u64,
        refill_interval: Duration,
        manual_time_nanos: solana_svm_type_overrides::sync::Arc<AtomicU64>,
    ) -> Self {
        let mut tracker = Self::new(max_streams_per_second, burst_capacity, refill_interval);
        tracker.manual_time_nanos = Some(manual_time_nanos);
        tracker
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

    /// Retrieves monotonic nanoseconds since epoch.
    fn nanos_since_epoch(&self) -> u64 {
        #[cfg(test)]
        if let Some(manual_time_nanos) = &self.manual_time_nanos {
            return manual_time_nanos.load(Ordering::Relaxed);
        }

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
    #[cfg(not(feature = "shuttle-test"))]
    use std::sync::Barrier;
    use {
        super::*,
        solana_svm_type_overrides::{sync::Arc, thread},
    };

    type TestClock = Arc<AtomicU64>;

    const REFILL_INTERVAL: Duration = Duration::from_millis(1);

    fn manual(max_streams_per_second: u64, burst_capacity: u64) -> (LoadDebtTracker, TestClock) {
        let clock = Arc::new(AtomicU64::new(0));
        let tracker = LoadDebtTracker::new_with_manual_time(
            max_streams_per_second,
            burst_capacity,
            REFILL_INTERVAL,
            Arc::clone(&clock),
        );
        (tracker, clock)
    }

    fn exact() -> (LoadDebtTracker, TestClock) {
        manual(1_000, 10)
    }

    fn acquire_n(g: &LoadDebtTracker, n: u64) {
        for _ in 0..n {
            g.acquire();
        }
    }

    fn set_time(clock: &TestClock, nanos: u64) {
        clock.store(nanos, Ordering::SeqCst);
    }

    #[cfg(not(feature = "shuttle-test"))]
    fn advance_and_probe(g: &LoadDebtTracker, clock: &TestClock, nanos: u64) -> bool {
        set_time(clock, nanos);
        g.is_saturated()
    }

    fn assert_lock_released(g: &LoadDebtTracker) {
        let raw = g.last_refill_nanos.load(Ordering::Relaxed);
        assert_eq!(raw & LOCK_BIT, 0, "lock bit leaked");
    }

    fn assert_state(g: &LoadDebtTracker, expected_level: i64, expected_saturated: bool) {
        assert_eq!(g.bucket_level(), expected_level);
        assert_eq!(g.is_saturated(), expected_saturated);
        assert_lock_released(g);
    }

    #[cfg(not(feature = "shuttle-test"))]
    #[test]
    fn test_starts_full_and_not_saturated() {
        let (g, _) = exact();
        assert_state(&g, 10, false);
    }

    #[cfg(not(feature = "shuttle-test"))]
    #[test]
    fn test_acquire_drains_into_debt_exactly() {
        let (g, _) = exact();
        acquire_n(&g, 14);
        assert_state(&g, -4, true);
    }

    #[cfg(not(feature = "shuttle-test"))]
    #[test]
    fn test_enter_saturation_at_zero_boundary() {
        let (g, _) = exact();
        acquire_n(&g, 9);
        assert_state(&g, 1, false);

        g.acquire();
        assert_state(&g, 0, true);
    }

    #[cfg(not(feature = "shuttle-test"))]
    #[test]
    fn test_is_saturated_self_heals_missed_enter() {
        let (g, _) = exact();
        acquire_n(&g, 10);
        assert_state(&g, 0, true);

        g.saturated.store(false, Ordering::Relaxed);
        assert_eq!(g.bucket_level(), 0);
        assert!(g.is_saturated());
    }

    #[cfg(not(feature = "shuttle-test"))]
    #[test]
    fn test_refill_is_lazy_until_probed() {
        let (g, clock) = exact();
        acquire_n(&g, 10);
        assert_state(&g, 0, true);

        set_time(&clock, 5_000_000);
        assert_eq!(g.bucket_level(), 0, "refill should be lazy");

        assert!(g.is_saturated());
        assert_eq!(g.bucket_level(), 5);
    }

    #[cfg(not(feature = "shuttle-test"))]
    #[test]
    fn test_refill_requires_full_interval_and_exact_boundary() {
        let (g, clock) = exact();
        acquire_n(&g, 10);
        assert_state(&g, 0, true);

        assert!(advance_and_probe(&g, &clock, 999_999));
        assert_eq!(g.bucket_level(), 0);

        assert!(advance_and_probe(&g, &clock, 1_000_000));
        assert_eq!(g.bucket_level(), 1);
    }

    #[cfg(not(feature = "shuttle-test"))]
    #[test]
    fn test_backward_time_is_ignored() {
        let (g, clock) = exact();
        acquire_n(&g, 10);
        assert!(advance_and_probe(&g, &clock, 5_000_000));
        assert_eq!(g.bucket_level(), 5);

        assert!(advance_and_probe(&g, &clock, 4_000_000));
        assert_eq!(g.bucket_level(), 5);
    }

    #[cfg(not(feature = "shuttle-test"))]
    #[test]
    fn test_recovery_hysteresis_boundary() {
        let (g, clock) = exact();
        acquire_n(&g, 14);
        assert_state(&g, -4, true);

        assert!(advance_and_probe(&g, &clock, 12_000_000));
        assert_eq!(g.bucket_level(), 8);
        assert!(g.is_saturated());

        assert!(!advance_and_probe(&g, &clock, 13_000_000));
        assert_state(&g, 9, false);
    }

    #[cfg(not(feature = "shuttle-test"))]
    #[test]
    fn test_refill_caps_at_burst_after_large_elapsed() {
        let (g, clock) = exact();
        acquire_n(&g, 10);
        assert!(!advance_and_probe(&g, &clock, 40_000_000));
        assert_state(&g, 10, false);
    }

    #[cfg(not(feature = "shuttle-test"))]
    #[test]
    fn test_force_update_enters_saturation_without_elapsed() {
        let (g, _) = exact();
        g.bucket.store(0, Ordering::Relaxed);
        g.saturated.store(false, Ordering::Relaxed);

        g.update_state_at(0, true);
        assert_state(&g, 0, true);
    }

    #[test]
    #[should_panic(expected = "refill_interval must be > 0")]
    fn test_new_panics_if_refill_interval_is_zero() {
        let _ = LoadDebtTracker::new(1_000, 10, Duration::ZERO);
    }

    #[test]
    #[should_panic(expected = "must yield at least 1 token per interval")]
    fn test_new_panics_if_refill_is_sub_token_per_interval() {
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

    #[cfg(not(feature = "shuttle-test"))]
    #[test]
    fn test_real_threads_preserve_basic_invariants() {
        // Simple std-thread smoke test to exercise the real OS-threaded code path.
        // The clock thread advances only after seeing writer progress,
        // so it can not run far ahead on a dedicated core before the acquire path
        // is exercised. yield_now() is just a scheduler hint that may increase
        // interleaving opportunities without introducing sleeps or timing thresholds.
        const CLOCK_STEPS: u64 = 200;
        const MAX_IDLE_YIELDS: usize = 100_000;

        let (g, clock) = manual(1_000, 32);
        let g = Arc::new(g);
        let clock = Arc::clone(&clock);
        let run = Arc::new(AtomicBool::new(true));
        let write_progress = Arc::new(AtomicU64::new(0));
        let start = Arc::new(Barrier::new(5));
        let mut handles = Vec::new();

        handles.push(thread::spawn({
            let clock = Arc::clone(&clock);
            let run = Arc::clone(&run);
            let write_progress = Arc::clone(&write_progress);
            let start = Arc::clone(&start);
            move || {
                start.wait();
                let mut last_progress = 0_u64;
                for step in 1..=CLOCK_STEPS {
                    let mut idle_yields = 0_usize;
                    while write_progress.load(Ordering::SeqCst) <= last_progress {
                        idle_yields += 1;
                        assert!(
                            idle_yields < MAX_IDLE_YIELDS,
                            "writer threads made no progress while clock thread was waiting"
                        );
                        thread::yield_now();
                    }
                    last_progress = write_progress.load(Ordering::SeqCst);
                    set_time(&clock, step * 1_000_000);
                    thread::yield_now();
                }
                run.store(false, Ordering::SeqCst);
            }
        }));

        for _ in 0..2 {
            handles.push(thread::spawn({
                let g = Arc::clone(&g);
                let run = Arc::clone(&run);
                let write_progress = Arc::clone(&write_progress);
                let start = Arc::clone(&start);
                move || {
                    start.wait();
                    while run.load(Ordering::SeqCst) {
                        g.acquire();
                        let _ = g.is_saturated();
                        write_progress.fetch_add(1, Ordering::SeqCst);
                        thread::yield_now();
                    }
                }
            }));
        }

        handles.push(thread::spawn({
            let g = Arc::clone(&g);
            let run = Arc::clone(&run);
            let start = Arc::clone(&start);
            move || {
                start.wait();
                while run.load(Ordering::SeqCst) {
                    let _ = g.is_saturated();
                    let _ = g.bucket_level();
                    thread::yield_now();
                }
            }
        }));

        start.wait();
        for handle in handles {
            handle.join().expect("worker thread should not panic");
        }

        assert!(
            write_progress.load(Ordering::SeqCst) >= CLOCK_STEPS,
            "writer threads did not make enough progress"
        );

        // Before consolidation, the snapshot may still be stale by design:
        // elapsed time can remain unclaimed and bucket/saturated can
        // transiently disagree under contention. Only assert invariants that
        // must already hold in the raw post-join state.
        assert_lock_released(&g);
        let raw_level = g.bucket_level();
        assert!(
            raw_level <= g.burst_capacity,
            "bucket overflowed past burst capacity before consolidation: {raw_level}"
        );

        // Force one final state claim at a known timestamp, then verify the
        // post-race state has converged to a sane quiescent view.
        set_time(&clock, (CLOCK_STEPS + 50) * 1_000_000);
        g.update_state_at((CLOCK_STEPS + 50) * 1_000_000, true);

        assert_lock_released(&g);
        let level = g.bucket_level();
        assert!(
            level <= g.burst_capacity,
            "bucket overflowed past burst capacity after consolidation: {level}"
        );

        let saturated = g.is_saturated();
        if level <= 0 {
            assert!(
                saturated,
                "debt ({level}) should imply saturated after consolidation"
            );
        }
        if level >= g.recovery_threshold {
            assert!(
                !saturated,
                "above recovery threshold ({level}) should imply unsaturated after consolidation"
            );
        }
    }

    // ── Shuttle tests ──────────────────────────────────────────────────

    #[cfg(feature = "shuttle-test")]
    // The DFS cases cover the small exact schedules; keep the one broader
    // random interleaving test long enough to be useful while staying cheap.
    const SHUTTLE_RANDOM_ITERATIONS: usize = 1_000;
    #[cfg(feature = "shuttle-test")]
    const SHUTTLE_DFS_ITERATIONS: Option<usize> = Some(10_000);

    #[cfg(feature = "shuttle-test")]
    fn do_shuttle_concurrent_enter_saturation() {
        let (g, _) = manual(1_000, 2);
        let g = Arc::new(g);

        let handles: Vec<_> = (0..3)
            .map(|_| {
                let g = Arc::clone(&g);
                thread::spawn(move || {
                    g.acquire();
                    shuttle::thread::yield_now();
                })
            })
            .collect();
        let observer = thread::spawn({
            let g = Arc::clone(&g);
            move || {
                let _ = g.is_saturated();
                shuttle::thread::yield_now();
            }
        });

        for handle in handles {
            handle.join().unwrap();
        }
        observer.join().unwrap();

        assert_state(&g, -1, true);
    }

    #[cfg(feature = "shuttle-test")]
    #[test]
    fn shuttle_concurrent_enter_saturation_dfs() {
        shuttle::check_dfs(
            do_shuttle_concurrent_enter_saturation,
            SHUTTLE_DFS_ITERATIONS,
        );
    }

    #[cfg(feature = "shuttle-test")]
    fn do_shuttle_single_interval_refill_claimed_once() {
        let (g, clock) = manual(1_000, 4);
        let g = Arc::new(g);
        acquire_n(&g, 4);
        assert!(g.is_saturated());

        set_time(&clock, 1_000_000);
        let handles: Vec<_> = (0..3)
            .map(|_| {
                let g = Arc::clone(&g);
                thread::spawn(move || {
                    let _ = g.is_saturated();
                    shuttle::thread::yield_now();
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        assert_state(&g, 1, true);
    }

    #[cfg(feature = "shuttle-test")]
    #[test]
    fn shuttle_single_interval_refill_claimed_once_dfs() {
        shuttle::check_dfs(
            do_shuttle_single_interval_refill_claimed_once,
            SHUTTLE_DFS_ITERATIONS,
        );
    }

    #[cfg(feature = "shuttle-test")]
    fn do_shuttle_recovery_hysteresis_boundary() {
        let (g, clock) = exact();
        let g = Arc::new(g);
        acquire_n(&g, 14);
        assert!(g.is_saturated());

        set_time(&clock, 12_000_000);
        let first_phase: Vec<_> = (0..2)
            .map(|_| {
                let g = Arc::clone(&g);
                thread::spawn(move || {
                    let _ = g.is_saturated();
                    shuttle::thread::yield_now();
                })
            })
            .collect();
        for handle in first_phase {
            handle.join().unwrap();
        }
        assert_state(&g, 8, true);

        set_time(&clock, 13_000_000);
        let _ = g.is_saturated();
        shuttle::thread::yield_now();

        assert_state(&g, 9, false);
    }

    #[cfg(feature = "shuttle-test")]
    #[test]
    fn shuttle_recovery_hysteresis_boundary_dfs() {
        shuttle::check_dfs(
            do_shuttle_recovery_hysteresis_boundary,
            SHUTTLE_DFS_ITERATIONS,
        );
    }

    #[cfg(feature = "shuttle-test")]
    fn do_shuttle_force_updates_claim_elapsed_once() {
        let (g, _) = exact();
        let g = Arc::new(g);
        acquire_n(&g, 10);
        assert!(g.is_saturated());

        let handles: Vec<_> = [1_u64, 2, 3]
            .into_iter()
            .map(|step| {
                let g = Arc::clone(&g);
                thread::spawn(move || {
                    let now = step * 1_000_000;
                    g.update_state_at(now, true);
                    shuttle::thread::yield_now();
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        assert_lock_released(&g);
        let raw_level = g.bucket_level();
        assert!(
            (1..=3).contains(&raw_level),
            "force-update race should leave 1..=3 tokens before consolidation, got {raw_level}"
        );
        let raw_timestamp = g.last_refill_nanos.load(Ordering::Relaxed) & NANO_MASK;
        assert!(
            (1_000_000..=3_000_000).contains(&raw_timestamp),
            "force-update race should claim some elapsed time before consolidation, got \
             {raw_timestamp}"
        );
        assert!(
            g.saturated.load(Ordering::Relaxed),
            "pre-consolidation state should still be saturated"
        );

        g.update_state_at(3_000_000, true);
        assert_state(&g, 3, true);
    }

    #[cfg(feature = "shuttle-test")]
    #[test]
    fn shuttle_force_updates_claim_elapsed_once_dfs() {
        shuttle::check_dfs(
            do_shuttle_force_updates_claim_elapsed_once,
            SHUTTLE_DFS_ITERATIONS,
        );
    }

    #[cfg(feature = "shuttle-test")]
    fn do_shuttle_fixed_time_accounting_race() {
        let (g, clock) = manual(1_000, 8);
        let g = Arc::new(g);

        // Start empty and saturated so refill can not be wasted by a full bucket.
        // Publish one fixed 6ms timestamp before any contender runs: exactly one
        // thread should claim those 6 tokens, and four acquires should then
        // leave an exact raw level of 2 regardless of interleaving.
        g.bucket.store(0, Ordering::SeqCst);
        g.saturated.store(true, Ordering::SeqCst);
        set_time(&clock, 6_000_000);

        let acquire_threads: Vec<_> = (0..2)
            .map(|_| {
                let g = Arc::clone(&g);
                thread::spawn(move || {
                    for _ in 0..2 {
                        g.acquire();
                        shuttle::thread::yield_now();
                    }
                })
            })
            .collect();

        let h_observe = thread::spawn({
            let g = Arc::clone(&g);
            move || {
                for _ in 0..6 {
                    let _ = g.is_saturated();
                    let _ = g.bucket_level();
                    shuttle::thread::yield_now();
                }
            }
        });

        for handle in acquire_threads {
            handle.join().unwrap();
        }
        h_observe.join().unwrap();

        assert_lock_released(&g);
        assert_eq!(
            g.bucket_level(),
            2,
            "fixed-time accounting race should end with an exact raw bucket level"
        );
        let raw_timestamp = g.last_refill_nanos.load(Ordering::Relaxed) & NANO_MASK;
        assert!(
            raw_timestamp == 6_000_000,
            "fixed-time accounting race should claim the full published timestamp exactly, got \
             {raw_timestamp}"
        );
        assert!(
            g.saturated.load(Ordering::Relaxed),
            "fixed-time accounting race should remain saturated before consolidation"
        );

        set_time(&clock, 6_000_000);
        g.update_state_at(6_000_000, true);
        assert_state(&g, 2, true);
    }

    #[cfg(feature = "shuttle-test")]
    #[test]
    fn shuttle_fixed_time_accounting_race_random() {
        shuttle::check_random(
            do_shuttle_fixed_time_accounting_race,
            SHUTTLE_RANDOM_ITERATIONS,
        );
    }
}
