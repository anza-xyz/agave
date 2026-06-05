use {
    super::{
        Bank, LoadAndExecuteTransactionsOutput, PreCommitResult, ProcessedTransactionCounts,
        TransactionBalances, TransactionLogCollectorConfig, TransactionLogCollectorFilter,
        TransactionLogInfo, TransactionSimulationResult,
    },
    crate::{
        account_saver::collect_accounts_to_store,
        transaction_batch::{OwnedOrBorrowed, TransactionBatch},
    },
    ahash::AHashSet,
    log::{debug, info},
    solana_account::{ReadableAccount, from_account},
    solana_accounts_db::{account_locks::validate_account_locks, ancestors::Ancestors},
    solana_clock::{Epoch, MAX_TRANSACTION_FORWARDING_DELAY, Slot},
    solana_fee_structure::FeeDetails,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_measure::measure_us,
    solana_message::{AccountKeys, VersionedMessage},
    solana_packet::PACKET_DATA_SIZE,
    solana_program_runtime::loaded_programs::ProgramRuntimeEnvironments,
    solana_pubkey::Pubkey,
    solana_runtime_transaction::{
        runtime_transaction::RuntimeTransaction, transaction_with_meta::TransactionWithMeta,
    },
    solana_signature::Signature,
    solana_slot_history::{Check, SlotHistory},
    solana_svm::{
        account_loader::LoadedTransaction,
        account_overrides::AccountOverrides,
        transaction_balances::BalanceCollector,
        transaction_commit_result::{CommittedTransaction, TransactionCommitResult},
        transaction_error_metrics::TransactionErrorMetrics,
        transaction_execution_result::{
            TransactionExecutionDetails, TransactionLoadedAccountsStats,
        },
        transaction_processing_result::{
            ProcessedTransaction, TransactionProcessingResult,
            TransactionProcessingResultExtensions,
        },
        transaction_processor::{
            ExecutionRecordingConfig, TransactionProcessingConfig, TransactionProcessingEnvironment,
        },
    },
    solana_svm_timings::{ExecuteTimingType, ExecuteTimings},
    solana_svm_transaction::svm_message::SVMMessage,
    solana_system_transaction as system_transaction, solana_sysvar as sysvar,
    solana_transaction::{
        Transaction, TransactionVerificationMode,
        sanitized::{MAX_TX_ACCOUNT_LOCKS, MessageHash, SanitizedTransaction},
        versioned::{TransactionVersion, VersionedTransaction},
    },
    solana_transaction_error::{TransactionError, TransactionResult as Result},
    solana_vote::vote_parser,
    std::{slice, sync::atomic::Ordering::Relaxed},
};
#[cfg(feature = "dev-context-only-utils")]
use {
    crate::bank_forks::BankForks,
    solana_account::WritableAccount,
    solana_nonce as nonce,
    solana_nonce_account::{SystemAccountKind, get_system_account_kind},
    solana_svm::transaction_processor::TransactionBatchProcessor,
};

impl Bank {
    /// Forget all signatures. Useful for benchmarking.
    #[cfg(feature = "dev-context-only-utils")]
    pub fn clear_signatures(&self) {
        self.status_cache.write().unwrap().clear();
    }

    pub fn clear_slot_signatures(&self, slot: Slot) {
        self.status_cache.write().unwrap().clear_slot_entries(slot);
    }

    fn update_transaction_statuses(
        &self,
        sanitized_txs: &[impl TransactionWithMeta],
        processing_results: &[TransactionProcessingResult],
    ) {
        let mut status_cache = self.status_cache.write().unwrap();
        assert_eq!(sanitized_txs.len(), processing_results.len());
        for (tx, processing_result) in sanitized_txs.iter().zip(processing_results) {
            if let Ok(processed_tx) = &processing_result {
                // Add the message hash to the status cache to ensure that this message
                // won't be processed again with a different signature.
                status_cache.insert(
                    tx.recent_blockhash(),
                    tx.message_hash(),
                    self.slot(),
                    processed_tx.status(),
                );
                // Add the transaction signature to the status cache so that transaction status
                // can be queried by transaction signature over RPC. In the future, this should
                // only be added for API nodes because voting validators don't need to do this.
                status_cache.insert(
                    tx.recent_blockhash(),
                    tx.signature(),
                    self.slot(),
                    processed_tx.status(),
                );
            }
        }
    }

    /// Get the max number of accounts that a transaction may lock in this block
    pub fn get_transaction_account_lock_limit(&self) -> usize {
        if let Some(transaction_account_lock_limit) = self.transaction_account_lock_limit {
            transaction_account_lock_limit
        } else if self.feature_set.snapshot().increase_tx_account_lock_limit {
            MAX_TX_ACCOUNT_LOCKS
        } else {
            64
        }
    }

    /// Prepare a transaction batch from a list of versioned transactions from
    /// an entry. Used for tests only.
    pub fn prepare_entry_batch(
        &self,
        txs: Vec<VersionedTransaction>,
    ) -> Result<TransactionBatch<'_, '_, RuntimeTransaction<SanitizedTransaction>>> {
        let enable_instruction_account_limit =
            self.feature_set.snapshot().limit_instruction_accounts;
        let sanitized_txs = txs
            .into_iter()
            .map(|tx| {
                RuntimeTransaction::try_create(
                    tx,
                    MessageHash::Compute,
                    None,
                    self,
                    self.get_reserved_account_keys(),
                    enable_instruction_account_limit,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(TransactionBatch::new(
            self.try_lock_accounts(&sanitized_txs),
            self,
            OwnedOrBorrowed::Owned(sanitized_txs),
        ))
    }

    /// Attempt to take locks on the accounts in a transaction batch
    pub fn try_lock_accounts(&self, txs: &[impl TransactionWithMeta]) -> Vec<Result<()>> {
        self.try_lock_accounts_with_results(txs, txs.iter().map(|_| Ok(())))
    }

    /// Attempt to take locks on the accounts in a transaction batch, and their cost
    /// limited packing status and duplicate transaction conflict status
    pub fn try_lock_accounts_with_results(
        &self,
        txs: &[impl TransactionWithMeta],
        tx_results: impl Iterator<Item = Result<()>>,
    ) -> Vec<Result<()>> {
        let tx_account_lock_limit = self.get_transaction_account_lock_limit();

        // we must fail transactions that duplicate a prior message hash
        let mut batch_message_hashes = AHashSet::with_capacity(txs.len());
        let tx_results = tx_results
            .enumerate()
            .map(|(i, tx_result)| match tx_result {
                Ok(()) => {
                    // `HashSet::insert()` returns `true` when the value does *not* already exist
                    if batch_message_hashes.insert(txs[i].message_hash()) {
                        Ok(())
                    } else {
                        Err(TransactionError::AlreadyProcessed)
                    }
                }
                Err(e) => Err(e),
            });

        self.rc
            .accounts
            .lock_accounts(txs.iter(), tx_results, tx_account_lock_limit)
    }

    /// Prepare a locked transaction batch from a list of sanitized transactions.
    pub fn prepare_sanitized_batch<'a, 'b, Tx: TransactionWithMeta>(
        &'a self,
        txs: &'b [Tx],
    ) -> TransactionBatch<'a, 'b, Tx> {
        self.prepare_sanitized_batch_with_results(txs, txs.iter().map(|_| Ok(())))
    }

    /// Prepare a locked transaction batch from a list of sanitized transactions, and their cost
    /// limited packing status
    pub fn prepare_sanitized_batch_with_results<'a, 'b, Tx: TransactionWithMeta>(
        &'a self,
        transactions: &'b [Tx],
        transaction_results: impl Iterator<Item = Result<()>>,
    ) -> TransactionBatch<'a, 'b, Tx> {
        // this lock_results could be: Ok, AccountInUse, WouldExceedBlockMaxLimit or WouldExceedAccountMaxLimit
        TransactionBatch::new(
            self.try_lock_accounts_with_results(transactions, transaction_results),
            self,
            OwnedOrBorrowed::Borrowed(transactions),
        )
    }

    /// Prepare a transaction batch from a single transaction without locking accounts
    pub fn prepare_unlocked_batch_from_single_tx<'a, Tx: SVMMessage>(
        &'a self,
        transaction: &'a Tx,
    ) -> TransactionBatch<'a, 'a, Tx> {
        let tx_account_lock_limit = self.get_transaction_account_lock_limit();
        let lock_result = validate_account_locks(transaction.account_keys(), tx_account_lock_limit);
        let mut batch = TransactionBatch::new(
            vec![lock_result],
            self,
            OwnedOrBorrowed::Borrowed(slice::from_ref(transaction)),
        );
        batch.set_needs_unlock(false);
        batch
    }

    /// Prepare a transaction batch from a single transaction after locking accounts
    pub fn prepare_locked_batch_from_single_tx<'a, Tx: TransactionWithMeta>(
        &'a self,
        transaction: &'a Tx,
    ) -> TransactionBatch<'a, 'a, Tx> {
        self.prepare_sanitized_batch(slice::from_ref(transaction))
    }

    pub fn resanitize_transaction_minimally(
        &self,
        transaction: &impl TransactionWithMeta,
        sanitized_epoch: Epoch,
        alt_invalidation_slot: Slot,
    ) -> Result<()> {
        if self.vote_only_bank() && !vote_parser::is_valid_vote_only_transaction(transaction) {
            return Err(TransactionError::SanitizeFailure);
        }

        // If the transaction was sanitized before this bank's epoch,
        // additional checks are necessary.
        if self.epoch() != sanitized_epoch {
            // Reserved key set may have changed, so we must verify that
            // no writable keys are reserved.
            self.check_reserved_keys(transaction)?;

            if self.feature_set.snapshot().limit_instruction_accounts {
                for instr in transaction.instructions_iter() {
                    if instr.accounts.len()
                        > solana_transaction_context::MAX_ACCOUNTS_PER_INSTRUCTION
                    {
                        return Err(solana_transaction_error::TransactionError::SanitizeFailure);
                    }
                }
            }
        }

        if self.slot() > alt_invalidation_slot {
            // The address table lookup **may** have expired, but the
            // expiration is not guaranteed since there may have been
            // skipped slot.
            // If the addresses still resolve here, then the transaction is still
            // valid, and we can continue with processing.
            // If they do not, then the ATL has expired and the transaction
            // can be dropped.
            let (_addresses, _deactivation_slot) =
                self.load_addresses_from_ref(transaction.message_address_table_lookups())?;
        }

        Ok(())
    }

    /// Run transactions against a frozen bank without committing the results
    pub fn simulate_transaction(
        &self,
        transaction: &impl TransactionWithMeta,
        enable_cpi_recording: bool,
    ) -> TransactionSimulationResult {
        assert!(self.is_frozen(), "simulation bank must be frozen");

        self.simulate_transaction_unchecked(transaction, enable_cpi_recording)
    }

    /// Run transactions against a bank without committing the results; does not check if the bank
    /// is frozen, enabling use in single-Bank test frameworks
    pub fn simulate_transaction_unchecked(
        &self,
        transaction: &impl TransactionWithMeta,
        enable_cpi_recording: bool,
    ) -> TransactionSimulationResult {
        let account_keys = transaction.account_keys();
        let number_of_accounts = account_keys.len();
        let account_overrides = self.get_account_overrides_for_simulation(&account_keys);
        let batch = self.prepare_unlocked_batch_from_single_tx(transaction);
        let mut timings = ExecuteTimings::default();

        let LoadAndExecuteTransactionsOutput {
            mut processing_results,
            balance_collector,
            ..
        } = self.load_and_execute_transactions(
            &batch,
            // After simulation, transactions will need to be forwarded to the leader
            // for processing. During forwarding, the transaction could expire if the
            // delay is not accounted for.
            self.max_processing_age()
                .saturating_sub(MAX_TRANSACTION_FORWARDING_DELAY),
            &mut timings,
            &mut TransactionErrorMetrics::default(),
            TransactionProcessingConfig {
                account_overrides: Some(&account_overrides),
                check_program_deployment_slot: self.check_program_deployment_slot,
                log_messages_bytes_limit: None,
                limit_to_load_programs: true,
                recording_config: ExecutionRecordingConfig {
                    enable_cpi_recording,
                    enable_log_recording: true,
                    enable_return_data_recording: true,
                    enable_transaction_balance_recording: true,
                },
                drop_on_failure: false,
                all_or_nothing: false,
            },
        );

        debug!("simulate_transaction: {timings:?}");

        let processing_result = processing_results
            .pop()
            .unwrap_or(Err(TransactionError::InvalidProgramForExecution));
        let (
            post_simulation_accounts,
            result,
            fee,
            logs,
            return_data,
            inner_instructions,
            units_consumed,
            loaded_accounts_data_size,
        ) = match processing_result {
            Ok(processed_tx) => {
                let executed_units = processed_tx.executed_units();
                let loaded_accounts_data_size = processed_tx.loaded_accounts_data_size();

                match processed_tx {
                    ProcessedTransaction::Executed(executed_tx) => {
                        let details = executed_tx.execution_details;
                        let post_simulation_accounts = executed_tx
                            .loaded_transaction
                            .accounts
                            .into_iter()
                            .take(number_of_accounts)
                            .collect::<Vec<_>>();
                        (
                            post_simulation_accounts,
                            details.status,
                            Some(executed_tx.loaded_transaction.fee_details.total_fee()),
                            details.log_messages,
                            details.return_data,
                            details.inner_instructions,
                            executed_units,
                            loaded_accounts_data_size,
                        )
                    }
                    ProcessedTransaction::FeesOnly(fees_only_tx) => (
                        vec![],
                        Err(fees_only_tx.load_error),
                        Some(fees_only_tx.fee_details.total_fee()),
                        None,
                        None,
                        None,
                        executed_units,
                        loaded_accounts_data_size,
                    ),
                }
            }
            Err(error) => (vec![], Err(error), None, None, None, None, 0, 0),
        };
        let logs = logs.unwrap_or_default();

        let (pre_balances, post_balances, pre_token_balances, post_token_balances) =
            match balance_collector {
                Some(balance_collector) => {
                    let (mut native_pre, mut native_post, mut token_pre, mut token_post) =
                        balance_collector.into_vecs();

                    (
                        native_pre.pop(),
                        native_post.pop(),
                        token_pre.pop(),
                        token_post.pop(),
                    )
                }
                None => (None, None, None, None),
            };

        TransactionSimulationResult {
            result,
            logs,
            post_simulation_accounts,
            units_consumed,
            loaded_accounts_data_size,
            return_data,
            inner_instructions,
            fee,
            pre_balances,
            post_balances,
            pre_token_balances,
            post_token_balances,
        }
    }

    fn get_account_overrides_for_simulation(&self, account_keys: &AccountKeys) -> AccountOverrides {
        let mut account_overrides = AccountOverrides::default();
        let slot_history_id = sysvar::slot_history::id();
        if account_keys.iter().any(|pubkey| *pubkey == slot_history_id) {
            let current_account = self.get_account_with_fixed_root(&slot_history_id);
            let slot_history = current_account
                .as_ref()
                .map(|account| from_account::<SlotHistory, _>(account).unwrap())
                .unwrap_or_default();
            if slot_history.check(self.slot()) == Check::Found {
                let ancestors = Ancestors::from(self.proper_ancestors().collect::<Vec<_>>());
                if let Some((account, _)) =
                    self.load_slow_with_fixed_root(&ancestors, &slot_history_id)
                {
                    account_overrides.set_slot_history(Some(account));
                }
            }
        }
        account_overrides
    }

    pub fn unlock_accounts<'a, Tx: SVMMessage + 'a>(
        &self,
        txs_and_results: impl Iterator<Item = (&'a Tx, &'a Result<()>)> + Clone,
    ) {
        self.rc.accounts.unlock_accounts(txs_and_results)
    }

    pub fn get_hash_age(&self, hash: &Hash) -> Option<u64> {
        self.blockhash_queue.read().unwrap().get_hash_age(hash)
    }

    pub fn is_hash_valid_for_age(&self, hash: &Hash, max_age: usize) -> bool {
        self.blockhash_queue
            .read()
            .unwrap()
            .is_hash_valid_for_age(hash, max_age)
    }

    pub fn collect_balances(
        &self,
        batch: &TransactionBatch<impl SVMMessage>,
    ) -> TransactionBalances {
        let mut balances: TransactionBalances = vec![];
        for transaction in batch.sanitized_transactions() {
            let mut transaction_balances: Vec<u64> = vec![];
            for account_key in transaction.account_keys().iter() {
                transaction_balances.push(self.get_balance(account_key));
            }
            balances.push(transaction_balances);
        }
        balances
    }

    pub fn load_and_execute_transactions(
        &self,
        batch: &TransactionBatch<impl TransactionWithMeta>,
        max_age: usize,
        timings: &mut ExecuteTimings,
        error_counters: &mut TransactionErrorMetrics,
        processing_config: TransactionProcessingConfig,
    ) -> LoadAndExecuteTransactionsOutput {
        let sanitized_txs = batch.sanitized_transactions();

        let (check_results, check_us) = measure_us!(self.check_transactions(
            sanitized_txs,
            batch.lock_results(),
            max_age,
            error_counters,
        ));
        timings.saturating_add_in_place(ExecuteTimingType::CheckUs, check_us);

        let (blockhash, blockhash_lamports_per_signature) =
            self.last_blockhash_and_lamports_per_signature();
        let effective_epoch_of_deployments =
            self.epoch_schedule().get_epoch(self.slot.saturating_add(
                solana_program_runtime::program_cache_entry::DELAY_VISIBILITY_SLOT_OFFSET,
            ));
        let processing_environment = TransactionProcessingEnvironment {
            blockhash,
            blockhash_lamports_per_signature,
            alpenglow_migration_succeeded: self.get_alpenglow_genesis_certificate().is_some(),
            epoch_total_stake: self.get_current_epoch_total_stake(),
            feature_set: self.feature_set.runtime_features(),
            program_runtime_environments: ProgramRuntimeEnvironments::new(
                self.transaction_processor
                    .program_runtime_environment
                    .clone(),
                self.transaction_processor
                    .program_runtime_environment_for_epoch(effective_epoch_of_deployments),
            ),
            rent: self.rent_collector.rent.clone(),
        };

        let sanitized_output = self
            .transaction_processor
            .load_and_execute_sanitized_transactions(
                self,
                sanitized_txs,
                check_results,
                &processing_environment,
                &processing_config,
            );

        // Accumulate the errors returned by the batch processor.
        error_counters.accumulate(&sanitized_output.error_metrics);

        // Accumulate the transaction batch execution timings.
        timings.accumulate(&sanitized_output.execute_timings);

        let ((), collect_logs_us) =
            measure_us!(self.collect_logs(sanitized_txs, &sanitized_output.processing_results));
        timings.saturating_add_in_place(ExecuteTimingType::CollectLogsUs, collect_logs_us);

        let mut processed_counts = ProcessedTransactionCounts::default();
        let err_count = &mut error_counters.total;

        for (processing_result, tx) in sanitized_output
            .processing_results
            .iter()
            .zip(sanitized_txs)
        {
            if let Some(debug_keys) = &self.transaction_debug_keys {
                for key in tx.account_keys().iter() {
                    if debug_keys.contains(key) {
                        let result = processing_result.flattened_result();
                        info!("slot: {} result: {:?} tx: {:?}", self.slot, result, tx);
                        break;
                    }
                }
            }

            if processing_result.was_processed() {
                // Signature count must be accumulated only if the transaction
                // is processed, otherwise a mismatched count between banking
                // and replay could occur
                processed_counts.signature_count +=
                    tx.signature_details().num_transaction_signatures();
                processed_counts.processed_transactions_count += 1;

                if !tx.is_simple_vote_transaction() {
                    processed_counts.processed_non_vote_transactions_count += 1;
                }
            }

            match processing_result.flattened_result() {
                Ok(()) => {
                    processed_counts.processed_with_successful_result_count += 1;
                }
                Err(err) => {
                    if err_count.0 == 0 {
                        debug!("tx error: {err:?} {tx:?}");
                    }
                    *err_count += 1;
                }
            }
        }

        LoadAndExecuteTransactionsOutput {
            processing_results: sanitized_output.processing_results,
            processed_counts,
            balance_collector: sanitized_output.balance_collector,
        }
    }

    fn collect_logs(
        &self,
        transactions: &[impl TransactionWithMeta],
        processing_results: &[TransactionProcessingResult],
    ) {
        let transaction_log_collector_config =
            self.transaction_log_collector_config.read().unwrap();
        if transaction_log_collector_config.filter == TransactionLogCollectorFilter::None {
            return;
        }

        let collected_logs: Vec<_> = processing_results
            .iter()
            .zip(transactions)
            .filter_map(|(processing_result, transaction)| {
                // Skip log collection for unprocessed transactions
                let processed_tx = processing_result.processed_transaction()?;
                // Skip log collection for unexecuted transactions
                let execution_details = processed_tx.execution_details()?;
                Self::collect_transaction_logs(
                    &transaction_log_collector_config,
                    transaction,
                    execution_details,
                )
            })
            .collect();

        if !collected_logs.is_empty() {
            let mut transaction_log_collector = self.transaction_log_collector.write().unwrap();
            for (log, filtered_mentioned_addresses) in collected_logs {
                let transaction_log_index = transaction_log_collector.logs.len();
                transaction_log_collector.logs.push(log);
                for key in filtered_mentioned_addresses.into_iter() {
                    transaction_log_collector
                        .mentioned_address_map
                        .entry(key)
                        .or_default()
                        .push(transaction_log_index);
                }
            }
        }
    }

    fn collect_transaction_logs(
        transaction_log_collector_config: &TransactionLogCollectorConfig,
        transaction: &impl TransactionWithMeta,
        execution_details: &TransactionExecutionDetails,
    ) -> Option<(TransactionLogInfo, Vec<Pubkey>)> {
        // Skip log collection if no log messages were recorded
        let log_messages = execution_details.log_messages.as_ref()?;

        let mut filtered_mentioned_addresses = Vec::new();
        if !transaction_log_collector_config
            .mentioned_addresses
            .is_empty()
        {
            for key in transaction.account_keys().iter() {
                if transaction_log_collector_config
                    .mentioned_addresses
                    .contains(key)
                {
                    filtered_mentioned_addresses.push(*key);
                }
            }
        }

        let is_vote = transaction.is_simple_vote_transaction();
        let store = match transaction_log_collector_config.filter {
            TransactionLogCollectorFilter::All => {
                !is_vote || !filtered_mentioned_addresses.is_empty()
            }
            TransactionLogCollectorFilter::AllWithVotes => true,
            TransactionLogCollectorFilter::None => false,
            TransactionLogCollectorFilter::OnlyMentionedAddresses => {
                !filtered_mentioned_addresses.is_empty()
            }
        };

        if store {
            Some((
                TransactionLogInfo {
                    signature: *transaction.signature(),
                    result: execution_details.status.clone(),
                    is_vote,
                    log_messages: log_messages.clone(),
                },
                filtered_mentioned_addresses,
            ))
        } else {
            None
        }
    }

    pub(super) fn filter_program_errors_and_collect_fee_details(
        &self,
        processing_results: &[TransactionProcessingResult],
    ) {
        let mut accumulated_fee_details = FeeDetails::default();

        processing_results.iter().for_each(|processing_result| {
            if let Ok(processed_tx) = processing_result {
                accumulated_fee_details.accumulate(&processed_tx.fee_details());
            }
        });

        self.collector_fee_details
            .write()
            .unwrap()
            .accumulate(&accumulated_fee_details);
    }

    pub fn commit_transactions(
        &self,
        sanitized_txs: &[impl TransactionWithMeta],
        processing_results: Vec<TransactionProcessingResult>,
        processed_counts: &ProcessedTransactionCounts,
        timings: &mut ExecuteTimings,
    ) -> Vec<TransactionCommitResult> {
        assert!(
            !self.freeze_started(),
            "commit_transactions() working on a bank that is already frozen or is undergoing \
             freezing!"
        );

        let ProcessedTransactionCounts {
            processed_transactions_count,
            processed_non_vote_transactions_count,
            processed_with_successful_result_count,
            signature_count,
        } = *processed_counts;

        self.increment_transaction_count(processed_transactions_count);
        self.increment_non_vote_transaction_count_since_restart(
            processed_non_vote_transactions_count,
        );
        self.increment_signature_count(signature_count);

        let processed_with_failure_result_count =
            processed_transactions_count.saturating_sub(processed_with_successful_result_count);
        self.transaction_error_count
            .fetch_add(processed_with_failure_result_count, Relaxed);

        if processed_transactions_count > 0 {
            self.is_delta.store(true, Relaxed);
            self.transaction_entries_count.fetch_add(1, Relaxed);
            self.transactions_per_entry_max
                .fetch_max(processed_transactions_count, Relaxed);
        }

        let ((), store_accounts_us) = measure_us!({
            // If geyser is present, we must collect `SanitizedTransaction`
            // references in order to comply with that interface - until it
            // is changed.
            let maybe_transaction_refs = self
                .accounts()
                .accounts_db
                .has_accounts_update_notifier()
                .then(|| {
                    sanitized_txs
                        .iter()
                        .map(|tx| tx.as_sanitized_transaction())
                        .collect::<Vec<_>>()
                });

            let (accounts_to_store, transactions) = collect_accounts_to_store(
                sanitized_txs,
                &maybe_transaction_refs,
                &processing_results,
            );

            let to_store = (self.slot(), accounts_to_store.as_slice());
            self.update_bank_hash_stats(&to_store);
            // See https://github.com/solana-labs/solana/pull/31455 for discussion
            // on *not* updating the index within a threadpool.
            self.rc
                .accounts
                .store_accounts_seq(to_store, transactions.as_deref(), &self.ancestors);
        });

        // Cached vote and stake accounts are synchronized with accounts-db
        // after each transaction.
        let ((), update_stakes_cache_us) =
            measure_us!(self.update_stakes_cache(sanitized_txs, &processing_results));

        let ((), update_executors_us) = measure_us!({
            let mut cache = None;
            for processing_result in &processing_results {
                if let Some(ProcessedTransaction::Executed(executed_tx)) =
                    processing_result.processed_transaction()
                {
                    let programs_modified_by_tx = &executed_tx.programs_modified_by_tx;
                    if executed_tx.was_successful() && !programs_modified_by_tx.is_empty() {
                        cache
                            .get_or_insert_with(|| {
                                self.transaction_processor
                                    .global_program_cache
                                    .write()
                                    .unwrap()
                            })
                            .merge(
                                &self.transaction_processor.program_runtime_environment,
                                self.slot,
                                programs_modified_by_tx,
                            );
                    }
                }
            }
        });

        let accounts_data_len_delta = processing_results
            .iter()
            .filter_map(|processing_result| processing_result.processed_transaction())
            .filter_map(|processed_tx| processed_tx.execution_details())
            .filter_map(|details| details.accounts_deltas.as_ref())
            .map(|deltas| {
                deltas
                    .accounts_resize_delta
                    .saturating_sub_unsigned(deltas.accounts_uninitialized_size)
            })
            .sum();
        self.update_accounts_data_size_delta_on_chain(accounts_data_len_delta);

        let ((), update_transaction_statuses_us) =
            measure_us!(self.update_transaction_statuses(sanitized_txs, &processing_results));

        self.filter_program_errors_and_collect_fee_details(&processing_results);

        timings.saturating_add_in_place(ExecuteTimingType::StoreUs, store_accounts_us);
        timings.saturating_add_in_place(
            ExecuteTimingType::UpdateStakesCacheUs,
            update_stakes_cache_us,
        );
        timings.saturating_add_in_place(ExecuteTimingType::UpdateExecutorsUs, update_executors_us);
        timings.saturating_add_in_place(
            ExecuteTimingType::UpdateTransactionStatuses,
            update_transaction_statuses_us,
        );

        Self::create_commit_results(processing_results)
    }

    fn create_commit_results(
        processing_results: Vec<TransactionProcessingResult>,
    ) -> Vec<TransactionCommitResult> {
        processing_results
            .into_iter()
            .map(|processing_result| {
                let processing_result = processing_result?;
                let executed_units = processing_result.executed_units();
                let loaded_accounts_data_size = processing_result.loaded_accounts_data_size();

                match processing_result {
                    ProcessedTransaction::Executed(executed_tx) => {
                        let successful = executed_tx.was_successful();
                        let execution_details = executed_tx.execution_details;
                        let LoadedTransaction {
                            accounts: loaded_accounts,
                            fee_details,
                            rollback_accounts,
                            ..
                        } = executed_tx.loaded_transaction;

                        // Rollback value is used for failure.
                        let fee_payer_post_balance = if successful {
                            loaded_accounts[0].1.lamports()
                        } else {
                            rollback_accounts.fee_payer().1.lamports()
                        };

                        Ok(CommittedTransaction {
                            status: execution_details.status,
                            log_messages: execution_details.log_messages,
                            inner_instructions: execution_details.inner_instructions,
                            return_data: execution_details.return_data,
                            executed_units,
                            fee_details,
                            loaded_account_stats: TransactionLoadedAccountsStats {
                                loaded_accounts_count: loaded_accounts.len(),
                                loaded_accounts_data_size,
                            },
                            fee_payer_post_balance,
                        })
                    }
                    ProcessedTransaction::FeesOnly(fees_only_tx) => Ok(CommittedTransaction {
                        status: Err(fees_only_tx.load_error),
                        log_messages: None,
                        inner_instructions: None,
                        return_data: None,
                        executed_units,
                        fee_details: fees_only_tx.fee_details,
                        loaded_account_stats: TransactionLoadedAccountsStats {
                            loaded_accounts_count: fees_only_tx.rollback_accounts.count(),
                            loaded_accounts_data_size,
                        },
                        fee_payer_post_balance: fees_only_tx
                            .rollback_accounts
                            .fee_payer()
                            .1
                            .lamports(),
                    }),
                }
            })
            .collect()
    }

    /// Process a batch of transactions.
    #[must_use]
    pub fn load_execute_and_commit_transactions(
        &self,
        batch: &TransactionBatch<impl TransactionWithMeta>,
        recording_config: ExecutionRecordingConfig,
        timings: &mut ExecuteTimings,
        log_messages_bytes_limit: Option<usize>,
    ) -> (Vec<TransactionCommitResult>, Option<BalanceCollector>) {
        self.do_load_execute_and_commit_transactions_with_pre_commit_callback(
            batch,
            recording_config,
            timings,
            log_messages_bytes_limit,
            None::<fn(&mut _, &_) -> _>,
        )
        .unwrap()
    }

    pub fn load_execute_and_commit_transactions_with_pre_commit_callback<'a>(
        &'a self,
        batch: &TransactionBatch<impl TransactionWithMeta>,
        recording_config: ExecutionRecordingConfig,
        timings: &mut ExecuteTimings,
        log_messages_bytes_limit: Option<usize>,
        pre_commit_callback: impl FnOnce(
            &mut ExecuteTimings,
            &[TransactionProcessingResult],
        ) -> PreCommitResult<'a>,
    ) -> Result<(Vec<TransactionCommitResult>, Option<BalanceCollector>)> {
        self.do_load_execute_and_commit_transactions_with_pre_commit_callback(
            batch,
            recording_config,
            timings,
            log_messages_bytes_limit,
            Some(pre_commit_callback),
        )
    }

    fn do_load_execute_and_commit_transactions_with_pre_commit_callback<'a>(
        &'a self,
        batch: &TransactionBatch<impl TransactionWithMeta>,
        recording_config: ExecutionRecordingConfig,
        timings: &mut ExecuteTimings,
        log_messages_bytes_limit: Option<usize>,
        pre_commit_callback: Option<
            impl FnOnce(&mut ExecuteTimings, &[TransactionProcessingResult]) -> PreCommitResult<'a>,
        >,
    ) -> Result<(Vec<TransactionCommitResult>, Option<BalanceCollector>)> {
        let LoadAndExecuteTransactionsOutput {
            processing_results,
            processed_counts,
            balance_collector,
        } = self.load_and_execute_transactions(
            batch,
            self.max_processing_age(),
            timings,
            &mut TransactionErrorMetrics::default(),
            TransactionProcessingConfig {
                account_overrides: None,
                check_program_deployment_slot: self.check_program_deployment_slot,
                log_messages_bytes_limit,
                limit_to_load_programs: false,
                recording_config,
                drop_on_failure: false,
                all_or_nothing: false,
            },
        );

        // pre_commit_callback could initiate an atomic operation (i.e. poh recording with block
        // producing unified scheduler). in that case, it returns Some(freeze_lock), which should
        // unlocked only after calling commit_transactions() immediately after calling the
        // callback.
        let freeze_lock = if let Some(pre_commit_callback) = pre_commit_callback {
            pre_commit_callback(timings, &processing_results)?
        } else {
            None
        };
        let commit_results = self.commit_transactions(
            batch.sanitized_transactions(),
            processing_results,
            &processed_counts,
            timings,
        );
        drop(freeze_lock);
        Ok((commit_results, balance_collector))
    }

    /// Process a Transaction. This is used for unit tests and simply calls the vector
    /// Bank::process_transactions method.
    pub fn process_transaction(&self, tx: &Transaction) -> Result<()> {
        self.try_process_transactions(std::iter::once(tx))?[0].clone()
    }

    /// Process a Transaction and store metadata. This is used for tests and the banks services. It
    /// replicates the vector Bank::process_transaction method with metadata recording enabled.
    pub fn process_transaction_with_metadata(
        &self,
        tx: impl Into<VersionedTransaction>,
    ) -> Result<CommittedTransaction> {
        let txs = vec![tx.into()];
        let batch = self.prepare_entry_batch(txs)?;

        let (mut commit_results, ..) = self.load_execute_and_commit_transactions(
            &batch,
            ExecutionRecordingConfig {
                enable_cpi_recording: false,
                enable_log_recording: true,
                enable_return_data_recording: true,
                enable_transaction_balance_recording: false,
            },
            &mut ExecuteTimings::default(),
            Some(1000 * 1000),
        );

        commit_results.remove(0)
    }

    /// Process multiple transaction in a single batch. This is used for benches and unit tests.
    /// Short circuits if any of the transactions do not pass sanitization checks.
    pub fn try_process_transactions<'a>(
        &self,
        txs: impl Iterator<Item = &'a Transaction>,
    ) -> Result<Vec<Result<()>>> {
        let txs = txs
            .map(|tx| VersionedTransaction::from(tx.clone()))
            .collect();
        self.try_process_entry_transactions(txs)
    }

    /// Process multiple transaction in a single batch. This is used for benches and unit tests.
    /// Short circuits if any of the transactions do not pass sanitization checks.
    pub fn try_process_entry_transactions(
        &self,
        txs: Vec<VersionedTransaction>,
    ) -> Result<Vec<Result<()>>> {
        let batch = self.prepare_entry_batch(txs)?;
        Ok(self.process_transaction_batch(&batch))
    }

    #[must_use]
    fn process_transaction_batch(
        &self,
        batch: &TransactionBatch<impl TransactionWithMeta>,
    ) -> Vec<Result<()>> {
        self.load_execute_and_commit_transactions(
            batch,
            ExecutionRecordingConfig::new_single_setting(false),
            &mut ExecuteTimings::default(),
            None,
        )
        .0
        .into_iter()
        .map(|commit_result| commit_result.and_then(|committed_tx| committed_tx.status))
        .collect()
    }

    /// Create, sign, and process a Transaction from `keypair` to `to` of
    /// `n` lamports where `blockhash` is the last Entry ID observed by the client.
    pub fn transfer(&self, n: u64, keypair: &Keypair, to: &Pubkey) -> Result<Signature> {
        let blockhash = self.last_blockhash();
        let tx = system_transaction::transfer(keypair, to, n, blockhash);
        let signature = tx.signatures[0];
        self.process_transaction(&tx).map(|_| signature)
    }

    pub fn get_signature_status_processed_since_parent(
        &self,
        signature: &Signature,
    ) -> Option<Result<()>> {
        if let Some((slot, status)) = self.get_signature_status_slot(signature) {
            if slot <= self.slot() {
                return Some(status);
            }
        }
        None
    }

    pub fn get_signature_status_with_blockhash(
        &self,
        signature: &Signature,
        blockhash: &Hash,
    ) -> Option<Result<()>> {
        let rcache = self.status_cache.read().unwrap();
        rcache
            .get_status(signature, blockhash, &self.ancestors)
            .map(|v| v.1)
    }

    pub fn get_committed_transaction_status_and_slot(
        &self,
        message_hash: &Hash,
        transaction_blockhash: &Hash,
    ) -> Option<(Slot, bool)> {
        let rcache = self.status_cache.read().unwrap();
        rcache
            .get_status(message_hash, transaction_blockhash, &self.ancestors)
            .map(|(slot, status)| (slot, status.is_ok()))
    }

    pub fn get_signature_status_slot(&self, signature: &Signature) -> Option<(Slot, Result<()>)> {
        let rcache = self.status_cache.read().unwrap();
        rcache.get_status_any_blockhash(signature, &self.ancestors)
    }

    pub fn get_signature_status(&self, signature: &Signature) -> Option<Result<()>> {
        self.get_signature_status_slot(signature).map(|v| v.1)
    }

    pub fn has_signature(&self, signature: &Signature) -> bool {
        self.get_signature_status_slot(signature).is_some()
    }

    /// Verify the transaction signatures, hash and other metadata.
    pub fn verify_transaction(
        &self,
        tx: VersionedTransaction,
        verification_mode: TransactionVerificationMode,
    ) -> Result<RuntimeTransaction<SanitizedTransaction>> {
        // Discard v1 transactions until feature gate is activated.
        if !self.feature_set.snapshot().enable_tx_v1
            && tx.version() == TransactionVersion::Number(1)
        {
            return Err(TransactionError::UnsupportedVersion);
        }

        let serialized_message = tx.message.serialize();
        self.verify_transaction_with_serialized_message(tx, &serialized_message, verification_mode)
    }

    /// Verify the transaction signatures, hash and other metadata, using the provided serialized
    /// message.
    ///
    /// Verifying a transaction requires the serialized message to calculate the message hash. Use
    /// this function if the message is already available. Note that the serialized message MUST
    /// correspond to the transaction's message.
    pub fn verify_transaction_with_serialized_message(
        &self,
        tx: VersionedTransaction,
        serialized_message: &[u8],
        verification_mode: TransactionVerificationMode,
    ) -> Result<RuntimeTransaction<SanitizedTransaction>> {
        // Discard v1 transactions until feature gate is activated.
        let enable_tx_v1 = self.feature_set.snapshot().enable_tx_v1;
        if !enable_tx_v1 && tx.version() == TransactionVersion::Number(1) {
            return Err(TransactionError::UnsupportedVersion);
        }
        let max_transaction_size = match tx.version() {
            TransactionVersion::Number(1) if enable_tx_v1 => {
                solana_message::v1::MAX_TRANSACTION_SIZE
            }
            _ => PACKET_DATA_SIZE,
        } as u64;

        let enable_instruction_account_limit =
            self.feature_set.snapshot().limit_instruction_accounts;

        // WARNING: Any pending features added here most likely must also be checked in
        //          `Bank::resanitize_transaction_minimally`.
        let sanitized_tx = {
            let size =
                wincode::serialized_size(&tx).map_err(|_| TransactionError::SanitizeFailure)?;
            if size > max_transaction_size {
                return Err(TransactionError::SanitizeFailure);
            }

            // SIMD-0160, check instruction limit before signature verification
            if tx.message.instructions().len()
                > solana_transaction_context::MAX_INSTRUCTION_TRACE_LENGTH
            {
                return Err(solana_transaction_error::TransactionError::SanitizeFailure);
            }

            let message_hash = if verification_mode == TransactionVerificationMode::FullVerification
            {
                tx.verify_and_hash_message()?
            } else {
                VersionedMessage::hash_raw_message(serialized_message)
            };

            RuntimeTransaction::try_create(
                tx,
                MessageHash::Precomputed(message_hash),
                None,
                self,
                self.get_reserved_account_keys(),
                enable_instruction_account_limit,
            )
        }?;

        Ok(sanitized_tx)
    }

    /// Checks if the transaction violates the bank's reserved keys.
    /// This needs to be checked upon epoch boundary crosses because the
    /// reserved key set may have changed since the initial sanitization.
    pub fn check_reserved_keys(&self, tx: &impl SVMMessage) -> Result<()> {
        // Check keys against the reserved set - these failures simply require us
        // to re-sanitize the transaction. We do not need to drop the transaction.
        let reserved_keys = self.get_reserved_account_keys();
        for (index, key) in tx.account_keys().iter().enumerate() {
            if tx.is_writable(index) && reserved_keys.contains(key) {
                return Err(TransactionError::ResanitizationNeeded);
            }
        }

        Ok(())
    }
}

#[cfg(feature = "dev-context-only-utils")]
impl Bank {
    /// Prepare a transaction batch from a list of legacy transactions. Used for tests only.
    pub fn prepare_batch_for_tests(
        &self,
        txs: Vec<Transaction>,
    ) -> TransactionBatch<'_, '_, RuntimeTransaction<SanitizedTransaction>> {
        let sanitized_txs = txs
            .into_iter()
            .map(RuntimeTransaction::from_transaction_for_tests)
            .collect::<Vec<_>>();
        TransactionBatch::new(
            self.try_lock_accounts(&sanitized_txs),
            self,
            OwnedOrBorrowed::Owned(sanitized_txs),
        )
    }

    /// Process multiple transaction in a single batch. This is used for benches and unit tests.
    ///
    /// # Panics
    ///
    /// Panics if any of the transactions do not pass sanitization checks.
    #[must_use]
    pub fn process_transactions<'a>(
        &self,
        txs: impl Iterator<Item = &'a Transaction>,
    ) -> Vec<Result<()>> {
        self.try_process_transactions(txs).unwrap()
    }

    /// Process entry transactions in a single batch. This is used for benches and unit tests.
    ///
    /// # Panics
    ///
    /// Panics if any of the transactions do not pass sanitization checks.
    #[must_use]
    pub fn process_entry_transactions(&self, txs: Vec<VersionedTransaction>) -> Vec<Result<()>> {
        self.try_process_entry_transactions(txs).unwrap()
    }

    pub fn get_transaction_processor(&self) -> &TransactionBatchProcessor<BankForks> {
        &self.transaction_processor
    }

    pub fn withdraw(&self, pubkey: &Pubkey, lamports: u64) -> Result<()> {
        match self.get_account_with_fixed_root(pubkey) {
            Some(mut account) => {
                let min_balance = match get_system_account_kind(&account) {
                    Some(SystemAccountKind::Nonce) => self
                        .rent_collector
                        .rent
                        .minimum_balance(nonce::state::State::size()),
                    _ => 0,
                };

                lamports
                    .checked_add(min_balance)
                    .filter(|required_balance| *required_balance <= account.lamports())
                    .ok_or(TransactionError::InsufficientFundsForFee)?;
                account
                    .checked_sub_lamports(lamports)
                    .map_err(|_| TransactionError::InsufficientFundsForFee)?;
                self.store_account(pubkey, &account);

                Ok(())
            }
            None => Err(TransactionError::AccountNotFound),
        }
    }
}
