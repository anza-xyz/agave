use {
    super::{
        consumer::Consumer,
        decision_maker::{BufferedPacketsDecision, DecisionMaker},
        latest_validator_vote_packet::VoteSource,
        leader_slot_metrics::{
            CommittedTransactionsCounts, LeaderSlotMetricsTracker, ProcessTransactionsSummary,
        },
        vote_packet_receiver::VotePacketReceiver,
        vote_storage::VoteStorage,
        BankingStageStats, SLOT_BOUNDARY_CHECK_PERIOD,
    },
    crate::banking_stage::{
        consumer::{ExecuteAndCommitTransactionsOutput, ProcessTransactionBatchOutput},
        transaction_scheduler::transaction_state_container::{RuntimeTransactionView, SharedBytes},
    },
    agave_transaction_view::{
        transaction_version::TransactionVersion, transaction_view::SanitizedTransactionView,
    },
    arrayvec::ArrayVec,
    crossbeam_channel::RecvTimeoutError,
    itertools::Itertools,
    solana_accounts_db::account_locks::validate_account_locks,
    solana_clock::FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET,
    solana_measure::{measure::Measure, measure_us},
    solana_poh::poh_recorder::PohRecorderError,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_runtime_transaction::{
        runtime_transaction::RuntimeTransaction, transaction_meta::StaticMeta,
        transaction_with_meta::TransactionWithMeta,
    },
    solana_svm::{
        account_loader::TransactionCheckResult, transaction_error_metrics::TransactionErrorMetrics,
    },
    solana_svm_transaction::svm_message::SVMMessage,
    solana_time_utils::timestamp,
    solana_transaction::sanitized::MessageHash,
    solana_transaction_error::TransactionError,
    std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, RwLock,
        },
        time::Instant,
    },
};

mod transaction {
    pub use solana_transaction_error::TransactionResult as Result;
}

// This vote batch size was selected to balance the following two things:
// 1. Amortize execution overhead (Larger is better)
// 2. Constrain max entry size for FEC set packing (Smaller is better)
pub const UNPROCESSED_BUFFER_STEP_SIZE: usize = 16;

pub struct VoteWorker {
    exit: Arc<AtomicBool>,
    decision_maker: DecisionMaker,
    tpu_receiver: VotePacketReceiver,
    gossip_receiver: VotePacketReceiver,
    storage: VoteStorage,
    bank_forks: Arc<RwLock<BankForks>>,
    consumer: Consumer,
}

impl VoteWorker {
    pub fn new(
        exit: Arc<AtomicBool>,
        decision_maker: DecisionMaker,
        tpu_receiver: VotePacketReceiver,
        gossip_receiver: VotePacketReceiver,
        storage: VoteStorage,
        bank_forks: Arc<RwLock<BankForks>>,
        consumer: Consumer,
    ) -> Self {
        Self {
            exit,
            decision_maker,
            tpu_receiver,
            gossip_receiver,
            storage,
            bank_forks,
            consumer,
        }
    }

    pub fn run(mut self) {
        let mut banking_stage_stats = BankingStageStats::new();
        let mut slot_metrics_tracker = LeaderSlotMetricsTracker::default();

        let mut last_metrics_update = Instant::now();

        while !self.exit.load(Ordering::Relaxed) {
            if !self.storage.is_empty()
                || last_metrics_update.elapsed() >= SLOT_BOUNDARY_CHECK_PERIOD
            {
                let (_, process_buffered_packets_us) = measure_us!(self
                    .process_buffered_packets(&mut banking_stage_stats, &mut slot_metrics_tracker));
                slot_metrics_tracker
                    .increment_process_buffered_packets_us(process_buffered_packets_us);
                last_metrics_update = Instant::now();
            }

            // Check for new packets from the tpu receiver
            match self.tpu_receiver.receive_and_buffer_packets(
                &mut self.storage,
                &mut banking_stage_stats,
                &mut slot_metrics_tracker,
                VoteSource::Tpu,
            ) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => (),
                Err(RecvTimeoutError::Disconnected) => break,
            }
            // Check for new packets from the gossip receiver
            match self.gossip_receiver.receive_and_buffer_packets(
                &mut self.storage,
                &mut banking_stage_stats,
                &mut slot_metrics_tracker,
                VoteSource::Gossip,
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
        let (decision, make_decision_us) =
            measure_us!(self.decision_maker.make_consume_or_forward_decision());
        let metrics_action = slot_metrics_tracker.check_leader_slot_boundary(decision.bank());
        slot_metrics_tracker.increment_make_decision_us(make_decision_us);

        // Take metrics action before processing packets (potentially resetting the
        // slot metrics tracker to the next slot) so that we don't count the
        // packet processing metrics from the next slot towards the metrics
        // of the previous slot
        slot_metrics_tracker.apply_action(metrics_action);

        match decision {
            BufferedPacketsDecision::Consume(bank) => {
                let (_, consume_buffered_packets_us) = measure_us!(self.consume_buffered_packets(
                    &bank,
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
        bank: &Bank,
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
            bank,
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
        bank: &Bank,
        consumed_buffered_packets_count: &mut usize,
        rebuffered_packet_count: &mut usize,
        banking_stage_stats: &BankingStageStats,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
    ) -> bool {
        // Based on the stake distribution present in the supplied bank, drain the unprocessed votes
        // from each validator using a weighted random ordering. Votes from validators with
        // 0 stake are ignored.
        let all_vote_packets = self.storage.drain_unprocessed(bank);

        let mut reached_end_of_slot = false;
        let mut error_counters: TransactionErrorMetrics = TransactionErrorMetrics::default();
        let mut resolved_txs = ArrayVec::<_, UNPROCESSED_BUFFER_STEP_SIZE>::new();
        for chunk in Itertools::chunks(all_vote_packets.into_iter(), UNPROCESSED_BUFFER_STEP_SIZE)
            .into_iter()
        {
            debug_assert!(resolved_txs.is_empty());

            // Short circuit if we've reached the end of slot.
            if reached_end_of_slot {
                self.storage.reinsert_packets(chunk.into_iter());

                continue;
            }

            // Sanitize & resolve our chunk.
            for packet in chunk.into_iter() {
                if let Some(tx) =
                    consume_scan_should_process_packet(bank, packet, &mut error_counters)
                {
                    resolved_txs.push(tx);
                }
            }

            if let Some(retryable_vote_indices) = self.do_process_packets(
                bank,
                &mut reached_end_of_slot,
                &resolved_txs,
                banking_stage_stats,
                consumed_buffered_packets_count,
                rebuffered_packet_count,
                slot_metrics_tracker,
            ) {
                self.storage.reinsert_packets(
                    Self::extract_retryable(&mut resolved_txs, retryable_vote_indices)
                        .map(|tx| tx.into_inner_transaction().into_view()),
                );
            } else {
                self.storage.reinsert_packets(
                    resolved_txs
                        .drain(..)
                        .map(|tx| tx.into_inner_transaction().into_view()),
                );
            }
        }

        reached_end_of_slot
    }

    fn do_process_packets(
        &self,
        bank: &Bank,
        reached_end_of_slot: &mut bool,
        sanitized_transactions: &[RuntimeTransactionView],
        banking_stage_stats: &BankingStageStats,
        consumed_buffered_packets_count: &mut usize,
        rebuffered_packet_count: &mut usize,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
    ) -> Option<Vec<usize>> {
        if *reached_end_of_slot {
            return None;
        }

        let (process_transactions_summary, process_packets_transactions_us) = measure_us!(self
            .process_packets_transactions(
                bank,
                sanitized_transactions,
                banking_stage_stats,
                slot_metrics_tracker,
            ));

        slot_metrics_tracker
            .increment_process_packets_transactions_us(process_packets_transactions_us);

        let ProcessTransactionsSummary {
            reached_max_poh_height,
            retryable_transaction_indexes,
            ..
        } = process_transactions_summary;

        *reached_end_of_slot = has_reached_end_of_slot(reached_max_poh_height, bank);

        // The difference between all transactions passed to execution and the ones that
        // are retryable were the ones that were either:
        // 1) Committed into the block
        // 2) Dropped without being committed because they had some fatal error (too old,
        // duplicate signature, etc.)
        //
        // Note: This assumes that every packet deserializes into one transaction!
        *consumed_buffered_packets_count += sanitized_transactions
            .len()
            .saturating_sub(retryable_transaction_indexes.len());

        // Out of the buffered packets just retried, collect any still unprocessed
        // transactions in this batch
        *rebuffered_packet_count += retryable_transaction_indexes.len();

        slot_metrics_tracker
            .increment_retryable_packets_count(retryable_transaction_indexes.len() as u64);

        Some(retryable_transaction_indexes)
    }

    fn process_packets_transactions(
        &self,
        bank: &Bank,
        sanitized_transactions: &[impl TransactionWithMeta],
        banking_stage_stats: &BankingStageStats,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
    ) -> ProcessTransactionsSummary {
        let (mut process_transactions_summary, process_transactions_us) =
            measure_us!(self.process_transactions(bank, sanitized_transactions));
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
        bank: &Bank,
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

        let should_bank_still_be_processing_txs = bank.is_complete();
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
            retryable_transaction_indexes: retryable_transaction_indexes
                .into_iter()
                .map(|retryable_index| retryable_index.index)
                .collect(),
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

    fn extract_retryable(
        vote_packets: &mut ArrayVec<RuntimeTransactionView, 16>,
        retryable_vote_indices: Vec<usize>,
    ) -> impl Iterator<Item = RuntimeTransactionView> + '_ {
        debug_assert!(retryable_vote_indices.is_sorted());
        let mut retryable_vote_indices = retryable_vote_indices.into_iter().peekable();

        vote_packets
            .drain(..)
            .enumerate()
            .filter_map(move |(i, packet)| {
                (Some(&i) == retryable_vote_indices.peek()).then(|| {
                    retryable_vote_indices.next();

                    packet
                })
            })
    }
}

fn consume_scan_should_process_packet(
    bank: &Bank,
    packet: SanitizedTransactionView<SharedBytes>,
    error_counters: &mut TransactionErrorMetrics,
) -> Option<RuntimeTransactionView> {
    // Construct the RuntimeTransaction.
    let Ok(view) = RuntimeTransaction::<SanitizedTransactionView<_>>::try_new(
        packet,
        MessageHash::Compute,
        None,
    ) else {
        return None;
    };

    // Filter invalid votes (should never be triggered).
    if !view.is_simple_vote_transaction() {
        return None;
    }

    // Resolve the transaction (votes do not have LUTs).
    debug_assert!(!matches!(view.version(), TransactionVersion::V0));
    let Ok(view) = RuntimeTransactionView::try_new(view, None, bank.get_reserved_account_keys())
    else {
        return None;
    };

    // Check the number of locks and whether there are duplicates
    if validate_account_locks(
        view.account_keys(),
        bank.get_transaction_account_lock_limit(),
    )
    .is_err()
    {
        return None;
    }

    if Consumer::check_fee_payer_unlocked(bank, &view, error_counters).is_err() {
        return None;
    }

    Some(view)
}

fn has_reached_end_of_slot(reached_max_poh_height: bool, bank: &Bank) -> bool {
    reached_max_poh_height || bank.is_complete()
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::banking_stage::{
            tests::create_slow_genesis_config, vote_storage::tests::packet_from_slots,
        },
        solana_ledger::genesis_utils::GenesisConfigInfo,
        solana_perf::packet::BytesPacket,
        solana_runtime::genesis_utils::ValidatorVoteKeypairs,
        solana_runtime_transaction::transaction_meta::StaticMeta,
        solana_svm::account_loader::CheckedTransactionDetails,
        std::collections::HashSet,
    };

    fn to_runtime_transaction_view(packet: BytesPacket) -> RuntimeTransactionView {
        let tx =
            SanitizedTransactionView::try_new_sanitized(Arc::new(packet.buffer().to_vec()), false)
                .unwrap();
        let tx = RuntimeTransaction::<SanitizedTransactionView<_>>::try_new(
            tx,
            MessageHash::Compute,
            None,
        )
        .unwrap();

        RuntimeTransactionView::try_new(tx, None, &HashSet::default()).unwrap()
    }

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

    #[test]
    fn extract_retryable_one_all_retryable() {
        let keypair_a = ValidatorVoteKeypairs::new_rand();
        let mut packets = ArrayVec::from_iter([to_runtime_transaction_view(packet_from_slots(
            vec![(1, 1)],
            &keypair_a,
            None,
        ))]);
        let retryable_indices = vec![0];

        // Assert - Able to extract exactly one packet.
        let expected = *packets[0].message_hash();
        let mut extracted = VoteWorker::extract_retryable(&mut packets, retryable_indices);
        assert_eq!(extracted.next().unwrap().message_hash(), &expected);
        assert!(extracted.next().is_none());
    }

    #[test]
    fn extract_retryable_one_none_retryable() {
        let keypair_a = ValidatorVoteKeypairs::new_rand();
        let mut packets = ArrayVec::from_iter([to_runtime_transaction_view(packet_from_slots(
            vec![(1, 1)],
            &keypair_a,
            None,
        ))]);
        let retryable_indices = vec![];

        // Assert - Able to extract exactly zero packets.
        let mut extracted = VoteWorker::extract_retryable(&mut packets, retryable_indices);
        assert!(extracted.next().is_none());
    }

    #[test]
    fn extract_retryable_three_last_retryable() {
        let keypair_a = ValidatorVoteKeypairs::new_rand();
        let mut packets = ArrayVec::from_iter(
            [
                packet_from_slots(vec![(5, 3)], &keypair_a, None),
                packet_from_slots(vec![(6, 2)], &keypair_a, None),
                packet_from_slots(vec![(7, 1)], &keypair_a, None),
            ]
            .into_iter()
            .map(to_runtime_transaction_view),
        );
        let retryable_indices = vec![2];

        // Assert - Able to extract exactly one packet.
        let expected = *packets[2].message_hash();
        let mut extracted = VoteWorker::extract_retryable(&mut packets, retryable_indices);
        assert_eq!(extracted.next().unwrap().message_hash(), &expected);
        assert!(extracted.next().is_none());
    }

    #[test]
    fn extract_retryable_three_first_last_retryable() {
        let keypair_a = ValidatorVoteKeypairs::new_rand();
        let mut packets = ArrayVec::from_iter(
            [
                packet_from_slots(vec![(5, 3)], &keypair_a, None),
                packet_from_slots(vec![(6, 2)], &keypair_a, None),
                packet_from_slots(vec![(7, 1)], &keypair_a, None),
            ]
            .into_iter()
            .map(to_runtime_transaction_view),
        );
        let retryable_indices = vec![0, 2];

        // Assert - Able to extract exactly one packet.
        let expected0 = *packets[0].message_hash();
        let expected1 = *packets[2].message_hash();
        let mut extracted = VoteWorker::extract_retryable(&mut packets, retryable_indices);
        assert_eq!(extracted.next().unwrap().message_hash(), &expected0);
        assert_eq!(extracted.next().unwrap().message_hash(), &expected1);
        assert!(extracted.next().is_none());
    }

    #[test]
    fn test_has_reached_end_of_slot() {
        let GenesisConfigInfo { genesis_config, .. } = create_slow_genesis_config(10_000);
        let (bank, _bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);

        assert!(!has_reached_end_of_slot(false, &bank));
        assert!(has_reached_end_of_slot(true, &bank));

        bank.fill_bank_with_ticks_for_tests();
        assert!(bank.is_complete());

        assert!(has_reached_end_of_slot(false, &bank));
        assert!(has_reached_end_of_slot(true, &bank));
    }
}
