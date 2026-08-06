use std::time::{Duration, Instant};

/// Releases a cost budget linearly over an optional fill time.
pub struct CostPacer {
    total_cost: u64,
    detection_time: Instant,
    fill_time: Option<Duration>,
}

impl CostPacer {
    pub fn new(total_cost: u64, detection_time: Instant, fill_time: Option<Duration>) -> Self {
        Self {
            total_cost,
            detection_time,
            fill_time,
        }
    }

    pub fn scheduling_budget(&self, current_time: &Instant, consumed_cost: u64) -> u64 {
        let target = if let Some(fill_time) = &self.fill_time {
            let time_since = current_time.saturating_duration_since(self.detection_time);
            if time_since >= *fill_time {
                self.total_cost
            } else {
                // on millisecond granularity, pace the cost linearly.
                let fill_time_millis = u64::try_from(fill_time.as_millis())
                    .unwrap_or(u64::MAX)
                    .max(1);
                let millis_since_detection =
                    u64::try_from(time_since.as_millis()).unwrap_or(u64::MAX);
                #[allow(clippy::arithmetic_side_effects)]
                let target = self
                    .total_cost
                    .saturating_mul(millis_since_detection)
                    .saturating_div(fill_time_millis);
                target
            }
        } else {
            self.total_cost
        };

        target.saturating_sub(consumed_cost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn releases_cost_linearly() {
        let start = Instant::now();
        let pacer = CostPacer::new(1_000, start, Some(Duration::from_millis(100)));

        assert_eq!(pacer.scheduling_budget(&start, 0), 0);
        assert_eq!(
            pacer.scheduling_budget(&start.checked_add(Duration::from_millis(25)).unwrap(), 0,),
            250
        );
        assert_eq!(
            pacer.scheduling_budget(&start.checked_add(Duration::from_millis(25)).unwrap(), 100,),
            150
        );
        assert_eq!(
            pacer.scheduling_budget(&start.checked_add(Duration::from_millis(100)).unwrap(), 400,),
            600
        );
    }

    #[test]
    fn releases_cost_linearly_when_cost_is_less_than_fill_time_millis() {
        let start = Instant::now();
        let pacer = CostPacer::new(10, start, Some(Duration::from_millis(100)));

        assert_eq!(
            pacer.scheduling_budget(&start.checked_add(Duration::from_millis(50)).unwrap(), 0),
            5
        );
    }

    #[test]
    fn disabled_pacing_releases_the_entire_budget() {
        let now = Instant::now();
        let pacer = CostPacer::new(1_000, now, None);

        assert_eq!(pacer.scheduling_budget(&now, 400), 600);
    }
}
