use {
    agave_math_utils::welford_stats::WelfordStats,
    solana_metrics::datapoint_info,
    std::time::{Duration, Instant},
};

/// Max amount of seconds to wait before triggering reporting of stats.
const DURATION_INTERVAL: Duration = Duration::from_secs(1);

pub(super) struct Stats {
    pub(super) vote_history_store_us: WelfordStats,
    last_report: Instant,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            vote_history_store_us: WelfordStats::default(),
            last_report: Instant::now(),
        }
    }
}

impl Stats {
    pub(super) fn maybe_report(&mut self) {
        if self.last_report.elapsed() < DURATION_INTERVAL {
            return;
        }
        let Self {
            last_report: _,
            vote_history_store_us,
        } = self;
        datapoint_info!(
            "voting-service",
            ("vote_history_store_cnt", vote_history_store_us.count(), i64),
            ("vote_history_store_mean", vote_history_store_us.mean::<u64>(), Option<i64>),
            ("vote_history_store_max", vote_history_store_us.maximum::<u64>(), Option<i64>),
        );
        *self = Self::default();
    }
}
