#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    super::{
        scheduler::{PreLockFilterAction, Scheduler, SchedulingSummary},
        scheduler_common::{
            select_thread, SchedulingCommon, TransactionSchedulingError, TransactionSchedulingInfo,
        },
        scheduler_error::SchedulerError,
        transaction_priority_id::TransactionPriorityId,
        transaction_state::TransactionState,
        transaction_state_container::StateContainer,
    },
    crate::banking_stage::{
        consumer::TARGET_NUM_TRANSACTIONS_PER_BATCH,
        read_write_account_set::ReadWriteAccountSet,
        scheduler_messages::{ConsumeWork, FinishedConsumeWork},
    },
    agave_scheduling_utils::thread_aware_account_locks::{
        ThreadAwareAccountLocks, ThreadId, ThreadSet, TryLockError,
    },
    crossbeam_channel::{Receiver, Sender},
    solana_cost_model::block_cost_limits::MAX_BLOCK_UNITS,
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    std::num::Saturating,
};

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) struct GreedySchedulerConfig {
    pub target_scheduled_cus: u64,
    pub max_scanned_transactions_per_scheduling_pass: usize,
    pub target_transactions_per_batch: usize,
}

impl Default for GreedySchedulerConfig {
    fn default() -> Self {
        Self {
            target_scheduled_cus: MAX_BLOCK_UNITS / 4,
            max_scanned_transactions_per_scheduling_pass: 100_000,
            target_transactions_per_batch: TARGET_NUM_TRANSACTIONS_PER_BATCH,
        }
    }
}

/// Dead-simple scheduler that is efficient and will attempt to schedule
/// in priority order, scheduling anything that can be immediately
/// scheduled, up to the limits.
pub struct GreedyScheduler<Tx: TransactionWithMeta> {
    common: SchedulingCommon<Tx>,
    working_account_set: ReadWriteAccountSet,
    unschedulables: Vec<TransactionPriorityId>,
    config: GreedySchedulerConfig,
}

impl<Tx: TransactionWithMeta> GreedyScheduler<Tx> {
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn new(
        consume_work_senders: Vec<Sender<ConsumeWork<Tx>>>,
        finished_consume_work_receiver: Receiver<FinishedConsumeWork<Tx>>,
        config: GreedySchedulerConfig,
    ) -> Self {
        Self {
            working_account_set: ReadWriteAccountSet::default(),
            unschedulables: Vec::with_capacity(config.max_scanned_transactions_per_scheduling_pass),
            common: SchedulingCommon::new(
                consume_work_senders,
                finished_consume_work_receiver,
                config.target_transactions_per_batch,
            ),
            config,
        }
    }
}

impl<Tx: TransactionWithMeta> Scheduler<Tx> for GreedyScheduler<Tx> {
    fn schedule<S: StateContainer<Tx>>(
        &mut self,
        container: &mut S,
        budget: u64,
        relax_intrabatch_account_locks: bool,
        _pre_graph_filter: impl Fn(&[&Tx], &mut [bool]),
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
        let target_cu_per_thread = self.config.target_scheduled_cus / num_threads as u64;

        let mut schedulable_threads = ThreadSet::any(num_threads);
        for thread_id in 0..num_threads {
            if self.common.in_flight_tracker.cus_in_flight_per_thread()[thread_id]
                >= target_cu_per_thread
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

        #[cfg(debug_assertions)]
        debug_assert!(
            self.common.batches.is_empty(),
            "batches must start empty for scheduling"
        );

        // Track metrics on filter.
        let mut num_scanned: usize = 0;
        let mut num_scheduled = Saturating::<usize>(0);
        let mut num_sent: usize = 0;
        let mut num_unschedulable_conflicts: usize = 0;
        let mut num_unschedulable_threads: usize = 0;

        while budget > 0
            && num_scanned < self.config.max_scanned_transactions_per_scheduling_pass
            && !schedulable_threads.is_empty()
            && !container.is_empty()
        {
            let Some(id) = container.pop() else {
                unreachable!("container is not empty")
            };

            num_scanned += 1;

            // Should always be in the container, during initial testing phase panic.
            // Later, we can replace with a continue in case this does happen.
            let Some(transaction_state) = container.get_mut_transaction_state(id.id) else {
                panic!("transaction state must exist")
            };

            // If there is a conflict with any of the transactions in the current batches,
            // we should immediately send out the batches, so this transaction may be scheduled.
            if !relax_intrabatch_account_locks
                && !self
                    .working_account_set
                    .check_locks(transaction_state.transaction())
            {
                self.working_account_set.clear();
                num_sent += self.common.send_batches()?;
            }

            // Now check if the transaction can actually be scheduled.
            match try_schedule_transaction(
                transaction_state,
                &pre_lock_filter,
                &mut self.common.account_locks,
                schedulable_threads,
                |thread_set| {
                    select_thread(
                        thread_set,
                        self.common.batches.total_cus(),
                        self.common.in_flight_tracker.cus_in_flight_per_thread(),
                        self.common.batches.transactions(),
                        self.common.in_flight_tracker.num_in_flight_per_thread(),
                    )
                },
            ) {
                Err(TransactionSchedulingError::UnschedulableConflicts) => {
                    num_unschedulable_conflicts += 1;
                    self.unschedulables.push(id);
                }
                Err(TransactionSchedulingError::UnschedulableThread) => {
                    num_unschedulable_threads += 1;
                    self.unschedulables.push(id);
                }
                Ok(TransactionSchedulingInfo {
                    thread_id,
                    transaction,
                    max_age,
                    cost,
                }) => {
                    if !relax_intrabatch_account_locks {
                        assert!(
                            self.working_account_set.take_locks(&transaction),
                            "locks must be available"
                        );
                    }
                    num_scheduled += 1;
                    self.common.batches.add_transaction_to_batch(
                        thread_id,
                        id.id,
                        transaction,
                        max_age,
                        cost,
                    );
                    budget = budget.saturating_sub(cost);

                    // If target batch size is reached, send all the batches
                    if self.common.batches.transactions()[thread_id].len()
                        >= self.config.target_transactions_per_batch
                    {
                        self.working_account_set.clear();
                        num_sent += self.common.send_batches()?;
                    }

                    // if the thread is at target_cu_per_thread, remove it from the schedulable threads
                    // if there are no more schedulable threads, stop scheduling.
                    if self.common.in_flight_tracker.cus_in_flight_per_thread()[thread_id]
                        + self.common.batches.total_cus()[thread_id]
                        >= target_cu_per_thread
                    {
                        schedulable_threads.remove(thread_id);
                        if schedulable_threads.is_empty() {
                            break;
                        }
                    }
                }
            }
        }

        self.working_account_set.clear();
        num_sent += self.common.send_batches()?;
        let Saturating(num_scheduled) = num_scheduled;
        assert_eq!(
            num_scheduled, num_sent,
            "number of scheduled and sent transactions must match"
        );

        // Push unschedulables back into the queue
        container.push_ids_into_queue(self.unschedulables.drain(..));

        Ok(SchedulingSummary {
            starting_queue_size,
            starting_buffer_size,
            num_scheduled,
            num_unschedulable_conflicts,
            num_unschedulable_threads,
            num_filtered_out: 0,
            filter_time_us: 0,
        })
    }

    fn scheduling_common_mut(&mut self) -> &mut SchedulingCommon<Tx> {
        &mut self.common
    }
}

fn try_schedule_transaction<Tx: TransactionWithMeta>(
    transaction_state: &mut TransactionState<Tx>,
    pre_lock_filter: impl Fn(&TransactionState<Tx>) -> PreLockFilterAction,
    account_locks: &mut ThreadAwareAccountLocks,
    schedulable_threads: ThreadSet,
    thread_selector: impl Fn(ThreadSet) -> ThreadId,
) -> Result<TransactionSchedulingInfo<Tx>, TransactionSchedulingError> {
    match pre_lock_filter(transaction_state) {
        PreLockFilterAction::AttemptToSchedule => {}
    }

    // Schedule the transaction if it can be.
    let transaction = transaction_state.transaction();
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
        schedulable_threads,
        thread_selector,
    ) {
        Ok(thread_id) => thread_id,
        Err(TryLockError::MultipleConflicts) => {
            return Err(TransactionSchedulingError::UnschedulableConflicts);
        }
        Err(TryLockError::ThreadNotAllowed) => {
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
mod test {
    use {
        super::*,
        crate::banking_stage::{
            scheduler_messages::{MaxAge, TransactionId},
            transaction_scheduler::transaction_state_container::TransactionStateContainer,
        },
        crossbeam_channel::unbounded,
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
        test_case::test_case,
    };

    #[allow(clippy::type_complexity)]
    fn create_test_frame(
        num_threads: usize,
        config: GreedySchedulerConfig,
    ) -> (
        GreedyScheduler<RuntimeTransaction<SanitizedTransaction>>,
        Vec<Receiver<ConsumeWork<RuntimeTransaction<SanitizedTransaction>>>>,
        Sender<FinishedConsumeWork<RuntimeTransaction<SanitizedTransaction>>>,
    ) {
        let (consume_work_senders, consume_work_receivers) =
            (0..num_threads).map(|_| unbounded()).unzip();
        let (finished_consume_work_sender, finished_consume_work_receiver) = unbounded();
        let scheduler =
            GreedyScheduler::new(consume_work_senders, finished_consume_work_receiver, config);
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
        let mut container = TransactionStateContainer::with_capacity(10 * 1024);
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
        let (mut scheduler, work_receivers, _finished_work_sender) =
            create_test_frame(1, GreedySchedulerConfig::default());
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
        let (mut scheduler, work_receivers, _finished_work_sender) =
            create_test_frame(1, GreedySchedulerConfig::default());
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
        let (mut scheduler, _work_receivers, _finished_work_sender) =
            create_test_frame(1, GreedySchedulerConfig::default());
        let mut container = create_container([
            (&Keypair::new(), &[Pubkey::new_unique()], 1, 1),
            (&Keypair::new(), &[Pubkey::new_unique()], 2, 2),
        ]);

        let scheduling_summary = scheduler
            .schedule(
                &mut container,
                0, // zero budget
                false,
                test_pre_graph_filter,
                test_pre_lock_filter,
            )
            .unwrap();
        assert_eq!(scheduling_summary.num_scheduled, 0);
        assert_eq!(scheduling_summary.num_unschedulable_conflicts, 0);
    }

    #[test]
    fn test_schedule_single_threaded_scheduling_cu_limit() {
        let (mut scheduler, work_receivers, _finished_work_sender) = create_test_frame(
            1,
            GreedySchedulerConfig {
                target_scheduled_cus: 1, // only allow 1 transaction scheduled
                ..GreedySchedulerConfig::default()
            },
        );
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
        assert_eq!(scheduling_summary.num_scheduled, 1);
        assert_eq!(scheduling_summary.num_unschedulable_conflicts, 0);
        assert_eq!(collect_work(&work_receivers[0]).1, vec![vec![1]]);
    }

    #[test]
    fn test_schedule_single_threaded_scheduling_scan_limit() {
        let (mut scheduler, work_receivers, _finished_work_sender) = create_test_frame(
            1,
            GreedySchedulerConfig {
                max_scanned_transactions_per_scheduling_pass: 1, // only allow 1 transaction scheduled
                ..GreedySchedulerConfig::default()
            },
        );
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
        assert_eq!(scheduling_summary.num_scheduled, 1);
        assert_eq!(scheduling_summary.num_unschedulable_conflicts, 0);
        assert_eq!(collect_work(&work_receivers[0]).1, vec![vec![1]]);
    }

    #[test]
    fn test_schedule_single_threaded_scheduling_batch_size() {
        let (mut scheduler, work_receivers, _finished_work_sender) = create_test_frame(
            1,
            GreedySchedulerConfig {
                target_transactions_per_batch: 1, // only allow 1 transaction per batch
                ..GreedySchedulerConfig::default()
            },
        );
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
        assert_eq!(collect_work(&work_receivers[0]).1, vec![vec![1], vec![0]]);
    }

    #[test_case(true; "relax_intrabatch_account_locks_true")]
    #[test_case(false; "relax_intrabatch_account_locks_false")]
    fn test_schedule_single_threaded_conflict(relax_intrabatch_account_locks: bool) {
        let (mut scheduler, work_receivers, _finished_work_sender) =
            create_test_frame(1, GreedySchedulerConfig::default());
        let pubkey = Pubkey::new_unique();
        let mut container = create_container([
            (&Keypair::new(), &[pubkey], 1, 1),
            (&Keypair::new(), &[pubkey], 1, 2),
        ]);

        let scheduling_summary = scheduler
            .schedule(
                &mut container,
                u64::MAX, // no budget
                relax_intrabatch_account_locks,
                test_pre_graph_filter,
                test_pre_lock_filter,
            )
            .unwrap();
        assert_eq!(scheduling_summary.num_scheduled, 2);
        assert_eq!(scheduling_summary.num_unschedulable_conflicts, 0);
        if relax_intrabatch_account_locks {
            assert_eq!(collect_work(&work_receivers[0]).1, vec![vec![1, 0]]);
        } else {
            assert_eq!(collect_work(&work_receivers[0]).1, vec![vec![1], vec![0]]);
        }
    }

    #[test]
    fn test_schedule_simple_thread_selection() {
        let (mut scheduler, work_receivers, _finished_work_sender) =
            create_test_frame(2, GreedySchedulerConfig::default());
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
    fn test_schedule_scan_past_highest_priority() {
        let (mut scheduler, work_receivers, _finished_work_sender) =
            create_test_frame(2, GreedySchedulerConfig::default());
        let pubkey1 = Pubkey::new_unique();
        let pubkey2 = Pubkey::new_unique();
        let pubkey3 = Pubkey::new_unique();

        // Dependency graph:
        // 3 --
        //     \
        //       -> 1 -> 0
        //     /
        // 2 --
        //
        // Notes:
        // - 3 and 2 are immediately schedulable at the top of the graph.
        // - 3 and 2 will be scheduled to different threads.
        // - 1 conflicts with both 3 and 2 (different threads) so is unschedulable.
        // - 0 conflicts only with 1. Without priority guarding, it will be scheduled
        let mut container = create_container([
            (Keypair::new(), &[pubkey3][..], 0, 0),
            (Keypair::new(), &[pubkey1, pubkey2, pubkey3][..], 1, 1),
            (Keypair::new(), &[pubkey2][..], 2, 2),
            (Keypair::new(), &[pubkey1][..], 3, 3),
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
        assert_eq!(scheduling_summary.num_scheduled, 3);
        assert_eq!(scheduling_summary.num_unschedulable_conflicts, 1);
        assert_eq!(collect_work(&work_receivers[0]).1, [vec![3], vec![0]]);
        assert_eq!(collect_work(&work_receivers[1]).1, [vec![2]]);
    }

    #[test]
    fn test_schedule_local_fee_markets() {
        let (mut scheduler, work_receivers, _finished_work_sender) = create_test_frame(
            2,
            GreedySchedulerConfig {
                target_scheduled_cus: 4 * 5_000, // 2 txs per thread
                ..GreedySchedulerConfig::default()
            },
        );

        // Low priority transaction that does not conflict with other work.
        // Enough work to fill up thread 0 on txs using `conflicting_pubkey`.
        let conflicting_pubkey = Pubkey::new_unique();
        let unique_pubkey = Pubkey::new_unique();
        let mut container = create_container([
            (Keypair::new(), [unique_pubkey], 0, 0),
            (Keypair::new(), [conflicting_pubkey], 1, 1),
            (Keypair::new(), [conflicting_pubkey], 2, 2),
            (Keypair::new(), [conflicting_pubkey], 3, 3),
            (Keypair::new(), [conflicting_pubkey], 4, 4),
            (Keypair::new(), [conflicting_pubkey], 5, 5),
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
        assert_eq!(scheduling_summary.num_scheduled, 3);
        assert_eq!(scheduling_summary.num_unschedulable_threads, 3);
        assert_eq!(collect_work(&work_receivers[0]).1, [vec![5], vec![4]]);
        assert_eq!(collect_work(&work_receivers[1]).1, [vec![0]]);
    }
}
