use {
    crate::{
        account_loader::{
            load_transaction, update_rent_exempt_status_for_account, validate_fee_payer,
            AccountLoader, CheckedTransactionDetails, LoadedTransaction, TransactionCheckResult,
            TransactionLoadResult, ValidatedTransactionDetails,
        },
        account_overrides::AccountOverrides,
        message_processor::process_message,
        nonce_info::NonceInfo,
        program_loader::{get_program_modification_slot, load_program_with_pubkey},
        rollback_accounts::RollbackAccounts,
        transaction_account_state_info::TransactionAccountStateInfo,
        transaction_balances::{BalanceCollectionRoutines, BalanceCollector},
        transaction_error_metrics::TransactionErrorMetrics,
        transaction_execution_result::{ExecutedTransaction, TransactionExecutionDetails},
        transaction_processing_result::{ProcessedTransaction, TransactionProcessingResult},
    },
    log::debug,
    percentage::Percentage,
    solana_account::{state_traits::StateMut, AccountSharedData, ReadableAccount, PROGRAM_OWNERS},
    solana_clock::{Epoch, Slot},
    solana_hash::Hash,
    solana_instruction::TRANSACTION_LEVEL_STACK_HEIGHT,
    solana_message::{
        compiled_instruction::CompiledInstruction,
        inner_instruction::{InnerInstruction, InnerInstructionsList},
    },
    solana_nonce::{
        state::{DurableNonce, State as NonceState},
        versions::Versions as NonceVersions,
    },
    solana_program_runtime::{
        execution_budget::SVMTransactionExecutionCost,
        invoke_context::{EnvironmentConfig, InvokeContext},
        loaded_programs::{
            EpochBoundaryPreparation, ForkGraph, ProgramCache, ProgramCacheEntry,
            ProgramCacheForTxBatch, ProgramCacheMatchCriteria, ProgramRuntimeEnvironments,
        },
        sysvar_cache::SysvarCache,
    },
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_sdk_ids::system_program,
    solana_svm_callback::TransactionProcessingCallback,
    solana_svm_feature_set::SVMFeatureSet,
    solana_svm_log_collector::LogCollector,
    solana_svm_measure::{measure::Measure, measure_us},
    solana_svm_timings::{ExecuteTimingType, ExecuteTimings},
    solana_svm_transaction::{svm_message::SVMMessage, svm_transaction::SVMTransaction},
    solana_svm_type_overrides::sync::{atomic::Ordering, Arc, RwLock, RwLockReadGuard},
    solana_transaction_context::{ExecutionRecord, TransactionContext},
    solana_transaction_error::{TransactionError, TransactionResult},
    std::{
        collections::HashSet,
        fmt::{Debug, Formatter},
        rc::Rc,
    },
};
#[cfg(feature = "dev-context-only-utils")]
use {
    qualifier_attr::{field_qualifiers, qualifiers},
    solana_program_runtime::{
        loaded_programs::ProgramRuntimeEnvironment,
        solana_sbpf::{program::BuiltinProgram, vm::Config as VmConfig},
    },
    std::sync::Weak,
};

/// A list of log messages emitted during a transaction
pub type TransactionLogMessages = Vec<String>;

/// The output of the transaction batch processor's
/// `load_and_execute_sanitized_transactions` method.
pub struct LoadAndExecuteSanitizedTransactionsOutput {
    /// Error metrics for transactions that were processed.
    pub error_metrics: TransactionErrorMetrics,
    /// Timings for transaction batch execution.
    pub execute_timings: ExecuteTimings,
    /// Vector of results indicating whether a transaction was processed or
    /// could not be processed. Note processed transactions can still have a
    /// failure result meaning that the transaction will be rolled back.
    pub processing_results: Vec<TransactionProcessingResult>,
    /// Balances accumulated for TransactionStatusSender when
    /// transaction balance recording is enabled.
    pub balance_collector: Option<BalanceCollector>,
}

/// Configuration of the recording capabilities for transaction execution
#[derive(Copy, Clone, Default)]
pub struct ExecutionRecordingConfig {
    pub enable_cpi_recording: bool,
    pub enable_log_recording: bool,
    pub enable_return_data_recording: bool,
    pub enable_transaction_balance_recording: bool,
}

impl ExecutionRecordingConfig {
    pub fn new_single_setting(option: bool) -> Self {
        ExecutionRecordingConfig {
            enable_return_data_recording: option,
            enable_log_recording: option,
            enable_cpi_recording: option,
            enable_transaction_balance_recording: option,
        }
    }
}

/// Configurations for processing transactions.
#[derive(Default)]
pub struct TransactionProcessingConfig<'a> {
    /// Encapsulates overridden accounts, typically used for transaction
    /// simulation.
    pub account_overrides: Option<&'a AccountOverrides>,
    /// Whether or not to check a program's modification slot when replenishing
    /// a program cache instance.
    pub check_program_modification_slot: bool,
    /// The maximum number of bytes that log messages can consume.
    pub log_messages_bytes_limit: Option<usize>,
    /// Whether to limit the number of programs loaded for the transaction
    /// batch.
    pub limit_to_load_programs: bool,
    /// Recording capabilities for transaction execution.
    pub recording_config: ExecutionRecordingConfig,
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

/// Runtime environment for transaction batch processing.
#[derive(Default)]
pub struct TransactionProcessingEnvironment {
    /// The blockhash to use for the transaction batch.
    pub blockhash: Hash,
    /// Lamports per signature that corresponds to this blockhash.
    ///
    /// Note: This value is primarily used for nonce accounts. If set to zero,
    /// it will disable transaction fees. However, any non-zero value will not
    /// change transaction fees. For this reason, it is recommended to use the
    /// `fee_per_signature` field to adjust transaction fees.
    pub blockhash_lamports_per_signature: u64,
    /// The total stake for the current epoch.
    pub epoch_total_stake: u64,
    /// Runtime feature set to use for the transaction batch.
    pub feature_set: SVMFeatureSet,
    /// The current ProgramRuntimeEnvironments derived from the SVMFeatureSet.
    pub program_runtime_environments_for_execution: ProgramRuntimeEnvironments,
    /// Depending on the next slot this is either the current or the upcoming
    /// ProgramRuntimeEnvironments.
    pub program_runtime_environments_for_deployment: ProgramRuntimeEnvironments,
    /// Rent calculator to use for the transaction batch.
    pub rent: Rent,
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[cfg_attr(
    feature = "dev-context-only-utils",
    field_qualifiers(slot(pub), epoch(pub), sysvar_cache(pub))
)]
pub struct TransactionBatchProcessor<FG: ForkGraph> {
    /// Bank slot (i.e. block)
    slot: Slot,

    /// Bank epoch
    epoch: Epoch,

    /// SysvarCache is a collection of system variables that are
    /// accessible from on chain programs. It is passed to SVM from
    /// client code (e.g. Bank) and forwarded to process_message.
    sysvar_cache: RwLock<SysvarCache>,

    /// Anticipates the environments of the upcoming epoch
    pub epoch_boundary_preparation: Arc<RwLock<EpochBoundaryPreparation>>,

    /// Programs required for transaction batch processing
    pub global_program_cache: Arc<RwLock<ProgramCache<FG>>>,

    /// Environments of the current epoch
    pub environments: ProgramRuntimeEnvironments,

    /// Builtin program ids
    pub builtin_program_ids: RwLock<HashSet<Pubkey>>,

    execution_cost: SVMTransactionExecutionCost,
}

impl<FG: ForkGraph> Debug for TransactionBatchProcessor<FG> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransactionBatchProcessor")
            .field("slot", &self.slot)
            .field("epoch", &self.epoch)
            .field("sysvar_cache", &self.sysvar_cache)
            .field("global_program_cache", &self.global_program_cache)
            .finish()
    }
}

impl<FG: ForkGraph> Default for TransactionBatchProcessor<FG> {
    fn default() -> Self {
        Self {
            slot: Slot::default(),
            epoch: Epoch::default(),
            sysvar_cache: RwLock::<SysvarCache>::default(),
            epoch_boundary_preparation: Arc::new(RwLock::new(EpochBoundaryPreparation::default())),
            global_program_cache: Arc::new(RwLock::new(ProgramCache::new(Slot::default()))),
            environments: ProgramRuntimeEnvironments::default(),
            builtin_program_ids: RwLock::new(HashSet::new()),
            execution_cost: SVMTransactionExecutionCost::default(),
        }
    }
}

impl<FG: ForkGraph> TransactionBatchProcessor<FG> {
    /// Create a new, uninitialized `TransactionBatchProcessor`.
    ///
    /// In this context, uninitialized means that the `TransactionBatchProcessor`
    /// has been initialized with an empty program cache. The cache contains no
    /// programs (including builtins) and has not been configured with a valid
    /// fork graph.
    ///
    /// When using this method, it's advisable to call `set_fork_graph_in_program_cache`
    /// as well as `add_builtin` to configure the cache before using the processor.
    pub fn new_uninitialized(slot: Slot, epoch: Epoch) -> Self {
        let epoch_boundary_preparation =
            Arc::new(RwLock::new(EpochBoundaryPreparation::new(epoch)));
        Self {
            slot,
            epoch,
            epoch_boundary_preparation,
            global_program_cache: Arc::new(RwLock::new(ProgramCache::new(slot))),
            ..Self::default()
        }
    }

    /// Create a new `TransactionBatchProcessor`.
    ///
    /// The created processor's program cache is initialized with the provided
    /// fork graph and loaders. If any loaders are omitted, a default "empty"
    /// loader (no syscalls) will be used.
    ///
    /// The cache will still not contain any builtin programs. It's advisable to
    /// call `add_builtin` to add the required builtins before using the processor.
    #[cfg(feature = "dev-context-only-utils")]
    pub fn new(
        slot: Slot,
        epoch: Epoch,
        fork_graph: Weak<RwLock<FG>>,
        program_runtime_environment_v1: Option<ProgramRuntimeEnvironment>,
        program_runtime_environment_v2: Option<ProgramRuntimeEnvironment>,
    ) -> Self {
        let mut processor = Self::new_uninitialized(slot, epoch);
        processor
            .global_program_cache
            .write()
            .unwrap()
            .set_fork_graph(fork_graph);
        let empty_loader = || Arc::new(BuiltinProgram::new_loader(VmConfig::default()));
        processor
            .global_program_cache
            .write()
            .unwrap()
            .latest_root_slot = processor.slot;
        processor
            .epoch_boundary_preparation
            .write()
            .unwrap()
            .upcoming_epoch = processor.epoch;
        processor.environments.program_runtime_v1 =
            program_runtime_environment_v1.unwrap_or(empty_loader());
        processor.environments.program_runtime_v2 =
            program_runtime_environment_v2.unwrap_or(empty_loader());
        processor
    }

    /// Create a new `TransactionBatchProcessor` from the current instance, but
    /// with the provided slot and epoch.
    ///
    /// * Inherits the program cache and builtin program ids from the current
    ///   instance.
    /// * Resets the sysvar cache.
    pub fn new_from(&self, slot: Slot, epoch: Epoch) -> Self {
        Self {
            slot,
            epoch,
            sysvar_cache: RwLock::<SysvarCache>::default(),
            epoch_boundary_preparation: self.epoch_boundary_preparation.clone(),
            global_program_cache: self.global_program_cache.clone(),
            environments: self.environments.clone(),
            builtin_program_ids: RwLock::new(self.builtin_program_ids.read().unwrap().clone()),
            execution_cost: self.execution_cost,
        }
    }

    /// Sets the base execution cost for the transactions that this instance of transaction processor
    /// will execute.
    pub fn set_execution_cost(&mut self, cost: SVMTransactionExecutionCost) {
        self.execution_cost = cost;
    }

    /// Updates the environments when entering a new Epoch.
    pub fn set_environments(&mut self, new_environments: ProgramRuntimeEnvironments) {
        // First update the parts of the environments which changed
        if self.environments.program_runtime_v1 != new_environments.program_runtime_v1 {
            self.environments.program_runtime_v1 = new_environments.program_runtime_v1;
        }
        if self.environments.program_runtime_v2 != new_environments.program_runtime_v2 {
            self.environments.program_runtime_v2 = new_environments.program_runtime_v2;
        }
        // Then try to consolidate with the upcoming environments (to reuse their address)
        if let Some(upcoming_environments) = &self
            .epoch_boundary_preparation
            .read()
            .unwrap()
            .upcoming_environments
        {
            if self.environments.program_runtime_v1 == upcoming_environments.program_runtime_v1
                && !Arc::ptr_eq(
                    &self.environments.program_runtime_v1,
                    &upcoming_environments.program_runtime_v1,
                )
            {
                self.environments.program_runtime_v1 =
                    upcoming_environments.program_runtime_v1.clone();
            }
            if self.environments.program_runtime_v2 == upcoming_environments.program_runtime_v2
                && !Arc::ptr_eq(
                    &self.environments.program_runtime_v2,
                    &upcoming_environments.program_runtime_v2,
                )
            {
                self.environments.program_runtime_v2 =
                    upcoming_environments.program_runtime_v2.clone();
            }
        }
    }

    /// Returns the current environments depending on the given epoch
    /// Returns None if the call could result in a deadlock
    pub fn get_environments_for_epoch(&self, epoch: Epoch) -> ProgramRuntimeEnvironments {
        self.epoch_boundary_preparation
            .read()
            .unwrap()
            .get_upcoming_environments_for_epoch(epoch)
            .unwrap_or_else(|| self.environments.clone())
    }

    pub fn sysvar_cache(&self) -> RwLockReadGuard<'_, SysvarCache> {
        self.sysvar_cache.read().unwrap()
    }

    /// Main entrypoint to the SVM.
    pub fn load_and_execute_sanitized_transactions<CB: TransactionProcessingCallback>(
        &self,
        callbacks: &CB,
        sanitized_txs: &[impl SVMTransaction],
        check_results: Vec<TransactionCheckResult>,
        environment: &TransactionProcessingEnvironment,
        config: &TransactionProcessingConfig,
    ) -> LoadAndExecuteSanitizedTransactionsOutput {
        // If `check_results` does not have the same length as `sanitized_txs`,
        // transactions could be truncated as a result of `.iter().zip()` in
        // many of the below methods.
        // See <https://doc.rust-lang.org/std/iter/trait.Iterator.html#method.zip>.
        debug_assert_eq!(
            sanitized_txs.len(),
            check_results.len(),
            "Length of check_results does not match length of sanitized_txs"
        );

        // Initialize metrics.
        let mut error_metrics = TransactionErrorMetrics::default();
        let mut execute_timings = ExecuteTimings::default();
        let mut processing_results = Vec::with_capacity(sanitized_txs.len());

        // Determine a capacity for the internal account cache. This
        // over-allocates but avoids ever reallocating, and spares us from
        // deduplicating the account keys lists.
        let account_keys_in_batch = sanitized_txs.iter().map(|tx| tx.account_keys().len()).sum();

        // Create the account loader, which wraps all external account fetching.
        let mut account_loader = AccountLoader::new_with_loaded_accounts_capacity(
            config.account_overrides,
            callbacks,
            &environment.feature_set,
            account_keys_in_batch,
        );

        // Create the transaction balance collector if recording is enabled.
        let mut balance_collector = config
            .recording_config
            .enable_transaction_balance_recording
            .then(|| BalanceCollector::new_with_transaction_count(sanitized_txs.len()));

        // Create the batch-local program cache.
        let mut program_cache_for_tx_batch = ProgramCacheForTxBatch::new(self.slot);
        let builtins = self.builtin_program_ids.read().unwrap().clone();
        let ((), program_cache_us) = measure_us!({
            self.replenish_program_cache(
                &account_loader,
                &builtins,
                &environment.program_runtime_environments_for_execution,
                &mut program_cache_for_tx_batch,
                &mut execute_timings,
                config.check_program_modification_slot,
                config.limit_to_load_programs,
                false, // increment_usage_counter
            );
        });
        execute_timings
            .saturating_add_in_place(ExecuteTimingType::ProgramCacheUs, program_cache_us);

        if program_cache_for_tx_batch.hit_max_limit {
            return LoadAndExecuteSanitizedTransactionsOutput {
                error_metrics,
                execute_timings,
                processing_results: (0..sanitized_txs.len())
                    .map(|_| Err(TransactionError::ProgramCacheHitMaxLimit))
                    .collect(),
                // If we abort the batch and balance recording is enabled, no balances should be
                // collected. If this is a leader thread, no batch will be committed.
                balance_collector: None,
            };
        }

        let (mut load_us, mut execution_us): (u64, u64) = (0, 0);

        // Validate, execute, and collect results from each transaction in order.
        // With SIMD83, transactions must be executed in order, because transactions
        // in the same batch may modify the same accounts. Transaction order is
        // preserved within entries written to the ledger.
        for (tx, check_result) in sanitized_txs.iter().zip(check_results) {
            let (validate_result, validate_fees_us) =
                measure_us!(check_result.and_then(|tx_details| {
                    Self::validate_transaction_nonce_and_fee_payer(
                        &mut account_loader,
                        tx,
                        tx_details,
                        &environment.blockhash,
                        &environment.rent,
                        &mut error_metrics,
                    )
                }));
            execute_timings
                .saturating_add_in_place(ExecuteTimingType::ValidateFeesUs, validate_fees_us);

            let (load_result, single_load_us) = measure_us!(load_transaction(
                &mut account_loader,
                tx,
                validate_result,
                &mut error_metrics,
                &environment.rent,
            ));
            load_us = load_us.saturating_add(single_load_us);

            let ((), collect_balances_us) =
                measure_us!(balance_collector.collect_pre_balances(&mut account_loader, tx));
            execute_timings
                .saturating_add_in_place(ExecuteTimingType::CollectBalancesUs, collect_balances_us);

            let (processing_result, single_execution_us) = measure_us!(match load_result {
                TransactionLoadResult::NotLoaded(err) => Err(err),
                TransactionLoadResult::FeesOnly(fees_only_tx) => match config.drop_on_failure {
                    true => Err(fees_only_tx.load_error),
                    false => {
                        // Update loaded accounts cache with nonce and fee-payer
                        account_loader
                            .update_accounts_for_failed_tx(&fees_only_tx.rollback_accounts);

                        Ok(ProcessedTransaction::FeesOnly(Box::new(fees_only_tx)))
                    }
                },
                TransactionLoadResult::Loaded(loaded_transaction) => {
                    let (program_accounts_set, filter_executable_us) = measure_us!(self
                        .filter_executable_program_accounts(
                            &account_loader,
                            &mut program_cache_for_tx_batch,
                            tx,
                        ));
                    execute_timings.saturating_add_in_place(
                        ExecuteTimingType::FilterExecutableUs,
                        filter_executable_us,
                    );

                    let ((), program_cache_us) = measure_us!({
                        self.replenish_program_cache(
                            &account_loader,
                            &program_accounts_set,
                            &environment.program_runtime_environments_for_execution,
                            &mut program_cache_for_tx_batch,
                            &mut execute_timings,
                            config.check_program_modification_slot,
                            config.limit_to_load_programs,
                            true, // increment_usage_counter
                        );
                    });
                    execute_timings.saturating_add_in_place(
                        ExecuteTimingType::ProgramCacheUs,
                        program_cache_us,
                    );

                    if program_cache_for_tx_batch.hit_max_limit {
                        return LoadAndExecuteSanitizedTransactionsOutput {
                            error_metrics,
                            execute_timings,
                            processing_results: (0..sanitized_txs.len())
                                .map(|_| Err(TransactionError::ProgramCacheHitMaxLimit))
                                .collect(),
                            // If we abort the batch and balance recording is enabled, no balances should be
                            // collected. If this is a leader thread, no batch will be committed.
                            balance_collector: None,
                        };
                    }

                    let executed_tx = self.execute_loaded_transaction(
                        callbacks,
                        tx,
                        loaded_transaction,
                        &mut execute_timings,
                        &mut error_metrics,
                        &mut program_cache_for_tx_batch,
                        environment,
                        config,
                    );

                    match (
                        &executed_tx.execution_details.status,
                        config.drop_on_failure,
                    ) {
                        // Successful transactions need to update the account loader cache as future
                        // transactions in the batch may depend on them.
                        (Ok(_), _) => {
                            account_loader.update_accounts_for_successful_tx(
                                tx,
                                &executed_tx.loaded_transaction.accounts,
                            );
                            // Also update local program cache with modifications made by the
                            // transaction, if it executed successfully.
                            program_cache_for_tx_batch.merge(&executed_tx.programs_modified_by_tx);

                            Ok(ProcessedTransaction::Executed(Box::new(executed_tx)))
                        }
                        // If the transaction failed & drop on failure is set then we don't want to
                        // update the accounts as this transaction will be dropped from the batch.
                        (Err(err), true) => Err(err.clone()),
                        // Unsuccessful transactions will still update rollback accounts (fee payer,
                        // nonce, etc).
                        (Err(_), false) => {
                            account_loader.update_accounts_for_failed_tx(
                                &executed_tx.loaded_transaction.rollback_accounts,
                            );

                            Ok(ProcessedTransaction::Executed(Box::new(executed_tx)))
                        }
                    }
                }
            });
            execution_us = execution_us.saturating_add(single_execution_us);

            let ((), collect_balances_us) =
                measure_us!(balance_collector.collect_post_balances(&mut account_loader, tx));
            execute_timings
                .saturating_add_in_place(ExecuteTimingType::CollectBalancesUs, collect_balances_us);

            // If this is an all or nothing batch and we failed to process this transaction then we
            // must abort all prior/remaining transactions.
            if config.all_or_nothing && processing_result.is_err() {
                // Abort prior transactions.
                for res in processing_results.iter_mut() {
                    *res = Err(TransactionError::CommitCancelled);
                }

                // Preserve the failure that triggered the batch to abort.
                processing_results.push(processing_result);

                // Abort remaining transactions.
                processing_results.extend(
                    (0..sanitized_txs.len() - processing_results.len())
                        .map(|_| Err(TransactionError::CommitCancelled)),
                );

                return LoadAndExecuteSanitizedTransactionsOutput {
                    error_metrics,
                    execute_timings,
                    processing_results,
                    // If we abort the batch and balance recording is enabled, no balances should be
                    // collected. If this is a leader thread, no batch will be committed.
                    balance_collector: None,
                };
            }

            processing_results.push(processing_result);
        }

        // Skip eviction when there's no chance this particular tx batch has increased the size of
        // ProgramCache entries. Note that loaded_missing is deliberately defined, so that there's
        // still at least one other batch, which will evict the program cache, even after the
        // occurrences of cooperative loading.
        if program_cache_for_tx_batch.loaded_missing || program_cache_for_tx_batch.merged_modified {
            const SHRINK_LOADED_PROGRAMS_TO_PERCENTAGE: u8 = 90;
            self.global_program_cache
                .write()
                .unwrap()
                .evict_using_2s_random_selection(
                    Percentage::from(SHRINK_LOADED_PROGRAMS_TO_PERCENTAGE),
                    self.slot,
                );
        }

        debug!(
            "load: {}us execute: {}us txs_len={}",
            load_us,
            execution_us,
            sanitized_txs.len(),
        );
        execute_timings.saturating_add_in_place(ExecuteTimingType::LoadUs, load_us);
        execute_timings.saturating_add_in_place(ExecuteTimingType::ExecuteUs, execution_us);

        if let Some(ref balance_collector) = balance_collector {
            debug_assert!(balance_collector.lengths_match_expected(sanitized_txs.len()));
        }

        LoadAndExecuteSanitizedTransactionsOutput {
            error_metrics,
            execute_timings,
            processing_results,
            balance_collector,
        }
    }

    fn validate_transaction_nonce_and_fee_payer<CB: TransactionProcessingCallback>(
        account_loader: &mut AccountLoader<CB>,
        message: &impl SVMMessage,
        checked_details: CheckedTransactionDetails,
        environment_blockhash: &Hash,
        rent: &Rent,
        error_counters: &mut TransactionErrorMetrics,
    ) -> TransactionResult<ValidatedTransactionDetails> {
        // If this is a nonce transaction, validate the nonce info.
        // This must be done for every transaction to support SIMD83 because
        // it may have changed due to use, authorization, or deallocation.
        // This function is a successful no-op if given a blockhash transaction.
        if let CheckedTransactionDetails {
            nonce: Some(ref nonce_info),
            compute_budget_and_limits: _,
        } = checked_details
        {
            let next_durable_nonce = DurableNonce::from_blockhash(environment_blockhash);
            Self::validate_transaction_nonce(
                account_loader,
                message,
                nonce_info,
                &next_durable_nonce,
                error_counters,
            )?;
        }

        // Now validate the fee-payer for the transaction unconditionally.
        Self::validate_transaction_fee_payer(
            account_loader,
            message,
            checked_details,
            rent,
            error_counters,
        )
    }

    // Loads transaction fee payer, collects rent if necessary, then calculates
    // transaction fees, and deducts them from the fee payer balance. If the
    // account is not found or has insufficient funds, an error is returned.
    fn validate_transaction_fee_payer<CB: TransactionProcessingCallback>(
        account_loader: &mut AccountLoader<CB>,
        message: &impl SVMMessage,
        checked_details: CheckedTransactionDetails,
        rent: &Rent,
        error_counters: &mut TransactionErrorMetrics,
    ) -> TransactionResult<ValidatedTransactionDetails> {
        let CheckedTransactionDetails {
            nonce,
            compute_budget_and_limits,
        } = checked_details;

        let fee_payer_address = message.fee_payer();

        // We *must* use load_transaction_account() here because *this* is when the fee-payer
        // is loaded for the transaction. Transaction loading skips the first account and
        // loads (and thus inspects) all others normally.
        let Some(mut loaded_fee_payer) =
            account_loader.load_transaction_account(fee_payer_address, true)
        else {
            error_counters.account_not_found += 1;
            return Err(TransactionError::AccountNotFound);
        };

        let fee_payer_loaded_rent_epoch = loaded_fee_payer.account.rent_epoch();
        update_rent_exempt_status_for_account(rent, &mut loaded_fee_payer.account);

        let fee_payer_index = 0;
        validate_fee_payer(
            fee_payer_address,
            &mut loaded_fee_payer.account,
            fee_payer_index,
            error_counters,
            rent,
            compute_budget_and_limits.fee_details.total_fee(),
        )?;

        // Capture fee-subtracted fee payer account and next nonce account state
        // to commit if transaction execution fails.
        let rollback_accounts = RollbackAccounts::new(
            nonce,
            *fee_payer_address,
            loaded_fee_payer.account.clone(),
            fee_payer_loaded_rent_epoch,
        );

        Ok(ValidatedTransactionDetails {
            fee_details: compute_budget_and_limits.fee_details,
            rollback_accounts,
            loaded_accounts_bytes_limit: compute_budget_and_limits.loaded_accounts_data_size_limit,
            compute_budget: compute_budget_and_limits.budget,
            loaded_fee_payer_account: loaded_fee_payer,
        })
    }

    fn validate_transaction_nonce<CB: TransactionProcessingCallback>(
        account_loader: &mut AccountLoader<CB>,
        message: &impl SVMMessage,
        nonce_info: &NonceInfo,
        next_durable_nonce: &DurableNonce,
        error_counters: &mut TransactionErrorMetrics,
    ) -> TransactionResult<()> {
        // When SIMD83 is enabled, if the nonce has been used in this batch already, we must drop
        // the transaction. This is the same as if it was used in different batches in the same slot.
        // If the nonce account was closed in the batch, we error as if the blockhash didn't validate.
        // We must validate the account in case it was reopened, either as a normal system account,
        // or a fake nonce account. We must also check the signer in case the authority was changed.
        //
        // Note these checks are *not* obviated by fee-only transactions.
        let nonce_is_valid = account_loader
            .load_transaction_account(nonce_info.address(), true)
            .and_then(|ref current_nonce| {
                system_program::check_id(current_nonce.account.owner()).then_some(())?;
                StateMut::<NonceVersions>::state(&current_nonce.account).ok()
            })
            .and_then(
                |current_nonce_versions| match current_nonce_versions.state() {
                    NonceState::Initialized(current_nonce_data) => {
                        let nonce_can_be_advanced =
                            &current_nonce_data.durable_nonce != next_durable_nonce;

                        let nonce_authority_is_valid = message
                            .account_keys()
                            .iter()
                            .enumerate()
                            .any(|(i, address)| {
                                address == &current_nonce_data.authority && message.is_signer(i)
                            });

                        if nonce_authority_is_valid {
                            Some(nonce_can_be_advanced)
                        } else {
                            None
                        }
                    }
                    _ => None,
                },
            );

        match nonce_is_valid {
            None => {
                error_counters.blockhash_not_found += 1;
                Err(TransactionError::BlockhashNotFound)
            }
            Some(false) => {
                error_counters.account_not_found += 1;
                Err(TransactionError::AccountNotFound)
            }
            Some(true) => Ok(()),
        }
    }

    /// Appends to a set of executable program accounts (all accounts owned by any loader)
    /// for transactions with a valid blockhash or nonce.
    fn filter_executable_program_accounts<CB: TransactionProcessingCallback>(
        &self,
        account_loader: &AccountLoader<CB>,
        program_cache_for_tx_batch: &mut ProgramCacheForTxBatch,
        tx: &impl SVMMessage,
    ) -> HashSet<Pubkey> {
        let mut program_accounts_set = HashSet::default();
        for account_key in tx.account_keys().iter() {
            if let Some(cache_entry) = program_cache_for_tx_batch.find(account_key) {
                cache_entry.tx_usage_counter.fetch_add(1, Ordering::Relaxed);
            } else if account_loader
                .get_account_shared_data(account_key)
                .map(|(account, _slot)| PROGRAM_OWNERS.contains(account.owner()))
                .unwrap_or(false)
            {
                program_accounts_set.insert(*account_key);
            }
        }
        program_accounts_set
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    fn replenish_program_cache<CB: TransactionProcessingCallback>(
        &self,
        account_loader: &AccountLoader<CB>,
        program_accounts_set: &HashSet<Pubkey>,
        program_runtime_environments_for_execution: &ProgramRuntimeEnvironments,
        program_cache_for_tx_batch: &mut ProgramCacheForTxBatch,
        execute_timings: &mut ExecuteTimings,
        check_program_modification_slot: bool,
        limit_to_load_programs: bool,
        increment_usage_counter: bool,
    ) {
        let mut missing_programs: Vec<(Pubkey, ProgramCacheMatchCriteria)> = program_accounts_set
            .iter()
            .map(|pubkey| {
                let match_criteria = if check_program_modification_slot {
                    get_program_modification_slot(account_loader, pubkey)
                        .map_or(ProgramCacheMatchCriteria::Tombstone, |slot| {
                            ProgramCacheMatchCriteria::DeployedOnOrAfterSlot(slot)
                        })
                } else {
                    ProgramCacheMatchCriteria::NoCriteria
                };
                (*pubkey, match_criteria)
            })
            .collect();

        let mut count_hits_and_misses = true;
        loop {
            let (program_to_store, task_cookie, task_waiter) = {
                // Lock the global cache.
                let global_program_cache = self.global_program_cache.read().unwrap();
                // Figure out which program needs to be loaded next.
                let program_to_load = global_program_cache.extract(
                    &mut missing_programs,
                    program_cache_for_tx_batch,
                    program_runtime_environments_for_execution,
                    increment_usage_counter,
                    count_hits_and_misses,
                );
                count_hits_and_misses = false;

                let program_to_store = program_to_load.map(|key| {
                    // Load, verify and compile one program.
                    let program = load_program_with_pubkey(
                        account_loader,
                        program_runtime_environments_for_execution,
                        &key,
                        self.slot,
                        execute_timings,
                        false,
                    )
                    .expect("called load_program_with_pubkey() with nonexistent account");
                    (key, program)
                });

                let task_waiter = Arc::clone(&global_program_cache.loading_task_waiter);
                (program_to_store, task_waiter.cookie(), task_waiter)
                // Unlock the global cache again.
            };

            if let Some((key, program)) = program_to_store {
                program_cache_for_tx_batch.loaded_missing = true;
                let mut global_program_cache = self.global_program_cache.write().unwrap();
                // Submit our last completed loading task.
                if global_program_cache.finish_cooperative_loading_task(
                    program_runtime_environments_for_execution,
                    self.slot,
                    key,
                    program,
                ) && limit_to_load_programs
                {
                    // This branch is taken when there is an error in assigning a program to a
                    // cache slot. It is not possible to mock this error for SVM unit
                    // tests purposes.
                    *program_cache_for_tx_batch = ProgramCacheForTxBatch::new(self.slot);
                    program_cache_for_tx_batch.hit_max_limit = true;
                    return;
                }
            } else if missing_programs.is_empty() {
                break;
            } else {
                // Sleep until the next finish_cooperative_loading_task() call.
                // Once a task completes we'll wake up and try to load the
                // missing programs inside the tx batch again.
                let _new_cookie = task_waiter.wait(task_cookie);
            }
        }
    }

    /// Execute a transaction using the provided loaded accounts and update
    /// the executors cache if the transaction was successful.
    #[allow(clippy::too_many_arguments)]
    fn execute_loaded_transaction<CB: TransactionProcessingCallback>(
        &self,
        callback: &CB,
        tx: &impl SVMTransaction,
        mut loaded_transaction: LoadedTransaction,
        execute_timings: &mut ExecuteTimings,
        error_metrics: &mut TransactionErrorMetrics,
        program_cache_for_tx_batch: &mut ProgramCacheForTxBatch,
        environment: &TransactionProcessingEnvironment,
        config: &TransactionProcessingConfig,
    ) -> ExecutedTransaction {
        let transaction_accounts = std::mem::take(&mut loaded_transaction.accounts);

        // Ensure the length of accounts matches the expected length from tx.account_keys().
        // This is a sanity check in case that someone starts adding some additional accounts
        // since this has been done before. See discussion in PR #4497 for details
        debug_assert!(transaction_accounts.len() == tx.account_keys().len());

        fn transaction_accounts_lamports_sum(
            accounts: &[(Pubkey, AccountSharedData)],
        ) -> Option<u128> {
            accounts.iter().try_fold(0u128, |sum, (_, account)| {
                sum.checked_add(u128::from(account.lamports()))
            })
        }

        let lamports_before_tx =
            transaction_accounts_lamports_sum(&transaction_accounts).unwrap_or(0);

        let compute_budget = loaded_transaction.compute_budget;

        let mut transaction_context = TransactionContext::new(
            transaction_accounts,
            environment.rent.clone(),
            compute_budget.max_instruction_stack_depth,
            compute_budget.max_instruction_trace_length,
        );

        let pre_account_state_info =
            TransactionAccountStateInfo::new(&transaction_context, tx, &environment.rent);

        let log_collector = if config.recording_config.enable_log_recording {
            match config.log_messages_bytes_limit {
                None => Some(LogCollector::new_ref()),
                Some(log_messages_bytes_limit) => Some(LogCollector::new_ref_with_limit(Some(
                    log_messages_bytes_limit,
                ))),
            }
        } else {
            None
        };

        let mut executed_units = 0u64;
        let sysvar_cache = &self.sysvar_cache.read().unwrap();

        let mut invoke_context = InvokeContext::new(
            &mut transaction_context,
            program_cache_for_tx_batch,
            EnvironmentConfig::new(
                environment.blockhash,
                environment.blockhash_lamports_per_signature,
                callback,
                &environment.feature_set,
                &environment.program_runtime_environments_for_execution,
                &environment.program_runtime_environments_for_deployment,
                sysvar_cache,
            ),
            log_collector.clone(),
            compute_budget,
            self.execution_cost,
        );

        let mut process_message_time = Measure::start("process_message_time");
        let process_result = process_message(
            tx,
            &loaded_transaction.program_indices,
            &mut invoke_context,
            execute_timings,
            &mut executed_units,
        );
        process_message_time.stop();

        drop(invoke_context);

        execute_timings.execute_accessories.process_message_us += process_message_time.as_us();

        let mut status = process_result
            .and_then(|info| {
                let post_account_state_info =
                    TransactionAccountStateInfo::new(&transaction_context, tx, &environment.rent);
                TransactionAccountStateInfo::verify_changes(
                    &pre_account_state_info,
                    &post_account_state_info,
                    &transaction_context,
                )
                .map(|_| info)
            })
            .map_err(|err| {
                match err {
                    TransactionError::InvalidRentPayingAccount
                    | TransactionError::InsufficientFundsForRent { .. } => {
                        error_metrics.invalid_rent_paying_account += 1;
                    }
                    TransactionError::InvalidAccountIndex => {
                        error_metrics.invalid_account_index += 1;
                    }
                    _ => {
                        error_metrics.instruction_error += 1;
                    }
                }
                err
            });

        let log_messages: Option<TransactionLogMessages> =
            log_collector.and_then(|log_collector| {
                Rc::try_unwrap(log_collector)
                    .map(|log_collector| log_collector.into_inner().into_messages())
                    .ok()
            });

        let (execution_record, inner_instructions) = Self::deconstruct_transaction(
            transaction_context,
            config.recording_config.enable_cpi_recording,
        );

        let ExecutionRecord {
            accounts,
            return_data,
            touched_account_count,
            accounts_resize_delta: accounts_data_len_delta,
        } = execution_record;

        if status.is_ok()
            && transaction_accounts_lamports_sum(&accounts)
                .filter(|lamports_after_tx| lamports_before_tx == *lamports_after_tx)
                .is_none()
        {
            status = Err(TransactionError::UnbalancedTransaction);
        }
        let status = status.map(|_| ());

        loaded_transaction.accounts = accounts;
        execute_timings.details.total_account_count += loaded_transaction.accounts.len() as u64;
        execute_timings.details.changed_account_count += touched_account_count;

        let return_data = if config.recording_config.enable_return_data_recording
            && !return_data.data.is_empty()
        {
            Some(return_data)
        } else {
            None
        };

        ExecutedTransaction {
            execution_details: TransactionExecutionDetails {
                status,
                log_messages,
                inner_instructions,
                return_data,
                executed_units,
                accounts_data_len_delta,
            },
            loaded_transaction,
            programs_modified_by_tx: program_cache_for_tx_batch.drain_modified_entries(),
        }
    }

    /// Extract an ExecutionRecord and an InnerInstructionsList from a TransactionContext
    fn deconstruct_transaction(
        mut transaction_context: TransactionContext,
        record_inner_instructions: bool,
    ) -> (ExecutionRecord, Option<InnerInstructionsList>) {
        let inner_ix = if record_inner_instructions {
            debug_assert!(transaction_context
                .get_instruction_context_at_index_in_trace(0)
                .map(|instruction_context| instruction_context.get_stack_height()
                    == TRANSACTION_LEVEL_STACK_HEIGHT)
                .unwrap_or(true));

            let (ix_trace, accounts, ix_data_trace) = transaction_context.take_instruction_trace();
            let mut outer_instructions = Vec::new();
            for ((ix_in_trace, ix_data), ix_accounts) in ix_trace
                .into_iter()
                .zip(ix_data_trace.into_iter())
                .zip(accounts)
            {
                let stack_height = ix_in_trace.nesting_level.saturating_add(1);
                if stack_height == TRANSACTION_LEVEL_STACK_HEIGHT {
                    outer_instructions.push(Vec::new());
                } else if let Some(inner_instructions) = outer_instructions.last_mut() {
                    let stack_height = u8::try_from(stack_height).unwrap_or(u8::MAX);
                    inner_instructions.push(InnerInstruction {
                        instruction: CompiledInstruction::new_from_raw_parts(
                            ix_in_trace.program_account_index_in_tx as u8,
                            ix_data.into_owned(),
                            ix_accounts
                                .iter()
                                .map(|acc| acc.index_in_transaction as u8)
                                .collect(),
                        ),
                        stack_height,
                    });
                } else {
                    debug_assert!(false);
                }
            }

            Some(outer_instructions)
        } else {
            None
        };

        let record: ExecutionRecord = transaction_context.into();

        (record, inner_ix)
    }

    pub fn fill_missing_sysvar_cache_entries<CB: TransactionProcessingCallback>(
        &self,
        callbacks: &CB,
    ) {
        let mut sysvar_cache = self.sysvar_cache.write().unwrap();
        sysvar_cache.fill_missing_entries(|pubkey, set_sysvar| {
            if let Some((account, _slot)) = callbacks.get_account_shared_data(pubkey) {
                set_sysvar(account.data());
            }
        });
    }

    pub fn reset_sysvar_cache(&self) {
        let mut sysvar_cache = self.sysvar_cache.write().unwrap();
        sysvar_cache.reset();
    }

    pub fn get_sysvar_cache_for_tests(&self) -> SysvarCache {
        self.sysvar_cache.read().unwrap().clone()
    }

    /// Add a built-in program
    pub fn add_builtin(&self, program_id: Pubkey, builtin: ProgramCacheEntry) {
        self.builtin_program_ids.write().unwrap().insert(program_id);
        self.global_program_cache.write().unwrap().assign_program(
            &self.environments,
            program_id,
            Arc::new(builtin),
        );
    }

    #[cfg(feature = "dev-context-only-utils")]
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    fn writable_sysvar_cache(&self) -> &RwLock<SysvarCache> {
        &self.sysvar_cache
    }
}

#[cfg(test)]
mod tests {
    #[allow(deprecated)]
    use solana_sysvar::fees::Fees;
    use {
        super::*,
        crate::{
            account_loader::{
                LoadedTransactionAccount, ValidatedTransactionDetails,
                TRANSACTION_ACCOUNT_BASE_SIZE,
            },
            nonce_info::NonceInfo,
            rent_calculator::RENT_EXEMPT_RENT_EPOCH,
            rollback_accounts::RollbackAccounts,
        },
        solana_account::{create_account_shared_data_for_test, WritableAccount},
        solana_clock::Clock,
        solana_compute_budget_interface::ComputeBudgetInstruction,
        solana_epoch_schedule::EpochSchedule,
        solana_fee_calculator::FeeCalculator,
        solana_fee_structure::FeeDetails,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_message::{LegacyMessage, Message, MessageHeader, SanitizedMessage},
        solana_nonce as nonce,
        solana_program_runtime::{
            execution_budget::{
                SVMTransactionExecutionAndFeeBudgetLimits, SVMTransactionExecutionBudget,
            },
            loaded_programs::{BlockRelation, ProgramCacheEntryType},
        },
        solana_rent::Rent,
        solana_sdk_ids::{bpf_loader, loader_v4, system_program, sysvar},
        solana_signature::Signature,
        solana_svm_callback::{AccountState, InvokeContextCallback},
        solana_transaction::{sanitized::SanitizedTransaction, Transaction},
        solana_transaction_context::TransactionContext,
        solana_transaction_error::TransactionError,
        std::collections::HashMap,
        test_case::test_case,
    };

    fn new_unchecked_sanitized_message(message: Message) -> SanitizedMessage {
        SanitizedMessage::Legacy(LegacyMessage::new(message, &HashSet::new()))
    }

    struct TestForkGraph {}

    impl ForkGraph for TestForkGraph {
        fn relationship(&self, _a: Slot, _b: Slot) -> BlockRelation {
            BlockRelation::Unknown
        }
    }

    #[derive(Clone)]
    struct MockBankCallback {
        account_shared_data: Arc<RwLock<HashMap<Pubkey, AccountSharedData>>>,
        #[allow(clippy::type_complexity)]
        inspected_accounts:
            Arc<RwLock<HashMap<Pubkey, Vec<(Option<AccountSharedData>, /* is_writable */ bool)>>>>,
        feature_set: SVMFeatureSet,
    }

    impl Default for MockBankCallback {
        fn default() -> Self {
            Self {
                account_shared_data: Arc::default(),
                inspected_accounts: Arc::default(),
                feature_set: SVMFeatureSet::all_enabled(),
            }
        }
    }

    impl InvokeContextCallback for MockBankCallback {}

    impl TransactionProcessingCallback for MockBankCallback {
        fn get_account_shared_data(&self, pubkey: &Pubkey) -> Option<(AccountSharedData, Slot)> {
            self.account_shared_data
                .read()
                .unwrap()
                .get(pubkey)
                .map(|account| (account.clone(), 0))
        }

        fn inspect_account(
            &self,
            address: &Pubkey,
            account_state: AccountState,
            is_writable: bool,
        ) {
            let account = match account_state {
                AccountState::Dead => None,
                AccountState::Alive(account) => Some(account.clone()),
            };
            self.inspected_accounts
                .write()
                .unwrap()
                .entry(*address)
                .or_default()
                .push((account, is_writable));
        }
    }

    impl MockBankCallback {
        pub fn calculate_fee_details(
            message: &impl SVMMessage,
            lamports_per_signature: u64,
            prioritization_fee: u64,
        ) -> FeeDetails {
            let signature_count = message
                .num_transaction_signatures()
                .saturating_add(message.num_ed25519_signatures())
                .saturating_add(message.num_secp256k1_signatures())
                .saturating_add(message.num_secp256r1_signatures());

            FeeDetails::new(
                signature_count.saturating_mul(lamports_per_signature),
                prioritization_fee,
            )
        }
    }

    impl<'a> From<&'a MockBankCallback> for AccountLoader<'a, MockBankCallback> {
        fn from(callbacks: &'a MockBankCallback) -> AccountLoader<'a, MockBankCallback> {
            AccountLoader::new_with_loaded_accounts_capacity(
                None,
                callbacks,
                &callbacks.feature_set,
                0,
            )
        }
    }

    #[test_case(1; "Check results too small")]
    #[test_case(3; "Check results too large")]
    #[should_panic(expected = "Length of check_results does not match length of sanitized_txs")]
    fn test_check_results_txs_length_mismatch(check_results_len: usize) {
        let sanitized_message = new_unchecked_sanitized_message(Message {
            account_keys: vec![Pubkey::new_from_array([0; 32])],
            header: MessageHeader::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 0,
                accounts: vec![],
                data: vec![],
            }],
            recent_blockhash: Hash::default(),
        });

        // Transactions, length 2.
        let sanitized_txs = vec![
            SanitizedTransaction::new_for_tests(
                sanitized_message,
                vec![Signature::new_unique()],
                false,
            );
            2
        ];

        let check_results = vec![
            TransactionCheckResult::Ok(CheckedTransactionDetails::default());
            check_results_len
        ];

        let batch_processor = TransactionBatchProcessor::<TestForkGraph>::default();
        let callback = MockBankCallback::default();

        batch_processor.load_and_execute_sanitized_transactions(
            &callback,
            &sanitized_txs,
            check_results,
            &TransactionProcessingEnvironment::default(),
            &TransactionProcessingConfig::default(),
        );
    }

    #[test]
    fn test_inner_instructions_list_from_instruction_trace() {
        let instruction_trace = [1, 2, 1, 1, 2, 3, 2];
        let mut transaction_context = TransactionContext::new(
            vec![(
                Pubkey::new_unique(),
                AccountSharedData::new(1, 1, &bpf_loader::ID),
            )],
            Rent::default(),
            3,
            instruction_trace.len(),
        );
        for (index_in_trace, stack_height) in instruction_trace.into_iter().enumerate() {
            while stack_height <= transaction_context.get_instruction_stack_height() {
                transaction_context.pop().unwrap();
            }
            if stack_height > transaction_context.get_instruction_stack_height() {
                transaction_context
                    .configure_next_instruction_for_tests(0, vec![], vec![index_in_trace as u8])
                    .unwrap();
                transaction_context.push().unwrap();
            }
        }
        let inner_instructions =
            TransactionBatchProcessor::<TestForkGraph>::deconstruct_transaction(
                transaction_context,
                true,
            )
            .1
            .unwrap();

        assert_eq!(
            inner_instructions,
            vec![
                vec![InnerInstruction {
                    instruction: CompiledInstruction::new_from_raw_parts(0, vec![1], vec![]),
                    stack_height: 2,
                }],
                vec![],
                vec![
                    InnerInstruction {
                        instruction: CompiledInstruction::new_from_raw_parts(0, vec![4], vec![]),
                        stack_height: 2,
                    },
                    InnerInstruction {
                        instruction: CompiledInstruction::new_from_raw_parts(0, vec![5], vec![]),
                        stack_height: 3,
                    },
                    InnerInstruction {
                        instruction: CompiledInstruction::new_from_raw_parts(0, vec![6], vec![]),
                        stack_height: 2,
                    },
                ]
            ]
        );
    }

    #[test]
    fn test_execute_loaded_transaction_recordings() {
        // Setting all the arguments correctly is too burdensome for testing
        // execute_loaded_transaction separately.This function will be tested in an integration
        // test with load_and_execute_sanitized_transactions
        let message = Message {
            account_keys: vec![Pubkey::new_from_array([0; 32])],
            header: MessageHeader::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 0,
                accounts: vec![],
                data: vec![],
            }],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);
        let mut program_cache_for_tx_batch = ProgramCacheForTxBatch::default();
        let batch_processor = TransactionBatchProcessor::<TestForkGraph>::default();

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );

        let loaded_transaction = LoadedTransaction {
            accounts: vec![(Pubkey::new_unique(), AccountSharedData::default())],
            program_indices: vec![0],
            fee_details: FeeDetails::default(),
            rollback_accounts: RollbackAccounts::default(),
            compute_budget: SVMTransactionExecutionBudget::default(),
            loaded_accounts_data_size: 32,
        };

        let processing_environment = TransactionProcessingEnvironment::default();

        let mut processing_config = TransactionProcessingConfig::default();
        processing_config.recording_config.enable_log_recording = true;

        let mock_bank = MockBankCallback::default();

        let executed_tx = batch_processor.execute_loaded_transaction(
            &mock_bank,
            &sanitized_transaction,
            loaded_transaction.clone(),
            &mut ExecuteTimings::default(),
            &mut TransactionErrorMetrics::default(),
            &mut program_cache_for_tx_batch,
            &processing_environment,
            &processing_config,
        );
        assert!(executed_tx.execution_details.log_messages.is_some());

        processing_config.log_messages_bytes_limit = Some(2);

        let executed_tx = batch_processor.execute_loaded_transaction(
            &mock_bank,
            &sanitized_transaction,
            loaded_transaction.clone(),
            &mut ExecuteTimings::default(),
            &mut TransactionErrorMetrics::default(),
            &mut program_cache_for_tx_batch,
            &processing_environment,
            &processing_config,
        );
        assert!(executed_tx.execution_details.log_messages.is_some());
        assert!(executed_tx.execution_details.inner_instructions.is_none());

        processing_config.recording_config.enable_log_recording = false;
        processing_config.recording_config.enable_cpi_recording = true;
        processing_config.log_messages_bytes_limit = None;

        let executed_tx = batch_processor.execute_loaded_transaction(
            &mock_bank,
            &sanitized_transaction,
            loaded_transaction,
            &mut ExecuteTimings::default(),
            &mut TransactionErrorMetrics::default(),
            &mut program_cache_for_tx_batch,
            &processing_environment,
            &processing_config,
        );

        assert!(executed_tx.execution_details.log_messages.is_none());
        assert!(executed_tx.execution_details.inner_instructions.is_some());
    }

    #[test]
    fn test_execute_loaded_transaction_error_metrics() {
        // Setting all the arguments correctly is too burdensome for testing
        // execute_loaded_transaction separately.This function will be tested in an integration
        // test with load_and_execute_sanitized_transactions
        let key1 = Pubkey::new_unique();
        let key2 = Pubkey::new_unique();
        let message = Message {
            account_keys: vec![key1, key2],
            header: MessageHeader::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 0,
                accounts: vec![2],
                data: vec![],
            }],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);
        let mut program_cache_for_tx_batch = ProgramCacheForTxBatch::default();
        let batch_processor = TransactionBatchProcessor::<TestForkGraph>::default();

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );

        let mut account_data = AccountSharedData::default();
        account_data.set_owner(bpf_loader::id());
        let loaded_transaction = LoadedTransaction {
            accounts: vec![
                (key1, AccountSharedData::default()),
                (key2, AccountSharedData::default()),
            ],
            program_indices: vec![0],
            fee_details: FeeDetails::default(),
            rollback_accounts: RollbackAccounts::default(),
            compute_budget: SVMTransactionExecutionBudget::default(),
            loaded_accounts_data_size: 0,
        };

        let processing_config = TransactionProcessingConfig {
            recording_config: ExecutionRecordingConfig::new_single_setting(false),
            ..Default::default()
        };
        let mut error_metrics = TransactionErrorMetrics::new();
        let mock_bank = MockBankCallback::default();

        let _ = batch_processor.execute_loaded_transaction(
            &mock_bank,
            &sanitized_transaction,
            loaded_transaction,
            &mut ExecuteTimings::default(),
            &mut error_metrics,
            &mut program_cache_for_tx_batch,
            &TransactionProcessingEnvironment::default(),
            &processing_config,
        );

        assert_eq!(error_metrics.instruction_error.0, 1);
    }

    #[test]
    #[should_panic = "called load_program_with_pubkey() with nonexistent account"]
    fn test_replenish_program_cache_with_nonexistent_accounts() {
        let mock_bank = MockBankCallback::default();
        let account_loader = (&mock_bank).into();
        let fork_graph = Arc::new(RwLock::new(TestForkGraph {}));
        let batch_processor =
            TransactionBatchProcessor::new(0, 0, Arc::downgrade(&fork_graph), None, None);
        let key = Pubkey::new_unique();

        let mut account_set = HashSet::new();
        account_set.insert(key);

        let mut program_cache_for_tx_batch = ProgramCacheForTxBatch::new(batch_processor.slot);

        batch_processor.replenish_program_cache(
            &account_loader,
            &account_set,
            &ProgramRuntimeEnvironments::default(),
            &mut program_cache_for_tx_batch,
            &mut ExecuteTimings::default(),
            false,
            true,
            true,
        );
    }

    #[test]
    fn test_replenish_program_cache() {
        let mock_bank = MockBankCallback::default();
        let fork_graph = Arc::new(RwLock::new(TestForkGraph {}));
        let batch_processor =
            TransactionBatchProcessor::new(0, 0, Arc::downgrade(&fork_graph), None, None);
        let program_runtime_environments_for_execution =
            batch_processor.get_environments_for_epoch(0);
        let key = Pubkey::new_unique();

        let mut account_data = AccountSharedData::default();
        account_data.set_owner(bpf_loader::id());
        mock_bank
            .account_shared_data
            .write()
            .unwrap()
            .insert(key, account_data);
        let account_loader = (&mock_bank).into();

        let mut account_set = HashSet::new();
        account_set.insert(key);
        let mut loaded_missing = 0;

        for limit_to_load_programs in [false, true] {
            let mut program_cache_for_tx_batch = ProgramCacheForTxBatch::new(batch_processor.slot);

            batch_processor.replenish_program_cache(
                &account_loader,
                &account_set,
                &program_runtime_environments_for_execution,
                &mut program_cache_for_tx_batch,
                &mut ExecuteTimings::default(),
                false,
                limit_to_load_programs,
                true,
            );
            assert!(!program_cache_for_tx_batch.hit_max_limit);
            if program_cache_for_tx_batch.loaded_missing {
                loaded_missing += 1;
            }

            let program = program_cache_for_tx_batch.find(&key).unwrap();
            assert!(matches!(
                program.program,
                ProgramCacheEntryType::FailedVerification(_)
            ));
        }
        assert!(loaded_missing > 0);
    }

    #[test]
    fn test_filter_executable_program_accounts() {
        let mock_bank = MockBankCallback::default();
        let key1 = Pubkey::new_unique();
        let owner1 = bpf_loader::id();
        let key2 = Pubkey::new_unique();
        let owner2 = loader_v4::id();

        let mut data1 = AccountSharedData::default();
        data1.set_owner(owner1);
        data1.set_lamports(93);
        mock_bank
            .account_shared_data
            .write()
            .unwrap()
            .insert(key1, data1);

        let mut data2 = AccountSharedData::default();
        data2.set_owner(owner2);
        data2.set_lamports(90);
        mock_bank
            .account_shared_data
            .write()
            .unwrap()
            .insert(key2, data2);
        let account_loader = (&mock_bank).into();

        let message = Message {
            account_keys: vec![key1, key2],
            header: MessageHeader::default(),
            instructions: vec![CompiledInstruction {
                program_id_index: 0,
                accounts: vec![],
                data: vec![],
            }],
            recent_blockhash: Hash::default(),
        };

        let sanitized_message = new_unchecked_sanitized_message(message);

        let sanitized_transaction = SanitizedTransaction::new_for_tests(
            sanitized_message,
            vec![Signature::new_unique()],
            false,
        );

        let batch_processor = TransactionBatchProcessor::<TestForkGraph>::default();
        let program_accounts_set = batch_processor.filter_executable_program_accounts(
            &account_loader,
            &mut ProgramCacheForTxBatch::default(),
            &sanitized_transaction,
        );

        assert_eq!(program_accounts_set.len(), 2);
        assert!(program_accounts_set.contains(&key1));
        assert!(program_accounts_set.contains(&key2));
    }

    #[test]
    fn test_filter_executable_program_accounts_no_errors() {
        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();

        let non_program_pubkey1 = Pubkey::new_unique();
        let non_program_pubkey2 = Pubkey::new_unique();
        let program1_pubkey = bpf_loader::id();
        let program2_pubkey = loader_v4::id();
        let account1_pubkey = Pubkey::new_unique();
        let account2_pubkey = Pubkey::new_unique();
        let account3_pubkey = Pubkey::new_unique();
        let account4_pubkey = Pubkey::new_unique();

        let account5_pubkey = Pubkey::new_unique();

        let bank = MockBankCallback::default();
        bank.account_shared_data.write().unwrap().insert(
            non_program_pubkey1,
            AccountSharedData::new(1, 10, &account5_pubkey),
        );
        bank.account_shared_data.write().unwrap().insert(
            non_program_pubkey2,
            AccountSharedData::new(1, 10, &account5_pubkey),
        );
        bank.account_shared_data.write().unwrap().insert(
            program1_pubkey,
            AccountSharedData::new(40, 1, &account5_pubkey),
        );
        bank.account_shared_data.write().unwrap().insert(
            program2_pubkey,
            AccountSharedData::new(40, 1, &account5_pubkey),
        );
        bank.account_shared_data.write().unwrap().insert(
            account1_pubkey,
            AccountSharedData::new(1, 10, &non_program_pubkey1),
        );
        bank.account_shared_data.write().unwrap().insert(
            account2_pubkey,
            AccountSharedData::new(1, 10, &non_program_pubkey2),
        );
        bank.account_shared_data.write().unwrap().insert(
            account3_pubkey,
            AccountSharedData::new(40, 1, &program1_pubkey),
        );
        bank.account_shared_data.write().unwrap().insert(
            account4_pubkey,
            AccountSharedData::new(40, 1, &program2_pubkey),
        );
        let account_loader = (&bank).into();

        let tx1 = Transaction::new_with_compiled_instructions(
            &[&keypair1],
            &[non_program_pubkey1],
            Hash::new_unique(),
            vec![account1_pubkey, account2_pubkey, account3_pubkey],
            vec![CompiledInstruction::new(1, &(), vec![0])],
        );
        let sanitized_tx1 = SanitizedTransaction::from_transaction_for_tests(tx1);

        let tx2 = Transaction::new_with_compiled_instructions(
            &[&keypair2],
            &[non_program_pubkey2],
            Hash::new_unique(),
            vec![account4_pubkey, account3_pubkey, account2_pubkey],
            vec![CompiledInstruction::new(1, &(), vec![0])],
        );
        let sanitized_tx2 = SanitizedTransaction::from_transaction_for_tests(tx2);

        let batch_processor = TransactionBatchProcessor::<TestForkGraph>::default();

        let tx1_programs = batch_processor.filter_executable_program_accounts(
            &account_loader,
            &mut ProgramCacheForTxBatch::default(),
            &sanitized_tx1,
        );

        assert_eq!(tx1_programs.len(), 1);
        assert!(
            tx1_programs.contains(&account3_pubkey),
            "failed to find the program account",
        );

        let tx2_programs = batch_processor.filter_executable_program_accounts(
            &account_loader,
            &mut ProgramCacheForTxBatch::default(),
            &sanitized_tx2,
        );

        assert_eq!(tx2_programs.len(), 2);
        assert!(
            tx2_programs.contains(&account3_pubkey),
            "failed to find the program account",
        );
        assert!(
            tx2_programs.contains(&account4_pubkey),
            "failed to find the program account",
        );
    }

    #[test]
    #[allow(deprecated)]
    fn test_sysvar_cache_initialization1() {
        let mock_bank = MockBankCallback::default();

        let clock = Clock {
            slot: 1,
            epoch_start_timestamp: 2,
            epoch: 3,
            leader_schedule_epoch: 4,
            unix_timestamp: 5,
        };
        let clock_account = create_account_shared_data_for_test(&clock);
        mock_bank
            .account_shared_data
            .write()
            .unwrap()
            .insert(sysvar::clock::id(), clock_account);

        let epoch_schedule = EpochSchedule::custom(64, 2, true);
        let epoch_schedule_account = create_account_shared_data_for_test(&epoch_schedule);
        mock_bank
            .account_shared_data
            .write()
            .unwrap()
            .insert(sysvar::epoch_schedule::id(), epoch_schedule_account);

        let fees = Fees {
            fee_calculator: FeeCalculator {
                lamports_per_signature: 123,
            },
        };
        let fees_account = create_account_shared_data_for_test(&fees);
        mock_bank
            .account_shared_data
            .write()
            .unwrap()
            .insert(sysvar::fees::id(), fees_account);

        let rent = Rent::with_slots_per_epoch(2048);
        let rent_account = create_account_shared_data_for_test(&rent);
        mock_bank
            .account_shared_data
            .write()
            .unwrap()
            .insert(sysvar::rent::id(), rent_account);

        let transaction_processor = TransactionBatchProcessor::<TestForkGraph>::default();
        transaction_processor.fill_missing_sysvar_cache_entries(&mock_bank);

        let sysvar_cache = transaction_processor.sysvar_cache.read().unwrap();
        let cached_clock = sysvar_cache.get_clock();
        let cached_epoch_schedule = sysvar_cache.get_epoch_schedule();
        let cached_fees = sysvar_cache.get_fees();
        let cached_rent = sysvar_cache.get_rent();

        assert_eq!(
            cached_clock.expect("clock sysvar missing in cache"),
            clock.into()
        );
        assert_eq!(
            cached_epoch_schedule.expect("epoch_schedule sysvar missing in cache"),
            epoch_schedule.into()
        );
        assert_eq!(
            cached_fees.expect("fees sysvar missing in cache"),
            fees.into()
        );
        assert_eq!(
            cached_rent.expect("rent sysvar missing in cache"),
            rent.into()
        );
        assert!(sysvar_cache.get_slot_hashes().is_err());
        assert!(sysvar_cache.get_epoch_rewards().is_err());
    }

    #[test]
    #[allow(deprecated)]
    fn test_reset_and_fill_sysvar_cache() {
        let mock_bank = MockBankCallback::default();

        let clock = Clock {
            slot: 1,
            epoch_start_timestamp: 2,
            epoch: 3,
            leader_schedule_epoch: 4,
            unix_timestamp: 5,
        };
        let clock_account = create_account_shared_data_for_test(&clock);
        mock_bank
            .account_shared_data
            .write()
            .unwrap()
            .insert(sysvar::clock::id(), clock_account);

        let epoch_schedule = EpochSchedule::custom(64, 2, true);
        let epoch_schedule_account = create_account_shared_data_for_test(&epoch_schedule);
        mock_bank
            .account_shared_data
            .write()
            .unwrap()
            .insert(sysvar::epoch_schedule::id(), epoch_schedule_account);

        let fees = Fees {
            fee_calculator: FeeCalculator {
                lamports_per_signature: 123,
            },
        };
        let fees_account = create_account_shared_data_for_test(&fees);
        mock_bank
            .account_shared_data
            .write()
            .unwrap()
            .insert(sysvar::fees::id(), fees_account);

        let rent = Rent::with_slots_per_epoch(2048);
        let rent_account = create_account_shared_data_for_test(&rent);
        mock_bank
            .account_shared_data
            .write()
            .unwrap()
            .insert(sysvar::rent::id(), rent_account);

        let transaction_processor = TransactionBatchProcessor::<TestForkGraph>::default();
        // Fill the sysvar cache
        transaction_processor.fill_missing_sysvar_cache_entries(&mock_bank);
        // Reset the sysvar cache
        transaction_processor.reset_sysvar_cache();

        {
            let sysvar_cache = transaction_processor.sysvar_cache.read().unwrap();
            // Test that sysvar cache is empty and none of the values are found
            assert!(sysvar_cache.get_clock().is_err());
            assert!(sysvar_cache.get_epoch_schedule().is_err());
            assert!(sysvar_cache.get_fees().is_err());
            assert!(sysvar_cache.get_epoch_rewards().is_err());
            assert!(sysvar_cache.get_rent().is_err());
            assert!(sysvar_cache.get_epoch_rewards().is_err());
        }

        // Refill the cache and test the values are available.
        transaction_processor.fill_missing_sysvar_cache_entries(&mock_bank);

        let sysvar_cache = transaction_processor.sysvar_cache.read().unwrap();
        let cached_clock = sysvar_cache.get_clock();
        let cached_epoch_schedule = sysvar_cache.get_epoch_schedule();
        let cached_fees = sysvar_cache.get_fees();
        let cached_rent = sysvar_cache.get_rent();

        assert_eq!(
            cached_clock.expect("clock sysvar missing in cache"),
            clock.into()
        );
        assert_eq!(
            cached_epoch_schedule.expect("epoch_schedule sysvar missing in cache"),
            epoch_schedule.into()
        );
        assert_eq!(
            cached_fees.expect("fees sysvar missing in cache"),
            fees.into()
        );
        assert_eq!(
            cached_rent.expect("rent sysvar missing in cache"),
            rent.into()
        );
        assert!(sysvar_cache.get_slot_hashes().is_err());
        assert!(sysvar_cache.get_epoch_rewards().is_err());
    }

    #[test]
    fn test_add_builtin() {
        let fork_graph = Arc::new(RwLock::new(TestForkGraph {}));
        let batch_processor =
            TransactionBatchProcessor::new(0, 0, Arc::downgrade(&fork_graph), None, None);

        let key = Pubkey::new_unique();
        let name = "a_builtin_name";
        let program = ProgramCacheEntry::new_builtin(
            0,
            name.len(),
            |_invoke_context, _param0, _param1, _param2, _param3, _param4| {},
        );

        batch_processor.add_builtin(key, program);

        let mut loaded_programs_for_tx_batch = ProgramCacheForTxBatch::new(0);
        let program_runtime_environments =
            batch_processor.get_environments_for_epoch(batch_processor.epoch);
        batch_processor
            .global_program_cache
            .write()
            .unwrap()
            .extract(
                &mut vec![(key, ProgramCacheMatchCriteria::NoCriteria)],
                &mut loaded_programs_for_tx_batch,
                &program_runtime_environments,
                true,
                true,
            );
        let entry = loaded_programs_for_tx_batch.find(&key).unwrap();

        // Repeating code because ProgramCacheEntry does not implement clone.
        let program = ProgramCacheEntry::new_builtin(
            0,
            name.len(),
            |_invoke_context, _param0, _param1, _param2, _param3, _param4| {},
        );
        assert_eq!(entry, Arc::new(program));
    }

    #[test]
    fn test_validate_transaction_fee_payer_exact_balance() {
        let lamports_per_signature = 5000;
        let message = new_unchecked_sanitized_message(Message::new_with_blockhash(
            &[
                ComputeBudgetInstruction::set_compute_unit_limit(2000u32),
                ComputeBudgetInstruction::set_compute_unit_price(1_000_000_000),
            ],
            Some(&Pubkey::new_unique()),
            &Hash::new_unique(),
        ));
        let fee_payer_address = message.fee_payer();
        let current_epoch = 42;
        let rent = Rent::default();
        let min_balance = rent.minimum_balance(nonce::state::State::size());
        let transaction_fee = lamports_per_signature;
        let priority_fee = 2_000_000u64;
        let starting_balance = transaction_fee + priority_fee;
        assert!(
            starting_balance > min_balance,
            "we're testing that a rent exempt fee payer can be fully drained, so ensure that the \
             starting balance is more than the min balance"
        );

        let fee_payer_rent_epoch = current_epoch;
        let fee_payer_account = AccountSharedData::new_rent_epoch(
            starting_balance,
            0,
            &Pubkey::default(),
            fee_payer_rent_epoch,
        );
        let mut mock_accounts = HashMap::new();
        mock_accounts.insert(*fee_payer_address, fee_payer_account.clone());
        let mock_bank = MockBankCallback {
            account_shared_data: Arc::new(RwLock::new(mock_accounts)),
            ..Default::default()
        };
        let mut account_loader = (&mock_bank).into();

        let mut error_counters = TransactionErrorMetrics::default();
        let compute_budget_and_limits = SVMTransactionExecutionAndFeeBudgetLimits {
            budget: SVMTransactionExecutionBudget {
                compute_unit_limit: 2000,
                ..SVMTransactionExecutionBudget::default()
            },
            fee_details: FeeDetails::new(transaction_fee, priority_fee),
            ..SVMTransactionExecutionAndFeeBudgetLimits::default()
        };
        let result =
            TransactionBatchProcessor::<TestForkGraph>::validate_transaction_nonce_and_fee_payer(
                &mut account_loader,
                &message,
                CheckedTransactionDetails::new(None, compute_budget_and_limits),
                &Hash::default(),
                &rent,
                &mut error_counters,
            );

        let post_validation_fee_payer_account = {
            let mut account = fee_payer_account.clone();
            account.set_rent_epoch(RENT_EXEMPT_RENT_EPOCH);
            account.set_lamports(0);
            account
        };

        assert_eq!(
            result,
            Ok(ValidatedTransactionDetails {
                rollback_accounts: RollbackAccounts::new(
                    None, // nonce
                    *fee_payer_address,
                    post_validation_fee_payer_account.clone(),
                    fee_payer_rent_epoch
                ),
                compute_budget: compute_budget_and_limits.budget,
                loaded_accounts_bytes_limit: compute_budget_and_limits
                    .loaded_accounts_data_size_limit,
                fee_details: FeeDetails::new(transaction_fee, priority_fee),
                loaded_fee_payer_account: LoadedTransactionAccount {
                    loaded_size: TRANSACTION_ACCOUNT_BASE_SIZE + fee_payer_account.data().len(),
                    account: post_validation_fee_payer_account,
                },
            })
        );
    }

    #[test]
    fn test_validate_transaction_fee_payer_rent_paying() {
        let lamports_per_signature = 5000;
        let message = new_unchecked_sanitized_message(Message::new_with_blockhash(
            &[],
            Some(&Pubkey::new_unique()),
            &Hash::new_unique(),
        ));
        let fee_payer_address = message.fee_payer();
        let rent = Rent {
            lamports_per_byte_year: 1_000_000,
            ..Default::default()
        };
        let min_balance = rent.minimum_balance(0);
        let transaction_fee = lamports_per_signature;
        let starting_balance = min_balance - 1;
        let fee_payer_account = AccountSharedData::new(starting_balance, 0, &Pubkey::default());

        let mut mock_accounts = HashMap::new();
        mock_accounts.insert(*fee_payer_address, fee_payer_account.clone());
        let mock_bank = MockBankCallback {
            account_shared_data: Arc::new(RwLock::new(mock_accounts)),
            ..Default::default()
        };
        let mut account_loader = (&mock_bank).into();

        let mut error_counters = TransactionErrorMetrics::default();
        let compute_budget_and_limits = SVMTransactionExecutionAndFeeBudgetLimits::with_fee(
            MockBankCallback::calculate_fee_details(&message, lamports_per_signature, 0),
        );
        let result =
            TransactionBatchProcessor::<TestForkGraph>::validate_transaction_nonce_and_fee_payer(
                &mut account_loader,
                &message,
                CheckedTransactionDetails::new(None, compute_budget_and_limits),
                &Hash::default(),
                &rent,
                &mut error_counters,
            );

        let post_validation_fee_payer_account = {
            let mut account = fee_payer_account.clone();
            account.set_lamports(starting_balance - transaction_fee);
            account
        };

        assert_eq!(
            result,
            Ok(ValidatedTransactionDetails {
                rollback_accounts: RollbackAccounts::new(
                    None, // nonce
                    *fee_payer_address,
                    post_validation_fee_payer_account.clone(),
                    0, // rent epoch
                ),
                compute_budget: compute_budget_and_limits.budget,
                loaded_accounts_bytes_limit: compute_budget_and_limits
                    .loaded_accounts_data_size_limit,
                fee_details: FeeDetails::new(transaction_fee, 0),
                loaded_fee_payer_account: LoadedTransactionAccount {
                    loaded_size: TRANSACTION_ACCOUNT_BASE_SIZE + fee_payer_account.data().len(),
                    account: post_validation_fee_payer_account,
                }
            })
        );
    }

    #[test]
    fn test_validate_transaction_fee_payer_not_found() {
        let message =
            new_unchecked_sanitized_message(Message::new(&[], Some(&Pubkey::new_unique())));

        let mock_bank = MockBankCallback::default();
        let mut account_loader = (&mock_bank).into();
        let mut error_counters = TransactionErrorMetrics::default();
        let result =
            TransactionBatchProcessor::<TestForkGraph>::validate_transaction_nonce_and_fee_payer(
                &mut account_loader,
                &message,
                CheckedTransactionDetails::new(
                    None,
                    SVMTransactionExecutionAndFeeBudgetLimits::default(),
                ),
                &Hash::default(),
                &Rent::default(),
                &mut error_counters,
            );

        assert_eq!(error_counters.account_not_found.0, 1);
        assert_eq!(result, Err(TransactionError::AccountNotFound));
    }

    #[test]
    fn test_validate_transaction_fee_payer_insufficient_funds() {
        let lamports_per_signature = 5000;
        let message =
            new_unchecked_sanitized_message(Message::new(&[], Some(&Pubkey::new_unique())));
        let fee_payer_address = message.fee_payer();
        let fee_payer_account = AccountSharedData::new(1, 0, &Pubkey::default());
        let mut mock_accounts = HashMap::new();
        mock_accounts.insert(*fee_payer_address, fee_payer_account.clone());
        let mock_bank = MockBankCallback {
            account_shared_data: Arc::new(RwLock::new(mock_accounts)),
            ..Default::default()
        };
        let mut account_loader = (&mock_bank).into();

        let mut error_counters = TransactionErrorMetrics::default();
        let result =
            TransactionBatchProcessor::<TestForkGraph>::validate_transaction_nonce_and_fee_payer(
                &mut account_loader,
                &message,
                CheckedTransactionDetails::new(
                    None,
                    SVMTransactionExecutionAndFeeBudgetLimits::with_fee(
                        MockBankCallback::calculate_fee_details(
                            &message,
                            lamports_per_signature,
                            0,
                        ),
                    ),
                ),
                &Hash::default(),
                &Rent::default(),
                &mut error_counters,
            );

        assert_eq!(error_counters.insufficient_funds.0, 1);
        assert_eq!(result, Err(TransactionError::InsufficientFundsForFee));
    }

    #[test]
    fn test_validate_transaction_fee_payer_insufficient_rent() {
        let lamports_per_signature = 5000;
        let message =
            new_unchecked_sanitized_message(Message::new(&[], Some(&Pubkey::new_unique())));
        let fee_payer_address = message.fee_payer();
        let transaction_fee = lamports_per_signature;
        let rent = Rent::default();
        let min_balance = rent.minimum_balance(0);
        let starting_balance = min_balance + transaction_fee - 1;
        let fee_payer_account = AccountSharedData::new(starting_balance, 0, &Pubkey::default());
        let mut mock_accounts = HashMap::new();
        mock_accounts.insert(*fee_payer_address, fee_payer_account.clone());
        let mock_bank = MockBankCallback {
            account_shared_data: Arc::new(RwLock::new(mock_accounts)),
            ..Default::default()
        };
        let mut account_loader = (&mock_bank).into();

        let mut error_counters = TransactionErrorMetrics::default();
        let result =
            TransactionBatchProcessor::<TestForkGraph>::validate_transaction_nonce_and_fee_payer(
                &mut account_loader,
                &message,
                CheckedTransactionDetails::new(
                    None,
                    SVMTransactionExecutionAndFeeBudgetLimits::with_fee(
                        MockBankCallback::calculate_fee_details(
                            &message,
                            lamports_per_signature,
                            0,
                        ),
                    ),
                ),
                &Hash::default(),
                &rent,
                &mut error_counters,
            );

        assert_eq!(
            result,
            Err(TransactionError::InsufficientFundsForRent { account_index: 0 })
        );
    }

    #[test]
    fn test_validate_transaction_fee_payer_invalid() {
        let lamports_per_signature = 5000;
        let message =
            new_unchecked_sanitized_message(Message::new(&[], Some(&Pubkey::new_unique())));
        let fee_payer_address = message.fee_payer();
        let fee_payer_account = AccountSharedData::new(1_000_000, 0, &Pubkey::new_unique());
        let mut mock_accounts = HashMap::new();
        mock_accounts.insert(*fee_payer_address, fee_payer_account.clone());
        let mock_bank = MockBankCallback {
            account_shared_data: Arc::new(RwLock::new(mock_accounts)),
            ..Default::default()
        };
        let mut account_loader = (&mock_bank).into();

        let mut error_counters = TransactionErrorMetrics::default();
        let result =
            TransactionBatchProcessor::<TestForkGraph>::validate_transaction_nonce_and_fee_payer(
                &mut account_loader,
                &message,
                CheckedTransactionDetails::new(
                    None,
                    SVMTransactionExecutionAndFeeBudgetLimits::with_fee(
                        MockBankCallback::calculate_fee_details(
                            &message,
                            lamports_per_signature,
                            0,
                        ),
                    ),
                ),
                &Hash::default(),
                &Rent::default(),
                &mut error_counters,
            );

        assert_eq!(error_counters.invalid_account_for_fee.0, 1);
        assert_eq!(result, Err(TransactionError::InvalidAccountForFee));
    }

    #[test]
    fn test_validate_transaction_fee_payer_is_nonce() {
        let lamports_per_signature = 5000;
        let rent = Rent::default();
        let compute_unit_limit = 1000u64;
        let last_blockhash = Hash::new_unique();
        let message = new_unchecked_sanitized_message(Message::new_with_blockhash(
            &[
                ComputeBudgetInstruction::set_compute_unit_limit(compute_unit_limit as u32),
                ComputeBudgetInstruction::set_compute_unit_price(1_000_000),
            ],
            Some(&Pubkey::new_unique()),
            &last_blockhash,
        ));
        let transaction_fee = lamports_per_signature;
        let compute_budget_and_limits = SVMTransactionExecutionAndFeeBudgetLimits {
            fee_details: FeeDetails::new(transaction_fee, compute_unit_limit),
            ..SVMTransactionExecutionAndFeeBudgetLimits::default()
        };
        let fee_payer_address = message.fee_payer();
        let min_balance = Rent::default().minimum_balance(nonce::state::State::size());
        let priority_fee = compute_unit_limit;

        let nonce_versions = nonce::versions::Versions::new(nonce::state::State::Initialized(
            nonce::state::Data::new(
                *fee_payer_address,
                DurableNonce::default(),
                lamports_per_signature,
            ),
        ));

        let environment_blockhash = Hash::new_unique();
        let next_durable_nonce = DurableNonce::from_blockhash(&environment_blockhash);

        // Sufficient Fees
        {
            let fee_payer_account = AccountSharedData::new_data(
                min_balance + transaction_fee + priority_fee,
                &nonce_versions,
                &system_program::id(),
            )
            .unwrap();

            let mut future_nonce = NonceInfo::new(*fee_payer_address, fee_payer_account.clone());
            future_nonce
                .try_advance_nonce(next_durable_nonce, lamports_per_signature)
                .unwrap();

            let mut mock_accounts = HashMap::new();
            mock_accounts.insert(*fee_payer_address, fee_payer_account.clone());
            let mock_bank = MockBankCallback {
                account_shared_data: Arc::new(RwLock::new(mock_accounts)),
                ..Default::default()
            };
            let mut account_loader = (&mock_bank).into();

            let mut error_counters = TransactionErrorMetrics::default();

            let tx_details = CheckedTransactionDetails::new(
                Some(future_nonce.clone()),
                compute_budget_and_limits,
            );

            let result = TransactionBatchProcessor::<TestForkGraph>::validate_transaction_nonce_and_fee_payer(
                   &mut account_loader,
                   &message,
                   tx_details,
                   &environment_blockhash,
                   &rent,
                   &mut error_counters,
               );

            let post_validation_fee_payer_account = {
                let mut account = fee_payer_account.clone();
                account.set_rent_epoch(RENT_EXEMPT_RENT_EPOCH);
                account.set_lamports(min_balance);
                account
            };

            assert_eq!(
                result,
                Ok(ValidatedTransactionDetails {
                    rollback_accounts: RollbackAccounts::new(
                        Some(future_nonce),
                        *fee_payer_address,
                        post_validation_fee_payer_account.clone(),
                        0, // fee_payer_rent_epoch
                    ),
                    compute_budget: compute_budget_and_limits.budget,
                    loaded_accounts_bytes_limit: compute_budget_and_limits
                        .loaded_accounts_data_size_limit,
                    fee_details: FeeDetails::new(transaction_fee, priority_fee),
                    loaded_fee_payer_account: LoadedTransactionAccount {
                        loaded_size: TRANSACTION_ACCOUNT_BASE_SIZE + fee_payer_account.data().len(),
                        account: post_validation_fee_payer_account,
                    }
                })
            );
        }

        // Insufficient Fees
        {
            let fee_payer_account = AccountSharedData::new_data(
                transaction_fee + priority_fee, // no min_balance this time
                &nonce_versions,
                &system_program::id(),
            )
            .unwrap();

            let mut future_nonce = NonceInfo::new(*fee_payer_address, fee_payer_account.clone());
            future_nonce
                .try_advance_nonce(next_durable_nonce, lamports_per_signature)
                .unwrap();

            let mut mock_accounts = HashMap::new();
            mock_accounts.insert(*fee_payer_address, fee_payer_account.clone());
            let mock_bank = MockBankCallback {
                account_shared_data: Arc::new(RwLock::new(mock_accounts)),
                ..Default::default()
            };
            let mut account_loader = (&mock_bank).into();

            let mut error_counters = TransactionErrorMetrics::default();

            let tx_details = CheckedTransactionDetails::new(
                Some(future_nonce.clone()),
                compute_budget_and_limits,
            );

            let result = TransactionBatchProcessor::<TestForkGraph>::validate_transaction_nonce_and_fee_payer(
                &mut account_loader,
                &message,
                tx_details,
                &environment_blockhash,
                &rent,
                &mut error_counters,
               );

            assert_eq!(error_counters.insufficient_funds.0, 1);
            assert_eq!(result, Err(TransactionError::InsufficientFundsForFee));
        }
    }

    // Ensure `TransactionProcessingCallback::inspect_account()` is called when
    // validating the fee payer, since that's when the fee payer account is loaded.
    #[test]
    fn test_inspect_account_fee_payer() {
        let fee_payer_address = Pubkey::new_unique();
        let fee_payer_account = AccountSharedData::new_rent_epoch(
            123_000_000_000,
            0,
            &Pubkey::default(),
            RENT_EXEMPT_RENT_EPOCH,
        );
        let mock_bank = MockBankCallback::default();
        mock_bank
            .account_shared_data
            .write()
            .unwrap()
            .insert(fee_payer_address, fee_payer_account.clone());
        let mut account_loader = (&mock_bank).into();

        let message = new_unchecked_sanitized_message(Message::new_with_blockhash(
            &[
                ComputeBudgetInstruction::set_compute_unit_limit(2000u32),
                ComputeBudgetInstruction::set_compute_unit_price(1_000_000_000),
            ],
            Some(&fee_payer_address),
            &Hash::new_unique(),
        ));
        TransactionBatchProcessor::<TestForkGraph>::validate_transaction_nonce_and_fee_payer(
            &mut account_loader,
            &message,
            CheckedTransactionDetails::new(
                None,
                SVMTransactionExecutionAndFeeBudgetLimits::with_fee(
                    MockBankCallback::calculate_fee_details(&message, 5000, 0),
                ),
            ),
            &Hash::default(),
            &Rent::default(),
            &mut TransactionErrorMetrics::default(),
        )
        .unwrap();

        // ensure the fee payer is an inspected account
        let actual_inspected_accounts: Vec<_> = mock_bank
            .inspected_accounts
            .read()
            .unwrap()
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        assert_eq!(
            actual_inspected_accounts.as_slice(),
            &[(fee_payer_address, vec![(Some(fee_payer_account), true)])],
        );
    }
}
