use {
    super::{
        committer::{CommitTransactionDetails, Committer, PreBalanceInfo},
        leader_slot_timing_metrics::LeaderExecuteAndCommitTimings,
        qos_service::QosService,
        scheduler_messages::MaxAge,
    },
    itertools::Itertools,
    solana_fee::FeeFeatures,
    solana_ledger::token_balances::collect_token_balances,
    solana_measure::measure_us,
    solana_poh::{
        poh_recorder::PohRecorderError,
        transaction_recorder::{
            RecordTransactionsSummary, RecordTransactionsTimings, TransactionRecorder,
        },
    },
    solana_runtime::{
        bank::{Bank, LoadAndExecuteTransactionsOutput},
        transaction_batch::TransactionBatch,
    },
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    solana_sdk::{clock::MAX_PROCESSING_AGE, fee::FeeBudgetLimits, transaction::TransactionError},
    solana_svm::{
        account_loader::validate_fee_payer,
        transaction_error_metrics::TransactionErrorMetrics,
        transaction_processing_result::TransactionProcessingResultExtensions,
        transaction_processor::{ExecutionRecordingConfig, TransactionProcessingConfig},
    },
    solana_timings::ExecuteTimings,
    std::{num::Saturating, sync::Arc},
};

/// Consumer will create chunks of transactions from buffer with up to this size.
pub const TARGET_NUM_TRANSACTIONS_PER_BATCH: usize = 64;

pub struct ProcessTransactionBatchOutput {
    // The number of transactions filtered out by the cost model
    pub(crate) cost_model_throttled_transactions_count: u64,
    // Amount of time spent running the cost model
    pub(crate) cost_model_us: u64,
    pub execute_and_commit_transactions_output: ExecuteAndCommitTransactionsOutput,
}

pub struct ExecuteAndCommitTransactionsOutput {
    // Transactions counts reported to `ConsumeWorkerMetrics` and then
    // accumulated later for `LeaderSlotMetrics`
    pub(crate) transaction_counts: LeaderProcessedTransactionCounts,
    // Transactions that either were not executed, or were executed and failed to be committed due
    // to the block ending.
    pub(crate) retryable_transaction_indexes: Vec<usize>,
    // A result that indicates whether transactions were successfully
    // committed into the Poh stream.
    pub commit_transactions_result: Result<Vec<CommitTransactionDetails>, PohRecorderError>,
    pub(crate) execute_and_commit_timings: LeaderExecuteAndCommitTimings,
    pub(crate) error_counters: TransactionErrorMetrics,
    pub(crate) min_prioritization_fees: u64,
    pub(crate) max_prioritization_fees: u64,
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
    qos_service: QosService,
    log_messages_bytes_limit: Option<usize>,
}

impl Consumer {
    pub fn new(
        committer: Committer,
        transaction_recorder: TransactionRecorder,
        qos_service: QosService,
        log_messages_bytes_limit: Option<usize>,
    ) -> Self {
        Self {
            committer,
            transaction_recorder,
            qos_service,
            log_messages_bytes_limit,
        }
    }

    pub fn process_and_record_transactions(
        &self,
        bank: &Arc<Bank>,
        txs: &[impl TransactionWithMeta],
        chunk_offset: usize,
    ) -> ProcessTransactionBatchOutput {
        let mut error_counters = TransactionErrorMetrics::default();
        let pre_results = vec![Ok(()); txs.len()];
        let check_results =
            bank.check_transactions(txs, &pre_results, MAX_PROCESSING_AGE, &mut error_counters);
        let check_results: Vec<_> = check_results
            .into_iter()
            .map(|result| match result {
                Ok(_) => Ok(()),
                Err(err) => Err(err),
            })
            .collect();
        let mut output = self.process_and_record_transactions_with_pre_results(
            bank,
            txs,
            chunk_offset,
            check_results.into_iter(),
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
        bank: &Arc<Bank>,
        txs: &[impl TransactionWithMeta],
        max_ages: &[MaxAge],
    ) -> ProcessTransactionBatchOutput {
        // Need to filter out transactions since they were sanitized earlier.
        // This means that the transaction may cross and epoch boundary (not allowed),
        //  or account lookup tables may have been closed.
        let pre_results = txs.iter().zip(max_ages).map(|(tx, max_age)| {
            // If the transaction was sanitized before this bank's epoch,
            // additional checks are necessary.
            if bank.epoch() != max_age.sanitized_epoch {
                // Reserved key set may have changed, so we must verify that
                // no writable keys are reserved.
                bank.check_reserved_keys(tx)?;
            }

            if bank.slot() > max_age.alt_invalidation_slot {
                // The address table lookup **may** have expired, but the
                // expiration is not guaranteed since there may have been
                // skipped slot.
                // If the addresses still resolve here, then the transaction is still
                // valid, and we can continue with processing.
                // If they do not, then the ATL has expired and the transaction
                // can be dropped.
                let (_addresses, _deactivation_slot) =
                    bank.load_addresses_from_ref(tx.message_address_table_lookups())?;
            }

            Ok(())
        });
        self.process_and_record_transactions_with_pre_results(bank, txs, 0, pre_results)
    }

    fn process_and_record_transactions_with_pre_results(
        &self,
        bank: &Arc<Bank>,
        txs: &[impl TransactionWithMeta],
        chunk_offset: usize,
        pre_results: impl Iterator<Item = Result<(), TransactionError>>,
    ) -> ProcessTransactionBatchOutput {
        let (
            (transaction_qos_cost_results, cost_model_throttled_transactions_count),
            cost_model_us,
        ) = measure_us!(self.qos_service.select_and_accumulate_transaction_costs(
            bank,
            txs,
            pre_results
        ));

        // Only lock accounts for those transactions are selected for the block;
        // Once accounts are locked, other threads cannot encode transactions that will modify the
        // same account state
        let (batch, lock_us) = measure_us!(bank.prepare_sanitized_batch_with_results(
            txs,
            transaction_qos_cost_results.iter().map(|r| match r {
                Ok(_cost) => Ok(()),
                Err(err) => Err(err.clone()),
            })
        ));

        // retryable_txs includes AccountInUse, WouldExceedMaxBlockCostLimit
        // WouldExceedMaxAccountCostLimit, WouldExceedMaxVoteCostLimit
        // and WouldExceedMaxAccountDataCostLimit
        let mut execute_and_commit_transactions_output =
            self.execute_and_commit_transactions_locked(bank, &batch);

        // Once the accounts are new transactions can enter the pipeline to process them
        let (_, unlock_us) = measure_us!(drop(batch));

        let ExecuteAndCommitTransactionsOutput {
            ref mut retryable_transaction_indexes,
            ref execute_and_commit_timings,
            ref commit_transactions_result,
            ..
        } = execute_and_commit_transactions_output;

        // Costs of all transactions are added to the cost_tracker before processing.
        // To ensure accurate tracking of compute units, transactions that ultimately
        // were not included in the block should have their cost removed, the rest
        // should update with their actually consumed units.
        QosService::remove_or_update_costs(
            transaction_qos_cost_results.iter(),
            commit_transactions_result.as_ref().ok(),
            bank,
        );

        retryable_transaction_indexes
            .iter_mut()
            .for_each(|x| *x += chunk_offset);

        let (cu, us) =
            Self::accumulate_execute_units_and_time(&execute_and_commit_timings.execute_timings);
        self.qos_service.accumulate_actual_execute_cu(cu);
        self.qos_service.accumulate_actual_execute_time(us);

        // reports qos service stats for this batch
        self.qos_service.report_metrics(bank.slot());

        debug!(
            "bank: {} lock: {}us unlock: {}us txs_len: {}",
            bank.slot(),
            lock_us,
            unlock_us,
            txs.len(),
        );

        ProcessTransactionBatchOutput {
            cost_model_throttled_transactions_count,
            cost_model_us,
            execute_and_commit_transactions_output,
        }
    }

    fn execute_and_commit_transactions_locked(
        &self,
        bank: &Arc<Bank>,
        batch: &TransactionBatch<impl TransactionWithMeta>,
    ) -> ExecuteAndCommitTransactionsOutput {
        let transaction_status_sender_enabled = self.committer.transaction_status_sender_enabled();
        let mut execute_and_commit_timings = LeaderExecuteAndCommitTimings::default();

        let mut pre_balance_info = PreBalanceInfo::default();
        let (_, collect_balances_us) = measure_us!({
            // If the extra meta-data services are enabled for RPC, collect the
            // pre-balances for native and token programs.
            if transaction_status_sender_enabled {
                pre_balance_info.native = bank.collect_balances(batch);
                pre_balance_info.token =
                    collect_token_balances(bank, batch, &mut pre_balance_info.mint_decimals)
            }
        });
        execute_and_commit_timings.collect_balances_us = collect_balances_us;

        let min_max = batch
            .sanitized_transactions()
            .iter()
            .filter_map(|transaction| {
                transaction
                    .compute_budget_instruction_details()
                    .sanitize_and_convert_to_compute_budget_limits(&bank.feature_set)
                    .ok()
                    .map(|limits| limits.compute_unit_price)
            })
            .minmax();
        let (min_prioritization_fees, max_prioritization_fees) =
            min_max.into_option().unwrap_or_default();

        let mut error_counters = TransactionErrorMetrics::default();
        let mut retryable_transaction_indexes: Vec<_> = batch
            .lock_results()
            .iter()
            .enumerate()
            .filter_map(|(index, res)| match res {
                // following are retryable errors
                Err(TransactionError::AccountInUse) => {
                    error_counters.account_in_use += 1;
                    Some(index)
                }
                Err(TransactionError::WouldExceedMaxBlockCostLimit) => {
                    error_counters.would_exceed_max_block_cost_limit += 1;
                    Some(index)
                }
                Err(TransactionError::WouldExceedMaxVoteCostLimit) => {
                    error_counters.would_exceed_max_vote_cost_limit += 1;
                    Some(index)
                }
                Err(TransactionError::WouldExceedMaxAccountCostLimit) => {
                    error_counters.would_exceed_max_account_cost_limit += 1;
                    Some(index)
                }
                Err(TransactionError::WouldExceedAccountDataBlockLimit) => {
                    error_counters.would_exceed_account_data_block_limit += 1;
                    Some(index)
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

        let (load_and_execute_transactions_output, load_execute_us) = measure_us!(bank
            .load_and_execute_transactions(
                batch,
                MAX_PROCESSING_AGE,
                &mut execute_and_commit_timings.execute_timings,
                &mut error_counters,
                TransactionProcessingConfig {
                    account_overrides: None,
                    check_program_modification_slot: bank.check_program_modification_slot(),
                    log_messages_bytes_limit: self.log_messages_bytes_limit,
                    limit_to_load_programs: true,
                    recording_config: ExecutionRecordingConfig::new_single_setting(
                        transaction_status_sender_enabled
                    ),
                }
            ));
        execute_and_commit_timings.load_execute_us = load_execute_us;

        let LoadAndExecuteTransactionsOutput {
            processing_results,
            processed_counts,
        } = load_and_execute_transactions_output;

        let transaction_counts = LeaderProcessedTransactionCounts {
            processed_count: processed_counts.processed_transactions_count,
            processed_with_successful_result_count: processed_counts
                .processed_with_successful_result_count,
            attempted_processing_count: processing_results.len() as u64,
        };

        let (processed_transactions, processing_results_to_transactions_us) =
            measure_us!(processing_results
                .iter()
                .zip(batch.sanitized_transactions())
                .filter_map(|(processing_result, tx)| {
                    if processing_result.was_processed() {
                        Some(tx.to_versioned_transaction())
                    } else {
                        None
                    }
                })
                .collect_vec());

        let (freeze_lock, freeze_lock_us) = measure_us!(bank.freeze_lock());
        execute_and_commit_timings.freeze_lock_us = freeze_lock_us;

        let (record_transactions_summary, record_us) = measure_us!(self
            .transaction_recorder
            .record_transactions(bank.slot(), processed_transactions));
        execute_and_commit_timings.record_us = record_us;

        let RecordTransactionsSummary {
            result: record_transactions_result,
            record_transactions_timings,
            starting_transaction_index,
        } = record_transactions_summary;
        execute_and_commit_timings.record_transactions_timings = RecordTransactionsTimings {
            processing_results_to_transactions_us: Saturating(
                processing_results_to_transactions_us,
            ),
            ..record_transactions_timings
        };

        if let Err(recorder_err) = record_transactions_result {
            retryable_transaction_indexes.extend(processing_results.iter().enumerate().filter_map(
                |(index, processing_result)| processing_result.was_processed().then_some(index),
            ));

            return ExecuteAndCommitTransactionsOutput {
                transaction_counts,
                retryable_transaction_indexes,
                commit_transactions_result: Err(recorder_err),
                execute_and_commit_timings,
                error_counters,
                min_prioritization_fees,
                max_prioritization_fees,
            };
        }

        let (commit_time_us, commit_transaction_statuses) =
            if processed_counts.processed_transactions_count != 0 {
                self.committer.commit_transactions(
                    batch,
                    processing_results,
                    starting_transaction_index,
                    bank,
                    &mut pre_balance_info,
                    &mut execute_and_commit_timings,
                    &processed_counts,
                )
            } else {
                (
                    0,
                    vec![CommitTransactionDetails::NotCommitted; processing_results.len()],
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
            transaction_counts,
            retryable_transaction_indexes,
            commit_transactions_result: Ok(commit_transaction_statuses),
            execute_and_commit_timings,
            error_counters,
            min_prioritization_fees,
            max_prioritization_fees,
        }
    }

    pub fn check_fee_payer_unlocked(
        bank: &Bank,
        transaction: &impl TransactionWithMeta,
        error_counters: &mut TransactionErrorMetrics,
    ) -> Result<(), TransactionError> {
        let fee_payer = transaction.fee_payer();
        let fee_budget_limits = FeeBudgetLimits::from(
            transaction
                .compute_budget_instruction_details()
                .sanitize_and_convert_to_compute_budget_limits(&bank.feature_set)?,
        );
        let fee = solana_fee::calculate_fee(
            transaction,
            bank.get_lamports_per_signature() == 0,
            bank.fee_structure().lamports_per_signature,
            fee_budget_limits.prioritization_fee,
            FeeFeatures::from(bank.feature_set.as_ref()),
        );
        let (mut fee_payer_account, _slot) = bank
            .rc
            .accounts
            .accounts_db
            .load_with_fixed_root(&bank.ancestors, fee_payer)
            .ok_or(TransactionError::AccountNotFound)?;

        validate_fee_payer(
            fee_payer,
            &mut fee_payer_account,
            0,
            error_counters,
            bank.rent_collector(),
            fee,
        )
    }

    fn accumulate_execute_units_and_time(execute_timings: &ExecuteTimings) -> (u64, u64) {
        execute_timings.details.per_program_timings.values().fold(
            (0, 0),
            |(units, times), program_timings| {
                (
                    (Saturating(units)
                        + program_timings.accumulated_units
                        + program_timings.total_errored_units)
                        .0,
                    (Saturating(times) + program_timings.accumulated_us).0,
                )
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::banking_stage::tests::{
            create_slow_genesis_config, sanitize_transactions, simulate_poh,
        },
        agave_reserved_account_keys::ReservedAccountKeys,
        crossbeam_channel::{unbounded, Receiver},
        solana_cost_model::{cost_model::CostModel, transaction_cost::TransactionCost},
        solana_entry::entry::{next_entry, next_versioned_entry},
        solana_ledger::{
            blockstore::{entries_to_test_shreds, Blockstore},
            blockstore_processor::TransactionStatusSender,
            genesis_utils::{
                bootstrap_validator_stake_lamports, create_genesis_config_with_leader,
                GenesisConfigInfo,
            },
            get_tmp_ledger_path_auto_delete,
            leader_schedule_cache::LeaderScheduleCache,
        },
        solana_poh::poh_recorder::{PohRecorder, Record},
        solana_rpc::transaction_status_service::TransactionStatusService,
        solana_runtime::prioritization_fee_cache::PrioritizationFeeCache,
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_sdk::{
            account::AccountSharedData,
            account_utils::StateMut,
            address_lookup_table::{
                self,
                state::{AddressLookupTable, LookupTableMeta},
            },
            fee_calculator::FeeCalculator,
            hash::Hash,
            instruction::InstructionError,
            message::{
                v0::{self, MessageAddressTableLookup},
                MessageHeader, VersionedMessage,
            },
            nonce::{self, state::DurableNonce},
            nonce_account::verify_nonce_account,
            poh_config::PohConfig,
            pubkey::Pubkey,
            signature::Keypair,
            signer::Signer,
            system_program, system_transaction,
            transaction::{MessageHash, Transaction, VersionedTransaction},
        },
        solana_timings::ProgramTiming,
        solana_transaction_status::{TransactionStatusMeta, VersionedTransactionWithStatusMeta},
        std::{
            borrow::Cow,
            sync::{
                atomic::{AtomicBool, AtomicU64, Ordering},
                RwLock,
            },
            thread::{Builder, JoinHandle},
            time::Duration,
        },
    };

    fn execute_transactions_with_dummy_poh_service(
        bank: Arc<Bank>,
        transactions: Vec<Transaction>,
    ) -> ProcessTransactionBatchOutput {
        let transactions = sanitize_transactions(transactions);
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let (poh_recorder, _entry_receiver) = PohRecorder::new(
            bank.tick_height(),
            bank.last_blockhash(),
            bank.clone(),
            Some((4, 4)),
            bank.ticks_per_slot(),
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );
        let (record_sender, record_receiver) = unbounded();
        let recorder = TransactionRecorder::new(record_sender, poh_recorder.is_exited.clone());
        let poh_recorder = Arc::new(RwLock::new(poh_recorder));

        poh_recorder
            .write()
            .unwrap()
            .set_bank_for_test(bank.clone());

        let poh_simulator = simulate_poh(record_receiver, &poh_recorder);

        let (replay_vote_sender, _replay_vote_receiver) = unbounded();
        let committer = Committer::new(
            None,
            replay_vote_sender,
            Arc::new(PrioritizationFeeCache::new(0u64)),
        );
        let consumer = Consumer::new(committer, recorder, QosService::new(1), None);
        let process_transactions_summary =
            consumer.process_and_record_transactions(&bank, &transactions, 0);

        poh_recorder
            .read()
            .unwrap()
            .is_exited
            .store(true, Ordering::Relaxed);
        let _ = poh_simulator.join();

        process_transactions_summary
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
        nonce_state: nonce::State,
    ) -> AccountSharedData {
        let mut account = AccountSharedData::new(1, nonce::State::size(), &system_program::id());
        account
            .set_state(&nonce::state::Versions::new(nonce_state))
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
        solana_logger::setup();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config_with_leader(
            10_000,
            &Pubkey::new_unique(),
            bootstrap_validator_stake_lamports(),
        );
        let (bank, _bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
        let pubkey = solana_pubkey::new_rand();

        let transactions = sanitize_transactions(vec![system_transaction::transfer(
            &mint_keypair,
            &pubkey,
            1,
            genesis_config.hash(),
        )]);

        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let (poh_recorder, entry_receiver) = PohRecorder::new(
            bank.tick_height(),
            bank.last_blockhash(),
            bank.clone(),
            Some((4, 4)),
            bank.ticks_per_slot(),
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );
        let (record_sender, record_receiver) = unbounded();
        let recorder = TransactionRecorder::new(record_sender, poh_recorder.is_exited.clone());
        let poh_recorder = Arc::new(RwLock::new(poh_recorder));

        let poh_simulator = simulate_poh(record_receiver, &poh_recorder);

        poh_recorder
            .write()
            .unwrap()
            .set_bank_for_test(bank.clone());
        let (replay_vote_sender, _replay_vote_receiver) = unbounded();
        let committer = Committer::new(
            None,
            replay_vote_sender,
            Arc::new(PrioritizationFeeCache::new(0u64)),
        );
        let consumer = Consumer::new(committer, recorder, QosService::new(1), None);

        let process_transactions_batch_output =
            consumer.process_and_record_transactions(&bank, &transactions, 0);

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

        // Tick up to max tick height
        while poh_recorder.read().unwrap().tick_height() != bank.max_tick_height() {
            poh_recorder.write().unwrap().tick();
        }

        let mut done = false;
        // read entries until I find mine, might be ticks...
        while let Ok((_bank, (entry, _tick_height))) = entry_receiver.recv() {
            if !entry.is_tick() {
                trace!("got entry");
                assert_eq!(entry.transactions.len(), transactions.len());
                assert_eq!(bank.get_balance(&pubkey), 1);
                done = true;
            }
            if done {
                break;
            }
        }
        trace!("done ticking");

        assert!(done);

        let transactions = sanitize_transactions(vec![system_transaction::transfer(
            &mint_keypair,
            &pubkey,
            2,
            genesis_config.hash(),
        )]);

        let process_transactions_batch_output =
            consumer.process_and_record_transactions(&bank, &transactions, 0);

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
        assert_eq!(retryable_transaction_indexes, vec![0]);
        assert_matches!(
            commit_transactions_result,
            Err(PohRecorderError::MaxHeightReached)
        );

        poh_recorder
            .read()
            .unwrap()
            .is_exited
            .store(true, Ordering::Relaxed);
        let _ = poh_simulator.join();

        assert_eq!(bank.get_balance(&pubkey), 1);
    }

    #[test]
    fn test_bank_nonce_update_blockhash_queried_before_transaction_record() {
        solana_logger::setup();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config_with_leader(
            10_000,
            &Pubkey::new_unique(),
            bootstrap_validator_stake_lamports(),
        );
        let (bank, _bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
        let pubkey = Pubkey::new_unique();

        // setup nonce account with a durable nonce different from the current
        // bank so that it can be advanced in this bank
        let durable_nonce = DurableNonce::from_blockhash(&Hash::new_unique());
        let nonce_hash = *durable_nonce.as_hash();
        let nonce_pubkey = Pubkey::new_unique();
        let nonce_state = nonce::State::Initialized(nonce::state::Data {
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

        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let (poh_recorder, entry_receiver) = PohRecorder::new(
            bank.tick_height(),
            bank.last_blockhash(),
            bank.clone(),
            Some((4, 4)),
            bank.ticks_per_slot(),
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
            &PohConfig::default(),
            Arc::new(AtomicBool::new(false)),
        );
        let (record_sender, record_receiver) = unbounded();
        let recorder = TransactionRecorder::new(record_sender, poh_recorder.is_exited.clone());
        let poh_recorder = Arc::new(RwLock::new(poh_recorder));

        fn poh_tick_before_returning_record_response(
            record_receiver: Receiver<Record>,
            poh_recorder: Arc<RwLock<PohRecorder>>,
        ) -> JoinHandle<()> {
            let is_exited = poh_recorder.read().unwrap().is_exited.clone();
            let tick_producer = Builder::new()
                .name("solana-simulate_poh".to_string())
                .spawn(move || loop {
                    let timeout = Duration::from_millis(10);
                    let record = record_receiver.recv_timeout(timeout);
                    if let Ok(record) = record {
                        let record_response = poh_recorder.write().unwrap().record(
                            record.slot,
                            record.mixin,
                            record.transactions,
                        );
                        poh_recorder.write().unwrap().tick();
                        if record.sender.send(record_response).is_err() {
                            panic!("Error returning mixin hash");
                        }
                    }
                    if is_exited.load(Ordering::Relaxed) {
                        break;
                    }
                });
            tick_producer.unwrap()
        }

        // Simulate a race condition by setting up poh to do the last tick
        // right before returning the transaction record response so that
        // bank blockhash queue is updated before transactions are
        // committed.
        let poh_simulator =
            poh_tick_before_returning_record_response(record_receiver, poh_recorder.clone());

        poh_recorder
            .write()
            .unwrap()
            .set_bank_for_test(bank.clone());

        // Tick up to max tick height - 1 so that only one tick remains
        // before recording transactions to poh
        while poh_recorder.read().unwrap().tick_height() != bank.max_tick_height() - 1 {
            poh_recorder.write().unwrap().tick();
        }

        let (replay_vote_sender, _replay_vote_receiver) = unbounded();
        let committer = Committer::new(
            None,
            replay_vote_sender,
            Arc::new(PrioritizationFeeCache::new(0u64)),
        );
        let consumer = Consumer::new(committer, recorder, QosService::new(1), None);

        let process_transactions_batch_output =
            consumer.process_and_record_transactions(&bank, &transactions, 0);
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

        // Ensure that poh did the last tick after recording transactions
        assert_eq!(
            poh_recorder.read().unwrap().tick_height(),
            bank.max_tick_height()
        );

        let mut done = false;
        // read entries until I find mine, might be ticks...
        while let Ok((_bank, (entry, _tick_height))) = entry_receiver.recv() {
            if !entry.is_tick() {
                assert_eq!(entry.transactions.len(), transactions.len());
                done = true;
                break;
            }
        }
        assert!(done);

        poh_recorder
            .read()
            .unwrap()
            .is_exited
            .store(true, Ordering::Relaxed);
        let _ = poh_simulator.join();

        // check that the nonce was advanced to the current bank's last blockhash
        // rather than the current bank's blockhash as would occur had the update
        // blockhash been queried _after_ transaction recording
        let expected_nonce = DurableNonce::from_blockhash(&genesis_config.hash());
        let expected_nonce_hash = expected_nonce.as_hash();
        let nonce_account = bank.get_account(&nonce_pubkey).unwrap();
        assert!(verify_nonce_account(&nonce_account, expected_nonce_hash).is_some());
    }

    #[test]
    fn test_bank_process_and_record_transactions_all_unexecuted() {
        solana_logger::setup();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(10_000);
        let (bank, _bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
        let pubkey = solana_pubkey::new_rand();

        let transactions = {
            let mut tx =
                system_transaction::transfer(&mint_keypair, &pubkey, 1, genesis_config.hash());
            // Add duplicate account key
            tx.message.account_keys.push(pubkey);
            sanitize_transactions(vec![tx])
        };

        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let (poh_recorder, _entry_receiver) = PohRecorder::new(
            bank.tick_height(),
            bank.last_blockhash(),
            bank.clone(),
            Some((4, 4)),
            bank.ticks_per_slot(),
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );
        let (record_sender, record_receiver) = unbounded();
        let recorder = TransactionRecorder::new(record_sender, poh_recorder.is_exited.clone());
        let poh_recorder = Arc::new(RwLock::new(poh_recorder));

        let poh_simulator = simulate_poh(record_receiver, &poh_recorder);

        poh_recorder
            .write()
            .unwrap()
            .set_bank_for_test(bank.clone());
        let (replay_vote_sender, _replay_vote_receiver) = unbounded();
        let committer = Committer::new(
            None,
            replay_vote_sender,
            Arc::new(PrioritizationFeeCache::new(0u64)),
        );
        let consumer = Consumer::new(committer, recorder, QosService::new(1), None);

        let process_transactions_batch_output =
            consumer.process_and_record_transactions(&bank, &transactions, 0);

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
            Some(vec![CommitTransactionDetails::NotCommitted; 1])
        );

        poh_recorder
            .read()
            .unwrap()
            .is_exited
            .store(true, Ordering::Relaxed);
        let _ = poh_simulator.join();
    }

    #[test]
    fn test_bank_process_and_record_transactions_cost_tracker() {
        solana_logger::setup();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(10_000);
        let mut bank = Bank::new_for_tests(&genesis_config);
        bank.ns_per_slot = u128::MAX;
        let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();
        let pubkey = solana_pubkey::new_rand();

        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let (poh_recorder, _entry_receiver) = PohRecorder::new(
            bank.tick_height(),
            bank.last_blockhash(),
            bank.clone(),
            Some((4, 4)),
            bank.ticks_per_slot(),
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );
        let (record_sender, record_receiver) = unbounded();
        let recorder = TransactionRecorder::new(record_sender, poh_recorder.is_exited.clone());
        let poh_recorder = Arc::new(RwLock::new(poh_recorder));

        let poh_simulator = simulate_poh(record_receiver, &poh_recorder);

        poh_recorder
            .write()
            .unwrap()
            .set_bank_for_test(bank.clone());
        let (replay_vote_sender, _replay_vote_receiver) = unbounded();
        let committer = Committer::new(
            None,
            replay_vote_sender,
            Arc::new(PrioritizationFeeCache::new(0u64)),
        );
        let consumer = Consumer::new(committer, recorder, QosService::new(1), None);

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
            genesis_config.hash(),
        )]);

        let process_transactions_batch_output =
            consumer.process_and_record_transactions(&bank, &transactions, 0);

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
                genesis_config.hash(),
                100,
            ),
            // this one won't execute in process_and_record_transactions from shared account lock overlap
            system_transaction::transfer(&mint_keypair, &pubkey, 2, genesis_config.hash()),
        ]);

        let process_transactions_batch_output =
            consumer.process_and_record_transactions(&bank, &transactions, 0);

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
            Some(CommitTransactionDetails::NotCommitted)
        );
        assert_eq!(retryable_transaction_indexes, vec![1]);

        let expected_block_cost = {
            let (actual_programs_execution_cost, actual_loaded_accounts_data_size_cost) =
                match commit_transactions_result.first().unwrap() {
                    CommitTransactionDetails::Committed {
                        compute_units,
                        loaded_accounts_data_size,
                    } => (
                        *compute_units,
                        CostModel::calculate_loaded_accounts_data_size_cost(
                            *loaded_accounts_data_size,
                            &bank.feature_set,
                        ),
                    ),
                    CommitTransactionDetails::NotCommitted => {
                        unreachable!()
                    }
                };

            let mut cost = CostModel::calculate_cost(&transactions[0], &bank.feature_set);
            if let TransactionCost::Transaction(ref mut usage_cost) = cost {
                usage_cost.programs_execution_cost = actual_programs_execution_cost;
                usage_cost.loaded_accounts_data_size_cost = actual_loaded_accounts_data_size_cost;
            }

            block_cost + cost.sum()
        };

        assert_eq!(get_block_cost(), expected_block_cost);
        assert_eq!(get_tx_count(), 2);

        poh_recorder
            .read()
            .unwrap()
            .is_exited
            .store(true, Ordering::Relaxed);
        let _ = poh_simulator.join();
    }

    #[test]
    fn test_bank_process_and_record_transactions_account_in_use() {
        solana_logger::setup();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(10_000);
        let (bank, _bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
        let pubkey = solana_pubkey::new_rand();
        let pubkey1 = solana_pubkey::new_rand();

        let transactions = sanitize_transactions(vec![
            system_transaction::transfer(&mint_keypair, &pubkey, 1, genesis_config.hash()),
            system_transaction::transfer(&mint_keypair, &pubkey1, 1, genesis_config.hash()),
        ]);

        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let (poh_recorder, _entry_receiver) = PohRecorder::new(
            bank.tick_height(),
            bank.last_blockhash(),
            bank.clone(),
            Some((4, 4)),
            bank.ticks_per_slot(),
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );
        let (record_sender, record_receiver) = unbounded();
        let recorder = TransactionRecorder::new(record_sender, poh_recorder.is_exited.clone());
        let poh_recorder = Arc::new(RwLock::new(poh_recorder));

        poh_recorder
            .write()
            .unwrap()
            .set_bank_for_test(bank.clone());

        let poh_simulator = simulate_poh(record_receiver, &poh_recorder);

        let (replay_vote_sender, _replay_vote_receiver) = unbounded();
        let committer = Committer::new(
            None,
            replay_vote_sender,
            Arc::new(PrioritizationFeeCache::new(0u64)),
        );
        let consumer = Consumer::new(committer, recorder, QosService::new(1), None);

        let process_transactions_batch_output =
            consumer.process_and_record_transactions(&bank, &transactions, 0);

        poh_recorder
            .read()
            .unwrap()
            .is_exited
            .store(true, Ordering::Relaxed);
        let _ = poh_simulator.join();

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
        assert_eq!(retryable_transaction_indexes, vec![1]);
        assert!(commit_transactions_result.is_ok());
    }

    #[test]
    fn test_process_transactions_instruction_error() {
        solana_logger::setup();
        let lamports = 10_000;
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(lamports);
        let (bank, _bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
        // set cost tracker limits to MAX so it will not filter out TXs
        bank.write_cost_tracker()
            .unwrap()
            .set_limits(u64::MAX, u64::MAX, u64::MAX);

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
        } = execute_transactions_with_dummy_poh_service(bank, transactions);

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
            (1..transactions_len - 1).collect::<Vec<usize>>()
        );
    }

    #[test]
    fn test_process_transactions_account_in_use() {
        solana_logger::setup();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(10_000);
        let (bank, _bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
        // set cost tracker limits to MAX so it will not filter out TXs
        bank.write_cost_tracker()
            .unwrap()
            .set_limits(u64::MAX, u64::MAX, u64::MAX);

        // Make all repetitive transactions that conflict on the `mint_keypair`, so only 1 should be executed
        let transactions = vec![
            system_transaction::transfer(
                &mint_keypair,
                &Pubkey::new_unique(),
                1,
                genesis_config.hash()
            );
            TARGET_NUM_TRANSACTIONS_PER_BATCH
        ];

        let transactions_len = transactions.len();
        let ProcessTransactionBatchOutput {
            execute_and_commit_transactions_output,
            ..
        } = execute_transactions_with_dummy_poh_service(bank, transactions);

        // All the transactions should have been replayed, but only 2 committed (first and last)
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
            1
        );
        assert_eq!(
            execute_and_commit_transactions_output
                .transaction_counts
                .processed_with_successful_result_count,
            1
        );

        // Everything except first of the transactions failed and are retryable
        assert_eq!(
            execute_and_commit_transactions_output.retryable_transaction_indexes,
            (1..transactions_len).collect::<Vec<usize>>()
        );
    }

    #[test]
    fn test_process_transactions_returns_unprocessed_txs() {
        solana_logger::setup();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(10_000);
        let (bank, _bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);

        let pubkey = solana_pubkey::new_rand();

        let transactions = sanitize_transactions(vec![system_transaction::transfer(
            &mint_keypair,
            &pubkey,
            1,
            genesis_config.hash(),
        )]);

        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let (poh_recorder, _entry_receiver) = PohRecorder::new(
            bank.tick_height(),
            bank.last_blockhash(),
            bank.clone(),
            Some((4, 4)),
            bank.ticks_per_slot(),
            Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );

        // Poh Recorder has no working bank, so should throw MaxHeightReached error on
        // record
        let (record_sender, record_receiver) = unbounded();
        let recorder = TransactionRecorder::new(record_sender, poh_recorder.is_exited.clone());

        let poh_simulator = simulate_poh(record_receiver, &Arc::new(RwLock::new(poh_recorder)));

        let (replay_vote_sender, _replay_vote_receiver) = unbounded();
        let committer = Committer::new(
            None,
            replay_vote_sender,
            Arc::new(PrioritizationFeeCache::new(0u64)),
        );
        let consumer = Consumer::new(committer, recorder.clone(), QosService::new(1), None);

        let process_transactions_summary =
            consumer.process_and_record_transactions(&bank, &transactions, 0);

        let ProcessTransactionBatchOutput {
            mut execute_and_commit_transactions_output,
            ..
        } = process_transactions_summary;

        // Transaction is successfully processed, but not committed due to poh recording error.
        assert!(execute_and_commit_transactions_output
            .commit_transactions_result
            .is_err());
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
        let expected: Vec<usize> = (0..transactions.len()).collect();
        assert_eq!(
            execute_and_commit_transactions_output.retryable_transaction_indexes,
            expected
        );

        recorder.is_exited.store(true, Ordering::Relaxed);
        let _ = poh_simulator.join();
    }

    #[test]
    fn test_write_persist_transaction_status() {
        solana_logger::setup();
        let GenesisConfigInfo {
            mut genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(solana_sdk::native_token::sol_to_lamports(1000.0));
        genesis_config.rent.lamports_per_byte_year = 50;
        genesis_config.rent.exemption_threshold = 2.0;
        let (bank, _bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
        let pubkey = solana_pubkey::new_rand();
        let pubkey1 = solana_pubkey::new_rand();
        let keypair1 = Keypair::new();

        let rent_exempt_amount = bank.get_minimum_balance_for_rent_exemption(0);

        let success_tx = system_transaction::transfer(
            &mint_keypair,
            &pubkey,
            rent_exempt_amount,
            genesis_config.hash(),
        );
        let success_signature = success_tx.signatures[0];
        let entry_1 = next_entry(&genesis_config.hash(), 1, vec![success_tx.clone()]);
        let ix_error_tx = system_transaction::transfer(
            &keypair1,
            &pubkey1,
            2 * rent_exempt_amount,
            genesis_config.hash(),
        );
        let ix_error_signature = ix_error_tx.signatures[0];
        let entry_2 = next_entry(&entry_1.hash, 1, vec![ix_error_tx.clone()]);
        let entries = vec![entry_1, entry_2];

        let transactions = sanitize_transactions(vec![success_tx, ix_error_tx]);
        bank.transfer(rent_exempt_amount, &mint_keypair, &keypair1.pubkey())
            .unwrap();

        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let blockstore = Arc::new(blockstore);
        let (poh_recorder, _entry_receiver) = PohRecorder::new(
            bank.tick_height(),
            bank.last_blockhash(),
            bank.clone(),
            Some((4, 4)),
            bank.ticks_per_slot(),
            blockstore.clone(),
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );
        let (record_sender, record_receiver) = unbounded();
        let recorder = TransactionRecorder::new(record_sender, poh_recorder.is_exited.clone());
        let poh_recorder = Arc::new(RwLock::new(poh_recorder));

        let poh_simulator = simulate_poh(record_receiver, &poh_recorder);

        poh_recorder
            .write()
            .unwrap()
            .set_bank_for_test(bank.clone());

        let shreds = entries_to_test_shreds(
            &entries,
            bank.slot(),
            0,    // parent_slot
            true, // is_full_slot
            0,    // version
            true, // merkle_variant
        );
        blockstore.insert_shreds(shreds, None, false).unwrap();
        blockstore.set_roots(std::iter::once(&bank.slot())).unwrap();

        let (transaction_status_sender, transaction_status_receiver) = unbounded();
        let tss_exit = Arc::new(AtomicBool::new(false));
        let transaction_status_service = TransactionStatusService::new(
            transaction_status_receiver,
            Arc::new(AtomicU64::default()),
            true,
            None,
            blockstore.clone(),
            false,
            tss_exit.clone(),
        );

        let (replay_vote_sender, _replay_vote_receiver) = unbounded();
        let committer = Committer::new(
            Some(TransactionStatusSender {
                sender: transaction_status_sender,
            }),
            replay_vote_sender,
            Arc::new(PrioritizationFeeCache::new(0u64)),
        );
        let consumer = Consumer::new(committer, recorder, QosService::new(1), None);

        let _ = consumer.process_and_record_transactions(&bank, &transactions, 0);

        drop(consumer); // drop/disconnect transaction_status_sender

        transaction_status_service.quiesce_and_join_for_tests(tss_exit);

        let confirmed_block = blockstore.get_rooted_block(bank.slot(), false).unwrap();
        let actual_tx_results: Vec<_> = confirmed_block
            .transactions
            .into_iter()
            .map(|VersionedTransactionWithStatusMeta { transaction, meta }| {
                (transaction.signatures[0], meta.status)
            })
            .collect();
        let expected_tx_results = vec![
            (success_signature, Ok(())),
            (
                ix_error_signature,
                Err(TransactionError::InstructionError(
                    0,
                    InstructionError::Custom(1),
                )),
            ),
        ];
        assert_eq!(actual_tx_results, expected_tx_results);

        poh_recorder
            .read()
            .unwrap()
            .is_exited
            .store(true, Ordering::Relaxed);
        let _ = poh_simulator.join();
    }

    #[test]
    fn test_write_persist_loaded_addresses() {
        solana_logger::setup();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(10_000);
        let (bank, bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
        let keypair = Keypair::new();

        let address_table_key = Pubkey::new_unique();
        let address_table_state = generate_new_address_lookup_table(None, 2);
        store_address_lookup_table(&bank, address_table_key, address_table_state);

        let new_bank = Bank::new_from_parent(bank, &Pubkey::new_unique(), 2);
        let bank = bank_forks
            .write()
            .unwrap()
            .insert(new_bank)
            .clone_without_scheduler();
        let message = VersionedMessage::V0(v0::Message {
            header: MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 0,
            },
            recent_blockhash: genesis_config.hash(),
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
            tx.clone(),
            MessageHash::Compute,
            Some(false),
            bank.as_ref(),
            &ReservedAccountKeys::empty_key_set(),
        )
        .unwrap();

        let entry = next_versioned_entry(&genesis_config.hash(), 1, vec![tx]);
        let entries = vec![entry];

        bank.transfer(1, &mint_keypair, &keypair.pubkey()).unwrap();

        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let blockstore = Arc::new(blockstore);
        let (poh_recorder, _entry_receiver) = PohRecorder::new(
            bank.tick_height(),
            bank.last_blockhash(),
            bank.clone(),
            Some((4, 4)),
            bank.ticks_per_slot(),
            blockstore.clone(),
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
            &PohConfig::default(),
            Arc::new(AtomicBool::default()),
        );
        let (record_sender, record_receiver) = unbounded();
        let recorder = TransactionRecorder::new(record_sender, poh_recorder.is_exited.clone());
        let poh_recorder = Arc::new(RwLock::new(poh_recorder));

        let poh_simulator = simulate_poh(record_receiver, &poh_recorder);

        poh_recorder
            .write()
            .unwrap()
            .set_bank_for_test(bank.clone());

        let shreds = entries_to_test_shreds(
            &entries,
            bank.slot(),
            0,    // parent_slot
            true, // is_full_slot
            0,    // version
            true, // merkle_variant
        );
        blockstore.insert_shreds(shreds, None, false).unwrap();
        blockstore.set_roots(std::iter::once(&bank.slot())).unwrap();

        let (transaction_status_sender, transaction_status_receiver) = unbounded();
        let tss_exit = Arc::new(AtomicBool::new(false));
        let transaction_status_service = TransactionStatusService::new(
            transaction_status_receiver,
            Arc::new(AtomicU64::default()),
            true,
            None,
            blockstore.clone(),
            false,
            tss_exit.clone(),
        );

        let (replay_vote_sender, _replay_vote_receiver) = unbounded();
        let committer = Committer::new(
            Some(TransactionStatusSender {
                sender: transaction_status_sender,
            }),
            replay_vote_sender,
            Arc::new(PrioritizationFeeCache::new(0u64)),
        );
        let consumer = Consumer::new(committer, recorder, QosService::new(1), None);

        let _ = consumer.process_and_record_transactions(&bank, &[sanitized_tx.clone()], 0);

        drop(consumer); // drop/disconnect transaction_status_sender

        transaction_status_service.quiesce_and_join_for_tests(tss_exit);

        let mut confirmed_block = blockstore.get_rooted_block(bank.slot(), false).unwrap();
        assert_eq!(confirmed_block.transactions.len(), 1);

        let recorded_meta = confirmed_block.transactions.pop().unwrap().meta;
        assert_eq!(
            recorded_meta,
            TransactionStatusMeta {
                status: Ok(()),
                pre_balances: vec![1, 0, 0],
                post_balances: vec![1, 0, 0],
                pre_token_balances: Some(vec![]),
                post_token_balances: Some(vec![]),
                rewards: Some(vec![]),
                loaded_addresses: sanitized_tx.get_loaded_addresses(),
                compute_units_consumed: Some(0),
                ..TransactionStatusMeta::default()
            }
        );
        poh_recorder
            .read()
            .unwrap()
            .is_exited
            .store(true, Ordering::Relaxed);
        let _ = poh_simulator.join();
    }

    #[test]
    fn test_accumulate_execute_units_and_time() {
        let mut execute_timings = ExecuteTimings::default();
        let mut expected_units = 0;
        let mut expected_us = 0;

        for n in 0..10 {
            execute_timings.details.per_program_timings.insert(
                Pubkey::new_unique(),
                ProgramTiming {
                    accumulated_us: Saturating(n * 100),
                    accumulated_units: Saturating(n * 1000),
                    count: Saturating(n as u32),
                    errored_txs_compute_consumed: vec![],
                    total_errored_units: Saturating(0),
                },
            );
            expected_us += n * 100;
            expected_units += n * 1000;
        }

        let (units, us) = Consumer::accumulate_execute_units_and_time(&execute_timings);

        assert_eq!(expected_units, units);
        assert_eq!(expected_us, us);
    }
}
