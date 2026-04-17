//! Quality of service for block producer.
//! Provides logic and functions to allow a Leader to prioritize
//! how transactions are included in blocks, and optimize those blocks.
//!

use {
    super::committer::CommitTransactionDetails,
    agave_feature_set::FeatureSet,
    solana_cost_model::{
        cost_model::CostModel, cost_tracker::UpdatedCosts, transaction_cost::TransactionCost,
    },
    solana_runtime::bank::Bank,
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    solana_transaction_error::TransactionError,
};

mod transaction {
    pub use solana_transaction_error::TransactionResult as Result;
}

// QosService is local to each banking thread, each instance of QosService provides services to
// one banking thread.
// Banking thread calls `report_metrics(slot)` at end of `process_and_record_transaction()`, or any time
// it wants.
//
pub struct QosService;

impl QosService {
    /// Calculate cost of transactions, if not already filtered out, determine which ones to
    /// include in the slot, and accumulate costs in the cost tracker.
    /// Returns a vector of results containing selected transaction costs, and the number of
    /// transactions that were *NOT* selected.
    pub fn select_and_accumulate_transaction_costs<'a, Tx: TransactionWithMeta>(
        bank: &Bank,
        transactions: &'a [Tx],
        pre_results: impl Iterator<Item = transaction::Result<()>>,
    ) -> (Vec<transaction::Result<TransactionCost<'a, Tx>>>, u64) {
        let transaction_costs =
            Self::compute_transaction_costs(&bank.feature_set, transactions.iter(), pre_results);
        let (transactions_qos_cost_results, num_included) = Self::select_transactions_per_cost(
            transactions.iter(),
            transaction_costs.into_iter(),
            bank,
        );
        let cost_model_throttled_transactions_count =
            transactions.len().saturating_sub(num_included) as u64;

        (
            transactions_qos_cost_results,
            cost_model_throttled_transactions_count,
        )
    }

    // invoke cost_model to calculate cost for the given list of transactions that have not
    // been filtered out already.
    fn compute_transaction_costs<'a, Tx: TransactionWithMeta>(
        feature_set: &FeatureSet,
        transactions: impl Iterator<Item = &'a Tx>,
        pre_results: impl Iterator<Item = transaction::Result<()>>,
    ) -> Vec<transaction::Result<TransactionCost<'a, Tx>>> {
        transactions
            .zip(pre_results)
            .map(|(tx, pre_result)| pre_result.map(|()| CostModel::calculate_cost(tx, feature_set)))
            .collect()
    }

    /// Given a list of transactions and their costs, this function returns a corresponding
    /// list of Results that indicate if a transaction is selected to be included in the current block,
    /// and a count of the number of transactions that would fit in the block
    fn select_transactions_per_cost<'a, Tx: TransactionWithMeta>(
        transactions: impl Iterator<Item = &'a Tx>,
        transactions_costs: impl Iterator<Item = transaction::Result<TransactionCost<'a, Tx>>>,
        bank: &Bank,
    ) -> (Vec<transaction::Result<TransactionCost<'a, Tx>>>, usize) {
        let mut cost_tracker = bank.write_cost_tracker().unwrap();
        let mut num_included = 0;
        let select_results = transactions
            .zip(transactions_costs)
            .map(|(tx, cost)| match cost {
                Ok(cost) => match cost_tracker.try_add(&cost) {
                    Ok(UpdatedCosts {
                        updated_block_cost,
                        updated_costliest_account_cost,
                    }) => {
                        debug!(
                            "slot {:?}, transaction {:?}, cost {:?}, fit into current block, \
                             current block cost {}, updated costliest account cost {}",
                            bank.slot(),
                            tx,
                            cost,
                            updated_block_cost,
                            updated_costliest_account_cost
                        );
                        num_included += 1;
                        Ok(cost)
                    }
                    Err(e) => {
                        debug!(
                            "slot {:?}, transaction {:?}, cost {:?}, not fit into current block, \
                             '{:?}'",
                            bank.slot(),
                            tx,
                            cost,
                            e
                        );
                        Err(TransactionError::from(e))
                    }
                },
                Err(e) => Err(e),
            })
            .collect();
        cost_tracker.add_transactions_in_flight(num_included);

        (select_results, num_included)
    }

    /// Removes transaction costs from the cost tracker if not committed or recorded, or
    /// updates the transaction costs for committed transactions.
    pub fn remove_or_update_costs<'a, Tx: TransactionWithMeta + 'a>(
        transaction_cost_results: impl Iterator<Item = &'a transaction::Result<TransactionCost<'a, Tx>>>,
        transaction_committed_status: Option<&Vec<CommitTransactionDetails>>,
        bank: &Bank,
    ) {
        match transaction_committed_status {
            Some(transaction_committed_status) => {
                Self::remove_or_update_recorded_transaction_costs(
                    transaction_cost_results,
                    transaction_committed_status,
                    bank,
                )
            }
            None => Self::remove_unrecorded_transaction_costs(transaction_cost_results, bank),
        }
    }

    /// For recorded transactions, remove units reserved by uncommitted transaction, or update
    /// units for committed transactions.
    fn remove_or_update_recorded_transaction_costs<'a, Tx: TransactionWithMeta + 'a>(
        transaction_cost_results: impl Iterator<Item = &'a transaction::Result<TransactionCost<'a, Tx>>>,
        transaction_committed_status: &Vec<CommitTransactionDetails>,
        bank: &Bank,
    ) {
        let mut cost_tracker = bank.write_cost_tracker().unwrap();
        let mut num_included = 0;
        transaction_cost_results
            .zip(transaction_committed_status)
            .for_each(|(tx_cost, transaction_committed_details)| {
                // Only transactions that the qos service included have to be
                // checked for update
                if let Ok(tx_cost) = tx_cost {
                    num_included += 1;
                    match transaction_committed_details {
                        CommitTransactionDetails::Committed {
                            compute_units,
                            loaded_accounts_data_size,
                            result: _,
                            fee_payer_post_balance: _,
                        } => {
                            cost_tracker.update_execution_cost(
                                tx_cost,
                                *compute_units,
                                CostModel::calculate_loaded_accounts_data_size_cost(
                                    *loaded_accounts_data_size,
                                    &bank.feature_set,
                                ),
                            );
                        }
                        CommitTransactionDetails::NotCommitted(_err) => {
                            cost_tracker.remove(tx_cost);
                        }
                    }
                }
            });
        cost_tracker.sub_transactions_in_flight(num_included);
    }

    /// Remove reserved units for transaction batch that unsuccessfully recorded.
    fn remove_unrecorded_transaction_costs<'a, Tx: TransactionWithMeta + 'a>(
        transaction_cost_results: impl Iterator<Item = &'a transaction::Result<TransactionCost<'a, Tx>>>,
        bank: &Bank,
    ) {
        let mut cost_tracker = bank.write_cost_tracker().unwrap();
        let mut num_included = 0;
        transaction_cost_results.for_each(|tx_cost| {
            // Only transactions that the qos service included have to be
            // removed
            if let Ok(tx_cost) = tx_cost {
                num_included += 1;
                cost_tracker.remove(tx_cost);
            }
        });
        cost_tracker.sub_transactions_in_flight(num_included);
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        itertools::Itertools,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_runtime::genesis_utils::{GenesisConfigInfo, create_genesis_config},
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_signer::Signer,
        solana_system_transaction as system_transaction,
        solana_vote::vote_transaction,
        solana_vote_program::vote_state::TowerSync,
        std::sync::Arc,
    };

    #[test]
    fn test_compute_transaction_costs() {
        agave_logger::setup();

        // make a vec of txs
        let keypair = Keypair::new();
        let transfer_tx = RuntimeTransaction::from_transaction_for_tests(
            system_transaction::transfer(&keypair, &keypair.pubkey(), 1, Hash::default()),
        );
        let vote_tx = RuntimeTransaction::from_transaction_for_tests(
            vote_transaction::new_tower_sync_transaction(
                TowerSync::from(vec![(42, 1)]),
                Hash::default(),
                &keypair,
                &keypair,
                &keypair,
                None,
            ),
        );
        let txs = [transfer_tx.clone(), vote_tx.clone(), vote_tx, transfer_tx];

        let txs_costs = QosService::compute_transaction_costs(
            &FeatureSet::all_enabled(),
            txs.iter(),
            std::iter::repeat(Ok(())),
        );

        // verify the size of txs_costs and its contents
        assert_eq!(txs_costs.len(), txs.len());
        txs_costs
            .iter()
            .enumerate()
            .map(|(index, cost)| {
                assert_eq!(
                    cost.as_ref().unwrap().sum(),
                    CostModel::calculate_cost(&txs[index], &FeatureSet::all_enabled()).sum()
                );
            })
            .collect_vec();
    }

    #[test]
    fn test_select_transactions_per_cost() {
        agave_logger::setup();
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));

        let keypair = Keypair::new();
        let transfer_tx = RuntimeTransaction::from_transaction_for_tests(
            system_transaction::transfer(&keypair, &keypair.pubkey(), 1, Hash::default()),
        );
        let vote_tx = RuntimeTransaction::from_transaction_for_tests(
            vote_transaction::new_tower_sync_transaction(
                TowerSync::from(vec![(42, 1)]),
                Hash::default(),
                &keypair,
                &keypair,
                &keypair,
                None,
            ),
        );
        let transfer_tx_cost =
            CostModel::calculate_cost(&transfer_tx, &FeatureSet::all_enabled()).sum();
        let vote_tx_cost = CostModel::calculate_cost(&vote_tx, &FeatureSet::all_enabled()).sum();
        // make a vec of txs
        let txs = [transfer_tx.clone(), vote_tx.clone(), transfer_tx, vote_tx];

        let txs_costs = QosService::compute_transaction_costs(
            &FeatureSet::all_enabled(),
            txs.iter(),
            std::iter::repeat(Ok(())),
        );

        // set cost tracker limit to bare minimum that will fit 1 transfer tx
        // and 1 vote tx
        let cost_limit = transfer_tx_cost + vote_tx_cost;
        bank.write_cost_tracker()
            .unwrap()
            .set_limits(cost_limit, cost_limit, cost_limit, 0);
        let (results, num_selected) =
            QosService::select_transactions_per_cost(txs.iter(), txs_costs.into_iter(), &bank);
        assert_eq!(num_selected, 2);

        // verify that first transfer tx and first vote are allowed
        assert_eq!(results.len(), txs.len());
        assert!(results[0].is_ok());
        assert!(results[1].is_ok());
        assert!(results[2].is_err());
        assert!(results[3].is_err());
    }

    #[test]
    fn test_update_and_remove_transaction_costs_committed() {
        agave_logger::setup();
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));

        // make some transfer transactions
        // calculate their costs, apply to cost_tracker
        let transaction_count = 5;
        let keypair = Keypair::new();
        let loaded_accounts_data_size: u32 = 1_000_000;
        let transaction = solana_transaction::Transaction::new_unsigned(solana_message::Message::new(
            &[
                solana_compute_budget_interface::ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(loaded_accounts_data_size),
                solana_system_interface::instruction::transfer(&keypair.pubkey(), &solana_pubkey::Pubkey::new_unique(), 1),
            ],
            Some(&keypair.pubkey()),
        ));
        let transfer_tx = RuntimeTransaction::from_transaction_for_tests(transaction);
        let txs: Vec<_> = (0..transaction_count)
            .map(|_| transfer_tx.clone())
            .collect();
        let execute_units_adjustment: u64 = 10;
        let loaded_accounts_data_size_adjustment: u32 = 32000;
        let loaded_accounts_data_size_cost_adjustment =
            CostModel::calculate_loaded_accounts_data_size_cost(
                loaded_accounts_data_size_adjustment,
                &bank.feature_set,
            );

        // assert all tx_costs should be applied to cost_tracker if all execution_results are all committed
        {
            let txs_costs = QosService::compute_transaction_costs(
                &FeatureSet::all_enabled(),
                txs.iter(),
                std::iter::repeat(Ok(())),
            );
            let total_txs_cost: u64 = txs_costs
                .iter()
                .map(|cost| cost.as_ref().unwrap().sum())
                .sum();
            let (qos_cost_results, _num_included) =
                QosService::select_transactions_per_cost(txs.iter(), txs_costs.into_iter(), &bank);
            assert_eq!(
                total_txs_cost,
                bank.read_cost_tracker().unwrap().block_cost()
            );
            // all transactions are committed with actual units more than estimated
            let committed_status: Vec<CommitTransactionDetails> = qos_cost_results
                .iter()
                .map(|tx_cost| CommitTransactionDetails::Committed {
                    compute_units: tx_cost.as_ref().unwrap().programs_execution_cost()
                        + execute_units_adjustment,
                    loaded_accounts_data_size: loaded_accounts_data_size
                        + loaded_accounts_data_size_adjustment,
                    result: Ok(()),
                    fee_payer_post_balance: 0,
                })
                .collect();
            let final_txs_cost = total_txs_cost
                + (execute_units_adjustment + loaded_accounts_data_size_cost_adjustment)
                    * transaction_count;

            QosService::remove_or_update_costs(
                qos_cost_results.iter(),
                Some(&committed_status),
                &bank,
            );
            assert_eq!(
                final_txs_cost,
                bank.read_cost_tracker().unwrap().block_cost()
            );
            assert_eq!(
                transaction_count,
                bank.read_cost_tracker().unwrap().transaction_count()
            );
        }
    }

    #[test]
    fn test_update_and_remove_transaction_costs_not_committed() {
        agave_logger::setup();
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));

        // make some transfer transactions
        // calculate their costs, apply to cost_tracker
        let transaction_count = 5;
        let keypair = Keypair::new();
        let txs: Vec<_> = (0..transaction_count)
            .map(|_| {
                RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
                    &keypair,
                    &keypair.pubkey(),
                    1,
                    Hash::default(),
                ))
            })
            .collect();

        // assert all tx_costs should be removed from cost_tracker if all execution_results are all Not Committed
        {
            let txs_costs = QosService::compute_transaction_costs(
                &FeatureSet::all_enabled(),
                txs.iter(),
                std::iter::repeat(Ok(())),
            );
            let total_txs_cost: u64 = txs_costs
                .iter()
                .map(|cost| cost.as_ref().unwrap().sum())
                .sum();
            let (qos_cost_results, _num_included) =
                QosService::select_transactions_per_cost(txs.iter(), txs_costs.into_iter(), &bank);
            assert_eq!(
                total_txs_cost,
                bank.read_cost_tracker().unwrap().block_cost()
            );

            QosService::remove_or_update_costs(qos_cost_results.iter(), None, &bank);
            assert_eq!(0, bank.read_cost_tracker().unwrap().block_cost());
            assert_eq!(0, bank.read_cost_tracker().unwrap().transaction_count());
        }
    }

    #[test]
    fn test_update_and_remove_transaction_costs_mixed_execution() {
        agave_logger::setup();
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));

        // make some transfer transactions
        // calculate their costs, apply to cost_tracker
        let transaction_count = 5;
        let keypair = Keypair::new();
        let loaded_accounts_data_size: u32 = 1_000_000;
        let transaction = solana_transaction::Transaction::new_unsigned(solana_message::Message::new(
            &[
                solana_compute_budget_interface::ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(loaded_accounts_data_size),
                solana_system_interface::instruction::transfer(&keypair.pubkey(), &solana_pubkey::Pubkey::new_unique(), 1),
            ],
            Some(&keypair.pubkey()),
        ));
        let txs: Vec<_> = (0..transaction_count)
            .map(|_| RuntimeTransaction::from_transaction_for_tests(transaction.clone()))
            .collect();
        let execute_units_adjustment: u64 = 10;
        let loaded_accounts_data_size_adjustment: u32 = 32000;
        let loaded_accounts_data_size_cost_adjustment =
            CostModel::calculate_loaded_accounts_data_size_cost(
                loaded_accounts_data_size_adjustment,
                &bank.feature_set,
            );

        // assert only committed tx_costs are applied cost_tracker
        {
            let txs_costs = QosService::compute_transaction_costs(
                &FeatureSet::all_enabled(),
                txs.iter(),
                std::iter::repeat(Ok(())),
            );
            let total_txs_cost: u64 = txs_costs
                .iter()
                .map(|cost| cost.as_ref().unwrap().sum())
                .sum();
            let (qos_cost_results, _num_included) =
                QosService::select_transactions_per_cost(txs.iter(), txs_costs.into_iter(), &bank);
            assert_eq!(
                total_txs_cost,
                bank.read_cost_tracker().unwrap().block_cost()
            );
            // Half of transactions are not committed, the rest with cost adjustment
            let committed_status: Vec<CommitTransactionDetails> = qos_cost_results
                .iter()
                .enumerate()
                .map(|(n, tx_cost)| {
                    if n % 2 == 0 {
                        CommitTransactionDetails::NotCommitted(
                            TransactionError::InsufficientFundsForFee,
                        )
                    } else {
                        CommitTransactionDetails::Committed {
                            compute_units: tx_cost.as_ref().unwrap().programs_execution_cost()
                                + execute_units_adjustment,
                            loaded_accounts_data_size: loaded_accounts_data_size
                                + loaded_accounts_data_size_adjustment,
                            result: Ok(()),
                            fee_payer_post_balance: 1,
                        }
                    }
                })
                .collect();

            QosService::remove_or_update_costs(
                qos_cost_results.iter(),
                Some(&committed_status),
                &bank,
            );

            // assert the final block cost
            let mut expected_final_txs_count = 0u64;
            let mut expected_final_block_cost = 0u64;
            qos_cost_results.iter().enumerate().for_each(|(n, cost)| {
                if n % 2 != 0 {
                    expected_final_txs_count += 1;
                    expected_final_block_cost += cost.as_ref().unwrap().sum()
                        + execute_units_adjustment
                        + loaded_accounts_data_size_cost_adjustment;
                }
            });
            assert_eq!(
                expected_final_block_cost,
                bank.read_cost_tracker().unwrap().block_cost()
            );
            assert_eq!(
                expected_final_txs_count,
                bank.read_cost_tracker().unwrap().transaction_count()
            );
        }
    }
}
