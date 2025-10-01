use {
    solana_metrics::datapoint_info,
    std::time::{Duration, Instant},
};

const STATS_REPORT_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub(crate) struct ConsensusPoolServiceStats {
    pub(crate) add_message_failed: u32,
    pub(crate) certificates_sent: u16,
    pub(crate) certificates_dropped: u16,
    pub(crate) new_finalized_slot: u16,
    pub(crate) parent_ready_missed_window: u16,
    pub(crate) parent_ready_produce_window: u16,
    pub(crate) received_votes: u32,
    pub(crate) received_certificates: u32,
    pub(crate) standstill: bool,
    pub(crate) prune_old_state_called: u64,
    last_request_time: Instant,
}

impl ConsensusPoolServiceStats {
    pub fn new() -> Self {
        Self {
            add_message_failed: 0,
            certificates_sent: 0,
            certificates_dropped: 0,
            new_finalized_slot: 0,
            parent_ready_missed_window: 0,
            parent_ready_produce_window: 0,
            received_votes: 0,
            received_certificates: 0,
            standstill: false,
            prune_old_state_called: 0,
            last_request_time: Instant::now(),
        }
    }

    pub fn incr_u16(value: &mut u16) {
        *value = value.saturating_add(1);
    }

    pub fn incr_u32(value: &mut u32) {
        *value = value.saturating_add(1);
    }

    pub fn incr_u64(value: &mut u64) {
        *value = value.saturating_add(1);
    }

    fn reset(&mut self) {
        self.add_message_failed = 0;
        self.certificates_sent = 0;
        self.certificates_dropped = 0;
        self.new_finalized_slot = 0;
        self.parent_ready_missed_window = 0;
        self.parent_ready_produce_window = 0;
        self.received_votes = 0;
        self.received_certificates = 0;
        self.standstill = false;
        self.prune_old_state_called = 0;
        self.last_request_time = Instant::now();
    }

    fn report(&self) {
        datapoint_info!(
            "consensus_pool_service",
            ("add_message_failed", self.add_message_failed, i64),
            ("certificates_sent", self.certificates_sent, i64),
            ("certificates_dropped", self.certificates_dropped, i64),
            ("new_finalized_slot", self.new_finalized_slot, i64),
            (
                "parent_ready_missed_window",
                self.parent_ready_missed_window,
                i64
            ),
            (
                "parent_ready_produce_window",
                self.parent_ready_produce_window,
                i64
            ),
            ("received_votes", self.received_votes, i64),
            ("received_certificates", self.received_certificates, i64),
            ("standstill", self.standstill, i64),
            ("prune_old_state_called", self.prune_old_state_called, i64),
        );
    }

    pub fn maybe_report(&mut self) {
        if self.last_request_time.elapsed() >= STATS_REPORT_INTERVAL {
            self.report();
            self.reset();
        }
    }
}
