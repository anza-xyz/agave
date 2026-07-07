use {
    crate::{
        bank::{Bank, PreCommitResult, TransactionBalancesSet},
        bank_utils,
        dependency_tracker::DependencyTracker,
        prioritization_fee_cache::PrioritizationFeeCache,
        transaction_balances::compile_collected_balances,
        transaction_batch::TransactionBatch,
        vote_sender_types::{ReplayVoteSendType, ReplayVoteSender},
    },
    log::{trace, warn},
    solana_clock::Slot,
    solana_cost_model::{cost_model::CostModel, transaction_cost::TransactionCost},
    solana_measure::measure::Measure,
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    solana_signature::Signature,
    solana_svm::{
        transaction_commit_result::{
            TransactionCommitResult, TransactionCommitResultExtensions as _,
        },
        transaction_processing_result::ProcessedTransaction,
        transaction_processor::ExecutionRecordingConfig,
    },
    solana_svm_timings::{ExecuteTimingType, ExecuteTimings},
    solana_svm_transaction::{svm_message::SVMMessage, svm_transaction::SVMTransaction},
    solana_transaction::sanitized::SanitizedTransaction,
    solana_transaction_error::{TransactionError, TransactionResult},
    solana_transaction_status::token_balances::TransactionTokenBalancesSet,
    std::{borrow::Cow, sync::Arc},
};

type WorkSequence = u64;

#[derive(Debug)]
pub struct TransactionStatusBatch {
    pub slot: Slot,
    pub transactions: Vec<SanitizedTransaction>,
    pub commit_results: Vec<TransactionCommitResult>,
    pub balances: TransactionBalancesSet,
    pub token_balances: TransactionTokenBalancesSet,
    pub costs: Vec<Option<u64>>,
    pub transaction_indexes: Vec<usize>,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum TransactionStatusMessage {
    Batch((TransactionStatusBatch, Option<WorkSequence>)),
    Freeze(Arc<Bank>),
}

pub struct TransactionBatchWithIndexes<'a, 'b, Tx: SVMMessage> {
    pub batch: TransactionBatch<'a, 'b, Tx>,
    pub transaction_indexes: Vec<usize>,
}

pub fn execute_batch<'a>(
    batch: &'a TransactionBatchWithIndexes<impl TransactionWithMeta>,
    bank: &'a Arc<Bank>,
    transaction_status_sender: Option<&'a TransactionStatusSender>,
    replay_vote_sender: Option<&'a ReplayVoteSender>,
    replay_vote_send_type: ReplayVoteSendType,
    timings: &'a mut ExecuteTimings,
    log_messages_bytes_limit: Option<usize>,
    prioritization_fee_cache: Option<&'a PrioritizationFeeCache>,
    extra_pre_commit_callback: Option<
        impl FnOnce(&TransactionResult<ProcessedTransaction>) -> TransactionResult<Option<usize>>,
    >,
) -> TransactionResult<()> {
    let TransactionBatchWithIndexes {
        batch,
        transaction_indexes,
    } = batch;

    // extra_pre_commit_callback allows for reuse of this function between the
    // unified scheduler block production path and block verification path(s)
    //   Some(_) => unified scheduler block production path
    //   None    => block verification path(s)
    let block_verification = extra_pre_commit_callback.is_none();
    let record_transaction_meta = transaction_status_sender.is_some();
    let mut transaction_indexes = Cow::from(transaction_indexes);

    let pre_commit_callback = |_timings: &mut _, processing_results: &_| -> PreCommitResult {
        match extra_pre_commit_callback {
            None => {
                // We're entering into one of the block-verification methods.
                get_first_error(batch, processing_results)?;
                Ok(None)
            }
            Some(extra_pre_commit_callback) => {
                // We're entering into the block-production unified scheduler special case...
                // `processing_results` should always contain exactly only 1 result in that case.
                let [result] = processing_results else {
                    panic!("unexpected result count: {}", processing_results.len());
                };
                // transaction_indexes is intended to be populated later; so barely-initialized vec
                // should be provided.
                assert!(transaction_indexes.is_empty());

                // From now on, we need to freeze-lock the tpu bank, in order to prevent it from
                // freezing in the middle of this code-path. Otherwise, the assertion at the start
                // of commit_transactions() would trigger panic because it's fatal runtime
                // invariant violation.
                let freeze_lock = bank.freeze_lock();

                // `result` won't be examined at all here. Rather, `extra_pre_commit_callback` is
                // responsible for all result handling, including the very basic precondition of
                // successful execution of transactions as well.
                let committed_index = extra_pre_commit_callback(result)?;

                // The callback succeeded. Optionally, update transaction_indexes as well.
                // Refer to TaskHandler::handle()'s transaction_indexes initialization for further
                // background.
                if let Some(index) = committed_index {
                    let transaction_indexes = transaction_indexes.to_mut();
                    // Adjust the empty new vec with the exact needed capacity. Otherwise, excess
                    // cap would be reserved on `.push()` in it.
                    transaction_indexes.reserve_exact(1);
                    transaction_indexes.push(index);
                }
                // At this point, poh should have been succeeded so it's guaranteed that the bank
                // hasn't been frozen yet and we're still holding the lock. So, it's okay to pass
                // down freeze_lock without any introspection here to be unconditionally dropped
                // after commit_transactions(). This reasoning is same as
                // solana_core::banking_stage::Consumer::execute_and_commit_transactions_locked()
                Ok(Some(freeze_lock))
            }
        }
    };

    let (commit_results, balance_collector) = batch
        .bank()
        .load_execute_and_commit_transactions_with_pre_commit_callback(
            batch,
            ExecutionRecordingConfig::new_single_setting(transaction_status_sender.is_some()),
            timings,
            log_messages_bytes_limit,
            pre_commit_callback,
        )?;

    let mut check_block_costs_elapsed = Measure::start("check_block_costs");
    let tx_costs = if block_verification {
        // Block verification (including unified scheduler) case;
        // collect and check transaction costs
        let tx_costs = get_transaction_costs(bank, &commit_results, batch.sanitized_transactions());
        check_block_cost_limits(bank, &tx_costs).map(|_| tx_costs)
    } else if record_transaction_meta {
        // Unified scheduler block production case;
        // the scheduler will track costs elsewhere but costs are recalculated
        // here so they can be recorded with other transaction metadata
        Ok(get_transaction_costs(
            bank,
            &commit_results,
            batch.sanitized_transactions(),
        ))
    } else {
        // Unified scheduler block production without metadata recording
        Ok(vec![])
    };
    check_block_costs_elapsed.stop();
    timings.saturating_add_in_place(
        ExecuteTimingType::CheckBlockLimitsUs,
        check_block_costs_elapsed.as_us(),
    );
    let tx_costs = tx_costs?;

    bank_utils::find_and_send_votes(
        batch.sanitized_transactions(),
        &commit_results,
        replay_vote_sender,
        replay_vote_send_type,
    );

    if let Some(prioritization_fee_cache) = prioritization_fee_cache {
        let committed_transactions = commit_results
            .iter()
            .zip(batch.sanitized_transactions())
            .filter_map(|(commit_result, tx)| commit_result.was_committed().then_some(tx));
        prioritization_fee_cache.update(bank, committed_transactions);
    }
    if let Some(transaction_status_sender) = transaction_status_sender {
        let transactions: Vec<SanitizedTransaction> = batch
            .sanitized_transactions()
            .iter()
            .map(|tx| tx.as_sanitized_transaction().into_owned())
            .collect();

        // There are two cases where balance_collector could be None:
        // * Balance recording is disabled. If that were the case, there would
        //   be no TransactionStatusSender, and we would not be in this branch.
        // * The batch was aborted in its entirety in SVM. In that case, nothing
        //   would have been committed.
        // Therefore this should always be true.
        debug_assert!(balance_collector.is_some());

        let (balances, token_balances) =
            compile_collected_balances(balance_collector.unwrap_or_default());

        // The length of costs vector needs to be consistent with all other
        // vectors that are sent over (such as `transactions`). So, replace the
        // None elements with Some(0)
        let tx_costs = tx_costs
            .into_iter()
            .map(|tx_cost_option| tx_cost_option.map(|tx_cost| tx_cost.sum()).or(Some(0)))
            .collect();

        transaction_status_sender.send_transaction_status_batch(
            bank.slot(),
            transactions,
            commit_results,
            balances,
            token_balances,
            tx_costs,
            transaction_indexes.into_owned(),
        );
    }

    Ok(())
}

fn check_block_cost_limits<Tx: TransactionWithMeta>(
    bank: &Bank,
    tx_costs: &[Option<TransactionCost<'_, Tx>>],
) -> TransactionResult<()> {
    let mut cost_tracker = bank.write_cost_tracker().unwrap();
    for tx_cost in tx_costs.iter().flatten() {
        cost_tracker
            .try_add(tx_cost)
            .map_err(TransactionError::from)?;
    }

    Ok(())
}

// Get actual transaction execution costs from transaction commit results
fn get_transaction_costs<'a, Tx: TransactionWithMeta>(
    bank: &Bank,
    commit_results: &[TransactionCommitResult],
    sanitized_transactions: &'a [Tx],
) -> Vec<Option<TransactionCost<'a, Tx>>> {
    assert_eq!(sanitized_transactions.len(), commit_results.len());

    commit_results
        .iter()
        .zip(sanitized_transactions)
        .map(|(commit_result, tx)| {
            if let Ok(committed_tx) = commit_result {
                Some(CostModel::calculate_cost_for_executed_transaction(
                    tx,
                    committed_tx.executed_units,
                    committed_tx.loaded_account_stats.loaded_accounts_data_size,
                    &bank.feature_set,
                ))
            } else {
                None
            }
        })
        .collect()
}

fn get_first_error<T, Tx: SVMTransaction>(
    batch: &TransactionBatch<Tx>,
    commit_results: &[TransactionResult<T>],
) -> TransactionResult<()> {
    do_get_first_error(batch, commit_results)
        .map(|(error, _signature)| error)
        .unwrap_or(Ok(()))
}

// Includes transaction signature for unit-testing
fn do_get_first_error<T, Tx: SVMTransaction>(
    batch: &TransactionBatch<Tx>,
    results: &[TransactionResult<T>],
) -> Option<(TransactionResult<()>, Signature)> {
    let mut first_err = None;
    for (result, transaction) in results.iter().zip(batch.sanitized_transactions()) {
        if let Err(err) = result {
            if first_err.is_none() {
                first_err = Some((Err(err.clone()), *transaction.signature()));
            }
            warn!("Unexpected validator error: {err:?}, transaction: {transaction:?}");
            datapoint_error!(
                "validator_process_entry_error",
                (
                    "error",
                    format!("error: {err:?}, transaction: {transaction:?}"),
                    String
                )
            );
        }
    }
    first_err
}

#[derive(Clone, Debug)]
pub struct TransactionStatusSender {
    pub sender: crossbeam_channel::Sender<TransactionStatusMessage>,
    pub dependency_tracker: Option<Arc<DependencyTracker>>,
}

impl TransactionStatusSender {
    pub fn send_transaction_status_batch(
        &self,
        slot: Slot,
        transactions: Vec<SanitizedTransaction>,
        commit_results: Vec<TransactionCommitResult>,
        balances: TransactionBalancesSet,
        token_balances: TransactionTokenBalancesSet,
        costs: Vec<Option<u64>>,
        transaction_indexes: Vec<usize>,
    ) {
        let work_sequence = self
            .dependency_tracker
            .as_ref()
            .map(|dependency_tracker| dependency_tracker.declare_work());

        if let Err(e) = self.sender.send(TransactionStatusMessage::Batch((
            TransactionStatusBatch {
                slot,
                transactions,
                commit_results,
                balances,
                token_balances,
                costs,
                transaction_indexes,
            },
            work_sequence,
        ))) {
            trace!("Slot {slot} transaction_status send batch failed: {e:?}");
        }
    }

    pub fn send_transaction_status_freeze_message(&self, bank: &Arc<Bank>) {
        if let Err(e) = self
            .sender
            .send(TransactionStatusMessage::Freeze(bank.clone()))
        {
            let slot = bank.slot();
            warn!("Slot {slot} transaction_status send freeze message failed: {e:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            genesis_utils::{
                GenesisConfigInfo, create_genesis_config, create_genesis_config_with_leader,
            },
            transaction_batch::OwnedOrBorrowed,
        },
        crossbeam_channel::bounded,
        solana_account::AccountSharedData,
        solana_cost_model::cost_tracker::CostTrackerLimits,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_pubkey::Pubkey,
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_signer::Signer as _,
        std::{assert_matches, slice},
        test_case::test_matrix,
    };

    #[test]
    fn test_check_block_cost_limit() {
        let dummy_leader_pubkey = solana_pubkey::new_rand();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config_with_leader(500, &dummy_leader_pubkey, 100);
        let bank = Bank::new_for_tests(&genesis_config);

        let tx =
            RuntimeTransaction::from_transaction_for_tests(solana_system_transaction::transfer(
                &mint_keypair,
                &Pubkey::new_unique(),
                1,
                genesis_config.hash(),
            ));
        let mut tx_cost = CostModel::calculate_cost(&tx, &bank.feature_set);
        let actual_execution_cu = 1;
        let actual_loaded_accounts_data_size = 64 * 1024;
        let usage_cost_details = tx_cost.usage_cost_details_mut();
        usage_cost_details.programs_execution_cost = actual_execution_cu;
        usage_cost_details.loaded_accounts_data_size_cost =
            CostModel::calculate_loaded_accounts_data_size_cost(
                actual_loaded_accounts_data_size,
                &bank.feature_set,
            );
        // set block-limit to be able to just have one transaction
        let block_limit = tx_cost.sum();
        bank.write_cost_tracker()
            .unwrap()
            .set_limits(CostTrackerLimits::new(u64::MAX, block_limit, u64::MAX));

        let tx_costs = vec![None, Some(tx_cost), None];
        // The transaction will fit when added the first time
        assert!(check_block_cost_limits(&bank, &tx_costs).is_ok());
        // But adding a second time will exceed the block limit
        assert_eq!(
            Err(TransactionError::WouldExceedMaxBlockCostLimit),
            check_block_cost_limits(&bank, &tx_costs)
        );
        // Adding another None will noop (even though the block is already full)
        assert!(check_block_cost_limits(&bank, &tx_costs[0..1]).is_ok());
    }

    #[test]
    fn test_get_first_error() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(1_000_000_000);
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let present_account_key = Keypair::new();
        let present_account = AccountSharedData::new(1, 10, &Pubkey::default());
        bank.store_account(&present_account_key.pubkey(), &present_account);

        let keypair = Keypair::new();

        // Create array of two transactions which throw different errors
        let account_not_found_tx = solana_system_transaction::transfer(
            &keypair,
            &solana_pubkey::new_rand(),
            42,
            bank.last_blockhash(),
        );
        let account_not_found_sig = account_not_found_tx.signatures[0];
        let invalid_blockhash_tx = solana_system_transaction::transfer(
            &mint_keypair,
            &solana_pubkey::new_rand(),
            42,
            Hash::default(),
        );
        let txs = vec![account_not_found_tx, invalid_blockhash_tx];
        let batch = bank.prepare_batch_for_tests(txs);
        let (commit_results, _) = batch.bank().load_execute_and_commit_transactions(
            &batch,
            ExecutionRecordingConfig::new_single_setting(false),
            &mut ExecuteTimings::default(),
            None,
        );
        let (err, signature) = do_get_first_error(&batch, &commit_results).unwrap();
        assert_eq!(err.unwrap_err(), TransactionError::AccountNotFound);
        assert_eq!(signature, account_not_found_sig);
    }

    enum TxResult {
        ExecutedWithSuccess,
        ExecutedWithFailure,
        NotExecuted,
    }

    #[test_matrix(
        [TxResult::ExecutedWithSuccess, TxResult::ExecutedWithFailure, TxResult::NotExecuted],
        [Ok(None), Ok(Some(4)), Err(TransactionError::CommitCancelled)]
    )]
    fn test_execute_batch_pre_commit_callback(
        tx_result: TxResult,
        poh_result: TransactionResult<Option<usize>>,
    ) {
        agave_logger::setup();
        let dummy_leader_pubkey = solana_pubkey::new_rand();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config_with_leader(500, &dummy_leader_pubkey, 100);
        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();
        let bank = Arc::new(bank);
        let pubkey = solana_pubkey::new_rand();
        let (tx, expected_tx_result) = match tx_result {
            TxResult::ExecutedWithSuccess => (
                RuntimeTransaction::from_transaction_for_tests(
                    solana_system_transaction::transfer(
                        &mint_keypair,
                        &pubkey,
                        1,
                        genesis_config.hash(),
                    ),
                ),
                Ok(()),
            ),
            TxResult::ExecutedWithFailure => (
                RuntimeTransaction::from_transaction_for_tests(
                    solana_system_transaction::transfer(
                        &mint_keypair,
                        &pubkey,
                        100000000,
                        genesis_config.hash(),
                    ),
                ),
                Ok(()),
            ),
            TxResult::NotExecuted => (
                RuntimeTransaction::from_transaction_for_tests(
                    solana_system_transaction::transfer(&mint_keypair, &pubkey, 1, Hash::default()),
                ),
                Err(TransactionError::BlockhashNotFound),
            ),
        };
        let mut batch = TransactionBatch::new(
            vec![Ok(()); 1],
            &bank,
            OwnedOrBorrowed::Borrowed(slice::from_ref(&tx)),
        );
        batch.set_needs_unlock(false);
        let poh_with_index = matches!(&poh_result, Ok(Some(_)));
        let batch = TransactionBatchWithIndexes {
            batch,
            transaction_indexes: vec![],
        };
        let mut timing = ExecuteTimings::default();
        let (sender, receiver) = bounded(1024);

        assert_eq!(bank.transaction_count(), 0);
        assert_eq!(bank.transaction_error_count(), 0);
        let should_commit = poh_result.is_ok();
        let mut is_called = false;
        let result = execute_batch(
            &batch,
            &bank,
            Some(&TransactionStatusSender {
                sender,
                dependency_tracker: None,
            }),
            None,
            ReplayVoteSendType::VerifiedExecuted,
            &mut timing,
            None,
            None,
            Some(|processing_result: &'_ TransactionResult<_>| {
                is_called = true;
                let ok = poh_result?;
                if let Err(error) = processing_result {
                    Err(error.clone())?;
                };
                Ok(ok)
            }),
        );

        // pre_commit_callback() should always be called regardless of tx_result
        assert!(is_called);

        if should_commit {
            assert_eq!(result, expected_tx_result);
            if expected_tx_result.is_ok() {
                assert_eq!(bank.transaction_count(), 1);
                if matches!(tx_result, TxResult::ExecutedWithFailure) {
                    assert_eq!(bank.transaction_error_count(), 1);
                } else {
                    assert_eq!(bank.transaction_error_count(), 0);
                }
            } else {
                assert_eq!(bank.transaction_count(), 0);
            }
        } else {
            assert_matches!(result, Err(TransactionError::CommitCancelled));
            assert_eq!(bank.transaction_count(), 0);
        }
        if poh_with_index && expected_tx_result.is_ok() {
            assert_matches!(
                receiver.try_recv(),
                Ok(TransactionStatusMessage::Batch((TransactionStatusBatch{transaction_indexes, ..}, _sequence)))
                    if transaction_indexes == vec![4_usize]
            );
        } else if should_commit && expected_tx_result.is_ok() {
            assert_matches!(
                receiver.try_recv(),
                Ok(TransactionStatusMessage::Batch((TransactionStatusBatch{transaction_indexes, ..}, _sequence)))
                    if transaction_indexes.is_empty()
            );
        } else {
            assert_matches!(receiver.try_recv(), Err(_));
        }
    }
}
