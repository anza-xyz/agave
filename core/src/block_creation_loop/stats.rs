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
        let Self {
            loop_count,
            bank_timeout_completion_count,
            window_production_elapsed,
            skipped_window_behind_parent_ready_count,
            bank_timeout_completion_elapsed_hist,
            last_report: _,
        } = self;
        0 == *loop_count
            + *bank_timeout_completion_count
            + *window_production_elapsed
            + *skipped_window_behind_parent_ready_count
            + bank_timeout_completion_elapsed_hist.entries()
    }

    pub(crate) fn report(&mut self, report_interval: Duration) {
        // skip reporting metrics if stats is empty
        if self.is_empty() {
            return;
        }

        let Self {
            loop_count,
            bank_timeout_completion_count,
            window_production_elapsed,
            skipped_window_behind_parent_ready_count,
            bank_timeout_completion_elapsed_hist,
            last_report,
        } = self;

        if last_report.elapsed() > report_interval {
            datapoint_info!(
                "block-creation-loop-metrics",
                ("loop_count", *loop_count, i64),
                (
                    "bank_timeout_completion_count",
                    *bank_timeout_completion_count,
                    i64
                ),
                ("window_production_elapsed", *window_production_elapsed, i64),
                (
                    "skipped_window_behind_parent_ready_count",
                    *skipped_window_behind_parent_ready_count,
                    i64
                ),
                (
                    "bank_timeout_completion_elapsed_90pct",
                    bank_timeout_completion_elapsed_hist
                        .percentile(90.0)
                        .unwrap_or(0),
                    i64
                ),
                (
                    "bank_timeout_completion_elapsed_mean",
                    bank_timeout_completion_elapsed_hist.mean().unwrap_or(0),
                    i64
                ),
                (
                    "bank_timeout_completion_elapsed_min",
                    bank_timeout_completion_elapsed_hist.minimum().unwrap_or(0),
                    i64
                ),
                (
                    "bank_timeout_completion_elapsed_max",
                    bank_timeout_completion_elapsed_hist.maximum().unwrap_or(0),
                    i64
                ),
            );

            // reset metrics
            *self = Self::default();
        }
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
    pub(crate) fn report(&self) {
        let Self {
            slot,
            attempt_start_leader_count,
            replay_is_behind_count,
            already_have_bank_count,
            replay_is_behind_cumulative_wait_elapsed,
            replay_is_behind_wait_elapsed_hist,
            slot_delay_hist,
        } = self;
        datapoint_info!(
            "slot-metrics",
            ("slot", *slot, i64),
            ("attempt_count", *attempt_start_leader_count, i64),
            ("replay_is_behind_count", *replay_is_behind_count, i64),
            ("already_have_bank_count", *already_have_bank_count, i64),
            (
                "slot_delay_90pct",
                slot_delay_hist.percentile(90.0).unwrap_or(0),
                i64
            ),
            ("slot_delay_mean", slot_delay_hist.mean().unwrap_or(0), i64),
            (
                "slot_delay_min",
                slot_delay_hist.minimum().unwrap_or(0),
                i64
            ),
            (
                "slot_delay_max",
                slot_delay_hist.maximum().unwrap_or(0),
                i64
            ),
            (
                "replay_is_behind_cumulative_wait_elapsed",
                *replay_is_behind_cumulative_wait_elapsed,
                i64
            ),
            (
                "replay_is_behind_wait_elapsed_90pct",
                replay_is_behind_wait_elapsed_hist
                    .percentile(90.0)
                    .unwrap_or(0),
                i64
            ),
            (
                "replay_is_behind_wait_elapsed_mean",
                replay_is_behind_wait_elapsed_hist.mean().unwrap_or(0),
                i64
            ),
            (
                "replay_is_behind_wait_elapsed_min",
                replay_is_behind_wait_elapsed_hist.minimum().unwrap_or(0),
                i64
            ),
            (
                "replay_is_behind_wait_elapsed_max",
                replay_is_behind_wait_elapsed_hist.maximum().unwrap_or(0),
                i64
            ),
        );
    }

    pub(crate) fn reset(&mut self, slot: Slot) {
        *self = Self::default();
        self.slot = slot;
    }
}
