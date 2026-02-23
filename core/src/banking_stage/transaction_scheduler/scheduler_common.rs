#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    super::{
        in_flight_tracker::InFlightTracker, scheduler_error::SchedulerError,
        transaction_state_container::StateContainer,
    },
    crate::banking_stage::scheduler_messages::{
        ConsumeWork, FinishedConsumeWork, MaxAge, TransactionBatchId, TransactionId,
    },
    agave_scheduling_utils::thread_aware_account_locks::{
        MAX_THREADS, ThreadAwareAccountLocks, ThreadId, ThreadSet,
    },
    crossbeam_channel::{Receiver, Sender, TryRecvError},
    itertools::izip,
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
};

pub struct Batches<Tx> {
    ids: Vec<Vec<TransactionId>>,
    transactions: Vec<Vec<Tx>>,
    max_ages: Vec<Vec<MaxAge>>,
    total_cus: Vec<u64>,
    target_num_transactions_per_batch: usize,
}

impl<Tx> Batches<Tx> {
    pub fn new(num_threads: usize, target_num_transactions_per_batch: usize) -> Self {
        fn make_vecs<T>(
            num_threads: usize,
            target_num_transactions_per_batch: usize,
        ) -> Vec<Vec<T>> {
            (0..num_threads)
                .map(|_| Vec::with_capacity(target_num_transactions_per_batch))
                .collect()
        }

        Self {
            ids: make_vecs(num_threads, target_num_transactions_per_batch),
            transactions: make_vecs(num_threads, target_num_transactions_per_batch),
            max_ages: make_vecs(num_threads, target_num_transactions_per_batch),
            total_cus: vec![0; num_threads],
            target_num_transactions_per_batch,
        }
    }

    #[cfg(debug_assertions)]
    pub fn is_empty(&self) -> bool {
        self.ids.iter().all(|ids| ids.is_empty())
            && self.transactions.iter().all(|txs| txs.is_empty())
            && self.max_ages.iter().all(|max_ages| max_ages.is_empty())
            && self.total_cus.iter().all(|&cus| cus == 0)
    }

    pub fn total_cus(&self) -> &[u64] {
        &self.total_cus
    }

    pub fn transactions(&self) -> &[Vec<Tx>] {
        &self.transactions
    }

    pub fn add_transaction_to_batch(
        &mut self,
        thread_id: ThreadId,
        transaction_id: TransactionId,
        transaction: Tx,
        max_age: MaxAge,
        cus: u64,
    ) {
        self.ids[thread_id].push(transaction_id);
        self.transactions[thread_id].push(transaction);
        self.max_ages[thread_id].push(max_age);
        self.total_cus[thread_id] += cus;
    }

    pub fn take_batch(
        &mut self,
        thread_id: ThreadId,
    ) -> (Vec<TransactionId>, Vec<Tx>, Vec<MaxAge>, u64) {
        (
            core::mem::replace(
                &mut self.ids[thread_id],
                Vec::with_capacity(self.target_num_transactions_per_batch),
            ),
            core::mem::replace(
                &mut self.transactions[thread_id],
                Vec::with_capacity(self.target_num_transactions_per_batch),
            ),
            core::mem::replace(
                &mut self.max_ages[thread_id],
                Vec::with_capacity(self.target_num_transactions_per_batch),
            ),
            core::mem::replace(&mut self.total_cus[thread_id], 0),
        )
    }
}

/// A transaction has been scheduled to a thread.
pub struct TransactionSchedulingInfo<Tx> {
    pub thread_id: ThreadId,
    pub transaction: Tx,
    pub max_age: MaxAge,
    pub cost: u64,
}

/// Error type for reasons a transaction could not be scheduled.
pub enum TransactionSchedulingError {
    /// Transaction cannot be scheduled due to conflicts, or
    /// higher priority conflicting transactions are unschedulable.
    UnschedulableConflicts,
    /// Thread is not allowed to be scheduled on at this time.
    UnschedulableThread,
}

/// Given the schedulable `thread_set`, select the thread with the least amount
/// of work queued up.
/// Currently, "work" is just defined as the number of transactions.
///
/// If the `chain_thread` is available, this thread will be selected, regardless of
/// load-balancing.
///
/// Panics if the `thread_set` is empty. This should never happen, see comment
/// on `ThreadAwareAccountLocks::try_lock_accounts`.
pub fn select_thread<Tx>(
    thread_set: ThreadSet,
    batch_cus_per_thread: &[u64],
    in_flight_cus_per_thread: &[u64],
    batches_per_thread: &[Vec<Tx>],
    in_flight_per_thread: &[usize],
) -> ThreadId {
    thread_set
        .contained_threads_iter()
        .map(|thread_id| {
            (
                thread_id,
                batch_cus_per_thread[thread_id] + in_flight_cus_per_thread[thread_id],
                batches_per_thread[thread_id].len() + in_flight_per_thread[thread_id],
            )
        })
        .min_by(|a, b| a.1.cmp(&b.1).then_with(|| a.2.cmp(&b.2)))
        .map(|(thread_id, _, _)| thread_id)
        .unwrap()
}

/// Common scheduler communication structure.
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) struct SchedulingCommon<Tx> {
    pub(crate) consume_work_senders: Vec<Sender<ConsumeWork<Tx>>>,
    pub(crate) finished_consume_work_receiver: Receiver<FinishedConsumeWork<Tx>>,
    pub(crate) in_flight_tracker: InFlightTracker,
    pub(crate) account_locks: ThreadAwareAccountLocks,
    pub(crate) batches: Batches<Tx>,
}

impl<Tx> SchedulingCommon<Tx> {
    pub fn new(
        consume_work_senders: Vec<Sender<ConsumeWork<Tx>>>,
        finished_consume_work_receiver: Receiver<FinishedConsumeWork<Tx>>,
        target_num_transactions_per_batch: usize,
    ) -> Self {
        let num_threads = consume_work_senders.len();
        assert!(num_threads > 0, "must have at least one worker");
        assert!(
            num_threads <= MAX_THREADS,
            "cannot have more than {MAX_THREADS} workers"
        );
        Self {
            batches: Batches::new(
                consume_work_senders.len(),
                target_num_transactions_per_batch,
            ),
            consume_work_senders,
            finished_consume_work_receiver,
            in_flight_tracker: InFlightTracker::new(num_threads),
            account_locks: ThreadAwareAccountLocks::new(num_threads),
        }
    }

    /// Send a batch of transactions to the given thread's `ConsumeWork` channel.
    /// Returns the number of transactions sent.
    pub fn send_batch(&mut self, thread_index: usize) -> Result<usize, SchedulerError> {
        if self.batches.ids[thread_index].is_empty() {
            return Ok(0);
        }

        let (ids, transactions, max_ages, total_cus) = self.batches.take_batch(thread_index);

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

    /// Send all batches of transactions to the worker threads.
    /// Returns the number of transactions sent.
    pub fn send_batches(&mut self) -> Result<usize, SchedulerError> {
        (0..self.consume_work_senders.len())
            .map(|thread_index| self.send_batch(thread_index))
            .sum()
    }
}

impl<Tx: TransactionWithMeta> SchedulingCommon<Tx> {
    /// Receive completed batches of transactions.
    /// Returns `Ok((num_transactions, num_retryable))` if a batch was received, `Ok((0, 0))` if no batch was received.
    pub fn try_receive_completed(
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
                        max_ages: _,
                    },
                retryable_indexes,
            }) => {
                let num_transactions = ids.len();
                let num_retryable = retryable_indexes.len();

                // Free the locks
                self.complete_batch(batch_id, &transactions);

                // Assumption - retryable indexes are in order (sorted by workers).
                let mut retryable_iter = retryable_indexes.iter().peekable();
                for (index, (id, transaction)) in izip!(ids, transactions).enumerate() {
                    if let Some(&retryable_index) = retryable_iter.peek() {
                        if retryable_index.index == index {
                            container.retry_transaction(
                                id,
                                transaction,
                                retryable_index.immediately_retryable,
                            );
                            retryable_iter.next();
                            continue;
                        }
                    }
                    container.remove_by_id(id);
                }

                debug_assert!(
                    retryable_iter.peek().is_none(),
                    "retryable indexes were not in order: {:?}",
                    retryable_indexes
                        .iter()
                        .map(|index| index.index)
                        .collect::<Vec<_>>(),
                );

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
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::banking_stage::{
            consumer::RetryableIndex,
            transaction_scheduler::transaction_state_container::TransactionStateContainer,
        },
        crossbeam_channel::unbounded,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_pubkey::Pubkey,
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_system_transaction as system_transaction,
        solana_transaction::sanitized::SanitizedTransaction,
        test_case::test_case,
    };

    const NUM_WORKERS: usize = 4;
    const DUMMY_COST: u64 = 1;

    fn simple_transaction() -> RuntimeTransaction<SanitizedTransaction> {
        RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
            &Keypair::new(),
            &Pubkey::new_unique(),
            1,
            Hash::default(),
        ))
    }

    fn add_transactions_to_container(
        container: &mut TransactionStateContainer<RuntimeTransaction<SanitizedTransaction>>,
        count: usize,
    ) {
        for index in 0..count {
            container.insert_new_transaction(
                simple_transaction(),
                MaxAge::MAX,
                (count - index) as u64,
                DUMMY_COST,
            );
        }
    }

    fn pop_and_add_transaction<Tx: TransactionWithMeta>(
        container: &mut TransactionStateContainer<Tx>,
        common: &mut SchedulingCommon<Tx>,
        thread_id: ThreadId,
    ) {
        let tx_id = container.pop().unwrap();
        let (transaction, max_age) = container
            .get_mut_transaction_state(tx_id.id)
            .unwrap()
            .take_transaction_for_scheduling();

        let account_keys = transaction.account_keys();
        let write_account_locks = account_keys
            .iter()
            .enumerate()
            .filter_map(|(index, key)| transaction.is_writable(index).then_some(key));
        let read_account_locks = account_keys
            .iter()
            .enumerate()
            .filter_map(|(index, key)| (!transaction.is_writable(index)).then_some(key));

        common
            .account_locks
            .try_lock_accounts(
                write_account_locks,
                read_account_locks,
                ThreadSet::any(NUM_WORKERS),
                |_thread_set| thread_id,
            )
            .unwrap();
        common.batches.add_transaction_to_batch(
            thread_id,
            tx_id.id,
            transaction,
            max_age,
            DUMMY_COST,
        );
    }

    #[test_case(
        ThreadSet::any(4),
        vec![0, 0, 0, 0],
        vec![0, 0, 0, 0],
        vec![vec![], vec![], vec![], vec![]],
        vec![0, 0, 0, 0],
        0 ; "test-case::simple")
    ]
    #[test_case(
        ThreadSet::any(4),
        vec![4, 3, 2, 1],
        vec![0, 0, 0, 0],
        vec![vec![()], vec![()], vec![()], vec![()]],
        vec![0, 0, 0, 0],
        3 ; "test-case::batch cu select"
    )]
    #[test_case(
        ThreadSet::any(4),
        vec![4, 4, 4, 4],
        vec![0, 0, 0, 0],
        vec![vec![(); 2], vec![(); 3], vec![(); 1], vec![(); 4]],
        vec![0, 0, 0, 0],
        2 ; "test-case::batch count select"
    )]
    #[test_case(
        ThreadSet::any(4),
        vec![0, 0, 0, 0],
        vec![4, 3, 2, 1],
        vec![vec![()], vec![()], vec![()], vec![()]],
        vec![0, 0, 0, 0],
        3 ; "test-case::in-flight cu select"
    )]
    #[test_case(
        ThreadSet::any(4),
        vec![0, 0, 0, 0],
        vec![0, 0, 0, 0],
        vec![vec![()], vec![()], vec![()], vec![()]],
        vec![2, 3, 1, 4],
        2 ; "test-case::in-flight count select"
    )]
    #[test_case(
        ThreadSet::any(4),
        vec![4, 3, 2, 1],
        vec![0, 0, 0, 0],
        vec![vec![()], vec![()], vec![()], vec![()]],
        vec![2, 3, 1, 4],
        3 ; "test-case::cus before count"
    )]
    #[test_case(
        ThreadSet::any(4) - ThreadSet::only(3),
        vec![4, 3, 2, 1],
        vec![0, 0, 0, 0],
        vec![vec![()], vec![()], vec![()], vec![()]],
        vec![2, 3, 1, 4],
        2 ; "test-case::thread_set"
    )]
    fn test_select_thread(
        thread_set: ThreadSet,
        batch_cus_per_thread: Vec<u64>,
        in_flight_cus_per_thread: Vec<u64>,
        batches_per_thread: Vec<Vec<()>>,
        in_flight_per_thread: Vec<usize>,
        expected_thread: ThreadId,
    ) {
        let selected_thread = select_thread(
            thread_set,
            &batch_cus_per_thread,
            &in_flight_cus_per_thread,
            &batches_per_thread,
            &in_flight_per_thread,
        );
        assert_eq!(selected_thread, expected_thread);
    }

    #[test]
    fn test_send_batches() {
        let mut container = TransactionStateContainer::with_capacity(1024);
        add_transactions_to_container(&mut container, 3);

        let (work_senders, work_receivers): (Vec<Sender<_>>, Vec<Receiver<_>>) =
            (0..NUM_WORKERS).map(|_| unbounded()).unzip();
        let (_finished_work_sender, finished_work_receiver) = unbounded();
        let mut common = SchedulingCommon::new(work_senders, finished_work_receiver, 10);

        pop_and_add_transaction(&mut container, &mut common, 0);
        let num_scheduled = common.send_batch(0).unwrap();
        assert_eq!(num_scheduled, 1);
        assert_eq!(work_receivers[0].len(), 1);
        assert_eq!(
            common.in_flight_tracker.num_in_flight_per_thread(),
            &[1, 0, 0, 0]
        );
        assert_eq!(
            common.in_flight_tracker.cus_in_flight_per_thread(),
            &[DUMMY_COST, 0, 0, 0]
        );

        let num_scheduled = common.send_batch(1).unwrap();
        assert_eq!(num_scheduled, 0);
        assert_eq!(work_receivers[1].len(), 0); // not actually sent since no transactions.

        work_receivers[0].recv().unwrap();

        // Multiple batches.
        pop_and_add_transaction(&mut container, &mut common, 0);
        pop_and_add_transaction(&mut container, &mut common, 2);

        common.send_batches().unwrap();
        assert_eq!(work_receivers[0].len(), 1);
        assert_eq!(work_receivers[1].len(), 0);
        assert_eq!(work_receivers[2].len(), 1);
        assert_eq!(work_receivers[3].len(), 0);
        assert_eq!(
            common.in_flight_tracker.num_in_flight_per_thread(),
            &[2, 0, 1, 0]
        );
        assert_eq!(
            common.in_flight_tracker.cus_in_flight_per_thread(),
            &[DUMMY_COST * 2, 0, DUMMY_COST, 0]
        );
    }

    #[test]
    fn test_receive_completed() {
        let mut container = TransactionStateContainer::with_capacity(1024);
        add_transactions_to_container(&mut container, 1);

        let (work_senders, work_receivers): (Vec<Sender<_>>, Vec<Receiver<_>>) =
            (0..NUM_WORKERS).map(|_| unbounded()).unzip();
        let (finished_work_sender, finished_work_receiver) = unbounded();
        let mut common = SchedulingCommon::new(work_senders, finished_work_receiver, 10);

        // Send a batch. Return completed work.
        pop_and_add_transaction(&mut container, &mut common, 0);
        let num_scheduled = common.send_batch(0).unwrap();

        let work = work_receivers[0].try_recv().unwrap();
        assert_eq!(work.ids.len(), num_scheduled);
        let retryable_indexes = vec![];
        let finished_work = FinishedConsumeWork {
            work,
            retryable_indexes,
        };

        finished_work_sender.send(finished_work).unwrap();
        let (num_transactions, num_retryable) =
            common.try_receive_completed(&mut container).unwrap();
        assert_eq!(num_transactions, num_scheduled);
        assert_eq!(num_retryable, 0);
        assert_eq!(container.buffer_size(), 0);

        // Retryable indexes.
        add_transactions_to_container(&mut container, 3);
        pop_and_add_transaction(&mut container, &mut common, 0);
        pop_and_add_transaction(&mut container, &mut common, 0);
        pop_and_add_transaction(&mut container, &mut common, 0);
        let num_scheduled = common.send_batch(0).unwrap();
        let work = work_receivers[0].try_recv().unwrap();
        assert_eq!(work.ids.len(), num_scheduled);
        let retryable_indexes = vec![
            RetryableIndex::new(0, true),
            RetryableIndex::new(1, false), // should be held by container.
        ];
        let expected_num_retryable = retryable_indexes.len();
        let finished_work = FinishedConsumeWork {
            work,
            retryable_indexes,
        };
        finished_work_sender.send(finished_work).unwrap();
        let (num_transactions, num_retryable) =
            common.try_receive_completed(&mut container).unwrap();
        assert_eq!(num_transactions, num_scheduled);
        assert_eq!(num_retryable, expected_num_retryable);
        assert_eq!(container.buffer_size(), expected_num_retryable);
        assert_eq!(container.queue_size(), expected_num_retryable - 1); // held transaction not in queue.
    }

    #[test]
    #[should_panic = "retryable indexes were not in order: [1, 0]"]
    fn test_receive_completed_out_of_order() {
        let mut container = TransactionStateContainer::with_capacity(1024);

        let (work_senders, work_receivers): (Vec<Sender<_>>, Vec<Receiver<_>>) =
            (0..NUM_WORKERS).map(|_| unbounded()).unzip();
        let (finished_work_sender, finished_work_receiver) = unbounded();
        let mut common = SchedulingCommon::new(work_senders, finished_work_receiver, 10);
        // Retryable indexes out-of-order.
        add_transactions_to_container(&mut container, 2);
        pop_and_add_transaction(&mut container, &mut common, 0);
        pop_and_add_transaction(&mut container, &mut common, 0);
        let num_scheduled = common.send_batch(0).unwrap();
        let work = work_receivers[0].try_recv().unwrap();
        assert_eq!(work.ids.len(), num_scheduled);
        let retryable_indexes = vec![RetryableIndex::new(1, true), RetryableIndex::new(0, true)];
        let finished_work = FinishedConsumeWork {
            work,
            retryable_indexes,
        };
        finished_work_sender.send(finished_work).unwrap();

        // This should panic because the retryable indexes are not in order.
        let _ = common.try_receive_completed(&mut container);
    }
}
