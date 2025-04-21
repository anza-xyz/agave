use {
    super::{
        consumer::Consumer,
        decision_maker::{BufferedPacketsDecision, DecisionMaker},
        immutable_deserialized_packet::ImmutableDeserializedPacket,
        leader_slot_metrics::{
            CommittedTransactionsCounts, LeaderSlotMetricsTracker, ProcessTransactionsSummary,
        },
        packet_receiver::PacketReceiver,
        vote_storage::VoteStorage,
        BankingStageStats, SLOT_BOUNDARY_CHECK_PERIOD,
    },
    crate::banking_stage::consumer::{
        ExecuteAndCommitTransactionsOutput, ProcessTransactionBatchOutput,
    },
    arrayvec::ArrayVec,
    crossbeam_channel::RecvTimeoutError,
    solana_accounts_db::account_locks::validate_account_locks,
    solana_measure::{measure::Measure, measure_us},
    solana_poh::poh_recorder::{BankStart, PohRecorderError},
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_runtime_transaction::{
        runtime_transaction::RuntimeTransaction, transaction_with_meta::TransactionWithMeta,
    },
    solana_sdk::{
        clock::FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET,
        timing::timestamp,
        transaction::{self, SanitizedTransaction, TransactionError},
    },
    solana_svm::{
        account_loader::TransactionCheckResult, transaction_error_metrics::TransactionErrorMetrics,
    },
    std::{
        sync::{atomic::Ordering, Arc, RwLock},
        time::Instant,
    },
};

// This vote batch size was selected to balance the following two things:
// 1. Amortize execution overhead (Larger is better)
// 2. Constrain max entry size for FEC set packing (Smaller is better)
pub const UNPROCESSED_BUFFER_STEP_SIZE: usize = 16;

pub struct VoteWorker {
    decision_maker: DecisionMaker,
    packet_receiver: PacketReceiver,
    storage: VoteStorage,
    bank_forks: Arc<RwLock<BankForks>>,
    consumer: Consumer,
    id: u32,
}

impl VoteWorker {
    pub fn new(
        decision_maker: DecisionMaker,
        packet_receiver: PacketReceiver,
        storage: VoteStorage,
        bank_forks: Arc<RwLock<BankForks>>,
        consumer: Consumer,
        id: u32,
    ) -> Self {
        Self {
            decision_maker,
            packet_receiver,
            storage,
            bank_forks,
            consumer,
            id,
        }
    }

    pub fn run(mut self) {
        let mut banking_stage_stats = BankingStageStats::new(self.id);
        let mut slot_metrics_tracker = LeaderSlotMetricsTracker::new(self.id);

        let mut last_metrics_update = Instant::now();

        loop {
            if !self.storage.is_empty()
                || last_metrics_update.elapsed() >= SLOT_BOUNDARY_CHECK_PERIOD
            {
                let (_, process_buffered_packets_us) = measure_us!(self
                    .process_buffered_packets(&mut banking_stage_stats, &mut slot_metrics_tracker));
                slot_metrics_tracker
                    .increment_process_buffered_packets_us(process_buffered_packets_us);
                last_metrics_update = Instant::now();
            }

            match self.packet_receiver.receive_and_buffer_packets(
                &mut self.storage,
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
        banking_stage_stats: &mut BankingStageStats,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
    ) {
        if self.storage.should_not_process() {
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
                let (_, consume_buffered_packets_us) = measure_us!(self.consume_buffered_packets(
                    &bank_start,
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
                self.storage.cache_epoch_boundary_info(&current_bank);
                self.storage.clear();
            }
            BufferedPacketsDecision::ForwardAndHold => {
                // get current working bank from bank_forks, use it to sanitize transaction and
                // load all accounts from address loader;
                let current_bank = self.bank_forks.read().unwrap().working_bank();
                self.storage.cache_epoch_boundary_info(&current_bank);
            }
            BufferedPacketsDecision::Hold => {}
        }
    }

    fn consume_buffered_packets(
        &mut self,
        bank_start: &BankStart,
        banking_stage_stats: &BankingStageStats,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
    ) {
        if self.storage.is_empty() {
            return;
        }

        let mut consumed_buffered_packets_count = 0;
        let mut rebuffered_packet_count = 0;
        let mut proc_start = Measure::start("consume_buffered_process");
        let num_packets_to_process = self.storage.len();

        let reached_end_of_slot = self.process_packets(
            bank_start,
            &mut consumed_buffered_packets_count,
            &mut rebuffered_packet_count,
            banking_stage_stats,
            slot_metrics_tracker,
        );

        if reached_end_of_slot {
            slot_metrics_tracker.set_end_of_slot_unprocessed_buffer_len(self.storage.len() as u64);
        }

        proc_start.stop();
        debug!(
            "@{:?} done processing buffered batches: {} time: {:?}ms tx count: {} tx/s: {}",
            timestamp(),
            num_packets_to_process,
            proc_start.as_ms(),
            consumed_buffered_packets_count,
            (consumed_buffered_packets_count as f32) / (proc_start.as_s())
        );

        banking_stage_stats
            .consume_buffered_packets_elapsed
            .fetch_add(proc_start.as_us(), Ordering::Relaxed);
        banking_stage_stats
            .rebuffered_packets_count
            .fetch_add(rebuffered_packet_count, Ordering::Relaxed);
        banking_stage_stats
            .consumed_buffered_packets_count
            .fetch_add(consumed_buffered_packets_count, Ordering::Relaxed);
    }

    // returns `true` if the end of slot is reached
    fn process_packets(
        &mut self,
        bank_start: &BankStart,
        consumed_buffered_packets_count: &mut usize,
        rebuffered_packet_count: &mut usize,
        banking_stage_stats: &BankingStageStats,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
    ) -> bool {
        // Based on the stake distribution present in the supplied bank, drain the unprocessed votes
        // from each validator using a weighted random ordering. Votes from validators with
        // 0 stake are ignored.
        let all_vote_packets = self.storage.drain_unprocessed(&bank_start.working_bank);

        let mut reached_end_of_slot = false;
        let mut sanitized_transactions = Vec::with_capacity(UNPROCESSED_BUFFER_STEP_SIZE);
        let mut error_counters: TransactionErrorMetrics = TransactionErrorMetrics::default();
        let mut vote_packets =
            ArrayVec::<Arc<ImmutableDeserializedPacket>, UNPROCESSED_BUFFER_STEP_SIZE>::new();
        for chunk in all_vote_packets.chunks(UNPROCESSED_BUFFER_STEP_SIZE) {
            vote_packets.clear();
            chunk.iter().for_each(|packet| {
                if consume_scan_should_process_packet(
                    &bank_start.working_bank,
                    banking_stage_stats,
                    packet,
                    reached_end_of_slot,
                    &mut error_counters,
                    &mut sanitized_transactions,
                    slot_metrics_tracker,
                ) {
                    vote_packets.push(packet.clone());
                }
            });

            if let Some(retryable_vote_indices) = self.do_process_packets(
                bank_start,
                &mut reached_end_of_slot,
                &mut sanitized_transactions,
                banking_stage_stats,
                consumed_buffered_packets_count,
                rebuffered_packet_count,
                vote_packets.len(),
                slot_metrics_tracker,
            ) {
                self.storage.reinsert_packets(
                    retryable_vote_indices
                        .into_iter()
                        .map(|index| vote_packets[index].clone()),
                );
            } else {
                self.storage.reinsert_packets(vote_packets.drain(..));
            }
        }

        reached_end_of_slot
    }

    fn do_process_packets(
        &self,
        bank_start: &BankStart,
        reached_end_of_slot: &mut bool,
        sanitized_transactions: &mut Vec<RuntimeTransaction<SanitizedTransaction>>,
        banking_stage_stats: &BankingStageStats,
        consumed_buffered_packets_count: &mut usize,
        rebuffered_packet_count: &mut usize,
        packets_to_process_len: usize,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
    ) -> Option<Vec<usize>> {
        if *reached_end_of_slot {
            return None;
        }

        let (process_transactions_summary, process_packets_transactions_us) = measure_us!(self
            .process_packets_transactions(
                &bank_start.working_bank,
                &bank_start.bank_creation_time,
                sanitized_transactions,
                banking_stage_stats,
                slot_metrics_tracker,
            ));

        slot_metrics_tracker
            .increment_process_packets_transactions_us(process_packets_transactions_us);

        // Clear sanitized_transactions for next iteration
        sanitized_transactions.clear();

        let ProcessTransactionsSummary {
            reached_max_poh_height,
            retryable_transaction_indexes,
            ..
        } = process_transactions_summary;

        if reached_max_poh_height || !bank_start.should_working_bank_still_be_processing_txs() {
            *reached_end_of_slot = true;
        }

        // The difference between all transactions passed to execution and the ones that
        // are retryable were the ones that were either:
        // 1) Committed into the block
        // 2) Dropped without being committed because they had some fatal error (too old,
        // duplicate signature, etc.)
        //
        // Note: This assumes that every packet deserializes into one transaction!
        *consumed_buffered_packets_count +=
            packets_to_process_len.saturating_sub(retryable_transaction_indexes.len());

        // Out of the buffered packets just retried, collect any still unprocessed
        // transactions in this batch
        *rebuffered_packet_count += retryable_transaction_indexes.len();

        slot_metrics_tracker
            .increment_retryable_packets_count(retryable_transaction_indexes.len() as u64);

        Some(retryable_transaction_indexes)
    }

    fn process_packets_transactions(
        &self,
        bank: &Arc<Bank>,
        bank_creation_time: &Instant,
        sanitized_transactions: &[impl TransactionWithMeta],
        banking_stage_stats: &BankingStageStats,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
    ) -> ProcessTransactionsSummary {
        let (mut process_transactions_summary, process_transactions_us) = measure_us!(
            self.process_transactions(bank, bank_creation_time, sanitized_transactions)
        );
        slot_metrics_tracker.increment_process_transactions_us(process_transactions_us);
        banking_stage_stats
            .transaction_processing_elapsed
            .fetch_add(process_transactions_us, Ordering::Relaxed);

        let ProcessTransactionsSummary {
            ref retryable_transaction_indexes,
            ref error_counters,
            ..
        } = process_transactions_summary;

        slot_metrics_tracker.accumulate_process_transactions_summary(&process_transactions_summary);
        slot_metrics_tracker.accumulate_transaction_errors(error_counters);

        // Filter out the retryable transactions that are too old
        let (filtered_retryable_transaction_indexes, filter_retryable_packets_us) =
            measure_us!(Self::filter_pending_packets_from_pending_txs(
                bank,
                sanitized_transactions,
                retryable_transaction_indexes,
            ));
        slot_metrics_tracker.increment_filter_retryable_packets_us(filter_retryable_packets_us);
        banking_stage_stats
            .filter_pending_packets_elapsed
            .fetch_add(filter_retryable_packets_us, Ordering::Relaxed);

        let retryable_packets_filtered_count = retryable_transaction_indexes
            .len()
            .saturating_sub(filtered_retryable_transaction_indexes.len());
        slot_metrics_tracker
            .increment_retryable_packets_filtered_count(retryable_packets_filtered_count as u64);

        banking_stage_stats
            .dropped_forward_packets_count
            .fetch_add(retryable_packets_filtered_count, Ordering::Relaxed);

        process_transactions_summary.retryable_transaction_indexes =
            filtered_retryable_transaction_indexes;
        process_transactions_summary
    }

    /// Sends transactions to the bank.
    ///
    /// Returns the number of transactions successfully processed by the bank, which may be less
    /// than the total number if max PoH height was reached and the bank halted
    fn process_transactions(
        &self,
        bank: &Arc<Bank>,
        bank_creation_time: &Instant,
        transactions: &[impl TransactionWithMeta],
    ) -> ProcessTransactionsSummary {
        let process_transaction_batch_output = self
            .consumer
            .process_and_record_transactions(bank, transactions);

        let ProcessTransactionBatchOutput {
            cost_model_throttled_transactions_count,
            cost_model_us,
            execute_and_commit_transactions_output,
        } = process_transaction_batch_output;

        let ExecuteAndCommitTransactionsOutput {
            transaction_counts,
            retryable_transaction_indexes,
            commit_transactions_result,
            execute_and_commit_timings,
            error_counters,
            ..
        } = execute_and_commit_transactions_output;

        let mut total_transaction_counts = CommittedTransactionsCounts::default();
        total_transaction_counts
            .accumulate(&transaction_counts, commit_transactions_result.is_ok());

        let should_bank_still_be_processing_txs =
            Bank::should_bank_still_be_processing_txs(bank_creation_time, bank.ns_per_slot);
        let reached_max_poh_height = match (
            commit_transactions_result,
            should_bank_still_be_processing_txs,
        ) {
            (Err(PohRecorderError::MaxHeightReached), _) | (_, false) => {
                info!(
                    "process transactions: max height reached slot: {} height: {}",
                    bank.slot(),
                    bank.tick_height()
                );
                true
            }
            _ => false,
        };

        ProcessTransactionsSummary {
            reached_max_poh_height,
            transaction_counts: total_transaction_counts,
            retryable_transaction_indexes,
            cost_model_throttled_transactions_count,
            cost_model_us,
            execute_and_commit_timings,
            error_counters,
        }
    }

    /// This function filters pending packets that are still valid
    /// # Arguments
    /// * `transactions` - a batch of transactions deserialized from packets
    /// * `pending_indexes` - identifies which indexes in the `transactions` list are still pending
    fn filter_pending_packets_from_pending_txs(
        bank: &Bank,
        transactions: &[impl TransactionWithMeta],
        pending_indexes: &[usize],
    ) -> Vec<usize> {
        let filter =
            Self::prepare_filter_for_pending_transactions(transactions.len(), pending_indexes);

        let results = bank.check_transactions_with_forwarding_delay(
            transactions,
            &filter,
            FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET,
        );

        Self::filter_valid_transaction_indexes(&results)
    }

    /// This function creates a filter of transaction results with Ok() for every pending
    /// transaction. The non-pending transactions are marked with TransactionError
    fn prepare_filter_for_pending_transactions(
        transactions_len: usize,
        pending_tx_indexes: &[usize],
    ) -> Vec<transaction::Result<()>> {
        let mut mask = vec![Err(TransactionError::BlockhashNotFound); transactions_len];
        pending_tx_indexes.iter().for_each(|x| mask[*x] = Ok(()));
        mask
    }

    /// This function returns a vector containing index of all valid transactions. A valid
    /// transaction has result Ok() as the value
    fn filter_valid_transaction_indexes(valid_txs: &[TransactionCheckResult]) -> Vec<usize> {
        valid_txs
            .iter()
            .enumerate()
            .filter_map(|(index, res)| res.as_ref().ok().map(|_| index))
            .collect()
    }
}

fn consume_scan_should_process_packet(
    bank: &Bank,
    banking_stage_stats: &BankingStageStats,
    packet: &ImmutableDeserializedPacket,
    reached_end_of_slot: bool,
    error_counters: &mut TransactionErrorMetrics,
    sanitized_transactions: &mut Vec<RuntimeTransaction<SanitizedTransaction>>,
    slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
) -> bool {
    // If end of the slot, return should process (quick loop after reached end of slot)
    if reached_end_of_slot {
        return true;
    }

    // Try to sanitize the packet. Ignore deactivation slot since we are
    // immediately attempting to process the transaction.
    let (maybe_sanitized_transaction, sanitization_time_us) = measure_us!(packet
        .build_sanitized_transaction(
            bank.vote_only_bank(),
            bank,
            bank.get_reserved_account_keys(),
        )
        .map(|(tx, _deactivation_slot)| tx));

    slot_metrics_tracker.increment_transactions_from_packets_us(sanitization_time_us);
    banking_stage_stats
        .packet_conversion_elapsed
        .fetch_add(sanitization_time_us, Ordering::Relaxed);

    if let Some(sanitized_transaction) = maybe_sanitized_transaction {
        let message = sanitized_transaction.message();

        // Check the number of locks and whether there are duplicates
        if validate_account_locks(
            message.account_keys(),
            bank.get_transaction_account_lock_limit(),
        )
        .is_err()
        {
            return false;
        }

        if Consumer::check_fee_payer_unlocked(bank, &sanitized_transaction, error_counters).is_err()
        {
            return false;
        }
        sanitized_transactions.push(sanitized_transaction);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_svm::account_loader::CheckedTransactionDetails};

    #[test]
    fn test_bank_prepare_filter_for_pending_transaction() {
        assert_eq!(
            VoteWorker::prepare_filter_for_pending_transactions(6, &[2, 4, 5]),
            vec![
                Err(TransactionError::BlockhashNotFound),
                Err(TransactionError::BlockhashNotFound),
                Ok(()),
                Err(TransactionError::BlockhashNotFound),
                Ok(()),
                Ok(())
            ]
        );

        assert_eq!(
            VoteWorker::prepare_filter_for_pending_transactions(6, &[0, 2, 3]),
            vec![
                Ok(()),
                Err(TransactionError::BlockhashNotFound),
                Ok(()),
                Ok(()),
                Err(TransactionError::BlockhashNotFound),
                Err(TransactionError::BlockhashNotFound),
            ]
        );
    }

    #[test]
    fn test_bank_filter_valid_transaction_indexes() {
        assert_eq!(
            VoteWorker::filter_valid_transaction_indexes(&[
                Err(TransactionError::BlockhashNotFound),
                Err(TransactionError::BlockhashNotFound),
                Ok(CheckedTransactionDetails::default()),
                Err(TransactionError::BlockhashNotFound),
                Ok(CheckedTransactionDetails::default()),
                Ok(CheckedTransactionDetails::default()),
            ]),
            [2, 4, 5]
        );

        assert_eq!(
            VoteWorker::filter_valid_transaction_indexes(&[
                Ok(CheckedTransactionDetails::default()),
                Err(TransactionError::BlockhashNotFound),
                Err(TransactionError::BlockhashNotFound),
                Ok(CheckedTransactionDetails::default()),
                Ok(CheckedTransactionDetails::default()),
                Ok(CheckedTransactionDetails::default()),
            ]),
            [0, 3, 4, 5]
        );
    }
}
