//! Stats about the block creation loop
use {
    agave_math_utils::welford_stats::WelfordStats,
    solana_clock::Slot,
    solana_metrics::datapoint_info,
    std::time::{Duration, Instant},
};

pub(super) struct LoopMetrics {
    pub(super) last_report: Instant,
    pub(super) loop_count: u64,
    pub(super) bank_timeout_completion_count: u64,
    pub(super) skipped_window_behind_parent_ready_count: u64,
    pub(super) window_production_elapsed: u64,
    pub(super) bank_timeout_completion_elapsed: WelfordStats,
}

impl Default for LoopMetrics {
    fn default() -> Self {
        Self {
            last_report: Instant::now(),
            loop_count: 0,
            bank_timeout_completion_count: 0,
            skipped_window_behind_parent_ready_count: 0,
            window_production_elapsed: 0,
            bank_timeout_completion_elapsed: WelfordStats::default(),
        }
    }
}

impl LoopMetrics {
    fn is_empty(&self) -> bool {
        let Self {
            loop_count,
            bank_timeout_completion_count,
            skipped_window_behind_parent_ready_count,
            window_production_elapsed,
            bank_timeout_completion_elapsed,
            last_report: _,
        } = self;
        0 == loop_count
            + bank_timeout_completion_count
            + skipped_window_behind_parent_ready_count
            + window_production_elapsed
            + bank_timeout_completion_elapsed.count()
    }

    pub(super) fn report(&mut self, report_interval: Duration) {
        if self.is_empty() {
            return;
        }
        let Self {
            loop_count,
            bank_timeout_completion_count,
            skipped_window_behind_parent_ready_count,
            window_production_elapsed,
            bank_timeout_completion_elapsed,
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
                    "bank_timeout_completion_elapsed_mean",
                    bank_timeout_completion_elapsed.mean::<u64>(),
                    Option<i64>
                ),
                (
                    "bank_timeout_completion_elapsed_max",
                    bank_timeout_completion_elapsed.maximum::<u64>(),
                    Option<i64>
                ),
                (
                    "bank_timeout_completion_elapsed_stddev",
                    bank_timeout_completion_elapsed.stddev::<u64>(),
                    Option<i64>
                ),
                (
                    "bank_timeout_completion_elapsed_count",
                   bank_timeout_completion_elapsed.count(),
                    i64
                ),
            );
            self.reset();
        }
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

// Metrics on slots that we attempt to start a leader block for
pub(super) struct SlotMetrics {
    slot: Slot,
    pub(super) attempt_start_leader_count: u64,
    /// Indicates we have attempted fast leader handover
    leader_handover_fast: bool,
    /// Indicates we had to switch parent.
    pub(super) leader_handover_sad: bool,
    pub(super) already_have_bank_count: u64,

    pub(super) slot_delay_us: u64,
    pub(super) replay_is_behind_us: u64,
}

impl SlotMetrics {
    pub(super) fn new(slot: Slot, leader_handover_fast: bool) -> Self {
        Self {
            slot,
            attempt_start_leader_count: 0,
            leader_handover_fast,
            leader_handover_sad: false,
            already_have_bank_count: 0,
            slot_delay_us: 0,
            replay_is_behind_us: 0,
        }
    }

    pub(super) fn report(self) {
        let Self {
            slot,
            attempt_start_leader_count,
            leader_handover_fast,
            leader_handover_sad,
            already_have_bank_count,
            slot_delay_us,
            replay_is_behind_us,
        } = self;
        datapoint_info!(
            "slot-metrics",
            ("slot", slot, i64),
            ("attempt_count", attempt_start_leader_count, i64),
            ("leader_handover_fast", leader_handover_fast, i64),
            ("leader_handover_sad", leader_handover_sad, i64),
            ("already_have_bank_count", already_have_bank_count, i64),
            ("slot_delay_us", slot_delay_us, i64),
            ("replay_is_behind_us", replay_is_behind_us, i64),
        );
    }
}
