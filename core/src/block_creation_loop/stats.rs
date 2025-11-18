//! Stats about the block creation loop
use {
    histogram::Histogram,
    solana_clock::Slot,
    solana_metrics::datapoint_info,
    std::time::{Duration, Instant},
};

pub(crate) struct LoopMetrics {
    pub(crate) last_report: Instant,
    pub(crate) loop_count: u64,
    pub(crate) bank_timeout_completion_count: u64,
    pub(crate) skipped_window_behind_parent_ready_count: u64,

    pub(crate) window_production_elapsed: u64,
    pub(crate) bank_timeout_completion_elapsed_hist: Histogram,
}

impl Default for LoopMetrics {
    fn default() -> Self {
        Self {
            last_report: Instant::now(),
            loop_count: 0,
            bank_timeout_completion_count: 0,
            skipped_window_behind_parent_ready_count: 0,
            window_production_elapsed: 0,
            bank_timeout_completion_elapsed_hist: Histogram::default(),
        }
    }
}

impl LoopMetrics {
    fn is_empty(&self) -> bool {
        0 == self.loop_count
            + self.bank_timeout_completion_count
            + self.window_production_elapsed
            + self.skipped_window_behind_parent_ready_count
            + self.bank_timeout_completion_elapsed_hist.entries()
    }

    pub(crate) fn report(&mut self, report_interval: Duration) {
        // skip reporting metrics if stats is empty
        if self.is_empty() {
            return;
        }

        if self.last_report.elapsed() > report_interval {
            datapoint_info!(
                "block-creation-loop-metrics",
                ("loop_count", self.loop_count, i64),
                (
                    "bank_timeout_completion_count",
                    self.bank_timeout_completion_count,
                    i64
                ),
                (
                    "window_production_elapsed",
                    self.window_production_elapsed,
                    i64
                ),
                (
                    "skipped_window_behind_parent_ready_count",
                    self.skipped_window_behind_parent_ready_count,
                    i64
                ),
                (
                    "bank_timeout_completion_elapsed_90pct",
                    self.bank_timeout_completion_elapsed_hist
                        .percentile(90.0)
                        .unwrap_or(0),
                    i64
                ),
                (
                    "bank_timeout_completion_elapsed_mean",
                    self.bank_timeout_completion_elapsed_hist
                        .mean()
                        .unwrap_or(0),
                    i64
                ),
                (
                    "bank_timeout_completion_elapsed_min",
                    self.bank_timeout_completion_elapsed_hist
                        .minimum()
                        .unwrap_or(0),
                    i64
                ),
                (
                    "bank_timeout_completion_elapsed_max",
                    self.bank_timeout_completion_elapsed_hist
                        .maximum()
                        .unwrap_or(0),
                    i64
                ),
            );

            // reset metrics
            self.reset();
        }
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

// Metrics on slots that we attempt to start a leader block for
#[derive(Default)]
pub(crate) struct SlotMetrics {
    pub(crate) slot: Slot,
    pub(crate) attempt_start_leader_count: u64,
    pub(crate) replay_is_behind_count: u64,
    pub(crate) already_have_bank_count: u64,

    pub(crate) slot_delay_hist: Histogram,
    pub(crate) replay_is_behind_cumulative_wait_elapsed: u64,
    pub(crate) replay_is_behind_wait_elapsed_hist: Histogram,
}

impl SlotMetrics {
    pub(crate) fn report(&mut self) {
        datapoint_info!(
            "slot-metrics",
            ("slot", self.slot, i64),
            ("attempt_count", self.attempt_start_leader_count, i64),
            ("replay_is_behind_count", self.replay_is_behind_count, i64),
            ("already_have_bank_count", self.already_have_bank_count, i64),
            (
                "slot_delay_90pct",
                self.slot_delay_hist.percentile(90.0).unwrap_or(0),
                i64
            ),
            (
                "slot_delay_mean",
                self.slot_delay_hist.mean().unwrap_or(0),
                i64
            ),
            (
                "slot_delay_min",
                self.slot_delay_hist.minimum().unwrap_or(0),
                i64
            ),
            (
                "slot_delay_max",
                self.slot_delay_hist.maximum().unwrap_or(0),
                i64
            ),
            (
                "replay_is_behind_cumulative_wait_elapsed",
                self.replay_is_behind_cumulative_wait_elapsed,
                i64
            ),
            (
                "replay_is_behind_wait_elapsed_90pct",
                self.replay_is_behind_wait_elapsed_hist
                    .percentile(90.0)
                    .unwrap_or(0),
                i64
            ),
            (
                "replay_is_behind_wait_elapsed_mean",
                self.replay_is_behind_wait_elapsed_hist.mean().unwrap_or(0),
                i64
            ),
            (
                "replay_is_behind_wait_elapsed_min",
                self.replay_is_behind_wait_elapsed_hist
                    .minimum()
                    .unwrap_or(0),
                i64
            ),
            (
                "replay_is_behind_wait_elapsed_max",
                self.replay_is_behind_wait_elapsed_hist
                    .maximum()
                    .unwrap_or(0),
                i64
            ),
        );
    }

    pub(crate) fn reset(&mut self, slot: Slot) {
        *self = Self::default();
        self.slot = slot;
    }
}
