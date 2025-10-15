#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    super::{
        scheduler::{PreLockFilterAction, Scheduler, SchedulingSummary},
        scheduler_common::{
            SchedulingCommon, TransactionSchedulingError, TransactionSchedulingInfo,
        },
        scheduler_error::SchedulerError,
    },
    crate::banking_stage::{
        consumer::TARGET_NUM_TRANSACTIONS_PER_BATCH,
        read_write_account_set::ReadWriteAccountSet,
        scheduler_messages::{ConsumeWork, FinishedConsumeWork},
        transaction_scheduler::{
            scheduler_common::select_thread, transaction_priority_id::TransactionPriorityId,
            transaction_state::TransactionState, transaction_state_container::StateContainer,
        },
    },
    agave_scheduling_utils::thread_aware_account_locks::{
        ThreadAwareAccountLocks, ThreadId, ThreadSet, TryLockError,
    },
    crossbeam_channel::{Receiver, Sender},
    prio_graph::{AccessKind, GraphNode, PrioGraph},
    solana_cost_model::block_cost_limits::MAX_BLOCK_UNITS,
    solana_measure::measure_us,
    solana_pubkey::Pubkey,
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    solana_svm_transaction::svm_message::SVMMessage,
    std::num::Saturating,
};

#[inline(always)]
fn passthrough_priority(
    id: &TransactionPriorityId,
    _graph_node: &GraphNode<TransactionPriorityId>,
) -> TransactionPriorityId {
    *id
}

type SchedulerPrioGraph = PrioGraph<
    TransactionPriorityId,
    Pubkey,
    TransactionPriorityId,
    fn(&TransactionPriorityId, &GraphNode<TransactionPriorityId>) -> TransactionPriorityId,
>;

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) struct PrioGraphSchedulerConfig {
    pub max_scheduled_cus: u64,
    pub max_scanned_transactions_per_scheduling_pass: usize,
    pub look_ahead_window_size: usize,
    pub target_transactions_per_batch: usize,
}

impl Default for PrioGraphSchedulerConfig {
    fn default() -> Self {
        Self {
            max_scheduled_cus: MAX_BLOCK_UNITS,
            max_scanned_transactions_per_scheduling_pass: 1000,
            look_ahead_window_size: 256,
            target_transactions_per_batch: TARGET_NUM_TRANSACTIONS_PER_BATCH,
        }
    }
}

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) struct PrioGraphScheduler<Tx> {
    common: SchedulingCommon<Tx>,
    prio_graph: SchedulerPrioGraph,
    config: PrioGraphSchedulerConfig,
}

impl<Tx: TransactionWithMeta> PrioGraphScheduler<Tx> {
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn new(
        consume_work_senders: Vec<Sender<ConsumeWork<Tx>>>,
        finished_consume_work_receiver: Receiver<FinishedConsumeWork<Tx>>,
        config: PrioGraphSchedulerConfig,
    ) -> Self {
        Self {
            common: SchedulingCommon::new(
                consume_work_senders,
                finished_consume_work_receiver,
                config.target_transactions_per_batch,
            ),
            prio_graph: PrioGraph::new(passthrough_priority),
            config,
        }
    }
}

impl<Tx: TransactionWithMeta> Scheduler<Tx> for PrioGraphScheduler<Tx> {
    /// Schedule transactions from the given `StateContainer` to be
    /// consumed by the worker threads. Returns summary of scheduling, or an
    /// error.
    /// `pre_graph_filter` is used to filter out transactions that should be
    /// skipped and dropped before insertion to the prio-graph. This fn should
    /// set `false` for transactions that should be dropped, and `true`
    /// otherwise.
    /// `pre_lock_filter` is used to filter out transactions after they have
    /// made it to the top of the prio-graph, and immediately before locks are
    /// checked and taken. This fn should return `true` for transactions that
    /// should be scheduled, and `false` otherwise.
    ///
    /// Uses a `PrioGraph` to perform look-ahead during the scheduling of transactions.
    /// This, combined with internal tracking of threads' in-flight transactions, allows
    /// for load-balancing while prioritizing scheduling transactions onto threads that will
    /// not cause conflicts in the near future.
    fn schedule<S: StateContainer<Tx>>(
        &mut self,
        container: &mut S,
        budget: u64,
        _relax_intrabatch_account_locks: bool,
        pre_graph_filter: impl Fn(&[&Tx], &mut [bool]),
        pre_lock_filter: impl Fn(&TransactionState<Tx>) -> PreLockFilterAction,
    ) -> Result<SchedulingSummary, SchedulerError> {
        // Subtract any in-flight compute units from the budget.
        let mut budget = budget.saturating_sub(
            self.common
                .in_flight_tracker
                .cus_in_flight_per_thread()
                .iter()
                .sum(),
        );

        let starting_queue_size = container.queue_size();
        let starting_buffer_size = container.buffer_size();

        let num_threads = self.common.consume_work_senders.len();
        let max_cu_per_thread = self.config.max_scheduled_cus / num_threads as u64;

        let mut schedulable_threads = ThreadSet::any(num_threads);
        for thread_id in 0..num_threads {
            if self.common.in_flight_tracker.cus_in_flight_per_thread()[thread_id]
                >= max_cu_per_thread
            {
                schedulable_threads.remove(thread_id);
            }
        }
        if schedulable_threads.is_empty() {
            return Ok(SchedulingSummary {
                starting_queue_size,
                starting_buffer_size,
                ..SchedulingSummary::default()
            });
        }

        // Some transactions may be unschedulable due to multi-thread conflicts.
        // These transactions cannot be scheduled until some conflicting work is completed.
        // However, the scheduler should not allow other transactions that conflict with
        // these transactions to be scheduled before them.
        let mut unschedulable_ids = Vec::new();
        let mut blocking_locks = ReadWriteAccountSet::default();

        // Track metrics on filter.
        let mut num_filtered_out = Saturating::<usize>(0);
        let mut total_filter_time_us = Saturating::<u64>(0);

        let mut window_budget = self.config.look_ahead_window_size;
        let mut chunked_pops = |container: &mut S,
                                prio_graph: &mut PrioGraph<_, _, _, _>,
                                window_budget: &mut usize| {
            while *window_budget > 0 {
                const MAX_FILTER_CHUNK_SIZE: usize = 128;
                let mut filter_array = [true; MAX_FILTER_CHUNK_SIZE];
                let mut ids = Vec::with_capacity(MAX_FILTER_CHUNK_SIZE);
                let mut txs = Vec::with_capacity(MAX_FILTER_CHUNK_SIZE);

                let chunk_size = (*window_budget).min(MAX_FILTER_CHUNK_SIZE);
                for _ in 0..chunk_size {
                    if let Some(id) = container.pop() {
                        ids.push(id);
                    } else {
                        break;
                    }
                }
                *window_budget = window_budget.saturating_sub(chunk_size);

                ids.iter().for_each(|id| {
                    let transaction = container.get_transaction(id.id).unwrap();
                    txs.push(transaction);
                });

                let (_, filter_us) =
                    measure_us!(pre_graph_filter(&txs, &mut filter_array[..chunk_size]));
                total_filter_time_us += filter_us;

                for (id, filter_result) in ids.iter().zip(&filter_array[..chunk_size]) {
                    if *filter_result {
                        let transaction = container.get_transaction(id.id).unwrap();
                        prio_graph.insert_transaction(
                            *id,
                            Self::get_transaction_account_access(transaction),
                        );
                    } else {
                        num_filtered_out += 1;
                        container.remove_by_id(id.id);
                    }
                }

                if ids.len() != chunk_size {
                    break;
                }
            }
        };

        // Create the initial look-ahead window.
        // Check transactions against filter, remove from container if it fails.
        chunked_pops(container, &mut self.prio_graph, &mut window_budget);

        #[cfg(debug_assertions)]
        debug_assert!(
            self.common.batches.is_empty(),
            "batches must start empty for scheduling"
        );
        let mut unblock_this_batch = Vec::with_capacity(
            self.common.consume_work_senders.len() * self.config.target_transactions_per_batch,
        );
        let mut num_scanned: usize = 0;
        let mut num_scheduled = Saturating::<usize>(0);
        let mut num_sent = Saturating::<usize>(0);
        let mut num_unschedulable_conflicts: usize = 0;
        let mut num_unschedulable_threads: usize = 0;
        while budget > 0 && num_scanned < self.config.max_scanned_transactions_per_scheduling_pass {
            // If nothing is in the main-queue of the `PrioGraph` then there's nothing left to schedule.
            if self.prio_graph.is_empty() {
                break;
            }

            while let Some(id) = self.prio_graph.pop() {
                num_scanned += 1;
                unblock_this_batch.push(id);

                // Should always be in the container, during initial testing phase panic.
                // Later, we can replace with a continue in case this does happen.
                let Some(transaction_state) = container.get_mut_transaction_state(id.id) else {
                    panic!("transaction state must exist")
                };

                let maybe_schedule_info = try_schedule_transaction(
                    transaction_state,
                    &pre_lock_filter,
                    &mut blocking_locks,
                    &mut self.common.account_locks,
                    num_threads,
                    |thread_set| {
                        select_thread(
                            thread_set,
                            self.common.batches.total_cus(),
                            self.common.in_flight_tracker.cus_in_flight_per_thread(),
                            self.common.batches.transactions(),
                            self.common.in_flight_tracker.num_in_flight_per_thread(),
                        )
                    },
                );

                match maybe_schedule_info {
                    Err(TransactionSchedulingError::UnschedulableConflicts) => {
                        num_unschedulable_conflicts += 1;
                        unschedulable_ids.push(id);
                    }
                    Err(TransactionSchedulingError::UnschedulableThread) => {
                        num_unschedulable_threads += 1;
                        unschedulable_ids.push(id);
                    }
                    Ok(TransactionSchedulingInfo {
                        thread_id,
                        transaction,
                        max_age,
                        cost,
                    }) => {
                        num_scheduled += 1;
                        self.common.batches.add_transaction_to_batch(
                            thread_id,
                            id.id,
                            transaction,
                            max_age,
                            cost,
                        );
                        budget = budget.saturating_sub(cost);

                        // If target batch size is reached, send only this batch.
                        if self.common.batches.transactions()[thread_id].len()
                            >= self.config.target_transactions_per_batch
                        {
                            num_sent += self.common.send_batch(thread_id)?;
                        }

                        // if the thread is at max_cu_per_thread, remove it from the schedulable threads
                        // if there are no more schedulable threads, stop scheduling.
                        if self.common.in_flight_tracker.cus_in_flight_per_thread()[thread_id]
                            + self.common.batches.total_cus()[thread_id]
                            >= max_cu_per_thread
                        {
                            schedulable_threads.remove(thread_id);
                            if schedulable_threads.is_empty() {
                                break;
                            }
                        }
                    }
                }

                if num_scanned >= self.config.max_scanned_transactions_per_scheduling_pass {
                    break;
                }
            }

            // Send all non-empty batches
            num_sent += self.common.send_batches()?;

            // Refresh window budget and do chunked pops
            window_budget += unblock_this_batch.len();
            chunked_pops(container, &mut self.prio_graph, &mut window_budget);

            // Unblock all transactions that were blocked by the transactions that were just sent.
            for id in unblock_this_batch.drain(..) {
                self.prio_graph.unblock(&id);
            }
        }

        // Send batches for any remaining transactions
        num_sent += self.common.send_batches()?;

        // Push unschedulable ids back into the container
        container.push_ids_into_queue(unschedulable_ids.into_iter());

        // Push remaining transactions back into the container
        container.push_ids_into_queue(std::iter::from_fn(|| {
            self.prio_graph.pop_and_unblock().map(|(id, _)| id)
        }));

        // No more remaining items in the queue.
        // Clear here to make sure the next scheduling pass starts fresh
        // without detecting any conflicts.
        self.prio_graph.clear();

        assert_eq!(
            num_scheduled, num_sent,
            "number of scheduled and sent transactions must match"
        );

        let Saturating(num_scheduled) = num_scheduled;
        let Saturating(num_filtered_out) = num_filtered_out;
        let Saturating(total_filter_time_us) = total_filter_time_us;

        Ok(SchedulingSummary {
            starting_queue_size,
            starting_buffer_size,
            num_scheduled,
            num_unschedulable_conflicts,
            num_unschedulable_threads,
            num_filtered_out,
            filter_time_us: total_filter_time_us,
        })
    }

    fn scheduling_common_mut(&mut self) -> &mut SchedulingCommon<Tx> {
        &mut self.common
    }
}

impl<Tx: TransactionWithMeta> PrioGraphScheduler<Tx> {
    /// Gets accessed accounts (resources) for use in `PrioGraph`.
    fn get_transaction_account_access(
        message: &impl SVMMessage,
    ) -> impl Iterator<Item = (Pubkey, AccessKind)> + '_ {
        message
            .account_keys()
            .iter()
            .enumerate()
            .map(|(index, key)| {
                if message.is_writable(index) {
                    (*key, AccessKind::Write)
                } else {
                    (*key, AccessKind::Read)
                }
            })
    }
}

fn try_schedule_transaction<Tx: TransactionWithMeta>(
    transaction_state: &mut TransactionState<Tx>,
    pre_lock_filter: impl Fn(&TransactionState<Tx>) -> PreLockFilterAction,
    blocking_locks: &mut ReadWriteAccountSet,
    account_locks: &mut ThreadAwareAccountLocks,
    num_threads: usize,
    thread_selector: impl Fn(ThreadSet) -> ThreadId,
) -> Result<TransactionSchedulingInfo<Tx>, TransactionSchedulingError> {
    match pre_lock_filter(transaction_state) {
        PreLockFilterAction::AttemptToSchedule => {}
    }

    // Check if this transaction conflicts with any blocked transactions
    let transaction = transaction_state.transaction();
    if !blocking_locks.check_locks(transaction) {
        blocking_locks.take_locks(transaction);
        return Err(TransactionSchedulingError::UnschedulableConflicts);
    }

    // Schedule the transaction if it can be.
    let account_keys = transaction.account_keys();
    let write_account_locks = account_keys
        .iter()
        .enumerate()
        .filter_map(|(index, key)| transaction.is_writable(index).then_some(key));
    let read_account_locks = account_keys
        .iter()
        .enumerate()
        .filter_map(|(index, key)| (!transaction.is_writable(index)).then_some(key));

    let thread_id = match account_locks.try_lock_accounts(
        write_account_locks,
        read_account_locks,
        ThreadSet::any(num_threads),
        thread_selector,
    ) {
        Ok(thread_id) => thread_id,
        Err(TryLockError::MultipleConflicts) => {
            blocking_locks.take_locks(transaction);
            return Err(TransactionSchedulingError::UnschedulableConflicts);
        }
        Err(TryLockError::ThreadNotAllowed) => {
            blocking_locks.take_locks(transaction);
            return Err(TransactionSchedulingError::UnschedulableThread);
        }
    };

    let (transaction, max_age) = transaction_state.take_transaction_for_scheduling();
    let cost = transaction_state.cost();

    Ok(TransactionSchedulingInfo {
        thread_id,
        transaction,
        max_age,
        cost,
    })
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::banking_stage::{
            scheduler_messages::{MaxAge, TransactionId},
            transaction_scheduler::transaction_state_container::TransactionStateContainer,
        },
        crossbeam_channel::{unbounded, Receiver},
        itertools::Itertools,
        solana_compute_budget_interface::ComputeBudgetInstruction,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_message::Message,
        solana_pubkey::Pubkey,
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_signer::Signer,
        solana_system_interface::instruction as system_instruction,
        solana_transaction::{sanitized::SanitizedTransaction, Transaction},
        std::borrow::Borrow,
    };

    #[allow(clippy::type_complexity)]
    fn create_test_frame(
        num_threads: usize,
    ) -> (
        PrioGraphScheduler<RuntimeTransaction<SanitizedTransaction>>,
        Vec<Receiver<ConsumeWork<RuntimeTransaction<SanitizedTransaction>>>>,
        Sender<FinishedConsumeWork<RuntimeTransaction<SanitizedTransaction>>>,
    ) {
        let (consume_work_senders, consume_work_receivers) =
            (0..num_threads).map(|_| unbounded()).unzip();
        let (finished_consume_work_sender, finished_consume_work_receiver) = unbounded();
        let scheduler = PrioGraphScheduler::new(
            consume_work_senders,
            finished_consume_work_receiver,
            PrioGraphSchedulerConfig::default(),
        );
        (
            scheduler,
            consume_work_receivers,
            finished_consume_work_sender,
        )
    }

    fn prioritized_tranfers(
        from_keypair: &Keypair,
        to_pubkeys: impl IntoIterator<Item = impl Borrow<Pubkey>>,
        lamports: u64,
        priority: u64,
    ) -> RuntimeTransaction<SanitizedTransaction> {
        let to_pubkeys_lamports = to_pubkeys
            .into_iter()
            .map(|pubkey| *pubkey.borrow())
            .zip(std::iter::repeat(lamports))
            .collect_vec();
        let mut ixs =
            system_instruction::transfer_many(&from_keypair.pubkey(), &to_pubkeys_lamports);
        let prioritization = ComputeBudgetInstruction::set_compute_unit_price(priority);
        ixs.push(prioritization);
        let message = Message::new(&ixs, Some(&from_keypair.pubkey()));
        let tx = Transaction::new(&[from_keypair], message, Hash::default());
        RuntimeTransaction::from_transaction_for_tests(tx)
    }

    fn create_container(
        tx_infos: impl IntoIterator<
            Item = (
                impl Borrow<Keypair>,
                impl IntoIterator<Item = impl Borrow<Pubkey>>,
                u64,
                u64,
            ),
        >,
    ) -> TransactionStateContainer<RuntimeTransaction<SanitizedTransaction>> {
        create_container_with_capacity(100 * 1024, tx_infos)
    }

    fn create_container_with_capacity(
        capacity: usize,
        tx_infos: impl IntoIterator<
            Item = (
                impl Borrow<Keypair>,
                impl IntoIterator<Item = impl Borrow<Pubkey>>,
                u64,
                u64,
            ),
        >,
    ) -> TransactionStateContainer<RuntimeTransaction<SanitizedTransaction>> {
        let mut container = TransactionStateContainer::with_capacity(capacity);
        for (from_keypair, to_pubkeys, lamports, compute_unit_price) in tx_infos.into_iter() {
            let transaction = prioritized_tranfers(
                from_keypair.borrow(),
                to_pubkeys,
                lamports,
                compute_unit_price,
            );

            const TEST_TRANSACTION_COST: u64 = 5000;
            container.insert_new_transaction(
                transaction,
                MaxAge::MAX,
                compute_unit_price,
                TEST_TRANSACTION_COST,
            );
        }

        container
    }

    fn collect_work(
        receiver: &Receiver<ConsumeWork<RuntimeTransaction<SanitizedTransaction>>>,
    ) -> (
        Vec<ConsumeWork<RuntimeTransaction<SanitizedTransaction>>>,
        Vec<Vec<TransactionId>>,
    ) {
        receiver
            .try_iter()
            .map(|work| {
                let ids = work.ids.clone();
                (work, ids)
            })
            .unzip()
    }

    fn test_pre_graph_filter(
        _txs: &[&RuntimeTransaction<SanitizedTransaction>],
        results: &mut [bool],
    ) {
        results.fill(true);
    }

    fn test_pre_lock_filter(
        _tx: &TransactionState<RuntimeTransaction<SanitizedTransaction>>,
    ) -> PreLockFilterAction {
        PreLockFilterAction::AttemptToSchedule
    }

    #[test]
    fn test_schedule_disconnected_channel() {
        let (mut scheduler, work_receivers, _finished_work_sender) = create_test_frame(1);
        let mut container = create_container([(&Keypair::new(), &[Pubkey::new_unique()], 1, 1)]);

        drop(work_receivers); // explicitly drop receivers
        assert_matches!(
            scheduler.schedule(
                &mut container,
                u64::MAX, // no budget
                false,
                test_pre_graph_filter,
                test_pre_lock_filter
            ),
            Err(SchedulerError::DisconnectedSendChannel(_))
        );
    }

    #[test]
    fn test_schedule_single_threaded_no_conflicts() {
        let (mut scheduler, work_receivers, _finished_work_sender) = create_test_frame(1);
        let mut container = create_container([
            (&Keypair::new(), &[Pubkey::new_unique()], 1, 1),
            (&Keypair::new(), &[Pubkey::new_unique()], 2, 2),
        ]);

        let scheduling_summary = scheduler
            .schedule(
                &mut container,
                u64::MAX, // no budget
                false,
                test_pre_graph_filter,
                test_pre_lock_filter,
            )
            .unwrap();
        assert_eq!(scheduling_summary.num_scheduled, 2);
        assert_eq!(scheduling_summary.num_unschedulable_conflicts, 0);
        assert_eq!(collect_work(&work_receivers[0]).1, vec![vec![1, 0]]);
    }

    #[test]
    fn test_schedule_budget() {
        let (mut scheduler, _work_receivers, _finished_work_sender) = create_test_frame(1);
        let mut container = create_container([
            (&Keypair::new(), &[Pubkey::new_unique()], 1, 1),
            (&Keypair::new(), &[Pubkey::new_unique()], 2, 2),
        ]);

        let scheduling_summary = scheduler
            .schedule(
                &mut container,
                0, // zero budget. nothing should be scheduled
                false,
                test_pre_graph_filter,
                test_pre_lock_filter,
            )
            .unwrap();
        assert_eq!(scheduling_summary.num_scheduled, 0);
        assert_eq!(scheduling_summary.num_unschedulable_conflicts, 0);
    }

    #[test]
    fn test_schedule_single_threaded_conflict() {
        let (mut scheduler, work_receivers, _finished_work_sender) = create_test_frame(1);
        let pubkey = Pubkey::new_unique();
        let mut container = create_container([
            (&Keypair::new(), &[pubkey], 1, 1),
            (&Keypair::new(), &[pubkey], 1, 2),
        ]);

        let scheduling_summary = scheduler
            .schedule(
                &mut container,
                u64::MAX, // no budget
                false,
                test_pre_graph_filter,
                test_pre_lock_filter,
            )
            .unwrap();
        assert_eq!(scheduling_summary.num_scheduled, 2);
        assert_eq!(scheduling_summary.num_unschedulable_conflicts, 0);
        assert_eq!(collect_work(&work_receivers[0]).1, vec![vec![1], vec![0]]);
    }

    #[test]
    fn test_schedule_consume_single_threaded_multi_batch() {
        let (mut scheduler, work_receivers, _finished_work_sender) = create_test_frame(1);
        let mut container = create_container(
            (0..4 * TARGET_NUM_TRANSACTIONS_PER_BATCH)
                .map(|i| (Keypair::new(), [Pubkey::new_unique()], i as u64, 1)),
        );

        // expect 4 full batches to be scheduled
        let scheduling_summary = scheduler
            .schedule(
                &mut container,
                u64::MAX, // no budget
                false,
                test_pre_graph_filter,
                test_pre_lock_filter,
            )
            .unwrap();
        assert_eq!(
            scheduling_summary.num_scheduled,
            4 * TARGET_NUM_TRANSACTIONS_PER_BATCH
        );
        assert_eq!(scheduling_summary.num_unschedulable_conflicts, 0);

        let thread0_work_counts: Vec<_> = work_receivers[0]
            .try_iter()
            .map(|work| work.ids.len())
            .collect();
        assert_eq!(thread0_work_counts, [TARGET_NUM_TRANSACTIONS_PER_BATCH; 4]);
    }

    #[test]
    fn test_schedule_simple_thread_selection() {
        let (mut scheduler, work_receivers, _finished_work_sender) = create_test_frame(2);
        let mut container =
            create_container((0..4).map(|i| (Keypair::new(), [Pubkey::new_unique()], 1, i)));

        let scheduling_summary = scheduler
            .schedule(
                &mut container,
                u64::MAX, // no budget
                false,
                test_pre_graph_filter,
                test_pre_lock_filter,
            )
            .unwrap();
        assert_eq!(scheduling_summary.num_scheduled, 4);
        assert_eq!(scheduling_summary.num_unschedulable_conflicts, 0);
        assert_eq!(collect_work(&work_receivers[0]).1, [vec![3, 1]]);
        assert_eq!(collect_work(&work_receivers[1]).1, [vec![2, 0]]);
    }

    #[test]
    fn test_schedule_priority_guard() {
        let (mut scheduler, work_receivers, finished_work_sender) = create_test_frame(2);
        // intentionally shorten the look-ahead window to cause unschedulable conflicts
        scheduler.config.look_ahead_window_size = 2;

        let accounts = (0..8).map(|_| Keypair::new()).collect_vec();
        let mut container = create_container([
            (&accounts[0], &[accounts[1].pubkey()], 1, 6),
            (&accounts[2], &[accounts[3].pubkey()], 1, 5),
            (&accounts[4], &[accounts[5].pubkey()], 1, 4),
            (&accounts[6], &[accounts[7].pubkey()], 1, 3),
            (&accounts[1], &[accounts[2].pubkey()], 1, 2),
            (&accounts[2], &[accounts[3].pubkey()], 1, 1),
        ]);

        // The look-ahead window is intentionally shortened, high priority transactions
        // [0, 1, 2, 3] do not conflict, and are scheduled onto threads in a
        // round-robin fashion. This leads to transaction [4] being unschedulable due
        // to conflicts with [0] and [1], which were scheduled to different threads.
        // Transaction [5] is technically schedulable, onto thread 1 since it only
        // conflicts with transaction [1]. However, [5] will not be scheduled because
        // it conflicts with a higher-priority transaction [4] that is unschedulable.
        // The full prio-graph can be visualized as:
        // [0] \
        //      -> [4] -> [5]
        // [1] / ------/
        // [2]
        // [3]
        // Because the look-ahead window is shortened to a size of 4, the scheduler does
        // not have knowledge of the joining at transaction [4] until after [0] and [1]
        // have been scheduled.
        let scheduling_summary = scheduler
            .schedule(
                &mut container,
                u64::MAX, // no budget
                false,
                test_pre_graph_filter,
                test_pre_lock_filter,
            )
            .unwrap();
        assert_eq!(scheduling_summary.num_scheduled, 4);
        assert_eq!(scheduling_summary.num_unschedulable_conflicts, 2);
        let (thread_0_work, thread_0_ids) = collect_work(&work_receivers[0]);
        assert_eq!(thread_0_ids, [vec![0], vec![2]]);
        assert_eq!(collect_work(&work_receivers[1]).1, [vec![1], vec![3]]);

        // Cannot schedule even on next pass because of lock conflicts
        let scheduling_summary = scheduler
            .schedule(
                &mut container,
                u64::MAX, // no budget
                false,
                test_pre_graph_filter,
                test_pre_lock_filter,
            )
            .unwrap();
        assert_eq!(scheduling_summary.num_scheduled, 0);
        assert_eq!(scheduling_summary.num_unschedulable_conflicts, 2);

        // Complete batch on thread 0. Remaining txs can be scheduled onto thread 1
        finished_work_sender
            .send(FinishedConsumeWork {
                work: thread_0_work.into_iter().next().unwrap(),
                retryable_indexes: vec![],
            })
            .unwrap();
        scheduler.receive_completed(&mut container).unwrap();
        let scheduling_summary = scheduler
            .schedule(
                &mut container,
                u64::MAX, // no budget
                false,
                test_pre_graph_filter,
                test_pre_lock_filter,
            )
            .unwrap();
        assert_eq!(scheduling_summary.num_scheduled, 2);
        assert_eq!(scheduling_summary.num_unschedulable_conflicts, 0);

        assert_eq!(collect_work(&work_receivers[1]).1, [vec![4], vec![5]]);
    }

    #[test]
    fn test_schedule_over_full_container() {
        let (mut scheduler, _work_receivers, _finished_work_sender) = create_test_frame(1);

        // set up a container is larger enough that single pass of scheduling will not deplete it.
        let capacity = scheduler
            .config
            .max_scanned_transactions_per_scheduling_pass
            + 2;
        let txs = (0..capacity)
            .map(|_| (Keypair::new(), [Pubkey::new_unique()], 1, 1))
            .collect_vec();
        let mut container = create_container_with_capacity(capacity, txs);

        let scheduling_summary = scheduler
            .schedule(
                &mut container,
                u64::MAX, // no budget
                false,
                test_pre_graph_filter,
                test_pre_lock_filter,
            )
            .unwrap();
        // for each pass, it'd schedule no more than configured max_scanned_transactions_per_scheduling_pass
        let expected_num_scheduled = std::cmp::min(
            capacity,
            scheduler
                .config
                .max_scanned_transactions_per_scheduling_pass,
        );
        assert_eq!(scheduling_summary.num_scheduled, expected_num_scheduled);
        assert_eq!(scheduling_summary.num_unschedulable_conflicts, 0);

        let mut post_schedule_remaining_ids = 0;
        while let Some(_p) = container.pop() {
            post_schedule_remaining_ids += 1;
        }

        // unscheduled ids should remain in the container
        assert_eq!(
            post_schedule_remaining_ids,
            capacity - expected_num_scheduled
        );
    }
}
