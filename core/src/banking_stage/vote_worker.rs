use {
    super::{
        consumer::Consumer,
        decision_maker::{BufferedPacketsDecision, DecisionMaker},
        leader_slot_metrics::{
            CommittedTransactionsCounts, LeaderSlotMetricsTracker, ProcessTransactionsSummary,
        },
        leader_slot_timing_metrics::LeaderExecuteAndCommitTimings,
        packet_receiver::PacketReceiver,
        vote_storage::VoteStorage,
        BankingStageStats, SLOT_BOUNDARY_CHECK_PERIOD,
    },
    crate::banking_stage::consumer::{
        ExecuteAndCommitTransactionsOutput, ProcessTransactionBatchOutput,
        TARGET_NUM_TRANSACTIONS_PER_BATCH,
    },
    crossbeam_channel::RecvTimeoutError,
    solana_measure::{measure::Measure, measure_us},
    solana_poh::poh_recorder::{BankStart, PohRecorderError},
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_runtime_transaction::{
        runtime_transaction::RuntimeTransaction, transaction_with_meta::TransactionWithMeta,
    },
    solana_sdk::{
        clock::FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET,
        saturating_add_assign,
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
                let (_, consume_buffered_packets_us) = measure_us!(self.consume_buffered_packets(
                    vote_storage,
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

    fn consume_buffered_packets(
        &mut self,
        vote_storage: &mut VoteStorage,
        bank_start: &BankStart,
        banking_stage_stats: &BankingStageStats,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
    ) {
        if vote_storage.is_empty() {
            return;
        }
        let mut rebuffered_packet_count = 0;
        let mut consumed_buffered_packets_count = 0;
        let mut proc_start = Measure::start("consume_buffered_process");
        let num_packets_to_process = vote_storage.len();

        let reached_end_of_slot = vote_storage.process_packets(
            bank_start.working_bank.clone(),
            banking_stage_stats,
            slot_metrics_tracker,
            |packets_to_process_len,
             reached_end_of_slot_par,
             sanitized_transaction,
             slot_metrics_tracker| {
                self.do_process_packets(
                    bank_start,
                    reached_end_of_slot_par,
                    sanitized_transaction,
                    banking_stage_stats,
                    &mut consumed_buffered_packets_count,
                    &mut rebuffered_packet_count,
                    packets_to_process_len,
                    slot_metrics_tracker,
                )
            },
        );

        if reached_end_of_slot {
            slot_metrics_tracker.set_end_of_slot_unprocessed_buffer_len(vote_storage.len() as u64);
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
        let mut chunk_start = 0;
        let mut all_retryable_tx_indexes = vec![];
        let mut total_transaction_counts = CommittedTransactionsCounts::default();
        let mut total_cost_model_throttled_transactions_count: u64 = 0;
        let mut total_cost_model_us: u64 = 0;
        let mut total_execute_and_commit_timings = LeaderExecuteAndCommitTimings::default();
        let mut total_error_counters = TransactionErrorMetrics::default();
        let mut reached_max_poh_height = false;
        let mut overall_min_prioritization_fees: u64 = u64::MAX;
        let mut overall_max_prioritization_fees: u64 = 0;
        while chunk_start != transactions.len() {
            let chunk_end = std::cmp::min(
                transactions.len(),
                chunk_start + TARGET_NUM_TRANSACTIONS_PER_BATCH,
            );
            let process_transaction_batch_output = self.consumer.process_and_record_transactions(
                bank,
                &transactions[chunk_start..chunk_end],
                chunk_start,
            );

            let ProcessTransactionBatchOutput {
                cost_model_throttled_transactions_count: new_cost_model_throttled_transactions_count,
                cost_model_us: new_cost_model_us,
                execute_and_commit_transactions_output,
            } = process_transaction_batch_output;
            saturating_add_assign!(
                total_cost_model_throttled_transactions_count,
                new_cost_model_throttled_transactions_count
            );
            saturating_add_assign!(total_cost_model_us, new_cost_model_us);

            let ExecuteAndCommitTransactionsOutput {
                transaction_counts: new_transaction_counts,
                retryable_transaction_indexes: new_retryable_transaction_indexes,
                commit_transactions_result: new_commit_transactions_result,
                execute_and_commit_timings: new_execute_and_commit_timings,
                error_counters: new_error_counters,
                min_prioritization_fees,
                max_prioritization_fees,
                ..
            } = execute_and_commit_transactions_output;

            total_execute_and_commit_timings.accumulate(&new_execute_and_commit_timings);
            total_error_counters.accumulate(&new_error_counters);
            total_transaction_counts.accumulate(
                &new_transaction_counts,
                new_commit_transactions_result.is_ok(),
            );

            overall_min_prioritization_fees =
                std::cmp::min(overall_min_prioritization_fees, min_prioritization_fees);
            overall_max_prioritization_fees =
                std::cmp::min(overall_max_prioritization_fees, max_prioritization_fees);

            // Add the retryable txs (transactions that errored in a way that warrants a retry)
            // to the list of unprocessed txs.
            all_retryable_tx_indexes.extend_from_slice(&new_retryable_transaction_indexes);

            let should_bank_still_be_processing_txs =
                Bank::should_bank_still_be_processing_txs(bank_creation_time, bank.ns_per_slot);
            match (
                new_commit_transactions_result,
                should_bank_still_be_processing_txs,
            ) {
                (Err(PohRecorderError::MaxHeightReached), _) | (_, false) => {
                    info!(
                        "process transactions: max height reached slot: {} height: {}",
                        bank.slot(),
                        bank.tick_height()
                    );
                    // process_and_record_transactions has returned all retryable errors in
                    // transactions[chunk_start..chunk_end], so we just need to push the remaining
                    // transactions into the unprocessed queue.
                    all_retryable_tx_indexes.extend(chunk_end..transactions.len());
                    reached_max_poh_height = true;
                    break;
                }
                _ => (),
            }
            // Don't exit early on any other type of error, continue processing...
            chunk_start = chunk_end;
        }

        ProcessTransactionsSummary {
            reached_max_poh_height,
            transaction_counts: total_transaction_counts,
            retryable_transaction_indexes: all_retryable_tx_indexes,
            cost_model_throttled_transactions_count: total_cost_model_throttled_transactions_count,
            cost_model_us: total_cost_model_us,
            execute_and_commit_timings: total_execute_and_commit_timings,
            error_counters: total_error_counters,
            min_prioritization_fees: overall_min_prioritization_fees,
            max_prioritization_fees: overall_max_prioritization_fees,
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
