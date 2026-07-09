//! Stats about the block creation loop
use {
    solana_clock::Slot,
    solana_metrics::datapoint_info,
    std::time::{Duration, Instant},
};

/// Running min/mean/max of `bank_completion` durations, in microseconds.
#[derive(Default)]
pub(crate) struct BankCompletionTimingSamples {
    min_us: u64,
    max_us: u64,
    sum_us: u64,
    count: u64,
}

impl BankCompletionTimingSamples {
    pub(crate) fn record(&mut self, elapsed_us: u64) {
        self.min_us = if self.count == 0 {
            elapsed_us
        } else {
            self.min_us.min(elapsed_us)
        };
        self.max_us = self.max_us.max(elapsed_us);
        self.sum_us += elapsed_us;
        self.count += 1;
    }

    fn mean(&self) -> u64 {
        self.sum_us.checked_div(self.count).unwrap_or(0)
    }
}

pub(crate) struct LoopMetrics {
    pub(crate) last_report: Instant,
    pub(crate) loop_count: u64,
    pub(crate) skipped_window_behind_parent_ready_count: u64,

    pub(crate) window_production_elapsed_us: u64,
    pub(crate) bank_completion_timing_stats: BankCompletionTimingSamples,
}

impl Default for LoopMetrics {
    fn default() -> Self {
        Self {
            last_report: Instant::now(),
            loop_count: 0,
            skipped_window_behind_parent_ready_count: 0,
            window_production_elapsed_us: 0,
            bank_completion_timing_stats: BankCompletionTimingSamples::default(),
        }
    }
}

impl LoopMetrics {
    fn is_empty(&self) -> bool {
        0 == self.loop_count
            + self.bank_completion_timing_stats.count
            + self.window_production_elapsed_us
            + self.skipped_window_behind_parent_ready_count
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
                    "bank_completion_count",
                    self.bank_completion_timing_stats.count,
                    i64
                ),
                (
                    "window_production_elapsed_us",
                    self.window_production_elapsed_us,
                    i64
                ),
                (
                    "skipped_window_behind_parent_ready_count",
                    self.skipped_window_behind_parent_ready_count,
                    i64
                ),
                (
                    "bank_completion_elapsed_us_min",
                    self.bank_completion_timing_stats.min_us,
                    i64
                ),
                (
                    "bank_completion_elapsed_us_mean",
                    self.bank_completion_timing_stats.mean(),
                    i64
                ),
                (
                    "bank_completion_elapsed_us_max",
                    self.bank_completion_timing_stats.max_us,
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
    /// Indicates we have attempted fast leader handover
    pub(crate) leader_handover_fast: bool,
    /// Indicates we had to switch parent.
    pub(crate) leader_handover_sad: bool,
    pub(crate) replay_is_behind_count: u64,
    pub(crate) already_have_bank_count: u64,

    pub(crate) slot_delay_us: u64,
    pub(crate) replay_is_behind_cumulative_wait_elapsed: u64,
}

impl SlotMetrics {
    pub(crate) fn report(&mut self) {
        datapoint_info!(
            "slot-metrics",
            ("slot", self.slot, i64),
            ("attempt_count", self.attempt_start_leader_count, i64),
            ("leader_handover_fast", self.leader_handover_fast, i64),
            ("leader_handover_sad", self.leader_handover_sad, i64),
            ("replay_is_behind_count", self.replay_is_behind_count, i64),
            ("already_have_bank_count", self.already_have_bank_count, i64),
            ("slot_delay_us", self.slot_delay_us, i64),
            (
                "replay_is_behind_cumulative_wait_elapsed_us",
                self.replay_is_behind_cumulative_wait_elapsed,
                i64
            ),
        );
    }

    pub(crate) fn mark_leader_handover_fast(&mut self) {
        self.leader_handover_fast = true;
    }

    pub(crate) fn mark_leader_handover_sad(&mut self) {
        self.leader_handover_sad = true;
    }

    pub(crate) fn reset(&mut self, slot: Slot) {
        let same_slot = self.slot == slot;
        let leader_handover_fast = same_slot && self.leader_handover_fast;
        let leader_handover_sad = same_slot && self.leader_handover_sad;
        *self = Self::default();
        self.leader_handover_fast = leader_handover_fast;
        self.leader_handover_sad = leader_handover_sad;
        self.slot = slot;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slot_metrics_handover() {
        let mut metrics = SlotMetrics::default();
        metrics.reset(42);
        metrics.mark_leader_handover_fast();
        metrics.mark_leader_handover_sad();

        metrics.reset(42);
        assert!(metrics.leader_handover_fast);
        assert!(metrics.leader_handover_sad);

        metrics.reset(43);
        assert!(!metrics.leader_handover_fast);
        assert!(!metrics.leader_handover_sad);
    }
}
