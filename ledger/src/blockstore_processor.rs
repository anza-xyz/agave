#[cfg(any(test, feature = "dev-context-only-utils"))]
use solana_entry::entry::{Entry, entry_views_for_tests};
use {
    crate::{
        block_error::BlockError,
        blockstore::{Blockstore, BlockstoreError},
        blockstore_meta::SlotMeta,
        entry_notifier_service::{EntryNotification, EntryNotifierSender, send_entry_notification},
        leader_schedule_cache::LeaderScheduleCache,
        shred::MAX_FEC_SETS_PER_SLOT,
        thread_pool::{WorkerJob, WorkerPool},
        use_snapshot_archives_at_startup::UseSnapshotArchivesAtStartup,
    },
    ExecuteTimingType::{NumExecuteBatches, TotalBatchesLen},
    agave_transaction_view::{
        transaction_data::TransactionData, transaction_view::UnsanitizedTransactionView,
    },
    agave_votor_messages::certificate::Certificate,
    bytes::Bytes,
    chrono_humanize::{Accuracy, HumanTime, Tense},
    crossbeam_channel::{Receiver, Sender},
    itertools::Itertools,
    log::*,
    rayon::ThreadPool,
    scopeguard::defer,
    smallvec::SmallVec,
    solana_accounts_db::{
        account_locks::validate_account_locks, accounts_db::AccountsDbConfig,
        accounts_update_notifier_interface::AccountsUpdateNotifier,
    },
    solana_clock::Slot,
    solana_entry::{
        block_component::{ParsedBlockComponent, VersionedBlockMarker},
        entry::{
            self, EntrySliceTickCheck as _, EntryType, EntryView, UnverifiedSignatures,
            create_ticks,
        },
    },
    solana_genesis_config::GenesisConfig,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank, bank_forks::BankForks, block_component_processor::BlockComponentProcessorError,
        installed_scheduler_pool::BankWithScheduler, leader_schedule_utils::leader_slot_index,
        runtime_config::RuntimeConfig, snapshot_controller::SnapshotController,
        transaction_execution::TransactionStatusSender,
    },
    solana_runtime_transaction::runtime_transaction::ReplayTransaction,
    solana_shred_version::compute_shred_version,
    solana_svm_timings::{ExecuteTimingType, ExecuteTimings, report_execute_timings},
    solana_svm_transaction::svm_message::SVMMessage,
    solana_transaction::TransactionVerificationMode,
    solana_transaction_error::{TransactionError, TransactionResult as Result},
    std::{
        cmp,
        collections::{HashMap, HashSet},
        mem,
        num::Saturating,
        ops::{Index, Range},
        path::PathBuf,
        result,
        sync::{
            Arc, OnceLock, RwLock,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    },
    thiserror::Error,
};
#[cfg(feature = "dev-context-only-utils")]
use {qualifier_attr::qualifiers, solana_runtime::bank::HashOverrides};

struct ReplayEntry {
    entry: EntryType<ReplayTransaction>,
    starting_index: usize,
}

fn transaction_hash_verify_thread_pool() -> &'static ThreadPool {
    const TX_HASH_VERIFY_THREAD_POOL_SIZE: usize = 4;
    static TX_HASH_VERIFY_THREAD_POOL: OnceLock<ThreadPool> = OnceLock::new();
    TX_HASH_VERIFY_THREAD_POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(TX_HASH_VERIFY_THREAD_POOL_SIZE)
            .thread_name(|i| format!("solReplayHash{i:02}"))
            .build()
            .expect("new transaction hash verify rayon threadpool")
    })
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

/// Process an ordered list of entries and wait for their completed execution.
/// 1. For each entry in order, up to a block-boundary `Tick`:
///    - `Transactions`: validate each transaction's account locks (and reject duplicate message
///      hashes within the entry) *without* taking the locks, then schedule the transactions directly
///      onto `bank`'s installed unified scheduler. The scheduler orders conflicts across entries
///      itself, so no account locks are held here.
///    - `Tick`: save it to register after the entries are scheduled.
/// 2. Register the `Tick` if it's available
/// 3. Wait for the scheduler's completed execution, so that `Ok(())` means the
///    entries executed successfully.
///
/// Waiting ends the bank's scheduler session; processing further entries against the same bank
/// requires wrapping it with a freshly taken scheduler again.
///
/// This method is for use testing against a single Bank, and assumes `Bank::transaction_count()`
/// represents the number of transactions executed in this Bank
#[cfg(feature = "dev-context-only-utils")]
pub fn process_entries_for_tests(bank: &BankWithScheduler, entries: Vec<Entry>) -> Result<()> {
    let result = schedule_entries_for_tests(bank, entries);

    // Wait even if scheduling failed, both to surface any transaction execution error like the
    // replay stage does before freezing the bank, and to return the scheduler to its pool before
    // `bank` is dropped.
    let wait_result = bank
        .wait_for_completed_scheduler()
        .map_or(Ok(()), |(wait_result, _timings)| wait_result);

    result.and(wait_result)
}

#[cfg(feature = "dev-context-only-utils")]
fn schedule_entries_for_tests(bank: &BankWithScheduler, entries: Vec<Entry>) -> Result<()> {
    let validate_and_hash_transaction = {
        let bank = bank.clone_with_scheduler();
        move |unsanitized: UnsanitizedTransactionView<Bytes>| {
            bank.verify_transaction(unsanitized, TransactionVerificationMode::HashOnly)
        }
    };

    let num_txs = entries.iter().map(|entry| entry.transactions.len()).sum();
    let entries = entry_views_for_tests(entries);
    let entry::ValidatedHashedTransactions {
        entries,
        unverified_signatures,
    } = entry::validate_and_hash_transactions(
        entries,
        num_txs,
        transaction_hash_verify_thread_pool(),
        validate_and_hash_transaction,
    )?;
    unverified_signatures.verify()?;

    let mut entry_starting_index: usize = bank.transaction_count().try_into().unwrap();
    let replay_entries: Vec<_> = entries
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

    process_entries(bank, replay_entries)
}

fn process_entries(bank: &BankWithScheduler, entries: Vec<ReplayEntry>) -> Result<()> {
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
                    break;
                }
            }
            EntryType::Transactions(transactions) => {
                // Any bank replaying transactions must have a scheduler installed. Slot 0 -
                // the only bank replayed before the scheduler pool is installed - is tick-only,
                // so it never reaches here.
                assert!(
                    bank.has_installed_scheduler(),
                    "no scheduler installed for bank of slot {} during replay",
                    bank.slot()
                );
                validate_entry_transactions(
                    &transactions,
                    bank.get_transaction_account_lock_limit(),
                )?;

                let indexes = starting_index..starting_index + transactions.len();
                // Widening usize index to OrderedTaskId (= u128) won't ever fail.
                let task_ids = indexes.map(|i| i.try_into().unwrap());

                bank.schedule_transaction_executions(transactions.into_iter().zip_eq(task_ids))?;
            }
        }
    }
    for hash in tick_hashes {
        bank.register_tick(&hash);
    }
    Ok(())
}

/// Validate an entry's transactions before scheduling: each transaction's account
/// locks (count and duplicates). Does not take account locks - the unified scheduler orders conflicts.
fn validate_entry_transactions(
    transactions: &[ReplayTransaction],
    tx_account_lock_limit: usize,
) -> Result<()> {
    for transaction in transactions {
        validate_account_locks(transaction.account_keys(), tx_account_lock_limit)?;
    }

    Ok(())
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

    #[error("block component processor error: {0}")]
    BlockComponentProcessor(#[from] BlockComponentProcessorError),

    #[error("bank hash mismatch at slot {0}: expected {1}, got {2}")]
    BankHashMismatch(Slot, Hash, Hash),
}

/// Callback for accessing bank state after each slot is confirmed while
/// processing the blockstore
pub type ProcessSlotCallback = Arc<dyn Fn(&Bank) + Sync + Send>;

#[derive(Default, Clone)]
pub struct ProcessOptions {
    /// Run PoH, transaction signature and other transaction verification on the entries.
    pub run_verification: bool,
    /// For startup replay / ledger-tool skip checks that verify a block chains to its parent correctly.
    pub skip_inter_slot_verification: bool,
    pub halt_at_slot: Option<Slot>,
    pub slot_callback: Option<ProcessSlotCallback>,
    pub new_hard_forks: Option<Vec<Slot>>,
    pub debug_keys: Option<Arc<HashSet<Pubkey>>>,
    pub limit_load_slot_count_from_snapshot: Option<usize>,
    pub allow_dead_slots: bool,
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
    let hard_forks = bank0.hard_forks();
    let bank_forks = BankForks::new_rw_arc(bank0);

    info!("Processing ledger for slot 0...");
    let replay_verification_worker_pool = ReplayVerificationWorkerPool::new(num_cpus::get());
    process_bank_0(
        &bank_forks
            .read()
            .unwrap()
            .get_with_scheduler(bank0_slot)
            .unwrap(),
        compute_shred_version(&genesis_config.hash(), Some(&hard_forks)),
        blockstore,
        &replay_verification_worker_pool,
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
    shred_version: u16,
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
            bank.write_cost_tracker().unwrap().set_limits_max();
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
        let replay_verification_worker_pool = ReplayVerificationWorkerPool::new(num_cpus::get());
        load_frozen_forks(
            bank_forks,
            shred_version,
            &start_slot_meta,
            blockstore,
            &replay_verification_worker_pool,
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
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn verify_ticks<D: TransactionData>(
    bank: &Bank,
    entries: &[EntryView<D>],
    slot_full: bool,
    _tick_hash_count: &mut u64,
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

    // Alpenglow runs PoH in low-power mode and requires every entry to have one hash.
    if entries.iter().any(|entry| entry.num_hashes != 1) {
        warn!(
            "Alpenglow entry with invalid num_hashes found in slot: {}",
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
    shred_version: u16,
    replay_verification_worker_pool: &ReplayVerificationWorkerPool,
    opts: &ProcessOptions,
    progress: &mut ConfirmationProgress,
    entry_notification_sender: Option<&EntryNotifierSender>,
    timing: &mut ExecuteTimings,
) -> result::Result<(), BlockstoreProcessorError> {
    let mut confirmation_timing = ConfirmationTiming::default();
    let skip_verification = !opts.run_verification;
    confirm_slot(
        blockstore,
        bank,
        shred_version,
        replay_verification_worker_pool,
        &mut confirmation_timing,
        progress,
        skip_verification,
        entry_notification_sender,
        None,
        opts.allow_dead_slots,
    )?;

    timing.accumulate(&confirmation_timing.batch_execute.totals);

    if !bank.is_complete() {
        return Err(BlockstoreProcessorError::InvalidBlock(
            BlockError::Incomplete,
        ));
    }

    if let Some((result, execute_time)) = bank.wait_for_completed_scheduler() {
        timing.accumulate(&execute_time);
        result?;
    }

    progress.wait_for_all_verification_results(&mut 0, &mut 0)
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

    /// Number of times this slot was switched from an alternate location.
    pub num_bank_switches: u64,
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
            num_bank_switches: 0,
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
                ("num_bank_switches", self.num_bank_switches as i64, i64),
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
            per_pubkey_timings.sort_by_key(|b| cmp::Reverse(b.1.accumulated_us));
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
    async_verification: Option<AsyncVerificationProgress>,
}

impl ConfirmationProgress {
    pub fn new(last_entry: Hash) -> Self {
        Self {
            last_entry,
            ..Self::default()
        }
    }

    pub fn new_with_async_verification(
        last_entry: Hash,
        async_verification: Option<AsyncVerificationProgress>,
    ) -> Self {
        debug_assert!(
            async_verification
                .as_ref()
                .map(|av| { av.pending_jobs == 0 && av.first_error.is_none() })
                .unwrap_or(true)
        );
        Self {
            last_entry,
            async_verification,
            ..Self::default()
        }
    }

    fn async_verification(
        &mut self,
        worker_pool: &ReplayVerificationWorkerPool,
    ) -> &mut AsyncVerificationProgress {
        self.async_verification
            .get_or_insert_with(|| AsyncVerificationProgress::new(worker_pool.job_capacity))
    }

    fn collect_available_verification_results(
        &mut self,
        poh_verify_elapsed: &mut u64,
        transaction_verify_elapsed: &mut u64,
    ) -> result::Result<(), BlockstoreProcessorError> {
        let Some(async_verification) = self.async_verification.as_mut() else {
            return Ok(());
        };
        let result = async_verification.collect_available_results();
        let (poh_us, transaction_us) = async_verification.take_timings();
        *poh_verify_elapsed = poh_verify_elapsed.saturating_add(poh_us);
        *transaction_verify_elapsed = transaction_verify_elapsed.saturating_add(transaction_us);
        result
    }

    pub fn wait_for_all_verification_results(
        &mut self,
        poh_verify_elapsed: &mut u64,
        transaction_verify_elapsed: &mut u64,
    ) -> result::Result<(), BlockstoreProcessorError> {
        let Some(async_verification) = self.async_verification.as_mut() else {
            return Ok(());
        };
        let result = async_verification.wait_for_all_results();
        let (poh_us, transaction_us) = async_verification.take_timings();
        *poh_verify_elapsed = poh_verify_elapsed.saturating_add(poh_us);
        *transaction_verify_elapsed = transaction_verify_elapsed.saturating_add(transaction_us);
        result
    }

    pub fn take_async_verification(&mut self) -> Option<AsyncVerificationProgress> {
        debug_assert!(
            self.async_verification
                .as_ref()
                .map(|av| { av.pending_jobs == 0 && av.first_error.is_none() })
                .unwrap_or(true)
        );
        self.async_verification.take()
    }
}

// return entries and signatures to the replay thread for deallocation to avoid creating unnecessary
// arena contention from the pool workers
enum AsyncVerificationResult {
    Poh {
        entries: Arc<VerificationBatch<Vec<entry::EntryVerificationData>>>,
        elapsed_us: u64,
        error: Option<BlockstoreProcessorError>,
    },
    Signatures {
        signatures: Arc<VerificationBatch<UnverifiedSignatures<Bytes>>>,
        elapsed_us: u64,
        error: Option<BlockstoreProcessorError>,
    },
}

// Wrapper used to track wall clock time for work that is split into multiple jobs and executed in
// parallel. The last job to finish will decrement remaining_jobs to 0 and record the total elapsed
// time since the batch was started.
struct VerificationBatch<T> {
    data: T,
    started: Instant,
    remaining_jobs: AtomicUsize,
}

impl<T> VerificationBatch<T> {
    fn finish_job(&self) -> u64 {
        if self.remaining_jobs.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.started.elapsed().as_micros() as u64
        } else {
            0
        }
    }
}

struct PohVerificationJob {
    entries: Arc<VerificationBatch<Vec<entry::EntryVerificationData>>>,
    range: Range<usize>,
    start_hash: Hash,
    slot: Slot,
    result_sender: Sender<AsyncVerificationResult>,
}

impl PohVerificationJob {
    fn run(self) {
        let Self {
            entries,
            range,
            start_hash,
            slot,
            result_sender,
        } = self;
        let verified = range.into_iter().all(|index| {
            let previous_hash = if index == 0 {
                &start_hash
            } else {
                &entries.data[index - 1].hash
            };
            entries.data[index].verify(previous_hash)
        });
        let elapsed_us = entries.finish_job();
        let error = (!verified).then(|| {
            warn!("Ledger proof of history failed at slot: {slot}");
            BlockstoreProcessorError::InvalidBlock(BlockError::InvalidEntryHash)
        });
        let _ = result_sender.send(AsyncVerificationResult::Poh {
            entries,
            elapsed_us,
            error,
        });
    }
}

struct SignaturesVerificationJob {
    signatures: Arc<VerificationBatch<UnverifiedSignatures<Bytes>>>,
    range: Range<usize>,
    slot: Slot,
    result_sender: Sender<AsyncVerificationResult>,
}

impl SignaturesVerificationJob {
    fn run(self) {
        let Self {
            signatures,
            range,
            slot,
            result_sender,
        } = self;
        let verified = range
            .into_iter()
            .all(|index| signatures.data.verify_signatures(index));
        let elapsed_us = signatures.finish_job();
        let error = (!verified).then_some(BlockstoreProcessorError::InvalidTransaction(
            TransactionError::SignatureFailure,
        ));
        if let Some(err) = &error {
            warn!("Ledger transaction signature verification failed at slot {slot}: {err}");
        }
        let _ = result_sender.send(AsyncVerificationResult::Signatures {
            signatures,
            elapsed_us,
            error,
        });
    }
}

enum VerificationJob {
    Poh(PohVerificationJob),
    Signatures(SignaturesVerificationJob),
}

impl WorkerJob for VerificationJob {
    fn run(self) {
        match self {
            Self::Poh(job) => job.run(),
            Self::Signatures(job) => job.run(),
        }
    }
}

pub struct ReplayVerificationWorkerPool {
    inner: WorkerPool<VerificationJob>,
    job_capacity: usize,
}

impl ReplayVerificationWorkerPool {
    pub fn new(num_workers: usize) -> Self {
        // set the maximum number of jobs that can be sent replaying a completely full slot as
        // the capacity, so we avoid blocking the replay thread when sending work. ~8MB of
        // memory. Not load bearing, a smaller capacity would do, just cause more stalls.
        let job_capacity = (MAX_FEC_SETS_PER_SLOT as usize)
            // poh + signature verification
            .checked_mul(2)
            .unwrap()
            // each poh/signature batch can be split into multiple jobs, at most 1 for each worker
            .checked_mul(num_workers)
            .expect("verification job queue capacity overflow");
        Self::with_capacity(num_workers, job_capacity)
    }

    fn with_capacity(num_workers: usize, job_capacity: usize) -> Self {
        Self {
            inner: WorkerPool::new("solReplayVer", num_workers, job_capacity),
            job_capacity,
        }
    }

    fn send(&self, job: VerificationJob) {
        self.inner.send(job);
    }
}

pub struct AsyncVerificationProgress {
    sender: Sender<AsyncVerificationResult>,
    receiver: Receiver<AsyncVerificationResult>,
    pending_jobs: usize,
    first_error: Option<BlockstoreProcessorError>,
    poh_verify_elapsed: u64,
    transaction_verify_elapsed: u64,
}

impl AsyncVerificationProgress {
    pub fn new(result_channel_capacity: usize) -> Self {
        assert_ne!(
            result_channel_capacity, 0,
            "verification result channel capacity must be nonzero"
        );
        let (sender, receiver) = crossbeam_channel::bounded(result_channel_capacity);
        Self {
            sender,
            receiver,
            pending_jobs: 0,
            first_error: None,
            poh_verify_elapsed: 0,
            transaction_verify_elapsed: 0,
        }
    }

    fn spawn_poh_verification(
        &mut self,
        worker_pool: &ReplayVerificationWorkerPool,
        entries: Vec<entry::EntryVerificationData>,
        start_hash: Hash,
        slot: Slot,
    ) -> result::Result<(), BlockstoreProcessorError> {
        let item_count = entries.len();
        if item_count == 0 {
            return Ok(());
        }
        let sender = self.sender.clone();
        self.send_jobs(worker_pool, entries, item_count, move |range, entries| {
            VerificationJob::Poh(PohVerificationJob {
                entries,
                range,
                start_hash,
                slot,
                result_sender: sender.clone(),
            })
        })
    }

    fn spawn_signature_verification(
        &mut self,
        worker_pool: &ReplayVerificationWorkerPool,
        signatures: UnverifiedSignatures<Bytes>,
        slot: Slot,
    ) -> result::Result<(), BlockstoreProcessorError> {
        let item_count = signatures.len();
        if item_count == 0 {
            return Ok(());
        }
        let sender = self.sender.clone();
        self.send_jobs(
            worker_pool,
            signatures,
            item_count,
            move |range, signatures| {
                VerificationJob::Signatures(SignaturesVerificationJob {
                    signatures,
                    range,
                    slot,
                    result_sender: sender.clone(),
                })
            },
        )
    }

    fn send_jobs<T>(
        &mut self,
        worker_pool: &ReplayVerificationWorkerPool,
        data: T,
        item_count: usize,
        create_job: impl Fn(Range<usize>, Arc<VerificationBatch<T>>) -> VerificationJob,
    ) -> result::Result<(), BlockstoreProcessorError> {
        debug_assert!(item_count > 0);
        let job_count = worker_pool.inner.num_workers().min(item_count);
        let result_capacity = self.sender.capacity().unwrap();
        assert!(
            self.pending_jobs <= result_capacity,
            "verification pending job count exceeds result channel capacity"
        );

        // Split the work evenly across workers. This is kinda naive but a good first impl.
        let items_per_job = item_count / job_count;
        let remainder = item_count % job_count;

        // wrap the data in Arc<VerificationBatch> so that we can track the wall clock time to
        // verify the whole thing
        let data = Arc::new(VerificationBatch {
            data,
            started: Instant::now(),
            remaining_jobs: AtomicUsize::new(job_count),
        });

        let mut range_start = 0;
        for job_index in 0..job_count {
            // The workers can be shared across banks. We never want them to stall because they've
            // done their job but can't post the result.
            if self.pending_jobs == result_capacity {
                let result = self.receiver.recv().map_err(|_| {
                    BlockstoreProcessorError::InvalidBlock(BlockError::InvalidEntryHash)
                })?;
                self.apply_result(result);
            }

            // the first `remainder` workers get an extra item each
            let range_end = range_start + items_per_job + usize::from(job_index < remainder);
            self.pending_jobs = self
                .pending_jobs
                .checked_add(1)
                .expect("verification pending job count overflow");
            worker_pool.send(create_job(range_start..range_end, Arc::clone(&data)));
            range_start = range_end;
        }

        // all jobs must be sent before returning an error because
        // `VerificationBatch::remaining_jobs` was initialized with `job_count`
        if let Some(error) = self.first_error.take() {
            return Err(error);
        }
        Ok(())
    }

    // Collects all available results from the channel.
    fn collect_available_results(&mut self) -> result::Result<(), BlockstoreProcessorError> {
        while let Ok(result) = self.receiver.try_recv() {
            self.apply_result(result);
        }
        if let Some(error) = self.first_error.take() {
            return Err(error);
        }
        Ok(())
    }

    // Waits for all pending jobs to complete and collects their results.
    //
    // This MUST be called at the end of a slot.
    fn wait_for_all_results(&mut self) -> result::Result<(), BlockstoreProcessorError> {
        while self.pending_jobs > 0 {
            let result = self.receiver.recv().map_err(|_| {
                BlockstoreProcessorError::InvalidBlock(BlockError::InvalidEntryHash)
            })?;
            self.apply_result(result);
        }
        if let Some(error) = self.first_error.take() {
            return Err(error);
        }
        Ok(())
    }

    fn apply_result(&mut self, result: AsyncVerificationResult) {
        self.pending_jobs = self
            .pending_jobs
            .checked_sub(1)
            .expect("verification result without a pending job");

        let error = match result {
            AsyncVerificationResult::Poh {
                entries,
                elapsed_us,
                error,
            } => {
                self.poh_verify_elapsed = self.poh_verify_elapsed.saturating_add(elapsed_us);
                drop(entries);
                error
            }
            AsyncVerificationResult::Signatures {
                signatures,
                elapsed_us,
                error,
            } => {
                self.transaction_verify_elapsed =
                    self.transaction_verify_elapsed.saturating_add(elapsed_us);
                drop(signatures);
                error
            }
        };

        if self.first_error.is_none() {
            self.first_error = error;
        }
    }

    fn take_timings(&mut self) -> (u64, u64) {
        (
            mem::take(&mut self.poh_verify_elapsed),
            mem::take(&mut self.transaction_verify_elapsed),
        )
    }
}

#[allow(clippy::too_many_arguments)]
pub fn confirm_slot(
    blockstore: &Blockstore,
    bank: &BankWithScheduler,
    shred_version: u16,
    replay_verification_worker_pool: &ReplayVerificationWorkerPool,
    timing: &mut ConfirmationTiming,
    progress: &mut ConfirmationProgress,
    skip_verification: bool,
    entry_notification_sender: Option<&EntryNotifierSender>,
    finalization_cert_sender: Option<&Sender<SmallVec<[Certificate; 2]>>>,
    allow_dead_slots: bool,
) -> result::Result<(), BlockstoreProcessorError> {
    let slot = bank.slot();

    let (slot_components, completed_ranges, slot_full) = {
        let mut load_elapsed = Measure::start("load_elapsed");
        let load_result = blockstore
            .get_slot_component_views_with_shred_info(slot, progress.num_shreds, allow_dead_slots)
            .map_err(BlockstoreProcessorError::FailedToLoadEntries);
        load_elapsed.stop();
        if load_result.is_err() {
            timing.fetch_fail_elapsed += load_elapsed.as_us();
        } else {
            timing.fetch_elapsed += load_elapsed.as_us();
        }
        load_result
    }?;

    // Only replay that starts at the persisted UpdateParent FEC set may accept
    // UpdateParent as its first parent marker. From-shred-zero replay still
    // requires a block header before UpdateParent.
    let replay_starts_at_update_parent = bank.feature_set.snapshot().alpenglow_fast_leader_handover
        && leader_slot_index(slot) == 0
        && blockstore
            .meta(slot)
            .expect("Blockstore operations must succeed")
            .is_some_and(|meta| {
                meta.has_update_parent()
                    && progress.num_shreds == u64::from(meta.replay_fec_set_index)
            });
    let mut processor = bank.block_component_processor.write().unwrap();

    // Find the index of the last EntryBatch in slot_components
    let last_entry_batch_index = slot_components
        .iter()
        .rposition(|bc| matches!(bc, ParsedBlockComponent::EntryBatch(_)));

    for (ix, (completed_range, component)) in
        completed_ranges.iter().zip(slot_components).enumerate()
    {
        let num_shreds = completed_range.end - completed_range.start;
        let is_final = slot_full && ix == completed_ranges.len() - 1;

        match component {
            ParsedBlockComponent::EntryBatch(entries) => {
                let slot_full = slot_full && ix == last_entry_batch_index.unwrap();

                // Skip block component validation for genesis block. Slot 0 is handled specially,
                // since it won't have the required block markers.
                if slot != 0 {
                    processor
                        .on_entry_batch(&entries, is_final)
                        .inspect_err(|err| {
                            warn!(
                                "BlockComponentProcessor::on_entry_batch() for slot {slot} failed \
                                 with {err}"
                            );
                        })?;
                }

                confirm_slot_entries(
                    bank,
                    replay_verification_worker_pool,
                    (entries, num_shreds as u64, slot_full),
                    timing,
                    progress,
                    skip_verification,
                    entry_notification_sender,
                )?;
            }
            ParsedBlockComponent::BlockMarker(marker) => {
                let block_footer = match &marker {
                    VersionedBlockMarker::V1(marker) => marker.as_block_footer().cloned(),
                };
                if block_footer.is_some() {
                    // The footer path mutates vote accounts directly to pay rewards.
                    // All prior transactions must finish first so vote account view is deterministic.
                    if let Some((result, execute_time)) = bank.wait_for_completed_scheduler() {
                        timing.batch_execute.totals.accumulate(&execute_time);
                        result?;
                    }
                }
                if let Some(parent_bank) = bank.parent() {
                    let allow_initial_update_parent =
                        replay_starts_at_update_parent && marker.is_update_parent();
                    processor
                        .on_marker(
                            bank.clone_without_scheduler(),
                            parent_bank,
                            shred_version,
                            marker,
                            allow_initial_update_parent,
                            finalization_cert_sender,
                        )
                        .inspect_err(|err| {
                            if !matches!(err, BlockComponentProcessorError::AbandonedBank(_)) {
                                warn!(
                                    "BlockComponentProcessor::on_marker() for slot {slot} failed \
                                     with {err}"
                                );
                            }
                        })?;
                    if let Some(block_footer) = block_footer
                        && let Some(entry_notification_sender) = entry_notification_sender
                        && let Err(err) = send_entry_notification(
                            entry_notification_sender,
                            EntryNotification::BlockFooter {
                                slot,
                                bank_id: bank.bank_id(),
                                block_footer: Box::new(block_footer),
                            },
                        )
                    {
                        warn!(
                            "Slot {slot} block footer entry_notification_sender send failed: \
                             {err:?}"
                        );
                    }
                }
                progress.num_shreds += num_shreds as u64;
            }
        }

        // Skip block component validation for genesis block. Slot 0 is handled specially,
        // since it won't have the required block markers.
        if is_final && slot != 0 {
            processor.on_final()?;
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn confirm_slot_entries(
    bank: &BankWithScheduler,
    replay_verification_worker_pool: &ReplayVerificationWorkerPool,
    slot_entries_load_result: (Vec<EntryView<Bytes>>, u64, bool),
    timing: &mut ConfirmationTiming,
    progress: &mut ConfirmationProgress,
    skip_verification: bool,
    entry_notification_sender: Option<&EntryNotifierSender>,
) -> result::Result<(), BlockstoreProcessorError> {
    let ConfirmationTiming {
        confirmation_elapsed,
        replay_elapsed,
        poh_verify_elapsed,
        transaction_verify_elapsed,
        ..
    } = timing;

    let confirmation_elapsed_timer = Measure::start("confirmation_elapsed");
    defer! {
        *confirmation_elapsed += confirmation_elapsed_timer.end_as_us();
    };

    let slot = bank.slot();
    let bank_id = bank.bank_id();
    let (entries, num_shreds, slot_full) = slot_entries_load_result;
    if slot_full {
        bank.set_accounts_lt_hash_async_progress_is_at_end();
    }
    let num_entries = entries.len();
    let mut entry_tx_starting_indexes = Vec::with_capacity(num_entries);
    let mut entry_tx_starting_index = progress.num_txs;
    let num_txs = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            if let Some(entry_notification_sender) = entry_notification_sender {
                let entry_index = progress.num_entries.saturating_add(i);
                if let Err(err) = send_entry_notification(
                    entry_notification_sender,
                    EntryNotification::Entry {
                        slot,
                        bank_id,
                        index: entry_index,
                        entry: entry.into(),
                        starting_transaction_index: entry_tx_starting_index,
                    },
                ) {
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
        let start_hash = progress.last_entry;
        let verify_entries = entry::entry_views_to_verification_data(&entries);
        datapoint_debug!(
            "verify-batch-size",
            ("size", verify_entries.len() as i64, i64)
        );
        progress
            .async_verification(replay_verification_worker_pool)
            .spawn_poh_verification(
                replay_verification_worker_pool,
                verify_entries,
                start_hash,
                slot,
            )?;
    }

    let validate_and_hash_transaction = {
        let bank = bank.clone_with_scheduler();
        move |unsanitized: UnsanitizedTransactionView<Bytes>| {
            bank.verify_transaction(unsanitized, TransactionVerificationMode::HashOnly)
        }
    };

    let entry::ValidatedHashedTransactions {
        entries,
        unverified_signatures,
    } = match entry::validate_and_hash_transactions(
        entries,
        num_txs,
        transaction_hash_verify_thread_pool(),
        validate_and_hash_transaction,
    ) {
        Ok(txs) => txs,
        Err(err) => {
            warn!(
                "Ledger transaction hash verification failed at slot: {}",
                bank.slot()
            );
            return Err(err.into());
        }
    };
    if !skip_verification {
        progress
            .async_verification(replay_verification_worker_pool)
            .spawn_signature_verification(
                replay_verification_worker_pool,
                unverified_signatures,
                slot,
            )?;
    }

    let mut replay_timer = Measure::start("replay_elapsed");
    let replay_entries: Vec<_> = entries
        .into_iter()
        .zip(entry_tx_starting_indexes)
        .map(|(entry, starting_index)| ReplayEntry {
            entry,
            starting_index,
        })
        .collect();

    let process_result =
        process_entries(bank, replay_entries).map_err(BlockstoreProcessorError::from);
    replay_timer.stop();
    *replay_elapsed += replay_timer.as_us();

    process_result?;
    progress
        .collect_available_verification_results(poh_verify_elapsed, transaction_verify_elapsed)?;

    progress.num_shreds += num_shreds;
    progress.num_entries += num_entries;
    progress.num_txs += num_txs;
    if let Some(last_entry_hash) = last_entry_hash {
        progress.last_entry = last_entry_hash;
    }

    Ok(())
}

// Special handling required for processing the entries in slot 0
#[allow(clippy::too_many_arguments)]
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
fn process_bank_0(
    bank0: &BankWithScheduler,
    shred_version: u16,
    blockstore: &Blockstore,
    replay_verification_worker_pool: &ReplayVerificationWorkerPool,
    opts: &ProcessOptions,
    transaction_status_sender: Option<&TransactionStatusSender>,
    entry_notification_sender: Option<&EntryNotifierSender>,
) -> result::Result<(), BlockstoreProcessorError> {
    assert_eq!(bank0.slot(), 0);
    let mut progress = ConfirmationProgress::new(bank0.last_blockhash());
    confirm_full_slot(
        blockstore,
        bank0,
        shred_version,
        replay_verification_worker_pool,
        opts,
        &mut progress,
        entry_notification_sender,
        &mut ExecuteTimings::default(),
    )
    .map_err(|err| match err {
        err @ BlockstoreProcessorError::InvalidTransaction(_) => panic!("{err}"),
        _ => BlockstoreProcessorError::FailedToReplayBank0,
    })?;
    bank0.set_block_id(Some(
        blockstore
            .get_block_id(bank0.slot())?
            .expect("block id for a full slot must exist"),
    ));
    bank0.freeze();
    if blockstore.is_primary_access() {
        blockstore.insert_bank_hash(bank0.slot(), bank0.hash());
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
            if !opts.skip_inter_slot_verification {
                let parent_block_id = bank.block_id();
                if bank.slot() != 0 && Some(next_meta.parent_block_id) != parent_block_id {
                    warn!(
                        "startup replay deferring slot {next_slot}: parent {} has block id {:?}, \
                         but SlotMeta expects {:?}",
                        bank.slot(),
                        parent_block_id,
                        next_meta.parent_block_id,
                    );
                    continue;
                }
            }

            let next_bank = Bank::new_from_parent(
                bank.clone(),
                leader_schedule_cache
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
    pending_slots.sort_by_key(|b| cmp::Reverse(b.1.slot()));
    Ok(())
}

/// Set alpenglow bank tick height.
///
/// For alpenglow banks the bank tick height is `max_tick_height` - 1, as only the Alpentick
/// (fake tick to signal bank completion) will be present.
pub fn set_alpenglow_ticks(bank: &Bank) {
    info!(
        "Alpenglow: Setting tick height for slot {} to {}",
        bank.slot(),
        bank.max_tick_height() - 1
    );
    bank.set_tick_height(bank.max_tick_height() - 1);
}

/// Starting with the root slot corresponding to `start_slot_meta`, iteratively
/// find and process children slots from the blockstore.
///
/// Returns a tuple (a, b) where a is the number of slots processed and b is
/// the number of newly found cluster roots.
#[allow(clippy::too_many_arguments)]
fn load_frozen_forks(
    bank_forks: &RwLock<BankForks>,
    shred_version: u16,
    start_slot_meta: &SlotMeta,
    blockstore: &Blockstore,
    replay_verification_worker_pool: &ReplayVerificationWorkerPool,
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
        "load_frozen_forks() bank forks root: {root}, latest root from blockstore: \
         {blockstore_max_root}, max_root: {max_root}",
    );

    // The total number of slots processed
    let mut total_slots_processed = 0;
    // The total number of newly identified root slots
    let total_rooted_slots = 0;

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
        const STATUS_REPORT_INTERVAL: Duration = Duration::from_secs(2);
        let mut last_status_report = Instant::now();
        let mut slots_processed = 0;
        let mut txs = 0;
        let mut set_root_us = 0;
        let mut root_retain_us = 0;
        let mut process_single_slot_us = 0;
        let mut rooting_us = 0;

        let mut async_verification = None;
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
                     process_single_slot_us:{process_single_slot_us}, rooting_us: {rooting_us}"
                );

                last_status_report = Instant::now();
                slots_processed = 0;
                txs = 0;
                set_root_us = 0;
                root_retain_us = 0;
                process_single_slot_us = 0;
                rooting_us = 0;
            }

            let mut progress = ConfirmationProgress::new_with_async_verification(
                last_entry_hash,
                async_verification.take(),
            );
            // Live replay restarts UpdateParent slots from the marker's FEC set.
            // Startup replay must use the same offset or a restarted validator can
            // execute the obsolete optimistic-parent prefix.
            if bank.feature_set.snapshot().alpenglow_fast_leader_handover
                && leader_slot_index(slot) == 0
                && meta.has_update_parent()
            {
                progress.num_shreds = u64::from(meta.replay_fec_set_index);
            }
            let mut m = Measure::start("process_single_slot");
            let bank = bank_forks.write().unwrap().insert(bank);
            if let Err(error) = process_single_slot(
                blockstore,
                &bank,
                shred_version,
                replay_verification_worker_pool,
                opts,
                &mut progress,
                transaction_status_sender,
                entry_notification_sender,
                timing,
            ) {
                assert!(bank_forks.write().unwrap().remove(bank.slot()).is_some());
                if opts.abort_on_invalid_block {
                    return Err(error);
                }

                continue;
            }
            async_verification = progress.take_async_verification();
            txs += progress.num_txs;

            // Block must be frozen by this point; otherwise,
            // process_single_slot() would have errored above.
            assert!(bank.is_frozen());
            m.stop();
            process_single_slot_us += m.as_us();

            let mut m = Measure::start("rooting");
            // Alpenglow roots are persisted by Votor. Startup replay follows those roots.
            let new_root_bank = (bank_forks.read().unwrap().root() < max_root
                && blockstore.is_root(slot))
            .then_some(&bank);
            m.stop();
            rooting_us += m.as_us();

            if let Some(new_root_bank) = new_root_bank {
                let mut m = Measure::start("set_root");
                root = new_root_bank.slot();

                leader_schedule_cache.set_root(new_root_bank);
                new_root_bank.prune_program_cache(&bank_forks.read().unwrap());
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

fn mark_dead_if_primary_access(blockstore: &Blockstore, slot: Slot) {
    if blockstore.is_primary_access() {
        blockstore
            .set_dead_slot(slot)
            .expect("Failed to mark slot as dead in blockstore");
    } else {
        info!("Failed slot {slot} won't be marked dead due to being read-only blockstore access");
    }
}

// Processes and replays the contents of a single slot, returns Error
// if failed to play the slot.
//
// For use during startup replay, enforces any pre and post replay checks
// that occur in ReplayStage.
#[allow(clippy::too_many_arguments)]
pub fn process_single_slot(
    blockstore: &Blockstore,
    bank: &BankWithScheduler,
    shred_version: u16,
    replay_verification_worker_pool: &ReplayVerificationWorkerPool,
    opts: &ProcessOptions,
    progress: &mut ConfirmationProgress,
    transaction_status_sender: Option<&TransactionStatusSender>,
    entry_notification_sender: Option<&EntryNotifierSender>,
    timing: &mut ExecuteTimings,
) -> result::Result<(), BlockstoreProcessorError> {
    let slot = bank.slot();

    // Mark corrupt slots as dead so validators don't replay this slot and
    // see AlreadyProcessed errors later in ReplayStage
    confirm_full_slot(
        blockstore,
        bank,
        shred_version,
        replay_verification_worker_pool,
        opts,
        progress,
        entry_notification_sender,
        timing,
    )
    .map_err(|err| {
        warn!("slot {slot} failed to verify: {err}");
        mark_dead_if_primary_access(blockstore, slot);
        err
    })?;

    let block_id = blockstore
        .get_block_id(slot)
        .expect("Blockstore operations must succeed")
        .expect("Full block must have block id");
    bank.set_block_id(Some(block_id));
    let verify_result = bank.freeze_and_verify_bank_hash(); // all banks handled by this routine are created from complete slots

    if let Err((expected_hash, computed_hash)) = verify_result {
        warn!(
            "slot {slot} failed to freeze, bank hash mismatch expected {expected_hash} computed \
             {computed_hash}"
        );
        mark_dead_if_primary_access(blockstore, slot);
        return Err(BlockstoreProcessorError::BankHashMismatch(
            slot,
            expected_hash,
            computed_hash,
        ));
    }

    if let Some(slot_callback) = &opts.slot_callback {
        slot_callback(bank);
    }

    if blockstore.is_primary_access() {
        blockstore.insert_bank_hash(bank.slot(), bank.hash());
    }

    if let Some(transaction_status_sender) = transaction_status_sender {
        transaction_status_sender.send_transaction_status_freeze_message(bank);
    }

    Ok(())
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
    let entries = create_ticks(num_slots * ticks_per_slot, 1, last_entry_hash);
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
            genesis_utils::{
                GenesisConfigInfo, create_genesis_config, create_genesis_config_with_leader,
            },
            shred::{ProcessShredsStats, Shred, Shredder},
        },
        agave_transaction_view::transaction_view::SanitizedTransactionView,
        assert_matches::assert_matches,
        crossbeam_channel::bounded,
        rand::{Rng, rng},
        solana_account::AccountSharedData,
        solana_entry::{
            block_component::{
                BlockComponent, BlockFooterV1, BlockHeaderV1, VersionedBlockFooter,
                VersionedBlockMarker,
            },
            entry::{create_ticks, next_entry, next_entry_mut},
        },
        solana_hash::Hash,
        solana_instruction::Instruction,
        solana_instruction_error::InstructionError,
        solana_keypair::Keypair,
        solana_leader_schedule::SlotLeader,
        solana_message::{Message, MessageHeader, compiled_instruction::CompiledInstruction},
        solana_native_token::LAMPORTS_PER_SOL,
        solana_program_runtime::{
            declare_process_instruction, solana_sbpf::program::BuiltinFunctionDefinition,
        },
        solana_pubkey::Pubkey,
        solana_runtime::{
            bank::bank_hash_details::SlotDetails,
            installed_scheduler_pool::{
                InstalledSchedulerPool, MockInstalledScheduler, MockUninstalledScheduler,
                SchedulerAborted, SchedulingContext,
            },
            transaction_execution::TransactionStatusMessage,
        },
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_signer::Signer,
        solana_system_interface::error::SystemError,
        solana_system_transaction as system_transaction,
        solana_transaction::{Transaction, sanitized::MessageHash},
        solana_transaction_error::TransactionError,
        solana_unified_scheduler_pool::DefaultSchedulerPool,
        std::{
            collections::BTreeSet,
            sync::{Arc, Barrier, Mutex, RwLock},
            thread,
        },
        test_case::test_case,
    };

    fn test_process_blockstore(
        genesis_config: &GenesisConfig,
        blockstore: &Blockstore,
        opts: &ProcessOptions,
    ) -> (Arc<RwLock<BankForks>>, LeaderScheduleCache) {
        let exit = Arc::default();
        let (bank_forks, _) = crate::bank_forks_utils::load_bank_forks_from_genesis(
            genesis_config,
            blockstore,
            Vec::new(),
            opts,
            None,
            None,
            None,
            exit,
        )
        .unwrap();
        bank_forks.write().unwrap().install_scheduler_pool(
            DefaultSchedulerPool::new_for_verification(None, None, None, None),
        );

        let leader_schedule_cache =
            LeaderScheduleCache::new_from_bank(&bank_forks.read().unwrap().root_bank());

        process_blockstore_from_root(
            blockstore,
            &bank_forks,
            compute_shred_version(&genesis_config.hash(), None),
            &leader_schedule_cache,
            opts,
            None,
            None,
            None, // snapshots are disabled
        )
        .unwrap();

        (bank_forks, leader_schedule_cache)
    }

    fn take_bank_with_scheduler_for_tests(
        pool: &Arc<DefaultSchedulerPool>,
        bank: Arc<Bank>,
    ) -> BankWithScheduler {
        let context = SchedulingContext::new(bank.clone());
        let scheduler = pool.take_scheduler(context).unwrap();
        BankWithScheduler::new(bank, Some(scheduler))
    }

    fn process_entries_with_pool_for_tests(
        pool: &Arc<DefaultSchedulerPool>,
        bank: &Arc<Bank>,
        entries: Vec<Entry>,
    ) -> Result<()> {
        let bank = take_bank_with_scheduler_for_tests(pool, bank.clone());
        process_entries_for_tests(&bank, entries)
    }

    fn process_entries_for_tests_with_scheduler(
        bank: &Arc<Bank>,
        entries: Vec<Entry>,
    ) -> Result<()> {
        let pool = DefaultSchedulerPool::new_for_verification(None, None, None, None);
        process_entries_with_pool_for_tests(&pool, bank, entries)
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
        process_entries_for_tests_with_scheduler(&bank, slot_entries).unwrap();
        assert_eq!(bank.process_transaction(&tx), Ok(()));
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
        let (bank_forks, ..) = test_process_blockstore(&genesis_config, &blockstore, &opts);
        let bank_forks = bank_forks.read().unwrap();

        assert_eq!(frozen_bank_slots(&bank_forks), vec![0]);
        let bank = bank_forks[0].clone();
        assert_eq!(bank.tick_height(), 1);
    }

    #[test]
    fn test_process_entries_tick() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(1000);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));

        // ensure bank can process a tick
        assert_eq!(bank.tick_height(), 0);
        let tick = next_entry(&genesis_config.hash(), 1, vec![]);
        assert_eq!(
            process_entries_for_tests_with_scheduler(&bank, vec![tick]),
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
            process_entries_for_tests_with_scheduler(&bank, vec![entry_1, entry_2]),
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
            process_entries_for_tests_with_scheduler(
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
                good_tx,
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
            process_entries_for_tests_with_scheduler(
                &bank,
                vec![entry_1_to_mint.clone(), entry_2_to_3_mint_to_1.clone()],
            ),
            Err(TransactionError::BlockhashNotFound)
        );

        // The scheduler commits each transaction individually and aborts asynchronously,
        // so the other transactions may or may not have been committed by now; only the failing
        // transaction is guaranteed not to have landed. In production such a block is marked
        // dead and its bank discarded, so any partial commit is never visible.
        assert_eq!(bank.get_balance(&keypair4.pubkey()), 4);

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

        // ensure the bank still processes new entries after the aborted scheduler; keypair4's
        // only prior transaction was the one guaranteed to have failed, so this outcome is
        // deterministic
        let pubkey5 = Pubkey::new_unique();
        let entry_3 = next_entry(
            &entry_2_to_3_mint_to_1.hash,
            1,
            vec![system_transaction::transfer(
                &keypair4,
                &pubkey5,
                1,
                bank.last_blockhash(),
            )],
        );
        assert_matches!(
            process_entries_for_tests_with_scheduler(&bank, vec![entry_3]),
            Ok(())
        );
        assert_eq!(bank.get_balance(&keypair4.pubkey()), 3);
        assert_eq!(bank.get_balance(&pubkey5), 1);
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
            MockBuiltinOk::register,
        );

        let tx = Transaction::new_signed_with_payer(
            &[Instruction::new_with_wincode(
                mock_program_id,
                &10,
                Vec::new(),
            )],
            Some(&mint_keypair.pubkey()),
            &[&mint_keypair],
            bank.last_blockhash(),
        );

        let entry = next_entry(&bank.last_blockhash(), 1, vec![tx]);
        let result = process_entries_for_tests_with_scheduler(&bank, vec![entry]);
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
                MockBuiltinErr::register,
            );

            let tx = Transaction::new_signed_with_payer(
                &[Instruction::new_with_wincode(
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
            let result = process_entries_for_tests_with_scheduler(&bank, vec![entry]);
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

    #[test]
    fn test_process_entries_2nd_entry_collision_with_self_and_error() {
        agave_logger::setup();

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(1000);
        let bank = Bank::new_for_tests(&genesis_config);
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

        // transactions within an entry may read/write and write/write the same accounts, so the
        // colliding entry is valid and the scheduler orders the conflicts itself
        let result = process_entries_for_tests_with_scheduler(
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

        assert!(result.is_ok());
        assert_eq!(balances, [0, 3, 3]);
    }

    #[test]
    fn test_process_entry_duplicate_transaction() {
        agave_logger::setup();

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(1000);
        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();
        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();

        // fund: put some money in each of 1 and 2
        assert_matches!(bank.transfer(5, &mint_keypair, &keypair1.pubkey()), Ok(_));
        assert_matches!(bank.transfer(5, &mint_keypair, &keypair2.pubkey()), Ok(_));

        // The scheduler executes identical transactions sequentially. The first transfer commits,
        // then the status cache rejects the second as already processed.

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

        let result = process_entries_for_tests_with_scheduler(&bank, vec![entry_1_to_2_twice]);

        assert_eq!(result, Err(TransactionError::AlreadyProcessed));
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
            process_entries_for_tests_with_scheduler(&bank, vec![entry_1, entry_2]),
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
            process_entries_for_tests_with_scheduler(&bank, entries),
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
            process_entries_for_tests_with_scheduler(&bank, vec![entry]),
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

    #[test_case(false; "strict_fee_payer")]
    #[test_case(true; "relaxed_fee_payer")]
    fn test_process_entries_2_entries_tick(relax_fee_payer_constraint: bool) {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(1000);
        let mut bank = Bank::new_for_tests(&genesis_config);
        if !relax_fee_payer_constraint {
            bank.deactivate_feature(&agave_feature_set::relax_fee_payer_constraint::id());
        }
        let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();
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
            process_entries_for_tests_with_scheduler(&bank, vec![entry_1, tick, entry_2.clone()],),
            Ok(())
        );
        assert_eq!(bank.get_balance(&keypair3.pubkey()), 1);
        assert_eq!(bank.get_balance(&keypair4.pubkey()), 1);

        // an error is returned for an empty fee-payer unless `relax_fee_payer_constraint` is enabled
        let tx =
            system_transaction::transfer(&keypair2, &keypair3.pubkey(), 1, bank.last_blockhash());
        let entry_3 = next_entry(&entry_2.hash, 1, vec![tx]);
        assert_eq!(
            process_entries_for_tests_with_scheduler(&bank, vec![entry_3]),
            if relax_fee_payer_constraint {
                Ok(())
            } else {
                Err(TransactionError::AccountNotFound)
            }
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
            &[Instruction::new_with_wincode(
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

    #[test]
    fn test_update_transaction_statuses_fail() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(11_000);
        let bank = Bank::new_for_tests(&genesis_config);
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

        assert_eq!(
            process_entries_for_tests_with_scheduler(&bank, vec![entry_1_to_mint]),
            Ok(())
        );

        assert_eq!(
            bank.process_transaction(&test_tx),
            Err(TransactionError::AlreadyProcessed)
        );
    }

    #[test]
    fn test_halt_at_slot_starting_snapshot_root() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(123);

        // Create a valid Alpenglow slot 0, then persist a later root.
        let (ledger_path, _) = create_new_tmp_ledger_auto_delete!(&genesis_config);
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        blockstore.set_roots([0, 1].iter()).unwrap();

        // Specify halting at slot 0
        let opts = ProcessOptions {
            run_verification: true,
            halt_at_slot: Some(0),
            ..ProcessOptions::default()
        };
        let (bank_forks, ..) = test_process_blockstore(&genesis_config, &blockstore, &opts);
        let bank_forks = bank_forks.read().unwrap();

        // Should be able to fetch slot 0 because we specified halting at slot 0, even
        // if there is a greater root at slot 1.
        assert!(bank_forks.get(0).is_some());
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
        let bank = Bank::new_for_tests(&genesis_config);
        let (mut bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();

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
            process_entries_for_tests_with_scheduler(&bank, entries).expect("paying failed");

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
            process_entries_for_tests_with_scheduler(&bank, entries).expect("refunding failed");

            // advance to next block
            process_entries_for_tests_with_scheduler(
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
            bank = Arc::new(Bank::new_from_parent(bank, SlotLeader::default(), slot));
        }
    }

    fn frozen_bank_slots(bank_forks: &BankForks) -> Vec<Slot> {
        let mut slots: Vec<_> = bank_forks
            .frozen_banks()
            .map(|(slot, _bank)| slot)
            .collect();
        slots.sort_unstable();
        slots
    }

    fn confirm_slot_entries_with_pool_for_tests(
        pool: &Arc<DefaultSchedulerPool>,
        bank: &Arc<Bank>,
        slot_entries: Vec<Entry>,
        slot_full: bool,
        progress: &mut ConfirmationProgress,
    ) -> result::Result<(), BlockstoreProcessorError> {
        let replay_verification_worker_pool = ReplayVerificationWorkerPool::new(1);
        let bank = take_bank_with_scheduler_for_tests(pool, bank.clone());
        let result = confirm_slot_entries(
            &bank,
            &replay_verification_worker_pool,
            (entry_views_for_tests(slot_entries), 0, slot_full),
            &mut ConfirmationTiming::default(),
            progress,
            false,
            None,
        );
        let (wait_result, _timings) = bank.wait_for_completed_scheduler().unwrap();
        result?;
        progress.wait_for_all_verification_results(&mut 0, &mut 0)?;
        Ok(wait_result?)
    }

    fn confirm_slot_entries_for_tests(
        bank: &Arc<Bank>,
        slot_entries: Vec<Entry>,
        slot_full: bool,
        prev_entry_hash: Hash,
    ) -> result::Result<(), BlockstoreProcessorError> {
        let pool = DefaultSchedulerPool::new_for_verification(None, None, None, None);
        let mut progress = ConfirmationProgress::new(prev_entry_hash);
        confirm_slot_entries_with_pool_for_tests(
            &pool,
            bank,
            slot_entries,
            slot_full,
            &mut progress,
        )
    }

    fn create_test_transactions(
        mint_keypair: &Keypair,
        genesis_hash: &Hash,
    ) -> Vec<ReplayTransaction> {
        let pubkey = solana_pubkey::new_rand();
        let keypair2 = Keypair::new();
        let pubkey2 = solana_pubkey::new_rand();
        let keypair3 = Keypair::new();
        let pubkey3 = solana_pubkey::new_rand();

        vec![
            ReplayTransaction::from(system_transaction::transfer(
                mint_keypair,
                &pubkey,
                1,
                *genesis_hash,
            )),
            ReplayTransaction::from(system_transaction::transfer(
                &keypair2,
                &pubkey2,
                1,
                *genesis_hash,
            )),
            ReplayTransaction::from(system_transaction::transfer(
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
        let (transaction_status_sender, transaction_status_receiver) = bounded(1024);
        let transaction_status_sender = TransactionStatusSender {
            sender: transaction_status_sender,
            dependency_tracker: None,
        };
        let pool = DefaultSchedulerPool::new_for_verification(
            None,
            None,
            Some(transaction_status_sender),
            None,
        );
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

        confirm_slot_entries_with_pool_for_tests(&pool, &bank, vec![entry], false, &mut progress)
            .unwrap();
        assert_eq!(progress.num_txs, 2);
        // The unified scheduler executes each transaction as its own task, so statuses arrive
        // as multiple batches in no particular order.
        let indexes = receive_transaction_indexes(&transaction_status_receiver);
        assert_eq!(indexes, [0, 1]);

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

        confirm_slot_entries_with_pool_for_tests(&pool, &bank, vec![entry], false, &mut progress)
            .unwrap();
        assert_eq!(progress.num_txs, 5);
        let indexes = receive_transaction_indexes(&transaction_status_receiver);
        assert_eq!(indexes, [2, 3, 4]);
    }

    fn receive_transaction_indexes(
        receiver: &crossbeam_channel::Receiver<TransactionStatusMessage>,
    ) -> Vec<usize> {
        let mut indexes = vec![];
        while let Ok(message) = receiver.try_recv() {
            match message {
                TransactionStatusMessage::Batch((batch, _sequence)) => {
                    assert_eq!(batch.transactions.len(), batch.transaction_indexes.len());
                    indexes.extend_from_slice(&batch.transaction_indexes);
                }
                TransactionStatusMessage::Freeze(_) => {}
            }
        }
        indexes.sort();
        indexes
    }

    #[test]
    fn test_confirm_slot_entries_async_sigverify_fail() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(100 * LAMPORTS_PER_SOL);
        let genesis_hash = genesis_config.hash();
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let mut tx =
            system_transaction::transfer(&mint_keypair, &Pubkey::new_unique(), 1, genesis_hash);
        tx.signatures[0] = solana_signature::Signature::default();
        let entry = Entry::new(&genesis_hash, 1, vec![tx]);

        assert_matches!(
            confirm_slot_entries_for_tests(&bank, vec![entry], false, genesis_hash),
            Err(BlockstoreProcessorError::InvalidTransaction(
                TransactionError::SignatureFailure
            ))
        );
    }

    #[test]
    fn test_verification_workers_small_capacity() {
        let num_workers = 2;
        let channel_capacity = 1;
        let worker_pool =
            ReplayVerificationWorkerPool::with_capacity(num_workers, channel_capacity);
        let mut progress = AsyncVerificationProgress::new(channel_capacity);
        let start_hash = Hash::new_unique();
        let verification_entries = entry::entries_to_verification_data(&create_ticks(
            (num_workers + 1) as u64,
            1,
            start_hash,
        ));

        for _ in 0..10 {
            progress
                .spawn_poh_verification(&worker_pool, verification_entries.clone(), start_hash, 0)
                .unwrap();
            assert_eq!(progress.pending_jobs, channel_capacity);
        }

        progress.wait_for_all_results().unwrap();
        assert_eq!(progress.pending_jobs, 0);
    }

    #[test]
    fn test_verification_workers_max_results() {
        let num_workers = 4;
        // ideally we'd use MAX_FEC_SETS_PER_SLOT here, but CI is too slow to be true
        let fake_max_fec_sets_per_slot = 10usize;
        let job_capacity = fake_max_fec_sets_per_slot
            // poh + signature verification
            .checked_mul(2)
            .unwrap()
            // each poh/signature batch can be split into multiple jobs, at most 1 for each worker
            .checked_mul(num_workers)
            .expect("verification job queue capacity overflow");
        let worker_pool = ReplayVerificationWorkerPool::with_capacity(num_workers, job_capacity);

        // make sure that each batch generates work for each worker
        let num_items = num_workers + 1;

        let start_hash = Hash::new_unique();
        let verification_entries =
            entry::entries_to_verification_data(&create_ticks(num_items as u64, 1, start_hash));
        let transaction = system_transaction::transfer(
            &Keypair::new(),
            &Pubkey::new_unique(),
            1,
            Hash::new_unique(),
        );
        let entry = Entry::new(&Hash::new_unique(), 1, vec![transaction; num_items]);

        // Simulate two parallel banks. This tests that we make progress even when the number of
        // jobs exceeds the capacity of the pool.
        let result_channel_capacity = worker_pool.job_capacity;
        let mut progresses = [
            AsyncVerificationProgress::new(result_channel_capacity),
            AsyncVerificationProgress::new(result_channel_capacity),
        ];
        // simulate full slots
        for _ in 0..fake_max_fec_sets_per_slot {
            for (slot, progress) in progresses.iter_mut().enumerate() {
                let slot = slot as Slot;
                progress
                    .spawn_poh_verification(
                        &worker_pool,
                        verification_entries.clone(),
                        start_hash,
                        slot,
                    )
                    .unwrap();
                let unverified_signatures = entry::validate_and_hash_transactions(
                    entry_views_for_tests(vec![entry.clone()]),
                    num_items,
                    transaction_hash_verify_thread_pool(),
                    |unsanitized: UnsanitizedTransactionView<Bytes>| {
                        let sanitized = unsanitized
                            .sanitize(
                                &solana_runtime_transaction::sanitize_config::sanitize_config(),
                            )
                            .map_err(|_| TransactionError::SanitizeFailure)?;
                        let statically_loaded = RuntimeTransaction::<
                            SanitizedTransactionView<Bytes>,
                        >::try_new(
                            sanitized, MessageHash::Compute
                        )?;
                        ReplayTransaction::try_new(
                            statically_loaded,
                            None,
                            &agave_reserved_account_keys::ReservedAccountKeys::empty_key_set(),
                        )
                    },
                )
                .unwrap()
                .unverified_signatures;
                progress
                    .spawn_signature_verification(&worker_pool, unverified_signatures, slot)
                    .unwrap();
            }
        }

        let expected_results = result_channel_capacity;
        for progress in &progresses {
            assert_eq!(progress.pending_jobs, expected_results);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while progresses
            .iter()
            .any(|progress| progress.receiver.len() < expected_results)
            && Instant::now() < deadline
        {
            thread::yield_now();
        }
        let result_counts = progresses
            .each_ref()
            .map(|progress| progress.receiver.len());

        for progress in &mut progresses {
            progress.wait_for_all_results().unwrap();
        }
        assert_eq!(
            result_counts, [expected_results; 2],
            "verification workers did not send all results before the timeout"
        );
    }

    #[test]
    fn test_async_verification_progress_drop() {
        struct BlockingVerificationJob {
            job: VerificationJob,
            barrier: Arc<Barrier>,
        }

        impl WorkerJob for BlockingVerificationJob {
            fn run(self) {
                self.barrier.wait();
                self.job.run();
            }
        }

        let pool = WorkerPool::new("solReplayTest", 1, 1);
        let barrier = Arc::new(Barrier::new(2));
        let progress = AsyncVerificationProgress::new(1);
        let entries = Arc::new(VerificationBatch {
            data: Vec::new(),
            started: Instant::now(),
            remaining_jobs: AtomicUsize::new(1),
        });
        let result_sender = progress.sender.clone();
        pool.send(BlockingVerificationJob {
            job: VerificationJob::Poh(PohVerificationJob {
                entries,
                range: 0..0,
                start_hash: Hash::default(),
                slot: 0,
                result_sender,
            }),
            barrier: Arc::clone(&barrier),
        });
        // this tests that dropping the progress while a job is running does not panic. Can happen
        // when a slot is dumped.
        drop(progress);
        barrier.wait();
        // this will join the pool
        drop(pool);
    }

    #[test]
    fn test_process_entries_tick_only_requires_no_scheduler() {
        // A tick-only slot (e.g. slot 0, replayed before the scheduler pool is
        // installed) has no transaction entries, so it must process without one.
        let genesis_config = create_genesis_config(100).genesis_config;
        let bank = BankWithScheduler::new_without_scheduler(Arc::new(Bank::new_for_tests(
            &genesis_config,
        )));
        let tick = next_entry(&genesis_config.hash(), 1, vec![]);
        assert_eq!(process_entries_for_tests(&bank, vec![tick]), Ok(()));
    }

    #[test]
    #[should_panic(expected = "no scheduler installed for bank of slot 0")]
    fn test_process_entries_asserts_installed_scheduler() {
        // Given: a bank with no scheduler installed and a single transaction entry
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(100);
        let bank = BankWithScheduler::new_without_scheduler(Arc::new(Bank::new_for_tests(
            &genesis_config,
        )));
        let tx = system_transaction::transfer(
            &mint_keypair,
            &Pubkey::new_unique(),
            1,
            genesis_config.hash(),
        );
        let entry = next_entry(&genesis_config.hash(), 1, vec![tx]);

        // When: processing the entry
        // Then: unreachable; the missing-scheduler assert must have fired
        let _ = process_entries_for_tests(&bank, vec![entry]);
    }

    fn do_test_process_entries(should_succeed: bool) {
        agave_logger::setup();
        let dummy_leader_pubkey = solana_pubkey::new_rand();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config_with_leader(500, &dummy_leader_pubkey, 100);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let context = SchedulingContext::new(bank.clone());

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

        // process_batches was removed; drive the same scheduling path through
        // process_entries with a single transaction entry.
        let replay_entry = ReplayEntry {
            entry: EntryType::Transactions(txs),
            starting_index: 0,
        };

        let result = process_entries(&bank, vec![replay_entry]);
        if should_succeed {
            assert_matches!(result, Ok(()));
        } else {
            assert_matches!(result, Err(TransactionError::InsufficientFundsForFee));
        }
    }

    #[test]
    fn test_process_entries_success() {
        do_test_process_entries(true);
    }

    #[test]
    fn test_process_entries_failure() {
        do_test_process_entries(false);
    }

    fn confirm_slot_with_block_markers_common(
        footer_before_alpentick: bool,
        mut genesis_config: GenesisConfig,
    ) -> (
        Blockstore,
        GenesisConfig,
        tempfile::TempDir,
        ReplayVerificationWorkerPool,
    ) {
        let ticks_per_slot = 1;
        genesis_config.ticks_per_slot = ticks_per_slot;

        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();
        let keypair = Arc::new(Keypair::new());

        let header = VersionedBlockMarker::from_block_header(BlockHeaderV1 {
            parent_slot: 0,
            parent_block_id: Hash::default(),
        });
        let header_component = BlockComponent::new_block_marker(header);

        let block_producer_time_nanos = u64::try_from(
            genesis_config
                .creation_time
                .saturating_mul(1_000_000_000)
                .saturating_add(1),
        )
        .unwrap();
        let footer = VersionedBlockMarker::from_block_footer(BlockFooterV1 {
            bank_hash: Hash::new_from_array([42; 32]),
            block_producer_time_nanos,
            block_user_agent: b"test".to_vec(),
            block_final_cert: None,
            skip_reward_cert: None,
            notar_reward_cert: None,
        });
        let footer_component = BlockComponent::new_block_marker(footer);

        let shredder = Shredder::new(1, 0, 0, 0).unwrap();
        let mut next_shred_index = 0u32;

        let header_shreds: Vec<Shred> = shredder
            .make_merkle_shreds_from_component(
                &keypair,
                &header_component,
                false,
                Hash::default(),
                next_shred_index,
                next_shred_index, // next_code_index
                &mut ProcessShredsStats::default(),
            )
            .into_iter()
            .filter(Shred::is_data)
            .collect();
        next_shred_index = header_shreds.last().unwrap().index() + 1;

        let mut all_shreds = header_shreds;
        let entries = vec![next_entry(&genesis_config.hash(), 1, vec![])];
        if footer_before_alpentick {
            let footer_shreds: Vec<Shred> = shredder
                .make_merkle_shreds_from_component(
                    &keypair,
                    &footer_component,
                    false,
                    Hash::default(),
                    next_shred_index,
                    next_shred_index, // next_code_index
                    &mut ProcessShredsStats::default(),
                )
                .into_iter()
                .filter(Shred::is_data)
                .collect();
            next_shred_index = footer_shreds.last().unwrap().index() + 1;

            let entry_shreds: Vec<Shred> = shredder
                .make_merkle_shreds_from_entries(
                    &keypair,
                    &entries,
                    true, // last in slot
                    Hash::default(),
                    next_shred_index,
                    next_shred_index, // next_code_index
                    &mut ProcessShredsStats::default(),
                )
                .into_iter()
                .filter(Shred::is_data)
                .collect();

            all_shreds.extend(footer_shreds);
            all_shreds.extend(entry_shreds);
        } else {
            let entry_shreds: Vec<Shred> = shredder
                .make_merkle_shreds_from_entries(
                    &keypair,
                    &entries,
                    false,
                    Hash::default(),
                    next_shred_index,
                    next_shred_index, // next_code_index
                    &mut ProcessShredsStats::default(),
                )
                .into_iter()
                .filter(Shred::is_data)
                .collect();
            next_shred_index = entry_shreds.last().unwrap().index() + 1;

            let footer_shreds: Vec<Shred> = shredder
                .make_merkle_shreds_from_component(
                    &keypair,
                    &footer_component,
                    true, // last in slot
                    Hash::default(),
                    next_shred_index,
                    next_shred_index, // next_code_index
                    &mut ProcessShredsStats::default(),
                )
                .into_iter()
                .filter(Shred::is_data)
                .collect();

            all_shreds.extend(entry_shreds);
            all_shreds.extend(footer_shreds);
        }
        blockstore.insert_shreds(all_shreds, true).unwrap();

        let replay_verification_worker_pool = ReplayVerificationWorkerPool::new(1);

        (
            blockstore,
            genesis_config,
            ledger_path,
            replay_verification_worker_pool,
        )
    }

    #[test]
    fn test_confirm_slot_block_with_markers_succeeds_with_alpenglow() {
        let genesis_config = create_genesis_config(100 * LAMPORTS_PER_SOL).genesis_config;
        let (blockstore, genesis_config, _ledger_path, replay_verification_worker_pool) =
            confirm_slot_with_block_markers_common(true, genesis_config);

        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis_config));
        let bank0 = bank_forks.read().unwrap().get(0).unwrap();
        let bank1 = Bank::new_from_parent(bank0.clone(), SlotLeader::default(), 1);
        let bank1 = bank_forks.write().unwrap().insert(bank1);
        let (entry_notification_sender, entry_notification_receiver) =
            bounded::<EntryNotification>(2);

        confirm_slot(
            &blockstore,
            &bank1,
            compute_shred_version(&genesis_config.hash(), None),
            &replay_verification_worker_pool,
            &mut ConfirmationTiming::default(),
            &mut ConfirmationProgress::new(bank0.last_blockhash()),
            true,
            Some(&entry_notification_sender),
            None,
            false,
        )
        .unwrap();

        let EntryNotification::BlockFooter {
            slot,
            bank_id,
            block_footer,
        } = entry_notification_receiver.try_recv().unwrap()
        else {
            panic!("expected block footer notification before the alpentick entry");
        };
        assert_eq!(slot, bank1.slot());
        assert_eq!(bank_id, bank1.bank_id());
        let VersionedBlockFooter::V1(block_footer) = *block_footer;
        assert_eq!(block_footer.bank_hash, Hash::new_from_array([42; 32]));
        assert_eq!(
            block_footer.block_producer_time_nanos,
            u64::try_from(
                genesis_config
                    .creation_time
                    .saturating_mul(1_000_000_000)
                    .saturating_add(1),
            )
            .unwrap()
        );
        assert_eq!(block_footer.block_user_agent, b"test");
        assert!(block_footer.block_final_cert.is_none());
        assert!(block_footer.skip_reward_cert.is_none());
        assert!(block_footer.notar_reward_cert.is_none());

        let EntryNotification::Entry {
            slot,
            bank_id,
            index,
            entry,
            starting_transaction_index,
        } = entry_notification_receiver.try_recv().unwrap()
        else {
            panic!("expected alpentick entry notification after the block footer");
        };
        assert_eq!(slot, bank1.slot());
        assert_eq!(bank_id, bank1.bank_id());
        assert_eq!(index, 0);
        assert_eq!(entry.num_transactions, 0);
        assert_eq!(starting_transaction_index, 0);
        assert!(entry_notification_receiver.try_recv().is_err());
    }

    #[test]
    fn test_confirm_slot_rejects_alpentick_before_footer() {
        let genesis_config = create_genesis_config(100 * LAMPORTS_PER_SOL).genesis_config;
        let (blockstore, genesis_config, _ledger_path, replay_verification_worker_pool) =
            confirm_slot_with_block_markers_common(false, genesis_config);

        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis_config));
        let bank0 = bank_forks.read().unwrap().get(0).unwrap();
        let bank1 = Bank::new_from_parent(bank0.clone(), SlotLeader::default(), 1);
        let bank1 = bank_forks.write().unwrap().insert(bank1);

        let result = confirm_slot(
            &blockstore,
            &bank1,
            compute_shred_version(&genesis_config.hash(), None),
            &replay_verification_worker_pool,
            &mut ConfirmationTiming::default(),
            &mut ConfirmationProgress::new(bank0.last_blockhash()),
            true,
            None,
            None,
            false,
        );
        assert_matches!(
            result,
            Err(BlockstoreProcessorError::BlockComponentProcessor(
                BlockComponentProcessorError::InvalidAlpentickPosition
            ))
        );
    }

    #[test]
    fn test_startup_parent_id_check() {
        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path()).unwrap();

        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis_config));
        let bank0 = bank_forks.read().unwrap().get(0).unwrap();
        let parent_bank = Arc::new(Bank::new_from_parent(bank0, SlotLeader::default(), 1));
        let parent_block_id = Hash::new_unique();
        parent_bank.set_block_id(Some(parent_block_id));

        let leader_schedule_cache = LeaderScheduleCache::new_from_bank(&parent_bank);
        let mut parent_meta = SlotMeta::new(1, Some(0));
        parent_meta.next_slots = smallvec::smallvec![2, 3];

        for (slot, block_id) in [(2, Hash::new_unique()), (3, parent_block_id)] {
            let mut meta = SlotMeta::new(slot, Some(1));
            meta.consumed = 1;
            meta.received = 1;
            meta.last_index = Some(0);
            meta.parent_block_id = block_id;
            meta.replay_fec_set_index = 32;
            blockstore.put_meta(slot, &meta).unwrap();
        }

        let mut pending_slots = Vec::new();
        process_next_slots(
            &parent_bank,
            &parent_meta,
            &blockstore,
            &leader_schedule_cache,
            &mut pending_slots,
            &ProcessOptions::default(),
        )
        .unwrap();

        assert_eq!(pending_slots.len(), 1);
        assert_eq!(pending_slots[0].1.slot(), 3);

        let mut pending_slots = Vec::new();
        process_next_slots(
            &parent_bank,
            &parent_meta,
            &blockstore,
            &leader_schedule_cache,
            &mut pending_slots,
            &ProcessOptions {
                skip_inter_slot_verification: true,
                ..ProcessOptions::default()
            },
        )
        .unwrap();

        assert_eq!(
            pending_slots
                .iter()
                .map(|(_, bank, _)| bank.slot())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([2, 3])
        );
    }

    #[test]
    fn test_validate_entry_transactions_ok() {
        let payer = Keypair::new();
        let hash = Hash::new_unique();
        let txs = vec![
            ReplayTransaction::from(system_transaction::transfer(
                &payer,
                &Pubkey::new_unique(),
                1,
                hash,
            )),
            ReplayTransaction::from(system_transaction::transfer(
                &payer,
                &Pubkey::new_unique(),
                1,
                hash,
            )),
        ];
        assert_eq!(validate_entry_transactions(&txs, 10), Ok(()));
    }

    #[test]
    fn test_validate_entry_transactions_too_many_locks() {
        let txs = vec![ReplayTransaction::from(system_transaction::transfer(
            &Keypair::new(),
            &Pubkey::new_unique(),
            1,
            Hash::new_unique(),
        ))];
        // transfer touches >1 account; limit of 1 must reject
        assert_eq!(
            validate_entry_transactions(&txs, 1),
            Err(TransactionError::TooManyAccountLocks)
        );
    }

    #[test]
    fn test_validate_entry_transactions_account_loaded_twice() {
        // Message compilation dedups account keys, so a normal transfer can't repeat one.
        // Hand-build a message whose account_keys list the payer twice to exercise the
        // duplicate-key check.
        let payer = Keypair::new();
        let message = Message {
            header: MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 1,
            },
            account_keys: vec![
                payer.pubkey(),
                payer.pubkey(),
                solana_system_interface::program::id(),
            ],
            recent_blockhash: Hash::new_unique(),
            instructions: vec![CompiledInstruction::new(2, &(), vec![0, 1])],
        };
        let txs = vec![ReplayTransaction::from(Transaction::new(
            &[&payer],
            message,
            Hash::new_unique(),
        ))];
        assert_eq!(
            validate_entry_transactions(&txs, 10),
            Err(TransactionError::AccountLoadedTwice)
        );
    }
}
