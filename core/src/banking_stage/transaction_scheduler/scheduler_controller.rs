//! Control flow for BankingStage's transaction scheduler.
//!

use {
    super::{
        receive_and_buffer::{DisconnectedError, ReceiveAndBuffer},
        scheduler::{PreLockFilterAction, Scheduler},
        scheduler_error::SchedulerError,
        scheduler_metrics::{SchedulerCountMetrics, SchedulerTimingMetrics, SchedulingDetails},
    },
    crate::{
        banking_stage::{
            consume_worker::ConsumeWorkerMetrics,
            consumer::Consumer,
            decision_maker::{BufferedPacketsDecision, DecisionMaker},
            transaction_scheduler::{
                receive_and_buffer::ReceivingStats, transaction_state_container::StateContainer,
            },
            TOTAL_BUFFERED_PACKETS,
        },
        validator::SchedulerPacing,
    },
    solana_clock::MAX_PROCESSING_AGE,
    solana_cost_model::cost_tracker::SharedBlockCost,
    solana_measure::measure_us,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_svm::transaction_error_metrics::TransactionErrorMetrics,
    std::{
        num::{NonZeroU64, Saturating},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, RwLock,
        },
        time::{Duration, Instant},
    },
};

#[derive(Clone)]
pub struct SchedulerConfig {
    pub scheduler_pacing: SchedulerPacing,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            scheduler_pacing: SchedulerPacing::FillTimeMillis(
                DEFAULT_SCHEDULER_PACING_FILL_TIME_MILLIS,
            ),
        }
    }
}

pub(crate) const DEFAULT_SCHEDULER_PACING_FILL_TIME_MILLIS: NonZeroU64 =
    NonZeroU64::new(350).unwrap();

/// Controls packet and transaction flow into scheduler, and scheduling execution.
pub(crate) struct SchedulerController<R, S>
where
    R: ReceiveAndBuffer,
    S: Scheduler<R::Transaction>,
{
    /// Exit signal for the scheduler thread.
    exit: Arc<AtomicBool>,
    config: SchedulerConfig,
    /// Decision maker for determining what should be done with transactions.
    decision_maker: DecisionMaker,
    receive_and_buffer: R,
    bank_forks: Arc<RwLock<BankForks>>,
    /// Container for transaction state.
    /// Shared resource between `packet_receiver` and `scheduler`.
    container: R::Container,
    /// State for scheduling and communicating with worker threads.
    scheduler: S,
    /// Metrics tracking counts on transactions in different states
    /// over an interval and during a leader slot.
    count_metrics: SchedulerCountMetrics,
    /// Metrics tracking time spent in difference code sections
    /// over an interval and during a leader slot.
    timing_metrics: SchedulerTimingMetrics,
    /// Metric report handles for the worker threads.
    worker_metrics: Vec<Arc<ConsumeWorkerMetrics>>,
    /// Detailed scheduling metrics.
    scheduling_details: SchedulingDetails,
}

impl<R, S> SchedulerController<R, S>
where
    R: ReceiveAndBuffer,
    S: Scheduler<R::Transaction>,
{
    pub fn new(
        exit: Arc<AtomicBool>,
        config: SchedulerConfig,
        decision_maker: DecisionMaker,
        receive_and_buffer: R,
        bank_forks: Arc<RwLock<BankForks>>,
        scheduler: S,
        worker_metrics: Vec<Arc<ConsumeWorkerMetrics>>,
    ) -> Self {
        Self {
            exit,
            config,
            decision_maker,
            receive_and_buffer,
            bank_forks,
            container: R::Container::with_capacity(TOTAL_BUFFERED_PACKETS),
            scheduler,
            count_metrics: SchedulerCountMetrics::default(),
            timing_metrics: SchedulerTimingMetrics::default(),
            worker_metrics,
            scheduling_details: SchedulingDetails::default(),
        }
    }

    pub fn run(mut self) -> Result<(), SchedulerError> {
        let mut most_recent_leader_slot = None;
        let mut cost_pacer = None;

        while !self.exit.load(Ordering::Relaxed) {
            let now = Instant::now();
            // BufferedPacketsDecision is shared with legacy BankingStage, which will forward
            // packets. Initially, not renaming these decision variants but the actions taken
            // are different, since new BankingStage will not forward packets.
            // For `Forward` and `ForwardAndHold`, we want to receive packets but will not
            // forward them to the next leader. In this case, `ForwardAndHold` is
            // indistinguishable from `Hold`.
            //
            // `Forward` will drop packets from the buffer instead of forwarding.
            // During receiving, since packets would be dropped from buffer anyway, we can
            // bypass sanitization and buffering and immediately drop the packets.
            let (decision, decision_time_us) =
                measure_us!(self.decision_maker.make_consume_or_forward_decision());
            self.timing_metrics.update(|timing_metrics| {
                timing_metrics.decision_time_us += decision_time_us;
            });
            let new_leader_slot = decision.bank().map(|b| b.slot());
            self.count_metrics
                .maybe_report_and_reset_slot(new_leader_slot);
            self.timing_metrics
                .maybe_report_and_reset_slot(new_leader_slot);

            if most_recent_leader_slot != new_leader_slot {
                self.container.flush_held_transactions();
                most_recent_leader_slot = new_leader_slot;
                cost_pacer = decision.bank().map(|b| {
                    let cost_tracker = b.read_cost_tracker().unwrap();
                    let block_limit = cost_tracker.get_block_limit();
                    let shared_block_cost = cost_tracker.shared_block_cost();
                    drop(cost_tracker);

                    // If pacing_fill_time is greater than the bank's slot time,
                    // adjust the pacing_fill_time to be the slot time, and warn.
                    let fill_time = self.config.scheduler_pacing.fill_time();
                    if let Some(pacing_fill_time) = fill_time.as_ref() {
                        if pacing_fill_time.as_nanos() > b.ns_per_slot {
                            warn!(
                                "scheduler pacing config pacing_fill_time {:?} is greater than \
                                 the bank's slot time {}, setting to slot time",
                                pacing_fill_time, b.ns_per_slot,
                            );
                            self.config.scheduler_pacing = SchedulerPacing::FillTimeMillis(
                                NonZeroU64::new(b.ns_per_slot as u64 / 1_000_000)
                                    .unwrap_or(NonZeroU64::new(1).unwrap()),
                            );
                        }
                    }

                    CostPacer {
                        block_limit,
                        shared_block_cost,
                        detection_time: now,
                        fill_time,
                    }
                });
            }

            self.receive_completed()?;
            self.process_transactions(&decision, cost_pacer.as_ref(), &now)?;
            if self.receive_and_buffer_packets(&decision).is_err() {
                break;
            }
            // Report metrics only if there is data.
            // Reset intervals when appropriate, regardless of report.
            let should_report = self.count_metrics.interval_has_data();
            let priority_min_max = self.container.get_min_max_priority();
            self.count_metrics.update(|count_metrics| {
                count_metrics.update_priority_stats(priority_min_max);
            });
            self.count_metrics
                .maybe_report_and_reset_interval(should_report);
            self.timing_metrics
                .maybe_report_and_reset_interval(should_report);
            self.worker_metrics
                .iter()
                .for_each(|metrics| metrics.maybe_report_and_reset());
            self.scheduling_details.maybe_report();
        }

        Ok(())
    }

    /// Process packets based on decision.
    fn process_transactions(
        &mut self,
        decision: &BufferedPacketsDecision,
        cost_pacer: Option<&CostPacer>,
        now: &Instant,
    ) -> Result<(), SchedulerError> {
        match decision {
            BufferedPacketsDecision::Consume(bank) => {
                let scheduling_budget = cost_pacer
                    .expect("cost pacer must be set for Consume")
                    .scheduling_budget(now);
                let (scheduling_summary, schedule_time_us) = measure_us!(self.scheduler.schedule(
                    &mut self.container,
                    scheduling_budget,
                    bank.feature_set
                        .is_active(&agave_feature_set::relax_intrabatch_account_locks::ID),
                    |txs, results| {
                        Self::pre_graph_filter(txs, results, bank, MAX_PROCESSING_AGE)
                    },
                    |_| PreLockFilterAction::AttemptToSchedule // no pre-lock filter for now
                )?);

                self.count_metrics.update(|count_metrics| {
                    count_metrics.num_scheduled += scheduling_summary.num_scheduled;
                    count_metrics.num_unschedulable_conflicts +=
                        scheduling_summary.num_unschedulable_conflicts;
                    count_metrics.num_unschedulable_threads +=
                        scheduling_summary.num_unschedulable_threads;
                    count_metrics.num_schedule_filtered_out += scheduling_summary.num_filtered_out;
                });

                self.timing_metrics.update(|timing_metrics| {
                    timing_metrics.schedule_filter_time_us += scheduling_summary.filter_time_us;
                    timing_metrics.schedule_time_us += schedule_time_us;
                });
                self.scheduling_details.update(&scheduling_summary);
            }
            BufferedPacketsDecision::Forward => {
                let (_, clear_time_us) = measure_us!(self.clear_container());
                self.timing_metrics.update(|timing_metrics| {
                    timing_metrics.clear_time_us += clear_time_us;
                });
            }
            BufferedPacketsDecision::ForwardAndHold => {
                let (_, clean_time_us) = measure_us!(self.clean_queue());
                self.timing_metrics.update(|timing_metrics| {
                    timing_metrics.clean_time_us += clean_time_us;
                });
            }
            BufferedPacketsDecision::Hold => {}
        }

        Ok(())
    }

    fn pre_graph_filter(
        transactions: &[&R::Transaction],
        results: &mut [bool],
        bank: &Bank,
        max_age: usize,
    ) {
        let lock_results = vec![Ok(()); transactions.len()];
        let mut error_counters = TransactionErrorMetrics::default();
        let check_results = bank.check_transactions::<R::Transaction>(
            transactions,
            &lock_results,
            max_age,
            &mut error_counters,
        );

        for ((check_result, tx), result) in check_results
            .into_iter()
            .zip(transactions)
            .zip(results.iter_mut())
        {
            *result = check_result
                .and_then(|_| Consumer::check_fee_payer_unlocked(bank, *tx, &mut error_counters))
                .is_ok();
        }
    }

    /// Clears the transaction state container.
    /// This only clears pending transactions, and does **not** clear in-flight transactions.
    fn clear_container(&mut self) {
        let mut num_dropped_on_clear = Saturating::<usize>(0);
        while let Some(id) = self.container.pop() {
            self.container.remove_by_id(id.id);
            num_dropped_on_clear += 1;
        }

        self.count_metrics.update(|count_metrics| {
            count_metrics.num_dropped_on_clear += num_dropped_on_clear;
        });
    }

    /// Clean unprocessable transactions from the queue. These will be transactions that are
    /// expired, already processed, or are no longer sanitizable.
    /// This only clears pending transactions, and does **not** clear in-flight transactions.
    fn clean_queue(&mut self) {
        // Clean up any transactions that have already been processed, are too old, or do not have
        // valid nonce accounts.
        const MAX_TRANSACTION_CHECKS: usize = 10_000;
        let mut transaction_ids = Vec::with_capacity(MAX_TRANSACTION_CHECKS);

        while transaction_ids.len() < MAX_TRANSACTION_CHECKS {
            let Some(id) = self.container.pop() else {
                break;
            };
            transaction_ids.push(id);
        }

        let bank = self.bank_forks.read().unwrap().working_bank();

        const CHUNK_SIZE: usize = 128;
        let mut error_counters = TransactionErrorMetrics::default();
        let mut num_dropped_on_clean = Saturating::<usize>(0);
        for chunk in transaction_ids.chunks(CHUNK_SIZE) {
            let lock_results = vec![Ok(()); chunk.len()];
            let sanitized_txs: Vec<_> = chunk
                .iter()
                .map(|id| {
                    self.container
                        .get_transaction(id.id)
                        .expect("transaction must exist")
                })
                .collect();

            let check_results = bank.check_transactions::<R::Transaction>(
                &sanitized_txs,
                &lock_results,
                MAX_PROCESSING_AGE,
                &mut error_counters,
            );

            // Remove errored transactions
            for (result, id) in check_results.iter().zip(chunk.iter()) {
                if result.is_err() {
                    num_dropped_on_clean += 1;
                    self.container.remove_by_id(id.id);
                }
            }

            // Push non-errored transaction into queue.
            self.container.push_ids_into_queue(
                check_results
                    .into_iter()
                    .zip(chunk.iter())
                    .filter(|(r, _)| r.is_ok())
                    .map(|(_, id)| *id),
            );
        }

        self.count_metrics.update(|count_metrics| {
            count_metrics.num_dropped_on_clean += num_dropped_on_clean;
        });
    }

    /// Receives completed transactions from the workers and updates metrics.
    fn receive_completed(&mut self) -> Result<(), SchedulerError> {
        let ((num_transactions, num_retryable), receive_completed_time_us) =
            measure_us!(self.scheduler.receive_completed(&mut self.container)?);

        self.count_metrics.update(|count_metrics| {
            count_metrics.num_finished += num_transactions;
            count_metrics.num_retryable += num_retryable;
        });
        self.timing_metrics.update(|timing_metrics| {
            timing_metrics.receive_completed_time_us += receive_completed_time_us;
        });

        Ok(())
    }

    /// Returns whether the packet receiver is still connected.
    fn receive_and_buffer_packets(
        &mut self,
        decision: &BufferedPacketsDecision,
    ) -> Result<ReceivingStats, DisconnectedError> {
        let receiving_stats = self
            .receive_and_buffer
            .receive_and_buffer_packets(&mut self.container, decision)?;

        self.count_metrics.update(|count_metrics| {
            let ReceivingStats {
                num_received,
                num_dropped_without_parsing: num_dropped_without_buffering,
                num_dropped_on_parsing_and_sanitization,
                num_dropped_on_lock_validation,
                num_dropped_on_compute_budget,
                num_dropped_on_age,
                num_dropped_on_already_processed,
                num_dropped_on_fee_payer,
                num_dropped_on_capacity,
                num_buffered,
                receive_time_us: _,
                buffer_time_us: _,
            } = &receiving_stats;

            count_metrics.num_received += *num_received;
            count_metrics.num_dropped_on_receive += *num_dropped_without_buffering;
            count_metrics.num_dropped_on_parsing_and_sanitization +=
                *num_dropped_on_parsing_and_sanitization;
            count_metrics.num_dropped_on_validate_locks += *num_dropped_on_lock_validation;
            count_metrics.num_dropped_on_receive_compute_budget += *num_dropped_on_compute_budget;
            count_metrics.num_dropped_on_receive_age += *num_dropped_on_age;
            count_metrics.num_dropped_on_receive_already_processed +=
                *num_dropped_on_already_processed;
            count_metrics.num_dropped_on_receive_fee_payer += *num_dropped_on_fee_payer;
            count_metrics.num_dropped_on_capacity += *num_dropped_on_capacity;
            count_metrics.num_buffered += *num_buffered;
        });

        self.timing_metrics.update(|timing_metrics| {
            timing_metrics.receive_time_us += receiving_stats.receive_time_us;
            timing_metrics.buffer_time_us += receiving_stats.buffer_time_us;
        });

        Ok(receiving_stats)
    }
}

struct CostPacer {
    block_limit: u64,
    shared_block_cost: SharedBlockCost,
    detection_time: Instant,
    fill_time: Option<Duration>,
}

impl CostPacer {
    fn scheduling_budget(&self, current_time: &Instant) -> u64 {
        let target = if let Some(fill_time) = &self.fill_time {
            let time_since = current_time.saturating_duration_since(self.detection_time);
            if time_since >= *fill_time {
                self.block_limit
            } else {
                // on millisecond granularity, pace the cost linearly.
                let allocation_per_milli = self.block_limit / fill_time.as_millis() as u64;
                let millis_since_detection = time_since.as_millis() as u64;
                allocation_per_milli * millis_since_detection
            }
        } else {
            self.block_limit
        };

        target.saturating_sub(self.shared_block_cost.load())
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::banking_stage::{
            consumer::{RetryableIndex, TARGET_NUM_TRANSACTIONS_PER_BATCH},
            scheduler_messages::{ConsumeWork, FinishedConsumeWork, TransactionBatchId},
            tests::create_slow_genesis_config,
            transaction_scheduler::prio_graph_scheduler::{
                PrioGraphScheduler, PrioGraphSchedulerConfig,
            },
            TransactionViewReceiveAndBuffer,
        },
        agave_banking_stage_ingress_types::{BankingPacketBatch, BankingPacketReceiver},
        crossbeam_channel::{unbounded, Receiver, Sender},
        itertools::Itertools,
        solana_compute_budget_interface::ComputeBudgetInstruction,
        solana_fee_calculator::FeeRateGovernor,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_ledger::genesis_utils::GenesisConfigInfo,
        solana_message::Message,
        solana_perf::packet::{to_packet_batches, PacketBatch, NUM_PACKETS},
        solana_poh::poh_recorder::{LeaderState, SharedLeaderState},
        solana_pubkey::Pubkey,
        solana_runtime::bank::Bank,
        solana_runtime_transaction::transaction_meta::StaticMeta,
        solana_signer::Signer,
        solana_system_interface::instruction as system_instruction,
        solana_transaction::Transaction,
        std::sync::{Arc, RwLock},
    };

    fn create_channels<T>(num: usize) -> (Vec<Sender<T>>, Vec<Receiver<T>>) {
        (0..num).map(|_| unbounded()).unzip()
    }

    // Helper struct to create tests that hold channels, files, etc.
    // such that our tests can be more easily set up and run.
    struct TestFrame<Tx> {
        bank: Arc<Bank>,
        mint_keypair: Keypair,
        banking_packet_sender: Sender<Arc<Vec<PacketBatch>>>,
        shared_leader_state: SharedLeaderState,
        consume_work_receivers: Vec<Receiver<ConsumeWork<Tx>>>,
        finished_consume_work_sender: Sender<FinishedConsumeWork<Tx>>,
    }

    fn test_create_transaction_view_receive_and_buffer(
        receiver: BankingPacketReceiver,
        bank_forks: Arc<RwLock<BankForks>>,
    ) -> TransactionViewReceiveAndBuffer {
        TransactionViewReceiveAndBuffer {
            receiver,
            sharable_banks: bank_forks.read().unwrap().sharable_banks(),
        }
    }

    #[allow(clippy::type_complexity)]
    fn create_test_frame<R: ReceiveAndBuffer>(
        num_threads: usize,
        create_receive_and_buffer: impl FnOnce(BankingPacketReceiver, Arc<RwLock<BankForks>>) -> R,
    ) -> (
        TestFrame<R::Transaction>,
        SchedulerController<R, PrioGraphScheduler<R::Transaction>>,
    ) {
        let GenesisConfigInfo {
            mut genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(u64::MAX);
        genesis_config.fee_rate_governor = FeeRateGovernor::new(5000, 0);
        let (bank, bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);

        let shared_leader_state = SharedLeaderState::new(0, None, None);

        let decision_maker = DecisionMaker::new(shared_leader_state.clone());

        let (banking_packet_sender, banking_packet_receiver) = unbounded();
        let receive_and_buffer =
            create_receive_and_buffer(banking_packet_receiver, bank_forks.clone());

        let (consume_work_senders, consume_work_receivers) = create_channels(num_threads);
        let (finished_consume_work_sender, finished_consume_work_receiver) = unbounded();

        let test_frame = TestFrame {
            bank,
            mint_keypair,
            shared_leader_state,
            banking_packet_sender,
            consume_work_receivers,
            finished_consume_work_sender,
        };

        let scheduler = PrioGraphScheduler::new(
            consume_work_senders,
            finished_consume_work_receiver,
            PrioGraphSchedulerConfig::default(),
        );
        let exit = Arc::new(AtomicBool::new(false));
        let scheduler_controller = SchedulerController::new(
            exit,
            SchedulerConfig::default(),
            decision_maker,
            receive_and_buffer,
            bank_forks,
            scheduler,
            vec![], // no actual workers with metrics to report, this can be empty
        );

        (test_frame, scheduler_controller)
    }

    fn create_and_fund_prioritized_transfer(
        bank: &Bank,
        mint_keypair: &Keypair,
        from_keypair: &Keypair,
        to_pubkey: &Pubkey,
        lamports: u64,
        compute_unit_price: u64,
        recent_blockhash: Hash,
    ) -> Transaction {
        // Fund the sending key, so that the transaction does not get filtered by the fee-payer check.
        {
            let transfer = solana_system_transaction::transfer(
                mint_keypair,
                &from_keypair.pubkey(),
                500_000, // just some amount that will always be enough
                bank.last_blockhash(),
            );
            bank.process_transaction(&transfer).unwrap();
        }

        let transfer = system_instruction::transfer(&from_keypair.pubkey(), to_pubkey, lamports);
        let prioritization = ComputeBudgetInstruction::set_compute_unit_price(compute_unit_price);
        let message = Message::new(&[transfer, prioritization], Some(&from_keypair.pubkey()));
        Transaction::new(&vec![from_keypair], message, recent_blockhash)
    }

    fn to_banking_packet_batch(txs: &[Transaction]) -> BankingPacketBatch {
        BankingPacketBatch::new(to_packet_batches(txs, NUM_PACKETS))
    }

    // Helper function to let test receive and then schedule packets.
    // The order of operations here is convenient for testing, but does not
    // match the order of operations in the actual scheduler.
    // The actual scheduler will process immediately after the decision,
    // in order to keep the decision as recent as possible for processing.
    // In the tests, the decision will not become stale, so it is more convenient
    // to receive first and then schedule.
    fn test_receive_then_schedule<R: ReceiveAndBuffer>(
        scheduler_controller: &mut SchedulerController<R, impl Scheduler<R::Transaction>>,
    ) {
        let decision = scheduler_controller
            .decision_maker
            .make_consume_or_forward_decision();
        assert!(matches!(decision, BufferedPacketsDecision::Consume(_)));
        assert!(scheduler_controller.receive_completed().is_ok());

        // Time is not a reliable way for deterministic testing.
        // Loop here until no more packets are received, this avoids parallel
        // tests from inconsistently timing out and not receiving
        // from the channel.
        while scheduler_controller
            .receive_and_buffer_packets(&decision)
            .map(|n| n.num_received > 0)
            .unwrap_or_default()
        {}
        let now = Instant::now();
        assert!(scheduler_controller
            .process_transactions(
                &decision,
                Some(&CostPacer {
                    block_limit: u64::MAX,
                    shared_block_cost: SharedBlockCost::new(0),
                    detection_time: now.checked_sub(Duration::from_millis(400)).unwrap(),
                    fill_time: Some(Duration::from_millis(300)),
                }),
                &now
            )
            .is_ok());
    }

    #[test]
    #[should_panic(expected = "batch id 0 is not being tracked")]
    fn test_unexpected_batch_id() {
        let (test_frame, scheduler_controller) =
            create_test_frame(1, test_create_transaction_view_receive_and_buffer);
        let TestFrame {
            finished_consume_work_sender,
            ..
        } = &test_frame;

        finished_consume_work_sender
            .send(FinishedConsumeWork {
                work: ConsumeWork {
                    batch_id: TransactionBatchId::new(0),
                    ids: vec![],
                    transactions: vec![],
                    max_ages: vec![],
                },
                retryable_indexes: vec![],
            })
            .unwrap();

        scheduler_controller.run().unwrap();
    }

    #[test]
    fn test_schedule_consume_single_threaded_no_conflicts() {
        let (mut test_frame, mut scheduler_controller) =
            create_test_frame(1, test_create_transaction_view_receive_and_buffer);
        let TestFrame {
            bank,
            mint_keypair,
            shared_leader_state,
            banking_packet_sender,
            consume_work_receivers,
            ..
        } = &mut test_frame;

        shared_leader_state.store(Arc::new(LeaderState::new(
            Some(bank.clone()),
            bank.tick_height(),
            None,
            None,
        )));

        // Send packet batch to the scheduler - should do nothing until we become the leader.
        let tx1 = create_and_fund_prioritized_transfer(
            bank,
            mint_keypair,
            &Keypair::new(),
            &Pubkey::new_unique(),
            1,
            1000,
            bank.last_blockhash(),
        );
        let tx2 = create_and_fund_prioritized_transfer(
            bank,
            mint_keypair,
            &Keypair::new(),
            &Pubkey::new_unique(),
            1,
            2000,
            bank.last_blockhash(),
        );
        let tx1_hash = tx1.message().hash();
        let tx2_hash = tx2.message().hash();

        let txs = vec![tx1, tx2];
        banking_packet_sender
            .send(to_banking_packet_batch(&txs))
            .unwrap();

        test_receive_then_schedule(&mut scheduler_controller);
        let consume_work = consume_work_receivers[0].try_recv().unwrap();
        assert_eq!(consume_work.ids.len(), 2);
        assert_eq!(consume_work.transactions.len(), 2);
        let message_hashes = consume_work
            .transactions
            .iter()
            .map(|tx| tx.message_hash())
            .collect_vec();
        assert_eq!(message_hashes, vec![&tx2_hash, &tx1_hash]);
    }

    #[test]
    fn test_schedule_consume_single_threaded_conflict() {
        let (mut test_frame, mut scheduler_controller) =
            create_test_frame(1, test_create_transaction_view_receive_and_buffer);
        let TestFrame {
            bank,
            mint_keypair,
            shared_leader_state,
            banking_packet_sender,
            consume_work_receivers,
            ..
        } = &mut test_frame;

        shared_leader_state.store(Arc::new(LeaderState::new(
            Some(bank.clone()),
            bank.tick_height(),
            None,
            None,
        )));

        let pk = Pubkey::new_unique();
        let tx1 = create_and_fund_prioritized_transfer(
            bank,
            mint_keypair,
            &Keypair::new(),
            &pk,
            1,
            1000,
            bank.last_blockhash(),
        );
        let tx2 = create_and_fund_prioritized_transfer(
            bank,
            mint_keypair,
            &Keypair::new(),
            &pk,
            1,
            2000,
            bank.last_blockhash(),
        );
        let tx1_hash = tx1.message().hash();
        let tx2_hash = tx2.message().hash();

        let txs = vec![tx1, tx2];
        banking_packet_sender
            .send(to_banking_packet_batch(&txs))
            .unwrap();

        // We expect 2 batches to be scheduled
        test_receive_then_schedule(&mut scheduler_controller);
        let consume_works = (0..2)
            .map(|_| consume_work_receivers[0].try_recv().unwrap())
            .collect_vec();

        let num_txs_per_batch = consume_works.iter().map(|cw| cw.ids.len()).collect_vec();
        let message_hashes = consume_works
            .iter()
            .flat_map(|cw| cw.transactions.iter().map(|tx| tx.message_hash()))
            .collect_vec();
        assert_eq!(num_txs_per_batch, vec![1; 2]);
        assert_eq!(message_hashes, vec![&tx2_hash, &tx1_hash]);
    }

    #[test]
    fn test_schedule_consume_single_threaded_multi_batch() {
        let (mut test_frame, mut scheduler_controller) =
            create_test_frame(1, test_create_transaction_view_receive_and_buffer);
        let TestFrame {
            bank,
            mint_keypair,
            shared_leader_state,
            banking_packet_sender,
            consume_work_receivers,
            ..
        } = &mut test_frame;

        shared_leader_state.store(Arc::new(LeaderState::new(
            Some(bank.clone()),
            bank.tick_height(),
            None,
            None,
        )));

        // Send multiple batches - all get scheduled
        let txs1 = (0..2 * TARGET_NUM_TRANSACTIONS_PER_BATCH)
            .map(|i| {
                create_and_fund_prioritized_transfer(
                    bank,
                    mint_keypair,
                    &Keypair::new(),
                    &Pubkey::new_unique(),
                    i as u64,
                    1,
                    bank.last_blockhash(),
                )
            })
            .collect_vec();
        let txs2 = (0..2 * TARGET_NUM_TRANSACTIONS_PER_BATCH)
            .map(|i| {
                create_and_fund_prioritized_transfer(
                    bank,
                    mint_keypair,
                    &Keypair::new(),
                    &Pubkey::new_unique(),
                    i as u64,
                    2,
                    bank.last_blockhash(),
                )
            })
            .collect_vec();

        banking_packet_sender
            .send(to_banking_packet_batch(&txs1))
            .unwrap();
        banking_packet_sender
            .send(to_banking_packet_batch(&txs2))
            .unwrap();

        // We expect 4 batches to be scheduled
        test_receive_then_schedule(&mut scheduler_controller);
        let consume_works = (0..4)
            .map(|_| consume_work_receivers[0].try_recv().unwrap())
            .collect_vec();

        assert_eq!(
            consume_works.iter().map(|cw| cw.ids.len()).collect_vec(),
            vec![TARGET_NUM_TRANSACTIONS_PER_BATCH; 4]
        );
    }

    #[test]
    fn test_schedule_consume_simple_thread_selection() {
        let (mut test_frame, mut scheduler_controller) =
            create_test_frame(2, test_create_transaction_view_receive_and_buffer);
        let TestFrame {
            bank,
            mint_keypair,
            shared_leader_state,
            banking_packet_sender,
            consume_work_receivers,
            ..
        } = &mut test_frame;

        shared_leader_state.store(Arc::new(LeaderState::new(
            Some(bank.clone()),
            bank.tick_height(),
            None,
            None,
        )));

        // Send 4 transactions w/o conflicts. 2 should be scheduled on each thread
        let txs = (0..4)
            .map(|i| {
                create_and_fund_prioritized_transfer(
                    bank,
                    mint_keypair,
                    &Keypair::new(),
                    &Pubkey::new_unique(),
                    1,
                    i * 10,
                    bank.last_blockhash(),
                )
            })
            .collect_vec();
        banking_packet_sender
            .send(to_banking_packet_batch(&txs))
            .unwrap();

        // Priority Expectation:
        // Thread 0: [3, 1]
        // Thread 1: [2, 0]
        let t0_expected = [3, 1]
            .into_iter()
            .map(|i| txs[i].message().hash())
            .collect_vec();
        let t1_expected = [2, 0]
            .into_iter()
            .map(|i| txs[i].message().hash())
            .collect_vec();

        test_receive_then_schedule(&mut scheduler_controller);
        let t0_actual = consume_work_receivers[0]
            .try_recv()
            .unwrap()
            .transactions
            .iter()
            .map(|tx| *tx.message_hash())
            .collect_vec();
        let t1_actual = consume_work_receivers[1]
            .try_recv()
            .unwrap()
            .transactions
            .iter()
            .map(|tx| *tx.message_hash())
            .collect_vec();

        assert_eq!(t0_actual, t0_expected);
        assert_eq!(t1_actual, t1_expected);
    }

    #[test]
    fn test_schedule_consume_retryable() {
        let (mut test_frame, mut scheduler_controller) =
            create_test_frame(1, test_create_transaction_view_receive_and_buffer);
        let TestFrame {
            bank,
            mint_keypair,
            shared_leader_state,
            banking_packet_sender,
            consume_work_receivers,
            finished_consume_work_sender,
            ..
        } = &mut test_frame;

        shared_leader_state.store(Arc::new(LeaderState::new(
            Some(bank.clone()),
            bank.tick_height(),
            None,
            None,
        )));

        // Send packet batch to the scheduler - should do nothing until we become the leader.
        let tx1 = create_and_fund_prioritized_transfer(
            bank,
            mint_keypair,
            &Keypair::new(),
            &Pubkey::new_unique(),
            1,
            1000,
            bank.last_blockhash(),
        );
        let tx2 = create_and_fund_prioritized_transfer(
            bank,
            mint_keypair,
            &Keypair::new(),
            &Pubkey::new_unique(),
            1,
            2000,
            bank.last_blockhash(),
        );
        let tx1_hash = tx1.message().hash();
        let tx2_hash = tx2.message().hash();

        let txs = vec![tx1, tx2];
        banking_packet_sender
            .send(to_banking_packet_batch(&txs))
            .unwrap();

        test_receive_then_schedule(&mut scheduler_controller);
        let consume_work = consume_work_receivers[0].try_recv().unwrap();
        assert_eq!(consume_work.ids.len(), 2);
        assert_eq!(consume_work.transactions.len(), 2);
        let message_hashes = consume_work
            .transactions
            .iter()
            .map(|tx| tx.message_hash())
            .collect_vec();
        assert_eq!(message_hashes, vec![&tx2_hash, &tx1_hash]);

        // Complete the batch - marking the second transaction as retryable
        finished_consume_work_sender
            .send(FinishedConsumeWork {
                work: consume_work,
                retryable_indexes: vec![RetryableIndex::new(1, true)],
            })
            .unwrap();

        // Transaction should be rescheduled
        test_receive_then_schedule(&mut scheduler_controller);
        let consume_work = consume_work_receivers[0].try_recv().unwrap();
        assert_eq!(consume_work.ids.len(), 1);
        assert_eq!(consume_work.transactions.len(), 1);
        let message_hashes = consume_work
            .transactions
            .iter()
            .map(|tx| tx.message_hash())
            .collect_vec();
        assert_eq!(message_hashes, vec![&tx1_hash]);
    }
}
