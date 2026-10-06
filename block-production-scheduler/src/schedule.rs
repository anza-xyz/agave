use {
    crate::{
        Scheduler, check_response::TransactionState, in_flight_tracker::InFlightTracker,
        progress_tracker::SchedulerState,
    },
    agave_scheduler_bindings::PackToExecutionWorkerMessage,
    agave_scheduler_handshake::ClientWorkerSession,
    agave_scheduling_utils::{
        ENTRY_OVERHEAD_BYTES,
        thread_aware_account_locks::{MAX_THREADS, ThreadAwareAccountLocks, ThreadId, ThreadSet},
        transaction_priority_queue::TransactionPriorityId,
        transaction_ptr::TransactionPtrBatch,
    },
    rts_alloc::Allocator,
    solana_clock::Slot,
};

const MAX_TRANSACTIONS_PER_BATCH: usize = 16;

impl Scheduler {
    pub(super) fn schedule(&mut self, mut budget: u64) {
        let SchedulerState::LeaderReady { slot, .. } = self.state else {
            return;
        };
        if budget == 0 {
            return;
        }
        self.scheduling_scratch.reset();
        let scheduled = &mut self.scheduling_scratch.scheduled;
        let mut allowed_workers = allowed_workers(
            &self.workers,
            &self.in_flight,
            self.max_cost_units_per_worker,
        );
        if allowed_workers.is_empty() {
            return;
        }
        let mut batches = ExecutionBatches::new(
            &mut allowed_workers,
            &self.allocator,
            &mut self.workers,
            &mut self.in_flight,
            slot,
            self.max_cost_units_per_batch,
            self.max_cost_units_per_worker,
            self.target_entry_bytes_per_batch,
        );

        // Most candidates may be blocked by account locks. Scan without modifying the queue,
        // collecting successful assignments in scratch. Dequeue only those IDs afterward,
        // avoiding pop/requeue work for deferred transactions.
        for &id in self.transactions.iter_by_priority() {
            if budget == 0 || allowed_workers.is_empty() {
                break;
            }
            let transaction = self.transactions.get(id.id).unwrap();
            let Some(worker) = try_schedule_transaction(
                id.id,
                transaction,
                &mut self.account_locks,
                &mut batches,
                &mut allowed_workers,
            ) else {
                continue;
            };
            self.scheduled_cost = self.scheduled_cost.saturating_add(transaction.cost);
            budget = budget.saturating_sub(transaction.cost);
            scheduled.push((id, worker));
        }

        batches.send_all(&mut ThreadSet::none());
        for (id, worker) in scheduled.drain(..) {
            self.transactions.dequeue(&id);
            self.transactions.get_mut(id.id).unwrap().execution_worker = Some(worker);
        }
    }
}

fn try_schedule_transaction(
    id: usize,
    transaction: &TransactionState,
    account_locks: &mut ThreadAwareAccountLocks,
    batches: &mut ExecutionBatches<'_>,
    allowed_workers: &mut ThreadSet,
) -> Option<ThreadId> {
    let writable = transaction.transaction.writable_accounts();
    let readonly = transaction.transaction.readonly_accounts();
    let account_keys = writable
        .map(|key| (key, true))
        .chain(readonly.map(|key| (key, false)));
    let worker = account_locks
        .try_lock_accounts(account_keys.clone(), *allowed_workers, |eligible| {
            select_worker(eligible, batches)
        })
        .ok()?;
    if !batches.try_push(worker, id, transaction, allowed_workers) {
        account_locks.unlock_accounts(account_keys, worker);
        return None;
    }
    Some(worker)
}

fn allowed_workers(
    workers: &[ClientWorkerSession],
    in_flight: &InFlightTracker,
    max_cost_units_per_worker: u64,
) -> ThreadSet {
    let mut allowed_workers = ThreadSet::any(workers.len());
    for (worker, session) in workers.iter().enumerate() {
        let load = in_flight.worker_load(worker);
        if load.batches >= session.pack_to_worker.capacity()
            || load.cost_units >= max_cost_units_per_worker
        {
            allowed_workers.remove(worker);
        }
    }
    allowed_workers
}

fn select_worker(eligible: ThreadSet, batches: &ExecutionBatches<'_>) -> ThreadId {
    eligible
        .contained_threads_iter()
        .min_by_key(|&worker| {
            let load = batches.in_flight.worker_load(worker);
            let (pending_cost, pending_transactions) = batches.pending_load(worker);
            (
                load.cost_units.saturating_add(pending_cost),
                load.transactions.saturating_add(pending_transactions),
            )
        })
        .unwrap()
}

// Metadata stays in shared memory so the response identifies the retained transactions.
pub(super) type ExecutionBatch<'a> = TransactionPtrBatch<'a, usize, MAX_TRANSACTIONS_PER_BATCH>;

/// Temporary scheduling state, reused without allocating on each pass.
pub(super) struct SchedulerScratch {
    /// Selected transaction IDs and worker assignments; capacity is retained across passes.
    scheduled: Vec<(TransactionPriorityId, ThreadId)>,
}

impl SchedulerScratch {
    pub(super) fn new(transaction_capacity: usize) -> Self {
        Self {
            scheduled: Vec::with_capacity(transaction_capacity),
        }
    }

    fn reset(&mut self) {
        self.scheduled.clear();
    }
}

struct WorkerBatch<'a> {
    batch: Option<ExecutionBatch<'a>>,
    cost_units: u64,
    entry_bytes: u64,
}

impl Default for WorkerBatch<'_> {
    fn default() -> Self {
        Self {
            batch: None,
            cost_units: 0,
            entry_bytes: ENTRY_OVERHEAD_BYTES,
        }
    }
}

impl WorkerBatch<'_> {
    fn len(&self) -> usize {
        self.batch.as_ref().map_or(0, ExecutionBatch::len)
    }
}

impl Drop for WorkerBatch<'_> {
    fn drop(&mut self) {
        if let Some(batch) = self.batch.take() {
            // SAFETY: this unpublished container is exclusively ours. Transactions stay owned
            // by the scheduler; only the container is freed here.
            unsafe { batch.free() };
        }
    }
}

/// Builds and publishes execution batches during a single scheduling pass.
struct ExecutionBatches<'a> {
    // ThreadSet yields worker IDs below MAX_THREADS, so each ID indexes this array.
    pending_worker_batches: [WorkerBatch<'a>; MAX_THREADS],
    allocator: &'a Allocator,
    workers: &'a mut [ClientWorkerSession],
    in_flight: &'a mut InFlightTracker,
    slot: Slot,
    max_cost_units_per_batch: u64,
    max_cost_units_per_worker: u64,
    target_entry_bytes_per_batch: u64,
}

impl<'a> ExecutionBatches<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        allowed_workers: &mut ThreadSet,
        allocator: &'a Allocator,
        workers: &'a mut [ClientWorkerSession],
        in_flight: &'a mut InFlightTracker,
        slot: Slot,
        max_cost_units_per_batch: u64,
        max_cost_units_per_worker: u64,
        target_entry_bytes_per_batch: u64,
    ) -> Self {
        let mut pending_worker_batches = core::array::from_fn(|_| WorkerBatch::default());
        for worker in allowed_workers.contained_threads_iter() {
            pending_worker_batches[worker].batch = ExecutionBatch::allocate(allocator);
            if pending_worker_batches[worker].batch.is_none() {
                allowed_workers.remove(worker);
            }
        }
        Self {
            pending_worker_batches,
            allocator,
            workers,
            in_flight,
            slot,
            max_cost_units_per_batch,
            max_cost_units_per_worker,
            target_entry_bytes_per_batch,
        }
    }

    fn pending_load(&self, worker: ThreadId) -> (u64, usize) {
        let pending = &self.pending_worker_batches[worker];
        (pending.cost_units, pending.len())
    }

    fn should_send_before(&self, worker: ThreadId, transaction_bytes: u64) -> bool {
        self.pending_worker_batches[worker]
            .entry_bytes
            .saturating_add(transaction_bytes)
            > self.target_entry_bytes_per_batch
    }

    /// Prepares and appends a transaction, returning false if the worker becomes unavailable.
    fn try_push(
        &mut self,
        worker: ThreadId,
        id: usize,
        transaction: &TransactionState,
        allowed_workers: &mut ThreadSet,
    ) -> bool {
        let transaction_bytes = transaction.transaction.view.data().len() as u64;
        if self.should_send_before(worker, transaction_bytes) {
            self.send_all(allowed_workers);
        }
        if !allowed_workers.contains(worker) {
            return false;
        }
        let pending = &mut self.pending_worker_batches[worker];
        let batch = pending.batch.as_mut().expect("batch has been prepared");
        // SAFETY: the scheduler retains this allocation until execution completes.
        let region = unsafe { transaction.transaction.to_region(self.allocator) };
        // SAFETY: the region belongs to this allocator and remains retained by the scheduler.
        unsafe { batch.try_push(region, id) }.expect("full batches are sent immediately");
        pending.cost_units = pending.cost_units.saturating_add(transaction.cost);
        pending.entry_bytes = pending.entry_bytes.saturating_add(u64::from(region.length));

        if self.should_send(worker) {
            // Match greedy: reaching a batch target sends all pending batches.
            self.send_all(allowed_workers);
        }
        if self.worker_at_capacity(worker) {
            allowed_workers.remove(worker);
        }
        true
    }

    fn should_send(&self, worker: ThreadId) -> bool {
        let pending = &self.pending_worker_batches[worker];
        pending.len() == MAX_TRANSACTIONS_PER_BATCH
            || pending.cost_units >= self.max_cost_units_per_batch
            || pending.entry_bytes >= self.target_entry_bytes_per_batch
    }

    fn worker_at_capacity(&self, worker: ThreadId) -> bool {
        let load = self.in_flight.worker_load(worker);
        let queue_at_capacity = load.batches >= self.workers[worker].pack_to_worker.capacity();
        let cost_at_capacity = load
            .cost_units
            .saturating_add(self.pending_worker_batches[worker].cost_units)
            >= self.max_cost_units_per_worker;

        queue_at_capacity || cost_at_capacity
    }

    /// Publishes the current batch.
    fn send(&mut self, worker: ThreadId) {
        let pending = &mut self.pending_worker_batches[worker];
        let Some(batch) = pending.batch.take() else {
            return;
        };
        let cost = core::mem::take(&mut pending.cost_units);
        pending.entry_bytes = ENTRY_OVERHEAD_BYTES;
        let count = batch.len();
        if batch.is_empty() {
            // SAFETY: this unpublished container contains no transactions.
            unsafe { batch.free() };
            return;
        }
        let queue = &mut self.workers[worker].pack_to_worker;
        queue
            .try_write(PackToExecutionWorkerMessage {
                flags: 0,
                max_working_slot: self.slot,
                batch: batch.to_sharable_transaction_batch_region(),
            })
            .expect("outstanding batch limit guarantees worker queue capacity");
        self.in_flight.track_batch(worker, count, cost);
    }

    fn send_all(&mut self, allowed_workers: &mut ThreadSet) {
        for worker in 0..self.workers.len() {
            if self.pending_worker_batches[worker].len() == 0 {
                continue;
            }
            self.send(worker);
            if !allowed_workers.contains(worker) {
                continue;
            }
            if self.worker_at_capacity(worker) {
                allowed_workers.remove(worker);
                continue;
            }
            self.pending_worker_batches[worker].batch = ExecutionBatch::allocate(self.allocator);
            if self.pending_worker_batches[worker].batch.is_none() {
                allowed_workers.remove(worker);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::tests::{insert, setup_with_workers},
        agave_scheduler_handshake::AgaveSession,
        core::time::Duration,
        solana_pubkey::Pubkey,
        std::time::Instant,
    };

    fn setup() -> (Scheduler, AgaveSession) {
        let (mut scheduler, agave) = setup_with_workers(2, 2);
        let start = Instant::now();
        scheduler.state = SchedulerState::LeaderReady {
            slot: 100,
            remaining_cost_units: 1_000,
            slot_start: start,
            slot_duration: Duration::from_millis(400),
        };
        scheduler.handle_slot_change();
        (scheduler, agave)
    }

    fn assert_batch(
        scheduler: &mut Scheduler,
        agave: &mut AgaveSession,
        worker: usize,
        expected: &[usize],
    ) {
        let message = agave.workers[worker].pack_to_worker.try_read().unwrap();
        assert_eq!(message.max_working_slot, 100);
        assert_eq!(message.flags, 0);
        // SAFETY: the scheduler published this initialized batch, now exclusively held here.
        let batch = unsafe {
            ExecutionBatch::from_sharable_transaction_batch_region(
                &message.batch,
                &scheduler.allocator,
            )
        };
        assert_eq!(batch.iter().map(|(_, id)| id).collect::<Vec<_>>(), expected);
        for (transaction, id) in batch.iter() {
            let state = scheduler.transactions.get_mut(id).unwrap();
            assert_eq!(state.execution_worker, Some(worker));
            // SAFETY: both pointers belong to the scheduler allocator and are still retained.
            let region =
                unsafe { transaction.to_sharable_transaction_region(&scheduler.allocator) };
            // SAFETY: this retained transaction belongs to the scheduler allocator.
            let expected_region = unsafe { state.transaction.to_region(&scheduler.allocator) };
            assert_eq!(region.offset, expected_region.offset);
            state.execution_worker = None;
        }
        // SAFETY: the test worker is done; only the container is freed, not retained transactions.
        unsafe { batch.free() };
    }

    #[test]
    fn dispatches_in_priority_order_balancing_cost_then_count() {
        let (mut scheduler, mut agave) = setup();
        let last = insert(&mut scheduler, 1, 0, &[4]);
        let third = insert(&mut scheduler, 2, 100, &[3]);
        let second = insert(&mut scheduler, 3, 100, &[2]);
        let first = insert(&mut scheduler, 4, 200, &[1]);
        scheduler.schedule(1_000);
        assert_eq!(scheduler.scheduled_cost, 400);
        assert_eq!(scheduler.in_flight.worker_load(0).transactions, 2);
        assert_eq!(scheduler.in_flight.worker_load(0).cost_units, 200);
        assert_eq!(scheduler.in_flight.worker_load(1).transactions, 2);
        assert!(scheduler.transactions.pop_highest().is_none());
        assert_batch(&mut scheduler, &mut agave, 0, &[first, last]);
        assert_batch(&mut scheduler, &mut agave, 1, &[second, third]);
    }

    #[test]
    fn flushes_at_transaction_and_cost_limits() {
        for (cost_limit, expected_count) in [(u64::MAX, 16), (15, 2), (20, 2)] {
            let (mut scheduler, mut agave) = setup();
            scheduler.max_cost_units_per_batch = cost_limit;
            let ids: Vec<_> = (0..=MAX_TRANSACTIONS_PER_BATCH * 2)
                .map(|_| insert(&mut scheduler, 1, 10, &[1]))
                .collect();
            let queue_capacity = scheduler.workers[0].pack_to_worker.capacity();
            let sent_count = ids.len().min(queue_capacity * expected_count);
            scheduler.schedule(1_000);
            assert_eq!(
                scheduler.in_flight.worker_load(0).batches,
                sent_count.div_ceil(expected_count)
            );
            assert_eq!(scheduler.in_flight.worker_load(0).transactions, sent_count);
            assert_eq!(scheduler.scheduled_cost, sent_count as u64 * 10);
            assert_eq!(
                scheduler.transactions.pop_highest(),
                ids.get(sent_count).copied()
            );
            for expected in ids[..sent_count].chunks(expected_count) {
                assert_batch(&mut scheduler, &mut agave, 0, expected);
            }
            assert!(agave.workers[0].pack_to_worker.try_read().is_none());
            assert!(agave.workers[1].pack_to_worker.try_read().is_none());
        }
    }

    #[test]
    fn sends_after_transaction_crosses_batch_cost_target() {
        let (mut scheduler, mut agave) = setup();
        scheduler.max_cost_units_per_batch = 20;
        let first = insert(&mut scheduler, 3, 10, &[1]);
        let oversized = insert(&mut scheduler, 2, 30, &[1]);
        let last = insert(&mut scheduler, 1, 10, &[1]);
        scheduler.schedule(1_000);
        assert_eq!(scheduler.scheduled_cost, 50);
        assert_eq!(scheduler.in_flight.worker_load(0).batches, 2);
        assert!(scheduler.transactions.pop_highest().is_none());
        assert_batch(&mut scheduler, &mut agave, 0, &[first, oversized]);
        assert_batch(&mut scheduler, &mut agave, 0, &[last]);
        assert!(agave.workers[0].pack_to_worker.try_read().is_none());
    }

    #[test]
    fn reaching_batch_target_sends_other_workers_partial_batches() {
        let (mut scheduler, mut agave) = setup();
        scheduler.max_cost_units_per_batch = 200;
        let first = insert(&mut scheduler, 4, 100, &[1]);
        let second = insert(&mut scheduler, 3, 100, &[2]);
        let third = insert(&mut scheduler, 2, 100, &[3]);
        let fourth = insert(&mut scheduler, 1, 50, &[4]);
        scheduler.schedule(1_000);
        assert_batch(&mut scheduler, &mut agave, 0, &[first, third]);
        assert_batch(&mut scheduler, &mut agave, 1, &[second]);
        assert_batch(&mut scheduler, &mut agave, 1, &[fourth]);
        assert!(agave.workers[0].pack_to_worker.try_read().is_none());
        assert!(agave.workers[1].pack_to_worker.try_read().is_none());
    }

    #[test]
    fn releases_locks_when_flushing_makes_worker_unavailable() {
        let (mut scheduler, mut agave) = setup();
        let queue_capacity = scheduler.workers[0].pack_to_worker.capacity();
        // Leave room for two more outstanding batches before the worker becomes unavailable.
        for _ in 0..queue_capacity - 2 {
            scheduler.in_flight.track_batch(0, 0, 0);
        }
        let first = insert(&mut scheduler, 3, 1, &[1]);
        let transaction_bytes = scheduler
            .transactions
            .get(first)
            .unwrap()
            .transaction
            .view
            .data()
            .len() as u64;
        scheduler.target_entry_bytes_per_batch = ENTRY_OVERHEAD_BYTES + transaction_bytes + 1;
        let second = insert(&mut scheduler, 2, 1, &[1]);
        let deferred = insert(&mut scheduler, 1, 1, &[1, 3]);

        scheduler.schedule(1_000);

        assert_eq!(scheduler.scheduled_cost, 2);
        assert_eq!(scheduler.transactions.pop_highest(), Some(deferred));
        assert_eq!(
            scheduler
                .transactions
                .get(deferred)
                .unwrap()
                .execution_worker,
            None
        );
        assert_batch(&mut scheduler, &mut agave, 0, &[first]);
        assert_batch(&mut scheduler, &mut agave, 0, &[second]);
        let account = Pubkey::from([3; 32]);
        assert_eq!(
            scheduler.account_locks.try_lock_accounts(
                [(&account, true)].into_iter(),
                ThreadSet::any(2),
                |_| 1,
            ),
            Ok(1)
        );
    }

    #[test]
    fn batches_by_serialized_entry_size() {
        for (transactions_per_target, below_target, expected_count) in
            [(2, 0, 2), (2, 1, 1), (1, 1, 1)]
        {
            let (mut scheduler, mut agave) = setup();
            let first = insert(&mut scheduler, 3, 1, &[1]);
            let transaction_bytes = scheduler
                .transactions
                .get(first)
                .unwrap()
                .transaction
                .view
                .data()
                .len() as u64;
            scheduler.target_entry_bytes_per_batch =
                ENTRY_OVERHEAD_BYTES + transaction_bytes * transactions_per_target - below_target;
            let second = insert(&mut scheduler, 2, 1, &[1]);
            let third = insert(&mut scheduler, 1, 1, &[1]);
            scheduler.schedule(1_000);
            assert_eq!(scheduler.scheduled_cost, 3);
            assert!(scheduler.transactions.pop_highest().is_none());
            for expected in [first, second, third].chunks(expected_count) {
                assert_batch(&mut scheduler, &mut agave, 0, expected);
            }
            assert!(agave.workers[0].pack_to_worker.try_read().is_none());
        }
    }

    #[test]
    fn defers_conflicts() {
        let (mut scheduler, mut agave) = setup();
        let first = insert(&mut scheduler, 4, 100, &[1]);
        let second = insert(&mut scheduler, 3, 100, &[2]);
        let blocked = insert(&mut scheduler, 2, 100, &[1, 2]);
        let same_worker = insert(&mut scheduler, 1, 100, &[1]);
        scheduler.schedule(1_000);
        scheduler.schedule(1_000);
        assert_eq!(scheduler.scheduled_cost, 300);
        assert_eq!(scheduler.transactions.pop_highest(), Some(blocked));
        assert!(scheduler.transactions.pop_highest().is_none());
        assert_batch(&mut scheduler, &mut agave, 0, &[first, same_worker]);
        assert_batch(&mut scheduler, &mut agave, 1, &[second]);
    }

    #[test]
    fn limits_worker_cost_including_pending_and_published_batches() {
        let (mut scheduler, mut agave) = setup();
        scheduler.max_cost_units_per_worker = 150;
        let first = insert(&mut scheduler, 3, 100, &[1]);
        let second = insert(&mut scheduler, 2, 60, &[1]);
        let deferred = insert(&mut scheduler, 1, 50, &[1]);
        scheduler.schedule(1_000);
        scheduler.schedule(1_000);
        assert_eq!(scheduler.in_flight.worker_load(0).cost_units, 160);
        assert_eq!(scheduler.scheduled_cost, 160);
        assert_eq!(scheduler.transactions.pop_highest(), Some(deferred));
        assert_batch(&mut scheduler, &mut agave, 0, &[first, second]);
        assert!(agave.workers[1].pack_to_worker.try_read().is_none());
    }

    #[test]
    fn allows_last_transaction_to_cross_supplied_budget() {
        let (mut scheduler, mut agave) = setup();
        let first = insert(&mut scheduler, 2, 300, &[2]);
        let blocked = insert(&mut scheduler, 1, 101, &[2]);
        scheduler.schedule(0);
        assert!(scheduler.in_flight.is_empty());
        scheduler.schedule(1);
        assert_eq!(scheduler.scheduled_cost, 300);
        assert_eq!(scheduler.transactions.pop_highest(), Some(blocked));
        assert!(agave.workers[1].pack_to_worker.try_read().is_none());
        assert_batch(&mut scheduler, &mut agave, 0, &[first]);
    }
}
