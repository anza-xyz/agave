use {core::time::Duration, std::time::Instant};

/// Computes a linearly prorated cost budget at millisecond granularity.
pub struct CostPacer {
    budget: u64,
    start: Instant,
    fill_duration: Duration,
}

impl CostPacer {
    /// A fill duration shorter than one millisecond makes the full budget available immediately.
    pub fn new(budget: u64, start: Instant, fill_duration: Duration) -> Self {
        Self {
            budget,
            start,
            fill_duration,
        }
    }

    /// Returns the time-prorated budget minus cost already consumed or reserved.
    pub fn available_budget(&self, now: Instant, consumed_cost: u64) -> u64 {
        let elapsed_ms = now.saturating_duration_since(self.start).as_millis();
        let fill_ms = self.fill_duration.as_millis();
        let prorated_budget = if elapsed_ms >= fill_ms {
            self.budget
        } else {
            u128::from(self.budget)
                .saturating_mul(elapsed_ms)
                .checked_div(fill_ms)
                .unwrap() as u64
        };
        prorated_budget.saturating_sub(consumed_cost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_budget_is_prorated_by_elapsed_time() {
        let start = Instant::now();
        let pacer = CostPacer::new(100, start, Duration::from_millis(390));
        assert_eq!(pacer.available_budget(start, 0), 0);
        assert_eq!(
            pacer.available_budget(start + Duration::from_millis(195), 0),
            50
        );
        assert_eq!(
            pacer.available_budget(start + Duration::from_millis(195), 30),
            20
        );
        assert_eq!(
            pacer.available_budget(start + Duration::from_millis(195), 60),
            0
        );
        assert_eq!(
            pacer.available_budget(start + Duration::from_millis(390), 0),
            100
        );
        assert_eq!(
            pacer.available_budget(start + Duration::from_millis(500), 0),
            100
        );
        assert_eq!(
            pacer.available_budget(start + Duration::from_millis(500), 100),
            0
        );
    }

    #[test]
    fn budget_uses_whole_milliseconds() {
        let start = Instant::now();
        let pacer = CostPacer::new(100, start, Duration::from_millis(10));
        assert_eq!(
            pacer.available_budget(start + Duration::from_micros(999), 0),
            0
        );
        assert_eq!(
            pacer.available_budget(start + Duration::from_millis(1), 0),
            10
        );
        assert_eq!(
            pacer.available_budget(start + Duration::from_micros(1_999), 0),
            10
        );

        let pacer = CostPacer::new(100, start, Duration::from_micros(999));
        assert_eq!(pacer.available_budget(start, 0), 100);
    }

    #[test]
    fn zero_fill_duration_makes_full_budget_available() {
        let start = Instant::now();
        let pacer = CostPacer::new(100, start, Duration::ZERO);
        assert_eq!(pacer.available_budget(start, 0), 100);
        assert_eq!(pacer.available_budget(start, 30), 70);
        assert_eq!(pacer.available_budget(start, 101), 0);
    }
}
