use {
    super::leader_slot_timing_metrics::LeaderExecuteAndCommitTimings,
    itertools::Itertools,
    solana_cost_model::cost_model::CostModel,
    solana_measure::measure_us,
    solana_runtime::{
        bank::{Bank, ProcessedTransactionCounts},
        bank_utils,
        prioritization_fee_cache::PrioritizationFeeCache,
        transaction_balances::compile_collected_balances,
        transaction_batch::TransactionBatch,
        transaction_execution::TransactionStatusSender,
        vote_sender_types::{ReplayVoteSendType, ReplayVoteSender},
    },
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    solana_svm::{
        transaction_balances::BalanceCollector,
        transaction_commit_result::{TransactionCommitResult, TransactionCommitResultExtensions},
        transaction_processing_result::{
            TransactionProcessingResult, TransactionProcessingResultExtensions,
        },
    },
    solana_transaction_error::TransactionError,
    std::{num::Saturating, sync::Arc},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitTransactionDetails {
    Committed {
        compute_units: u64,
        loaded_accounts_data_size: u32,
        fee_payer_post_balance: u64,
        result: Result<(), TransactionError>,
    },
    NotCommitted(TransactionError),
}

#[derive(Clone)]
pub struct Committer {
    transaction_status_sender: Option<TransactionStatusSender>,
    replay_vote_sender: ReplayVoteSender,
    prioritization_fee_cache: Option<Arc<PrioritizationFeeCache>>,
}

impl Committer {
    pub fn new(
        transaction_status_sender: Option<TransactionStatusSender>,
        replay_vote_sender: ReplayVoteSender,
        prioritization_fee_cache: Option<Arc<PrioritizationFeeCache>>,
    ) -> Self {
        Self {
            transaction_status_sender,
            replay_vote_sender,
            prioritization_fee_cache,
        }
    }

    pub(super) fn transaction_status_sender_enabled(&self) -> bool {
        self.transaction_status_sender.is_some()
    }

    pub(super) fn commit_transactions(
        &self,
        batch: &TransactionBatch<impl TransactionWithMeta>,
        processing_results: Vec<TransactionProcessingResult>,
        starting_transaction_index: Option<usize>,
        bank: &Bank,
        balance_collector: Option<BalanceCollector>,
        execute_and_commit_timings: &mut LeaderExecuteAndCommitTimings,
        processed_counts: &ProcessedTransactionCounts,
    ) -> (u64, Vec<CommitTransactionDetails>) {
        // Assign each processed transaction its index within the block. This used
        // to be computed further down, when building the status batch; it has to
        // happen before commit now, because commit is what notifies geyser of the
        // account updates these indexes label. Deriving it from
        // `processing_results` rather than `commit_results` is equivalent —
        // `Bank::create_commit_results` maps `processing_result?`, so a result is
        // committed exactly when it was processed.
        //
        // Nothing consumes these unless the node tracks indexes or records
        // transaction status, so skip the allocation otherwise.
        let batch_transaction_indexes = (starting_transaction_index.is_some()
            || self.transaction_status_sender.is_some())
        .then(|| {
            let mut next_index = Saturating(starting_transaction_index.unwrap_or_default());
            processing_results
                .iter()
                .map(|processing_result| {
                    if processing_result.was_processed() {
                        let Saturating(this_transaction_index) = next_index;
                        next_index += 1;
                        this_transaction_index
                    } else {
                        0
                    }
                })
                .collect::<Vec<_>>()
        });

        // Only label geyser account updates when the index is real. With no
        // `starting_transaction_index` the node isn't tracking indexes, and the
        // 0-based fallback above — kept for the status batch's existing
        // behavior — would misreport positions.
        let geyser_transaction_indexes =
            starting_transaction_index.and(batch_transaction_indexes.as_deref());

        let (commit_results, commit_time_us) = measure_us!(bank.commit_transactions(
            batch.sanitized_transactions(),
            processing_results,
            processed_counts,
            &mut execute_and_commit_timings.execute_timings,
            geyser_transaction_indexes,
        ));
        execute_and_commit_timings.commit_us = commit_time_us;

        let commit_transaction_statuses = commit_results
            .iter()
            .map(|commit_result| match commit_result {
                // reports actual execution CUs, and actual loaded accounts size for
                // transaction committed to block. qos_service uses these information to adjust
                // reserved block space.
                Ok(committed_tx) => CommitTransactionDetails::Committed {
                    compute_units: committed_tx.executed_units,
                    loaded_accounts_data_size: committed_tx
                        .loaded_account_stats
                        .loaded_accounts_data_size,
                    result: committed_tx.status.clone(),
                    fee_payer_post_balance: committed_tx.fee_payer_post_balance,
                },
                Err(err) => CommitTransactionDetails::NotCommitted(err.clone()),
            })
            .collect();

        let ((), find_and_send_votes_us) = measure_us!({
            bank_utils::find_and_send_votes(
                batch.sanitized_transactions(),
                &commit_results,
                Some(&self.replay_vote_sender),
                ReplayVoteSendType::VerifiedExecuted,
            );

            if let Some(prioritization_fee_cache) = self.prioritization_fee_cache.as_ref() {
                let fee_paying_transactions = commit_results
                    .iter()
                    .zip(batch.sanitized_transactions())
                    .filter_map(|(commit_result, tx)| commit_result.was_fee_paying().then_some(tx));
                prioritization_fee_cache.update(bank, fee_paying_transactions);
            }

            self.collect_balances_and_send_status_batch(
                commit_results,
                bank,
                batch,
                balance_collector,
                batch_transaction_indexes,
            );
        });
        execute_and_commit_timings.find_and_send_votes_us = find_and_send_votes_us;
        (commit_time_us, commit_transaction_statuses)
    }

    fn collect_balances_and_send_status_batch(
        &self,
        commit_results: Vec<TransactionCommitResult>,
        bank: &Bank,
        batch: &TransactionBatch<impl TransactionWithMeta>,
        balance_collector: Option<BalanceCollector>,
        batch_transaction_indexes: Option<Vec<usize>>,
    ) {
        if let Some(transaction_status_sender) = &self.transaction_status_sender {
            let sanitized_transactions = batch.sanitized_transactions();

            // Clone `SanitizedTransaction` out of `RuntimeTransaction`, this is
            // done to send over the status sender.
            let txs = sanitized_transactions
                .iter()
                .map(|tx| tx.as_sanitized_transaction().into_owned())
                .collect_vec();
            let batch_transaction_indexes = batch_transaction_indexes
                .expect("indexes are computed whenever a transaction status sender is present");
            let tx_costs = commit_results
                .iter()
                .zip(sanitized_transactions.iter())
                .map(|(commit_result, tx)| {
                    if let Ok(committed_tx) = commit_result {
                        Some(
                            CostModel::calculate_cost_for_executed_transaction(
                                tx,
                                committed_tx.executed_units,
                                committed_tx.loaded_account_stats.loaded_accounts_data_size,
                                &bank.feature_set,
                            )
                            .sum(),
                        )
                    } else {
                        Some(0)
                    }
                })
                .collect_vec();

            // There are two cases where balance_collector could be None:
            // * Balance recording is disabled. If that were the case, there would
            //   be no TransactionStatusSender, and we would not be in this branch.
            // * The batch was aborted in its entirety in SVM. In that case, there
            //   would be zero processed transactions, and commit_transactions()
            //   would not have been called at all.
            // Therefore this should always be true.
            debug_assert!(balance_collector.is_some());

            let (balances, token_balances) =
                compile_collected_balances(balance_collector.unwrap_or_default());

            transaction_status_sender.send_transaction_status_batch(
                bank.slot(),
                bank.bank_id(),
                txs,
                commit_results,
                balances,
                token_balances,
                tx_costs,
                batch_transaction_indexes,
            );
        }
    }
}
