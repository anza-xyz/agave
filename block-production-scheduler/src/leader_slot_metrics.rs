use {crate::Scheduler, solana_clock::Slot, solana_metrics::datapoint_info};

#[derive(Default)]
pub(super) struct LeaderSlotMetrics {
    /// Only leader slots are reported; counters accumulate unconditionally between transitions.
    pub(super) slot: Option<Slot>,
    /// Checked transactions retained in the container, plus packets awaiting checks at slot entry.
    pub(super) starting_transactions: usize,
    /// TPU packets dequeued during the slot, before checks or capacity rejection.
    pub(super) received_packets: usize,
    /// Checked transactions accepted into the container, including replacements of evicted entries.
    pub(super) buffered_transactions: usize,
    /// Transactions discarded at ingress, during checks, by eviction, or after execution rejection.
    pub(super) dropped_transactions: usize,
    /// Dispatch attempts, including retries.
    pub(super) scheduled_transactions: usize,
    /// Transactions reported as included by execution workers.
    pub(super) completed_transactions: usize,
}

impl LeaderSlotMetrics {
    pub(super) fn report(&self) {
        let Some(slot) = self.slot else {
            return;
        };
        datapoint_info!(
            "block-production-scheduler-leader-slot",
            ("slot", slot, i64),
            ("starting_transactions", self.starting_transactions, i64),
            ("received_packets", self.received_packets, i64),
            ("buffered_transactions", self.buffered_transactions, i64),
            ("dropped_transactions", self.dropped_transactions, i64),
            ("scheduled_transactions", self.scheduled_transactions, i64),
            ("completed_transactions", self.completed_transactions, i64),
        );
    }
}

impl Scheduler {
    pub(super) fn handle_leader_slot_metrics(&mut self) {
        let slot = self.state.leader_slot();
        // As with scheduling, finish outstanding execution before advancing to the next slot.
        if self.leader_slot_metrics.slot == slot || !self.in_flight.is_empty() {
            return;
        }
        self.leader_slot_metrics.report();
        self.leader_slot_metrics = LeaderSlotMetrics {
            slot,
            starting_transactions: self
                .transactions
                .len()
                .saturating_add(self.outstanding_check_packets),
            ..LeaderSlotMetrics::default()
        };
    }
}
