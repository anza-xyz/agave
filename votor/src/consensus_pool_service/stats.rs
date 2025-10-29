use {
    solana_metrics::datapoint_info,
    std::{
        num::Saturating,
        time::{Duration, Instant},
    },
};

const STATS_REPORT_INTERVAL: Duration = Duration::from_secs(1);

pub(super) struct Stats {
    pub(super) add_message_failed: Saturating<usize>,
    pub(super) certificates_sent: Saturating<usize>,
    pub(super) certificates_dropped: Saturating<usize>,
    pub(super) new_finalized_slot: Saturating<usize>,
    pub(super) parent_ready_missed_window: Saturating<usize>,
    pub(super) parent_ready_produce_window: Saturating<usize>,
    pub(super) received_votes: Saturating<usize>,
    pub(super) received_certificates: Saturating<usize>,
    pub(super) standstill: bool,
    pub(super) prune_old_state_called: Saturating<usize>,
    last_request_time: Instant,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            add_message_failed: Saturating(0),
            certificates_sent: Saturating(0),
            certificates_dropped: Saturating(0),
            new_finalized_slot: Saturating(0),
            parent_ready_missed_window: Saturating(0),
            parent_ready_produce_window: Saturating(0),
            received_votes: Saturating(0),
            received_certificates: Saturating(0),
            standstill: false,
            prune_old_state_called: Saturating(0),
            last_request_time: Instant::now(),
        }
    }
}

impl Stats {
    fn report(&self) {
        let &Self {
            add_message_failed: Saturating(add_message_failed),
            certificates_sent: Saturating(certificates_sent),
            certificates_dropped: Saturating(certificates_dropped),
            new_finalized_slot: Saturating(new_finalized_slot),
            parent_ready_missed_window: Saturating(parent_ready_missed_window),
            parent_ready_produce_window: Saturating(parent_ready_produce_window),
            received_votes: Saturating(received_votes),
            received_certificates: Saturating(received_certificates),
            standstill,
            prune_old_state_called: Saturating(prune_old_state_called),
            last_request_time: _,
        } = self;
        datapoint_info!(
            "consensus_pool_service",
            ("add_message_failed", add_message_failed, i64),
            ("certificates_sent", certificates_sent, i64),
            ("certificates_dropped", certificates_dropped, i64),
            ("new_finalized_slot", new_finalized_slot, i64),
            (
                "parent_ready_missed_window",
                parent_ready_missed_window,
                i64
            ),
            (
                "parent_ready_produce_window",
                parent_ready_produce_window,
                i64
            ),
            ("received_votes", received_votes, i64),
            ("received_certificates", received_certificates, i64),
            ("entered_standstill_bool", standstill, bool),
            ("prune_old_state_called", prune_old_state_called, i64),
        );
    }

    pub(super) fn maybe_report(&mut self) {
        if self.last_request_time.elapsed() >= STATS_REPORT_INTERVAL {
            self.report();
            *self = Self::default();
        }
    }
}
