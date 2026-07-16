use {
    crate::{
        account_loader::{
            AccountLoader, CheckedTransactionDetails, FeesOnlyTransaction, LoadedTransaction,
            NoOpTransaction, TransactionCheckResult, TransactionLoadResult,
            TransactionValidationResult, ValidatedTransactionDetails, load_transaction,
            update_rent_exempt_status_for_account, validate_fee_payer,
        },
        account_overrides::AccountOverrides,
        nonce_info::NonceInfo,
        program_loader::{filter_executable_program_accounts, load_program_with_pubkey},
        rollback_accounts::RollbackAccounts,
        transaction_account_state_info::{
            TransactionAccountStateInfo, get_uninitialized_accounts_size, verify_changes,
        },
        transaction_balances::{BalanceCollectionRoutines, BalanceCollector},
        transaction_error_metrics::TransactionErrorMetrics,
        transaction_execution_result::{
            AccountsDeltas, ExecutedTransaction, TransactionExecutionDetails,
        },
        transaction_processing_result::{ProcessedTransaction, TransactionProcessingResult},
    },
    log::debug,
    percentage::Percentage,
    solana_account::{AccountSharedData, ReadableAccount, state_traits::StateMut},
    solana_clock::{Epoch, Slot},
    solana_hash::Hash,
    solana_instruction::TRANSACTION_LEVEL_STACK_HEIGHT,
    solana_message::{
        compiled_instruction::CompiledInstruction,
        inner_instruction::{InnerInstruction, InnerInstructionsList},
    },
    solana_nonce::{
        NONCED_TX_MARKER_IX_INDEX,
        state::{DurableNonce, State as NonceState},
        versions::Versions as NonceVersions,
    },
    solana_nonce_account::verify_nonce_account,
    solana_program_runtime::{
        execution_budget::{
            SVMTransactionExecutionAndFeeBudgetLimits, SVMTransactionExecutionCost,
        },
        invoke_context::{EnvironmentConfig, InvokeContext},
        loaded_programs::{
            EpochBoundaryPreparation, ForkGraph, ProgramCache, ProgramCacheForTxBatch,
            ProgramCacheMatchCriteria, ProgramRuntimeEnvironment, ProgramRuntimeEnvironments,
            ProgramToLoad,
        },
        program_cache_entry::{ProgramCacheEntry, ProgramCacheEntryOwner},
        program_metrics::ProgramStatistics,
        solana_sbpf::{program::BuiltinProgram, vm::Config as VmConfig},
        sysvar_cache::SysvarCache,
    },
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_svm_callback::{InvokeContextCallback, TransactionProcessingCallback},
    solana_svm_feature_set::SVMFeatureSet,
    solana_svm_log_collector::LogCollector,
    solana_svm_measure::{measure::Measure, measure_us},
    solana_svm_timings::{ExecuteTimingType, ExecuteTimings},
    solana_svm_transaction::{svm_message::SVMMessage, svm_transaction::SVMTransaction},
    solana_svm_type_overrides::sync::{Arc, RwLock, RwLockReadGuard},
    solana_transaction_context::transaction::{ExecutionRecord, TransactionContext},
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
    /// Whether or not to check a program's deployment slot when replenishing
    /// a program cache instance.
    pub check_program_deployment_slot: bool,
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
    /// Strictly require durable nonce accounts to have the canonical nonce account size.
    ///
    /// This is a leader-side filtering policy. It must not be enabled for replay.
    pub strict_nonce_size_check: bool,
    /// Do not commit unprocessable (no-op) transactions to the ledger.
    ///
    /// This is a leader-side filtering policy. It must not be enabled for replay.
    pub drop_noop_transactions: bool,
}

/// Runtime environment for transaction batch processing.
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
    /// Whether the alpenglow migration has completed for this bank context.
    pub alpenglow_migration_succeeded: bool,
    /// The total stake for the current epoch.
    pub epoch_total_stake: u64,
    /// Runtime feature set to use for the transaction batch.
    pub feature_set: SVMFeatureSet,
    /// Program runtime environments for execution and deployment.
    pub program_runtime_environments: ProgramRuntimeEnvironments,
    /// Rent calculator to use for the transaction batch.
    pub rent: Rent,
}

#[cfg(feature = "dev-context-only-utils")]
pub fn get_mock_transaction_processing_environment() -> TransactionProcessingEnvironment {
    TransactionProcessingEnvironment {
        blockhash: Hash::default(),
        blockhash_lamports_per_signature: 0,
        alpenglow_migration_succeeded: false,
        epoch_total_stake: 0,
        feature_set: SVMFeatureSet::default(),
        program_runtime_environments: ProgramRuntimeEnvironments::mock(),
        rent: Rent::default(),
    }
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

    /// ProgramRuntimeEnvironment of the current epoch
    pub program_runtime_environment: ProgramRuntimeEnvironment,

    /// Builtin program ids
    pub builtin_program_ids: RwLock<HashSet<Pubkey>>,

    /// Cached ProgramCacheForTxBatch pre-populated with builtin entries.
    /// Populated once per block in `new_from()` from the global program cache,
    /// avoiding re-acquiring the lock and re-running extract() on every batch.
    builtin_program_cache: RwLock<ProgramCacheForTxBatch>,

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
            program_runtime_environment: ProgramRuntimeEnvironment::from(
                BuiltinProgram::new_loader(VmConfig::default()),
            ),
            builtin_program_ids: RwLock::new(HashSet::new()),
            builtin_program_cache: RwLock::new(ProgramCacheForTxBatch::new(Slot::default())),
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
            builtin_program_cache: RwLock::new(ProgramCacheForTxBatch::new(slot)),
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
        program_runtime_environment: Option<ProgramRuntimeEnvironment>,
    ) -> Self {
        let mut processor = Self::new_uninitialized(slot, epoch);
        processor
            .global_program_cache
            .write()
            .unwrap()
            .set_fork_graph(fork_graph);
        let empty_loader = || ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock());
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
        processor.program_runtime_environment =
            program_runtime_environment.unwrap_or(empty_loader());
        processor
    }

    /// Create a new `TransactionBatchProcessor` from the current instance, but
    /// with the provided slot and epoch.
    ///
    /// * Inherits the program cache and builtin program ids from the current
    ///   instance.
    /// * Resets the sysvar cache.
    pub fn new_from(&self, slot: Slot, epoch: Epoch) -> Self {
        let builtin_program_ids = self.builtin_program_ids.read().unwrap().clone();
        let environments = self.program_runtime_environment.clone();

        // Pre-populate the builtin program cache from the global cache.
        // This is done once per block rather than once per batch.
        let mut builtin_program_cache = ProgramCacheForTxBatch::new(slot);
        let mut search_for: Vec<ProgramToLoad> = builtin_program_ids
            .iter()
            .map(|program_id| ProgramToLoad {
                program_id,
                loader: ProgramCacheEntryOwner::NativeLoader,
                match_criteria: ProgramCacheMatchCriteria::NoCriteria,
                last_modification_slot: 0,
            })
            .collect();
        self.global_program_cache.read().unwrap().extract(
            &mut search_for,
            &mut builtin_program_cache,
            &environments,
            false,
            false,
        );

        Self {
            slot,
            epoch,
            sysvar_cache: RwLock::<SysvarCache>::default(),
            epoch_boundary_preparation: self.epoch_boundary_preparation.clone(),
            global_program_cache: self.global_program_cache.clone(),
            program_runtime_environment: environments,
            builtin_program_ids: RwLock::new(builtin_program_ids),
            builtin_program_cache: RwLock::new(builtin_program_cache),
            execution_cost: self.execution_cost,
        }
    }

    /// Sets the base execution cost for the transactions that this instance of transaction processor
    /// will execute.
    pub fn set_execution_cost(&mut self, cost: SVMTransactionExecutionCost) {
        self.execution_cost = cost;
    }

    /// Updates the environments when entering a new Epoch.
    pub fn set_program_runtime_environment(&mut self, new_environment: ProgramRuntimeEnvironment) {
        // First update the environment only if it is different
        if *self.program_runtime_environment != *new_environment {
            self.program_runtime_environment = new_environment;
        }
        // Then try to consolidate with the upcoming environment (to reuse the address)
        if let Some(upcoming_environment) = &self
            .epoch_boundary_preparation
            .read()
            .unwrap()
            .upcoming_environment
        {
            let upcoming_environment = ProgramRuntimeEnvironment::clone(upcoming_environment);
            if self.program_runtime_environment != upcoming_environment
                && *self.program_runtime_environment == *upcoming_environment
            {
                // Use the prediction if equal but not identical
                self.program_runtime_environment = upcoming_environment;
            }
        }
    }

    /// Returns the current environments depending on the given epoch
    /// Returns None if the call could result in a deadlock
    pub fn program_runtime_environment_for_epoch(&self, epoch: Epoch) -> ProgramRuntimeEnvironment {
        self.epoch_boundary_preparation
            .read()
            .unwrap()
            .get_upcoming_environment_for_epoch(epoch)
            .unwrap_or_else(|| ProgramRuntimeEnvironment::clone(&self.program_runtime_environment))
    }

    pub fn sysvar_cache(&self) -> RwLockReadGuard<'_, SysvarCache> {
        self.sysvar_cache.read().unwrap()
    }

    /// Main entrypoint to the SVM.
    pub fn load_and_execute_sanitized_transactions<
        CB: TransactionProcessingCallback + InvokeContextCallback,
    >(
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

        // Clone the batch-local program cache (builtins already populated in new_from()).
        // User-deployed programs are loaded per-transaction via replenish_program_cache
        // in the transaction loop below.
        let mut program_cache_for_tx_batch = self.builtin_program_cache.read().unwrap().clone();

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
            let (validate_result, validate_fees_us) = measure_us!(match check_result {
                Ok(tx_details) => Self::validate_transaction_nonce_and_fee_payer(
                    &mut account_loader,
                    tx,
                    tx_details,
                    &environment.blockhash,
                    environment.blockhash_lamports_per_signature,
                    &environment.rent,
                    environment.feature_set.relax_post_exec_min_balance_check,
                    config.strict_nonce_size_check,
                    &mut error_metrics,
                ),
                Err(e) => TransactionValidationResult::Unprocessable(e),
            });
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
                // Unprocessable transactions always result in an error
                TransactionLoadResult::Unprocessable(e) => Err(e),

                // Validation failures that would be no-ops become errors with `drop_on_failure` or
                // `drop_noop_transactions`. These may be produced by SIMD-0290 (fee-payer failure)
                // or SIMD-0297 (nonce failure), but are dropped during block production to avoid commit.
                TransactionLoadResult::NoOp(NoOpTransaction {
                    validation_error: e,
                    ..
                }) if config.drop_on_failure || config.drop_noop_transactions => Err(e),

                // SIMD-0290 (fee-payer failure) or SIMD-0297 (nonce failure) is a non-error no-op on replay
                TransactionLoadResult::NoOp(no_op_tx) =>
                    Ok(ProcessedTransaction::NoOp(Box::new(no_op_tx))),

                // Loading failures that would be fee-only become errors with `drop_on_failure`
                TransactionLoadResult::FeesOnly(FeesOnlyTransaction { load_error: e, .. })
                    if config.drop_on_failure =>
                    Err(e),

                // Transactions that fail at account loading charge fees and roll nonces
                TransactionLoadResult::FeesOnly(fees_only_tx) => {
                    account_loader
                        .update_accounts_for_failed_tx(&fees_only_tx.rollback_accounts, self.slot);
                    Ok(ProcessedTransaction::FeesOnly(Box::new(fees_only_tx)))
                }

                // Transaction is able to be executed
                TransactionLoadResult::Loaded(loaded_transaction) => {
                    let (missing_programs, filter_executable_us) =
                        measure_us!(filter_executable_program_accounts(
                            &account_loader,
                            &program_cache_for_tx_batch,
                            tx.account_keys().iter(),
                            config.check_program_deployment_slot,
                        ));
                    execute_timings.saturating_add_in_place(
                        ExecuteTimingType::FilterExecutableUs,
                        filter_executable_us,
                    );

                    let ((), program_cache_us) = measure_us!({
                        self.replenish_program_cache(
                            &account_loader,
                            missing_programs,
                            environment
                                .program_runtime_environments
                                .get_env_for_execution(),
                            &mut program_cache_for_tx_batch,
                            &mut execute_timings,
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
                                &executed_tx.loaded_transaction.touched_flags,
                                self.slot,
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
                                self.slot,
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
                .evict_using_random_selection(
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
        next_lamports_per_signature: u64,
        rent: &Rent,
        relax_post_exec_min_balance_check: bool,
        strict_nonce_size_check: bool,
        error_counters: &mut TransactionErrorMetrics,
    ) -> TransactionValidationResult {
        let CheckedTransactionDetails {
            nonce_address,
            compute_budget_and_limits,
        } = checked_details;

        // If this is a nonce transaction, validate the nonce info.
        // This must be done for every transaction to support SIMD83 because
        // it may have changed due to use, authorization, or deallocation.
        let nonce_info = if let Some(ref nonce_address) = nonce_address {
            let next_durable_nonce = DurableNonce::from_blockhash(environment_blockhash);
            let nonce_result = Self::validate_transaction_nonce(
                account_loader,
                message,
                nonce_address,
                &next_durable_nonce,
                next_lamports_per_signature,
                strict_nonce_size_check,
                error_counters,
            );

            match nonce_result {
                Ok(nonce_info) => Some(nonce_info),
                Err(e) => return TransactionValidationResult::Unprocessable(e),
            }
        } else {
            None
        };

        let is_blockhash_transaction = nonce_info.is_none();

        // Now validate the fee-payer for the transaction unconditionally.
        let fee_payer_result = Self::validate_transaction_fee_payer(
            account_loader,
            message,
            nonce_info,
            compute_budget_and_limits,
            rent,
            relax_post_exec_min_balance_check,
            error_counters,
        );

        // With SIMD-0290 enabled, a blockhash transaction with an invalid fee-payer can
        // be processed as a no-op. However, nonce transactions must ALWAYS be discarded,
        // regardless of whether they use a standard or nonce-based fee-payer.
        // Fee-payer failure for nonces will be handled the same as nonce failures
        // themselves when we implement SIMD-0297.
        let allow_noop =
            account_loader.feature_set.relax_fee_payer_constraint && is_blockhash_transaction;

        match fee_payer_result {
            Ok(details) => TransactionValidationResult::Loadable(details),
            Err(e) if allow_noop => {
                // We correctly report the fee-payer balance if the account exists but is invalid.
                let fee_payer_balance = account_loader
                    .load_account(message.fee_payer())
                    .map(|account| account.lamports());

                // Per SIMD-0290, we report the maximum allowed compute and data usage.
                // In essence the intent is if you pack a block with n non-paying transactions,
                // you could have packed n or more of the same paying transactions, rather than
                // being able to pack some multiple more non-paying than paying.
                TransactionValidationResult::NoOp(NoOpTransaction {
                    validation_error: e,
                    fee_payer_balance,
                    compute_unit_limit: compute_budget_and_limits.budget.compute_unit_limit,
                    loaded_accounts_bytes_limit: compute_budget_and_limits
                        .loaded_accounts_data_size_limit,
                })
            }
            Err(e) => TransactionValidationResult::Unprocessable(e),
        }
    }

    // Loads transaction fee payer, collects rent if necessary, then calculates
    // transaction fees, and deducts them from the fee payer balance. If the
    // account is not found or has insufficient funds, an error is returned.
    fn validate_transaction_fee_payer<CB: TransactionProcessingCallback>(
        account_loader: &mut AccountLoader<CB>,
        message: &impl SVMMessage,
        nonce_info: Option<NonceInfo>,
        compute_budget_and_limits: SVMTransactionExecutionAndFeeBudgetLimits,
        rent: &Rent,
        relax_post_exec_min_balance_check: bool,
        error_counters: &mut TransactionErrorMetrics,
    ) -> TransactionResult<ValidatedTransactionDetails> {
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
            &mut loaded_fee_payer.account,
            fee_payer_index,
            error_counters,
            rent,
            compute_budget_and_limits.fee_details.total_fee(),
            relax_post_exec_min_balance_check,
        )?;

        // Capture fee-subtracted fee payer account and next nonce account state
        // to commit if transaction execution fails.
        let rollback_accounts = RollbackAccounts::new(
            nonce_info,
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
        nonce_address: &Pubkey,
        next_durable_nonce: &DurableNonce,
        next_lamports_per_signature: u64,
        strict_nonce_size_check: bool,
        error_counters: &mut TransactionErrorMetrics,
    ) -> TransactionResult<NonceInfo> {
        // When SIMD83 is enabled, if the nonce has been used in this batch already, we must drop
        // the transaction. This is the same as if it was used in different batches in the same slot.
        // It is possible that the nonce account was used, closed, closed and reopened, closed and
        // spoofed by a non-system program, or had its authority changed. Such a transaction cannot
        // be processed, even as fee-only.

        let Some(mut nonce_account) = account_loader
            .load_transaction_account(nonce_address, true)
            .map(|loaded| loaded.account)
        else {
            error_counters.account_not_found += 1;
            return Err(TransactionError::AccountNotFound);
        };

        if strict_nonce_size_check && nonce_account.data().len() != NonceState::size() {
            error_counters.blockhash_not_found += 1;
            return Err(TransactionError::BlockhashNotFound);
        }

        // This function verifies:
        // * Nonce account owner is SystemProgram
        // * Nonce account parses as State::Initialized
        // * Stored durable nonce matches the message blockhash
        let Some(nonce_data) = verify_nonce_account(&nonce_account, message.recent_blockhash())
        else {
            error_counters.blockhash_not_found += 1;
            return Err(TransactionError::BlockhashNotFound);
        };

        // We must still check that the nonce account is usable and that its authority has signed.
        let nonce_can_be_advanced = &nonce_data.durable_nonce != next_durable_nonce;
        let nonce_authority_is_valid = message
            .get_ix_signers(NONCED_TX_MARKER_IX_INDEX as usize)
            .any(|signer| signer == &nonce_data.authority);

        if nonce_can_be_advanced && nonce_authority_is_valid {
            let next_nonce_state = NonceState::new_initialized(
                &nonce_data.authority,
                *next_durable_nonce,
                next_lamports_per_signature,
            );
            nonce_account
                .set_state(&NonceVersions::new(next_nonce_state))
                .expect("Serializing into a validated nonce account cannot fail");

            Ok(NonceInfo::new(*nonce_address, nonce_account))
        } else {
            error_counters.blockhash_not_found += 1;
            Err(TransactionError::BlockhashNotFound)
        }
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    fn replenish_program_cache<CB: TransactionProcessingCallback>(
        &self,
        account_loader: &AccountLoader<CB>,
        mut missing_programs: Vec<ProgramToLoad>,
        program_runtime_environment_for_execution: &ProgramRuntimeEnvironment,
        program_cache_for_tx_batch: &mut ProgramCacheForTxBatch,
        execute_timings: &mut ExecuteTimings,
        limit_to_load_programs: bool,
        increment_usage_counter: bool,
    ) {
        let mut count_hits_and_misses = true;
        loop {
            // Lock the global cache.
            let global_program_cache = self.global_program_cache.read().unwrap();
            // Figure out which program needs to be loaded next.
            let program_to_load = global_program_cache.extract(
                &mut missing_programs,
                program_cache_for_tx_batch,
                program_runtime_environment_for_execution,
                increment_usage_counter,
                count_hits_and_misses,
            );
            count_hits_and_misses = false;
            let task_waiter = Arc::clone(&global_program_cache.loading_task_waiter);
            let task_cookie = task_waiter.cookie();
            // Unlock the global cache again.
            drop(global_program_cache);

            let program_to_store = program_to_load.map(|key| {
                // Load, verify and compile one program.
                let (program, last_modification_slot) = load_program_with_pubkey(
                    account_loader,
                    program_runtime_environment_for_execution,
                    &key,
                    self.slot,
                    execute_timings,
                )
                .expect("called load_program_with_pubkey() with nonexistent account");
                (key, program, last_modification_slot)
            });

            if let Some((key, program, last_modification_slot)) = program_to_store {
                program_cache_for_tx_batch.loaded_missing = true;
                let mut global_program_cache = self.global_program_cache.write().unwrap();
                // Submit our last completed loading task.
                if global_program_cache.finish_cooperative_loading_task(
                    program_runtime_environment_for_execution,
                    self.slot,
                    key,
                    last_modification_slot,
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
                // Remember: there are multiple transaction processor threads running concurrently
                // and those other threads may be loading this or other programs.
                //
                // So, sleep until some other thread submits a program with their
                // `finish_cooperative_loading_task` call. We'll then wake up and try to load the
                // missing programs inside the tx batch again.
                let _new_cookie = task_waiter.wait(task_cookie);
            }
        }
    }

    /// Similar to replenish_program_cache() but only used in Bank::prepare_program_cache_for_upcoming_feature_set().
    pub fn prepare_one_program_for_upcoming_feature_set<CB: TransactionProcessingCallback>(
        &self,
        account_loader: &CB,
        check_program_deployment_slot: bool,
        upcoming_environment: &ProgramRuntimeEnvironment,
        key: &Pubkey,
        stats_of_enqueued_program: &ProgramStatistics,
    ) {
        let mut program_cache_for_tx_batch = ProgramCacheForTxBatch::new(self.slot);
        let mut missing_programs = filter_executable_program_accounts(
            account_loader,
            &program_cache_for_tx_batch,
            std::iter::once(key),
            check_program_deployment_slot,
        );
        if missing_programs.is_empty() {
            // Program account was closed
            return;
        }
        let program_to_load = {
            let program_cache_guard = self.global_program_cache.read().unwrap();
            program_cache_guard.extract(
                &mut missing_programs,
                &mut program_cache_for_tx_batch,
                upcoming_environment,
                false, // increment_usage_counter
                false, // count_hits_and_misses
            )
            // Unlock again because load_program_with_pubkey() might take a while.
        };
        // Maybe the enqueued program was already loaded and can be skipped.
        if let Some(key) = program_to_load {
            // Load, verify and compile one program.
            let (recompiled, last_modification_slot) = load_program_with_pubkey(
                account_loader,
                upcoming_environment,
                &key,
                self.slot,
                &mut ExecuteTimings::default(),
            )
            .expect("called load_program_with_pubkey() with nonexistent account");
            recompiled.stats.merge_from(stats_of_enqueued_program);
            // Lock the global cache as writable this time.
            let mut program_cache_guard = self.global_program_cache.write().unwrap();
            // Submit our last completed loading task.
            program_cache_guard.finish_cooperative_loading_task(
                upcoming_environment,
                self.slot,
                key,
                last_modification_slot,
                recompiled,
            );
        }
    }

    /// Execute a transaction using the provided loaded accounts and update
    /// the executors cache if the transaction was successful.
    fn execute_loaded_transaction<CB: InvokeContextCallback>(
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
            tx.num_instructions(),
        );

        let relax_post_exec_min_balance_check =
            environment.feature_set.relax_post_exec_min_balance_check;
        let pre_account_state_info = TransactionAccountStateInfo::new_pre_exec(
            &transaction_context,
            tx,
            &environment.rent,
            relax_post_exec_min_balance_check,
        );

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
                environment.alpenglow_migration_succeeded,
                callback,
                &environment.feature_set,
                &environment.program_runtime_environments,
                sysvar_cache,
            ),
            log_collector.clone(),
            compute_budget,
            self.execution_cost,
        );

        let mut process_message_time = Measure::start("process_message_time");
        let process_result = invoke_context
            .process_message(tx, execute_timings, &mut executed_units)
            .map_err(|(index, err)| TransactionError::InstructionError(index, err));
        process_message_time.stop();

        drop(invoke_context);

        execute_timings.execute_accessories.process_message_us += process_message_time.as_us();

        let mut post_account_state_info_result = process_result
            .and_then(|_info| {
                let post_account_state_info = TransactionAccountStateInfo::new_post_exec(
                    &transaction_context,
                    tx,
                    &pre_account_state_info,
                    &environment.rent,
                    relax_post_exec_min_balance_check,
                );
                verify_changes(
                    &pre_account_state_info,
                    &post_account_state_info,
                    &transaction_context,
                )
                .map(|_| post_account_state_info)
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
            mut touched_flags,
            accounts_resize_delta,
        } = execution_record;

        // The fee payer (account index 0) is debited during loading, outside the
        // VM, so it carries no VM touch flag but must still be written back.
        if let Some(fee_payer_touched) = touched_flags.first_mut() {
            *fee_payer_touched = true;
        }

        // changed_account_count reflects every account that will be written back,
        // including the fee payer marked above.
        let touched_account_count = touched_flags.iter().filter(|touched| **touched).count();

        if post_account_state_info_result.is_ok()
            && transaction_accounts_lamports_sum(&accounts)
                .filter(|lamports_after_tx| lamports_before_tx == *lamports_after_tx)
                .is_none()
        {
            post_account_state_info_result = Err(TransactionError::UnbalancedTransaction);
        }

        // accounts_resize_delta and accounts_uninitialized_size must be set to None
        // in the result if status is an error
        let (status, accounts_deltas) = post_account_state_info_result
            .map(|post_state_info| {
                (
                    Ok(()),
                    Some(AccountsDeltas {
                        accounts_resize_delta,
                        accounts_uninitialized_size: get_uninitialized_accounts_size(
                            &post_state_info,
                        ),
                    }),
                )
            })
            .unwrap_or_else(|err| (Err(err), None));

        loaded_transaction.accounts = accounts;
        loaded_transaction.touched_flags = touched_flags;
        execute_timings.details.total_account_count += loaded_transaction.accounts.len() as u64;
        execute_timings.details.changed_account_count += touched_account_count as u64;

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
                accounts_deltas,
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
            debug_assert!(
                transaction_context
                    .get_instruction_context_at_index_in_trace(0)
                    .map(|instruction_context| instruction_context.get_stack_height()
                        == TRANSACTION_LEVEL_STACK_HEIGHT)
                    .unwrap_or(true)
            );

            let top_level_ixs_num = transaction_context
                .get_instruction_trace_length()
                .saturating_sub(transaction_context.number_of_cpis_in_trace());
            // This vector is a map between CPI number in trace (not counting top level
            // instructions) and the top level caller index.
            // In TransactionContext, caller instructions always precede callee instructions, so
            // we can use it to avoid backtracking on instructions callers to
            // find the top level instruction that started the call chain.
            let mut parent_positions: Vec<usize> =
                vec![usize::MAX; transaction_context.number_of_cpis_in_trace()];
            let (ix_trace, accounts, ix_data_trace) = transaction_context.take_instruction_trace();
            let mut outer_instructions: Vec<Vec<InnerInstruction>> =
                vec![Vec::new(); top_level_ixs_num];
            for (cpi_num, ((ix_in_trace, ix_data), ix_accounts)) in ix_trace
                .into_iter()
                .zip(ix_data_trace)
                .zip(accounts)
                .skip(top_level_ixs_num)
                .enumerate()
            {
                let caller_ix = ix_in_trace.index_of_caller_instruction;
                debug_assert_ne!(caller_ix, u16::MAX, "Instruction is not a CPI");

                // If the caller index is less than the number of top level instructions,
                // it directly represents a top level instruction index.
                // Top level instructions precede all CPIs in the instruction trace.
                let outer_index = if (caller_ix as usize) < top_level_ixs_num {
                    *parent_positions.get_mut(cpi_num).unwrap() = caller_ix as usize;
                    caller_ix as usize
                // If the above condition was false, we are dealing with a nested CPI.
                // The caller_ix represents the CPI index in the instruction trace.
                // To calculate its cpi_number (i.e. the index in `parent_positions)`
                // we subtract is from the number of top level instructions.
                } else if let Some(caller_index) = parent_positions
                    .get((caller_ix as usize).saturating_sub(top_level_ixs_num))
                    .copied()
                    && caller_index != usize::MAX
                {
                    *parent_positions.get_mut(cpi_num).unwrap() = caller_index;
                    caller_index
                } else {
                    // This case shall never happen. Program runtime always executes caller before
                    // callees, so the if-statement can only be broken into two different cases:
                    // 1. Top-level instructions doing a CPI
                    // 2. A nested CPI.
                    debug_assert!(false);
                    usize::MAX
                };

                if let Some(inner_instructions) = outer_instructions.get_mut(outer_index) {
                    let stack_height = ix_in_trace.nesting_level.saturating_add(1);
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
        Self::fill_missing_sysvar_cache_entries_from_accounts(&mut sysvar_cache, callbacks);
    }

    pub fn reset_and_fill_sysvar_cache_entries<CB: TransactionProcessingCallback>(
        &self,
        callbacks: &CB,
    ) {
        let mut sysvar_cache = self.sysvar_cache.write().unwrap();
        sysvar_cache.reset();
        Self::fill_missing_sysvar_cache_entries_from_accounts(&mut sysvar_cache, callbacks);
    }

    fn fill_missing_sysvar_cache_entries_from_accounts<CB: TransactionProcessingCallback>(
        sysvar_cache: &mut SysvarCache,
        callbacks: &CB,
    ) {
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
        let entry = Arc::new(builtin);
        self.global_program_cache.write().unwrap().assign_program(
            &self.program_runtime_environment,
            program_id,
            0,
            Arc::clone(&entry),
        );
        self.builtin_program_cache
            .write()
            .unwrap()
            .replenish(program_id, entry);
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
                LoadedTransactionAccount, TRANSACTION_ACCOUNT_BASE_SIZE,
                ValidatedTransactionDetails,
            },
            nonce_info::NonceInfo,
            rent_calculator::RENT_EXEMPT_RENT_EPOCH,
            rollback_accounts::RollbackAccounts,
        },
        solana_account::{WritableAccount, create_account_shared_data_for_test},
        solana_clock::Clock,
        solana_compute_budget_interface::ComputeBudgetInstruction,
        solana_epoch_schedule::EpochSchedule,
        solana_fee_calculator::FeeCalculator,
        solana_fee_structure::FeeDetails,
        solana_hash::Hash,
        solana_message::{LegacyMessage, Message, MessageHeader, SanitizedMessage},
        solana_nonce as nonce,
        solana_program_runtime::{
            execution_budget::{
                SVMTransactionExecutionAndFeeBudgetLimits, SVMTransactionExecutionBudget,
            },
            invoke_context::BuiltinFunctionRegisterer,
            loaded_programs::BlockRelation,
            program_cache_entry::ProgramCacheEntryType,
        },
        solana_rent::Rent,
        solana_sbpf::vm,
        solana_sdk_ids::{bpf_loader, system_program, sysvar},
        solana_signature::Signature,
        solana_svm_callback::{AccountState, InvokeContextCallback},
        solana_system_interface::instruction as system_instruction,
        solana_transaction::sanitized::SanitizedTransaction,
        solana_transaction_context::transaction::TransactionContext,
        solana_transaction_error::TransactionError,
        std::{borrow::Cow, collections::HashMap},
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
            &get_mock_transaction_processing_environment(),
            &TransactionProcessingConfig::default(),
        );
    }

    #[test]
    fn test_inner_instructions_list_from_instruction_trace() {
        let mut transaction_context = TransactionContext::new(
            vec![(
                Pubkey::new_unique(),
                AccountSharedData::new(1, 1, &bpf_loader::ID),
            )],
            Rent::default(),
            4,
            11,
            4,
        );

        // Four top level instructions
        for i in 0..4 {
            transaction_context
                .configure_instruction_at_index(
                    i,
                    0,
                    vec![],
                    vec![u16::MAX; 256],
                    Cow::Owned(vec![i as u8]),
                    None,
                )
                .unwrap();
        }

        // Execute ix #0
        transaction_context.push().unwrap();
        // ix #0 does a CPI
        transaction_context
            .configure_next_cpi_for_tests(0, vec![], vec![0, 0])
            .unwrap();
        transaction_context.push().unwrap();
        // Returning from everything
        transaction_context.pop().unwrap();
        transaction_context.pop().unwrap();
        // Execute ix #1
        transaction_context.push().unwrap();
        transaction_context.pop().unwrap();
        // Execute ix #2
        transaction_context.push().unwrap();
        // ix #2 does a CPI
        transaction_context
            .configure_next_cpi_for_tests(0, vec![], vec![2, 0])
            .unwrap();
        transaction_context.push().unwrap();
        // A nested CPI
        transaction_context
            .configure_next_cpi_for_tests(0, vec![], vec![2, 1])
            .unwrap();
        transaction_context.push().unwrap();
        // Return from nested CPI
        transaction_context.pop().unwrap();
        // Return from CPI
        transaction_context.pop().unwrap();
        // ix #2 does another CPI
        transaction_context
            .configure_next_cpi_for_tests(0, vec![], vec![2, 2])
            .unwrap();
        transaction_context.push().unwrap();
        // Return from everything related to ix #2
        transaction_context.pop().unwrap();
        transaction_context.pop().unwrap();
        // Execute ix #3
        transaction_context.push().unwrap();
        // ix #3 does a CPI
        transaction_context
            .configure_next_cpi_for_tests(0, vec![], vec![3, 0])
            .unwrap();
        transaction_context.push().unwrap();
        // ix #3 does a nested CPI
        transaction_context
            .configure_next_cpi_for_tests(0, vec![], vec![3, 1])
            .unwrap();
        transaction_context.push().unwrap();
        // ix #3 does a second nested CPI
        transaction_context
            .configure_next_cpi_for_tests(0, vec![], vec![3, 2])
            .unwrap();
        transaction_context.push().unwrap();
        // Return from everything related to ix #3
        transaction_context.pop().unwrap();
        transaction_context.pop().unwrap();
        transaction_context.pop().unwrap();
        transaction_context.pop().unwrap();

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
                    instruction: CompiledInstruction::new_from_raw_parts(0, vec![0, 0], vec![]),
                    stack_height: 2,
                }],
                vec![],
                vec![
                    InnerInstruction {
                        instruction: CompiledInstruction::new_from_raw_parts(0, vec![2, 0], vec![]),
                        stack_height: 2,
                    },
                    InnerInstruction {
                        instruction: CompiledInstruction::new_from_raw_parts(0, vec![2, 1], vec![]),
                        stack_height: 3,
                    },
                    InnerInstruction {
                        instruction: CompiledInstruction::new_from_raw_parts(0, vec![2, 2], vec![]),
                        stack_height: 2,
                    },
                ],
                vec![
                    InnerInstruction {
                        instruction: CompiledInstruction::new_from_raw_parts(0, vec![3, 0], vec![]),
                        stack_height: 2,
                    },
                    InnerInstruction {
                        instruction: CompiledInstruction::new_from_raw_parts(0, vec![3, 1], vec![]),
                        stack_height: 3,
                    },
                    InnerInstruction {
                        instruction: CompiledInstruction::new_from_raw_parts(0, vec![3, 2], vec![]),
                        stack_height: 4,
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
            touched_flags: Box::default(),
            fee_details: FeeDetails::default(),
            rollback_accounts: RollbackAccounts::default(),
            compute_budget: SVMTransactionExecutionBudget::default(),
            loaded_accounts_data_size: 32,
        };

        let processing_environment = get_mock_transaction_processing_environment();

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
            touched_flags: Box::default(),
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
            &get_mock_transaction_processing_environment(),
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
            TransactionBatchProcessor::new(0, 0, Arc::downgrade(&fork_graph), None);
        let program_runtime_environment_for_execution =
            batch_processor.program_runtime_environment_for_epoch(0);
        let key = Pubkey::new_unique();

        let mut program_cache_for_tx_batch = ProgramCacheForTxBatch::new(batch_processor.slot);

        batch_processor.replenish_program_cache(
            &account_loader,
            vec![ProgramToLoad {
                program_id: &key,
                loader: ProgramCacheEntryOwner::LoaderV3,
                match_criteria: ProgramCacheMatchCriteria::NoCriteria,
                last_modification_slot: 0,
            }],
            &program_runtime_environment_for_execution,
            &mut program_cache_for_tx_batch,
            &mut ExecuteTimings::default(),
            true,
            true,
        );
    }

    #[test]
    fn test_replenish_program_cache() {
        let mock_bank = MockBankCallback::default();
        let fork_graph = Arc::new(RwLock::new(TestForkGraph {}));
        let batch_processor =
            TransactionBatchProcessor::new(0, 0, Arc::downgrade(&fork_graph), None);
        let program_runtime_environment_for_execution =
            batch_processor.program_runtime_environment_for_epoch(0);
        let key = Pubkey::new_unique();

        let mut account_data = AccountSharedData::default();
        account_data.set_owner(bpf_loader::id());
        mock_bank
            .account_shared_data
            .write()
            .unwrap()
            .insert(key, account_data);
        let account_loader = (&mock_bank).into();

        let mut loaded_missing = 0;
        for limit_to_load_programs in [false, true] {
            let mut program_cache_for_tx_batch = ProgramCacheForTxBatch::new(batch_processor.slot);

            batch_processor.replenish_program_cache(
                &account_loader,
                vec![ProgramToLoad {
                    program_id: &key,
                    loader: ProgramCacheEntryOwner::LoaderV2,
                    match_criteria: ProgramCacheMatchCriteria::NoCriteria,
                    last_modification_slot: 0,
                }],
                &program_runtime_environment_for_execution,
                &mut program_cache_for_tx_batch,
                &mut ExecuteTimings::default(),
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

        let rent = Rent::default();
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

        let rent = Rent::default();
        let rent_account = create_account_shared_data_for_test(&rent);
        mock_bank
            .account_shared_data
            .write()
            .unwrap()
            .insert(sysvar::rent::id(), rent_account);

        let transaction_processor = TransactionBatchProcessor::<TestForkGraph>::default();
        // Fill the sysvar cache
        transaction_processor.fill_missing_sysvar_cache_entries(&mock_bank);

        let updated_clock = Clock {
            slot: 6,
            epoch_start_timestamp: 7,
            epoch: 8,
            leader_schedule_epoch: 9,
            unix_timestamp: 10,
        };
        let updated_clock_account = create_account_shared_data_for_test(&updated_clock);
        mock_bank
            .account_shared_data
            .write()
            .unwrap()
            .insert(sysvar::clock::id(), updated_clock_account);
        transaction_processor.reset_and_fill_sysvar_cache_entries(&mock_bank);
        {
            let sysvar_cache = transaction_processor.sysvar_cache.read().unwrap();
            assert_eq!(
                sysvar_cache
                    .get_clock()
                    .expect("clock sysvar missing in cache"),
                updated_clock.clone().into()
            );
        }

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
            updated_clock.into()
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
            TransactionBatchProcessor::new(0, 0, Arc::downgrade(&fork_graph), None);

        let key = Pubkey::new_unique();
        let name = "a_builtin_name";
        let register_fn: BuiltinFunctionRegisterer = |p, n| {
            p.register_function(
                n,
                (
                    |_invoke_context, _param0, _param1, _param2, _param3, _param4| {},
                    |_| {},
                ),
            )
        };
        let program = ProgramCacheEntry::new_builtin(0, name.len(), register_fn);
        batch_processor.add_builtin(key, program);

        let mut loaded_programs_for_tx_batch = ProgramCacheForTxBatch::new(0);
        let program_runtime_environment =
            batch_processor.program_runtime_environment_for_epoch(batch_processor.epoch);
        batch_processor
            .global_program_cache
            .write()
            .unwrap()
            .extract(
                &mut vec![ProgramToLoad {
                    program_id: &key,
                    loader: ProgramCacheEntryOwner::NativeLoader,
                    match_criteria: ProgramCacheMatchCriteria::NoCriteria,
                    last_modification_slot: 0,
                }],
                &mut loaded_programs_for_tx_batch,
                &program_runtime_environment,
                true,
                true,
            );
        let entry = loaded_programs_for_tx_batch.find(&key).unwrap();

        // Repeating code because ProgramCacheEntry does not implement clone.
        let program = ProgramCacheEntry::new_builtin(0, name.len(), register_fn);
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
                lamports_per_signature,
                &rent,
                mock_bank.feature_set.relax_post_exec_min_balance_check,
                false,
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
            TransactionValidationResult::Loadable(ValidatedTransactionDetails {
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

    #[test_case(false; "strict_fee_payer")]
    #[test_case(true; "relaxed_fee_payer")]
    fn test_validate_transaction_fee_payer_not_found(relax_fee_payer_constraint: bool) {
        let lamports_per_signature = 5000;
        let message =
            new_unchecked_sanitized_message(Message::new(&[], Some(&Pubkey::new_unique())));

        let fee_and_limits = SVMTransactionExecutionAndFeeBudgetLimits::default();
        let mut mock_bank = MockBankCallback::default();
        mock_bank.feature_set.relax_fee_payer_constraint = relax_fee_payer_constraint;
        let mut account_loader = (&mock_bank).into();
        let mut error_counters = TransactionErrorMetrics::default();

        let expected_error = TransactionError::AccountNotFound;
        let expected_result = if relax_fee_payer_constraint {
            TransactionValidationResult::NoOp(NoOpTransaction {
                validation_error: expected_error,
                fee_payer_balance: None,
                compute_unit_limit: fee_and_limits.budget.compute_unit_limit,
                loaded_accounts_bytes_limit: fee_and_limits.loaded_accounts_data_size_limit,
            })
        } else {
            TransactionValidationResult::Unprocessable(expected_error)
        };

        let actual_result =
            TransactionBatchProcessor::<TestForkGraph>::validate_transaction_nonce_and_fee_payer(
                &mut account_loader,
                &message,
                CheckedTransactionDetails::new(None, fee_and_limits),
                &Hash::default(),
                lamports_per_signature,
                &Rent::default(),
                mock_bank.feature_set.relax_post_exec_min_balance_check,
                false,
                &mut error_counters,
            );

        assert_eq!(error_counters.account_not_found.0, 1);
        assert_eq!(actual_result, expected_result);
    }

    #[test_case(false; "strict_fee_payer")]
    #[test_case(true; "relaxed_fee_payer")]
    fn test_validate_transaction_fee_payer_insufficient_funds(relax_fee_payer_constraint: bool) {
        let lamports_per_signature = 5000;
        let fee_payer_balance = 1;
        let message =
            new_unchecked_sanitized_message(Message::new(&[], Some(&Pubkey::new_unique())));
        let fee_details =
            MockBankCallback::calculate_fee_details(&message, lamports_per_signature, 0);
        let fee_and_limits = SVMTransactionExecutionAndFeeBudgetLimits::with_fee(fee_details);
        let fee_payer_address = message.fee_payer();
        let fee_payer_account = AccountSharedData::new(fee_payer_balance, 0, &Pubkey::default());
        let mut mock_accounts = HashMap::new();
        mock_accounts.insert(*fee_payer_address, fee_payer_account);
        let mut mock_bank = MockBankCallback {
            account_shared_data: Arc::new(RwLock::new(mock_accounts)),
            ..Default::default()
        };
        mock_bank.feature_set.relax_fee_payer_constraint = relax_fee_payer_constraint;
        let mut account_loader = (&mock_bank).into();

        let mut error_counters = TransactionErrorMetrics::default();

        let expected_error = TransactionError::InsufficientFundsForFee;
        let expected_result = if relax_fee_payer_constraint {
            TransactionValidationResult::NoOp(NoOpTransaction {
                validation_error: expected_error,
                fee_payer_balance: Some(fee_payer_balance),
                compute_unit_limit: fee_and_limits.budget.compute_unit_limit,
                loaded_accounts_bytes_limit: fee_and_limits.loaded_accounts_data_size_limit,
            })
        } else {
            TransactionValidationResult::Unprocessable(expected_error)
        };

        let actual_result =
            TransactionBatchProcessor::<TestForkGraph>::validate_transaction_nonce_and_fee_payer(
                &mut account_loader,
                &message,
                CheckedTransactionDetails::new(None, fee_and_limits),
                &Hash::default(),
                lamports_per_signature,
                &Rent::default(),
                mock_bank.feature_set.relax_post_exec_min_balance_check,
                false,
                &mut error_counters,
            );

        assert_eq!(error_counters.insufficient_funds.0, 1);
        assert_eq!(actual_result, expected_result);
    }

    #[test_case(false; "strict_fee_payer")]
    #[test_case(true; "relaxed_fee_payer")]
    fn test_validate_transaction_fee_payer_insufficient_rent(relax_fee_payer_constraint: bool) {
        let lamports_per_signature = 5000;
        let message =
            new_unchecked_sanitized_message(Message::new(&[], Some(&Pubkey::new_unique())));
        let fee_details =
            MockBankCallback::calculate_fee_details(&message, lamports_per_signature, 0);
        let fee_and_limits = SVMTransactionExecutionAndFeeBudgetLimits::with_fee(fee_details);
        let fee_payer_address = message.fee_payer();
        let transaction_fee = lamports_per_signature;
        let rent = Rent::default();
        let min_balance = rent.minimum_balance(0);
        let starting_balance = min_balance + transaction_fee - 1;
        let fee_payer_account = AccountSharedData::new(starting_balance, 0, &Pubkey::default());
        let mut mock_accounts = HashMap::new();
        mock_accounts.insert(*fee_payer_address, fee_payer_account);
        let mut mock_bank = MockBankCallback {
            account_shared_data: Arc::new(RwLock::new(mock_accounts)),
            ..Default::default()
        };
        mock_bank.feature_set.relax_fee_payer_constraint = relax_fee_payer_constraint;
        let mut account_loader = (&mock_bank).into();

        let mut error_counters = TransactionErrorMetrics::default();

        let expected_error = TransactionError::InsufficientFundsForRent { account_index: 0 };
        let expected_result = if relax_fee_payer_constraint {
            TransactionValidationResult::NoOp(NoOpTransaction {
                validation_error: expected_error,
                fee_payer_balance: Some(starting_balance),
                compute_unit_limit: fee_and_limits.budget.compute_unit_limit,
                loaded_accounts_bytes_limit: fee_and_limits.loaded_accounts_data_size_limit,
            })
        } else {
            TransactionValidationResult::Unprocessable(expected_error)
        };

        let actual_result =
            TransactionBatchProcessor::<TestForkGraph>::validate_transaction_nonce_and_fee_payer(
                &mut account_loader,
                &message,
                CheckedTransactionDetails::new(None, fee_and_limits),
                &Hash::default(),
                lamports_per_signature,
                &rent,
                mock_bank.feature_set.relax_post_exec_min_balance_check,
                false,
                &mut error_counters,
            );

        assert_eq!(actual_result, expected_result);
    }

    #[test_case(false; "strict_fee_payer")]
    #[test_case(true; "relaxed_fee_payer")]
    fn test_validate_transaction_fee_payer_invalid(relax_fee_payer_constraint: bool) {
        let lamports_per_signature = 5000;
        let fee_payer_balance = 1_000_000;
        let message =
            new_unchecked_sanitized_message(Message::new(&[], Some(&Pubkey::new_unique())));
        let fee_details =
            MockBankCallback::calculate_fee_details(&message, lamports_per_signature, 0);
        let fee_and_limits = SVMTransactionExecutionAndFeeBudgetLimits::with_fee(fee_details);
        let fee_payer_address = message.fee_payer();
        let fee_payer_account = AccountSharedData::new(fee_payer_balance, 0, &Pubkey::new_unique());
        let mut mock_accounts = HashMap::new();
        mock_accounts.insert(*fee_payer_address, fee_payer_account);
        let mut mock_bank = MockBankCallback {
            account_shared_data: Arc::new(RwLock::new(mock_accounts)),
            ..Default::default()
        };
        mock_bank.feature_set.relax_fee_payer_constraint = relax_fee_payer_constraint;
        let mut account_loader = (&mock_bank).into();

        let mut error_counters = TransactionErrorMetrics::default();

        let expected_error = TransactionError::InvalidAccountForFee;
        let expected_result = if relax_fee_payer_constraint {
            TransactionValidationResult::NoOp(NoOpTransaction {
                validation_error: expected_error,
                fee_payer_balance: Some(fee_payer_balance),
                compute_unit_limit: fee_and_limits.budget.compute_unit_limit,
                loaded_accounts_bytes_limit: fee_and_limits.loaded_accounts_data_size_limit,
            })
        } else {
            TransactionValidationResult::Unprocessable(expected_error)
        };

        let actual_result =
            TransactionBatchProcessor::<TestForkGraph>::validate_transaction_nonce_and_fee_payer(
                &mut account_loader,
                &message,
                CheckedTransactionDetails::new(None, fee_and_limits),
                &Hash::default(),
                lamports_per_signature,
                &Rent::default(),
                mock_bank.feature_set.relax_post_exec_min_balance_check,
                false,
                &mut error_counters,
            );

        assert_eq!(error_counters.invalid_account_for_fee.0, 1);
        assert_eq!(actual_result, expected_result);
    }

    #[derive(Debug, PartialEq, Eq)]
    enum ValidateNonce {
        Success,
        NoAccount,
        BadOwner,
        BlockhashMismatch,
        AlreadyUsed,
        BadSigner,
    }

    #[test_case(ValidateNonce::Success)]
    #[test_case(ValidateNonce::NoAccount)]
    #[test_case(ValidateNonce::BadOwner)]
    #[test_case(ValidateNonce::BlockhashMismatch)]
    #[test_case(ValidateNonce::AlreadyUsed)]
    #[test_case(ValidateNonce::BadSigner)]
    fn test_validate_transaction_nonce(case: ValidateNonce) {
        let lamports_per_signature = 5000;
        let previous_durable_nonce = DurableNonce::from_blockhash(&Hash::new_unique());
        let nonce_address = Pubkey::new_unique();
        let authority_address = Pubkey::new_unique();

        let message_blockhash = if case == ValidateNonce::BlockhashMismatch {
            Hash::new_unique()
        } else {
            *previous_durable_nonce.as_hash()
        };

        let message_authority = if case == ValidateNonce::BadSigner {
            Pubkey::new_unique()
        } else {
            authority_address
        };

        let message = new_unchecked_sanitized_message(Message::new_with_blockhash(
            &[system_instruction::advance_nonce_account(
                &nonce_address,
                &message_authority,
            )],
            Some(&Pubkey::new_unique()),
            &message_blockhash,
        ));

        let environment_blockhash = Hash::new_unique();
        let next_durable_nonce = DurableNonce::from_blockhash(&environment_blockhash);

        let stored_durable_nonce = if case == ValidateNonce::AlreadyUsed {
            next_durable_nonce
        } else {
            previous_durable_nonce
        };

        let nonce_versions = nonce::versions::Versions::new(nonce::state::State::Initialized(
            nonce::state::Data::new(
                authority_address,
                stored_durable_nonce,
                lamports_per_signature,
            ),
        ));

        let nonce_owner = if case == ValidateNonce::BadOwner {
            Pubkey::new_unique()
        } else {
            system_program::id()
        };

        let nonce_account = AccountSharedData::new_data(1, &nonce_versions, &nonce_owner).unwrap();

        let mut mock_accounts = HashMap::new();

        if case != ValidateNonce::NoAccount {
            mock_accounts.insert(nonce_address, nonce_account.clone());
        }

        let mock_bank = MockBankCallback {
            account_shared_data: Arc::new(RwLock::new(mock_accounts)),
            ..Default::default()
        };
        let mut account_loader = (&mock_bank).into();

        let mut error_counters = TransactionErrorMetrics::default();
        let result = TransactionBatchProcessor::<TestForkGraph>::validate_transaction_nonce(
            &mut account_loader,
            &message,
            &nonce_address,
            &next_durable_nonce,
            lamports_per_signature,
            false,
            &mut error_counters,
        );

        match case {
            ValidateNonce::Success => {
                let mut future_nonce_info = NonceInfo::new(nonce_address, nonce_account);
                future_nonce_info
                    .try_advance_nonce(next_durable_nonce, lamports_per_signature)
                    .unwrap();

                assert_eq!(result, Ok(future_nonce_info));
            }
            ValidateNonce::NoAccount => {
                assert_eq!(error_counters.account_not_found.0, 1);
                assert_eq!(result, Err(TransactionError::AccountNotFound));
            }
            _ => {
                assert_eq!(error_counters.blockhash_not_found.0, 1);
                assert_eq!(result, Err(TransactionError::BlockhashNotFound));
            }
        }
    }

    #[test_case(false; "strict_fee_payer")]
    #[test_case(true; "relaxed_fee_payer")]
    fn test_validate_transaction_fee_payer_is_nonce(relax_fee_payer_constraint: bool) {
        let lamports_per_signature = 5000;
        let rent = Rent::default();
        let compute_unit_limit = 1000u64;
        let previous_durable_nonce = DurableNonce::from_blockhash(&Hash::new_unique());
        let fee_payer_address = &Pubkey::new_unique();
        let message = new_unchecked_sanitized_message(Message::new_with_blockhash(
            &[
                system_instruction::advance_nonce_account(fee_payer_address, fee_payer_address),
                ComputeBudgetInstruction::set_compute_unit_limit(compute_unit_limit as u32),
                ComputeBudgetInstruction::set_compute_unit_price(1_000_000),
            ],
            Some(fee_payer_address),
            previous_durable_nonce.as_hash(),
        ));
        let transaction_fee = lamports_per_signature;
        let compute_budget_and_limits = SVMTransactionExecutionAndFeeBudgetLimits {
            fee_details: FeeDetails::new(transaction_fee, compute_unit_limit),
            ..SVMTransactionExecutionAndFeeBudgetLimits::default()
        };
        let min_balance = Rent::default().minimum_balance(nonce::state::State::size());
        let priority_fee = compute_unit_limit;

        let nonce_versions = nonce::versions::Versions::new(nonce::state::State::Initialized(
            nonce::state::Data::new(
                *fee_payer_address,
                previous_durable_nonce,
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
            let mut mock_bank = MockBankCallback {
                account_shared_data: Arc::new(RwLock::new(mock_accounts)),
                ..Default::default()
            };
            mock_bank.feature_set.relax_fee_payer_constraint = relax_fee_payer_constraint;
            let mut account_loader = (&mock_bank).into();

            let mut error_counters = TransactionErrorMetrics::default();

            let tx_details =
                CheckedTransactionDetails::new(Some(*fee_payer_address), compute_budget_and_limits);

            let result = TransactionBatchProcessor::<TestForkGraph>::validate_transaction_nonce_and_fee_payer(
                &mut account_loader,
                &message,
                tx_details,
                &environment_blockhash,
                lamports_per_signature,
                &rent,
                mock_bank.feature_set.relax_post_exec_min_balance_check,
                false,
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
                TransactionValidationResult::Loadable(ValidatedTransactionDetails {
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

            let mut mock_accounts = HashMap::new();
            mock_accounts.insert(*fee_payer_address, fee_payer_account);
            let mut mock_bank = MockBankCallback {
                account_shared_data: Arc::new(RwLock::new(mock_accounts)),
                ..Default::default()
            };
            mock_bank.feature_set.relax_fee_payer_constraint = relax_fee_payer_constraint;
            let mut account_loader = (&mock_bank).into();

            let mut error_counters = TransactionErrorMetrics::default();

            let tx_details =
                CheckedTransactionDetails::new(Some(*fee_payer_address), compute_budget_and_limits);

            let result = TransactionBatchProcessor::<TestForkGraph>::validate_transaction_nonce_and_fee_payer(
                &mut account_loader,
                &message,
                tx_details,
                &environment_blockhash,
                lamports_per_signature,
                &rent,
                mock_bank.feature_set.relax_post_exec_min_balance_check,
                false,
                &mut error_counters,
            );

            // nonce transactions are never processable under SIMD-0290 rules
            assert_eq!(error_counters.insufficient_funds.0, 1);
            assert_eq!(
                result,
                TransactionValidationResult::Unprocessable(
                    TransactionError::InsufficientFundsForFee
                )
            );
        }
    }

    // Ensure `TransactionProcessingCallback::inspect_account()` is called when
    // validating the fee payer, since that's when the fee payer account is loaded.
    #[test]
    fn test_inspect_account_fee_payer() {
        let lamports_per_signature = 5000;
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

        assert!(matches!(
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
                lamports_per_signature,
                &Rent::default(),
                mock_bank.feature_set.relax_post_exec_min_balance_check,
                false,
                &mut TransactionErrorMetrics::default(),
            ),
            TransactionValidationResult::Loadable(_)
        ));

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

    #[test]
    fn test_set_program_runtime_environment() {
        let mut transaction_processor = TransactionBatchProcessor::<TestForkGraph>::default();
        let current_environment =
            ProgramRuntimeEnvironment::clone(&transaction_processor.program_runtime_environment);
        let new_environment = ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock());
        let config = vm::Config {
            enable_symbol_and_section_labels: true,
            ..vm::Config::default()
        };
        let new_environment2 = ProgramRuntimeEnvironment::from(BuiltinProgram::new_loader(config));
        assert_ne!(current_environment, new_environment);
        assert_ne!(current_environment, new_environment2);
        assert_ne!(new_environment, new_environment2);
        // Assign an equal and identical environment: No changes
        transaction_processor.set_program_runtime_environment(ProgramRuntimeEnvironment::clone(
            &current_environment,
        ));
        assert_eq!(
            transaction_processor.program_runtime_environment,
            current_environment,
        );
        // Assign an equal but not identical environment: No changes
        transaction_processor
            .set_program_runtime_environment(ProgramRuntimeEnvironment::clone(&new_environment));
        assert_eq!(
            transaction_processor.program_runtime_environment,
            current_environment,
        );
        // Assign a different and not identical environment: Overwritten
        transaction_processor
            .set_program_runtime_environment(ProgramRuntimeEnvironment::clone(&new_environment2));
        assert_eq!(
            transaction_processor.program_runtime_environment,
            new_environment2,
        );
        // Assign an environment which is equal to the upcoming_environment: Overwritten
        transaction_processor
            .epoch_boundary_preparation
            .write()
            .unwrap()
            .upcoming_environment = Some(ProgramRuntimeEnvironment::clone(&new_environment));
        transaction_processor.set_program_runtime_environment(ProgramRuntimeEnvironment::clone(
            &current_environment,
        ));
        assert_eq!(
            transaction_processor.program_runtime_environment,
            new_environment,
        );
    }
}
