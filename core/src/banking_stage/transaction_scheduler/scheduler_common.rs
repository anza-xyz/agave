use {
    super::{
        in_flight_tracker::InFlightTracker,
        scheduler_error::SchedulerError,
        thread_aware_account_locks::{ThreadAwareAccountLocks, ThreadId, ThreadSet},
        transaction_state_container::StateContainer,
    },
    crate::banking_stage::{
        scheduler_messages::{
            ConsumeWork, FinishedConsumeWork, MaxAge, TransactionBatchId, TransactionId,
        },
        transaction_scheduler::thread_aware_account_locks::MAX_THREADS,
    },
    crossbeam_channel::{Receiver, Sender, TryRecvError},
    itertools::izip,
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
};

pub struct Batches<Tx> {
    pub ids: Vec<Vec<TransactionId>>,
    pub transactions: Vec<Vec<Tx>>,
    pub max_ages: Vec<Vec<MaxAge>>,
    pub total_cus: Vec<u64>,
}

impl<Tx> Batches<Tx> {
    pub fn new(num_threads: usize, target_num_transactions_per_batch: usize) -> Self {
        Self {
            ids: vec![Vec::with_capacity(target_num_transactions_per_batch); num_threads],

            transactions: (0..num_threads)
                .map(|_| Vec::with_capacity(target_num_transactions_per_batch))
                .collect(),
            max_ages: vec![Vec::with_capacity(target_num_transactions_per_batch); num_threads],
            total_cus: vec![0; num_threads],
        }
    }

    pub fn take_batch(
        &mut self,
        thread_id: ThreadId,
        target_num_transactions_per_batch: usize,
    ) -> (Vec<TransactionId>, Vec<Tx>, Vec<MaxAge>, u64) {
        (
            core::mem::replace(
                &mut self.ids[thread_id],
                Vec::with_capacity(target_num_transactions_per_batch),
            ),
            core::mem::replace(
                &mut self.transactions[thread_id],
                Vec::with_capacity(target_num_transactions_per_batch),
            ),
            core::mem::replace(
                &mut self.max_ages[thread_id],
                Vec::with_capacity(target_num_transactions_per_batch),
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
pub(crate) struct SchedulingCommon<Tx> {
    pub(crate) consume_work_senders: Vec<Sender<ConsumeWork<Tx>>>,
    pub(crate) finished_consume_work_receiver: Receiver<FinishedConsumeWork<Tx>>,
    pub(crate) in_flight_tracker: InFlightTracker,
    pub(crate) account_locks: ThreadAwareAccountLocks,
}

impl<Tx> SchedulingCommon<Tx> {
    pub fn new(
        consume_work_senders: Vec<Sender<ConsumeWork<Tx>>>,
        finished_consume_work_receiver: Receiver<FinishedConsumeWork<Tx>>,
    ) -> Self {
        let num_threads = consume_work_senders.len();
        assert!(num_threads > 0, "must have at least one worker");
        assert!(
            num_threads <= MAX_THREADS,
            "cannot have more than {MAX_THREADS} workers"
        );
        Self {
            consume_work_senders,
            finished_consume_work_receiver,
            in_flight_tracker: InFlightTracker::new(num_threads),
            account_locks: ThreadAwareAccountLocks::new(num_threads),
        }
    }

    /// Send a batch of transactions to the given thread's `ConsumeWork` channel.
    /// Returns the number of transactions sent.
    pub fn send_batch(
        &mut self,
        batches: &mut Batches<Tx>,
        thread_index: usize,
        target_transactions_per_batch: usize,
    ) -> Result<usize, SchedulerError> {
        if batches.ids[thread_index].is_empty() {
            return Ok(0);
        }

        let (ids, transactions, max_ages, total_cus) =
            batches.take_batch(thread_index, target_transactions_per_batch);

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
    pub fn send_batches(
        &mut self,
        batches: &mut Batches<Tx>,
        target_transactions_per_batch: usize,
    ) -> Result<usize, SchedulerError> {
        (0..self.consume_work_senders.len())
            .map(|thread_index| {
                self.send_batch(batches, thread_index, target_transactions_per_batch)
            })
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
                mut retryable_indexes,
            }) => {
                let num_transactions = ids.len();
                let num_retryable = retryable_indexes.len();

                // Free the locks
                self.complete_batch(batch_id, &transactions);

                // Retryable transactions should be inserted back into the container
                // Need to sort because recording failures lead to out-of-order indexes
                retryable_indexes.sort_unstable();
                let mut retryable_iter = retryable_indexes.iter().peekable();
                for (index, (id, transaction)) in izip!(ids, transactions).enumerate() {
                    if let Some(&&retryable_index) = retryable_iter.peek() {
                        if retryable_index == index {
                            container.retry_transaction(id, transaction);
                            retryable_iter.next();
                            continue;
                        }
                    }
                    container.remove_by_id(id);
                }

                debug_assert!(
                    retryable_iter.peek().is_none(),
                    "retryable indexes were not in order: {retryable_indexes:?}"
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
