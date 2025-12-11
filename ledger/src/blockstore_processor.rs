use {
    crate::{
        block_error::BlockError,
        blockstore::{Blockstore, BlockstoreError},
        blockstore_meta::SlotMeta,
        entry_notifier_service::{EntryNotification, EntryNotifierSender},
        leader_schedule_cache::LeaderScheduleCache,
        transaction_balances::compile_collected_balances,
        use_snapshot_archives_at_startup::UseSnapshotArchivesAtStartup,
    },
    agave_snapshots::snapshot_config::SnapshotConfig,
    chrono_humanize::{Accuracy, HumanTime, Tense},
    crossbeam_channel::Sender,
    itertools::Itertools,
    log::*,
    rayon::{prelude::*, ThreadPool},
    scopeguard::defer,
    solana_accounts_db::{
        accounts_db::AccountsDbConfig, accounts_update_notifier_interface::AccountsUpdateNotifier,
    },
    solana_clock::{Slot, MAX_PROCESSING_AGE},
    solana_cost_model::{cost_model::CostModel, transaction_cost::TransactionCost},
    solana_entry::entry::{self, create_ticks, Entry, EntrySlice, EntryType},
    solana_genesis_config::GenesisConfig,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_measure::{measure::Measure, measure_us},
    solana_metrics::datapoint_error,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::{Bank, PreCommitResult, TransactionBalancesSet},
        bank_forks::BankForks,
        bank_utils,
        commitment::VOTE_THRESHOLD_SIZE,
        dependency_tracker::DependencyTracker,
        installed_scheduler_pool::BankWithScheduler,
        prioritization_fee_cache::PrioritizationFeeCache,
        runtime_config::RuntimeConfig,
        snapshot_controller::SnapshotController,
        transaction_batch::{OwnedOrBorrowed, TransactionBatch},
        vote_sender_types::ReplayVoteSender,
    },
    solana_runtime_transaction::{
        runtime_transaction::RuntimeTransaction, transaction_with_meta::TransactionWithMeta,
    },
    solana_signature::Signature,
    solana_svm::{
        transaction_commit_result::{TransactionCommitResult, TransactionCommitResultExtensions},
        transaction_processing_result::ProcessedTransaction,
        transaction_processor::ExecutionRecordingConfig,
    },
    solana_svm_timings::{report_execute_timings, ExecuteTimingType, ExecuteTimings},
    solana_svm_transaction::{svm_message::SVMMessage, svm_transaction::SVMTransaction},
    solana_transaction::{
        sanitized::SanitizedTransaction, versioned::VersionedTransaction,
        TransactionVerificationMode,
    },
    solana_transaction_error::{TransactionError, TransactionResult as Result},
    solana_transaction_status::token_balances::TransactionTokenBalancesSet,
    solana_vote::vote_account::VoteAccountsHashMap,
    std::{
        borrow::Cow,
        collections::{HashMap, HashSet},
        num::Saturating,
        ops::Index,
        path::PathBuf,
        result,
        sync::{atomic::AtomicBool, Arc, Mutex, RwLock},
        time::{Duration, Instant},
        vec::Drain,
    },
    thiserror::Error,
    ExecuteTimingType::{NumExecuteBatches, TotalBatchesLen},
};
#[cfg(feature = "dev-context-only-utils")]
use {qualifier_attr::qualifiers, solana_runtime::bank::HashOverrides};

pub struct TransactionBatchWithIndexes<'a, 'b, Tx: SVMMessage> {
    pub batch: TransactionBatch<'a, 'b, Tx>,
    pub transaction_indexes: Vec<usize>,
}

// `TransactionBatchWithIndexes` but without the `Drop` that prevents
// us from nicely unwinding these with manual unlocking.
pub struct LockedTransactionsWithIndexes<Tx: SVMMessage> {
    lock_results: Vec<Result<()>>,
    transactions: Vec<RuntimeTransaction<Tx>>,
    starting_index: usize,
}

struct ReplayEntry {
    entry: EntryType<RuntimeTransaction<SanitizedTransaction>>,
    starting_index: usize,
}

fn first_err(results: &[Result<()>]) -> Result<()> {
    for r in results {
        if r.is_err() {
            return r.clone();
        }
    }
    Ok(())
}

// Includes transaction signature for unit-testing
fn do_get_first_error<T, Tx: SVMTransaction>(
    batch: &TransactionBatch<Tx>,
    results: &[Result<T>],
) -> Option<(Result<()>, Signature)> {
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

fn get_first_error<T, Tx: SVMTransaction>(
    batch: &TransactionBatch<Tx>,
    commit_results: &[Result<T>],
) -> Result<()> {
    do_get_first_error(batch, commit_results)
        .map(|(error, _signature)| error)
        .unwrap_or(Ok(()))
}

fn create_thread_pool(num_threads: usize) -> ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(num_threads)
        .thread_name(|i| format!("solReplayTx{i:02}"))
        .build()
        .expect("new rayon threadpool")
}

pub fn execute_batch<'a>(
    batch: &'a TransactionBatchWithIndexes<impl TransactionWithMeta>,
    bank: &'a Arc<Bank>,
    transaction_status_sender: Option<&'a TransactionStatusSender>,
    replay_vote_sender: Option<&'a ReplayVoteSender>,
    timings: &'a mut ExecuteTimings,
    log_messages_bytes_limit: Option<usize>,
    prioritization_fee_cache: Option<&'a PrioritizationFeeCache>,
    extra_pre_commit_callback: Option<
        impl FnOnce(&Result<ProcessedTransaction>) -> Result<Option<usize>>,
    >,
) -> Result<()> {
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
            MAX_PROCESSING_AGE,
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

fn check_block_cost_limits<Tx: TransactionWithMeta>(
    bank: &Bank,
    tx_costs: &[Option<TransactionCost<'_, Tx>>],
) -> Result<()> {
    let mut cost_tracker = bank.write_cost_tracker().unwrap();
    for tx_cost in tx_costs.iter().flatten() {
        cost_tracker
            .try_add(tx_cost)
            .map_err(TransactionError::from)?;
    }

    Ok(())
}

#[derive(Default)]
pub struct ExecuteBatchesInternalMetrics {
    execution_timings_per_thread: HashMap<usize, ThreadExecuteTimings>,
    total_batches_len: u64,
    execute_batches_us: u64,
}

impl ExecuteBatchesInternalMetrics {
    pub fn new_with_timings_from_all_threads(execute_timings: ExecuteTimings) -> Self {
        const DUMMY_THREAD_INDEX: usize = 999;
        let mut new = Self::default();
        new.execution_timings_per_thread.insert(
            DUMMY_THREAD_INDEX,
            ThreadExecuteTimings {
                execute_timings,
                ..ThreadExecuteTimings::default()
            },
        );
        new
    }
}

fn execute_batches_internal(
    bank: &Arc<Bank>,
    replay_tx_thread_pool: &ThreadPool,
    batches: &[TransactionBatchWithIndexes<RuntimeTransaction<SanitizedTransaction>>],
    transaction_status_sender: Option<&TransactionStatusSender>,
    replay_vote_sender: Option<&ReplayVoteSender>,
    log_messages_bytes_limit: Option<usize>,
    prioritization_fee_cache: Option<&PrioritizationFeeCache>,
) -> Result<ExecuteBatchesInternalMetrics> {
    assert!(!batches.is_empty());
    let execution_timings_per_thread: Mutex<HashMap<usize, ThreadExecuteTimings>> =
        Mutex::new(HashMap::new());

    let mut execute_batches_elapsed = Measure::start("execute_batches_elapsed");
    let results: Vec<Result<()>> = replay_tx_thread_pool.install(|| {
        batches
            .into_par_iter()
            .map(|transaction_batch| {
                let transaction_count =
                    transaction_batch.batch.sanitized_transactions().len() as u64;
                let mut timings = ExecuteTimings::default();
                let (result, execute_batches_us) = measure_us!(execute_batch(
                    transaction_batch,
                    bank,
                    transaction_status_sender,
                    replay_vote_sender,
                    &mut timings,
                    log_messages_bytes_limit,
                    prioritization_fee_cache,
                    None::<fn(&_) -> _>,
                ));

                let thread_index = replay_tx_thread_pool.current_thread_index().unwrap();
                execution_timings_per_thread
                    .lock()
                    .unwrap()
                    .entry(thread_index)
                    .and_modify(|thread_execution_time| {
                        let ThreadExecuteTimings {
                            total_thread_us,
                            total_transactions_executed,
                            execute_timings: total_thread_execute_timings,
                        } = thread_execution_time;
                        *total_thread_us += execute_batches_us;
                        *total_transactions_executed += transaction_count;
                        total_thread_execute_timings
                            .saturating_add_in_place(ExecuteTimingType::TotalBatchesLen, 1);
                        total_thread_execute_timings.accumulate(&timings);
                    })
                    .or_insert(ThreadExecuteTimings {
                        total_thread_us: Saturating(execute_batches_us),
                        total_transactions_executed: Saturating(transaction_count),
                        execute_timings: timings,
                    });
                result
            })
            .collect()
    });
    execute_batches_elapsed.stop();

    first_err(&results)?;

    Ok(ExecuteBatchesInternalMetrics {
        execution_timings_per_thread: execution_timings_per_thread.into_inner().unwrap(),
        total_batches_len: batches.len() as u64,
        execute_batches_us: execute_batches_elapsed.as_us(),
    })
}

// This fn diverts the code-path into two variants. Both must provide exactly the same set of
// validations. For this reason, this fn is deliberately inserted into the code path to be called
// inside process_entries(), so that Bank::prepare_sanitized_batch() has been called on all of
// batches already, while minimizing code duplication (thus divergent behavior risk) at the cost of
// acceptable overhead of meaningless buffering of batches for the scheduler variant.
//
// Also note that the scheduler variant can't implement the batch-level sanitization naively, due
// to the nature of individual tx processing. That's another reason of this particular placement of
// divergent point in the code-path (i.e. not one layer up with its own prepare_sanitized_batch()
// invocation).
fn process_batches(
    bank: &BankWithScheduler,
    replay_tx_thread_pool: &ThreadPool,
    locked_entries: impl ExactSizeIterator<Item = LockedTransactionsWithIndexes<SanitizedTransaction>>,
    transaction_status_sender: Option<&TransactionStatusSender>,
    replay_vote_sender: Option<&ReplayVoteSender>,
    batch_execution_timing: &mut BatchExecutionTiming,
    log_messages_bytes_limit: Option<usize>,
    prioritization_fee_cache: Option<&PrioritizationFeeCache>,
) -> Result<()> {
    if bank.has_installed_scheduler() {
        debug!(
            "process_batches()/schedule_batches_for_execution({} batches)",
            locked_entries.len()
        );
        // Scheduling usually succeeds (immediately returns `Ok(())`) here without being blocked on
        // the actual transaction executions.
        //
        // As an exception, this code path could propagate the transaction execution _errors of
        // previously-scheduled transactions_ to notify the replay stage. Then, the replay stage
        // will bail out the further processing of the malformed (possibly malicious) block
        // immediately, not to waste any system resources. Note that this propagation is of early
        // hints. Even if errors won't be propagated in this way, they are guaranteed to be
        // propagated eventually via the blocking fn called
        // BankWithScheduler::wait_for_completed_scheduler().
        //
        // To recite, the returned error is completely unrelated to the argument's `locked_entries`
        // at the hand. While being awkward, the _async_ unified scheduler is abusing this existing
        // error propagation code path to the replay stage for compatibility and ease of
        // integration, exploiting the fact that the replay stage doesn't care _which transaction
        // the returned error is originating from_.
        //
        // In the future, more proper error propagation mechanism will be introduced once after we
        // fully transition to the unified scheduler for the block verification. That one would be
        // a push based one from the unified scheduler to the replay stage to eliminate the current
        // overhead: 1 read lock per batch in
        // `BankWithScheduler::schedule_transaction_executions()`.
        schedule_batches_for_execution(bank, locked_entries)
    } else {
        debug!(
            "process_batches()/execute_batches({} batches)",
            locked_entries.len()
        );
        execute_batches(
            bank,
            replay_tx_thread_pool,
            locked_entries,
            transaction_status_sender,
            replay_vote_sender,
            batch_execution_timing,
            log_messages_bytes_limit,
            prioritization_fee_cache,
        )
    }
}

fn schedule_batches_for_execution(
    bank: &BankWithScheduler,
    locked_entries: impl Iterator<Item = LockedTransactionsWithIndexes<SanitizedTransaction>>,
) -> Result<()> {
    // Track the first error encountered in the loop below, if any.
    // This error will be propagated to the replay stage, or Ok(()).
    let mut first_err = Ok(());

    for LockedTransactionsWithIndexes {
        lock_results,
        transactions,
        starting_index,
    } in locked_entries
    {
        // unlock before sending to scheduler.
        bank.unlock_accounts(transactions.iter().zip(lock_results.iter()));
        // give ownership to scheduler. capture the first error, but continue the loop
        // to unlock.
        // scheduling is skipped if we have already detected an error in this loop
        let indexes = starting_index..starting_index + transactions.len();
        // Widening usize index to OrderedTaskId (= u128) won't ever fail.
        let task_ids = indexes.map(|i| i.try_into().unwrap());
        first_err = first_err.and_then(|()| {
            bank.schedule_transaction_executions(transactions.into_iter().zip_eq(task_ids))
        });
    }
    first_err
}

fn execute_batches(
    bank: &Arc<Bank>,
    replay_tx_thread_pool: &ThreadPool,
    locked_entries: impl ExactSizeIterator<Item = LockedTransactionsWithIndexes<SanitizedTransaction>>,
    transaction_status_sender: Option<&TransactionStatusSender>,
    replay_vote_sender: Option<&ReplayVoteSender>,
    timing: &mut BatchExecutionTiming,
    log_messages_bytes_limit: Option<usize>,
    prioritization_fee_cache: Option<&PrioritizationFeeCache>,
) -> Result<()> {
    if locked_entries.len() == 0 {
        return Ok(());
    }

    let tx_batches: Vec<_> = locked_entries
        .into_iter()
        .map(
            |LockedTransactionsWithIndexes {
                 lock_results,
                 transactions,
                 starting_index,
             }| {
                let ending_index = starting_index + transactions.len();
                TransactionBatchWithIndexes {
                    batch: TransactionBatch::new(
                        lock_results,
                        bank,
                        OwnedOrBorrowed::Owned(transactions),
                    ),
                    transaction_indexes: (starting_index..ending_index).collect(),
                }
            },
        )
        .collect();

    let execute_batches_internal_metrics = execute_batches_internal(
        bank,
        replay_tx_thread_pool,
        &tx_batches,
        transaction_status_sender,
        replay_vote_sender,
        log_messages_bytes_limit,
        prioritization_fee_cache,
    )?;

    // Pass false because this code-path is never touched by unified scheduler.
    timing.accumulate(execute_batches_internal_metrics, false);
    Ok(())
}

/// Process an ordered list of entries in parallel
/// 1. In order lock accounts for each entry while the lock succeeds, up to a Tick entry
/// 2. Process the locked group in parallel
/// 3. Register the `Tick` if it's available
/// 4. Update the leader scheduler, goto 1
///
/// This method is for use testing against a single Bank, and assumes `Bank::transaction_count()`
/// represents the number of transactions executed in this Bank
pub fn process_entries_for_tests(
    bank: &BankWithScheduler,
    entries: Vec<Entry>,
    transaction_status_sender: Option<&TransactionStatusSender>,
    replay_vote_sender: Option<&ReplayVoteSender>,
) -> Result<()> {
    let replay_tx_thread_pool = create_thread_pool(1);
    let verify_transaction = {
        let bank = bank.clone_with_scheduler();
        move |versioned_tx: VersionedTransaction| -> Result<RuntimeTransaction<SanitizedTransaction>> {
            bank.verify_transaction(versioned_tx, TransactionVerificationMode::FullVerification)
        }
    };

    let mut entry_starting_index: usize = bank.transaction_count().try_into().unwrap();
    let mut batch_timing = BatchExecutionTiming::default();
    let replay_entries: Vec<_> = entry::verify_transactions(
        entries,
        &replay_tx_thread_pool,
        Arc::new(verify_transaction),
    )?
    .into_iter()
    .map(|entry| {
        let starting_index = entry_starting_index;
        if let EntryType::Transactions(ref transactions) = entry {
            entry_starting_index = entry_starting_index.saturating_add(transactions.len());
        }
        ReplayEntry {
            entry,
            starting_index,
        }
    })
    .collect();

    let result = process_entries(
        bank,
        &replay_tx_thread_pool,
        replay_entries,
        transaction_status_sender,
        replay_vote_sender,
        &mut batch_timing,
        None,
        None,
    );

    debug!("process_entries: {batch_timing:?}");
    result
}

fn process_entries(
    bank: &BankWithScheduler,
    replay_tx_thread_pool: &ThreadPool,
    entries: Vec<ReplayEntry>,
    transaction_status_sender: Option<&TransactionStatusSender>,
    replay_vote_sender: Option<&ReplayVoteSender>,
    batch_timing: &mut BatchExecutionTiming,
    log_messages_bytes_limit: Option<usize>,
    prioritization_fee_cache: Option<&PrioritizationFeeCache>,
) -> Result<()> {
    // accumulator for entries that can be processed in parallel
    let mut batches = vec![];
    let mut tick_hashes = vec![];

    for ReplayEntry {
        entry,
        starting_index,
    } in entries
    {
        match entry {
            EntryType::Tick(hash) => {
                // If it's a tick, save it for later
                tick_hashes.push(hash);
                if bank.is_block_boundary(bank.tick_height() + tick_hashes.len() as u64) {
                    // If it's a tick that will cause a new blockhash to be created,
                    // execute the group and register the tick
                    process_batches(
                        bank,
                        replay_tx_thread_pool,
                        batches.drain(..),
                        transaction_status_sender,
                        replay_vote_sender,
                        batch_timing,
                        log_messages_bytes_limit,
                        prioritization_fee_cache,
                    )?;
                    for hash in tick_hashes.drain(..) {
                        bank.register_tick(&hash);
                    }
                }
            }
            EntryType::Transactions(transactions) => {
                queue_batches_with_lock_retry(
                    bank,
                    starting_index,
                    transactions,
                    &mut batches,
                    |batches| {
                        process_batches(
                            bank,
                            replay_tx_thread_pool,
                            batches,
                            transaction_status_sender,
                            replay_vote_sender,
                            batch_timing,
                            log_messages_bytes_limit,
                            prioritization_fee_cache,
                        )
                    },
                )?;
            }
        }
    }
    process_batches(
        bank,
        replay_tx_thread_pool,
        batches.into_iter(),
        transaction_status_sender,
        replay_vote_sender,
        batch_timing,
        log_messages_bytes_limit,
        prioritization_fee_cache,
    )?;
    for hash in tick_hashes {
        bank.register_tick(&hash);
    }
    Ok(())
}

/// If an entry can be locked without failure, the transactions are pushed
/// as a batch to `batches`. If the lock fails, the transactions are unlocked
/// and the batches are processed.
/// The locking process is retried, and if it fails again the block is marked
/// as dead.
/// If the lock retry succeeds, then the batch is pushed into `batches`.
fn queue_batches_with_lock_retry(
    bank: &Bank,
    starting_index: usize,
    transactions: Vec<RuntimeTransaction<SanitizedTransaction>>,
    batches: &mut Vec<LockedTransactionsWithIndexes<SanitizedTransaction>>,
    mut process_batches: impl FnMut(
        Drain<LockedTransactionsWithIndexes<SanitizedTransaction>>,
    ) -> Result<()>,
) -> Result<()> {
    // try to lock the accounts
    let lock_results = bank.try_lock_accounts(&transactions);
    let first_lock_err = first_err(&lock_results);
    if first_lock_err.is_ok() {
        batches.push(LockedTransactionsWithIndexes {
            lock_results,
            transactions,
            starting_index,
        });
        return Ok(());
    }

    // We need to unlock the transactions that succeeded to lock before the
    // retry.
    bank.unlock_accounts(transactions.iter().zip(lock_results.iter()));

    // We failed to lock, there are 2 possible reasons:
    // 1. A batch already in `batches` holds the lock.
    // 2. The batch is "self-conflicting" (i.e. the batch has account lock conflicts with itself)

    // Use the callback to process batches, and clear them.
    // Clearing the batches will `Drop` the batches which will unlock the accounts.
    process_batches(batches.drain(..))?;

    // Retry the lock
    let lock_results = bank.try_lock_accounts(&transactions);
    match first_err(&lock_results) {
        Ok(()) => {
            batches.push(LockedTransactionsWithIndexes {
                lock_results,
                transactions,
                starting_index,
            });
            Ok(())
        }
        Err(err) => {
            // We still may have succeeded to lock some accounts, unlock them.
            bank.unlock_accounts(transactions.iter().zip(lock_results.iter()));

            // An entry has account lock conflicts with *itself*, which should not happen
            // if generated by a properly functioning leader
            datapoint_error!(
                "validator_process_entry_error",
                (
                    "error",
                    format!(
                        "Lock accounts error, entry conflicts with itself, txs: {transactions:?}"
                    ),
                    String
                )
            );
            Err(err)
        }
    }
}

#[derive(Error, Debug)]
pub enum BlockstoreProcessorError {
    #[error("failed to load entries, error: {0}")]
    FailedToLoadEntries(#[from] BlockstoreError),

    #[error("failed to load meta")]
    FailedToLoadMeta,

    #[error("failed to replay bank 0, did you forget to provide a snapshot")]
    FailedToReplayBank0,

    #[error("invalid block error: {0}")]
    InvalidBlock(#[from] BlockError),

    #[error("invalid transaction error: {0}")]
    InvalidTransaction(#[from] TransactionError),

    #[error("no valid forks found")]
    NoValidForksFound,

    #[error("invalid hard fork slot {0}")]
    InvalidHardFork(Slot),

    #[error("root bank with mismatched capitalization at {0}")]
    RootBankWithMismatchedCapitalization(Slot),

    #[error("incomplete final fec set")]
    IncompleteFinalFecSet,

    #[error("invalid retransmitter signature final fec set")]
    InvalidRetransmitterSignatureFinalFecSet,
}

/// Callback for accessing bank state after each slot is confirmed while
/// processing the blockstore
pub type ProcessSlotCallback = Arc<dyn Fn(&Bank) + Sync + Send>;

#[derive(Default, Clone)]
pub struct ProcessOptions {
    /// Run PoH, transaction signature and other transaction verification on the entries.
    pub run_verification: bool,
    pub full_leader_cache: bool,
    pub halt_at_slot: Option<Slot>,
    pub slot_callback: Option<ProcessSlotCallback>,
    pub new_hard_forks: Option<Vec<Slot>>,
    pub debug_keys: Option<Arc<HashSet<Pubkey>>>,
    pub limit_load_slot_count_from_snapshot: Option<usize>,
    pub allow_dead_slots: bool,
    pub accounts_db_skip_shrink: bool,
    pub accounts_db_force_initial_clean: bool,
    pub accounts_db_config: AccountsDbConfig,
    pub verify_index: bool,
    pub runtime_config: RuntimeConfig,
    /// true if after processing the contents of the blockstore at startup, we should run an accounts hash calc
    /// This is useful for debugging.
    pub run_final_accounts_hash_calc: bool,
    pub use_snapshot_archives_at_startup: UseSnapshotArchivesAtStartup,
    #[cfg(feature = "dev-context-only-utils")]
    pub hash_overrides: Option<HashOverrides>,
    pub abort_on_invalid_block: bool,
    pub no_block_cost_limits: bool,
}

pub fn test_process_blockstore(
    genesis_config: &GenesisConfig,
    blockstore: &Blockstore,
    opts: &ProcessOptions,
    exit: Arc<AtomicBool>,
) -> (Arc<RwLock<BankForks>>, LeaderScheduleCache) {
    let (bank_forks, leader_schedule_cache, ..) = crate::bank_forks_utils::load_bank_forks(
        genesis_config,
        blockstore,
        Vec::new(),
        &SnapshotConfig::new_disabled(),
        opts,
        None,
        None,
        None,
        exit.clone(),
    )
    .unwrap();

    process_blockstore_from_root(
        blockstore,
        &bank_forks,
        &leader_schedule_cache,
        opts,
        None,
        None,
        None, // snapshots are disabled
    )
    .unwrap();

    (bank_forks, leader_schedule_cache)
}

pub(crate) fn process_blockstore_for_bank_0(
    genesis_config: &GenesisConfig,
    blockstore: &Blockstore,
    account_paths: Vec<PathBuf>,
    opts: &ProcessOptions,
    transaction_status_sender: Option<&TransactionStatusSender>,
    entry_notification_sender: Option<&EntryNotifierSender>,
    accounts_update_notifier: Option<AccountsUpdateNotifier>,
    exit: Arc<AtomicBool>,
) -> result::Result<Arc<RwLock<BankForks>>, BlockstoreProcessorError> {
    // Setup bank for slot 0
    let bank0 = Bank::new_from_genesis(
        genesis_config,
        Arc::new(opts.runtime_config.clone()),
        account_paths,
        opts.debug_keys.clone(),
        opts.accounts_db_config.clone(),
        accounts_update_notifier,
        None,
        exit,
        None,
        None,
    );
    let bank0_slot = bank0.slot();
    let bank_forks = BankForks::new_rw_arc(bank0);

    info!("Processing ledger for slot 0...");
    let replay_tx_thread_pool = create_thread_pool(num_cpus::get());
    process_bank_0(
        &bank_forks
            .read()
            .unwrap()
            .get_with_scheduler(bank0_slot)
            .unwrap(),
        blockstore,
        &replay_tx_thread_pool,
        opts,
        transaction_status_sender,
        entry_notification_sender,
    )?;

    Ok(bank_forks)
}

/// Process blockstore from a known root bank
#[allow(clippy::too_many_arguments)]
pub fn process_blockstore_from_root(
    blockstore: &Blockstore,
    bank_forks: &RwLock<BankForks>,
    leader_schedule_cache: &LeaderScheduleCache,
    opts: &ProcessOptions,
    transaction_status_sender: Option<&TransactionStatusSender>,
    entry_notification_sender: Option<&EntryNotifierSender>,
    snapshot_controller: Option<&SnapshotController>,
) -> result::Result<(), BlockstoreProcessorError> {
    let (start_slot, start_slot_hash) = {
        // Starting slot must be a root, and thus has no parents
        assert_eq!(bank_forks.read().unwrap().banks().len(), 1);
        let bank = bank_forks.read().unwrap().root_bank();
        #[cfg(feature = "dev-context-only-utils")]
        if let Some(hash_overrides) = &opts.hash_overrides {
            info!("Will override following slots' hashes: {hash_overrides:#?}");
            bank.set_hash_overrides(hash_overrides.clone());
        }
        if opts.no_block_cost_limits {
            warn!("setting block cost limits to MAX");
            bank.write_cost_tracker()
                .unwrap()
                .set_limits(u64::MAX, u64::MAX, u64::MAX);
        }
        assert!(bank.parent().is_none());
        (bank.slot(), bank.hash())
    };

    info!("Processing ledger from slot {start_slot}...");
    let now = Instant::now();

    // Ensure start_slot is rooted for correct replay; also ensure start_slot and
    // qualifying children are marked as connected
    if blockstore.is_primary_access() {
        blockstore
            .mark_slots_as_if_rooted_normally_at_startup(
                vec![(start_slot, Some(start_slot_hash))],
                true,
            )
            .expect("Couldn't mark start_slot as root in startup");
        blockstore
            .set_and_chain_connected_on_root_and_next_slots(start_slot)
            .expect("Couldn't mark start_slot as connected during startup")
    } else {
        info!(
            "Start slot {start_slot} isn't a root, and won't be updated due to read-only \
             blockstore access"
        );
    }

    if let Ok(Some(highest_slot)) = blockstore.highest_slot() {
        info!("ledger holds data through slot {highest_slot}");
    }

    let mut timing = ExecuteTimings::default();
    let (num_slots_processed, num_new_roots_found) = if let Some(start_slot_meta) = blockstore
        .meta(start_slot)
        .unwrap_or_else(|_| panic!("Failed to get meta for slot {start_slot}"))
    {
        let replay_tx_thread_pool = create_thread_pool(num_cpus::get());
        load_frozen_forks(
            bank_forks,
            &start_slot_meta,
            blockstore,
            &replay_tx_thread_pool,
            leader_schedule_cache,
            opts,
            transaction_status_sender,
            entry_notification_sender,
            &mut timing,
            snapshot_controller,
        )?
    } else {
        // If there's no meta in the blockstore for the input `start_slot`,
        // then we started from a snapshot and are unable to process anything.
        //
        // If the ledger has any data at all, the snapshot was likely taken at
        // a slot that is not within the range of ledger min/max slot(s).
        warn!("Starting slot {start_slot} is not in Blockstore, unable to process");
        (0, 0)
    };

    let processing_time = now.elapsed();
    let num_frozen_banks = bank_forks.read().unwrap().frozen_banks().count();
    datapoint_info!(
        "process_blockstore_from_root",
        ("total_time_us", processing_time.as_micros(), i64),
        ("frozen_banks", num_frozen_banks, i64),
        ("slot", bank_forks.read().unwrap().root(), i64),
        ("num_slots_processed", num_slots_processed, i64),
        ("num_new_roots_found", num_new_roots_found, i64),
        ("forks", bank_forks.read().unwrap().banks().len(), i64),
    );

    info!("ledger processing timing: {timing:?}");
    {
        let bank_forks = bank_forks.read().unwrap();
        let mut bank_slots = bank_forks.banks().keys().copied().collect::<Vec<_>>();
        bank_slots.sort_unstable();

        info!(
            "ledger processed in {}. root slot is {}, {} bank{}: {}",
            HumanTime::from(chrono::Duration::from_std(processing_time).unwrap())
                .to_text_en(Accuracy::Precise, Tense::Present),
            bank_forks.root(),
            bank_slots.len(),
            if bank_slots.len() > 1 { "s" } else { "" },
            bank_slots.iter().map(|slot| slot.to_string()).join(", "),
        );
        assert!(bank_forks.active_bank_slots().is_empty());
    }

    Ok(())
}

/// Verify that a segment of entries has the correct number of ticks and hashes
fn verify_ticks(
    bank: &Bank,
    mut entries: &[Entry],
    slot_full: bool,
    tick_hash_count: &mut u64,
) -> std::result::Result<(), BlockError> {
    let next_bank_tick_height = bank.tick_height() + entries.tick_count();
    let max_bank_tick_height = bank.max_tick_height();

    if next_bank_tick_height > max_bank_tick_height {
        warn!("Too many entry ticks found in slot: {}", bank.slot());
        return Err(BlockError::TooManyTicks);
    }

    if next_bank_tick_height < max_bank_tick_height && slot_full {
        info!("Too few entry ticks found in slot: {}", bank.slot());
        return Err(BlockError::TooFewTicks);
    }

    if next_bank_tick_height == max_bank_tick_height {
        let has_trailing_entry = entries.last().map(|e| !e.is_tick()).unwrap_or_default();
        if has_trailing_entry {
            warn!("Slot: {} did not end with a tick entry", bank.slot());
            return Err(BlockError::TrailingEntry);
        }

        if !slot_full {
            warn!("Slot: {} was not marked full", bank.slot());
            return Err(BlockError::InvalidLastTick);
        }
    }

    if let Some(first_alpenglow_slot) = bank
        .feature_set
        .activated_slot(&agave_feature_set::alpenglow::id())
    {
        if bank.parent_slot() >= first_alpenglow_slot {
            // If both the parent and the bank slot are in an epoch post alpenglow activation,
            // no tick verification is needed
            return Ok(());
        }

        // If the bank is in the alpenglow epoch, but the parent is from an epoch
        // where the feature flag is not active, we must verify ticks that correspond
        // to the epoch in which PoH is active. This verification is critical, as otherwise
        // a leader could jump the gun and publish a block in the alpenglow epoch without waiting
        // the appropriate time as determined by PoH in the prior epoch.
        if bank.slot() >= first_alpenglow_slot && next_bank_tick_height == max_bank_tick_height {
            if entries.is_empty() {
                // This shouldn't happen, but good to double check
                error!("Processing empty entries in verify_ticks()");
                return Ok(());
            }
            // last entry must be a tick, as verified by the `has_trailing_entry`
            // check above. Because in Alpenglow the last tick does not have any
            // hashing guarantees, we pass everything but that last tick to the
            // entry verification.
            entries = &entries[..entries.len() - 1];
        }
    }

    let hashes_per_tick = bank.hashes_per_tick().unwrap_or(0);
    if !entries.verify_tick_hash_count(tick_hash_count, hashes_per_tick) {
        warn!(
            "Tick with invalid number of hashes found in slot: {}",
            bank.slot()
        );
        return Err(BlockError::InvalidTickHashCount);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn confirm_full_slot(
    blockstore: &Blockstore,
    bank: &BankWithScheduler,
    replay_tx_thread_pool: &ThreadPool,
    opts: &ProcessOptions,
    progress: &mut ConfirmationProgress,
    transaction_status_sender: Option<&TransactionStatusSender>,
    entry_notification_sender: Option<&EntryNotifierSender>,
    replay_vote_sender: Option<&ReplayVoteSender>,
    timing: &mut ExecuteTimings,
) -> result::Result<(), BlockstoreProcessorError> {
    let mut confirmation_timing = ConfirmationTiming::default();
    let skip_verification = !opts.run_verification;

    confirm_slot(
        blockstore,
        bank,
        replay_tx_thread_pool,
        &mut confirmation_timing,
        progress,
        skip_verification,
        transaction_status_sender,
        entry_notification_sender,
        replay_vote_sender,
        opts.allow_dead_slots,
        opts.runtime_config.log_messages_bytes_limit,
        None,
    )?;

    timing.accumulate(&confirmation_timing.batch_execute.totals);

    if !bank.is_complete() {
        Err(BlockstoreProcessorError::InvalidBlock(
            BlockError::Incomplete,
        ))
    } else {
        Ok(())
    }
}

/// Measures different parts of the slot confirmation processing pipeline.
#[derive(Debug)]
pub struct ConfirmationTiming {
    /// Moment when the `ConfirmationTiming` instance was created.  Used to track the total wall
    /// clock time from the moment the first shard for the slot is received and to the moment the
    /// slot is complete.
    pub started: Instant,

    /// Wall clock time used by the slot confirmation code, including PoH/signature verification,
    /// and replay.  As replay can run in parallel with the verification, this value can not be
    /// recovered from the `replay_elapsed` and or `{poh,transaction}_verify_elapsed`.  This
    /// includes failed cases, when `confirm_slot_entries` exist with an error.  In microseconds.
    /// When unified scheduler is enabled, replay excludes the transaction execution, only
    /// accounting for task creation and submission to the scheduler.
    pub confirmation_elapsed: u64,

    /// Wall clock time used by the entry replay code.  Does not include the PoH or the transaction
    /// signature/precompiles verification, but can overlap with the PoH and signature verification.
    /// In microseconds.
    /// When unified scheduler is enabled, replay excludes the transaction execution, only
    /// accounting for task creation and submission to the scheduler.
    pub replay_elapsed: u64,

    /// Wall clock times, used for the PoH verification of entries.  In microseconds.
    pub poh_verify_elapsed: u64,

    /// Wall clock time, used for the signature verification as well as precompiles verification.
    /// In microseconds.
    pub transaction_verify_elapsed: u64,

    /// Wall clock time spent loading data sets (and entries) from the blockstore.  This does not
    /// include the case when the blockstore load failed.  In microseconds.
    pub fetch_elapsed: u64,

    /// Same as `fetch_elapsed` above, but for the case when the blockstore load fails.  In
    /// microseconds.
    pub fetch_fail_elapsed: u64,

    /// `batch_execute()` measurements.
    pub batch_execute: BatchExecutionTiming,
}

impl Default for ConfirmationTiming {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            confirmation_elapsed: 0,
            replay_elapsed: 0,
            poh_verify_elapsed: 0,
            transaction_verify_elapsed: 0,
            fetch_elapsed: 0,
            fetch_fail_elapsed: 0,
            batch_execute: BatchExecutionTiming::default(),
        }
    }
}

/// Measures times related to transaction execution in a slot.
#[derive(Debug, Default)]
pub struct BatchExecutionTiming {
    /// Time used by transaction execution.  Accumulated across multiple threads that are running
    /// `execute_batch()`.
    pub totals: ExecuteTimings,

    /// Wall clock time used by the transaction execution part of pipeline.
    /// [`ConfirmationTiming::replay_elapsed`] includes this time.  In microseconds.
    wall_clock_us: Saturating<u64>,

    /// Time used to execute transactions, via `execute_batch()`, in the thread that consumed the
    /// most time (in terms of total_thread_us) among rayon threads. Note that the slowest thread
    /// is determined each time a given group of batches is newly processed. So, this is a coarse
    /// approximation of wall-time single-threaded linearized metrics, discarding all metrics other
    /// than the arbitrary set of batches mixed with various transactions, which replayed slowest
    /// as a whole for each rayon processing session.
    ///
    /// When unified scheduler is enabled, this field isn't maintained, because it's not batched at
    /// all.
    slowest_thread: ThreadExecuteTimings,
}

impl BatchExecutionTiming {
    pub fn accumulate(
        &mut self,
        new_batch: ExecuteBatchesInternalMetrics,
        is_unified_scheduler_enabled: bool,
    ) {
        let Self {
            totals,
            wall_clock_us,
            slowest_thread,
        } = self;

        // These metric fields aren't applicable for the unified scheduler
        if !is_unified_scheduler_enabled {
            *wall_clock_us += new_batch.execute_batches_us;

            totals.saturating_add_in_place(TotalBatchesLen, new_batch.total_batches_len);
            totals.saturating_add_in_place(NumExecuteBatches, 1);
        }

        for thread_times in new_batch.execution_timings_per_thread.values() {
            totals.accumulate(&thread_times.execute_timings);
        }

        // This whole metric (replay-slot-end-to-end-stats) isn't applicable for the unified
        // scheduler.
        if !is_unified_scheduler_enabled {
            let slowest = new_batch
                .execution_timings_per_thread
                .values()
                .max_by_key(|thread_times| thread_times.total_thread_us);

            if let Some(slowest) = slowest {
                slowest_thread.accumulate(slowest);
                slowest_thread
                    .execute_timings
                    .saturating_add_in_place(NumExecuteBatches, 1);
            };
        }
    }
}

#[derive(Debug, Default)]
pub struct ThreadExecuteTimings {
    pub total_thread_us: Saturating<u64>,
    pub total_transactions_executed: Saturating<u64>,
    pub execute_timings: ExecuteTimings,
}

impl ThreadExecuteTimings {
    pub fn report_stats(&self, slot: Slot) {
        lazy! {
            datapoint_info!(
                "replay-slot-end-to-end-stats",
                ("slot", slot as i64, i64),
                ("total_thread_us", self.total_thread_us.0 as i64, i64),
                ("total_transactions_executed", self.total_transactions_executed.0 as i64, i64),
                // Everything inside the `eager!` block will be eagerly expanded before
                // evaluation of the rest of the surrounding macro.
                // Pass false because this code-path is never touched by unified scheduler.
                eager!{report_execute_timings!(self.execute_timings, false)}
            );
        };
    }

    pub fn accumulate(&mut self, other: &ThreadExecuteTimings) {
        self.execute_timings.accumulate(&other.execute_timings);
        self.total_thread_us += other.total_thread_us;
        self.total_transactions_executed += other.total_transactions_executed;
    }
}

#[derive(Default)]
pub struct ReplaySlotStats(ConfirmationTiming);
impl std::ops::Deref for ReplaySlotStats {
    type Target = ConfirmationTiming;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for ReplaySlotStats {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl ReplaySlotStats {
    pub fn report_stats(
        &self,
        slot: Slot,
        num_txs: usize,
        num_entries: usize,
        num_shreds: u64,
        bank_complete_time_us: u64,
        is_unified_scheduler_enabled: bool,
    ) {
        let confirmation_elapsed = if is_unified_scheduler_enabled {
            "confirmation_without_replay_us"
        } else {
            "confirmation_time_us"
        };
        let replay_elapsed = if is_unified_scheduler_enabled {
            "task_submission_us"
        } else {
            "replay_time"
        };
        let execute_batches_us = if is_unified_scheduler_enabled {
            None
        } else {
            Some(self.batch_execute.wall_clock_us.0 as i64)
        };

        lazy! {
            datapoint_info!(
                "replay-slot-stats",
                ("slot", slot as i64, i64),
                ("fetch_entries_time", self.fetch_elapsed as i64, i64),
                (
                    "fetch_entries_fail_time",
                    self.fetch_fail_elapsed as i64,
                    i64
                ),
                (
                    "entry_poh_verification_time",
                    self.poh_verify_elapsed as i64,
                    i64
                ),
                (
                    "entry_transaction_verification_time",
                    self.transaction_verify_elapsed as i64,
                    i64
                ),
                (confirmation_elapsed, self.confirmation_elapsed as i64, i64),
                (replay_elapsed, self.replay_elapsed as i64, i64),
                ("execute_batches_us", execute_batches_us, Option<i64>),
                (
                    "replay_total_elapsed",
                    self.started.elapsed().as_micros() as i64,
                    i64
                ),
                ("bank_complete_time_us", bank_complete_time_us, i64),
                ("total_transactions", num_txs as i64, i64),
                ("total_entries", num_entries as i64, i64),
                ("total_shreds", num_shreds as i64, i64),
                // Everything inside the `eager!` block will be eagerly expanded before
                // evaluation of the rest of the surrounding macro.
                eager!{report_execute_timings!(self.batch_execute.totals, is_unified_scheduler_enabled)}
            );
        };

        // Skip reporting replay-slot-end-to-end-stats entirely if unified scheduler is enabled,
        // because the whole metrics itself is only meaningful for rayon-based worker threads.
        //
        // See slowest_thread doc comment for details.
        if !is_unified_scheduler_enabled {
            self.batch_execute.slowest_thread.report_stats(slot);
        }

        // per_program_timings datapoints are only reported at the trace level, and all preparations
        // required to generate them can only occur when trace level is enabled.
        if log::log_enabled!(log::Level::Trace) {
            let mut per_pubkey_timings: Vec<_> = self
                .batch_execute
                .totals
                .details
                .per_program_timings
                .iter()
                .collect();
            per_pubkey_timings.sort_by(|a, b| b.1.accumulated_us.cmp(&a.1.accumulated_us));
            let (total_us, total_units, total_count, total_errored_units, total_errored_count) =
                per_pubkey_timings.iter().fold(
                    (0, 0, 0, 0, 0),
                    |(sum_us, sum_units, sum_count, sum_errored_units, sum_errored_count), a| {
                        (
                            sum_us + a.1.accumulated_us.0,
                            sum_units + a.1.accumulated_units.0,
                            sum_count + a.1.count.0,
                            sum_errored_units + a.1.total_errored_units.0,
                            sum_errored_count + a.1.errored_txs_compute_consumed.len(),
                        )
                    },
                );

            for (pubkey, time) in per_pubkey_timings.iter().take(5) {
                datapoint_trace!(
                    "per_program_timings",
                    ("slot", slot as i64, i64),
                    ("pubkey", pubkey.to_string(), String),
                    ("execute_us", time.accumulated_us.0, i64),
                    ("accumulated_units", time.accumulated_units.0, i64),
                    ("errored_units", time.total_errored_units.0, i64),
                    ("count", time.count.0, i64),
                    (
                        "errored_count",
                        time.errored_txs_compute_consumed.len(),
                        i64
                    ),
                );
            }
            datapoint_info!(
                "per_program_timings",
                ("slot", slot as i64, i64),
                ("pubkey", "all", String),
                ("execute_us", total_us, i64),
                ("accumulated_units", total_units, i64),
                ("count", total_count, i64),
                ("errored_units", total_errored_units, i64),
                ("errored_count", total_errored_count, i64)
            );
        }
    }
}

#[derive(Default)]
pub struct ConfirmationProgress {
    pub last_entry: Hash,
    pub tick_hash_count: u64,
    pub num_shreds: u64,
    pub num_entries: usize,
    pub num_txs: usize,
}

impl ConfirmationProgress {
    pub fn new(last_entry: Hash) -> Self {
        Self {
            last_entry,
            ..Self::default()
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn confirm_slot(
    blockstore: &Blockstore,
    bank: &BankWithScheduler,
    replay_tx_thread_pool: &ThreadPool,
    timing: &mut ConfirmationTiming,
    progress: &mut ConfirmationProgress,
    skip_verification: bool,
    transaction_status_sender: Option<&TransactionStatusSender>,
    entry_notification_sender: Option<&EntryNotifierSender>,
    replay_vote_sender: Option<&ReplayVoteSender>,
    allow_dead_slots: bool,
    log_messages_bytes_limit: Option<usize>,
    prioritization_fee_cache: Option<&PrioritizationFeeCache>,
) -> result::Result<(), BlockstoreProcessorError> {
    let slot = bank.slot();

    let slot_entries_load_result = {
        let mut load_elapsed = Measure::start("load_elapsed");
        let load_result = blockstore
            .get_slot_entries_with_shred_info(slot, progress.num_shreds, allow_dead_slots)
            .map_err(BlockstoreProcessorError::FailedToLoadEntries);
        load_elapsed.stop();
        if load_result.is_err() {
            timing.fetch_fail_elapsed += load_elapsed.as_us();
        } else {
            timing.fetch_elapsed += load_elapsed.as_us();
        }
        load_result
    }?;

    confirm_slot_entries(
        bank,
        replay_tx_thread_pool,
        slot_entries_load_result,
        timing,
        progress,
        skip_verification,
        transaction_status_sender,
        entry_notification_sender,
        replay_vote_sender,
        log_messages_bytes_limit,
        prioritization_fee_cache,
    )
}

#[allow(clippy::too_many_arguments)]
fn confirm_slot_entries(
    bank: &BankWithScheduler,
    replay_tx_thread_pool: &ThreadPool,
    slot_entries_load_result: (Vec<Entry>, u64, bool),
    timing: &mut ConfirmationTiming,
    progress: &mut ConfirmationProgress,
    skip_verification: bool,
    transaction_status_sender: Option<&TransactionStatusSender>,
    entry_notification_sender: Option<&EntryNotifierSender>,
    replay_vote_sender: Option<&ReplayVoteSender>,
    log_messages_bytes_limit: Option<usize>,
    prioritization_fee_cache: Option<&PrioritizationFeeCache>,
) -> result::Result<(), BlockstoreProcessorError> {
    let ConfirmationTiming {
        confirmation_elapsed,
        replay_elapsed,
        poh_verify_elapsed,
        transaction_verify_elapsed,
        batch_execute: batch_execute_timing,
        ..
    } = timing;

    let confirmation_elapsed_timer = Measure::start("confirmation_elapsed");
    defer! {
        *confirmation_elapsed += confirmation_elapsed_timer.end_as_us();
    };

    let slot = bank.slot();
    let (entries, num_shreds, slot_full) = slot_entries_load_result;
    let num_entries = entries.len();
    let mut entry_tx_starting_indexes = Vec::with_capacity(num_entries);
    let mut entry_tx_starting_index = progress.num_txs;
    let num_txs = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            if let Some(entry_notification_sender) = entry_notification_sender {
                let entry_index = progress.num_entries.saturating_add(i);
                if let Err(err) = entry_notification_sender.send(EntryNotification {
                    slot,
                    index: entry_index,
                    entry: entry.into(),
                    starting_transaction_index: entry_tx_starting_index,
                }) {
                    warn!(
                        "Slot {slot}, entry {entry_index} entry_notification_sender send failed: \
                         {err:?}"
                    );
                }
            }
            let num_txs = entry.transactions.len();
            let next_tx_starting_index = entry_tx_starting_index.saturating_add(num_txs);
            entry_tx_starting_indexes.push(entry_tx_starting_index);
            entry_tx_starting_index = next_tx_starting_index;
            num_txs
        })
        .sum::<usize>();
    trace!(
        "Fetched entries for slot {slot}, num_entries: {num_entries}, num_shreds: {num_shreds}, \
         num_txs: {num_txs}, slot_full: {slot_full}",
    );

    if !skip_verification {
        let tick_hash_count = &mut progress.tick_hash_count;
        verify_ticks(bank, &entries, slot_full, tick_hash_count).map_err(|err| {
            warn!(
                "{:#?}, slot: {}, entry len: {}, tick_height: {}, last entry: {}, last_blockhash: \
                 {}, shred_index: {}, slot_full: {}",
                err,
                slot,
                num_entries,
                bank.tick_height(),
                progress.last_entry,
                bank.last_blockhash(),
                num_shreds,
                slot_full,
            );
            err
        })?;
    }

    let last_entry_hash = entries.last().map(|e| e.hash);
    if !skip_verification {
        datapoint_debug!("verify-batch-size", ("size", num_entries as i64, i64));
        let entry_state = entries.verify(&progress.last_entry, replay_tx_thread_pool);
        *poh_verify_elapsed += entry_state.poh_duration_us();
        if !entry_state.status() {
            warn!("Ledger proof of history failed at slot: {slot}");
            return Err(BlockError::InvalidEntryHash.into());
        }
    }

    let verify_transaction = {
        let bank = bank.clone_with_scheduler();
        move |versioned_tx: VersionedTransaction,
              verification_mode: TransactionVerificationMode|
              -> Result<RuntimeTransaction<SanitizedTransaction>> {
            bank.verify_transaction(versioned_tx, verification_mode)
        }
    };

    let transaction_verification_start = Instant::now();
    let transaction_verification_result = entry::start_verify_transactions(
        entries,
        skip_verification,
        replay_tx_thread_pool,
        Arc::new(verify_transaction),
    );
    let transaction_cpu_duration_us = transaction_verification_start.elapsed().as_micros() as u64;

    let mut transaction_verification_result = match transaction_verification_result {
        Ok(transaction_verification_result) => transaction_verification_result,
        Err(err) => {
            warn!(
                "Ledger transaction signature verification failed at slot: {}",
                bank.slot()
            );
            return Err(err.into());
        }
    };

    let entries = transaction_verification_result
        .entries()
        .expect("Transaction verification generates entries");

    let mut replay_timer = Measure::start("replay_elapsed");
    let replay_entries: Vec<_> = entries
        .into_iter()
        .zip(entry_tx_starting_indexes)
        .map(|(entry, tx_starting_index)| ReplayEntry {
            entry,
            starting_index: tx_starting_index,
        })
        .collect();
    let process_result = process_entries(
        bank,
        replay_tx_thread_pool,
        replay_entries,
        transaction_status_sender,
        replay_vote_sender,
        batch_execute_timing,
        log_messages_bytes_limit,
        prioritization_fee_cache,
    )
    .map_err(BlockstoreProcessorError::from);
    replay_timer.stop();
    *replay_elapsed += replay_timer.as_us();

    {
        let valid = transaction_verification_result.status();
        *transaction_verify_elapsed += transaction_cpu_duration_us;

        if !valid {
            warn!(
                "Ledger transaction signature verification failed at slot: {}",
                bank.slot()
            );
            return Err(TransactionError::SignatureFailure.into());
        }
    }

    process_result?;

    progress.num_shreds += num_shreds;
    progress.num_entries += num_entries;
    progress.num_txs += num_txs;
    if let Some(last_entry_hash) = last_entry_hash {
        progress.last_entry = last_entry_hash;
    }

    Ok(())
}

// Special handling required for processing the entries in slot 0
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn process_bank_0(
    bank0: &BankWithScheduler,
    blockstore: &Blockstore,
    replay_tx_thread_pool: &ThreadPool,
    opts: &ProcessOptions,
    transaction_status_sender: Option<&TransactionStatusSender>,
    entry_notification_sender: Option<&EntryNotifierSender>,
) -> result::Result<(), BlockstoreProcessorError> {
    assert_eq!(bank0.slot(), 0);
    let mut progress = ConfirmationProgress::new(bank0.last_blockhash());
    confirm_full_slot(
        blockstore,
        bank0,
        replay_tx_thread_pool,
        opts,
        &mut progress,
        None,
        entry_notification_sender,
        None,
        &mut ExecuteTimings::default(),
    )
    .map_err(|_| BlockstoreProcessorError::FailedToReplayBank0)?;
    if let Some((result, _timings)) = bank0.wait_for_completed_scheduler() {
        result.unwrap();
    }
    bank0.freeze();
    if blockstore.is_primary_access() {
        blockstore.insert_bank_hash(bank0.slot(), bank0.hash(), false);
    }

    if let Some(transaction_status_sender) = transaction_status_sender {
        transaction_status_sender.send_transaction_status_freeze_message(bank0);
    }

    Ok(())
}

// Given a bank, add its children to the pending slots queue if those children slots are
// complete
fn process_next_slots(
    bank: &Arc<Bank>,
    meta: &SlotMeta,
    blockstore: &Blockstore,
    leader_schedule_cache: &LeaderScheduleCache,
    pending_slots: &mut Vec<(SlotMeta, Bank, Hash)>,
    opts: &ProcessOptions,
) -> result::Result<(), BlockstoreProcessorError> {
    if meta.next_slots.is_empty() {
        return Ok(());
    }

    // This is a fork point if there are multiple children, create a new child bank for each fork
    for next_slot in &meta.next_slots {
        if opts
            .halt_at_slot
            .is_some_and(|halt_at_slot| *next_slot > halt_at_slot)
        {
            continue;
        }
        if !opts.allow_dead_slots && blockstore.is_dead(*next_slot) {
            continue;
        }

        let next_meta = blockstore
            .meta(*next_slot)
            .map_err(|err| {
                warn!("Failed to load meta for slot {next_slot}: {err:?}");
                BlockstoreProcessorError::FailedToLoadMeta
            })?
            .unwrap();

        // Only process full slots in blockstore_processor, replay_stage
        // handles any partials
        if next_meta.is_full() {
            let next_bank = Bank::new_from_parent(
                bank.clone(),
                &leader_schedule_cache
                    .slot_leader_at(*next_slot, Some(bank))
                    .unwrap(),
                *next_slot,
            );
            set_alpenglow_ticks(&next_bank);
            trace!(
                "New bank for slot {}, parent slot is {}",
                next_slot,
                bank.slot(),
            );
            pending_slots.push((next_meta, next_bank, bank.last_blockhash()));
        }
    }

    // Reverse sort by slot, so the next slot to be processed can be popped
    pending_slots.sort_by(|a, b| b.1.slot().cmp(&a.1.slot()));
    Ok(())
}

/// Set alpenglow bank tick height.
///
/// For alpenglow banks this tick height is `max_tick_height` - 1, for a bank on the epoch boundary
/// of feature activation, we need ticks_per_slot for each slot between the parent and epoch boundary
/// and one extra tick for the alpenglow bank
pub fn set_alpenglow_ticks(bank: &Bank) {
    let Some(first_alpenglow_slot) = bank
        .feature_set
        .activated_slot(&agave_feature_set::alpenglow::id())
    else {
        return;
    };

    let Some(alpenglow_ticks) = calculate_alpenglow_ticks(
        bank.slot(),
        first_alpenglow_slot,
        bank.parent_slot(),
        bank.ticks_per_slot(),
    ) else {
        return;
    };

    info!(
        "Alpenglow: Setting tick height for slot {} to {}",
        bank.slot(),
        bank.max_tick_height() - alpenglow_ticks
    );
    bank.set_tick_height(bank.max_tick_height() - alpenglow_ticks);
}

/// Calculates how many ticks are needed for a block at `slot` with parent `parent_slot`
///
/// If both `parent_slot` and `slot` are greater than or equal to `first_alpenglow_slot`, then
/// only 1 tick is needed. This tick has no hashing guarantees, it is simply used as a signal
/// for the end of the block.
///
/// If both `parent_slot` and `slot` are less than `first_alpenglow_slot`, we need the
/// appropriate amount of PoH ticks, indicated by a None return value.
///
/// If `parent_slot` is less than `first_alpenglow_slot` and `slot` is greater than or equal
/// to `first_alpenglow_slot` (A block that "straddles" the activation epoch boundary) then:
///
/// 1. All slots between `parent_slot` and `first_alpenglow_slot` need to have `ticks_per_slot` ticks
/// 2. One extra tick for the actual alpenglow slot
/// 3. There are no ticks for any skipped alpenglow slots
fn calculate_alpenglow_ticks(
    slot: Slot,
    first_alpenglow_slot: Slot,
    parent_slot: Slot,
    ticks_per_slot: u64,
) -> Option<u64> {
    // Slots before alpenglow shouldn't have alpenglow ticks
    if slot < first_alpenglow_slot {
        return None;
    }

    let alpenglow_ticks = if parent_slot < first_alpenglow_slot && slot >= first_alpenglow_slot {
        (first_alpenglow_slot - parent_slot - 1) * ticks_per_slot + 1
    } else {
        1
    };

    Some(alpenglow_ticks)
}

/// Starting with the root slot corresponding to `start_slot_meta`, iteratively
/// find and process children slots from the blockstore.
///
/// Returns a tuple (a, b) where a is the number of slots processed and b is
/// the number of newly found cluster roots.
#[allow(clippy::too_many_arguments)]
fn load_frozen_forks(
    bank_forks: &RwLock<BankForks>,
    start_slot_meta: &SlotMeta,
    blockstore: &Blockstore,
    replay_tx_thread_pool: &ThreadPool,
    leader_schedule_cache: &LeaderScheduleCache,
    opts: &ProcessOptions,
    transaction_status_sender: Option<&TransactionStatusSender>,
    entry_notification_sender: Option<&EntryNotifierSender>,
    timing: &mut ExecuteTimings,
    snapshot_controller: Option<&SnapshotController>,
) -> result::Result<(u64, usize), BlockstoreProcessorError> {
    let blockstore_max_root = blockstore.max_root();
    let mut root = bank_forks.read().unwrap().root();
    let max_root = std::cmp::max(root, blockstore_max_root);
    info!(
        "load_frozen_forks() latest root from blockstore: {blockstore_max_root}, max_root: \
         {max_root}",
    );

    // The total number of slots processed
    let mut total_slots_processed = 0;
    // The total number of newly identified root slots
    let mut total_rooted_slots = 0;

    let mut pending_slots = vec![];
    process_next_slots(
        &bank_forks
            .read()
            .unwrap()
            .get(start_slot_meta.slot)
            .unwrap(),
        start_slot_meta,
        blockstore,
        leader_schedule_cache,
        &mut pending_slots,
        opts,
    )?;

    if Some(bank_forks.read().unwrap().root()) != opts.halt_at_slot {
        let mut all_banks = HashMap::new();

        const STATUS_REPORT_INTERVAL: Duration = Duration::from_secs(2);
        let mut last_status_report = Instant::now();
        let mut slots_processed = 0;
        let mut txs = 0;
        let mut set_root_us = 0;
        let mut root_retain_us = 0;
        let mut process_single_slot_us = 0;
        let mut voting_us = 0;

        while !pending_slots.is_empty() {
            timing.details.per_program_timings.clear();
            let (meta, bank, last_entry_hash) = pending_slots.pop().unwrap();
            let slot = bank.slot();
            if last_status_report.elapsed() > STATUS_REPORT_INTERVAL {
                let secs = last_status_report.elapsed().as_secs() as f32;
                let slots_per_sec = slots_processed as f32 / secs;
                let txs_per_sec = txs as f32 / secs;
                info!(
                    "processing ledger: slot={slot}, root_slot={root} slots={slots_processed}, \
                     slots/s={slots_per_sec}, txs/s={txs_per_sec}"
                );
                debug!(
                    "processing ledger timing: set_root_us={set_root_us}, \
                     root_retain_us={root_retain_us}, \
                     process_single_slot_us:{process_single_slot_us}, voting_us: {voting_us}"
                );

                last_status_report = Instant::now();
                slots_processed = 0;
                txs = 0;
                set_root_us = 0;
                root_retain_us = 0;
                process_single_slot_us = 0;
                voting_us = 0;
            }

            let mut progress = ConfirmationProgress::new(last_entry_hash);
            let mut m = Measure::start("process_single_slot");
            let bank = bank_forks.write().unwrap().insert_from_ledger(bank);
            if let Err(error) = process_single_slot(
                blockstore,
                &bank,
                replay_tx_thread_pool,
                opts,
                &mut progress,
                transaction_status_sender,
                entry_notification_sender,
                None,
                timing,
            ) {
                assert!(bank_forks.write().unwrap().remove(bank.slot()).is_some());
                if opts.abort_on_invalid_block {
                    Err(error)?
                }
                continue;
            }
            txs += progress.num_txs;

            // Block must be frozen by this point; otherwise,
            // process_single_slot() would have errored above.
            assert!(bank.is_frozen());
            all_banks.insert(bank.slot(), bank.clone_with_scheduler());
            m.stop();
            process_single_slot_us += m.as_us();

            let mut m = Measure::start("voting");
            // If we've reached the last known root in blockstore, start looking
            // for newer cluster confirmed roots
            let new_root_bank = {
                if bank_forks.read().unwrap().root() >= max_root {
                    supermajority_root_from_vote_accounts(
                        bank.total_epoch_stake(),
                        &bank.vote_accounts(),
                    ).and_then(|supermajority_root| {
                        if supermajority_root > root {
                            // If there's a cluster confirmed root greater than our last
                            // replayed root, then because the cluster confirmed root should
                            // be descended from our last root, it must exist in `all_banks`
                            let cluster_root_bank = all_banks.get(&supermajority_root).unwrap();

                            // cluster root must be a descendant of our root, otherwise something
                            // is drastically wrong
                            assert!(cluster_root_bank.ancestors.contains_key(&root));
                            info!(
                                "blockstore processor found new cluster confirmed root: {}, observed in bank: {}",
                                cluster_root_bank.slot(), bank.slot()
                            );

                            // Ensure cluster-confirmed root and parents are set as root in blockstore
                            let mut rooted_slots = vec![];
                            let mut new_root_bank = cluster_root_bank.clone_without_scheduler();
                            loop {
                                if new_root_bank.slot() == root { break; } // Found the last root in the chain, yay!
                                assert!(new_root_bank.slot() > root);

                                rooted_slots.push((new_root_bank.slot(), Some(new_root_bank.hash())));
                                // As noted, the cluster confirmed root should be descended from
                                // our last root; therefore parent should be set
                                new_root_bank = new_root_bank.parent().unwrap();
                            }
                            total_rooted_slots += rooted_slots.len();
                            if blockstore.is_primary_access() {
                                blockstore
                                    .mark_slots_as_if_rooted_normally_at_startup(rooted_slots, true)
                                    .expect("Blockstore::mark_slots_as_if_rooted_normally_at_startup() should succeed");
                            }
                            Some(cluster_root_bank)
                        } else {
                            None
                        }
                    })
                } else if blockstore.is_root(slot) {
                    Some(&bank)
                } else {
                    None
                }
            };
            m.stop();
            voting_us += m.as_us();

            if let Some(new_root_bank) = new_root_bank {
                let mut m = Measure::start("set_root");
                root = new_root_bank.slot();

                leader_schedule_cache.set_root(new_root_bank);
                new_root_bank.prune_program_cache(root, new_root_bank.epoch());
                let _ = bank_forks
                    .write()
                    .unwrap()
                    .set_root(root, snapshot_controller, None);
                m.stop();
                set_root_us += m.as_us();

                // Filter out all non descendants of the new root
                let mut m = Measure::start("filter pending slots");
                pending_slots
                    .retain(|(_, pending_bank, _)| pending_bank.ancestors.contains_key(&root));
                all_banks.retain(|_, bank| bank.ancestors.contains_key(&root));
                m.stop();
                root_retain_us += m.as_us();
            }

            slots_processed += 1;
            total_slots_processed += 1;

            trace!(
                "Bank for {}slot {} is complete",
                if root == slot { "root " } else { "" },
                slot,
            );

            let done_processing = opts
                .halt_at_slot
                .map(|halt_at_slot| slot >= halt_at_slot)
                .unwrap_or(false);
            if done_processing {
                if opts.run_final_accounts_hash_calc {
                    bank.run_final_hash_calc();
                }
                break;
            }

            process_next_slots(
                &bank,
                &meta,
                blockstore,
                leader_schedule_cache,
                &mut pending_slots,
                opts,
            )?;
        }
    } else if opts.run_final_accounts_hash_calc {
        bank_forks.read().unwrap().root_bank().run_final_hash_calc();
    }

    Ok((total_slots_processed, total_rooted_slots))
}

// `roots` is sorted largest to smallest by root slot
fn supermajority_root(roots: &[(Slot, u64)], total_epoch_stake: u64) -> Option<Slot> {
    if roots.is_empty() {
        return None;
    }

    // Find latest root
    let mut total = 0;
    let mut prev_root = roots[0].0;
    for (root, stake) in roots.iter() {
        assert!(*root <= prev_root);
        total += stake;
        if total as f64 / total_epoch_stake as f64 > VOTE_THRESHOLD_SIZE {
            return Some(*root);
        }
        prev_root = *root;
    }

    None
}

fn supermajority_root_from_vote_accounts(
    total_epoch_stake: u64,
    vote_accounts: &VoteAccountsHashMap,
) -> Option<Slot> {
    let mut roots_stakes: Vec<(Slot, u64)> = vote_accounts
        .values()
        .filter_map(|(stake, account)| {
            if *stake == 0 {
                return None;
            }

            Some((account.vote_state_view().root_slot()?, *stake))
        })
        .collect();

    // Sort from greatest to smallest slot
    roots_stakes.sort_unstable_by(|a, b| a.0.cmp(&b.0).reverse());

    // Find latest root
    supermajority_root(&roots_stakes, total_epoch_stake)
}

// Processes and replays the contents of a single slot, returns Error
// if failed to play the slot
#[allow(clippy::too_many_arguments)]
pub fn process_single_slot(
    blockstore: &Blockstore,
    bank: &BankWithScheduler,
    replay_tx_thread_pool: &ThreadPool,
    opts: &ProcessOptions,
    progress: &mut ConfirmationProgress,
    transaction_status_sender: Option<&TransactionStatusSender>,
    entry_notification_sender: Option<&EntryNotifierSender>,
    replay_vote_sender: Option<&ReplayVoteSender>,
    timing: &mut ExecuteTimings,
) -> result::Result<(), BlockstoreProcessorError> {
    let slot = bank.slot();
    // Mark corrupt slots as dead so validators don't replay this slot and
    // see AlreadyProcessed errors later in ReplayStage
    confirm_full_slot(
        blockstore,
        bank,
        replay_tx_thread_pool,
        opts,
        progress,
        transaction_status_sender,
        entry_notification_sender,
        replay_vote_sender,
        timing,
    )
    .and_then(|()| {
        if let Some((result, completed_timings)) = bank.wait_for_completed_scheduler() {
            timing.accumulate(&completed_timings);
            result?
        }
        Ok(())
    })
    .map_err(|err| {
        warn!("slot {slot} failed to verify: {err}");
        if blockstore.is_primary_access() {
            blockstore
                .set_dead_slot(slot)
                .expect("Failed to mark slot as dead in blockstore");
        } else {
            info!(
                "Failed slot {slot} won't be marked dead due to being read-only blockstore access"
            );
        }
        err
    })?;

    if let Some((result, _timings)) = bank.wait_for_completed_scheduler() {
        result?
    }

    let block_id = blockstore
        .check_last_fec_set_and_get_block_id(slot, bank.hash(), &bank.feature_set)
        .inspect_err(|err| {
            warn!("slot {slot} failed last fec set checks: {err}");
            if blockstore.is_primary_access() {
                blockstore
                    .set_dead_slot(slot)
                    .expect("Failed to mark slot as dead in blockstore");
            } else {
                info!(
                    "Failed last fec set checks slot {slot} won't be marked dead due to being \
                     read-only blockstore access"
                );
            }
        })?;
    bank.set_block_id(block_id);
    bank.freeze(); // all banks handled by this routine are created from complete slots

    if let Some(slot_callback) = &opts.slot_callback {
        slot_callback(bank);
    }

    if blockstore.is_primary_access() {
        blockstore.insert_bank_hash(bank.slot(), bank.hash(), false);
    }

    if let Some(transaction_status_sender) = transaction_status_sender {
        transaction_status_sender.send_transaction_status_freeze_message(bank);
    }

    Ok(())
}

type WorkSequence = u64;

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum TransactionStatusMessage {
    Batch((TransactionStatusBatch, Option<WorkSequence>)),
    Freeze(Arc<Bank>),
}

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

#[derive(Clone, Debug)]
pub struct TransactionStatusSender {
    pub sender: Sender<TransactionStatusMessage>,
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

// used for tests only
pub fn fill_blockstore_slot_with_ticks(
    blockstore: &Blockstore,
    ticks_per_slot: u64,
    slot: u64,
    parent_slot: u64,
    last_entry_hash: Hash,
) -> Hash {
    // Only slot 0 can be equal to the parent_slot
    assert!(slot.saturating_sub(1) >= parent_slot);
    let num_slots = (slot - parent_slot).max(1);
    let entries = create_ticks(num_slots * ticks_per_slot, 0, last_entry_hash);
    let last_entry_hash = entries.last().unwrap().hash;

    blockstore
        .write_entries(
            slot,
            0,
            0,
            ticks_per_slot,
            Some(parent_slot),
            true,
            &Arc::new(Keypair::new()),
            entries,
            0,
        )
        .unwrap();

    last_entry_hash
}

#[cfg(test)]
pub mod tests {
    use {
        super::*,
        crate::{
            blockstore_options::{AccessType, BlockstoreOptions},
            genesis_utils::{
                create_genesis_config, create_genesis_config_with_leader, GenesisConfigInfo,
            },
        },
        assert_matches::assert_matches,
        rand::{rng, Rng},
        solana_account::{AccountSharedData, WritableAccount},
        solana_cost_model::transaction_cost::TransactionCost,
        solana_entry::entry::{create_ticks, next_entry, next_entry_mut},
        solana_epoch_schedule::EpochSchedule,
        solana_hash::Hash,
        solana_instruction::{error::InstructionError, Instruction},
        solana_keypair::Keypair,
        solana_native_token::LAMPORTS_PER_SOL,
        solana_program_runtime::declare_process_instruction,
        solana_pubkey::Pubkey,
        solana_runtime::{
            bank::bank_hash_details::SlotDetails,
            genesis_utils::{
                self, create_genesis_config_with_vote_accounts, ValidatorVoteKeypairs,
            },
            installed_scheduler_pool::{
                MockInstalledScheduler, MockUninstalledScheduler, SchedulerAborted,
                SchedulingContext,
            },
        },
        solana_signer::Signer,
        solana_svm::transaction_processor::ExecutionRecordingConfig,
        solana_system_interface::error::SystemError,
        solana_system_transaction as system_transaction,
        solana_transaction::Transaction,
        solana_transaction_error::TransactionError,
        solana_vote::{vote_account::VoteAccount, vote_transaction},
        solana_vote_program::{
            self,
            vote_state::{TowerSync, VoteStateV4, VoteStateVersions, MAX_LOCKOUT_HISTORY},
        },
        std::{collections::BTreeSet, slice, sync::RwLock},
        test_case::{test_case, test_matrix},
        trees::tr,
    };

    // Convenience wrapper to optionally process blockstore with ReadOnly access.
    //
    // Setting up the ledger for a test requires Primary access as items will need to be inserted.
    // However, once a ReadOnly access has been opened, it won't automatically see updates made by
    // the Primary access. So, open (and close) the ReadOnly access within this function to ensure
    // that "stale" ReadOnly accesses don't propagate.
    fn test_process_blockstore_with_custom_options(
        genesis_config: &GenesisConfig,
        blockstore: &Blockstore,
        opts: &ProcessOptions,
        access_type: AccessType,
    ) -> (Arc<RwLock<BankForks>>, LeaderScheduleCache) {
        match access_type {
            AccessType::Primary | AccessType::PrimaryForMaintenance => {
                // Attempting to open a second Primary access would fail, so
                // just pass the original session if it is a Primary variant
                test_process_blockstore(genesis_config, blockstore, opts, Arc::default())
            }
            AccessType::ReadOnly => {
                let read_only_blockstore = Blockstore::open_with_options(
                    blockstore.ledger_path(),
                    BlockstoreOptions {
                        access_type,
                        ..BlockstoreOptions::default()
                    },
                )
                .expect("Unable to open access to blockstore");
                test_process_blockstore(genesis_config, &read_only_blockstore, opts, Arc::default())
            }
        }
    }

    fn process_entries_for_tests_without_scheduler(
        bank: &Arc<Bank>,
        entries: Vec<Entry>,
    ) -> Result<()> {
        process_entries_for_tests(
            &BankWithScheduler::new_without_scheduler(bank.clone()),
            entries,
            None,
            None,
        )
    }

    #[test]
    fn test_process_blockstore_with_missing_hashes() {
        do_test_process_blockstore_with_missing_hashes(AccessType::Primary);
    }

    #[test]
    fn test_process_blockstore_with_missing_hashes_read_only_access() {
        do_test_process_blockstore_with_missing_hashes(AccessType::ReadOnly);
    }

    // Intentionally make slot 1 faulty and ensure that processing sees it as dead
    fn do_test_process_blockstore_with_missing_hashes(blockstore_access_type: AccessType) {
        agave_logger::setup();

        let hashes_per_tick = 2;
        let GenesisConfigInfo {
            mut genesis_config, ..
        } = create_genesis_config(10_000);
        genesis_config.poh_config.hashes_per_tick = Some(hashes_per_tick);
        let ticks_per_slot = genesis_config.ticks_per_slot;

        let (ledger_path, blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();

        let parent_slot = 0;
        let slot = 1;
        let entries = create_ticks(ticks_per_slot, hashes_per_tick - 1, blockhash);
        assert_matches!(
            blockstore.write_entries(
                slot,
                0,
                0,
                ticks_per_slot,
                Some(parent_slot),
                true,
                &Arc::new(Keypair::new()),
                entries,
                0,
            ),
            Ok(_)
        );

        let (bank_forks, ..) = test_process_blockstore_with_custom_options(
            &genesis_config,
            &blockstore,
            &ProcessOptions {
                run_verification: true,
                ..ProcessOptions::default()
            },
            blockstore_access_type.clone(),
        );
        assert_eq!(frozen_bank_slots(&bank_forks.read().unwrap()), vec![0]);

        let dead_slots: Vec<Slot> = blockstore.dead_slots_iterator(0).unwrap().collect();
        match blockstore_access_type {
            // In ReadOnly access even though a dead slot
            // will be identified, it won't actually be marked dead.
            AccessType::ReadOnly => {
                assert_eq!(dead_slots.len(), 0);
            }
            AccessType::Primary | AccessType::PrimaryForMaintenance => {
                assert_eq!(&dead_slots, &[1]);
            }
        }
    }

    #[test]
    fn test_process_blockstore_with_invalid_slot_tick_count() {
        agave_logger::setup();

        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let ticks_per_slot = genesis_config.ticks_per_slot;

        // Create a new ledger with slot 0 full of ticks
        let (ledger_path, blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();

        // Write slot 1 with one tick missing
        let parent_slot = 0;
        let slot = 1;
        let entries = create_ticks(ticks_per_slot - 1, 0, blockhash);
        assert_matches!(
            blockstore.write_entries(
                slot,
                0,
                0,
                ticks_per_slot,
                Some(parent_slot),
                true,
                &Arc::new(Keypair::new()),
                entries,
                0,
            ),
            Ok(_)
        );

        // Should return slot 0, the last slot on the fork that is valid
        let (bank_forks, ..) = test_process_blockstore(
            &genesis_config,
            &blockstore,
            &ProcessOptions {
                run_verification: true,
                ..ProcessOptions::default()
            },
            Arc::default(),
        );
        assert_eq!(frozen_bank_slots(&bank_forks.read().unwrap()), vec![0]);

        // Write slot 2 fully
        let _last_slot2_entry_hash =
            fill_blockstore_slot_with_ticks(&blockstore, ticks_per_slot, 2, 0, blockhash);

        let (bank_forks, ..) = test_process_blockstore(
            &genesis_config,
            &blockstore,
            &ProcessOptions {
                run_verification: true,
                ..ProcessOptions::default()
            },
            Arc::default(),
        );

        // One valid fork, one bad fork.  process_blockstore() should only return the valid fork
        assert_eq!(frozen_bank_slots(&bank_forks.read().unwrap()), vec![0, 2]);
        assert_eq!(bank_forks.read().unwrap().working_bank().slot(), 2);
        assert_eq!(bank_forks.read().unwrap().root(), 0);
    }

    #[test]
    fn test_process_blockstore_with_slot_with_trailing_entry() {
        agave_logger::setup();

        let GenesisConfigInfo {
            mint_keypair,
            genesis_config,
            ..
        } = create_genesis_config(10_000);
        let ticks_per_slot = genesis_config.ticks_per_slot;

        let (ledger_path, blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();

        let mut entries = create_ticks(ticks_per_slot, 0, blockhash);
        let trailing_entry = {
            let keypair = Keypair::new();
            let tx = system_transaction::transfer(&mint_keypair, &keypair.pubkey(), 1, blockhash);
            next_entry(&blockhash, 1, vec![tx])
        };
        entries.push(trailing_entry);

        // Tricks blockstore into writing the trailing entry by lying that there is one more tick
        // per slot.
        let parent_slot = 0;
        let slot = 1;
        assert_matches!(
            blockstore.write_entries(
                slot,
                0,
                0,
                ticks_per_slot + 1,
                Some(parent_slot),
                true,
                &Arc::new(Keypair::new()),
                entries,
                0,
            ),
            Ok(_)
        );

        let opts = ProcessOptions {
            run_verification: true,
            ..ProcessOptions::default()
        };
        let (bank_forks, ..) =
            test_process_blockstore(&genesis_config, &blockstore, &opts, Arc::default());
        assert_eq!(frozen_bank_slots(&bank_forks.read().unwrap()), vec![0]);
    }

    #[test]
    fn test_process_blockstore_with_incomplete_slot() {
        agave_logger::setup();

        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let ticks_per_slot = genesis_config.ticks_per_slot;

        /*
          Build a blockstore in the ledger with the following fork structure:

               slot 0 (all ticks)
                 |
               slot 1 (all ticks but one)
                 |
               slot 2 (all ticks)

           where slot 1 is incomplete (missing 1 tick at the end)
        */

        // Create a new ledger with slot 0 full of ticks
        let (ledger_path, mut blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
        debug!("ledger_path: {ledger_path:?}");

        let blockstore = Blockstore::open(ledger_path.path()).unwrap();

        // Write slot 1
        // slot 1, points at slot 0.  Missing one tick
        {
            let parent_slot = 0;
            let slot = 1;
            let mut entries = create_ticks(ticks_per_slot, 0, blockhash);
            blockhash = entries.last().unwrap().hash;

            // throw away last one
            entries.pop();

            assert_matches!(
                blockstore.write_entries(
                    slot,
                    0,
                    0,
                    ticks_per_slot,
                    Some(parent_slot),
                    false,
                    &Arc::new(Keypair::new()),
                    entries,
                    0,
                ),
                Ok(_)
            );
        }

        // slot 2, points at slot 1
        fill_blockstore_slot_with_ticks(&blockstore, ticks_per_slot, 2, 1, blockhash);

        let opts = ProcessOptions {
            run_verification: true,
            ..ProcessOptions::default()
        };
        let (bank_forks, ..) =
            test_process_blockstore(&genesis_config, &blockstore, &opts, Arc::default());

        assert_eq!(frozen_bank_slots(&bank_forks.read().unwrap()), vec![0]); // slot 1 isn't "full", we stop at slot zero

        /* Add a complete slot such that the store looks like:

                                 slot 0 (all ticks)
                               /                  \
               slot 1 (all ticks but one)        slot 3 (all ticks)
                      |
               slot 2 (all ticks)
        */
        let opts = ProcessOptions {
            run_verification: true,
            ..ProcessOptions::default()
        };
        fill_blockstore_slot_with_ticks(&blockstore, ticks_per_slot, 3, 0, blockhash);
        // Slot 0 should not show up in the ending bank_forks_info
        let (bank_forks, ..) =
            test_process_blockstore(&genesis_config, &blockstore, &opts, Arc::default());

        // slot 1 isn't "full", we stop at slot zero
        assert_eq!(frozen_bank_slots(&bank_forks.read().unwrap()), vec![0, 3]);
    }

    #[test]
    fn test_process_blockstore_with_two_forks_and_squash() {
        agave_logger::setup();

        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let ticks_per_slot = genesis_config.ticks_per_slot;

        // Create a new ledger with slot 0 full of ticks
        let (ledger_path, blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
        debug!("ledger_path: {ledger_path:?}");
        let mut last_entry_hash = blockhash;

        /*
            Build a blockstore in the ledger with the following fork structure:

                 slot 0
                   |
                 slot 1
                 /   \
            slot 2   |
               /     |
            slot 3   |
                     |
                   slot 4 <-- set_root(true)

        */
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();

        // Fork 1, ending at slot 3
        let last_slot1_entry_hash =
            fill_blockstore_slot_with_ticks(&blockstore, ticks_per_slot, 1, 0, last_entry_hash);
        last_entry_hash = fill_blockstore_slot_with_ticks(
            &blockstore,
            ticks_per_slot,
            2,
            1,
            last_slot1_entry_hash,
        );
        let last_fork1_entry_hash =
            fill_blockstore_slot_with_ticks(&blockstore, ticks_per_slot, 3, 2, last_entry_hash);

        // Fork 2, ending at slot 4
        let last_fork2_entry_hash = fill_blockstore_slot_with_ticks(
            &blockstore,
            ticks_per_slot,
            4,
            1,
            last_slot1_entry_hash,
        );

        info!("last_fork1_entry.hash: {last_fork1_entry_hash:?}");
        info!("last_fork2_entry.hash: {last_fork2_entry_hash:?}");

        blockstore.set_roots([0, 1, 4].iter()).unwrap();

        let opts = ProcessOptions {
            run_verification: true,
            ..ProcessOptions::default()
        };
        let (bank_forks, ..) =
            test_process_blockstore(&genesis_config, &blockstore, &opts, Arc::default());
        let bank_forks = bank_forks.read().unwrap();

        // One fork, other one is ignored b/c not a descendant of the root
        assert_eq!(frozen_bank_slots(&bank_forks), vec![4]);

        assert!(&bank_forks[4]
            .parents()
            .iter()
            .map(|bank| bank.slot())
            .next()
            .is_none());

        // Ensure bank_forks holds the right banks
        verify_fork_infos(&bank_forks);

        assert_eq!(bank_forks.root(), 4);
    }

    #[test]
    fn test_process_blockstore_with_two_forks() {
        agave_logger::setup();

        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let ticks_per_slot = genesis_config.ticks_per_slot;

        // Create a new ledger with slot 0 full of ticks
        let (ledger_path, blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
        debug!("ledger_path: {ledger_path:?}");
        let mut last_entry_hash = blockhash;

        /*
            Build a blockstore in the ledger with the following fork structure:

                 slot 0
                   |
                 slot 1  <-- set_root(true)
                 /   \
            slot 2   |
               /     |
            slot 3   |
                     |
                   slot 4

        */
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();

        // Fork 1, ending at slot 3
        let last_slot1_entry_hash =
            fill_blockstore_slot_with_ticks(&blockstore, ticks_per_slot, 1, 0, last_entry_hash);
        last_entry_hash = fill_blockstore_slot_with_ticks(
            &blockstore,
            ticks_per_slot,
            2,
            1,
            last_slot1_entry_hash,
        );
        let last_fork1_entry_hash =
            fill_blockstore_slot_with_ticks(&blockstore, ticks_per_slot, 3, 2, last_entry_hash);

        // Fork 2, ending at slot 4
        let last_fork2_entry_hash = fill_blockstore_slot_with_ticks(
            &blockstore,
            ticks_per_slot,
            4,
            1,
            last_slot1_entry_hash,
        );

        info!("last_fork1_entry.hash: {last_fork1_entry_hash:?}");
        info!("last_fork2_entry.hash: {last_fork2_entry_hash:?}");

        blockstore.set_roots([0, 1].iter()).unwrap();

        let opts = ProcessOptions {
            run_verification: true,
            ..ProcessOptions::default()
        };
        let (bank_forks, ..) =
            test_process_blockstore(&genesis_config, &blockstore, &opts, Arc::default());
        let bank_forks = bank_forks.read().unwrap();

        assert_eq!(frozen_bank_slots(&bank_forks), vec![1, 2, 3, 4]);
        assert_eq!(bank_forks.working_bank().slot(), 4);
        assert_eq!(bank_forks.root(), 1);

        assert_eq!(
            &bank_forks[3]
                .parents()
                .iter()
                .map(|bank| bank.slot())
                .collect::<Vec<_>>(),
            &[2, 1]
        );
        assert_eq!(
            &bank_forks[4]
                .parents()
                .iter()
                .map(|bank| bank.slot())
                .collect::<Vec<_>>(),
            &[1]
        );

        assert_eq!(bank_forks.root(), 1);

        // Ensure bank_forks holds the right banks
        verify_fork_infos(&bank_forks);
    }

    #[test]
    fn test_process_blockstore_with_dead_slot() {
        agave_logger::setup();

        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let ticks_per_slot = genesis_config.ticks_per_slot;
        let (ledger_path, blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
        debug!("ledger_path: {ledger_path:?}");

        /*
                   slot 0
                     |
                   slot 1
                  /     \
                 /       \
           slot 2 (dead)  \
                           \
                        slot 3
        */
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        let slot1_blockhash =
            fill_blockstore_slot_with_ticks(&blockstore, ticks_per_slot, 1, 0, blockhash);
        fill_blockstore_slot_with_ticks(&blockstore, ticks_per_slot, 2, 1, slot1_blockhash);
        blockstore.set_dead_slot(2).unwrap();
        fill_blockstore_slot_with_ticks(&blockstore, ticks_per_slot, 3, 1, slot1_blockhash);

        let (bank_forks, ..) = test_process_blockstore(
            &genesis_config,
            &blockstore,
            &ProcessOptions::default(),
            Arc::default(),
        );
        let bank_forks = bank_forks.read().unwrap();

        assert_eq!(frozen_bank_slots(&bank_forks), vec![0, 1, 3]);
        assert_eq!(bank_forks.working_bank().slot(), 3);
        assert_eq!(
            &bank_forks[3]
                .parents()
                .iter()
                .map(|bank| bank.slot())
                .collect::<Vec<_>>(),
            &[1, 0]
        );
        verify_fork_infos(&bank_forks);
    }

    #[test]
    fn test_process_blockstore_with_dead_child() {
        agave_logger::setup();

        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let ticks_per_slot = genesis_config.ticks_per_slot;
        let (ledger_path, blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
        debug!("ledger_path: {ledger_path:?}");

        /*
                   slot 0
                     |
                   slot 1
                  /     \
                 /       \
              slot 2      \
               /           \
           slot 4 (dead)   slot 3
        */
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        let slot1_blockhash =
            fill_blockstore_slot_with_ticks(&blockstore, ticks_per_slot, 1, 0, blockhash);
        let slot2_blockhash =
            fill_blockstore_slot_with_ticks(&blockstore, ticks_per_slot, 2, 1, slot1_blockhash);
        fill_blockstore_slot_with_ticks(&blockstore, ticks_per_slot, 4, 2, slot2_blockhash);
        blockstore.set_dead_slot(4).unwrap();
        fill_blockstore_slot_with_ticks(&blockstore, ticks_per_slot, 3, 1, slot1_blockhash);

        let (bank_forks, ..) = test_process_blockstore(
            &genesis_config,
            &blockstore,
            &ProcessOptions::default(),
            Arc::default(),
        );
        let bank_forks = bank_forks.read().unwrap();

        // Should see the parent of the dead child
        assert_eq!(frozen_bank_slots(&bank_forks), vec![0, 1, 2, 3]);
        assert_eq!(bank_forks.working_bank().slot(), 3);

        assert_eq!(
            &bank_forks[3]
                .parents()
                .iter()
                .map(|bank| bank.slot())
                .collect::<Vec<_>>(),
            &[1, 0]
        );
        assert_eq!(
            &bank_forks[2]
                .parents()
                .iter()
                .map(|bank| bank.slot())
                .collect::<Vec<_>>(),
            &[1, 0]
        );
        assert_eq!(bank_forks.working_bank().slot(), 3);
        verify_fork_infos(&bank_forks);
    }

    #[test]
    fn test_root_with_all_dead_children() {
        agave_logger::setup();

        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let ticks_per_slot = genesis_config.ticks_per_slot;
        let (ledger_path, blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
        debug!("ledger_path: {ledger_path:?}");

        /*
                   slot 0
                 /        \
                /          \
           slot 1 (dead)  slot 2 (dead)
        */
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        fill_blockstore_slot_with_ticks(&blockstore, ticks_per_slot, 1, 0, blockhash);
        fill_blockstore_slot_with_ticks(&blockstore, ticks_per_slot, 2, 0, blockhash);
        blockstore.set_dead_slot(1).unwrap();
        blockstore.set_dead_slot(2).unwrap();
        let (bank_forks, ..) = test_process_blockstore(
            &genesis_config,
            &blockstore,
            &ProcessOptions::default(),
            Arc::default(),
        );
        let bank_forks = bank_forks.read().unwrap();

        // Should see only the parent of the dead children
        assert_eq!(frozen_bank_slots(&bank_forks), vec![0]);
        verify_fork_infos(&bank_forks);
    }

    #[test]
    fn test_process_blockstore_epoch_boundary_root() {
        agave_logger::setup();

        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let ticks_per_slot = genesis_config.ticks_per_slot;

        // Create a new ledger with slot 0 full of ticks
        let (ledger_path, blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
        let mut last_entry_hash = blockhash;

        let blockstore = Blockstore::open(ledger_path.path()).unwrap();

        // Let `last_slot` be the number of slots in the first two epochs
        let epoch_schedule = get_epoch_schedule(&genesis_config);
        let last_slot = epoch_schedule.get_last_slot_in_epoch(1);

        // Create a single chain of slots with all indexes in the range [0, v + 1]
        for i in 1..=last_slot + 1 {
            last_entry_hash = fill_blockstore_slot_with_ticks(
                &blockstore,
                ticks_per_slot,
                i,
                i - 1,
                last_entry_hash,
            );
        }

        // Set a root on the last slot of the last confirmed epoch
        let rooted_slots: Vec<Slot> = (0..=last_slot).collect();
        blockstore.set_roots(rooted_slots.iter()).unwrap();

        // Set a root on the next slot of the confirmed epoch
        blockstore
            .set_roots(std::iter::once(&(last_slot + 1)))
            .unwrap();

        // Check that we can properly restart the ledger / leader scheduler doesn't fail
        let opts = ProcessOptions {
            run_verification: true,
            ..ProcessOptions::default()
        };
        let (bank_forks, ..) =
            test_process_blockstore(&genesis_config, &blockstore, &opts, Arc::default());
        let bank_forks = bank_forks.read().unwrap();

        // There is one fork, head is last_slot + 1
        assert_eq!(frozen_bank_slots(&bank_forks), vec![last_slot + 1]);

        // The latest root should have purged all its parents
        assert!(&bank_forks[last_slot + 1]
            .parents()
            .iter()
            .map(|bank| bank.slot())
            .next()
            .is_none());
    }

    #[test]
    fn test_first_err() {
        assert_eq!(first_err(&[Ok(())]), Ok(()));
        assert_eq!(
            first_err(&[Ok(()), Err(TransactionError::AlreadyProcessed)]),
            Err(TransactionError::AlreadyProcessed)
        );
        assert_eq!(
            first_err(&[
                Ok(()),
                Err(TransactionError::AlreadyProcessed),
                Err(TransactionError::AccountInUse)
            ]),
            Err(TransactionError::AlreadyProcessed)
        );
        assert_eq!(
            first_err(&[
                Ok(()),
                Err(TransactionError::AccountInUse),
                Err(TransactionError::AlreadyProcessed)
            ]),
            Err(TransactionError::AccountInUse)
        );
        assert_eq!(
            first_err(&[
                Err(TransactionError::AccountInUse),
                Ok(()),
                Err(TransactionError::AlreadyProcessed)
            ]),
            Err(TransactionError::AccountInUse)
        );
    }

    #[test]
    fn test_process_empty_entry_is_registered() {
        agave_logger::setup();

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(2);
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let keypair = Keypair::new();
        let slot_entries = create_ticks(genesis_config.ticks_per_slot, 1, genesis_config.hash());
        let tx = system_transaction::transfer(
            &mint_keypair,
            &keypair.pubkey(),
            1,
            slot_entries.last().unwrap().hash,
        );

        // First, ensure the TX is rejected because of the unregistered last ID
        assert_eq!(
            bank.process_transaction(&tx),
            Err(TransactionError::BlockhashNotFound)
        );

        // Now ensure the TX is accepted despite pointing to the ID of an empty entry.
        process_entries_for_tests_without_scheduler(&bank, slot_entries).unwrap();
        assert_eq!(bank.process_transaction(&tx), Ok(()));
    }

    #[test]
    fn test_process_ledger_simple() {
        agave_logger::setup();
        let leader_pubkey = solana_pubkey::new_rand();
        let mint = 100;
        let hashes_per_tick = 10;
        let GenesisConfigInfo {
            mut genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config_with_leader(mint, &leader_pubkey, 50);
        genesis_config.poh_config.hashes_per_tick = Some(hashes_per_tick);
        let (ledger_path, mut last_entry_hash) =
            create_new_tmp_ledger_auto_delete!(&genesis_config);
        debug!("ledger_path: {ledger_path:?}");

        let deducted_from_mint = 3;
        let mut entries = vec![];
        let blockhash = genesis_config.hash();
        for _ in 0..deducted_from_mint {
            // Transfer one token from the mint to a random account
            let keypair = Keypair::new();
            let tx = system_transaction::transfer(&mint_keypair, &keypair.pubkey(), 1, blockhash);
            let entry = next_entry_mut(&mut last_entry_hash, 1, vec![tx]);
            entries.push(entry);

            // Add a second Transaction that will produce a
            // InstructionError<0, ResultWithNegativeLamports> error when processed
            let keypair2 = Keypair::new();
            let tx =
                system_transaction::transfer(&mint_keypair, &keypair2.pubkey(), 101, blockhash);
            let entry = next_entry_mut(&mut last_entry_hash, 1, vec![tx]);
            entries.push(entry);
        }

        let remaining_hashes = hashes_per_tick - entries.len() as u64;
        let tick_entry = next_entry_mut(&mut last_entry_hash, remaining_hashes, vec![]);
        entries.push(tick_entry);

        // Fill up the rest of slot 1 with ticks
        entries.extend(create_ticks(
            genesis_config.ticks_per_slot - 1,
            genesis_config.poh_config.hashes_per_tick.unwrap(),
            last_entry_hash,
        ));
        let last_blockhash = entries.last().unwrap().hash;

        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        blockstore
            .write_entries(
                1,
                0,
                0,
                genesis_config.ticks_per_slot,
                None,
                true,
                &Arc::new(Keypair::new()),
                entries,
                0,
            )
            .unwrap();
        let opts = ProcessOptions {
            run_verification: true,
            ..ProcessOptions::default()
        };
        let (bank_forks, ..) =
            test_process_blockstore(&genesis_config, &blockstore, &opts, Arc::default());
        let bank_forks = bank_forks.read().unwrap();

        assert_eq!(frozen_bank_slots(&bank_forks), vec![0, 1]);
        assert_eq!(bank_forks.root(), 0);
        assert_eq!(bank_forks.working_bank().slot(), 1);

        let bank = bank_forks[1].clone();
        assert_eq!(
            bank.get_balance(&mint_keypair.pubkey()),
            mint - deducted_from_mint
        );
        assert_eq!(bank.tick_height(), 2 * genesis_config.ticks_per_slot);
        assert_eq!(bank.last_blockhash(), last_blockhash);
    }

    #[test]
    fn test_process_ledger_with_one_tick_per_slot() {
        let GenesisConfigInfo {
            mut genesis_config, ..
        } = create_genesis_config(123);
        genesis_config.ticks_per_slot = 1;
        let (ledger_path, _blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);

        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        let opts = ProcessOptions {
            run_verification: true,
            ..ProcessOptions::default()
        };
        let (bank_forks, ..) =
            test_process_blockstore(&genesis_config, &blockstore, &opts, Arc::default());
        let bank_forks = bank_forks.read().unwrap();

        assert_eq!(frozen_bank_slots(&bank_forks), vec![0]);
        let bank = bank_forks[0].clone();
        assert_eq!(bank.tick_height(), 1);
    }

    #[test]
    fn test_process_ledger_options_full_leader_cache() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(123);
        let (ledger_path, _blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);

        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        let opts = ProcessOptions {
            full_leader_cache: true,
            ..ProcessOptions::default()
        };
        let (_bank_forks, leader_schedule) =
            test_process_blockstore(&genesis_config, &blockstore, &opts, Arc::default());
        assert_eq!(leader_schedule.max_schedules(), usize::MAX);
    }

    #[test]
    fn test_process_entries_tick() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(1000);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));

        // ensure bank can process a tick
        assert_eq!(bank.tick_height(), 0);
        let tick = next_entry(&genesis_config.hash(), 1, vec![]);
        assert_eq!(
            process_entries_for_tests_without_scheduler(&bank, vec![tick]),
            Ok(())
        );
        assert_eq!(bank.tick_height(), 1);
    }

    #[test]
    fn test_process_entries_2_entries_collision() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(1000);
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();

        let blockhash = bank.last_blockhash();

        // ensure bank can process 2 entries that have a common account and no tick is registered
        let tx = system_transaction::transfer(
            &mint_keypair,
            &keypair1.pubkey(),
            2,
            bank.last_blockhash(),
        );
        let entry_1 = next_entry(&blockhash, 1, vec![tx]);
        let tx = system_transaction::transfer(
            &mint_keypair,
            &keypair2.pubkey(),
            2,
            bank.last_blockhash(),
        );
        let entry_2 = next_entry(&entry_1.hash, 1, vec![tx]);
        assert_eq!(
            process_entries_for_tests_without_scheduler(&bank, vec![entry_1, entry_2]),
            Ok(())
        );
        assert_eq!(bank.get_balance(&keypair1.pubkey()), 2);
        assert_eq!(bank.get_balance(&keypair2.pubkey()), 2);
        assert_eq!(bank.last_blockhash(), blockhash);
    }

    #[test]
    fn test_process_entries_2_txes_collision() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(1000);
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();
        let keypair3 = Keypair::new();

        // fund: put 4 in each of 1 and 2
        assert_matches!(bank.transfer(4, &mint_keypair, &keypair1.pubkey()), Ok(_));
        assert_matches!(bank.transfer(4, &mint_keypair, &keypair2.pubkey()), Ok(_));

        // construct an Entry whose 2nd transaction would cause a lock conflict with previous entry
        let entry_1_to_mint = next_entry(
            &bank.last_blockhash(),
            1,
            vec![system_transaction::transfer(
                &keypair1,
                &mint_keypair.pubkey(),
                1,
                bank.last_blockhash(),
            )],
        );

        let entry_2_to_3_mint_to_1 = next_entry(
            &entry_1_to_mint.hash,
            1,
            vec![
                system_transaction::transfer(
                    &keypair2,
                    &keypair3.pubkey(),
                    2,
                    bank.last_blockhash(),
                ), // should be fine
                system_transaction::transfer(
                    &keypair1,
                    &mint_keypair.pubkey(),
                    2,
                    bank.last_blockhash(),
                ), // will collide
            ],
        );

        assert_eq!(
            process_entries_for_tests_without_scheduler(
                &bank,
                vec![entry_1_to_mint, entry_2_to_3_mint_to_1],
            ),
            Ok(())
        );

        assert_eq!(bank.get_balance(&keypair1.pubkey()), 1);
        assert_eq!(bank.get_balance(&keypair2.pubkey()), 2);
        assert_eq!(bank.get_balance(&keypair3.pubkey()), 2);
    }

    #[test]
    fn test_process_entries_2_txes_collision_and_error() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(1000);
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();
        let keypair3 = Keypair::new();
        let keypair4 = Keypair::new();

        // fund: put 4 in each of 1 and 2
        assert_matches!(bank.transfer(4, &mint_keypair, &keypair1.pubkey()), Ok(_));
        assert_matches!(bank.transfer(4, &mint_keypair, &keypair2.pubkey()), Ok(_));
        assert_matches!(bank.transfer(4, &mint_keypair, &keypair4.pubkey()), Ok(_));

        let good_tx = system_transaction::transfer(
            &keypair1,
            &mint_keypair.pubkey(),
            1,
            bank.last_blockhash(),
        );

        // construct an Entry whose 2nd transaction would cause a lock conflict with previous entry
        let entry_1_to_mint = next_entry(
            &bank.last_blockhash(),
            1,
            vec![
                good_tx.clone(),
                system_transaction::transfer(
                    &keypair4,
                    &keypair4.pubkey(),
                    1,
                    Hash::default(), // Should cause a transaction failure with BlockhashNotFound
                ),
            ],
        );

        let entry_2_to_3_mint_to_1 = next_entry(
            &entry_1_to_mint.hash,
            1,
            vec![
                system_transaction::transfer(
                    &keypair2,
                    &keypair3.pubkey(),
                    2,
                    bank.last_blockhash(),
                ), // should be fine
                system_transaction::transfer(
                    &keypair1,
                    &mint_keypair.pubkey(),
                    2,
                    bank.last_blockhash(),
                ), // will collide
            ],
        );

        assert_matches!(
            process_entries_for_tests_without_scheduler(
                &bank,
                vec![entry_1_to_mint.clone(), entry_2_to_3_mint_to_1.clone()],
            ),
            Err(TransactionError::BlockhashNotFound)
        );

        // First transaction in first entry was rolled-back, so keypair1 didn't lost 1 lamport
        assert_eq!(bank.get_balance(&keypair1.pubkey()), 4);
        assert_eq!(bank.get_balance(&keypair2.pubkey()), 4);

        // Check all accounts are unlocked
        let txs1 = entry_1_to_mint.transactions;
        let txs2 = entry_2_to_3_mint_to_1.transactions;
        let batch1 = bank.prepare_entry_batch(txs1).unwrap();
        for result in batch1.lock_results() {
            assert!(result.is_ok());
        }
        // txs1 and txs2 have accounts that conflict, so we must drop txs1 first
        drop(batch1);
        let batch2 = bank.prepare_entry_batch(txs2).unwrap();
        for result in batch2.lock_results() {
            assert!(result.is_ok());
        }
        drop(batch2);

        // ensure good_tx will succeed and was just rolled back above due to other failing tx
        let entry_3 = next_entry(&entry_2_to_3_mint_to_1.hash, 1, vec![good_tx]);
        assert_matches!(
            process_entries_for_tests_without_scheduler(&bank, vec![entry_3]),
            Ok(())
        );
        // First transaction in third entry succeeded, so keypair1 lost 1 lamport
        assert_eq!(bank.get_balance(&keypair1.pubkey()), 3);
    }

    #[test]
    fn test_transaction_result_does_not_affect_bankhash() {
        agave_logger::setup();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(1000);

        fn get_instruction_errors() -> Vec<InstructionError> {
            vec![
                InstructionError::GenericError,
                InstructionError::InvalidArgument,
                InstructionError::InvalidInstructionData,
                InstructionError::InvalidAccountData,
                InstructionError::AccountDataTooSmall,
                InstructionError::InsufficientFunds,
                InstructionError::IncorrectProgramId,
                InstructionError::MissingRequiredSignature,
                InstructionError::AccountAlreadyInitialized,
                InstructionError::UninitializedAccount,
                InstructionError::UnbalancedInstruction,
                InstructionError::ModifiedProgramId,
                InstructionError::ExternalAccountLamportSpend,
                InstructionError::ExternalAccountDataModified,
                InstructionError::ReadonlyLamportChange,
                InstructionError::ReadonlyDataModified,
                InstructionError::DuplicateAccountIndex,
                InstructionError::ExecutableModified,
                InstructionError::RentEpochModified,
                #[allow(deprecated)]
                InstructionError::NotEnoughAccountKeys,
                InstructionError::AccountDataSizeChanged,
                InstructionError::AccountNotExecutable,
                InstructionError::AccountBorrowFailed,
                InstructionError::AccountBorrowOutstanding,
                InstructionError::DuplicateAccountOutOfSync,
                InstructionError::Custom(0),
                InstructionError::InvalidError,
                InstructionError::ExecutableDataModified,
                InstructionError::ExecutableLamportChange,
                InstructionError::ExecutableAccountNotRentExempt,
                InstructionError::UnsupportedProgramId,
                InstructionError::CallDepth,
                InstructionError::MissingAccount,
                InstructionError::ReentrancyNotAllowed,
                InstructionError::MaxSeedLengthExceeded,
                InstructionError::InvalidSeeds,
                InstructionError::InvalidRealloc,
                InstructionError::ComputationalBudgetExceeded,
                InstructionError::PrivilegeEscalation,
                InstructionError::ProgramEnvironmentSetupFailure,
                InstructionError::ProgramFailedToComplete,
                InstructionError::ProgramFailedToCompile,
                InstructionError::Immutable,
                InstructionError::IncorrectAuthority,
                InstructionError::BorshIoError,
                InstructionError::AccountNotRentExempt,
                InstructionError::InvalidAccountOwner,
                InstructionError::ArithmeticOverflow,
                InstructionError::UnsupportedSysvar,
                InstructionError::IllegalOwner,
                InstructionError::MaxAccountsDataAllocationsExceeded,
                InstructionError::MaxAccountsExceeded,
                InstructionError::MaxInstructionTraceLengthExceeded,
                InstructionError::BuiltinProgramsMustConsumeComputeUnits,
            ]
        }

        declare_process_instruction!(MockBuiltinOk, 1, |_invoke_context| {
            // Always succeeds
            Ok(())
        });

        let mock_program_id = Pubkey::new_unique();

        let (bank, _bank_forks) = Bank::new_with_mockup_builtin_for_tests(
            &genesis_config,
            mock_program_id,
            MockBuiltinOk::vm,
        );

        let tx = Transaction::new_signed_with_payer(
            &[Instruction::new_with_bincode(
                mock_program_id,
                &10,
                Vec::new(),
            )],
            Some(&mint_keypair.pubkey()),
            &[&mint_keypair],
            bank.last_blockhash(),
        );

        let entry = next_entry(&bank.last_blockhash(), 1, vec![tx]);
        let result = process_entries_for_tests_without_scheduler(&bank, vec![entry]);
        bank.freeze();
        let ok_bank_details = SlotDetails::new_from_bank(&bank, true).unwrap();
        assert!(result.is_ok());

        declare_process_instruction!(MockBuiltinErr, 1, |invoke_context| {
            let instruction_errors = get_instruction_errors();

            let instruction_context = invoke_context
                .transaction_context
                .get_current_instruction_context()
                .expect("Failed to get instruction context");
            let err = instruction_context
                .get_instruction_data()
                .first()
                .expect("Failed to get instruction data");
            Err(instruction_errors
                .get(*err as usize)
                .expect("Invalid error index")
                .clone())
        });

        // Store details to compare against subsequent iterations
        let mut err_bank_details = None;

        (0..get_instruction_errors().len()).for_each(|err| {
            let (bank, _bank_forks) = Bank::new_with_mockup_builtin_for_tests(
                &genesis_config,
                mock_program_id,
                MockBuiltinErr::vm,
            );

            let tx = Transaction::new_signed_with_payer(
                &[Instruction::new_with_bincode(
                    mock_program_id,
                    &(err as u8),
                    Vec::new(),
                )],
                Some(&mint_keypair.pubkey()),
                &[&mint_keypair],
                bank.last_blockhash(),
            );

            let entry = next_entry(&bank.last_blockhash(), 1, vec![tx]);
            let bank = Arc::new(bank);
            let result = process_entries_for_tests_without_scheduler(&bank, vec![entry]);
            assert!(result.is_ok()); // No failing transaction error - only instruction errors
            bank.freeze();
            let bank_details = SlotDetails::new_from_bank(&bank, true).unwrap();

            // Transaction success/failure should not affect block hash ...
            assert_eq!(
                ok_bank_details
                    .bank_hash_components
                    .as_ref()
                    .unwrap()
                    .last_blockhash,
                bank_details
                    .bank_hash_components
                    .as_ref()
                    .unwrap()
                    .last_blockhash
            );
            // Though bankhash is not affected, bank_details should be different.
            assert_ne!(ok_bank_details, bank_details);
            // Different types of transaction failure should not affect bank hash
            if let Some(prev_bank_details) = &err_bank_details {
                assert_eq!(
                    *prev_bank_details,
                    bank_details,
                    "bank hash mismatched for tx error: {:?}",
                    get_instruction_errors()[err]
                );
            } else {
                err_bank_details = Some(bank_details);
            }
        });
    }

    #[test_case(false; "old")]
    #[test_case(true; "simd83")]
    fn test_process_entries_2nd_entry_collision_with_self_and_error(
        relax_intrabatch_account_locks: bool,
    ) {
        agave_logger::setup();

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(1000);
        let mut bank = Bank::new_for_tests(&genesis_config);
        if !relax_intrabatch_account_locks {
            bank.deactivate_feature(&agave_feature_set::relax_intrabatch_account_locks::id());
        }
        let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();
        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();
        let keypair3 = Keypair::new();

        // fund: put some money in each of 1 and 2
        assert_matches!(bank.transfer(5, &mint_keypair, &keypair1.pubkey()), Ok(_));
        assert_matches!(bank.transfer(4, &mint_keypair, &keypair2.pubkey()), Ok(_));

        // 3 entries: first has a transfer, 2nd has a conflict with 1st, 3rd has a conflict with itself
        let entry_1_to_mint = next_entry(
            &bank.last_blockhash(),
            1,
            vec![system_transaction::transfer(
                &keypair1,
                &mint_keypair.pubkey(),
                1,
                bank.last_blockhash(),
            )],
        );
        // should now be:
        // keypair1=4
        // keypair2=4
        // keypair3=0

        let entry_2_to_3_and_1_to_mint = next_entry(
            &entry_1_to_mint.hash,
            1,
            vec![
                system_transaction::transfer(
                    &keypair2,
                    &keypair3.pubkey(),
                    2,
                    bank.last_blockhash(),
                ), // should be fine
                system_transaction::transfer(
                    &keypair1,
                    &mint_keypair.pubkey(),
                    2,
                    bank.last_blockhash(),
                ), // will collide with preceding entry
            ],
        );
        // should now be:
        // keypair1=2
        // keypair2=2
        // keypair3=2

        let entry_conflict_itself = next_entry(
            &entry_2_to_3_and_1_to_mint.hash,
            1,
            vec![
                system_transaction::transfer(
                    &keypair1,
                    &keypair3.pubkey(),
                    1,
                    bank.last_blockhash(),
                ),
                system_transaction::transfer(
                    &keypair1,
                    &keypair2.pubkey(),
                    1,
                    bank.last_blockhash(),
                ), // will collide with preceding transaction
            ],
        );
        // if successful, becomes:
        // keypair1=0
        // keypair2=3
        // keypair3=3

        // succeeds following simd83 locking, fails otherwise
        let result = process_entries_for_tests_without_scheduler(
            &bank,
            vec![
                entry_1_to_mint,
                entry_2_to_3_and_1_to_mint,
                entry_conflict_itself,
            ],
        );

        let balances = [
            bank.get_balance(&keypair1.pubkey()),
            bank.get_balance(&keypair2.pubkey()),
            bank.get_balance(&keypair3.pubkey()),
        ];

        if relax_intrabatch_account_locks {
            assert!(result.is_ok());
            assert_eq!(balances, [0, 3, 3]);
        } else {
            assert!(result.is_err());
            assert_eq!(balances, [2, 2, 2]);
        }
    }

    #[test_case(false; "old")]
    #[test_case(true; "simd83")]
    fn test_process_entry_duplicate_transaction(relax_intrabatch_account_locks: bool) {
        agave_logger::setup();

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(1000);
        let mut bank = Bank::new_for_tests(&genesis_config);
        if !relax_intrabatch_account_locks {
            bank.deactivate_feature(&agave_feature_set::relax_intrabatch_account_locks::id());
        }
        let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();
        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();

        // fund: put some money in each of 1 and 2
        assert_matches!(bank.transfer(5, &mint_keypair, &keypair1.pubkey()), Ok(_));
        assert_matches!(bank.transfer(5, &mint_keypair, &keypair2.pubkey()), Ok(_));

        // one entry, two instances of the same transaction. this entry is invalid
        // without simd83: due to lock conflicts
        // with simd83: due to message hash duplication
        let entry_1_to_2_twice = next_entry(
            &bank.last_blockhash(),
            1,
            vec![
                system_transaction::transfer(
                    &keypair1,
                    &keypair2.pubkey(),
                    1,
                    bank.last_blockhash(),
                ),
                system_transaction::transfer(
                    &keypair1,
                    &keypair2.pubkey(),
                    1,
                    bank.last_blockhash(),
                ),
            ],
        );
        // should now be:
        // keypair1=5
        // keypair2=5

        // succeeds following simd83 locking, fails otherwise
        let result = process_entries_for_tests_without_scheduler(&bank, vec![entry_1_to_2_twice]);

        let balances = [
            bank.get_balance(&keypair1.pubkey()),
            bank.get_balance(&keypair2.pubkey()),
        ];

        assert_eq!(balances, [5, 5]);
        if relax_intrabatch_account_locks {
            assert_eq!(result, Err(TransactionError::AlreadyProcessed));
        } else {
            assert_eq!(result, Err(TransactionError::AccountInUse));
        }
    }

    #[test]
    fn test_process_entries_2_entries_par() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(1000);
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();
        let keypair3 = Keypair::new();
        let keypair4 = Keypair::new();

        //load accounts
        let tx = system_transaction::transfer(
            &mint_keypair,
            &keypair1.pubkey(),
            1,
            bank.last_blockhash(),
        );
        assert_eq!(bank.process_transaction(&tx), Ok(()));
        let tx = system_transaction::transfer(
            &mint_keypair,
            &keypair2.pubkey(),
            1,
            bank.last_blockhash(),
        );
        assert_eq!(bank.process_transaction(&tx), Ok(()));

        // ensure bank can process 2 entries that do not have a common account and no tick is registered
        let blockhash = bank.last_blockhash();
        let tx =
            system_transaction::transfer(&keypair1, &keypair3.pubkey(), 1, bank.last_blockhash());
        let entry_1 = next_entry(&blockhash, 1, vec![tx]);
        let tx =
            system_transaction::transfer(&keypair2, &keypair4.pubkey(), 1, bank.last_blockhash());
        let entry_2 = next_entry(&entry_1.hash, 1, vec![tx]);
        assert_eq!(
            process_entries_for_tests_without_scheduler(&bank, vec![entry_1, entry_2]),
            Ok(())
        );
        assert_eq!(bank.get_balance(&keypair3.pubkey()), 1);
        assert_eq!(bank.get_balance(&keypair4.pubkey()), 1);
        assert_eq!(bank.last_blockhash(), blockhash);
    }

    #[test]
    fn test_process_entry_tx_random_execution_with_error() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(1_000_000_000);
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        const NUM_TRANSFERS_PER_ENTRY: usize = 8;
        const NUM_TRANSFERS: usize = NUM_TRANSFERS_PER_ENTRY * 32;
        // large enough to scramble locks and results

        let keypairs: Vec<_> = (0..NUM_TRANSFERS * 2).map(|_| Keypair::new()).collect();

        // give everybody one lamport
        for keypair in &keypairs {
            bank.transfer(1, &mint_keypair, &keypair.pubkey())
                .expect("funding failed");
        }
        let mut hash = bank.last_blockhash();

        let present_account_key = Keypair::new();
        let present_account = AccountSharedData::new(1, 10, &Pubkey::default());
        bank.store_account(&present_account_key.pubkey(), &present_account);

        let entries: Vec<_> = (0..NUM_TRANSFERS)
            .step_by(NUM_TRANSFERS_PER_ENTRY)
            .map(|i| {
                let mut transactions = (0..NUM_TRANSFERS_PER_ENTRY)
                    .map(|j| {
                        system_transaction::transfer(
                            &keypairs[i + j],
                            &keypairs[i + j + NUM_TRANSFERS].pubkey(),
                            1,
                            bank.last_blockhash(),
                        )
                    })
                    .collect::<Vec<_>>();

                transactions.push(system_transaction::create_account(
                    &mint_keypair,
                    &present_account_key, // puts a TX error in results
                    bank.last_blockhash(),
                    1,
                    0,
                    &solana_pubkey::new_rand(),
                ));

                next_entry_mut(&mut hash, 0, transactions)
            })
            .collect();
        assert_eq!(
            process_entries_for_tests_without_scheduler(&bank, entries),
            Ok(())
        );
    }

    #[test]
    fn test_process_entry_tx_random_execution_no_error() {
        // entropy multiplier should be big enough to provide sufficient entropy
        // but small enough to not take too much time while executing the test.
        let entropy_multiplier: usize = 25;
        let initial_lamports = 100;

        // number of accounts need to be in multiple of 4 for correct
        // execution of the test.
        let num_accounts = entropy_multiplier * 4;
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config((num_accounts + 1) as u64 * initial_lamports);

        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let mut keypairs: Vec<Keypair> = vec![];

        for _ in 0..num_accounts {
            let keypair = Keypair::new();
            let create_account_tx = system_transaction::transfer(
                &mint_keypair,
                &keypair.pubkey(),
                0,
                bank.last_blockhash(),
            );
            assert_eq!(bank.process_transaction(&create_account_tx), Ok(()));
            assert_matches!(
                bank.transfer(initial_lamports, &mint_keypair, &keypair.pubkey()),
                Ok(_)
            );
            keypairs.push(keypair);
        }

        let mut tx_vector: Vec<Transaction> = vec![];

        for i in (0..num_accounts).step_by(4) {
            tx_vector.append(&mut vec![
                system_transaction::transfer(
                    &keypairs[i + 1],
                    &keypairs[i].pubkey(),
                    initial_lamports,
                    bank.last_blockhash(),
                ),
                system_transaction::transfer(
                    &keypairs[i + 3],
                    &keypairs[i + 2].pubkey(),
                    initial_lamports,
                    bank.last_blockhash(),
                ),
            ]);
        }

        // Transfer lamports to each other
        let entry = next_entry(&bank.last_blockhash(), 1, tx_vector);
        assert_eq!(
            process_entries_for_tests_without_scheduler(&bank, vec![entry]),
            Ok(())
        );
        bank.squash();

        // Even number keypair should have balance of 2 * initial_lamports and
        // odd number keypair should have balance of 0, which proves
        // that even in case of random order of execution, overall state remains
        // consistent.
        for (i, keypair) in keypairs.iter().enumerate() {
            if i % 2 == 0 {
                assert_eq!(bank.get_balance(&keypair.pubkey()), 2 * initial_lamports);
            } else {
                assert_eq!(bank.get_balance(&keypair.pubkey()), 0);
            }
        }
    }

    #[test]
    fn test_process_entries_2_entries_tick() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(1000);
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();
        let keypair3 = Keypair::new();
        let keypair4 = Keypair::new();

        //load accounts
        let tx = system_transaction::transfer(
            &mint_keypair,
            &keypair1.pubkey(),
            1,
            bank.last_blockhash(),
        );
        assert_eq!(bank.process_transaction(&tx), Ok(()));
        let tx = system_transaction::transfer(
            &mint_keypair,
            &keypair2.pubkey(),
            1,
            bank.last_blockhash(),
        );
        assert_eq!(bank.process_transaction(&tx), Ok(()));

        let blockhash = bank.last_blockhash();
        while blockhash == bank.last_blockhash() {
            bank.register_default_tick_for_test();
        }

        // ensure bank can process 2 entries that do not have a common account and tick is registered
        let tx = system_transaction::transfer(&keypair2, &keypair3.pubkey(), 1, blockhash);
        let entry_1 = next_entry(&blockhash, 1, vec![tx]);
        let tick = next_entry(&entry_1.hash, 1, vec![]);
        let tx =
            system_transaction::transfer(&keypair1, &keypair4.pubkey(), 1, bank.last_blockhash());
        let entry_2 = next_entry(&tick.hash, 1, vec![tx]);
        assert_eq!(
            process_entries_for_tests_without_scheduler(
                &bank,
                vec![entry_1, tick, entry_2.clone()],
            ),
            Ok(())
        );
        assert_eq!(bank.get_balance(&keypair3.pubkey()), 1);
        assert_eq!(bank.get_balance(&keypair4.pubkey()), 1);

        // ensure that an error is returned for an empty account (keypair2)
        let tx =
            system_transaction::transfer(&keypair2, &keypair3.pubkey(), 1, bank.last_blockhash());
        let entry_3 = next_entry(&entry_2.hash, 1, vec![tx]);
        assert_eq!(
            process_entries_for_tests_without_scheduler(&bank, vec![entry_3]),
            Err(TransactionError::AccountNotFound)
        );
    }

    #[test]
    fn test_update_transaction_statuses() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(11_000);
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        // Make sure instruction errors still update the signature cache
        let pubkey = solana_pubkey::new_rand();
        bank.transfer(1_000, &mint_keypair, &pubkey).unwrap();
        assert_eq!(bank.transaction_count(), 1);
        assert_eq!(bank.get_balance(&pubkey), 1_000);
        assert_eq!(
            bank.transfer(10_001, &mint_keypair, &pubkey),
            Err(TransactionError::InstructionError(
                0,
                SystemError::ResultWithNegativeLamports.into(),
            ))
        );
        assert_eq!(
            bank.transfer(10_001, &mint_keypair, &pubkey),
            Err(TransactionError::AlreadyProcessed)
        );

        // Make sure fees-only transactions still update the signature cache
        let missing_program_id = Pubkey::new_unique();
        let tx = Transaction::new_signed_with_payer(
            &[Instruction::new_with_bincode(
                missing_program_id,
                &10,
                Vec::new(),
            )],
            Some(&mint_keypair.pubkey()),
            &[&mint_keypair],
            bank.last_blockhash(),
        );
        // First process attempt will fail but still update status cache
        assert_eq!(
            bank.process_transaction(&tx),
            Err(TransactionError::ProgramAccountNotFound)
        );
        // Second attempt will be rejected since tx was already in status cache
        assert_eq!(
            bank.process_transaction(&tx),
            Err(TransactionError::AlreadyProcessed)
        );

        // Make sure other errors don't update the signature cache
        let tx = system_transaction::transfer(&mint_keypair, &pubkey, 1000, Hash::default());
        let signature = tx.signatures[0];

        // Should fail with blockhash not found
        assert_eq!(
            bank.process_transaction(&tx).map(|_| signature),
            Err(TransactionError::BlockhashNotFound)
        );

        // Should fail again with blockhash not found
        assert_eq!(
            bank.process_transaction(&tx).map(|_| signature),
            Err(TransactionError::BlockhashNotFound)
        );
    }

    #[test_case(false; "old")]
    #[test_case(true; "simd83")]
    fn test_update_transaction_statuses_fail(relax_intrabatch_account_locks: bool) {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(11_000);
        let mut bank = Bank::new_for_tests(&genesis_config);
        if !relax_intrabatch_account_locks {
            bank.deactivate_feature(&agave_feature_set::relax_intrabatch_account_locks::id());
        }
        let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();
        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();
        let success_tx = system_transaction::transfer(
            &mint_keypair,
            &keypair1.pubkey(),
            1,
            bank.last_blockhash(),
        );
        let test_tx = system_transaction::transfer(
            &mint_keypair,
            &keypair2.pubkey(),
            2,
            bank.last_blockhash(),
        );

        let entry_1_to_mint = next_entry(
            &bank.last_blockhash(),
            1,
            vec![
                success_tx,
                test_tx.clone(), // will collide
            ],
        );

        // succeeds with simd83, fails because of account locking conflict otherwise
        assert_eq!(
            process_entries_for_tests_without_scheduler(&bank, vec![entry_1_to_mint]),
            if relax_intrabatch_account_locks {
                Ok(())
            } else {
                Err(TransactionError::AccountInUse)
            }
        );

        // fails with simd83 as already processed, succeeds otherwise
        assert_eq!(
            bank.process_transaction(&test_tx),
            if relax_intrabatch_account_locks {
                Err(TransactionError::AlreadyProcessed)
            } else {
                Ok(())
            }
        );
    }

    #[test]
    fn test_halt_at_slot_starting_snapshot_root() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(123);

        // Create roots at slots 0, 1
        let forks = tr(0) / tr(1);
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        blockstore.add_tree(
            forks,
            false,
            true,
            genesis_config.ticks_per_slot,
            genesis_config.hash(),
        );
        blockstore.set_roots([0, 1].iter()).unwrap();

        // Specify halting at slot 0
        let opts = ProcessOptions {
            run_verification: true,
            halt_at_slot: Some(0),
            ..ProcessOptions::default()
        };
        let (bank_forks, ..) =
            test_process_blockstore(&genesis_config, &blockstore, &opts, Arc::default());
        let bank_forks = bank_forks.read().unwrap();

        // Should be able to fetch slot 0 because we specified halting at slot 0, even
        // if there is a greater root at slot 1.
        assert!(bank_forks.get(0).is_some());
    }

    #[test]
    fn test_process_blockstore_from_root() {
        let GenesisConfigInfo {
            mut genesis_config, ..
        } = create_genesis_config(123);

        let ticks_per_slot = 1;
        genesis_config.ticks_per_slot = ticks_per_slot;
        let (ledger_path, blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();

        /*
          Build a blockstore in the ledger with the following fork structure:

               slot 0 (all ticks)
                 |
               slot 1 (all ticks)
                 |
               slot 2 (all ticks)
                 |
               slot 3 (all ticks) -> root
                 |
               slot 4 (all ticks)
                 |
               slot 5 (all ticks) -> root
                 |
               slot 6 (all ticks)
        */

        let mut last_hash = blockhash;
        for i in 0..6 {
            last_hash =
                fill_blockstore_slot_with_ticks(&blockstore, ticks_per_slot, i + 1, i, last_hash);
        }
        blockstore.set_roots([3, 5].iter()).unwrap();

        // Set up bank1
        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis_config));
        let bank0 = bank_forks.read().unwrap().get_with_scheduler(0).unwrap();
        let opts = ProcessOptions {
            run_verification: true,
            ..ProcessOptions::default()
        };
        let replay_tx_thread_pool = create_thread_pool(1);
        process_bank_0(
            &bank0,
            &blockstore,
            &replay_tx_thread_pool,
            &opts,
            None,
            None,
        )
        .unwrap();
        let bank0_last_blockhash = bank0.last_blockhash();
        let bank1 = bank_forks.write().unwrap().insert(Bank::new_from_parent(
            bank0.clone_without_scheduler(),
            &Pubkey::default(),
            1,
        ));
        confirm_full_slot(
            &blockstore,
            &bank1,
            &replay_tx_thread_pool,
            &opts,
            &mut ConfirmationProgress::new(bank0_last_blockhash),
            None,
            None,
            None,
            &mut ExecuteTimings::default(),
        )
        .unwrap();
        bank_forks.write().unwrap().set_root(1, None, None);

        let leader_schedule_cache = LeaderScheduleCache::new_from_bank(&bank1);

        // Test process_blockstore_from_root() from slot 1 onwards
        process_blockstore_from_root(
            &blockstore,
            &bank_forks,
            &leader_schedule_cache,
            &opts,
            None,
            None,
            None, // snapshot_controller
        )
        .unwrap();

        let bank_forks = bank_forks.read().unwrap();

        assert_eq!(frozen_bank_slots(&bank_forks), vec![5, 6]);
        assert_eq!(bank_forks.working_bank().slot(), 6);
        assert_eq!(bank_forks.root(), 5);

        // Verify the parents of the head of the fork
        assert_eq!(
            &bank_forks[6]
                .parents()
                .iter()
                .map(|bank| bank.slot())
                .collect::<Vec<_>>(),
            &[5]
        );

        // Check that bank forks has the correct banks
        verify_fork_infos(&bank_forks);
    }

    #[test]
    #[ignore]
    fn test_process_entries_stress() {
        // this test throws lots of rayon threads at process_entries()
        //  finds bugs in very low-layer stuff
        agave_logger::setup();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(1_000_000_000);
        let mut bank = Arc::new(Bank::new_for_tests(&genesis_config));

        const NUM_TRANSFERS_PER_ENTRY: usize = 8;
        const NUM_TRANSFERS: usize = NUM_TRANSFERS_PER_ENTRY * 32;

        let keypairs: Vec<_> = (0..NUM_TRANSFERS * 2).map(|_| Keypair::new()).collect();

        // give everybody one lamport
        for keypair in &keypairs {
            bank.transfer(1, &mint_keypair, &keypair.pubkey())
                .expect("funding failed");
        }

        let present_account_key = Keypair::new();
        let present_account = AccountSharedData::new(1, 10, &Pubkey::default());
        bank.store_account(&present_account_key.pubkey(), &present_account);

        let mut i = 0;
        let mut hash = bank.last_blockhash();
        let mut root: Option<Arc<Bank>> = None;
        loop {
            let entries: Vec<_> = (0..NUM_TRANSFERS)
                .step_by(NUM_TRANSFERS_PER_ENTRY)
                .map(|i| {
                    next_entry_mut(&mut hash, 0, {
                        let mut transactions = (i..i + NUM_TRANSFERS_PER_ENTRY)
                            .map(|i| {
                                system_transaction::transfer(
                                    &keypairs[i],
                                    &keypairs[i + NUM_TRANSFERS].pubkey(),
                                    1,
                                    bank.last_blockhash(),
                                )
                            })
                            .collect::<Vec<_>>();

                        transactions.push(system_transaction::create_account(
                            &mint_keypair,
                            &present_account_key, // puts a TX error in results
                            bank.last_blockhash(),
                            100,
                            100,
                            &solana_pubkey::new_rand(),
                        ));
                        transactions
                    })
                })
                .collect();
            info!("paying iteration {i}");
            process_entries_for_tests_without_scheduler(&bank, entries).expect("paying failed");

            let entries: Vec<_> = (0..NUM_TRANSFERS)
                .step_by(NUM_TRANSFERS_PER_ENTRY)
                .map(|i| {
                    next_entry_mut(
                        &mut hash,
                        0,
                        (i..i + NUM_TRANSFERS_PER_ENTRY)
                            .map(|i| {
                                system_transaction::transfer(
                                    &keypairs[i + NUM_TRANSFERS],
                                    &keypairs[i].pubkey(),
                                    1,
                                    bank.last_blockhash(),
                                )
                            })
                            .collect::<Vec<_>>(),
                    )
                })
                .collect();

            info!("refunding iteration {i}");
            process_entries_for_tests_without_scheduler(&bank, entries).expect("refunding failed");

            // advance to next block
            process_entries_for_tests_without_scheduler(
                &bank,
                (0..bank.ticks_per_slot())
                    .map(|_| next_entry_mut(&mut hash, 1, vec![]))
                    .collect::<Vec<_>>(),
            )
            .expect("process ticks failed");

            if i % 16 == 0 {
                if let Some(old_root) = root {
                    old_root.squash();
                }
                root = Some(bank.clone());
            }
            i += 1;

            let slot = bank.slot() + rng().random_range(1..3);
            bank = Arc::new(Bank::new_from_parent(bank, &Pubkey::default(), slot));
        }
    }

    #[test]
    fn test_process_ledger_ticks_ordering() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(100);
        let (bank0, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let genesis_hash = genesis_config.hash();
        let keypair = Keypair::new();

        // Simulate a slot of virtual ticks, creates a new blockhash
        let mut entries = create_ticks(genesis_config.ticks_per_slot, 1, genesis_hash);

        // The new blockhash is going to be the hash of the last tick in the block
        let new_blockhash = entries.last().unwrap().hash;
        // Create an transaction that references the new blockhash, should still
        // be able to find the blockhash if we process transactions all in the same
        // batch
        let tx = system_transaction::transfer(&mint_keypair, &keypair.pubkey(), 1, new_blockhash);
        let entry = next_entry(&new_blockhash, 1, vec![tx]);
        entries.push(entry);

        process_entries_for_tests_without_scheduler(&bank0, entries).unwrap();
        assert_eq!(bank0.get_balance(&keypair.pubkey()), 1)
    }

    fn get_epoch_schedule(genesis_config: &GenesisConfig) -> EpochSchedule {
        let bank = Bank::new_for_tests(genesis_config);
        bank.epoch_schedule().clone()
    }

    fn frozen_bank_slots(bank_forks: &BankForks) -> Vec<Slot> {
        let mut slots: Vec<_> = bank_forks
            .frozen_banks()
            .map(|(slot, _bank)| slot)
            .collect();
        slots.sort_unstable();
        slots
    }

    // Check that `bank_forks` contains all the ancestors and banks for each fork identified in
    // `bank_forks_info`
    fn verify_fork_infos(bank_forks: &BankForks) {
        for slot in frozen_bank_slots(bank_forks) {
            let head_bank = &bank_forks[slot];
            let mut parents = head_bank.parents();
            parents.push(head_bank.clone());

            // Ensure the tip of each fork and all its parents are in the given bank_forks
            for parent in parents {
                let parent_bank = &bank_forks[parent.slot()];
                assert_eq!(parent_bank.slot(), parent.slot());
                assert!(parent_bank.is_frozen());
            }
        }
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
        let account_not_found_tx = system_transaction::transfer(
            &keypair,
            &solana_pubkey::new_rand(),
            42,
            bank.last_blockhash(),
        );
        let account_not_found_sig = account_not_found_tx.signatures[0];
        let invalid_blockhash_tx = system_transaction::transfer(
            &mint_keypair,
            &solana_pubkey::new_rand(),
            42,
            Hash::default(),
        );
        let txs = vec![account_not_found_tx, invalid_blockhash_tx];
        let batch = bank.prepare_batch_for_tests(txs);
        let (commit_results, _) = batch.bank().load_execute_and_commit_transactions(
            &batch,
            MAX_PROCESSING_AGE,
            ExecutionRecordingConfig::new_single_setting(false),
            &mut ExecuteTimings::default(),
            None,
        );
        let (err, signature) = do_get_first_error(&batch, &commit_results).unwrap();
        assert_eq!(err.unwrap_err(), TransactionError::AccountNotFound);
        assert_eq!(signature, account_not_found_sig);
    }

    #[test]
    fn test_replay_vote_sender() {
        let validator_keypairs: Vec<_> =
            (0..10).map(|_| ValidatorVoteKeypairs::new_rand()).collect();
        let GenesisConfigInfo {
            genesis_config,
            voting_keypair: _,
            ..
        } = create_genesis_config_with_vote_accounts(
            1_000_000_000,
            &validator_keypairs,
            vec![100; validator_keypairs.len()],
        );
        let (bank0, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        bank0.freeze();

        let bank1 = bank_forks
            .write()
            .unwrap()
            .insert(Bank::new_from_parent(
                bank0.clone(),
                &solana_pubkey::new_rand(),
                1,
            ))
            .clone_without_scheduler();

        // The new blockhash is going to be the hash of the last tick in the block
        let bank_1_blockhash = bank1.last_blockhash();

        // Create an transaction that references the new blockhash, should still
        // be able to find the blockhash if we process transactions all in the same
        // batch
        let mut expected_successful_voter_pubkeys = BTreeSet::new();
        let vote_txs: Vec<_> = validator_keypairs
            .iter()
            .enumerate()
            .map(|(i, validator_keypairs)| {
                let tower_sync = TowerSync::new_from_slots(vec![0], bank0.hash(), None);
                if i % 3 == 0 {
                    // These votes are correct
                    expected_successful_voter_pubkeys
                        .insert(validator_keypairs.vote_keypair.pubkey());
                    vote_transaction::new_tower_sync_transaction(
                        tower_sync,
                        bank_1_blockhash,
                        &validator_keypairs.node_keypair,
                        &validator_keypairs.vote_keypair,
                        &validator_keypairs.vote_keypair,
                        None,
                    )
                } else if i % 3 == 1 {
                    // These have the wrong authorized voter
                    vote_transaction::new_tower_sync_transaction(
                        tower_sync,
                        bank_1_blockhash,
                        &validator_keypairs.node_keypair,
                        &validator_keypairs.vote_keypair,
                        &Keypair::new(),
                        None,
                    )
                } else {
                    // These have an invalid vote for non-existent bank 2
                    vote_transaction::new_tower_sync_transaction(
                        TowerSync::from(vec![(bank1.slot() + 1, 1)]),
                        bank_1_blockhash,
                        &validator_keypairs.node_keypair,
                        &validator_keypairs.vote_keypair,
                        &validator_keypairs.vote_keypair,
                        None,
                    )
                }
            })
            .collect();
        let entry = next_entry(&bank_1_blockhash, 1, vote_txs);
        let (replay_vote_sender, replay_vote_receiver) = crossbeam_channel::unbounded();
        let _ = process_entries_for_tests(
            &BankWithScheduler::new_without_scheduler(bank1),
            vec![entry],
            None,
            Some(&replay_vote_sender),
        );
        let successes: BTreeSet<Pubkey> = replay_vote_receiver
            .try_iter()
            .map(|(vote_pubkey, ..)| vote_pubkey)
            .collect();
        assert_eq!(successes, expected_successful_voter_pubkeys);
    }

    fn make_slot_with_vote_tx(
        blockstore: &Blockstore,
        ticks_per_slot: u64,
        tx_landed_slot: Slot,
        parent_slot: Slot,
        parent_blockhash: &Hash,
        vote_tx: Transaction,
        slot_leader_keypair: &Arc<Keypair>,
    ) {
        // Add votes to `last_slot` so that `root` will be confirmed
        let vote_entry = next_entry(parent_blockhash, 1, vec![vote_tx]);
        let mut entries = create_ticks(ticks_per_slot, 0, vote_entry.hash);
        entries.insert(0, vote_entry);
        blockstore
            .write_entries(
                tx_landed_slot,
                0,
                0,
                ticks_per_slot,
                Some(parent_slot),
                true,
                slot_leader_keypair,
                entries,
                0,
            )
            .unwrap();
    }

    fn run_test_process_blockstore_with_supermajority_root(
        blockstore_root: Option<Slot>,
        blockstore_access_type: AccessType,
    ) {
        agave_logger::setup();
        /*
            Build fork structure:
                 slot 0
                   |
                 slot 1 <- (blockstore root)
                 /    \
            slot 2    |
               |      |
            slot 4    |
                    slot 5
                      |
                `expected_root_slot`
                     /    \
                  ...    minor fork
                  /
            `last_slot`
                 |
            `really_last_slot`
        */
        let starting_fork_slot = 5;
        let mut main_fork = tr(starting_fork_slot);
        let mut main_fork_ref = main_fork.root_mut().get_mut();

        // Make enough slots to make a root slot > blockstore_root
        let expected_root_slot = starting_fork_slot + blockstore_root.unwrap_or(0);
        let really_expected_root_slot = expected_root_slot + 1;
        let last_main_fork_slot = expected_root_slot + MAX_LOCKOUT_HISTORY as u64 + 1;
        let really_last_main_fork_slot = last_main_fork_slot + 1;

        // Make `minor_fork`
        let last_minor_fork_slot = really_last_main_fork_slot + 1;
        let minor_fork = tr(last_minor_fork_slot);

        // Make 'main_fork`
        for slot in starting_fork_slot + 1..last_main_fork_slot {
            if slot - 1 == expected_root_slot {
                main_fork_ref.push_front(minor_fork.clone());
            }
            main_fork_ref.push_front(tr(slot));
            main_fork_ref = main_fork_ref.front_mut().unwrap().get_mut();
        }
        let forks = tr(0) / (tr(1) / (tr(2) / (tr(4))) / main_fork);
        let validator_keypairs = ValidatorVoteKeypairs::new_rand();
        let GenesisConfigInfo { genesis_config, .. } =
            genesis_utils::create_genesis_config_with_vote_accounts(
                10_000,
                &[&validator_keypairs],
                vec![100],
            );
        let ticks_per_slot = genesis_config.ticks_per_slot();
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        blockstore.add_tree(forks, false, true, ticks_per_slot, genesis_config.hash());

        if let Some(blockstore_root) = blockstore_root {
            blockstore
                .set_roots(std::iter::once(&blockstore_root))
                .unwrap();
        }

        let opts = ProcessOptions {
            run_verification: true,
            ..ProcessOptions::default()
        };

        let (bank_forks, ..) = test_process_blockstore_with_custom_options(
            &genesis_config,
            &blockstore,
            &opts,
            blockstore_access_type.clone(),
        );
        let bank_forks = bank_forks.read().unwrap();

        // prepare to add votes
        let last_vote_bank_hash = bank_forks.get(last_main_fork_slot - 1).unwrap().hash();
        let last_vote_blockhash = bank_forks
            .get(last_main_fork_slot - 1)
            .unwrap()
            .last_blockhash();
        let tower_sync = TowerSync::new_from_slot(last_main_fork_slot - 1, last_vote_bank_hash);
        let vote_tx = vote_transaction::new_tower_sync_transaction(
            tower_sync,
            last_vote_blockhash,
            &validator_keypairs.node_keypair,
            &validator_keypairs.vote_keypair,
            &validator_keypairs.vote_keypair,
            None,
        );

        // Add votes to `last_slot` so that `root` will be confirmed
        let leader_keypair = Arc::new(validator_keypairs.node_keypair);
        make_slot_with_vote_tx(
            &blockstore,
            ticks_per_slot,
            last_main_fork_slot,
            last_main_fork_slot - 1,
            &last_vote_blockhash,
            vote_tx,
            &leader_keypair,
        );

        let (bank_forks, ..) = test_process_blockstore_with_custom_options(
            &genesis_config,
            &blockstore,
            &opts,
            blockstore_access_type.clone(),
        );
        let bank_forks = bank_forks.read().unwrap();

        assert_eq!(bank_forks.root(), expected_root_slot);
        assert_eq!(
            bank_forks.frozen_banks().count() as u64,
            last_minor_fork_slot - really_expected_root_slot + 1
        );

        // Minor fork at `last_main_fork_slot + 1` was above the `expected_root_slot`
        // so should not have been purged
        //
        // Fork at slot 2 was purged because it was below the `expected_root_slot`
        for slot in 0..=last_minor_fork_slot {
            // this slot will be created below
            if slot == really_last_main_fork_slot {
                continue;
            }
            if slot >= expected_root_slot {
                let bank = bank_forks.get(slot).unwrap();
                assert_eq!(bank.slot(), slot);
                assert!(bank.is_frozen());
            } else {
                assert!(bank_forks.get(slot).is_none());
            }
        }

        // really prepare to add votes
        let last_vote_bank_hash = bank_forks.get(last_main_fork_slot).unwrap().hash();
        let last_vote_blockhash = bank_forks
            .get(last_main_fork_slot)
            .unwrap()
            .last_blockhash();
        let tower_sync = TowerSync::new_from_slot(last_main_fork_slot, last_vote_bank_hash);
        let vote_tx = vote_transaction::new_tower_sync_transaction(
            tower_sync,
            last_vote_blockhash,
            &leader_keypair,
            &validator_keypairs.vote_keypair,
            &validator_keypairs.vote_keypair,
            None,
        );

        // Add votes to `really_last_slot` so that `root` will be confirmed again
        make_slot_with_vote_tx(
            &blockstore,
            ticks_per_slot,
            really_last_main_fork_slot,
            last_main_fork_slot,
            &last_vote_blockhash,
            vote_tx,
            &leader_keypair,
        );

        let (bank_forks, ..) = test_process_blockstore_with_custom_options(
            &genesis_config,
            &blockstore,
            &opts,
            blockstore_access_type,
        );
        let bank_forks = bank_forks.read().unwrap();

        assert_eq!(bank_forks.root(), really_expected_root_slot);
    }

    #[test]
    fn test_process_blockstore_with_supermajority_root_without_blockstore_root() {
        run_test_process_blockstore_with_supermajority_root(None, AccessType::Primary);
    }

    #[test]
    fn test_process_blockstore_with_supermajority_root_without_blockstore_root_readonly_access() {
        run_test_process_blockstore_with_supermajority_root(None, AccessType::ReadOnly);
    }

    #[test]
    fn test_process_blockstore_with_supermajority_root_with_blockstore_root() {
        run_test_process_blockstore_with_supermajority_root(Some(1), AccessType::Primary)
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn test_supermajority_root_from_vote_accounts() {
        let convert_to_vote_accounts = |roots_stakes: Vec<(Slot, u64)>| -> VoteAccountsHashMap {
            roots_stakes
                .into_iter()
                .map(|(root, stake)| {
                    let mut vote_state = VoteStateV4::default();
                    vote_state.root_slot = Some(root);
                    let mut vote_account = AccountSharedData::new(
                        1,
                        VoteStateV4::size_of(),
                        &solana_vote_program::id(),
                    );
                    let versioned = VoteStateVersions::new_v4(vote_state);
                    VoteStateV4::serialize(&versioned, vote_account.data_as_mut_slice()).unwrap();
                    (
                        solana_pubkey::new_rand(),
                        (stake, VoteAccount::try_from(vote_account).unwrap()),
                    )
                })
                .collect()
        };

        let total_stake = 10;

        // Supermajority root should be None
        assert!(supermajority_root_from_vote_accounts(total_stake, &HashMap::default()).is_none());

        // Supermajority root should be None
        let roots_stakes = vec![(8, 1), (3, 1), (4, 1), (8, 1)];
        let accounts = convert_to_vote_accounts(roots_stakes);
        assert!(supermajority_root_from_vote_accounts(total_stake, &accounts).is_none());

        // Supermajority root should be 4, has 7/10 of the stake
        let roots_stakes = vec![(8, 1), (3, 1), (4, 1), (8, 5)];
        let accounts = convert_to_vote_accounts(roots_stakes);
        assert_eq!(
            supermajority_root_from_vote_accounts(total_stake, &accounts).unwrap(),
            4
        );

        // Supermajority root should be 8, it has 7/10 of the stake
        let roots_stakes = vec![(8, 1), (3, 1), (4, 1), (8, 6)];
        let accounts = convert_to_vote_accounts(roots_stakes);
        assert_eq!(
            supermajority_root_from_vote_accounts(total_stake, &accounts).unwrap(),
            8
        );
    }

    fn confirm_slot_entries_for_tests(
        bank: &Arc<Bank>,
        slot_entries: Vec<Entry>,
        slot_full: bool,
        prev_entry_hash: Hash,
    ) -> result::Result<(), BlockstoreProcessorError> {
        let replay_tx_thread_pool = create_thread_pool(1);
        confirm_slot_entries(
            &BankWithScheduler::new_without_scheduler(bank.clone()),
            &replay_tx_thread_pool,
            (slot_entries, 0, slot_full),
            &mut ConfirmationTiming::default(),
            &mut ConfirmationProgress::new(prev_entry_hash),
            false,
            None,
            None,
            None,
            None,
            None,
        )
    }

    fn create_test_transactions(
        mint_keypair: &Keypair,
        genesis_hash: &Hash,
    ) -> Vec<RuntimeTransaction<SanitizedTransaction>> {
        let pubkey = solana_pubkey::new_rand();
        let keypair2 = Keypair::new();
        let pubkey2 = solana_pubkey::new_rand();
        let keypair3 = Keypair::new();
        let pubkey3 = solana_pubkey::new_rand();

        vec![
            RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
                mint_keypair,
                &pubkey,
                1,
                *genesis_hash,
            )),
            RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
                &keypair2,
                &pubkey2,
                1,
                *genesis_hash,
            )),
            RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
                &keypair3,
                &pubkey3,
                1,
                *genesis_hash,
            )),
        ]
    }

    #[test]
    fn test_confirm_slot_entries_progress_num_txs_indexes() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(100 * LAMPORTS_PER_SOL);
        let genesis_hash = genesis_config.hash();
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let bank = BankWithScheduler::new_without_scheduler(bank);
        let replay_tx_thread_pool = create_thread_pool(1);
        let mut timing = ConfirmationTiming::default();
        let mut progress = ConfirmationProgress::new(genesis_hash);
        let amount = genesis_config.rent.minimum_balance(0);
        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();
        let keypair3 = Keypair::new();
        let keypair4 = Keypair::new();
        bank.transfer(LAMPORTS_PER_SOL, &mint_keypair, &keypair1.pubkey())
            .unwrap();
        bank.transfer(LAMPORTS_PER_SOL, &mint_keypair, &keypair2.pubkey())
            .unwrap();

        let (transaction_status_sender, transaction_status_receiver) =
            crossbeam_channel::unbounded();
        let transaction_status_sender = TransactionStatusSender {
            sender: transaction_status_sender,
            dependency_tracker: None,
        };

        let blockhash = bank.last_blockhash();
        let tx1 = system_transaction::transfer(
            &keypair1,
            &keypair3.pubkey(),
            amount,
            bank.last_blockhash(),
        );
        let tx2 = system_transaction::transfer(
            &keypair2,
            &keypair4.pubkey(),
            amount,
            bank.last_blockhash(),
        );
        let entry = next_entry(&blockhash, 1, vec![tx1, tx2]);
        let new_hash = entry.hash;

        confirm_slot_entries(
            &bank,
            &replay_tx_thread_pool,
            (vec![entry], 0, false),
            &mut timing,
            &mut progress,
            false,
            Some(&transaction_status_sender),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(progress.num_txs, 2);
        let batch = transaction_status_receiver.recv().unwrap();
        if let TransactionStatusMessage::Batch((batch, _sequence)) = batch {
            assert_eq!(batch.transactions.len(), 2);
            assert_eq!(batch.transaction_indexes.len(), 2);
            assert_eq!(batch.transaction_indexes, [0, 1]);
        } else {
            panic!("batch should have been sent");
        }

        let tx1 = system_transaction::transfer(
            &keypair1,
            &keypair3.pubkey(),
            amount + 1,
            bank.last_blockhash(),
        );
        let tx2 = system_transaction::transfer(
            &keypair2,
            &keypair4.pubkey(),
            amount + 1,
            bank.last_blockhash(),
        );
        let tx3 = system_transaction::transfer(
            &mint_keypair,
            &Pubkey::new_unique(),
            amount,
            bank.last_blockhash(),
        );
        let entry = next_entry(&new_hash, 1, vec![tx1, tx2, tx3]);

        confirm_slot_entries(
            &bank,
            &replay_tx_thread_pool,
            (vec![entry], 0, false),
            &mut timing,
            &mut progress,
            false,
            Some(&transaction_status_sender),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(progress.num_txs, 5);
        let batch = transaction_status_receiver.recv().unwrap();
        if let TransactionStatusMessage::Batch((batch, _sequnce)) = batch {
            assert_eq!(batch.transactions.len(), 3);
            assert_eq!(batch.transaction_indexes.len(), 3);
            assert_eq!(batch.transaction_indexes, [2, 3, 4]);
        } else {
            panic!("batch should have been sent");
        }
    }

    fn do_test_schedule_batches_for_execution(should_succeed: bool) {
        agave_logger::setup();
        let dummy_leader_pubkey = solana_pubkey::new_rand();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config_with_leader(500, &dummy_leader_pubkey, 100);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let context = SchedulingContext::for_verification(bank.clone());

        let txs = create_test_transactions(&mint_keypair, &genesis_config.hash());

        let mut mocked_scheduler = MockInstalledScheduler::new();
        let seq = Arc::new(Mutex::new(mockall::Sequence::new()));
        let seq_cloned = seq.clone();
        mocked_scheduler
            .expect_context()
            .times(1)
            .in_sequence(&mut seq.lock().unwrap())
            .return_const(context);
        if should_succeed {
            mocked_scheduler
                .expect_schedule_execution()
                .times(txs.len())
                .returning(|_, _| Ok(()));
        } else {
            // mocked_scheduler isn't async; so short-circuiting behavior is quite visible in that
            // .times(1) is called instead of .times(txs.len()), not like the succeeding case
            mocked_scheduler
                .expect_schedule_execution()
                .times(1)
                .returning(|_, _| Err(SchedulerAborted));
            mocked_scheduler
                .expect_recover_error_after_abort()
                .times(1)
                .returning(|| TransactionError::InsufficientFundsForFee);
        }
        mocked_scheduler
            .expect_wait_for_termination()
            .with(mockall::predicate::eq(true))
            .times(1)
            .in_sequence(&mut seq.lock().unwrap())
            .returning(move |_| {
                let mut mocked_uninstalled_scheduler = MockUninstalledScheduler::new();
                mocked_uninstalled_scheduler
                    .expect_return_to_pool()
                    .times(1)
                    .in_sequence(&mut seq_cloned.lock().unwrap())
                    .returning(|| ());
                (
                    (Ok(()), ExecuteTimings::default()),
                    Box::new(mocked_uninstalled_scheduler),
                )
            });
        let bank = BankWithScheduler::new(bank, Some(Box::new(mocked_scheduler)));

        let locked_entry = LockedTransactionsWithIndexes {
            lock_results: bank.try_lock_accounts(&txs),
            transactions: txs,
            starting_index: 0,
        };

        let replay_tx_thread_pool = create_thread_pool(1);
        let mut batch_execution_timing = BatchExecutionTiming::default();
        let result = process_batches(
            &bank,
            &replay_tx_thread_pool,
            [locked_entry].into_iter(),
            None,
            None,
            &mut batch_execution_timing,
            None,
            None,
        );
        if should_succeed {
            assert_matches!(result, Ok(()));
        } else {
            assert_matches!(result, Err(TransactionError::InsufficientFundsForFee));
        }
    }

    #[test]
    fn test_schedule_batches_for_execution_success() {
        do_test_schedule_batches_for_execution(true);
    }

    #[test]
    fn test_schedule_batches_for_execution_failure() {
        do_test_schedule_batches_for_execution(false);
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
        poh_result: Result<Option<usize>>,
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
                RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
                    &mint_keypair,
                    &pubkey,
                    1,
                    genesis_config.hash(),
                )),
                Ok(()),
            ),
            TxResult::ExecutedWithFailure => (
                RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
                    &mint_keypair,
                    &pubkey,
                    100000000,
                    genesis_config.hash(),
                )),
                Ok(()),
            ),
            TxResult::NotExecuted => (
                RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
                    &mint_keypair,
                    &pubkey,
                    1,
                    Hash::default(),
                )),
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
        let (sender, receiver) = crossbeam_channel::unbounded();

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
            &mut timing,
            None,
            None,
            Some(|processing_result: &'_ Result<_>| {
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

    #[test]
    fn test_confirm_slot_entries_with_fix() {
        const HASHES_PER_TICK: u64 = 10;
        const TICKS_PER_SLOT: u64 = 2;

        let leader_id = Pubkey::new_unique();

        let GenesisConfigInfo {
            mut genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10_000);
        genesis_config.poh_config.hashes_per_tick = Some(HASHES_PER_TICK);
        genesis_config.ticks_per_slot = TICKS_PER_SLOT;
        let genesis_hash = genesis_config.hash();

        let (slot_0_bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        assert_eq!(slot_0_bank.slot(), 0);
        assert_eq!(slot_0_bank.tick_height(), 0);
        assert_eq!(slot_0_bank.max_tick_height(), 2);
        assert_eq!(slot_0_bank.last_blockhash(), genesis_hash);
        assert_eq!(slot_0_bank.get_hash_age(&genesis_hash), Some(0));

        let slot_0_entries = entry::create_ticks(TICKS_PER_SLOT, HASHES_PER_TICK, genesis_hash);
        let slot_0_hash = slot_0_entries.last().unwrap().hash;
        confirm_slot_entries_for_tests(&slot_0_bank, slot_0_entries, true, genesis_hash).unwrap();
        assert_eq!(slot_0_bank.tick_height(), slot_0_bank.max_tick_height());
        assert_eq!(slot_0_bank.last_blockhash(), slot_0_hash);
        assert_eq!(slot_0_bank.get_hash_age(&genesis_hash), Some(1));
        assert_eq!(slot_0_bank.get_hash_age(&slot_0_hash), Some(0));

        let new_bank = Bank::new_from_parent(slot_0_bank, &leader_id, 2);
        let slot_2_bank = bank_forks
            .write()
            .unwrap()
            .insert(new_bank)
            .clone_without_scheduler();
        assert_eq!(slot_2_bank.slot(), 2);
        assert_eq!(slot_2_bank.tick_height(), 2);
        assert_eq!(slot_2_bank.max_tick_height(), 6);
        assert_eq!(slot_2_bank.last_blockhash(), slot_0_hash);

        let slot_1_entries = entry::create_ticks(TICKS_PER_SLOT, HASHES_PER_TICK, slot_0_hash);
        let slot_1_hash = slot_1_entries.last().unwrap().hash;
        confirm_slot_entries_for_tests(&slot_2_bank, slot_1_entries, false, slot_0_hash).unwrap();
        assert_eq!(slot_2_bank.tick_height(), 4);
        assert_eq!(slot_2_bank.last_blockhash(), slot_0_hash);
        assert_eq!(slot_2_bank.get_hash_age(&genesis_hash), Some(1));
        assert_eq!(slot_2_bank.get_hash_age(&slot_0_hash), Some(0));

        struct TestCase {
            recent_blockhash: Hash,
            expected_result: result::Result<(), BlockstoreProcessorError>,
        }

        let test_cases = [
            TestCase {
                recent_blockhash: slot_1_hash,
                expected_result: Err(BlockstoreProcessorError::InvalidTransaction(
                    TransactionError::BlockhashNotFound,
                )),
            },
            TestCase {
                recent_blockhash: slot_0_hash,
                expected_result: Ok(()),
            },
        ];

        // Check that slot 2 transactions can only use hashes for completed blocks.
        for TestCase {
            recent_blockhash,
            expected_result,
        } in test_cases
        {
            let slot_2_entries = {
                let to_pubkey = Pubkey::new_unique();
                let mut prev_entry_hash = slot_1_hash;
                let mut remaining_entry_hashes = HASHES_PER_TICK;

                let tx =
                    system_transaction::transfer(&mint_keypair, &to_pubkey, 1, recent_blockhash);
                remaining_entry_hashes = remaining_entry_hashes.checked_sub(1).unwrap();
                let mut entries = vec![next_entry_mut(&mut prev_entry_hash, 1, vec![tx])];

                entries.push(next_entry_mut(
                    &mut prev_entry_hash,
                    remaining_entry_hashes,
                    vec![],
                ));
                entries.push(next_entry_mut(
                    &mut prev_entry_hash,
                    HASHES_PER_TICK,
                    vec![],
                ));

                entries
            };

            let slot_2_hash = slot_2_entries.last().unwrap().hash;
            let result =
                confirm_slot_entries_for_tests(&slot_2_bank, slot_2_entries, true, slot_1_hash);
            match (result, expected_result) {
                (Ok(()), Ok(())) => {
                    assert_eq!(slot_2_bank.tick_height(), slot_2_bank.max_tick_height());
                    assert_eq!(slot_2_bank.last_blockhash(), slot_2_hash);
                    assert_eq!(slot_2_bank.get_hash_age(&genesis_hash), Some(2));
                    assert_eq!(slot_2_bank.get_hash_age(&slot_0_hash), Some(1));
                    assert_eq!(slot_2_bank.get_hash_age(&slot_2_hash), Some(0));
                }
                (
                    Err(BlockstoreProcessorError::InvalidTransaction(err)),
                    Err(BlockstoreProcessorError::InvalidTransaction(expected_err)),
                ) => {
                    assert_eq!(err, expected_err);
                }
                (result, expected_result) => {
                    panic!("actual result {result:?} != expected result {expected_result:?}");
                }
            }
        }
    }

    #[test]
    fn test_check_block_cost_limit() {
        let dummy_leader_pubkey = solana_pubkey::new_rand();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config_with_leader(500, &dummy_leader_pubkey, 100);
        let bank = Bank::new_for_tests(&genesis_config);

        let tx = RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
            &mint_keypair,
            &Pubkey::new_unique(),
            1,
            genesis_config.hash(),
        ));
        let mut tx_cost = CostModel::calculate_cost(&tx, &bank.feature_set);
        let actual_execution_cu = 1;
        let actual_loaded_accounts_data_size = 64 * 1024;
        let TransactionCost::Transaction(ref mut usage_cost_details) = tx_cost else {
            unreachable!("test tx is non-vote tx");
        };
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
            .set_limits(u64::MAX, block_limit, u64::MAX);

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
    fn test_calculate_alpenglow_ticks() {
        let first_alpenglow_slot = 10;
        let ticks_per_slot = 2;

        // Slots before alpenglow don't have alpenglow ticks
        let slot = 9;
        let parent_slot = 8;
        assert!(
            calculate_alpenglow_ticks(slot, first_alpenglow_slot, parent_slot, ticks_per_slot)
                .is_none()
        );

        // First alpenglow slot should only have 1 tick
        let slot = first_alpenglow_slot;
        let parent_slot = first_alpenglow_slot - 1;
        assert_eq!(
            calculate_alpenglow_ticks(slot, first_alpenglow_slot, parent_slot, ticks_per_slot)
                .unwrap(),
            1
        );

        // First alpenglow slot with skipped non-alpenglow slots
        // need to have `ticks_per_slot` ticks per skipped slot and
        // then one additional tick for the first alpenglow slot
        let slot = first_alpenglow_slot;
        let num_skipped_slots = 3;
        let parent_slot = first_alpenglow_slot - num_skipped_slots - 1;
        assert_eq!(
            calculate_alpenglow_ticks(slot, first_alpenglow_slot, parent_slot, ticks_per_slot)
                .unwrap(),
            num_skipped_slots * ticks_per_slot + 1
        );

        // Skipped alpenglow slots don't need any additional ticks
        let slot = first_alpenglow_slot + 2;
        let parent_slot = first_alpenglow_slot;
        assert_eq!(
            calculate_alpenglow_ticks(slot, first_alpenglow_slot, parent_slot, ticks_per_slot)
                .unwrap(),
            1
        );

        // Skipped alpenglow slots along skipped non-alpenglow slots
        // need to have `ticks_per_slot` ticks per skipped non-alpenglow
        // slot only and then one additional tick for the alpenglow slot
        let slot = first_alpenglow_slot + 2;
        let num_skipped_non_alpenglow_slots = 4;
        let parent_slot = first_alpenglow_slot - num_skipped_non_alpenglow_slots - 1;
        assert_eq!(
            calculate_alpenglow_ticks(slot, first_alpenglow_slot, parent_slot, ticks_per_slot)
                .unwrap(),
            num_skipped_non_alpenglow_slots * ticks_per_slot + 1
        );
    }
}
