use {
    super::{
        in_flight_tracker::InFlightTracker,
        prio_graph_scheduler::{
            Batches, PrioGraphScheduler, TransactionSchedulingError, TransactionSchedulingInfo,
        },
        scheduler::{Scheduler, SchedulingSummary},
        scheduler_error::SchedulerError,
        thread_aware_account_locks::{ThreadAwareAccountLocks, ThreadId, ThreadSet},
        transaction_priority_id::TransactionPriorityId,
        transaction_state::{SanitizedTransactionTTL, TransactionState},
        transaction_state_container::StateContainer,
    },
    crate::banking_stage::{
        consumer::TARGET_NUM_TRANSACTIONS_PER_BATCH,
        read_write_account_set::ReadWriteAccountSet,
        scheduler_messages::{ConsumeWork, FinishedConsumeWork, TransactionBatchId},
        transaction_scheduler::thread_aware_account_locks::MAX_THREADS,
    },
    crossbeam_channel::{Receiver, Sender, TryRecvError},
    itertools::izip,
    solana_cost_model::block_cost_limits::MAX_BLOCK_UNITS,
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    solana_sdk::saturating_add_assign,
};

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
    in_flight_tracker: InFlightTracker,
    account_locks: ThreadAwareAccountLocks,
    consume_work_senders: Vec<Sender<ConsumeWork<Tx>>>,
    finished_consume_work_receiver: Receiver<FinishedConsumeWork<Tx>>,
    working_account_set: ReadWriteAccountSet,
    unschedulables: Vec<TransactionPriorityId>,
    config: GreedySchedulerConfig,
}

impl<Tx: TransactionWithMeta> GreedyScheduler<Tx> {
    pub(crate) fn new(
        consume_work_senders: Vec<Sender<ConsumeWork<Tx>>>,
        finished_consume_work_receiver: Receiver<FinishedConsumeWork<Tx>>,
        config: GreedySchedulerConfig,
    ) -> Self {
        let num_threads = consume_work_senders.len();
        assert!(num_threads > 0, "must have at least one worker");
        assert!(
            num_threads <= MAX_THREADS,
            "cannot have more than {MAX_THREADS} workers"
        );
        Self {
            in_flight_tracker: InFlightTracker::new(num_threads),
            account_locks: ThreadAwareAccountLocks::new(num_threads),
            consume_work_senders,
            finished_consume_work_receiver,
            working_account_set: ReadWriteAccountSet::default(),
            unschedulables: Vec::with_capacity(config.max_scanned_transactions_per_scheduling_pass),
            config,
        }
    }
}

impl<Tx: TransactionWithMeta> Scheduler<Tx> for GreedyScheduler<Tx> {
    fn schedule<S: StateContainer<Tx>>(
        &mut self,
        container: &mut S,
        _pre_graph_filter: impl Fn(&[&Tx], &mut [bool]),
        pre_lock_filter: impl Fn(&Tx) -> bool,
    ) -> Result<SchedulingSummary, SchedulerError> {
        let num_threads = self.consume_work_senders.len();
        let target_cu_per_thread = self.config.target_scheduled_cus / num_threads as u64;

        let mut schedulable_threads = ThreadSet::any(num_threads);
        for thread_id in 0..num_threads {
            if self.in_flight_tracker.cus_in_flight_per_thread()[thread_id] >= target_cu_per_thread
            {
                schedulable_threads.remove(thread_id);
            }
        }
        if schedulable_threads.is_empty() {
            return Ok(SchedulingSummary::default());
        }

        // Track metrics on filter.
        let mut num_filtered_out: usize = 0;
        let mut num_scanned: usize = 0;
        let mut num_scheduled: usize = 0;
        let mut num_sent: usize = 0;
        let mut num_unschedulable: usize = 0;

        let mut batches = Batches::new(num_threads, self.config.target_transactions_per_batch);
        while num_scanned < self.config.max_scanned_transactions_per_scheduling_pass
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
            if !self
                .working_account_set
                .check_locks(&transaction_state.transaction_ttl().transaction)
            {
                self.working_account_set.clear();
                num_sent += self.send_batches(&mut batches)?;
            }

            // Now check if the transaction can actually be scheduled.
            match try_schedule_transaction(
                transaction_state,
                &pre_lock_filter,
                &mut self.account_locks,
                schedulable_threads,
                |thread_set| {
                    PrioGraphScheduler::<Tx>::select_thread(
                        thread_set,
                        &batches.total_cus,
                        self.in_flight_tracker.cus_in_flight_per_thread(),
                        &batches.transactions,
                        self.in_flight_tracker.num_in_flight_per_thread(),
                    )
                },
            ) {
                Err(TransactionSchedulingError::Filtered) => {
                    num_filtered_out += 1;
                    container.remove_by_id(id.id);
                }
                Err(TransactionSchedulingError::UnschedulableConflicts) => {
                    num_unschedulable += 1;
                    self.unschedulables.push(id);
                }
                Ok(TransactionSchedulingInfo {
                    thread_id,
                    transaction,
                    max_age,
                    cost,
                }) => {
                    assert!(
                        self.working_account_set.take_locks(&transaction),
                        "locks must be available"
                    );
                    saturating_add_assign!(num_scheduled, 1);
                    batches.transactions[thread_id].push(transaction);
                    batches.ids[thread_id].push(id.id);
                    batches.max_ages[thread_id].push(max_age);
                    saturating_add_assign!(batches.total_cus[thread_id], cost);

                    // If target batch size is reached, send all the batches
                    if batches.ids[thread_id].len() >= self.config.target_transactions_per_batch {
                        self.working_account_set.clear();
                        num_sent += self.send_batches(&mut batches)?;
                    }

                    // if the thread is at target_cu_per_thread, remove it from the schedulable threads
                    // if there are no more schedulable threads, stop scheduling.
                    if self.in_flight_tracker.cus_in_flight_per_thread()[thread_id]
                        + batches.total_cus[thread_id]
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
        num_sent += self.send_batches(&mut batches)?;
        assert_eq!(
            num_scheduled, num_sent,
            "number of scheduled and sent transactions must match"
        );

        // Push unschedulables back into the queue
        for id in self.unschedulables.drain(..) {
            container.push_id_into_queue(id);
        }

        Ok(SchedulingSummary {
            num_scheduled,
            num_unschedulable,
            num_filtered_out,
            filter_time_us: 0,
        })
    }

    /// Receive completed batches of transactions without blocking.
    /// Returns (num_transactions, num_retryable_transactions) on success.
    fn receive_completed(
        &mut self,
        container: &mut impl StateContainer<Tx>,
    ) -> Result<(usize, usize), SchedulerError> {
        let mut total_num_transactions: usize = 0;
        let mut total_num_retryable: usize = 0;
        loop {
            let (num_transactions, num_retryable) = self.try_receive_completed(container)?;
            if num_transactions == 0 {
                break;
            }
            saturating_add_assign!(total_num_transactions, num_transactions);
            saturating_add_assign!(total_num_retryable, num_retryable);
        }
        Ok((total_num_transactions, total_num_retryable))
    }
}

impl<Tx: TransactionWithMeta> GreedyScheduler<Tx> {
    /// Receive completed batches of transactions.
    /// Returns `Ok((num_transactions, num_retryable))` if a batch was received, `Ok((0, 0))` if no batch was received.
    fn try_receive_completed(
        &mut self,
        container: &mut impl StateContainer<Tx>,
    ) -> Result<(usize, usize), SchedulerError> {
        match self.finished_consume_work_receiver.try_recv() {
            Ok(FinishedConsumeWork {
                work:
                    ConsumeWork {
                        batch_id,
                        ids,
                        transactions,
                        max_ages,
                    },
                retryable_indexes,
            }) => {
                let num_transactions = ids.len();
                let num_retryable = retryable_indexes.len();

                // Free the locks
                self.complete_batch(batch_id, &transactions);

                // Retryable transactions should be inserted back into the container
                let mut retryable_iter = retryable_indexes.into_iter().peekable();
                for (index, (id, transaction, max_age)) in
                    izip!(ids, transactions, max_ages).enumerate()
                {
                    if let Some(retryable_index) = retryable_iter.peek() {
                        if *retryable_index == index {
                            container.retry_transaction(
                                id,
                                SanitizedTransactionTTL {
                                    transaction,
                                    max_age,
                                },
                            );
                            retryable_iter.next();
                            continue;
                        }
                    }
                    container.remove_by_id(id);
                }

                Ok((num_transactions, num_retryable))
            }
            Err(TryRecvError::Empty) => Ok((0, 0)),
            Err(TryRecvError::Disconnected) => Err(SchedulerError::DisconnectedRecvChannel(
                "finished consume work",
            )),
        }
    }

    /// Mark a given `TransactionBatchId` as completed.
    /// This will update the internal tracking, including account locks.
    fn complete_batch(&mut self, batch_id: TransactionBatchId, transactions: &[Tx]) {
        let thread_id = self.in_flight_tracker.complete_batch(batch_id);
        for transaction in transactions {
            let account_keys = transaction.account_keys();
            let write_account_locks = account_keys
                .iter()
                .enumerate()
                .filter_map(|(index, key)| transaction.is_writable(index).then_some(key));
            let read_account_locks = account_keys
                .iter()
                .enumerate()
                .filter_map(|(index, key)| (!transaction.is_writable(index)).then_some(key));
            self.account_locks
                .unlock_accounts(write_account_locks, read_account_locks, thread_id);
        }
    }

    /// Send all batches of transactions to the worker threads.
    /// Returns the number of transactions sent.
    fn send_batches(&mut self, batches: &mut Batches<Tx>) -> Result<usize, SchedulerError> {
        (0..self.consume_work_senders.len())
            .map(|thread_index| self.send_batch(batches, thread_index))
            .sum()
    }

    /// Send a batch of transactions to the given thread's `ConsumeWork` channel.
    /// Returns the number of transactions sent.
    fn send_batch(
        &mut self,
        batches: &mut Batches<Tx>,
        thread_index: usize,
    ) -> Result<usize, SchedulerError> {
        if batches.ids[thread_index].is_empty() {
            return Ok(0);
        }

        let (ids, transactions, max_ages, total_cus) =
            batches.take_batch(thread_index, self.config.target_transactions_per_batch);

        let batch_id = self
            .in_flight_tracker
            .track_batch(ids.len(), total_cus, thread_index);

        let num_scheduled = ids.len();
        let work = ConsumeWork {
            batch_id,
            ids,
            transactions,
            max_ages,
        };
        self.consume_work_senders[thread_index]
            .send(work)
            .map_err(|_| SchedulerError::DisconnectedSendChannel("consume work sender"))?;

        Ok(num_scheduled)
    }
}

fn try_schedule_transaction<Tx: TransactionWithMeta>(
    transaction_state: &mut TransactionState<Tx>,
    pre_lock_filter: impl Fn(&Tx) -> bool,
    account_locks: &mut ThreadAwareAccountLocks,
    schedulable_threads: ThreadSet,
    thread_selector: impl Fn(ThreadSet) -> ThreadId,
) -> Result<TransactionSchedulingInfo<Tx>, TransactionSchedulingError> {
    let transaction = &transaction_state.transaction_ttl().transaction;
    if !pre_lock_filter(transaction) {
        return Err(TransactionSchedulingError::Filtered);
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

    let Some(thread_id) = account_locks.try_lock_accounts(
        write_account_locks,
        read_account_locks,
        schedulable_threads,
        thread_selector,
    ) else {
        return Err(TransactionSchedulingError::UnschedulableConflicts);
    };

    let sanitized_transaction_ttl = transaction_state.transition_to_pending();
    let cost = transaction_state.cost();

    Ok(TransactionSchedulingInfo {
        thread_id,
        transaction: sanitized_transaction_ttl.transaction,
        max_age: sanitized_transaction_ttl.max_age,
        cost,
    })
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::banking_stage::{
            immutable_deserialized_packet::ImmutableDeserializedPacket,
            scheduler_messages::{MaxAge, TransactionId},
            transaction_scheduler::{
                transaction_state::SanitizedTransactionTTL,
                transaction_state_container::TransactionStateContainer,
            },
        },
        crossbeam_channel::unbounded,
        itertools::Itertools,
        solana_perf::packet::Packet,
        solana_pubkey::Pubkey,
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_sdk::{
            compute_budget::ComputeBudgetInstruction,
            hash::Hash,
            message::Message,
            signature::Keypair,
            signer::Signer,
            system_instruction,
            transaction::{SanitizedTransaction, Transaction},
        },
        std::{borrow::Borrow, sync::Arc},
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
            let packet = Arc::new(
                ImmutableDeserializedPacket::new(
                    Packet::from_data(None, transaction.to_versioned_transaction()).unwrap(),
                )
                .unwrap(),
            );
            let transaction_ttl = SanitizedTransactionTTL {
                transaction,
                max_age: MaxAge::MAX,
            };
            const TEST_TRANSACTION_COST: u64 = 5000;
            container.insert_new_transaction(
                transaction_ttl,
                packet,
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

    fn test_pre_lock_filter(_tx: &RuntimeTransaction<SanitizedTransaction>) -> bool {
        true
    }

    #[test]
    fn test_schedule_disconnected_channel() {
        let (mut scheduler, work_receivers, _finished_work_sender) =
            create_test_frame(1, GreedySchedulerConfig::default());
        let mut container = create_container([(&Keypair::new(), &[Pubkey::new_unique()], 1, 1)]);

        drop(work_receivers); // explicitly drop receivers
        assert_matches!(
            scheduler.schedule(&mut container, test_pre_graph_filter, test_pre_lock_filter),
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
            .schedule(&mut container, test_pre_graph_filter, test_pre_lock_filter)
            .unwrap();
        assert_eq!(scheduling_summary.num_scheduled, 2);
        assert_eq!(scheduling_summary.num_unschedulable, 0);
        assert_eq!(collect_work(&work_receivers[0]).1, vec![vec![1, 0]]);
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
            .schedule(&mut container, test_pre_graph_filter, test_pre_lock_filter)
            .unwrap();
        assert_eq!(scheduling_summary.num_scheduled, 1);
        assert_eq!(scheduling_summary.num_unschedulable, 0);
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
            .schedule(&mut container, test_pre_graph_filter, test_pre_lock_filter)
            .unwrap();
        assert_eq!(scheduling_summary.num_scheduled, 1);
        assert_eq!(scheduling_summary.num_unschedulable, 0);
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
            .schedule(&mut container, test_pre_graph_filter, test_pre_lock_filter)
            .unwrap();
        assert_eq!(scheduling_summary.num_scheduled, 2);
        assert_eq!(scheduling_summary.num_unschedulable, 0);
        assert_eq!(collect_work(&work_receivers[0]).1, vec![vec![1], vec![0]]);
    }

    #[test]
    fn test_schedule_single_threaded_conflict() {
        let (mut scheduler, work_receivers, _finished_work_sender) =
            create_test_frame(1, GreedySchedulerConfig::default());
        let pubkey = Pubkey::new_unique();
        let mut container = create_container([
            (&Keypair::new(), &[pubkey], 1, 1),
            (&Keypair::new(), &[pubkey], 1, 2),
        ]);

        let scheduling_summary = scheduler
            .schedule(&mut container, test_pre_graph_filter, test_pre_lock_filter)
            .unwrap();
        assert_eq!(scheduling_summary.num_scheduled, 2);
        assert_eq!(scheduling_summary.num_unschedulable, 0);
        assert_eq!(collect_work(&work_receivers[0]).1, vec![vec![1], vec![0]]);
    }

    #[test]
    fn test_schedule_simple_thread_selection() {
        let (mut scheduler, work_receivers, _finished_work_sender) =
            create_test_frame(2, GreedySchedulerConfig::default());
        let mut container =
            create_container((0..4).map(|i| (Keypair::new(), [Pubkey::new_unique()], 1, i)));

        let scheduling_summary = scheduler
            .schedule(&mut container, test_pre_graph_filter, test_pre_lock_filter)
            .unwrap();
        assert_eq!(scheduling_summary.num_scheduled, 4);
        assert_eq!(scheduling_summary.num_unschedulable, 0);
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

        // Dependecy graph:
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
            .schedule(&mut container, test_pre_graph_filter, test_pre_lock_filter)
            .unwrap();
        assert_eq!(scheduling_summary.num_scheduled, 3);
        assert_eq!(scheduling_summary.num_unschedulable, 1);
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
            .schedule(&mut container, test_pre_graph_filter, test_pre_lock_filter)
            .unwrap();
        assert_eq!(scheduling_summary.num_scheduled, 3);
        assert_eq!(scheduling_summary.num_unschedulable, 3);
        assert_eq!(collect_work(&work_receivers[0]).1, [vec![5], vec![4]]);
        assert_eq!(collect_work(&work_receivers[1]).1, [vec![0]]);
    }
}
