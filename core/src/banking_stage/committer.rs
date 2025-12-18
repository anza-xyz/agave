use {
    super::leader_slot_timing_metrics::LeaderExecuteAndCommitTimings,
    itertools::Itertools,
    solana_cost_model::cost_model::CostModel,
    solana_ledger::{
        blockstore_processor::TransactionStatusSender,
        transaction_balances::compile_collected_balances,
    },
    solana_measure::measure_us,
    solana_runtime::{
        bank::{Bank, ProcessedTransactionCounts},
        bank_utils,
        prioritization_fee_cache::PrioritizationFeeCache,
        transaction_batch::TransactionBatch,
        vote_sender_types::ReplayVoteSender,
    },
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    solana_svm::{
        transaction_balances::BalanceCollector,
        transaction_commit_result::{TransactionCommitResult, TransactionCommitResultExtensions},
        transaction_processing_result::TransactionProcessingResult,
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
        let (commit_results, commit_time_us) = measure_us!(bank.commit_transactions(
            batch.sanitized_transactions(),
            processing_results,
            processed_counts,
            &mut execute_and_commit_timings.execute_timings,
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
            );

            if let Some(prioritization_fee_cache) = self.prioritization_fee_cache.as_ref() {
                let committed_transactions = commit_results
                    .iter()
                    .zip(batch.sanitized_transactions())
                    .filter_map(|(commit_result, tx)| commit_result.was_committed().then_some(tx));
                prioritization_fee_cache.update(bank, committed_transactions);
            }

            self.collect_balances_and_send_status_batch(
                commit_results,
                bank,
                batch,
                balance_collector,
                starting_transaction_index,
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
        starting_transaction_index: Option<usize>,
    ) {
        if let Some(transaction_status_sender) = &self.transaction_status_sender {
            let sanitized_transactions = batch.sanitized_transactions();

            // Clone `SanitizedTransaction` out of `RuntimeTransaction`, this is
            // done to send over the status sender.
            let txs = sanitized_transactions
                .iter()
                .map(|tx| tx.as_sanitized_transaction().into_owned())
                .collect_vec();
            let mut transaction_index = Saturating(starting_transaction_index.unwrap_or_default());
            let (batch_transaction_indexes, tx_costs): (Vec<_>, Vec<_>) = commit_results
                .iter()
                .zip(sanitized_transactions.iter())
                .map(|(commit_result, tx)| {
                    if let Ok(committed_tx) = commit_result {
                        let Saturating(this_transaction_index) = transaction_index;
                        transaction_index += 1;

                        let tx_cost = Some(
                            CostModel::calculate_cost_for_executed_transaction(
                                tx,
                                committed_tx.executed_units,
                                committed_tx.loaded_account_stats.loaded_accounts_data_size,
                                &bank.feature_set,
                            )
                            .sum(),
                        );

                        (this_transaction_index, tx_cost)
                    } else {
                        (0, Some(0))
                    }
                })
                .unzip();

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
