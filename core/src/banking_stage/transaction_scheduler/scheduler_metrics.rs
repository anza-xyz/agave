use {
    super::scheduler::SchedulingSummary,
    itertools::MinMaxResult,
    solana_clock::Slot,
    solana_time_utils::AtomicInterval,
    std::{
        num::Wrapping,
        time::{Duration, Instant},
    },
};

#[derive(Default)]
pub struct SchedulerCountMetrics {
    interval: IntervalSchedulerCountMetrics,
    slot: SlotSchedulerCountMetrics,
}

impl SchedulerCountMetrics {
    pub fn update(&mut self, update: impl Fn(&mut SchedulerCountMetricsInner)) {
        update(&mut self.interval.metrics);
        update(&mut self.slot.metrics);
    }

    pub fn maybe_report_and_reset_slot(&mut self, slot: Option<Slot>) {
        self.slot.maybe_report_and_reset(slot);
    }

    pub fn maybe_report_and_reset_interval(&mut self, should_report: bool) {
        self.interval.maybe_report_and_reset(should_report);
    }

    pub fn interval_has_data(&self) -> bool {
        self.interval.metrics.has_data()
    }
}

#[derive(Default)]
struct IntervalSchedulerCountMetrics {
    interval: AtomicInterval,
    metrics: SchedulerCountMetricsInner,
}

#[derive(Default)]
struct SlotSchedulerCountMetrics {
    slot: Option<Slot>,
    metrics: SchedulerCountMetricsInner,
}

#[derive(Default)]
pub struct SchedulerCountMetricsInner {
    /// Number of packets received.
    pub num_received: Wrapping<usize>,
    /// Number of packets buffered.
    pub num_buffered: Wrapping<usize>,
    /// Number of transactions scheduled.
    pub num_scheduled: Wrapping<usize>,
    /// Number of transactions that were unschedulable due to multiple conflicts.
    pub num_unschedulable_conflicts: Wrapping<usize>,
    /// Number of transactions that were unschedulable due to thread capacity.
    pub num_unschedulable_threads: Wrapping<usize>,
    /// Number of transactions that were filtered out during scheduling.
    pub num_schedule_filtered_out: Wrapping<usize>,
    /// Number of completed transactions received from workers.
    pub num_finished: Wrapping<usize>,
    /// Number of transactions that were retryable.
    pub num_retryable: Wrapping<usize>,

    /// Number of transactions that were immediately dropped on receive.
    pub num_dropped_on_receive: Wrapping<usize>,
    /// Number of transactions that were dropped due to sanitization failure.
    pub num_dropped_on_sanitization: Wrapping<usize>,
    /// Number of transactions that were dropped due to failed lock validation.
    pub num_dropped_on_validate_locks: Wrapping<usize>,
    /// Number of transactions that were dropped due to failed transaction
    /// checks during receive.
    pub num_dropped_on_receive_transaction_checks: Wrapping<usize>,
    /// Number of transactions that were dropped due to clearing.
    pub num_dropped_on_clear: Wrapping<usize>,
    /// Number of transactions that were dropped during cleaning.
    pub num_dropped_on_clean: Wrapping<usize>,
    /// Number of transactions that were dropped due to exceeded capacity.
    pub num_dropped_on_capacity: Wrapping<usize>,
    /// Min prioritization fees in the transaction container
    pub min_prioritization_fees: u64,
    /// Max prioritization fees in the transaction container
    pub max_prioritization_fees: u64,
}

impl IntervalSchedulerCountMetrics {
    fn maybe_report_and_reset(&mut self, should_report: bool) {
        const REPORT_INTERVAL_MS: u64 = 1000;
        if self.interval.should_update(REPORT_INTERVAL_MS) {
            if should_report {
                self.metrics.report("banking_stage_scheduler_counts", None);
            }
            self.metrics.reset();
        }
    }
}

impl SlotSchedulerCountMetrics {
    fn maybe_report_and_reset(&mut self, slot: Option<Slot>) {
        if self.slot != slot {
            // Only report if there was an assigned slot.
            if self.slot.is_some() {
                self.metrics
                    .report("banking_stage_scheduler_slot_counts", self.slot);
            }
            self.metrics.reset();
            self.slot = slot;
        }
    }
}

impl SchedulerCountMetricsInner {
    fn report(&self, name: &'static str, slot: Option<Slot>) {
        let &Self {
            num_received: Wrapping(num_received),
            num_buffered: Wrapping(num_buffered),
            num_scheduled: Wrapping(num_scheduled),
            num_unschedulable_conflicts: Wrapping(num_unschedulable_conflicts),
            num_unschedulable_threads: Wrapping(num_unschedulable_threads),
            num_schedule_filtered_out: Wrapping(num_schedule_filtered_out),
            num_finished: Wrapping(num_finished),
            num_retryable: Wrapping(num_retryable),
            num_dropped_on_receive: Wrapping(num_dropped_on_receive),
            num_dropped_on_sanitization: Wrapping(num_dropped_on_sanitization),
            num_dropped_on_validate_locks: Wrapping(num_dropped_on_validate_locks),
            num_dropped_on_receive_transaction_checks:
                Wrapping(num_dropped_on_receive_transaction_checks),
            num_dropped_on_clear: Wrapping(num_dropped_on_clear),
            num_dropped_on_clean: Wrapping(num_dropped_on_clean),
            num_dropped_on_capacity: Wrapping(num_dropped_on_capacity),
            min_prioritization_fees: _min_prioritization_fees,
            max_prioritization_fees: _max_prioritization_fees,
        } = self;
        let mut datapoint = create_datapoint!(
            @point name,
            ("num_received", num_received, i64),
            ("num_buffered", num_buffered, i64),
            ("num_scheduled", num_scheduled, i64),
            ("num_unschedulable_conflicts", num_unschedulable_conflicts, i64),
            ("num_unschedulable_threads", num_unschedulable_threads, i64),
            (
                "num_schedule_filtered_out",
                num_schedule_filtered_out,
                i64
            ),
            ("num_finished", num_finished, i64),
            ("num_retryable", num_retryable, i64),
            ("num_dropped_on_receive", num_dropped_on_receive, i64),
            (
                "num_dropped_on_sanitization",
                num_dropped_on_sanitization,
                i64
            ),
            (
                "num_dropped_on_validate_locks",
                num_dropped_on_validate_locks,
                i64
            ),
            (
                "num_dropped_on_receive_transaction_checks",
                num_dropped_on_receive_transaction_checks,
                i64
            ),
            ("num_dropped_on_clear", num_dropped_on_clear, i64),
            (
                "num_dropped_on_clean",
                num_dropped_on_clean,
                i64
            ),
            ("num_dropped_on_capacity", num_dropped_on_capacity, i64),
            ("min_priority", self.get_min_priority(), i64),
            ("max_priority", self.get_max_priority(), i64)
        );
        if let Some(slot) = slot {
            datapoint.add_field_i64("slot", slot as i64);
        }
        solana_metrics::submit(datapoint, log::Level::Info);
    }

    fn has_data(&self) -> bool {
        self.num_received != Wrapping(0)
            || self.num_buffered != Wrapping(0)
            || self.num_scheduled != Wrapping(0)
            || self.num_unschedulable_conflicts != Wrapping(0)
            || self.num_unschedulable_threads != Wrapping(0)
            || self.num_schedule_filtered_out != Wrapping(0)
            || self.num_finished != Wrapping(0)
            || self.num_retryable != Wrapping(0)
            || self.num_dropped_on_receive != Wrapping(0)
            || self.num_dropped_on_sanitization != Wrapping(0)
            || self.num_dropped_on_validate_locks != Wrapping(0)
            || self.num_dropped_on_receive_transaction_checks != Wrapping(0)
            || self.num_dropped_on_clear != Wrapping(0)
            || self.num_dropped_on_clean != Wrapping(0)
            || self.num_dropped_on_capacity != Wrapping(0)
    }

    fn reset(&mut self) {
        self.num_received = Wrapping(0);
        self.num_buffered = Wrapping(0);
        self.num_scheduled = Wrapping(0);
        self.num_unschedulable_conflicts = Wrapping(0);
        self.num_unschedulable_threads = Wrapping(0);
        self.num_schedule_filtered_out = Wrapping(0);
        self.num_finished = Wrapping(0);
        self.num_retryable = Wrapping(0);
        self.num_dropped_on_receive = Wrapping(0);
        self.num_dropped_on_sanitization = Wrapping(0);
        self.num_dropped_on_validate_locks = Wrapping(0);
        self.num_dropped_on_receive_transaction_checks = Wrapping(0);
        self.num_dropped_on_clear = Wrapping(0);
        self.num_dropped_on_clean = Wrapping(0);
        self.num_dropped_on_capacity = Wrapping(0);
        self.min_prioritization_fees = u64::MAX;
        self.max_prioritization_fees = 0;
    }

    pub fn update_priority_stats(&mut self, min_max_fees: MinMaxResult<u64>) {
        // update min/max priority
        match min_max_fees {
            itertools::MinMaxResult::NoElements => {
                // do nothing
            }
            itertools::MinMaxResult::OneElement(e) => {
                self.min_prioritization_fees = e;
                self.max_prioritization_fees = e;
            }
            itertools::MinMaxResult::MinMax(min, max) => {
                self.min_prioritization_fees = min;
                self.max_prioritization_fees = max;
            }
        }
    }

    fn get_min_priority(&self) -> u64 {
        // to avoid getting u64::max recorded by metrics / in case of edge cases
        if self.min_prioritization_fees != u64::MAX {
            self.min_prioritization_fees
        } else {
            0
        }
    }

    fn get_max_priority(&self) -> u64 {
        self.max_prioritization_fees
    }
}

#[derive(Default)]
pub struct SchedulerTimingMetrics {
    interval: IntervalSchedulerTimingMetrics,
    slot: SlotSchedulerTimingMetrics,
}

impl SchedulerTimingMetrics {
    pub fn update(&mut self, update: impl Fn(&mut SchedulerTimingMetricsInner)) {
        update(&mut self.interval.metrics);
        update(&mut self.slot.metrics);
    }

    pub fn maybe_report_and_reset_slot(&mut self, slot: Option<Slot>) {
        self.slot.maybe_report_and_reset(slot);
    }

    pub fn maybe_report_and_reset_interval(&mut self, should_report: bool) {
        self.interval.maybe_report_and_reset(should_report);
    }
}

#[derive(Default)]
struct IntervalSchedulerTimingMetrics {
    interval: AtomicInterval,
    metrics: SchedulerTimingMetricsInner,
}

#[derive(Default)]
struct SlotSchedulerTimingMetrics {
    slot: Option<Slot>,
    metrics: SchedulerTimingMetricsInner,
}

#[derive(Default)]
pub struct SchedulerTimingMetricsInner {
    /// Time spent making processing decisions.
    pub decision_time_us: Wrapping<u64>,
    /// Time spent receiving packets.
    pub receive_time_us: Wrapping<u64>,
    /// Time spent buffering packets.
    pub buffer_time_us: Wrapping<u64>,
    /// Time spent filtering transactions during scheduling.
    pub schedule_filter_time_us: Wrapping<u64>,
    /// Time spent scheduling transactions.
    pub schedule_time_us: Wrapping<u64>,
    /// Time spent clearing transactions from the container.
    pub clear_time_us: Wrapping<u64>,
    /// Time spent cleaning expired or processed transactions from the container.
    pub clean_time_us: Wrapping<u64>,
    /// Time spent receiving completed transactions.
    pub receive_completed_time_us: Wrapping<u64>,
}

impl IntervalSchedulerTimingMetrics {
    fn maybe_report_and_reset(&mut self, should_report: bool) {
        const REPORT_INTERVAL_MS: u64 = 1000;
        if self.interval.should_update(REPORT_INTERVAL_MS) {
            if should_report {
                self.metrics.report("banking_stage_scheduler_timing", None);
            }
            self.metrics.reset();
        }
    }
}

impl SlotSchedulerTimingMetrics {
    fn maybe_report_and_reset(&mut self, slot: Option<Slot>) {
        if self.slot != slot {
            // Only report if there was an assigned slot.
            if self.slot.is_some() {
                self.metrics
                    .report("banking_stage_scheduler_slot_timing", self.slot);
            }
            self.metrics.reset();
            self.slot = slot;
        }
    }
}

impl SchedulerTimingMetricsInner {
    fn report(&self, name: &'static str, slot: Option<Slot>) {
        let &Self {
            decision_time_us: Wrapping(decision_time_us),
            receive_time_us: Wrapping(receive_time_us),
            buffer_time_us: Wrapping(buffer_time_us),
            schedule_filter_time_us: Wrapping(schedule_filter_time_us),
            schedule_time_us: Wrapping(schedule_time_us),
            clear_time_us: Wrapping(clear_time_us),
            clean_time_us: Wrapping(clean_time_us),
            receive_completed_time_us: Wrapping(receive_completed_time_us),
        } = self;
        let mut datapoint = create_datapoint!(
            @point name,
            ("decision_time_us", decision_time_us, i64),
            ("receive_time_us", receive_time_us, i64),
            ("buffer_time_us", buffer_time_us, i64),
            ("schedule_filter_time_us", schedule_filter_time_us, i64),
            ("schedule_time_us", schedule_time_us, i64),
            ("clear_time_us", clear_time_us, i64),
            ("clean_time_us", clean_time_us, i64),
            (
                "receive_completed_time_us",
                receive_completed_time_us,
                i64
            )
        );
        if let Some(slot) = slot {
            datapoint.add_field_i64("slot", slot as i64);
        }
        solana_metrics::submit(datapoint, log::Level::Info);
    }

    fn reset(&mut self) {
        self.decision_time_us = Wrapping(0);
        self.receive_time_us = Wrapping(0);
        self.buffer_time_us = Wrapping(0);
        self.schedule_filter_time_us = Wrapping(0);
        self.schedule_time_us = Wrapping(0);
        self.clear_time_us = Wrapping(0);
        self.clean_time_us = Wrapping(0);
        self.receive_completed_time_us = Wrapping(0);
    }
}

pub struct SchedulingDetails {
    pub last_report: Instant,
    pub num_schedule_calls: usize,

    pub min_starting_queue_size: usize,
    pub max_starting_queue_size: usize,
    pub sum_starting_queue_size: usize, // div for report

    pub min_starting_buffer_size: usize,
    pub max_starting_buffer_size: usize,
    pub sum_starting_buffer_size: usize, // div for report

    pub sum_num_scheduled: usize,
    pub sum_unschedulable_conflicts: usize,
    pub sum_unschedulable_threads: usize,
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
    pub fn update(&mut self, scheduling_summary: &SchedulingSummary) {
        self.num_schedule_calls += 1;

        self.min_starting_queue_size = self
            .min_starting_queue_size
            .min(scheduling_summary.starting_queue_size);
        self.max_starting_queue_size = self
            .max_starting_queue_size
            .max(scheduling_summary.starting_queue_size);
        self.sum_starting_queue_size += scheduling_summary.starting_queue_size;

        self.min_starting_buffer_size = self
            .min_starting_buffer_size
            .min(scheduling_summary.starting_buffer_size);
        self.max_starting_buffer_size = self
            .max_starting_buffer_size
            .max(scheduling_summary.starting_buffer_size);
        self.sum_starting_buffer_size += scheduling_summary.starting_buffer_size;

        self.sum_num_scheduled += scheduling_summary.num_scheduled;
        self.sum_unschedulable_conflicts += scheduling_summary.num_unschedulable_conflicts;
        self.sum_unschedulable_threads += scheduling_summary.num_unschedulable_threads;
    }

    pub fn maybe_report(&mut self) {
        const REPORT_INTERVAL: Duration = Duration::from_millis(20);
        let now = Instant::now();
        if now.duration_since(self.last_report) > REPORT_INTERVAL {
            self.last_report = now;
            if self.num_schedule_calls > 0 {
                let avg_starting_queue_size =
                    self.sum_starting_queue_size / self.num_schedule_calls;
                let avg_starting_buffer_size =
                    self.sum_starting_buffer_size / self.num_schedule_calls;
                datapoint_info!(
                    "scheduling_details",
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
                *self = Self {
                    last_report: now,
                    ..Self::default()
                }
            }
        }
    }
}
