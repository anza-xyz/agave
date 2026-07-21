use {
    super::{
        committer::{CommitTransactionDetails, Committer},
        leader_slot_timing_metrics::LeaderExecuteAndCommitTimings,
        scheduler_messages::MaxAge,
    },
    smallvec::SmallVec,
    solana_cost_model::{cost_model::CostModel, transaction_cost::TransactionCost},
    solana_hash::Hash,
    solana_measure::measure_us,
    solana_poh::{
        poh_recorder::PohRecorderError,
        transaction_recorder::{RecordTransactionsTimings, TransactionRecorder},
    },
    solana_runtime::{
        bank::{
            Bank, LoadAndExecuteTransactionsOutput, ProcessedTransactionCounts,
            entry_bytes_budget::EntryBytesReserveError,
        },
        transaction_batch::TransactionBatch,
    },
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    solana_svm::{
        account_loader::validate_fee_payer,
        transaction_error_metrics::TransactionErrorMetrics,
        transaction_processing_result::{
            TransactionProcessingResult, TransactionProcessingResultExtensions,
        },
        transaction_processor::{ExecutionRecordingConfig, TransactionProcessingConfig},
    },
    solana_transaction_error::TransactionError,
    solana_vote::vote_parser,
    std::num::Saturating,
};

/// Consumer will create chunks of transactions from buffer with up to this size.
pub const TARGET_NUM_TRANSACTIONS_PER_BATCH: usize = 64;

pub(crate) const ENTRY_OVERHEAD_BYTES: u64 = 48;

const SERIALIZED_ENTRIES_OVERHEAD: u64 = ENTRY_OVERHEAD_BYTES + 8; // Vec<Entry> length

#[derive(Debug)]
pub struct ExecutionFlags {
    /// Should failing transactions within the batch be dropped (no fee charged
    /// & not committed).
    pub drop_on_failure: bool,
    /// If any transaction in the batch is not committed then the entire batch
    /// should not be committed.
    ///
    /// # Note
    ///
    /// Without `drop_on_failure` this flag will still allow processed but
    /// failing transactions to be committed. If both flags are set then any
    /// failing transaction will cause all transactions to be aborted.
    pub all_or_nothing: bool,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RetryableIndex {
    pub index: usize,
    pub immediately_retryable: bool,
}

#[cfg(test)]
impl RetryableIndex {
    pub fn new(index: usize, immediately_retryable: bool) -> Self {
        Self {
            index,
            immediately_retryable,
        }
    }
}

pub struct ProcessTransactionBatchOutput {
    // The number of transactions filtered out by the cost model
    pub(crate) cost_model_throttled_transactions_count: u64,
    // Amount of time spent running the cost model
    pub(crate) cost_model_us: u64,
    pub execute_and_commit_transactions_output: ExecuteAndCommitTransactionsOutput,
}

pub struct ExecuteAndCommitTransactionsOutput {
    // The number of transactions filtered out by the cost model
    pub(crate) cost_model_throttled_transactions_count: u64,
    // Amount of time spent running the cost model
    pub(crate) cost_model_us: u64,
    // Transactions counts reported to `ConsumeWorkerMetrics` and then
    // accumulated later for `LeaderSlotMetrics`
    pub(crate) transaction_counts: LeaderProcessedTransactionCounts,
    // Transactions that either were not executed, or were executed and failed to be committed due
    // to the block ending.
    pub(crate) retryable_transaction_indexes: Vec<RetryableIndex>,
    // A result that indicates whether transactions were successfully
    // committed into the Poh stream.
    pub commit_transactions_result: Result<Vec<CommitTransactionDetails>, PohRecorderError>,
    pub(crate) execute_and_commit_timings: LeaderExecuteAndCommitTimings,
    pub(crate) error_counters: TransactionErrorMetrics,
}

#[derive(Debug, Default, PartialEq)]
pub struct LeaderProcessedTransactionCounts {
    // Total number of transactions that were passed as candidates for processing
    pub(crate) attempted_processing_count: u64,
    // The number of transactions of that were processed. See description of in `ProcessTransactionsSummary`
    // for possible outcomes of execution.
    pub(crate) processed_count: u64,
    // Total number of the processed transactions that returned success/not
    // an error.
    pub(crate) processed_with_successful_result_count: u64,
}

pub struct Consumer {
    committer: Committer,
    transaction_recorder: TransactionRecorder,
    log_messages_bytes_limit: Option<usize>,
}

impl Consumer {
    pub fn new(
        committer: Committer,
        transaction_recorder: TransactionRecorder,
        log_messages_bytes_limit: Option<usize>,
    ) -> Self {
        Self {
            committer,
            transaction_recorder,
            log_messages_bytes_limit,
        }
    }

    pub fn process_and_record_transactions(
        &self,
        bank: &Bank,
        txs: &[impl TransactionWithMeta],
    ) -> ProcessTransactionBatchOutput {
        let mut error_counters = TransactionErrorMetrics::default();
        let pre_results =
            SmallVec::<[_; TARGET_NUM_TRANSACTIONS_PER_BATCH]>::from_elem(Ok(()), txs.len());
        let check_results = bank.check_transactions(
            txs,
            &pre_results,
            bank.max_processing_age(),
            true,
            &mut error_counters,
        );
        let check_results = check_results
            .into_iter()
            .zip(txs.iter())
            .map(|(result, tx)| match result {
                Ok(_) => {
                    if bank.vote_only_bank() && !vote_parser::is_valid_vote_only_transaction(tx) {
                        Err(TransactionError::SanitizeFailure)
                    } else {
                        Ok(())
                    }
                }
                Err(err) => Err(err),
            });

        let mut output = self.process_and_record_transactions_with_pre_results(
            bank,
            txs,
            check_results,
            ExecutionFlags {
                drop_on_failure: false,
                all_or_nothing: false,
            },
        );

        // Accumulate error counters from the initial checks into final results
        output
            .execute_and_commit_transactions_output
            .error_counters
            .accumulate(&error_counters);
        output
    }

    pub fn process_and_record_aged_transactions(
        &self,
        bank: &Bank,
        txs: &[impl TransactionWithMeta],
        max_ages: &[MaxAge],
        flags: ExecutionFlags,
    ) -> ProcessTransactionBatchOutput {
        // Need to filter out transactions since they were sanitized earlier.
        // This means that the transaction may cross and epoch boundary (not allowed),
        //  or account lookup tables may have been closed.
        let pre_results = txs.iter().zip(max_ages).map(|(tx, max_age)| {
            bank.resanitize_transaction_minimally(
                tx,
                max_age.sanitized_epoch,
                max_age.alt_invalidation_slot,
            )
        });
        self.process_and_record_transactions_with_pre_results(bank, txs, pre_results, flags)
    }

    fn process_and_record_transactions_with_pre_results(
        &self,
        bank: &Bank,
        txs: &[impl TransactionWithMeta],
        pre_results: impl Iterator<Item = Result<(), TransactionError>>,
        flags: ExecutionFlags,
    ) -> ProcessTransactionBatchOutput {
        // Only lock accounts for transactions that passed pre-lock checks;
        // Once accounts are locked, other threads cannot encode transactions that will modify the
        // same account state
        let (batch, lock_us) =
            measure_us!(bank.prepare_sanitized_batch_with_results(txs, pre_results));

        let execute_and_commit_transactions_output =
            self.execute_and_commit_transactions_locked(bank, &batch, flags);

        // Once the accounts are new transactions can enter the pipeline to process them
        let (_, unlock_us) = measure_us!(drop(batch));

        debug!(
            "bank: {} lock: {}us unlock: {}us txs_len: {}",
            bank.slot(),
            lock_us,
            unlock_us,
            txs.len(),
        );

        ProcessTransactionBatchOutput {
            cost_model_throttled_transactions_count: execute_and_commit_transactions_output
                .cost_model_throttled_transactions_count,
            cost_model_us: execute_and_commit_transactions_output.cost_model_us,
            execute_and_commit_transactions_output,
        }
    }

    fn execute_and_commit_transactions_locked(
        &self,
        bank: &Bank,
        batch: &TransactionBatch<impl TransactionWithMeta>,
        flags: ExecutionFlags,
    ) -> ExecuteAndCommitTransactionsOutput {
        let transaction_status_sender_enabled = self.committer.transaction_status_sender_enabled();
        let mut execute_and_commit_timings = LeaderExecuteAndCommitTimings::default();

        let mut error_counters = TransactionErrorMetrics::default();
        let mut retryable_transaction_indexes: Vec<_> = batch
            .lock_results()
            .iter()
            .enumerate()
            .filter_map(|(index, res)| match res {
                // Account lock conflicts are immediately retryable.
                Err(TransactionError::AccountInUse) => {
                    error_counters.account_in_use += 1;
                    // locking failure due to vote conflict or jito - immediately retry.
                    Some(RetryableIndex {
                        index,
                        immediately_retryable: true,
                    })
                }
                // following are non-retryable errors
                Err(TransactionError::TooManyAccountLocks) => {
                    error_counters.too_many_account_locks += 1;
                    None
                }
                Err(_) => None,
                Ok(_) => None,
            })
            .collect();

        let (load_and_execute_transactions_output, load_execute_us) =
            measure_us!(bank.load_and_execute_transactions(
                batch,
                bank.max_processing_age(),
                &mut execute_and_commit_timings.execute_timings,
                &mut error_counters,
                TransactionProcessingConfig {
                    account_overrides: None,
                    check_program_deployment_slot: bank.check_program_deployment_slot(),
                    log_messages_bytes_limit: self.log_messages_bytes_limit,
                    limit_to_load_programs: true,
                    recording_config: ExecutionRecordingConfig::new_single_setting(
                        transaction_status_sender_enabled
                    ),
                    drop_on_failure: flags.drop_on_failure,
                    all_or_nothing: flags.all_or_nothing,
                    strict_nonce_size_check: true,
                    drop_noop_transactions: true,
                }
            ));
        execute_and_commit_timings.load_execute_us = load_execute_us;

        let LoadAndExecuteTransactionsOutput {
            mut processing_results,
            mut processed_counts,
            balance_collector,
        } = load_and_execute_transactions_output;

        // Calculate actual transaction costs before blocking freeze. Processed
        // transactions' costs are added to Cost Tracker while holding bank
        // freeze_lock, ensuring cost_update_service to report finalized stats.
        let (transaction_costs, mut cost_model_us) =
            measure_us!(Self::calculate_processed_transaction_costs(
                bank,
                batch.sanitized_transactions(),
                &processing_results,
            ));

        let (freeze_lock, freeze_lock_us) = measure_us!(bank.freeze_lock());
        execute_and_commit_timings.freeze_lock_us = freeze_lock_us;
        let bank_already_frozen = *freeze_lock != Hash::default();
        if bank_already_frozen {
            let transaction_counts = LeaderProcessedTransactionCounts {
                processed_count: processed_counts.processed_transactions_count,
                processed_with_successful_result_count: processed_counts
                    .processed_with_successful_result_count,
                attempted_processing_count: processing_results.len() as u64,
            };
            Self::extend_processed_retryable_transaction_indexes(
                &mut retryable_transaction_indexes,
                &processing_results,
            );

            return ExecuteAndCommitTransactionsOutput {
                cost_model_throttled_transactions_count: 0,
                cost_model_us,
                transaction_counts,
                retryable_transaction_indexes,
                commit_transactions_result: Err(PohRecorderError::MaxHeightReached),
                execute_and_commit_timings,
                error_counters,
            };
        }

        let ((transaction_costs, mut actual_cost_retryable_transaction_indexes), cost_add_us) =
            measure_us!(Self::try_add_processed_transaction_costs(
                bank,
                batch.sanitized_transactions(),
                transaction_costs,
                &mut processing_results,
                &mut processed_counts,
                &mut error_counters,
                flags.all_or_nothing,
            ));
        cost_model_us = cost_model_us.saturating_add(cost_add_us);
        let cost_model_throttled_transactions_count =
            actual_cost_retryable_transaction_indexes.len() as u64;
        retryable_transaction_indexes.append(&mut actual_cost_retryable_transaction_indexes);
        retryable_transaction_indexes.sort_unstable();

        let transaction_counts = LeaderProcessedTransactionCounts {
            processed_count: processed_counts.processed_transactions_count,
            processed_with_successful_result_count: processed_counts
                .processed_with_successful_result_count,
            attempted_processing_count: processing_results.len() as u64,
        };

        let mut entry_bytes = SERIALIZED_ENTRIES_OVERHEAD;
        let (processed_transactions, processing_results_to_transactions_us) = measure_us!({
            let mut processed_transactions =
                Vec::with_capacity(processed_counts.processed_transactions_count as usize);
            for (processing_result, tx) in processing_results
                .iter()
                .zip(batch.sanitized_transactions())
            {
                if processing_result.was_processed() {
                    entry_bytes += tx.serialized_size() as u64;
                    processed_transactions.push(tx.to_versioned_transaction());
                }
            }
            processed_transactions
        });

        let reserved_bytes =
            bank.entry_bytes_budget()
                .reserve(entry_bytes)
                .map_err(|err| match err {
                    EntryBytesReserveError::ExceedsSlotLimit => PohRecorderError::MaxHeightReached,
                });
        let (record_transactions_summary, record_us) = measure_us!(reserved_bytes.map(|_| {
            self.transaction_recorder
                .record_transactions(bank.bank_id(), processed_transactions)
        }));
        execute_and_commit_timings.record_us = record_us;

        let (recording_result, starting_transaction_index) = match record_transactions_summary {
            Ok(summary) => {
                execute_and_commit_timings.record_transactions_timings =
                    RecordTransactionsTimings {
                        processing_results_to_transactions_us: Saturating(
                            processing_results_to_transactions_us,
                        ),
                        ..summary.record_transactions_timings
                    };
                (summary.result, summary.starting_transaction_index)
            }
            Err(err) => (Err(err), None),
        };

        if let Err(recorder_err) = recording_result {
            Self::remove_added_transaction_costs(bank, &transaction_costs);

            Self::extend_processed_retryable_transaction_indexes(
                &mut retryable_transaction_indexes,
                &processing_results,
            );

            return ExecuteAndCommitTransactionsOutput {
                cost_model_throttled_transactions_count,
                cost_model_us,
                transaction_counts,
                retryable_transaction_indexes,
                commit_transactions_result: Err(recorder_err),
                execute_and_commit_timings,
                error_counters,
            };
        }

        let (commit_time_us, commit_transaction_statuses) =
            if processed_counts.processed_transactions_count != 0 {
                self.committer.commit_transactions(
                    batch,
                    processing_results,
                    starting_transaction_index,
                    bank,
                    balance_collector,
                    &mut execute_and_commit_timings,
                    &processed_counts,
                )
            } else {
                (
                    0,
                    processing_results
                        .into_iter()
                        .map(|processing_result| match processing_result {
                            Ok(_) => unreachable!("processed transaction count is 0"),
                            Err(err) => CommitTransactionDetails::NotCommitted(err),
                        })
                        .collect(),
                )
            };

        drop(freeze_lock);

        debug!(
            "bank: {} process_and_record_locked: {}us record: {}us commit: {}us txs_len: {}",
            bank.slot(),
            load_execute_us,
            record_us,
            commit_time_us,
            batch.sanitized_transactions().len(),
        );

        debug!(
            "execute_and_commit_transactions_locked: {:?}",
            execute_and_commit_timings.execute_timings,
        );

        debug_assert_eq!(
            transaction_counts.attempted_processing_count,
            commit_transaction_statuses.len() as u64,
        );

        ExecuteAndCommitTransactionsOutput {
            cost_model_throttled_transactions_count,
            cost_model_us,
            transaction_counts,
            retryable_transaction_indexes,
            commit_transactions_result: Ok(commit_transaction_statuses),
            execute_and_commit_timings,
            error_counters,
        }
    }

    fn extend_processed_retryable_transaction_indexes(
        retryable_transaction_indexes: &mut Vec<RetryableIndex>,
        processing_results: &[TransactionProcessingResult],
    ) {
        retryable_transaction_indexes.extend(processing_results.iter().enumerate().filter_map(
            |(index, processing_result)| {
                processing_result.was_processed().then_some(RetryableIndex {
                    index,
                    immediately_retryable: true, // block-ending errors are immediately retryable
                })
            },
        ));

        // retryable indexes are expected to be sorted - in this case the
        // `extend` can cause that assumption to be violated.
        retryable_transaction_indexes.sort_unstable();
    }

    fn calculate_processed_transaction_costs<'a, Tx: TransactionWithMeta>(
        bank: &Bank,
        transactions: &'a [Tx],
        processing_results: &[TransactionProcessingResult],
    ) -> Vec<Option<TransactionCost<'a, Tx>>> {
        let mut transaction_costs = Vec::with_capacity(processing_results.len());

        for (tx, processing_result) in transactions.iter().zip(processing_results) {
            let Some((executed_units, loaded_accounts_data_size)) = processing_result
                .processed_transaction()
                .map(|processed_tx| {
                    (
                        processed_tx.executed_units(),
                        processed_tx.loaded_accounts_data_size(),
                    )
                })
            else {
                transaction_costs.push(None);
                continue;
            };

            transaction_costs.push(Some(CostModel::calculate_cost_for_executed_transaction(
                tx,
                executed_units,
                loaded_accounts_data_size,
                &bank.feature_set,
            )));
        }

        transaction_costs
    }

    /// Returns `(added_transaction_costs, retryable_indexes)`.
    ///
    /// `added_transaction_costs` has one entry per input transaction. `Some(cost)` means
    /// the processed transaction's actual cost was added to the cost tracker.
    /// `None` means no cost added for that transaction due to: the transaction was
    /// not processed, its cost was rejected by the cost tracker, a prior
    /// cost-tracker rejection canceled the rest of the batch, or an
    /// all-or-nothing cost failure rolled back the batch.
    fn try_add_processed_transaction_costs<'a, Tx: TransactionWithMeta>(
        bank: &Bank,
        transactions: &'a [Tx],
        mut transaction_costs: Vec<Option<TransactionCost<'a, Tx>>>,
        processing_results: &mut [TransactionProcessingResult],
        processed_counts: &mut ProcessedTransactionCounts,
        error_counters: &mut TransactionErrorMetrics,
        all_or_nothing: bool,
    ) -> (Vec<Option<TransactionCost<'a, Tx>>>, Vec<RetryableIndex>) {
        let mut retryable_transaction_indexes = Vec::with_capacity(processing_results.len());
        let mut all_or_nothing_error = None;
        let mut remaining_batch_error = None;
        let mut cost_tracker = bank.write_cost_tracker().unwrap();

        for (index, transaction_cost) in transaction_costs.iter_mut().enumerate() {
            let Some(cost) = transaction_cost.as_ref() else {
                continue;
            };

            match cost_tracker.try_add(cost) {
                Ok(_) => {}
                Err(err) => {
                    let transaction_error = TransactionError::from(err);
                    *transaction_cost = None;
                    if all_or_nothing {
                        all_or_nothing_error = Some((index, transaction_error));
                        break;
                    } else {
                        remaining_batch_error = Some((index, transaction_error));
                        break;
                    }
                }
            }
        }

        if let Some((failed_index, transaction_error)) = all_or_nothing_error {
            for transaction_cost in transaction_costs[..failed_index].iter().flatten() {
                cost_tracker.remove(transaction_cost);
            }
            transaction_costs.iter_mut().for_each(|cost| *cost = None);
            retryable_transaction_indexes.clear();

            Self::cancel_processed_transactions_for_retry(
                transactions,
                processing_results,
                processed_counts,
                error_counters,
                &mut retryable_transaction_indexes,
                failed_index,
                &transaction_error,
                0,
            );
        } else if let Some((failed_index, transaction_error)) = remaining_batch_error {
            transaction_costs[failed_index..]
                .iter_mut()
                .for_each(|cost| *cost = None);

            Self::cancel_processed_transactions_for_retry(
                transactions,
                processing_results,
                processed_counts,
                error_counters,
                &mut retryable_transaction_indexes,
                failed_index,
                &transaction_error,
                failed_index,
            );
        }

        (transaction_costs, retryable_transaction_indexes)
    }

    fn cancel_processed_transactions_for_retry<Tx: TransactionWithMeta>(
        transactions: &[Tx],
        processing_results: &mut [TransactionProcessingResult],
        processed_counts: &mut ProcessedTransactionCounts,
        error_counters: &mut TransactionErrorMetrics,
        retryable_transaction_indexes: &mut Vec<RetryableIndex>,
        failed_index: usize,
        transaction_error: &TransactionError,
        start_index: usize,
    ) {
        for (index, (tx, processing_result)) in transactions
            .iter()
            .zip(processing_results.iter_mut())
            .enumerate()
            .skip(start_index)
        {
            if processing_result.was_processed() {
                Self::decrement_processed_counts(tx, processing_result, processed_counts);
                let retry_error = if index == failed_index {
                    transaction_error.clone()
                } else {
                    TransactionError::CommitCancelled
                };
                Self::accumulate_post_execution_transaction_error(
                    processing_result,
                    &retry_error,
                    error_counters,
                );
                *processing_result = Err(retry_error);
                retryable_transaction_indexes.push(RetryableIndex {
                    index,
                    // The cost-limit failure should be held until a later retry opportunity.
                    // Transactions canceled behind it did not fail cost are immediately retryable..
                    immediately_retryable: index != failed_index,
                });
            }
        }
    }

    fn accumulate_post_execution_transaction_error(
        previous_processing_result: &TransactionProcessingResult,
        transaction_error: &TransactionError,
        error_counters: &mut TransactionErrorMetrics,
    ) {
        // To avoid double counting, only increment `total` by 1 if a successful transaction
        // is turned into error (eg cost limit error, or CommitCancelled).
        if previous_processing_result.flattened_result().is_ok() {
            error_counters.total += 1;
        }
        Self::accumulate_cost_limit_error(transaction_error, error_counters);
    }

    fn remove_added_transaction_costs<Tx: TransactionWithMeta>(
        bank: &Bank,
        transaction_costs: &[Option<TransactionCost<'_, Tx>>],
    ) {
        let mut cost_tracker = bank.write_cost_tracker().unwrap();
        for transaction_cost in transaction_costs.iter().flatten() {
            cost_tracker.remove(transaction_cost);
        }
    }

    fn decrement_processed_counts(
        transaction: &impl TransactionWithMeta,
        processing_result: &TransactionProcessingResult,
        processed_counts: &mut ProcessedTransactionCounts,
    ) {
        processed_counts.processed_transactions_count = processed_counts
            .processed_transactions_count
            .saturating_sub(1);
        processed_counts.signature_count = processed_counts
            .signature_count
            .saturating_sub(transaction.signature_details().num_transaction_signatures());

        if !transaction.is_simple_vote_transaction() {
            processed_counts.processed_non_vote_transactions_count = processed_counts
                .processed_non_vote_transactions_count
                .saturating_sub(1);
        }

        if processing_result.was_processed_with_successful_result() {
            processed_counts.processed_with_successful_result_count = processed_counts
                .processed_with_successful_result_count
                .saturating_sub(1);
        }
    }

    fn accumulate_cost_limit_error(
        transaction_error: &TransactionError,
        error_counters: &mut TransactionErrorMetrics,
    ) {
        match transaction_error {
            TransactionError::WouldExceedMaxBlockCostLimit => {
                error_counters.would_exceed_max_block_cost_limit += 1;
            }
            TransactionError::WouldExceedMaxVoteCostLimit => {
                error_counters.would_exceed_max_vote_cost_limit += 1;
            }
            TransactionError::WouldExceedMaxAccountCostLimit => {
                error_counters.would_exceed_max_account_cost_limit += 1;
            }
            TransactionError::WouldExceedAccountDataBlockLimit => {
                error_counters.would_exceed_account_data_block_limit += 1;
            }
            _ => {}
        }
    }

    pub fn check_fee_payer_unlocked(
        bank: &Bank,
        transaction: &impl TransactionWithMeta,
        error_counters: &mut TransactionErrorMetrics,
    ) -> Result<(), TransactionError> {
        let fee_payer = transaction.fee_payer();
        let transaction_configuration = transaction.transaction_configuration(&bank.feature_set)?;
        let fee = solana_fee::calculate_fee(
            transaction,
            bank.fee_structure().lamports_per_signature,
            transaction_configuration.priority_fee_lamports,
            bank.fee_features(),
        );
        let (mut fee_payer_account, _slot) = bank
            .rc
            .accounts
            .load_with_fixed_root(&bank.ancestors, fee_payer)
            .ok_or(TransactionError::AccountNotFound)?;

        validate_fee_payer(
            &mut fee_payer_account,
            0,
            error_counters,
            &bank.rent_collector().rent,
            fee,
            bank.feature_set
                .snapshot()
                .relax_post_exec_min_balance_check,
        )
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::banking_stage::tests::{create_slow_genesis_config, sanitize_transactions},
        agave_reserved_account_keys::ReservedAccountKeys,
        crossbeam_channel::bounded,
        solana_account::{AccountSharedData, state_traits::StateMut},
        solana_address_lookup_table_interface::{
            self as address_lookup_table,
            state::{AddressLookupTable, LookupTableMeta},
        },
        solana_cost_model::{cost_model::CostModel, cost_tracker::CostTrackerLimits},
        solana_fee_calculator::FeeCalculator,
        solana_hash::Hash,
        solana_instruction::error::InstructionError,
        solana_keypair::Keypair,
        solana_leader_schedule::SlotLeader,
        solana_ledger::genesis_utils::{
            GenesisConfigInfo, bootstrap_validator_stake_lamports,
            create_genesis_config_with_leader,
        },
        solana_message::{
            MessageHeader, VersionedMessage,
            v0::{self, MessageAddressTableLookup},
        },
        solana_nonce::{self as nonce, state::DurableNonce},
        solana_nonce_account::verify_nonce_account,
        solana_poh::record_channels::{RecordReceiver, record_channels},
        solana_pubkey::Pubkey,
        solana_runtime::{
            bank_forks::BankForks,
            transaction_execution::{TransactionStatusMessage, TransactionStatusSender},
        },
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_signer::Signer,
        solana_system_interface::program as system_program,
        solana_system_transaction as system_transaction,
        solana_transaction::{
            Transaction, sanitized::MessageHash, versioned::VersionedTransaction,
        },
        std::{
            borrow::Cow,
            slice,
            sync::{Arc, RwLock},
        },
        test_case::test_case,
    };

    struct TestFrame {
        mint_keypair: Keypair,
        bank: Arc<Bank>,
        bank_forks: Arc<RwLock<BankForks>>,
        record_receiver: RecordReceiver,
        consumer: Consumer,
    }

    fn setup_test(transaction_status_sender: Option<TransactionStatusSender>) -> TestFrame {
        setup_test_with_lamports(10_000, transaction_status_sender)
    }

    fn setup_test_with_lamports(
        lamports: u64,
        transaction_status_sender: Option<TransactionStatusSender>,
    ) -> TestFrame {
        agave_logger::setup();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config_with_leader(
            lamports,
            &Pubkey::new_unique(),
            bootstrap_validator_stake_lamports(),
        );
        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();

        let (record_sender, mut record_receiver) = record_channels(false);
        let recorder = TransactionRecorder::new(record_sender);
        record_receiver.restart(bank.bank_id());

        let (replay_vote_sender, _replay_vote_receiver) = bounded(1024);
        let committer = Committer::new(transaction_status_sender, replay_vote_sender, None);
        let consumer = Consumer::new(committer, recorder, None);

        TestFrame {
            mint_keypair,
            bank,
            bank_forks,
            record_receiver,
            consumer,
        }
    }

    fn execute_transactions_for_test(
        bank: Arc<Bank>,
        transactions: Vec<Transaction>,
    ) -> ProcessTransactionBatchOutput {
        let transactions = sanitize_transactions(transactions);

        let (record_sender, mut record_receiver) = record_channels(false);
        let recorder = TransactionRecorder::new(record_sender);
        record_receiver.restart(bank.bank_id());

        let (replay_vote_sender, _replay_vote_receiver) = bounded(1024);
        let committer = Committer::new(None, replay_vote_sender, None);
        let consumer = Consumer::new(committer, recorder, None);
        consumer.process_and_record_transactions(&bank, &transactions)
    }

    fn generate_new_address_lookup_table(
        authority: Option<Pubkey>,
        num_addresses: usize,
    ) -> AddressLookupTable<'static> {
        let mut addresses = Vec::with_capacity(num_addresses);
        addresses.resize_with(num_addresses, Pubkey::new_unique);
        AddressLookupTable {
            meta: LookupTableMeta {
                authority,
                ..LookupTableMeta::default()
            },
            addresses: Cow::Owned(addresses),
        }
    }

    fn store_nonce_account(
        bank: &Bank,
        account_address: Pubkey,
        nonce_state: nonce::state::State,
    ) -> AccountSharedData {
        let mut account =
            AccountSharedData::new(1, nonce::state::State::size(), &system_program::id());
        account
            .set_state(&nonce::versions::Versions::new(nonce_state))
            .unwrap();
        bank.store_account(&account_address, &account);

        account
    }

    fn store_address_lookup_table(
        bank: &Bank,
        account_address: Pubkey,
        address_lookup_table: AddressLookupTable<'static>,
    ) -> AccountSharedData {
        let data = address_lookup_table.serialize_for_tests().unwrap();
        let mut account =
            AccountSharedData::new(1, data.len(), &address_lookup_table::program::id());
        account.set_data(data);
        bank.store_account(&account_address, &account);

        account
    }

    #[test]
    fn test_bank_process_and_record_transactions() {
        let TestFrame {
            mint_keypair,
            bank,
            bank_forks: _bank_forks,
            mut record_receiver,
            consumer,
        } = setup_test(None);

        let pubkey = solana_pubkey::new_rand();
        let transactions = sanitize_transactions(vec![system_transaction::transfer(
            &mint_keypair,
            &pubkey,
            1,
            bank.confirmed_last_blockhash(),
        )]);

        let process_transactions_batch_output =
            consumer.process_and_record_transactions(&bank, &transactions);

        let ExecuteAndCommitTransactionsOutput {
            transaction_counts,
            commit_transactions_result,
            ..
        } = process_transactions_batch_output.execute_and_commit_transactions_output;

        assert_eq!(
            transaction_counts,
            LeaderProcessedTransactionCounts {
                attempted_processing_count: 1,
                processed_count: 1,
                processed_with_successful_result_count: 1,
            }
        );
        assert!(commit_transactions_result.is_ok());

        // When poh is near end of slot, it will be shutdown.
        record_receiver.shutdown();

        let record = record_receiver.drain().next().unwrap();
        assert_eq!(record.bank_id, bank.bank_id());
        assert_eq!(record.transactions.len(), 1);

        let transactions = sanitize_transactions(vec![system_transaction::transfer(
            &mint_keypair,
            &pubkey,
            2,
            bank.confirmed_last_blockhash(),
        )]);

        let process_transactions_batch_output =
            consumer.process_and_record_transactions(&bank, &transactions);

        let ExecuteAndCommitTransactionsOutput {
            transaction_counts,
            retryable_transaction_indexes,
            commit_transactions_result,
            ..
        } = process_transactions_batch_output.execute_and_commit_transactions_output;
        assert_eq!(
            transaction_counts,
            LeaderProcessedTransactionCounts {
                attempted_processing_count: 1,
                // Transaction was still processed, just wasn't committed, so should be counted here.
                processed_count: 1,
                processed_with_successful_result_count: 1,
            }
        );
        assert_eq!(
            retryable_transaction_indexes,
            vec![RetryableIndex {
                index: 0,
                immediately_retryable: true
            }]
        );
        assert_matches!(
            commit_transactions_result,
            Err(PohRecorderError::MaxHeightReached)
        );

        assert_eq!(bank.get_balance(&pubkey), 1);
    }

    #[test]
    fn test_bank_process_and_record_transactions_already_frozen() {
        let TestFrame {
            mint_keypair,
            bank,
            bank_forks: _bank_forks,
            record_receiver,
            consumer,
        } = setup_test(None);

        let pubkey = solana_pubkey::new_rand();
        let transactions = sanitize_transactions(vec![system_transaction::transfer(
            &mint_keypair,
            &pubkey,
            1,
            bank.confirmed_last_blockhash(),
        )]);

        bank.freeze();
        assert_ne!(bank.hash(), Hash::default());

        let process_transactions_batch_output =
            consumer.process_and_record_transactions(&bank, &transactions);

        let ExecuteAndCommitTransactionsOutput {
            transaction_counts,
            retryable_transaction_indexes,
            commit_transactions_result,
            ..
        } = process_transactions_batch_output.execute_and_commit_transactions_output;
        assert_eq!(
            transaction_counts,
            LeaderProcessedTransactionCounts {
                attempted_processing_count: 1,
                processed_count: 1,
                processed_with_successful_result_count: 1,
            }
        );
        assert_eq!(
            retryable_transaction_indexes,
            vec![RetryableIndex {
                index: 0,
                immediately_retryable: true,
            }]
        );
        assert_matches!(
            commit_transactions_result,
            Err(PohRecorderError::MaxHeightReached)
        );

        let cost_tracker = bank.read_cost_tracker().unwrap();
        assert_eq!(cost_tracker.transaction_count(), 0);
        assert_eq!(cost_tracker.block_cost(), 0);
        drop(cost_tracker);
        assert!(record_receiver.try_recv().is_err());
        assert_eq!(bank.get_balance(&pubkey), 0);
    }

    #[test]
    fn test_bank_nonce_update_blockhash_queried_before_transaction_record() {
        let TestFrame {
            mint_keypair,
            bank,
            bank_forks: _bank_forks,
            record_receiver: _record_receiver,
            consumer,
        } = setup_test(None);
        let pubkey = Pubkey::new_unique();

        // setup nonce account with a durable nonce different from the current
        // bank so that it can be advanced in this bank
        let durable_nonce = DurableNonce::from_blockhash(&Hash::new_unique());
        let nonce_hash = *durable_nonce.as_hash();
        let nonce_pubkey = Pubkey::new_unique();
        let nonce_state = nonce::state::State::Initialized(nonce::state::Data {
            authority: mint_keypair.pubkey(),
            durable_nonce,
            fee_calculator: FeeCalculator::new(5000),
        });

        store_nonce_account(&bank, nonce_pubkey, nonce_state);

        // setup a valid nonce tx which will fail during execution
        let transactions = sanitize_transactions(vec![system_transaction::nonced_transfer(
            &mint_keypair,
            &pubkey,
            u64::MAX,
            &nonce_pubkey,
            &mint_keypair,
            nonce_hash,
        )]);
        // get original backhash before we tick to the end.
        let bank_hash = bank.last_blockhash();

        while bank.tick_height() != bank.max_tick_height() - 1 {
            bank.register_default_tick_for_test();
        }

        let process_transactions_batch_output =
            consumer.process_and_record_transactions(&bank, &transactions);
        let ExecuteAndCommitTransactionsOutput {
            transaction_counts,
            commit_transactions_result,
            ..
        } = process_transactions_batch_output.execute_and_commit_transactions_output;

        assert_eq!(
            transaction_counts,
            LeaderProcessedTransactionCounts {
                attempted_processing_count: 1,
                processed_count: 1,
                processed_with_successful_result_count: 0,
            }
        );
        assert!(commit_transactions_result.is_ok());
        bank.register_default_tick_for_test();

        // check that the nonce was advanced to the current bank's last blockhash
        // rather than the current bank's blockhash as would occur had the update
        // blockhash been queried _after_ transaction recording
        let expected_nonce = DurableNonce::from_blockhash(&bank_hash);
        let expected_nonce_hash = expected_nonce.as_hash();
        let nonce_account = bank.get_account(&nonce_pubkey).unwrap();
        assert!(verify_nonce_account(&nonce_account, expected_nonce_hash).is_some());
    }

    #[test]
    fn test_bank_process_and_record_transactions_all_unexecuted() {
        let TestFrame {
            mint_keypair,
            bank,
            bank_forks: _bank_forks,
            record_receiver: _record_receiver,
            consumer,
        } = setup_test(None);

        let pubkey = solana_pubkey::new_rand();
        let transactions = {
            let mut tx =
                system_transaction::transfer(&mint_keypair, &pubkey, 1, bank.last_blockhash());
            // Add duplicate account key
            tx.message.account_keys.push(pubkey);
            sanitize_transactions(vec![tx])
        };

        let process_transactions_batch_output =
            consumer.process_and_record_transactions(&bank, &transactions);

        let ExecuteAndCommitTransactionsOutput {
            transaction_counts,
            commit_transactions_result,
            retryable_transaction_indexes,
            ..
        } = process_transactions_batch_output.execute_and_commit_transactions_output;

        assert_eq!(
            transaction_counts,
            LeaderProcessedTransactionCounts {
                attempted_processing_count: 1,
                processed_count: 0,
                processed_with_successful_result_count: 0,
            }
        );
        assert!(retryable_transaction_indexes.is_empty());
        assert_eq!(
            commit_transactions_result.ok(),
            Some(vec![
                CommitTransactionDetails::NotCommitted(
                    TransactionError::AccountLoadedTwice
                );
                1
            ])
        );
    }

    #[test]
    fn test_bank_process_and_record_transactions_cost_tracker() {
        let TestFrame {
            mint_keypair,
            bank,
            bank_forks: _bank_forks,
            record_receiver: _record_receiver,
            consumer,
        } = setup_test(None);

        let pubkey = solana_pubkey::new_rand();

        let get_block_cost = || bank.read_cost_tracker().unwrap().block_cost();
        let get_tx_count = || bank.read_cost_tracker().unwrap().transaction_count();
        assert_eq!(get_block_cost(), 0);
        assert_eq!(get_tx_count(), 0);

        //
        // TEST: cost tracker's block cost increases when successfully processing a tx
        //

        let transactions = sanitize_transactions(vec![system_transaction::transfer(
            &mint_keypair,
            &pubkey,
            1,
            bank.last_blockhash(),
        )]);

        let process_transactions_batch_output =
            consumer.process_and_record_transactions(&bank, &transactions);

        let ExecuteAndCommitTransactionsOutput {
            transaction_counts,
            commit_transactions_result,
            ..
        } = process_transactions_batch_output.execute_and_commit_transactions_output;
        assert_eq!(transaction_counts.processed_with_successful_result_count, 1);
        assert!(commit_transactions_result.is_ok());

        let block_cost = get_block_cost();
        assert_ne!(block_cost, 0);
        assert_eq!(get_tx_count(), 1);

        // TEST: it's expected that the allocation will execute but the transfer will not
        // because of a shared write-lock between mint_keypair. Ensure only the first transaction
        // takes compute units in the block
        let allocate_keypair = Keypair::new();
        let transactions = sanitize_transactions(vec![
            system_transaction::allocate(
                &mint_keypair,
                &allocate_keypair,
                bank.last_blockhash(),
                100,
            ),
            // this one won't execute in process_and_record_transactions from shared account lock overlap
            system_transaction::transfer(&mint_keypair, &pubkey, 2, bank.last_blockhash()),
        ]);

        let conflicting_transaction = sanitize_transactions(vec![system_transaction::transfer(
            &Keypair::new(),
            &pubkey,
            1,
            bank.last_blockhash(),
        )]);
        bank.try_lock_accounts(&conflicting_transaction);

        let process_transactions_batch_output =
            consumer.process_and_record_transactions(&bank, &transactions);

        let ExecuteAndCommitTransactionsOutput {
            transaction_counts,
            commit_transactions_result,
            retryable_transaction_indexes,
            ..
        } = process_transactions_batch_output.execute_and_commit_transactions_output;
        assert_eq!(transaction_counts.processed_with_successful_result_count, 1);
        assert!(commit_transactions_result.is_ok());

        // first one should have been committed, second one not committed due to AccountInUse error during
        // account locking
        let commit_transactions_result = commit_transactions_result.unwrap();
        assert_eq!(commit_transactions_result.len(), 2);
        assert_matches!(
            commit_transactions_result.first(),
            Some(CommitTransactionDetails::Committed { .. })
        );
        assert_matches!(
            commit_transactions_result.get(1),
            Some(CommitTransactionDetails::NotCommitted(_))
        );
        assert_eq!(
            retryable_transaction_indexes,
            vec![RetryableIndex::new(1, true)]
        );

        let expected_block_cost = {
            let (actual_programs_execution_cost, actual_loaded_accounts_data_size_cost) =
                match commit_transactions_result.first().unwrap() {
                    CommitTransactionDetails::Committed {
                        compute_units,
                        loaded_accounts_data_size,
                        result: _,
                        fee_payer_post_balance: _,
                    } => (
                        *compute_units,
                        CostModel::calculate_loaded_accounts_data_size_cost(
                            *loaded_accounts_data_size,
                            &bank.feature_set,
                        ),
                    ),
                    CommitTransactionDetails::NotCommitted(_err) => {
                        unreachable!()
                    }
                };

            let mut cost = CostModel::calculate_cost(&transactions[0], &bank.feature_set);
            let usage_cost = cost.usage_cost_details_mut();
            usage_cost.programs_execution_cost = actual_programs_execution_cost;
            usage_cost.loaded_accounts_data_size_cost = actual_loaded_accounts_data_size_cost;

            block_cost + cost.sum()
        };

        assert_eq!(get_block_cost(), expected_block_cost);
        assert_eq!(get_tx_count(), 2);
    }

    #[test]
    fn test_bank_process_and_record_transactions_cost_tracker_noop() {
        let TestFrame {
            mint_keypair: _mint_keypair,
            bank,
            bank_forks: _bank_forks,
            record_receiver: _record_receiver,
            consumer,
        } = setup_test(None);

        let get_block_cost = || bank.read_cost_tracker().unwrap().block_cost();
        let get_tx_count = || bank.read_cost_tracker().unwrap().transaction_count();
        assert_eq!(get_block_cost(), 0);
        assert_eq!(get_tx_count(), 0);

        // TEST: a blockhash transaction with an invalid fee-payer is committed as a no-op
        // on replay (with `relax_fee_payer_constraint`), but during block production
        // `drop_noop_transactions` turns it into an error. It must not be committed and
        // must leave the cost tracker untouched.
        let transactions = sanitize_transactions(vec![system_transaction::transfer(
            &Keypair::new(),
            &Pubkey::new_unique(),
            1,
            bank.last_blockhash(),
        )]);

        let process_transactions_batch_output =
            consumer.process_and_record_transactions(&bank, &transactions);

        let ExecuteAndCommitTransactionsOutput {
            transaction_counts,
            commit_transactions_result,
            ..
        } = process_transactions_batch_output.execute_and_commit_transactions_output;

        // the no-op transaction is not committed
        assert_eq!(transaction_counts.processed_with_successful_result_count, 0);
        assert_eq!(
            commit_transactions_result.ok(),
            Some(vec![CommitTransactionDetails::NotCommitted(
                TransactionError::AccountNotFound
            )])
        );

        // and the cost tracker is unchanged after processing it
        assert_eq!(get_block_cost(), 0);
        assert_eq!(get_tx_count(), 0);
    }

    #[test]
    fn test_actual_cost_limit_rejects_after_execution_before_record() {
        const TRANSACTION_COUNT: usize = 32;

        let TestFrame {
            mint_keypair,
            bank,
            bank_forks: _bank_forks,
            record_receiver,
            consumer,
        } = setup_test_with_lamports(1_000_000_000, None);

        let payer_keypairs = (0..TRANSACTION_COUNT)
            .map(|_| Keypair::new())
            .collect::<Vec<_>>();
        for payer in &payer_keypairs {
            bank.transfer(1_000_000, &mint_keypair, &payer.pubkey())
                .unwrap();
        }

        let transactions = sanitize_transactions(
            payer_keypairs
                .iter()
                .map(|payer| {
                    system_transaction::transfer(
                        payer,
                        &Pubkey::new_unique(),
                        1,
                        bank.last_blockhash(),
                    )
                })
                .collect(),
        );
        let estimated_cost = CostModel::calculate_cost(&transactions[0], &bank.feature_set).sum();
        let block_limit = estimated_cost.saturating_mul(2);
        bank.write_cost_tracker()
            .unwrap()
            .set_limits(CostTrackerLimits {
                account_cost: block_limit,
                block_cost: block_limit,
                allocated_data_size: u64::MAX,
            });

        let process_transactions_batch_output =
            consumer.process_and_record_transactions(&bank, &transactions);
        let ProcessTransactionBatchOutput {
            cost_model_throttled_transactions_count,
            execute_and_commit_transactions_output,
            ..
        } = process_transactions_batch_output;
        let ExecuteAndCommitTransactionsOutput {
            transaction_counts,
            retryable_transaction_indexes,
            commit_transactions_result,
            error_counters,
            ..
        } = execute_and_commit_transactions_output;
        let commit_transaction_details = commit_transactions_result.unwrap();
        let committed_count = commit_transaction_details
            .iter()
            .filter(|details| matches!(details, CommitTransactionDetails::Committed { .. }))
            .count();
        let late_cost_rejected_indexes = commit_transaction_details
            .iter()
            .enumerate()
            .filter_map(|(index, details)| {
                matches!(
                    details,
                    CommitTransactionDetails::NotCommitted(
                        TransactionError::WouldExceedMaxBlockCostLimit
                    )
                )
                .then_some(index)
            })
            .collect::<Vec<_>>();
        let commit_cancelled_count = commit_transaction_details
            .iter()
            .filter(|details| {
                matches!(
                    details,
                    CommitTransactionDetails::NotCommitted(TransactionError::CommitCancelled)
                )
            })
            .count();
        let retryable_indexes = (committed_count..TRANSACTION_COUNT)
            .map(|index| RetryableIndex::new(index, index != committed_count))
            .collect::<Vec<_>>();

        assert!(committed_count > 0);
        assert_eq!(late_cost_rejected_indexes, vec![committed_count]);
        assert_eq!(
            commit_cancelled_count,
            TRANSACTION_COUNT - committed_count - late_cost_rejected_indexes.len()
        );
        assert_eq!(error_counters.total.0, TRANSACTION_COUNT - committed_count);
        assert_eq!(
            error_counters.would_exceed_max_block_cost_limit.0,
            late_cost_rejected_indexes.len()
        );
        assert_eq!(
            cost_model_throttled_transactions_count,
            retryable_indexes.len() as u64
        );
        assert_eq!(
            transaction_counts.attempted_processing_count,
            TRANSACTION_COUNT as u64
        );
        assert_eq!(transaction_counts.processed_count, committed_count as u64);
        assert_eq!(
            transaction_counts.processed_with_successful_result_count,
            committed_count as u64
        );
        assert_eq!(retryable_transaction_indexes, retryable_indexes);

        let cost_tracker = bank.read_cost_tracker().unwrap();
        assert_eq!(cost_tracker.transaction_count(), committed_count as u64);
        assert!(cost_tracker.block_cost() <= block_limit);
        drop(cost_tracker);

        let record = record_receiver.drain().next().unwrap();
        assert_eq!(record.transactions.len(), committed_count);
    }

    #[test]
    fn test_actual_cost_limit_honors_all_or_nothing() {
        const TRANSACTION_COUNT: usize = 32;

        let TestFrame {
            mint_keypair,
            bank,
            bank_forks: _bank_forks,
            record_receiver: _record_receiver,
            consumer,
        } = setup_test_with_lamports(1_000_000_000, None);

        let payer_keypairs = (0..TRANSACTION_COUNT)
            .map(|_| Keypair::new())
            .collect::<Vec<_>>();
        for payer in &payer_keypairs {
            bank.transfer(1_000_000, &mint_keypair, &payer.pubkey())
                .unwrap();
        }

        let transactions = sanitize_transactions(
            payer_keypairs
                .iter()
                .map(|payer| {
                    system_transaction::transfer(
                        payer,
                        &Pubkey::new_unique(),
                        1,
                        bank.last_blockhash(),
                    )
                })
                .collect(),
        );
        let estimated_cost = CostModel::calculate_cost(&transactions[0], &bank.feature_set).sum();
        let block_limit = estimated_cost.saturating_mul(2);
        bank.write_cost_tracker()
            .unwrap()
            .set_limits(CostTrackerLimits {
                account_cost: block_limit,
                block_cost: block_limit,
                allocated_data_size: u64::MAX,
            });

        let process_transactions_batch_output = consumer
            .process_and_record_transactions_with_pre_results(
                &bank,
                &transactions,
                std::iter::repeat(Ok(())),
                ExecutionFlags {
                    drop_on_failure: false,
                    all_or_nothing: true,
                },
            );
        let ExecuteAndCommitTransactionsOutput {
            transaction_counts,
            retryable_transaction_indexes,
            commit_transactions_result,
            error_counters,
            ..
        } = process_transactions_batch_output.execute_and_commit_transactions_output;
        let commit_transaction_details = commit_transactions_result.unwrap();

        assert_eq!(
            process_transactions_batch_output.cost_model_throttled_transactions_count,
            TRANSACTION_COUNT as u64
        );
        assert_eq!(
            transaction_counts.attempted_processing_count,
            TRANSACTION_COUNT as u64
        );
        assert_eq!(transaction_counts.processed_count, 0);
        assert_eq!(transaction_counts.processed_with_successful_result_count, 0);
        let cost_limit_failure_indexes = commit_transaction_details
            .iter()
            .enumerate()
            .filter_map(|(index, details)| {
                matches!(
                    details,
                    CommitTransactionDetails::NotCommitted(
                        TransactionError::WouldExceedMaxBlockCostLimit
                    )
                )
                .then_some(index)
            })
            .collect::<Vec<_>>();
        let commit_cancelled_count = commit_transaction_details
            .iter()
            .filter(|details| {
                matches!(
                    details,
                    CommitTransactionDetails::NotCommitted(TransactionError::CommitCancelled)
                )
            })
            .count();
        assert_eq!(cost_limit_failure_indexes.len(), 1);
        assert_eq!(commit_cancelled_count, TRANSACTION_COUNT - 1);
        assert_eq!(error_counters.total.0, TRANSACTION_COUNT);
        assert_eq!(error_counters.would_exceed_max_block_cost_limit.0, 1);
        let failed_index = cost_limit_failure_indexes[0];
        assert_eq!(
            retryable_transaction_indexes,
            (0..TRANSACTION_COUNT)
                .map(|index| RetryableIndex::new(index, index != failed_index))
                .collect::<Vec<_>>()
        );

        let cost_tracker = bank.read_cost_tracker().unwrap();
        assert_eq!(cost_tracker.transaction_count(), 0);
        assert_eq!(cost_tracker.block_cost(), 0);
    }

    #[test_case(false; "locked")]
    #[test_case(true; "duplicate")]
    fn test_bank_process_and_record_transactions_account_in_use(use_duplicate_transaction: bool) {
        let TestFrame {
            mint_keypair,
            bank,
            bank_forks: _bank_forks,
            record_receiver: _record_receiver,
            consumer,
        } = setup_test(None);

        let pubkey = solana_pubkey::new_rand();
        let pubkey1 = solana_pubkey::new_rand();

        let transactions = sanitize_transactions(vec![
            system_transaction::transfer(&mint_keypair, &pubkey, 1, bank.last_blockhash()),
            system_transaction::transfer(
                &mint_keypair,
                if use_duplicate_transaction {
                    &pubkey
                } else {
                    &pubkey1
                },
                1,
                bank.last_blockhash(),
            ),
        ]);
        assert_eq!(
            transactions[0].message_hash() == transactions[1].message_hash(),
            use_duplicate_transaction
        );

        // with a duplicate transaction, we get a conflict from message hash equality in the batch
        // with no duplicate, we must take a cross-batch lock on an account to create a conflict
        if !use_duplicate_transaction {
            let conflicting_transaction =
                sanitize_transactions(vec![system_transaction::transfer(
                    &Keypair::new(),
                    &pubkey1,
                    1,
                    bank.last_blockhash(),
                )]);
            bank.try_lock_accounts(&conflicting_transaction);
        }

        let process_transactions_batch_output =
            consumer.process_and_record_transactions(&bank, &transactions);

        let ExecuteAndCommitTransactionsOutput {
            transaction_counts,
            retryable_transaction_indexes,
            commit_transactions_result,
            ..
        } = process_transactions_batch_output.execute_and_commit_transactions_output;

        assert_eq!(
            transaction_counts,
            LeaderProcessedTransactionCounts {
                attempted_processing_count: 2,
                processed_count: 1,
                processed_with_successful_result_count: 1,
            }
        );
        assert!(commit_transactions_result.is_ok());

        // duplicate transactions are not retryable
        if use_duplicate_transaction {
            assert_eq!(retryable_transaction_indexes, Vec::<_>::new());
        } else {
            assert_eq!(
                retryable_transaction_indexes,
                vec![RetryableIndex::new(1, true)]
            );
        }
    }

    #[test]
    fn test_process_transactions_instruction_error() {
        agave_logger::setup();
        let lamports = 10_000;
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(lamports);
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        // set cost tracker limits to MAX so it will not filter out TXs
        bank.write_cost_tracker().unwrap().set_limits_max();

        // Transfer more than the balance of the mint keypair, should cause a
        // InstructionError::InsufficientFunds that is then committed.
        let transactions = vec![system_transaction::transfer(
            &mint_keypair,
            &Pubkey::new_unique(),
            lamports + 1,
            genesis_config.hash(),
        )];

        let transactions_len = transactions.len();
        let ProcessTransactionBatchOutput {
            execute_and_commit_transactions_output,
            ..
        } = execute_transactions_for_test(bank, transactions);

        // All the transactions should have been replayed
        assert_eq!(
            execute_and_commit_transactions_output
                .transaction_counts
                .attempted_processing_count,
            transactions_len as u64
        );
        assert_eq!(
            execute_and_commit_transactions_output
                .transaction_counts
                .processed_count,
            1,
        );
        assert_eq!(
            execute_and_commit_transactions_output
                .transaction_counts
                .processed_with_successful_result_count,
            0,
        );

        assert_eq!(
            execute_and_commit_transactions_output.retryable_transaction_indexes,
            (1..transactions_len - 1)
                .map(|index| RetryableIndex::new(index, true))
                .collect::<Vec<_>>()
        );
    }

    #[test_case(false; "locked")]
    #[test_case(true; "duplicate")]
    fn test_process_transactions_account_in_use(use_duplicate_transaction: bool) {
        agave_logger::setup();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(10_000);
        let mut bank = Bank::new_for_tests(&genesis_config);
        bank.ns_per_slot = u128::MAX;
        let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();
        // set cost tracker limits to MAX so it will not filter out TXs
        bank.write_cost_tracker().unwrap().set_limits_max();

        let mut transactions = vec![];
        let destination = Pubkey::new_unique();
        let mut amount = 1;

        // Make distinct, or identical, transactions that conflict on the `mint_keypair`
        for _ in 0..TARGET_NUM_TRANSACTIONS_PER_BATCH {
            transactions.push(system_transaction::transfer(
                &mint_keypair,
                &destination,
                amount,
                genesis_config.hash(),
            ));

            if !use_duplicate_transaction {
                amount += 1;
            }
        }

        let transactions_len = transactions.len();
        let ProcessTransactionBatchOutput {
            execute_and_commit_transactions_output,
            ..
        } = execute_transactions_for_test(bank, transactions);

        // if the transactions are distinct, all are executed.
        // otherwise, only one is executed. regardless, all are attempted.
        let execution_count = if !use_duplicate_transaction {
            transactions_len
        } else {
            1
        } as u64;

        assert_eq!(
            execute_and_commit_transactions_output
                .transaction_counts
                .attempted_processing_count,
            transactions_len as u64
        );
        assert_eq!(
            execute_and_commit_transactions_output
                .transaction_counts
                .processed_count,
            execution_count
        );
        assert_eq!(
            execute_and_commit_transactions_output
                .transaction_counts
                .processed_with_successful_result_count,
            execution_count
        );

        // If the transactions are distinct, there are zero retryable (all executed).
        // If the transactions are identical, there are zero retryable (marked AlreadyProcessed).
        assert_eq!(
            execute_and_commit_transactions_output.retryable_transaction_indexes,
            Vec::<_>::new()
        );
    }

    #[test]
    fn test_process_transactions_returns_unprocessed_txs() {
        let TestFrame {
            mint_keypair,
            bank,
            bank_forks: _bank_forks,
            mut record_receiver,
            consumer,
        } = setup_test(None);

        let pubkey = solana_pubkey::new_rand();

        let transactions = sanitize_transactions(vec![system_transaction::transfer(
            &mint_keypair,
            &pubkey,
            1,
            bank.last_blockhash(),
        )]);

        // Channel shutdown should result in error returned on record.
        record_receiver.shutdown();

        let process_transactions_summary =
            consumer.process_and_record_transactions(&bank, &transactions);

        let ProcessTransactionBatchOutput {
            mut execute_and_commit_transactions_output,
            ..
        } = process_transactions_summary;

        // Transaction is successfully processed, but not committed due to poh recording error.
        assert!(
            execute_and_commit_transactions_output
                .commit_transactions_result
                .is_err()
        );
        assert_eq!(
            execute_and_commit_transactions_output
                .transaction_counts
                .attempted_processing_count,
            1
        );
        assert_eq!(
            execute_and_commit_transactions_output
                .transaction_counts
                .processed_count,
            1
        );
        assert_eq!(
            execute_and_commit_transactions_output
                .transaction_counts
                .processed_with_successful_result_count,
            1
        );

        execute_and_commit_transactions_output
            .retryable_transaction_indexes
            .sort_unstable();
        let expected: Vec<_> = (0..transactions.len())
            .map(|index| RetryableIndex::new(index, true))
            .collect();
        assert_eq!(
            execute_and_commit_transactions_output.retryable_transaction_indexes,
            expected
        );
    }

    #[test]
    fn test_write_persist_transaction_status() {
        let (transaction_status_sender, transaction_status_receiver) = bounded(1024);
        let tss = Some(TransactionStatusSender {
            sender: transaction_status_sender,
            dependency_tracker: None,
        });
        let TestFrame {
            mint_keypair,
            bank,
            bank_forks: _bank_forks,
            record_receiver: _record_receiver,
            consumer,
        } = setup_test(tss);

        let pubkey = solana_pubkey::new_rand();
        let pubkey1 = solana_pubkey::new_rand();
        let keypair1 = Keypair::new();

        let rent_exempt_amount = bank.get_minimum_balance_for_rent_exemption(0);
        assert!(rent_exempt_amount > 0);

        let success_tx = system_transaction::transfer(
            &mint_keypair,
            &pubkey,
            rent_exempt_amount,
            bank.last_blockhash(),
        );
        let ix_error_tx = system_transaction::transfer(
            &keypair1,
            &pubkey1,
            2 * rent_exempt_amount,
            bank.last_blockhash(),
        );

        let transactions = sanitize_transactions(vec![success_tx, ix_error_tx]);
        let batch_transactions_inner = transactions
            .iter()
            .map(|tx| tx.clone().into_inner_transaction())
            .collect::<Vec<_>>();
        bank.transfer(rent_exempt_amount, &mint_keypair, &keypair1.pubkey())
            .unwrap();

        let _ = consumer.process_and_record_transactions(&bank, &transactions);
        drop(consumer); // drop/disconnect transaction_status_sender

        let status_messages = transaction_status_receiver.into_iter().collect::<Vec<_>>();
        assert_eq!(status_messages.len(), 1);
        let TransactionStatusMessage::Batch((status_batch, _)) =
            status_messages.into_iter().next().unwrap()
        else {
            panic!("not a batch");
        };
        assert_eq!(status_batch.transactions, batch_transactions_inner);
        let commit_results = status_batch
            .commit_results
            .into_iter()
            .map(|r| r.unwrap().status)
            .collect::<Vec<_>>();
        assert_eq!(
            commit_results,
            vec![
                Ok(()),
                Err(TransactionError::InstructionError(
                    0,
                    InstructionError::Custom(1)
                ))
            ]
        );
    }

    #[test]
    fn test_write_persist_loaded_addresses() {
        let (transaction_status_sender, transaction_status_receiver) = bounded(1024);
        let tss = Some(TransactionStatusSender {
            sender: transaction_status_sender,
            dependency_tracker: None,
        });
        let TestFrame {
            mint_keypair,
            bank,
            bank_forks,
            mut record_receiver,
            consumer,
        } = setup_test(tss);

        let keypair = Keypair::new();
        let address_table_key = Pubkey::new_unique();
        let address_table_state = generate_new_address_lookup_table(None, 2);
        store_address_lookup_table(&bank, address_table_key, address_table_state);

        let new_bank = Bank::new_from_parent(bank, SlotLeader::new_unique(), 2);
        let bank = bank_forks
            .write()
            .unwrap()
            .insert(new_bank)
            .clone_without_scheduler();

        record_receiver.shutdown();
        record_receiver.restart(bank.bank_id());

        let message = VersionedMessage::V0(v0::Message {
            header: MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 0,
            },
            recent_blockhash: bank.last_blockhash(),
            account_keys: vec![keypair.pubkey()],
            address_table_lookups: vec![MessageAddressTableLookup {
                account_key: address_table_key,
                writable_indexes: vec![0],
                readonly_indexes: vec![1],
            }],
            instructions: vec![],
        });

        let tx = VersionedTransaction::try_new(message, &[&keypair]).unwrap();
        let sanitized_tx = RuntimeTransaction::try_create(
            tx,
            MessageHash::Compute,
            Some(false),
            bank.as_ref(),
            &ReservedAccountKeys::empty_key_set(),
        )
        .unwrap();
        let batch_transactions_inner = [&sanitized_tx]
            .into_iter()
            .map(|tx| tx.clone().into_inner_transaction())
            .collect::<Vec<_>>();

        bank.transfer(1, &mint_keypair, &keypair.pubkey()).unwrap();

        let _ = consumer.process_and_record_transactions(&bank, slice::from_ref(&sanitized_tx));
        drop(consumer); // drop/disconnect transaction_status_sender

        let status_messages = transaction_status_receiver.into_iter().collect::<Vec<_>>();
        assert_eq!(status_messages.len(), 1);
        let TransactionStatusMessage::Batch((status_batch, _)) =
            status_messages.into_iter().next().unwrap()
        else {
            panic!("not a batch");
        };
        assert_eq!(status_batch.transactions, batch_transactions_inner);
        assert_eq!(status_batch.commit_results.len(), 1);
        let committed_transaction = status_batch
            .commit_results
            .into_iter()
            .next()
            .unwrap()
            .unwrap();
        assert!(committed_transaction.status.is_ok());
    }
}
