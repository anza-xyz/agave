use {
    super::thread_aware_account_locks::{ThreadId, ThreadSet},
    crate::banking_stage::scheduler_messages::{MaxAge, TransactionId},
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
