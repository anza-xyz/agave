//! Stats about the block creation loop
use {solana_clock::Slot, solana_metrics::datapoint_info, solana_time_utils::timestamp};

#[derive(Default)]
pub(crate) struct BlockCreationLoopMetrics {
    pub(crate) last_report: u64,
    pub(crate) loop_count: u64,
    pub(crate) bank_timeout_completion_count: u64,
    pub(crate) skipped_window_behind_parent_ready_count: u64,

    pub(crate) window_production_elapsed: u64,
    pub(crate) bank_timeout_completion_elapsed_hist: histogram::Histogram,
}

impl BlockCreationLoopMetrics {
    fn is_empty(&self) -> bool {
        0 == self.loop_count
            + self.bank_timeout_completion_count
            + self.window_production_elapsed
            + self.skipped_window_behind_parent_ready_count
            + self.bank_timeout_completion_elapsed_hist.entries()
    }

    pub(crate) fn report(&mut self, report_interval_ms: u64) {
        // skip reporting metrics if stats is empty
        if self.is_empty() {
            return;
        }

        let now = timestamp();
        let elapsed_ms = now - self.last_report;

        if elapsed_ms > report_interval_ms {
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
            self.bank_timeout_completion_count = 0;
            self.window_production_elapsed = 0;
            self.skipped_window_behind_parent_ready_count = 0;
            self.bank_timeout_completion_elapsed_hist.clear();
            self.last_report = now;
        }
    }
}

// Metrics on slots that we attempt to start a leader block for
#[derive(Default)]
pub(crate) struct SlotMetrics {
    pub(crate) slot: Slot,
    pub(crate) attempt_count: u64,
    pub(crate) replay_is_behind_count: u64,
    pub(crate) already_have_bank_count: u64,

    pub(crate) slot_delay_hist: histogram::Histogram,
    pub(crate) replay_is_behind_cumulative_wait_elapsed: u64,
    pub(crate) replay_is_behind_wait_elapsed_hist: histogram::Histogram,
}

impl SlotMetrics {
    pub(crate) fn report(&mut self) {
        datapoint_info!(
            "slot-metrics",
            ("slot", self.slot, i64),
            ("attempt_count", self.attempt_count, i64),
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

        // reset metrics
        self.attempt_count = 0;
        self.replay_is_behind_count = 0;
        self.already_have_bank_count = 0;
        self.slot_delay_hist.clear();
        self.replay_is_behind_cumulative_wait_elapsed = 0;
        self.replay_is_behind_wait_elapsed_hist.clear();
    }
}
