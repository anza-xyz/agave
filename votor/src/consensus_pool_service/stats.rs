use {
    solana_metrics::datapoint_info,
    std::{
        num::Saturating,
        time::{Duration, Instant},
    },
};

const STATS_REPORT_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub(super) struct ConsensusPoolServiceStats {
    pub(super) add_message_failed: Saturating<usize>,
    pub(super) certificates_sent: Saturating<usize>,
    pub(super) certificates_dropped: Saturating<usize>,
    pub(super) certificates_skipped_unstaked: Saturating<usize>,
    pub(super) new_finalized_slot: Saturating<usize>,
    pub(super) parent_ready_missed_window: Saturating<usize>,
    pub(super) parent_ready_produce_window: Saturating<usize>,
    pub(super) own_votes_received: Saturating<u64>,
    pub(super) footer_certs_received: Saturating<u64>,
    pub(super) vote_aggregates_received: Saturating<u64>,
    pub(super) certs_received: Saturating<u64>,
    pub(super) own_message_receive_limit_reached: Saturating<u64>,
    pub(super) consensus_message_batch_receive_limit_reached: Saturating<u64>,
    pub(super) standstill: bool,
    pub(super) prune_old_state_called: Saturating<usize>,
    pub(crate) pending_safe_to_notar_repair_sent: Saturating<usize>,
    pub(crate) pending_safe_to_notar_resolved: Saturating<usize>,

    last_request_time: Instant,
}

impl ConsensusPoolServiceStats {
    pub fn new() -> Self {
        Self {
            add_message_failed: Saturating(0),
            certificates_sent: Saturating(0),
            certificates_dropped: Saturating(0),
            certificates_skipped_unstaked: Saturating(0),
            new_finalized_slot: Saturating(0),
            parent_ready_missed_window: Saturating(0),
            parent_ready_produce_window: Saturating(0),
            own_message_receive_limit_reached: Saturating(0),
            consensus_message_batch_receive_limit_reached: Saturating(0),
            own_votes_received: Saturating(0),
            vote_aggregates_received: Saturating(0),
            certs_received: Saturating(0),
            footer_certs_received: Saturating(0),
            standstill: false,
            prune_old_state_called: Saturating(0),
            pending_safe_to_notar_repair_sent: Saturating(0),
            pending_safe_to_notar_resolved: Saturating(0),
            last_request_time: Instant::now(),
        }
    }

    pub(super) fn do_report(&self) {
        let &Self {
            add_message_failed,
            certificates_sent,
            certificates_dropped,
            certificates_skipped_unstaked,
            new_finalized_slot,
            parent_ready_missed_window,
            parent_ready_produce_window,
            own_votes_received,
            footer_certs_received,
            vote_aggregates_received,
            certs_received,
            own_message_receive_limit_reached,
            consensus_message_batch_receive_limit_reached,
            standstill,
            prune_old_state_called,
            pending_safe_to_notar_repair_sent,
            pending_safe_to_notar_resolved,
            last_request_time: _,
        } = self;
        datapoint_info!(
            "consensus_pool_service",
            ("add_message_failed", add_message_failed.0, i64),
            ("certificates_sent", certificates_sent.0, i64),
            ("certificates_dropped", certificates_dropped.0, i64),
            (
                "certificates_skipped_unstaked",
                certificates_skipped_unstaked.0,
                i64
            ),
            ("new_finalized_slot", new_finalized_slot.0, i64),
            (
                "parent_ready_missed_window",
                parent_ready_missed_window.0,
                i64
            ),
            (
                "parent_ready_produce_window",
                parent_ready_produce_window.0,
                i64
            ),
            ("vote_aggregates_received", vote_aggregates_received.0, i64),
            ("own_votes_received", own_votes_received.0, i64),
            ("certs_received", certs_received.0, i64),
            (
                "own_message_receive_limit_reached",
                own_message_receive_limit_reached.0,
                i64
            ),
            (
                "consensus_message_batch_receive_limit_reached",
                consensus_message_batch_receive_limit_reached.0,
                i64
            ),
            ("footer_certs_received", footer_certs_received.0, i64),
            ("in_standstill_bool", standstill, bool),
            ("prune_old_state_called", prune_old_state_called.0, i64),
            (
                "pending_safe_to_notar_repair_sent",
                pending_safe_to_notar_repair_sent.0,
                i64
            ),
            (
                "pending_safe_to_notar_resolved",
                pending_safe_to_notar_resolved.0,
                i64
            ),
        );
    }

    pub(super) fn maybe_report(&mut self) {
        if self.last_request_time.elapsed() >= STATS_REPORT_INTERVAL {
            self.do_report();
            *self = Self::new();
        }
    }
}
