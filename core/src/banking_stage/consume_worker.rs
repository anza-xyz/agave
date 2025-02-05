use {
    super::{
        consumer::{Consumer, ExecuteAndCommitTransactionsOutput, ProcessTransactionBatchOutput},
        leader_slot_timing_metrics::LeaderExecuteAndCommitTimings,
        scheduler_messages::{ConsumeWork, FinishedConsumeWork},
    },
    crossbeam_channel::{Receiver, SendError, Sender, TryRecvError},
    solana_measure::measure_us,
    solana_poh::leader_bank_notifier::LeaderBankNotifier,
    solana_runtime::bank::Bank,
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    solana_sdk::timing::AtomicInterval,
    solana_svm::transaction_error_metrics::TransactionErrorMetrics,
    std::{sync::Arc, time::Duration},
    thiserror::Error,
};

#[derive(Debug, Error)]
pub enum ConsumeWorkerError<Tx> {
    #[error("Failed to receive work from scheduler: {0}")]
    Recv(#[from] TryRecvError),
    #[error("Failed to send finalized consume work to scheduler: {0}")]
    Send(#[from] SendError<FinishedConsumeWork<Tx>>),
}

pub(crate) struct ConsumeWorker<Tx> {
    consume_receiver: Receiver<ConsumeWork<Tx>>,
    consumer: Consumer,
    consumed_sender: Sender<FinishedConsumeWork<Tx>>,

    leader_bank_notifier: Arc<LeaderBankNotifier>,
    metrics: ConsumeWorkerMetrics,
}

impl<Tx: TransactionWithMeta> ConsumeWorker<Tx> {
    pub fn new(
        id: u32,
        consume_receiver: Receiver<ConsumeWork<Tx>>,
        consumer: Consumer,
        consumed_sender: Sender<FinishedConsumeWork<Tx>>,
        leader_bank_notifier: Arc<LeaderBankNotifier>,
    ) -> Self {
        Self {
            consume_receiver,
            consumer,
            consumed_sender,
            leader_bank_notifier,
            metrics: ConsumeWorkerMetrics::new(id),
        }
    }

    pub fn run(mut self) -> Result<(), ConsumeWorkerError<Tx>> {
        loop {
            match self.consume_receiver.try_recv() {
                Ok(work) => {
                    self.consume_loop(work)?;
                }
                Err(TryRecvError::Empty) => {
                    const SLEEP_DURATION: Duration = Duration::from_millis(5);
                    std::thread::sleep(SLEEP_DURATION);
                }
                Err(err) => {
                    return Err(ConsumeWorkerError::from(err));
                }
            }
            self.metrics.maybe_report_and_reset();
        }
    }

    fn consume_loop(&mut self, work: ConsumeWork<Tx>) -> Result<(), ConsumeWorkerError<Tx>> {
        let (maybe_consume_bank, get_bank_us) = measure_us!(self.get_consume_bank());

        let Some(mut bank) = maybe_consume_bank else {
            self.metrics.timing_metrics.wait_for_bank_failure_us += get_bank_us;
            return self.retry_drain(work);
        };
        self.metrics.timing_metrics.wait_for_bank_success_us += get_bank_us;

        for work in try_drain_iter(work, &self.consume_receiver.clone()) {
            if bank.is_complete() || {
                // check if the bank got interrupted before completion
                self.get_consume_bank_id() != Some(bank.bank_id())
            } {
                let (maybe_new_bank, get_bank_us) = measure_us!(self.get_consume_bank());
                if let Some(new_bank) = maybe_new_bank {
                    self.metrics.timing_metrics.wait_for_bank_success_us += get_bank_us;
                    bank = new_bank;
                } else {
                    self.metrics.timing_metrics.wait_for_bank_failure_us += get_bank_us;
                    return self.retry_drain(work);
                }
            }
            self.consume(&bank, work)?;
        }

        Ok(())
    }

    /// Consume a single batch.
    fn consume(
        &mut self,
        bank: &Arc<Bank>,
        work: ConsumeWork<Tx>,
    ) -> Result<(), ConsumeWorkerError<Tx>> {
        let output = self.consumer.process_and_record_aged_transactions(
            bank,
            &work.transactions,
            &work.max_ages,
        );

        self.metrics.update_for_consume(&output);
        self.metrics.has_data = true;

        self.consumed_sender.send(FinishedConsumeWork {
            work,
            retryable_indexes: output
                .execute_and_commit_transactions_output
                .retryable_transaction_indexes,
        })?;
        Ok(())
    }

    /// Try to get a bank for consuming.
    fn get_consume_bank(&self) -> Option<Arc<Bank>> {
        self.leader_bank_notifier
            .get_or_wait_for_in_progress(Duration::from_millis(50))
            .upgrade()
    }

    /// Try to get the id for the bank that should be used for consuming
    fn get_consume_bank_id(&self) -> Option<u64> {
        self.leader_bank_notifier.get_current_bank_id()
    }

    /// Retry current batch and all outstanding batches.
    fn retry_drain(&mut self, work: ConsumeWork<Tx>) -> Result<(), ConsumeWorkerError<Tx>> {
        for work in try_drain_iter(work, &self.consume_receiver.clone()) {
            self.retry(work)?;
        }
        Ok(())
    }

    /// Send transactions back to scheduler as retryable.
    fn retry(&mut self, work: ConsumeWork<Tx>) -> Result<(), ConsumeWorkerError<Tx>> {
        let retryable_indexes: Vec<_> = (0..work.transactions.len()).collect();
        let num_retryable = retryable_indexes.len();
        self.metrics.count_metrics.retryable_transaction_count += num_retryable;
        self.metrics.count_metrics.retryable_expired_bank_count += num_retryable;
        self.metrics.has_data = true;
        self.consumed_sender.send(FinishedConsumeWork {
            work,
            retryable_indexes,
        })?;
        Ok(())
    }
}

/// Helper function to create an non-blocking iterator over work in the receiver,
/// starting with the given work item.
fn try_drain_iter<T>(work: T, receiver: &Receiver<T>) -> impl Iterator<Item = T> + '_ {
    std::iter::once(work).chain(receiver.try_iter())
}

/// Metrics tracking number of packets processed by the consume worker.
/// These are atomic, and intended to be reported by the scheduling thread
/// since the consume worker thread is sleeping unless there is work to be
/// done.
pub(crate) struct ConsumeWorkerMetrics {
    id: String,
    interval: AtomicInterval,
    has_data: bool,

    count_metrics: ConsumeWorkerCountMetrics,
    error_metrics: ConsumeWorkerTransactionErrorMetrics,
    timing_metrics: ConsumeWorkerTimingMetrics,
}

impl ConsumeWorkerMetrics {
    /// Report and reset metrics iff the interval has elapsed and the worker did some work.
    pub fn maybe_report_and_reset(&mut self) {
        const REPORT_INTERVAL_MS: u64 = 20;
        if self.interval.should_update(REPORT_INTERVAL_MS) && self.has_data {
            self.has_data = false;
            self.count_metrics.report_and_reset(&self.id);
            self.timing_metrics.report_and_reset(&self.id);
            self.error_metrics.report_and_reset(&self.id);
        }
    }

    fn new(id: u32) -> Self {
        Self {
            id: id.to_string(),
            interval: AtomicInterval::default(),
            has_data: false,
            count_metrics: ConsumeWorkerCountMetrics::default(),
            error_metrics: ConsumeWorkerTransactionErrorMetrics::default(),
            timing_metrics: ConsumeWorkerTimingMetrics::default(),
        }
    }

    fn update_for_consume(
        &mut self,
        ProcessTransactionBatchOutput {
            cost_model_throttled_transactions_count,
            cost_model_us,
            execute_and_commit_transactions_output,
        }: &ProcessTransactionBatchOutput,
    ) {
        self.count_metrics.cost_model_throttled_transactions_count +=
            *cost_model_throttled_transactions_count;
        self.timing_metrics.cost_model_us += *cost_model_us;
        self.update_on_execute_and_commit_transactions_output(
            execute_and_commit_transactions_output,
        );
    }

    fn update_on_execute_and_commit_transactions_output(
        &mut self,
        ExecuteAndCommitTransactionsOutput {
            transaction_counts,
            retryable_transaction_indexes,
            execute_and_commit_timings,
            error_counters,
            min_prioritization_fees,
            max_prioritization_fees,
            ..
        }: &ExecuteAndCommitTransactionsOutput,
    ) {
        self.count_metrics.transactions_attempted_processing_count +=
            transaction_counts.attempted_processing_count;
        self.count_metrics.processed_transactions_count += transaction_counts.processed_count;
        self.count_metrics.processed_with_successful_result_count +=
            transaction_counts.processed_with_successful_result_count;
        self.count_metrics.retryable_transaction_count += retryable_transaction_indexes.len();
        self.count_metrics.min_prioritization_fees = self
            .count_metrics
            .min_prioritization_fees
            .min(*min_prioritization_fees);
        self.count_metrics.max_prioritization_fees = self
            .count_metrics
            .max_prioritization_fees
            .max(*max_prioritization_fees);
        self.update_on_execute_and_commit_timings(execute_and_commit_timings);
        self.update_on_error_counters(error_counters);
    }

    fn update_on_execute_and_commit_timings(
        &mut self,
        LeaderExecuteAndCommitTimings {
            collect_balances_us,
            load_execute_us,
            freeze_lock_us,
            record_us,
            commit_us,
            find_and_send_votes_us,
            ..
        }: &LeaderExecuteAndCommitTimings,
    ) {
        self.timing_metrics.collect_balances_us += *collect_balances_us;
        self.timing_metrics.load_execute_us_min += *load_execute_us;
        self.timing_metrics.load_execute_us_max += *load_execute_us;
        self.timing_metrics.load_execute_us += *load_execute_us;
        self.timing_metrics.freeze_lock_us += *freeze_lock_us;
        self.timing_metrics.record_us += *record_us;
        self.timing_metrics.commit_us += *commit_us;
        self.timing_metrics.find_and_send_votes_us += *find_and_send_votes_us;
        self.timing_metrics.num_batches_processed += 1;
    }

    fn update_on_error_counters(
        &mut self,
        TransactionErrorMetrics {
            total,
            account_in_use,
            too_many_account_locks,
            account_loaded_twice,
            account_not_found,
            blockhash_not_found,
            blockhash_too_old,
            call_chain_too_deep,
            already_processed,
            instruction_error,
            insufficient_funds,
            invalid_account_for_fee,
            invalid_account_index,
            invalid_program_for_execution,
            invalid_compute_budget,
            not_allowed_during_cluster_maintenance,
            invalid_writable_account,
            invalid_rent_paying_account,
            would_exceed_max_block_cost_limit,
            would_exceed_max_account_cost_limit,
            would_exceed_max_vote_cost_limit,
            would_exceed_account_data_block_limit,
            max_loaded_accounts_data_size_exceeded,
            program_execution_temporarily_restricted,
        }: &TransactionErrorMetrics,
    ) {
        self.error_metrics.total += total.0;
        self.error_metrics.account_in_use += account_in_use.0;
        self.error_metrics.too_many_account_locks += too_many_account_locks.0;
        self.error_metrics.account_loaded_twice += account_loaded_twice.0;
        self.error_metrics.account_not_found += account_not_found.0;
        self.error_metrics.blockhash_not_found += blockhash_not_found.0;
        self.error_metrics.blockhash_too_old += blockhash_too_old.0;
        self.error_metrics.call_chain_too_deep += call_chain_too_deep.0;
        self.error_metrics.already_processed += already_processed.0;
        self.error_metrics.instruction_error += instruction_error.0;
        self.error_metrics.insufficient_funds += insufficient_funds.0;
        self.error_metrics.invalid_account_for_fee += invalid_account_for_fee.0;
        self.error_metrics.invalid_account_index += invalid_account_index.0;
        self.error_metrics.invalid_program_for_execution += invalid_program_for_execution.0;
        self.error_metrics.invalid_compute_budget += invalid_compute_budget.0;
        self.error_metrics.not_allowed_during_cluster_maintenance +=
            not_allowed_during_cluster_maintenance.0;
        self.error_metrics.invalid_writable_account += invalid_writable_account.0;
        self.error_metrics.invalid_rent_paying_account += invalid_rent_paying_account.0;
        self.error_metrics.would_exceed_max_block_cost_limit += would_exceed_max_block_cost_limit.0;
        self.error_metrics.would_exceed_max_account_cost_limit +=
            would_exceed_max_account_cost_limit.0;
        self.error_metrics.would_exceed_max_vote_cost_limit += would_exceed_max_vote_cost_limit.0;
        self.error_metrics.would_exceed_account_data_block_limit +=
            would_exceed_account_data_block_limit.0;
        self.error_metrics.max_loaded_accounts_data_size_exceeded +=
            max_loaded_accounts_data_size_exceeded.0;
        self.error_metrics.program_execution_temporarily_restricted +=
            program_execution_temporarily_restricted.0;
    }
}

struct ConsumeWorkerCountMetrics {
    transactions_attempted_processing_count: u64,
    processed_transactions_count: u64,
    processed_with_successful_result_count: u64,
    retryable_transaction_count: usize,
    retryable_expired_bank_count: usize,
    cost_model_throttled_transactions_count: u64,
    min_prioritization_fees: u64,
    max_prioritization_fees: u64,
}

impl Default for ConsumeWorkerCountMetrics {
    fn default() -> Self {
        Self {
            transactions_attempted_processing_count: 0,
            processed_transactions_count: 0,
            processed_with_successful_result_count: 0,
            retryable_transaction_count: 0,
            retryable_expired_bank_count: 0,
            cost_model_throttled_transactions_count: 0,
            min_prioritization_fees: u64::MAX,
            max_prioritization_fees: 0,
        }
    }
}

impl ConsumeWorkerCountMetrics {
    fn report_and_reset(&mut self, id: &str) {
        datapoint_info!(
            "banking_stage_worker_counts",
            "id" => id,
            (
                "transactions_attempted_processing_count",
                self.transactions_attempted_processing_count,
                i64
            ),
            (
                "processed_transactions_count",
                self.processed_transactions_count,
                i64
            ),
            (
                "processed_with_successful_result_count",
                self.processed_with_successful_result_count,
                i64
            ),
            (
                "retryable_transaction_count",
                self.retryable_transaction_count,
                i64
            ),
            (
                "retryable_expired_bank_count",
                self.retryable_expired_bank_count,
                i64
            ),
            (
                "cost_model_throttled_transactions_count",
                self.cost_model_throttled_transactions_count,
                i64
            ),
            (
                "min_prioritization_fees",
                self.min_prioritization_fees,
                i64
            ),
            (
                "max_prioritization_fees",
                self.max_prioritization_fees,
                i64
            ),
        );
        *self = Self::default();
    }
}

#[derive(Default)]
struct ConsumeWorkerTimingMetrics {
    cost_model_us: u64,
    collect_balances_us: u64,
    load_execute_us: u64,
    load_execute_us_min: u64,
    load_execute_us_max: u64,
    freeze_lock_us: u64,
    record_us: u64,
    commit_us: u64,
    find_and_send_votes_us: u64,
    wait_for_bank_success_us: u64,
    wait_for_bank_failure_us: u64,
    num_batches_processed: u64,
}

impl ConsumeWorkerTimingMetrics {
    fn report_and_reset(&mut self, id: &str) {
        datapoint_info!(
            "banking_stage_worker_timing",
            "id" => id,
            (
                "cost_model_us",
                self.cost_model_us,
                i64
            ),
            (
                "collect_balances_us",
                self.collect_balances_us,
                i64
            ),
            (
                "load_execute_us",
                self.load_execute_us,
                i64
            ),
            (
                "load_execute_us_min",
                self.load_execute_us_min,
                i64
            ),
            (
                "load_execute_us_max",
                self.load_execute_us_max,
                i64
            ),
            (
                "num_batches_processed",
                self.num_batches_processed,
                i64
            ),
            (
                "freeze_lock_us",
                self.freeze_lock_us,
                i64
            ),
            ("record_us", self.record_us, i64),
            ("commit_us", self.commit_us, i64),
            (
                "find_and_send_votes_us",
                self.find_and_send_votes_us,
                i64
            ),
            (
                "wait_for_bank_success_us",
                self.wait_for_bank_success_us,
                i64
            ),
            (
                "wait_for_bank_failure_us",
                self.wait_for_bank_failure_us,
                i64
            ),
        );
        *self = Self::default();
    }
}

#[derive(Default)]
struct ConsumeWorkerTransactionErrorMetrics {
    total: usize,
    account_in_use: usize,
    too_many_account_locks: usize,
    account_loaded_twice: usize,
    account_not_found: usize,
    blockhash_not_found: usize,
    blockhash_too_old: usize,
    call_chain_too_deep: usize,
    already_processed: usize,
    instruction_error: usize,
    insufficient_funds: usize,
    invalid_account_for_fee: usize,
    invalid_account_index: usize,
    invalid_program_for_execution: usize,
    invalid_compute_budget: usize,
    not_allowed_during_cluster_maintenance: usize,
    invalid_writable_account: usize,
    invalid_rent_paying_account: usize,
    would_exceed_max_block_cost_limit: usize,
    would_exceed_max_account_cost_limit: usize,
    would_exceed_max_vote_cost_limit: usize,
    would_exceed_account_data_block_limit: usize,
    max_loaded_accounts_data_size_exceeded: usize,
    program_execution_temporarily_restricted: usize,
}

impl ConsumeWorkerTransactionErrorMetrics {
    fn report_and_reset(&self, id: &str) {
        datapoint_info!(
            "banking_stage_worker_error_metrics",
            "id" => id,
            ("total", self.total, i64),
            (
                "account_in_use",
                self.account_in_use,
                i64
            ),
            (
                "too_many_account_locks",
                self.too_many_account_locks,
                i64
            ),
            (
                "account_loaded_twice",
                self.account_loaded_twice,
                i64
            ),
            (
                "account_not_found",
                self.account_not_found,
                i64
            ),
            (
                "blockhash_not_found",
                self.blockhash_not_found,
                i64
            ),
            (
                "blockhash_too_old",
                self.blockhash_too_old,
                i64
            ),
            (
                "call_chain_too_deep",
                self.call_chain_too_deep,
                i64
            ),
            (
                "already_processed",
                self.already_processed,
                i64
            ),
            (
                "instruction_error",
                self.instruction_error,
                i64
            ),
            (
                "insufficient_funds",
                self.insufficient_funds,
                i64
            ),
            (
                "invalid_account_for_fee",
                self.invalid_account_for_fee,
                i64
            ),
            (
                "invalid_account_index",
                self.invalid_account_index,
                i64
            ),
            (
                "invalid_program_for_execution",
                self.invalid_program_for_execution,
                i64
            ),
            (
                "invalid_compute_budget",
                self.invalid_compute_budget,
                i64
            ),
            (
                "not_allowed_during_cluster_maintenance",
                self.not_allowed_during_cluster_maintenance,
                i64
            ),
            (
                "invalid_writable_account",
                self.invalid_writable_account,
                i64
            ),
            (
                "invalid_rent_paying_account",
                self.invalid_rent_paying_account,
                i64
            ),
            (
                "would_exceed_max_block_cost_limit",
                self.would_exceed_max_block_cost_limit,
                i64
            ),
            (
                "would_exceed_max_account_cost_limit",
                self.would_exceed_max_account_cost_limit,
                i64
            ),
            (
                "would_exceed_max_vote_cost_limit",
                self.would_exceed_max_vote_cost_limit,
                i64
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::banking_stage::{
            committer::Committer,
            qos_service::QosService,
            scheduler_messages::{MaxAge, TransactionBatchId},
            tests::{create_slow_genesis_config, sanitize_transactions, simulate_poh},
        },
        crossbeam_channel::unbounded,
        solana_ledger::{
            blockstore::Blockstore, genesis_utils::GenesisConfigInfo,
            get_tmp_ledger_path_auto_delete, leader_schedule_cache::LeaderScheduleCache,
        },
        solana_poh::poh_recorder::{PohRecorder, WorkingBankEntry},
        solana_runtime::{
            bank_forks::BankForks, prioritization_fee_cache::PrioritizationFeeCache,
            vote_sender_types::ReplayVoteReceiver,
        },
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_sdk::{
            address_lookup_table::AddressLookupTableAccount,
            clock::{Slot, MAX_PROCESSING_AGE},
            genesis_config::GenesisConfig,
            message::{
                v0::{self, LoadedAddresses},
                SimpleAddressLoader, VersionedMessage,
            },
            poh_config::PohConfig,
            pubkey::Pubkey,
            signature::Keypair,
            signer::Signer,
            system_instruction, system_transaction,
            transaction::{
                MessageHash, SanitizedTransaction, TransactionError, VersionedTransaction,
            },
        },
        solana_svm_transaction::svm_message::SVMMessage,
        std::{
            collections::HashSet,
            sync::{atomic::AtomicBool, RwLock},
            thread::JoinHandle,
        },
        tempfile::TempDir,
    };

    // Helper struct to create tests that hold channels, files, etc.
    // such that our tests can be more easily set up and run.
    struct TestFrame {
        mint_keypair: Keypair,
        genesis_config: GenesisConfig,
        bank: Arc<Bank>,
        _bank_forks: Arc<RwLock<BankForks>>,
        _ledger_path: TempDir,
        _entry_receiver: Receiver<WorkingBankEntry>,
        poh_recorder: Arc<RwLock<PohRecorder>>,
        _poh_simulator: JoinHandle<()>,
        _replay_vote_receiver: ReplayVoteReceiver,

        consume_sender: Sender<ConsumeWork<RuntimeTransaction<SanitizedTransaction>>>,
        consumed_receiver: Receiver<FinishedConsumeWork<RuntimeTransaction<SanitizedTransaction>>>,
    }

    fn setup_test_frame() -> (
        TestFrame,
        ConsumeWorker<RuntimeTransaction<SanitizedTransaction>>,
    ) {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(10_000);
        let (bank, bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
        // Warp to next epoch for MaxAge tests.
        let bank = Arc::new(Bank::new_from_parent(
            bank.clone(),
            &Pubkey::new_unique(),
            bank.get_epoch_info().slots_in_epoch,
        ));

        let ledger_path = get_tmp_ledger_path_auto_delete!();
        let blockstore = Blockstore::open(ledger_path.path())
            .expect("Expected to be able to open database ledger");
        let (poh_recorder, entry_receiver, record_receiver) = PohRecorder::new(
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
        let recorder = poh_recorder.new_recorder();
        let poh_recorder = Arc::new(RwLock::new(poh_recorder));
        let poh_simulator = simulate_poh(record_receiver, &poh_recorder);

        let (replay_vote_sender, replay_vote_receiver) = unbounded();
        let committer = Committer::new(
            None,
            replay_vote_sender,
            Arc::new(PrioritizationFeeCache::new(0u64)),
        );
        let consumer = Consumer::new(committer, recorder, QosService::new(1), None);

        let (consume_sender, consume_receiver) = unbounded();
        let (consumed_sender, consumed_receiver) = unbounded();
        let worker = ConsumeWorker::new(
            0,
            consume_receiver,
            consumer,
            consumed_sender,
            poh_recorder.read().unwrap().new_leader_bank_notifier(),
        );

        (
            TestFrame {
                mint_keypair,
                genesis_config,
                bank,
                _bank_forks: bank_forks,
                _ledger_path: ledger_path,
                _entry_receiver: entry_receiver,
                poh_recorder,
                _poh_simulator: poh_simulator,
                _replay_vote_receiver: replay_vote_receiver,
                consume_sender,
                consumed_receiver,
            },
            worker,
        )
    }

    #[test]
    fn test_worker_consume_no_bank() {
        let (test_frame, worker) = setup_test_frame();
        let TestFrame {
            mint_keypair,
            genesis_config,
            bank,
            consume_sender,
            consumed_receiver,
            ..
        } = &test_frame;
        let worker_thread = std::thread::spawn(move || worker.run());

        let pubkey1 = Pubkey::new_unique();

        let transactions = sanitize_transactions(vec![system_transaction::transfer(
            mint_keypair,
            &pubkey1,
            1,
            genesis_config.hash(),
        )]);
        let bid = TransactionBatchId::new(0);
        let id = 0;
        let max_age = MaxAge {
            sanitized_epoch: bank.epoch(),
            alt_invalidation_slot: bank.slot(),
        };
        let work = ConsumeWork {
            batch_id: bid,
            ids: vec![id],
            transactions,
            max_ages: vec![max_age],
        };
        consume_sender.send(work).unwrap();
        let consumed = consumed_receiver.recv().unwrap();
        assert_eq!(consumed.work.batch_id, bid);
        assert_eq!(consumed.work.ids, vec![id]);
        assert_eq!(consumed.work.max_ages, vec![max_age]);
        assert_eq!(consumed.retryable_indexes, vec![0]);

        drop(test_frame);
        let _ = worker_thread.join().unwrap();
    }

    #[test]
    fn test_worker_consume_simple() {
        let (test_frame, worker) = setup_test_frame();
        let TestFrame {
            mint_keypair,
            genesis_config,
            bank,
            poh_recorder,
            consume_sender,
            consumed_receiver,
            ..
        } = &test_frame;
        let worker_thread = std::thread::spawn(move || worker.run());
        poh_recorder
            .write()
            .unwrap()
            .set_bank_for_test(bank.clone());

        let pubkey1 = Pubkey::new_unique();

        let transactions = sanitize_transactions(vec![system_transaction::transfer(
            mint_keypair,
            &pubkey1,
            1,
            genesis_config.hash(),
        )]);
        let bid = TransactionBatchId::new(0);
        let id = 0;
        let max_age = MaxAge {
            sanitized_epoch: bank.epoch(),
            alt_invalidation_slot: bank.slot(),
        };
        let work = ConsumeWork {
            batch_id: bid,
            ids: vec![id],
            transactions,
            max_ages: vec![max_age],
        };
        consume_sender.send(work).unwrap();
        let consumed = consumed_receiver.recv().unwrap();
        assert_eq!(consumed.work.batch_id, bid);
        assert_eq!(consumed.work.ids, vec![id]);
        assert_eq!(consumed.work.max_ages, vec![max_age]);
        assert_eq!(consumed.retryable_indexes, Vec::<usize>::new());

        drop(test_frame);
        let _ = worker_thread.join().unwrap();
    }

    #[test]
    fn test_worker_consume_self_conflicting() {
        let (test_frame, worker) = setup_test_frame();
        let TestFrame {
            mint_keypair,
            genesis_config,
            bank,
            poh_recorder,
            consume_sender,
            consumed_receiver,
            ..
        } = &test_frame;
        let worker_thread = std::thread::spawn(move || worker.run());
        poh_recorder
            .write()
            .unwrap()
            .set_bank_for_test(bank.clone());

        let pubkey1 = Pubkey::new_unique();
        let pubkey2 = Pubkey::new_unique();

        let txs = sanitize_transactions(vec![
            system_transaction::transfer(mint_keypair, &pubkey1, 2, genesis_config.hash()),
            system_transaction::transfer(mint_keypair, &pubkey2, 2, genesis_config.hash()),
        ]);

        let bid = TransactionBatchId::new(0);
        let id1 = 1;
        let id2 = 0;
        let max_age = MaxAge {
            sanitized_epoch: bank.epoch(),
            alt_invalidation_slot: bank.slot(),
        };
        consume_sender
            .send(ConsumeWork {
                batch_id: bid,
                ids: vec![id1, id2],
                transactions: txs,
                max_ages: vec![max_age, max_age],
            })
            .unwrap();

        let consumed = consumed_receiver.recv().unwrap();
        assert_eq!(consumed.work.batch_id, bid);
        assert_eq!(consumed.work.ids, vec![id1, id2]);
        assert_eq!(consumed.work.max_ages, vec![max_age, max_age]);
        assert_eq!(consumed.retryable_indexes, vec![1]); // id2 is retryable since lock conflict

        drop(test_frame);
        let _ = worker_thread.join().unwrap();
    }

    #[test]
    fn test_worker_consume_multiple_messages() {
        let (test_frame, worker) = setup_test_frame();
        let TestFrame {
            mint_keypair,
            genesis_config,
            bank,
            poh_recorder,
            consume_sender,
            consumed_receiver,
            ..
        } = &test_frame;
        let worker_thread = std::thread::spawn(move || worker.run());
        poh_recorder
            .write()
            .unwrap()
            .set_bank_for_test(bank.clone());

        let pubkey1 = Pubkey::new_unique();
        let pubkey2 = Pubkey::new_unique();

        let txs1 = sanitize_transactions(vec![system_transaction::transfer(
            mint_keypair,
            &pubkey1,
            2,
            genesis_config.hash(),
        )]);
        let txs2 = sanitize_transactions(vec![system_transaction::transfer(
            mint_keypair,
            &pubkey2,
            2,
            genesis_config.hash(),
        )]);

        let bid1 = TransactionBatchId::new(0);
        let bid2 = TransactionBatchId::new(1);
        let id1 = 1;
        let id2 = 0;
        let max_age = MaxAge {
            sanitized_epoch: bank.epoch(),
            alt_invalidation_slot: bank.slot(),
        };
        consume_sender
            .send(ConsumeWork {
                batch_id: bid1,
                ids: vec![id1],
                transactions: txs1,
                max_ages: vec![max_age],
            })
            .unwrap();

        consume_sender
            .send(ConsumeWork {
                batch_id: bid2,
                ids: vec![id2],
                transactions: txs2,
                max_ages: vec![max_age],
            })
            .unwrap();
        let consumed = consumed_receiver.recv().unwrap();
        assert_eq!(consumed.work.batch_id, bid1);
        assert_eq!(consumed.work.ids, vec![id1]);
        assert_eq!(consumed.work.max_ages, vec![max_age]);
        assert_eq!(consumed.retryable_indexes, Vec::<usize>::new());

        let consumed = consumed_receiver.recv().unwrap();
        assert_eq!(consumed.work.batch_id, bid2);
        assert_eq!(consumed.work.ids, vec![id2]);
        assert_eq!(consumed.work.max_ages, vec![max_age]);
        assert_eq!(consumed.retryable_indexes, Vec::<usize>::new());

        drop(test_frame);
        let _ = worker_thread.join().unwrap();
    }

    #[test]
    fn test_worker_ttl() {
        let (test_frame, worker) = setup_test_frame();
        let TestFrame {
            mint_keypair,
            genesis_config,
            bank,
            poh_recorder,
            consume_sender,
            consumed_receiver,
            ..
        } = &test_frame;
        let worker_thread = std::thread::spawn(move || worker.run());
        poh_recorder
            .write()
            .unwrap()
            .set_bank_for_test(bank.clone());
        assert!(bank.slot() > 0);
        assert!(bank.epoch() > 0);

        // No conflicts between transactions. Test 6 cases.
        // 1. Epoch expiration, before slot => still succeeds due to resanitizing
        // 2. Epoch expiration, on slot => succeeds normally
        // 3. Epoch expiration, after slot => succeeds normally
        // 4. ALT expiration, before slot => fails
        // 5. ALT expiration, on slot => succeeds normally
        // 6. ALT expiration, after slot => succeeds normally
        let simple_transfer = || {
            system_transaction::transfer(
                &Keypair::new(),
                &Pubkey::new_unique(),
                1,
                genesis_config.hash(),
            )
        };
        let simple_v0_transfer = || {
            let payer = Keypair::new();
            let to_pubkey = Pubkey::new_unique();
            let loaded_addresses = LoadedAddresses {
                writable: vec![to_pubkey],
                readonly: vec![],
            };
            let loader = SimpleAddressLoader::Enabled(loaded_addresses);
            RuntimeTransaction::try_create(
                VersionedTransaction::try_new(
                    VersionedMessage::V0(
                        v0::Message::try_compile(
                            &payer.pubkey(),
                            &[system_instruction::transfer(&payer.pubkey(), &to_pubkey, 1)],
                            &[AddressLookupTableAccount {
                                key: Pubkey::new_unique(), // will fail if using **bank** to lookup
                                addresses: vec![to_pubkey],
                            }],
                            genesis_config.hash(),
                        )
                        .unwrap(),
                    ),
                    &[&payer],
                )
                .unwrap(),
                MessageHash::Compute,
                None,
                loader,
                &HashSet::default(),
            )
            .unwrap()
        };

        let mut txs = sanitize_transactions(vec![
            simple_transfer(),
            simple_transfer(),
            simple_transfer(),
        ]);
        txs.push(simple_v0_transfer());
        txs.push(simple_v0_transfer());
        txs.push(simple_v0_transfer());
        let sanitized_txs = txs.clone();

        // Fund the keypairs.
        for tx in &txs {
            bank.process_transaction(&system_transaction::transfer(
                mint_keypair,
                &tx.account_keys()[0],
                2,
                genesis_config.hash(),
            ))
            .unwrap();
        }

        consume_sender
            .send(ConsumeWork {
                batch_id: TransactionBatchId::new(1),
                ids: vec![0, 1, 2, 3, 4, 5],
                transactions: txs,
                max_ages: vec![
                    MaxAge {
                        sanitized_epoch: bank.epoch() - 1,
                        alt_invalidation_slot: Slot::MAX,
                    },
                    MaxAge {
                        sanitized_epoch: bank.epoch(),
                        alt_invalidation_slot: Slot::MAX,
                    },
                    MaxAge {
                        sanitized_epoch: bank.epoch() + 1,
                        alt_invalidation_slot: Slot::MAX,
                    },
                    MaxAge {
                        sanitized_epoch: bank.epoch(),
                        alt_invalidation_slot: bank.slot() - 1,
                    },
                    MaxAge {
                        sanitized_epoch: bank.epoch(),
                        alt_invalidation_slot: bank.slot(),
                    },
                    MaxAge {
                        sanitized_epoch: bank.epoch(),
                        alt_invalidation_slot: bank.slot() + 1,
                    },
                ],
            })
            .unwrap();

        let consumed = consumed_receiver.recv().unwrap();
        assert_eq!(consumed.retryable_indexes, Vec::<usize>::new());
        // all but one succeed. 6 for initial funding
        assert_eq!(bank.transaction_count(), 6 + 5);

        let already_processed_results = bank
            .check_transactions(
                &sanitized_txs,
                &vec![Ok(()); sanitized_txs.len()],
                MAX_PROCESSING_AGE,
                &mut TransactionErrorMetrics::default(),
            )
            .into_iter()
            .map(|r| match r {
                Ok(_) => Ok(()),
                Err(err) => Err(err),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            already_processed_results,
            vec![
                Err(TransactionError::AlreadyProcessed),
                Err(TransactionError::AlreadyProcessed),
                Err(TransactionError::AlreadyProcessed),
                Ok(()), // <--- this transaction was not processed
                Err(TransactionError::AlreadyProcessed),
                Err(TransactionError::AlreadyProcessed)
            ]
        );

        drop(test_frame);
        let _ = worker_thread.join().unwrap();
    }
}
