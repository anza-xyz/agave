use {
    solana_metrics::create_datapoint,
    std::time::{Duration, Instant},
};

/// Metrics from scheduling transactions.
#[derive(Default, Debug, PartialEq, Eq)]
pub(super) struct SchedulingSummary {
    /// Starting queue size
    pub(super) starting_queue_size: usize,
    /// Starting buffer size (outstanding txs are not counted in queue)
    pub(super) starting_buffer_size: usize,

    /// Number of transactions scheduled.
    pub(super) num_scheduled: usize,
    /// Number of transactions that were not scheduled due to conflicts.
    pub(super) num_unschedulable_conflicts: usize,
    /// Number of transactions that were skipped due to thread capacity.
    pub(super) num_unschedulable_threads: usize,
}

pub(super) struct SchedulingDetails {
    pub(super) last_report: Instant,
    pub(super) num_schedule_calls: usize,

    pub(super) min_starting_queue_size: usize,
    pub(super) max_starting_queue_size: usize,
    pub(super) sum_starting_queue_size: usize, // div for report

    pub(super) min_starting_buffer_size: usize,
    pub(super) max_starting_buffer_size: usize,
    pub(super) sum_starting_buffer_size: usize, // div for report

    pub(super) sum_num_scheduled: usize,
    pub(super) sum_unschedulable_conflicts: usize,
    pub(super) sum_unschedulable_threads: usize,
}

impl Default for SchedulingDetails {
    fn default() -> Self {
        Self {
            last_report: Instant::now(),
            num_schedule_calls: 0,
            min_starting_queue_size: usize::MAX,
            max_starting_queue_size: 0,
            sum_starting_queue_size: 0,
            min_starting_buffer_size: usize::MAX,
            max_starting_buffer_size: 0,
            sum_starting_buffer_size: 0,
            sum_num_scheduled: 0,
            sum_unschedulable_conflicts: 0,
            sum_unschedulable_threads: 0,
        }
    }
}

impl SchedulingDetails {
    pub(super) fn update(&mut self, scheduling_summary: &SchedulingSummary) {
        self.num_schedule_calls = self.num_schedule_calls.wrapping_add(1);

        self.min_starting_queue_size = self
            .min_starting_queue_size
            .min(scheduling_summary.starting_queue_size);
        self.max_starting_queue_size = self
            .max_starting_queue_size
            .max(scheduling_summary.starting_queue_size);
        self.sum_starting_queue_size = self
            .sum_starting_queue_size
            .wrapping_add(scheduling_summary.starting_queue_size);

        self.min_starting_buffer_size = self
            .min_starting_buffer_size
            .min(scheduling_summary.starting_buffer_size);
        self.max_starting_buffer_size = self
            .max_starting_buffer_size
            .max(scheduling_summary.starting_buffer_size);
        self.sum_starting_buffer_size = self
            .sum_starting_buffer_size
            .wrapping_add(scheduling_summary.starting_buffer_size);

        self.sum_num_scheduled = self
            .sum_num_scheduled
            .wrapping_add(scheduling_summary.num_scheduled);
        self.sum_unschedulable_conflicts = self
            .sum_unschedulable_conflicts
            .wrapping_add(scheduling_summary.num_unschedulable_conflicts);
        self.sum_unschedulable_threads = self
            .sum_unschedulable_threads
            .wrapping_add(scheduling_summary.num_unschedulable_threads);
    }

    pub(super) fn maybe_report(&mut self) {
        const REPORT_INTERVAL: Duration = Duration::from_millis(20);
        let now = Instant::now();
        if now.duration_since(self.last_report) > REPORT_INTERVAL {
            self.last_report = now;
            if let (Some(avg_starting_queue_size), Some(avg_starting_buffer_size)) = (
                self.sum_starting_queue_size
                    .checked_div(self.num_schedule_calls),
                self.sum_starting_buffer_size
                    .checked_div(self.num_schedule_calls),
            ) {
                let datapoint = create_datapoint!(
                    @point "block-production-scheduler-scheduling-details",
                    ("num_schedule_calls", self.num_schedule_calls, i64),
                    ("min_starting_queue_size", self.min_starting_queue_size, i64),
                    ("max_starting_queue_size", self.max_starting_queue_size, i64),
                    ("avg_starting_queue_size", avg_starting_queue_size, i64),
                    (
                        "min_starting_buffer_size",
                        self.min_starting_buffer_size,
                        i64
                    ),
                    (
                        "max_starting_buffer_size",
                        self.max_starting_buffer_size,
                        i64
                    ),
                    ("avg_starting_buffer_size", avg_starting_buffer_size, i64),
                    ("num_scheduled", self.sum_num_scheduled, i64),
                    (
                        "num_unschedulable_conflicts",
                        self.sum_unschedulable_conflicts,
                        i64
                    ),
                    (
                        "num_unschedulable_threads",
                        self.sum_unschedulable_threads,
                        i64
                    ),
                );
                solana_metrics::submit(datapoint, log::Level::Trace);
                *self = Self {
                    last_report: now,
                    ..Self::default()
                }
            }
        }
    }
}
