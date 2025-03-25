use {
    super::{
        consumer::Consumer,
        decision_maker::{BufferedPacketsDecision, DecisionMaker},
        leader_slot_metrics::LeaderSlotMetricsTracker,
        packet_receiver::PacketReceiver,
        vote_storage::VoteStorage,
        BankingStageStats, SLOT_BOUNDARY_CHECK_PERIOD,
    },
    crossbeam_channel::RecvTimeoutError,
    solana_measure::measure_us,
    solana_runtime::bank_forks::BankForks,
    std::{
        sync::{Arc, RwLock},
        time::Instant,
    },
};

pub struct VoteWorker {
    decision_maker: DecisionMaker,
    packet_receiver: PacketReceiver,
    bank_forks: Arc<RwLock<BankForks>>,
    consumer: Consumer,
    id: u32,
}

impl VoteWorker {
    pub fn new(
        decision_maker: DecisionMaker,
        packet_receiver: PacketReceiver,
        bank_forks: Arc<RwLock<BankForks>>,
        consumer: Consumer,
        id: u32,
    ) -> Self {
        Self {
            decision_maker,
            packet_receiver,
            bank_forks,
            consumer,
            id,
        }
    }

    pub fn run(mut self, mut vote_storage: VoteStorage) {
        let mut banking_stage_stats = BankingStageStats::new(self.id);
        let mut slot_metrics_tracker = LeaderSlotMetricsTracker::new(self.id);

        let mut last_metrics_update = Instant::now();

        loop {
            if !vote_storage.is_empty()
                || last_metrics_update.elapsed() >= SLOT_BOUNDARY_CHECK_PERIOD
            {
                let (_, process_buffered_packets_us) = measure_us!(self.process_buffered_packets(
                    &mut vote_storage,
                    &mut banking_stage_stats,
                    &mut slot_metrics_tracker
                ));
                slot_metrics_tracker
                    .increment_process_buffered_packets_us(process_buffered_packets_us);
                last_metrics_update = Instant::now();
            }

            match self.packet_receiver.receive_and_buffer_packets(
                &mut vote_storage,
                &mut banking_stage_stats,
                &mut slot_metrics_tracker,
            ) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => (),
                Err(RecvTimeoutError::Disconnected) => break,
            }
            banking_stage_stats.report(1000);
        }
    }

    fn process_buffered_packets(
        &mut self,
        vote_storage: &mut VoteStorage,
        banking_stage_stats: &mut BankingStageStats,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
    ) {
        if vote_storage.should_not_process() {
            return;
        }
        let (decision, make_decision_us) =
            measure_us!(self.decision_maker.make_consume_or_forward_decision());
        let metrics_action = slot_metrics_tracker.check_leader_slot_boundary(decision.bank_start());
        slot_metrics_tracker.increment_make_decision_us(make_decision_us);

        match decision {
            BufferedPacketsDecision::Consume(bank_start) => {
                // Take metrics action before consume packets (potentially resetting the
                // slot metrics tracker to the next slot) so that we don't count the
                // packet processing metrics from the next slot towards the metrics
                // of the previous slot
                slot_metrics_tracker.apply_action(metrics_action);
                let (_, consume_buffered_packets_us) =
                    measure_us!(self.consumer.consume_buffered_packets(
                        &bank_start,
                        vote_storage,
                        banking_stage_stats,
                        slot_metrics_tracker,
                    ));
                slot_metrics_tracker
                    .increment_consume_buffered_packets_us(consume_buffered_packets_us);
            }
            BufferedPacketsDecision::Forward => {
                // get current working bank from bank_forks, use it to sanitize transaction and
                // load all accounts from address loader;
                let current_bank = self.bank_forks.read().unwrap().working_bank();
                vote_storage.cache_epoch_boundary_info(&current_bank);
                vote_storage.clear();
            }
            BufferedPacketsDecision::ForwardAndHold => {
                // get current working bank from bank_forks, use it to sanitize transaction and
                // load all accounts from address loader;
                let current_bank = self.bank_forks.read().unwrap().working_bank();
                vote_storage.cache_epoch_boundary_info(&current_bank);
            }
            BufferedPacketsDecision::Hold => {}
        }
    }
}
