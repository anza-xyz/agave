use {
    crate::{
        Scheduler, check_response::TransactionState, schedule::ExecutionBatch,
        transaction_container::TransactionContainer,
    },
    agave_scheduler_bindings::{
        ExecutionWorkerToPackMessage, processed_codes,
        worker_message_types::{ExecutionResponse, not_included_reasons},
    },
    agave_scheduling_utils::{
        responses_region::ExecutionResponsesPtr,
        thread_aware_account_locks::{ThreadAwareAccountLocks, ThreadId},
    },
    rts_alloc::Allocator,
};

const MAX_EXECUTION_RESPONSE_PACKETS_PER_ITERATION: usize = 1024;

impl Scheduler {
    pub(super) fn handle_execution_worker_responses(&mut self) {
        let mut received = 0usize;
        for worker in 0..self.workers.len() {
            while let Some(message) = self.workers[worker].worker_to_pack.try_read() {
                received = received.saturating_add(usize::from(message.batch.num_transactions));
                self.handle_execution_worker_response(worker, message);
                if received >= MAX_EXECUTION_RESPONSE_PACKETS_PER_ITERATION {
                    return;
                }
            }
        }
    }

    fn handle_execution_worker_response(
        &mut self,
        worker: ThreadId,
        message: ExecutionWorkerToPackMessage,
    ) {
        // SAFETY: the worker returns our batch with its metadata intact and no remaining accesses.
        let batch = unsafe {
            ExecutionBatch::from_sharable_transaction_batch_region(&message.batch, &self.allocator)
        };
        let mut estimated_cost = 0u64;
        let mut actual_cost = 0u64;
        let mut dropped = 0usize;
        if message.processed_code == processed_codes::PROCESSED {
            assert_eq!(
                usize::from(message.responses.num_transaction_responses),
                batch.len(),
                "execution worker invariant violated: response count must match batch length"
            );
            // SAFETY: processed messages transfer an initialized response allocation to us.
            let responses = unsafe {
                ExecutionResponsesPtr::from_transaction_response_region(
                    &message.responses,
                    &self.allocator,
                )
            };
            for ((_, id), response) in batch.iter().zip(responses.iter()) {
                let action = ExecutionResponseAction::from_response(response);
                if response.not_included_reason == not_included_reasons::NONE {
                    actual_cost = actual_cost.saturating_add(response.cost_units);
                    self.leader_slot_metrics.completed_transactions = self
                        .leader_slot_metrics
                        .completed_transactions
                        .saturating_add(1);
                } else if matches!(action, ExecutionResponseAction::Release) {
                    dropped = dropped.saturating_add(1);
                }
                estimated_cost = estimated_cost.saturating_add(complete_transaction(
                    id,
                    worker,
                    action,
                    &mut self.transactions,
                    &mut self.account_locks,
                    &mut self.deferred_execution,
                    &self.allocator,
                ));
            }
            // SAFETY: all responses have been consumed and the allocation is exclusively ours.
            unsafe { responses.free(&self.allocator) };
        } else {
            // Unprocessed messages have an undefined response region.
            let action = ExecutionResponseAction::from_processed_code(message.processed_code);
            if matches!(action, ExecutionResponseAction::Release) {
                dropped = dropped.saturating_add(batch.len());
            }
            for (_, id) in batch.iter() {
                estimated_cost = estimated_cost.saturating_add(complete_transaction(
                    id,
                    worker,
                    action,
                    &mut self.transactions,
                    &mut self.account_locks,
                    &mut self.deferred_execution,
                    &self.allocator,
                ));
            }
        }
        self.in_flight
            .complete_batch(worker, batch.len(), estimated_cost);
        self.leader_slot_metrics.dropped_transactions = self
            .leader_slot_metrics
            .dropped_transactions
            .saturating_add(dropped);
        self.scheduled_cost = self
            .scheduled_cost
            .saturating_sub(estimated_cost)
            .saturating_add(actual_cost);
        // SAFETY: transactions have been retained or freed; the returned container is exclusively ours.
        unsafe { batch.free() };
    }
}

fn complete_transaction(
    id: usize,
    worker: ThreadId,
    action: ExecutionResponseAction,
    transactions: &mut TransactionContainer<TransactionState>,
    account_locks: &mut ThreadAwareAccountLocks,
    deferred_execution: &mut Vec<usize>,
    allocator: &Allocator,
) -> u64 {
    let transaction = transactions.get_mut(id).unwrap();
    assert_eq!(transaction.execution_worker.take(), Some(worker));
    let cost = transaction.cost;
    let writable = transaction.transaction.writable_accounts();
    let readonly = transaction.transaction.readonly_accounts();
    let account_keys = writable
        .map(|key| (key, true))
        .chain(readonly.map(|key| (key, false)));
    account_locks.unlock_accounts(account_keys, worker);
    match action {
        ExecutionResponseAction::RequeueInCurrentSlot => {
            transactions.requeue(id);
        }
        ExecutionResponseAction::DeferToNextSlot => deferred_execution.push(id),
        ExecutionResponseAction::Release => {
            let transaction = transactions.remove(id).unwrap();
            // SAFETY: the worker and scheduler have released all references to this transaction.
            unsafe { transaction.transaction.free(allocator) };
        }
    }
    cost
}

/// What happens to a transaction once its execution batch returns.
#[derive(Clone, Copy)]
enum ExecutionResponseAction {
    RequeueInCurrentSlot,
    DeferToNextSlot,
    Release,
}

impl ExecutionResponseAction {
    fn from_response(response: &ExecutionResponse) -> Self {
        match response.not_included_reason {
            not_included_reasons::BANK_NOT_AVAILABLE | not_included_reasons::ACCOUNT_IN_USE => {
                Self::RequeueInCurrentSlot
            }
            not_included_reasons::WOULD_EXCEED_MAX_BLOCK_COST_LIMIT
            | not_included_reasons::WOULD_EXCEED_MAX_VOTE_COST_LIMIT
            | not_included_reasons::WOULD_EXCEED_MAX_ACCOUNT_COST_LIMIT
            | not_included_reasons::WOULD_EXCEED_ACCOUNT_DATA_BLOCK_LIMIT => Self::DeferToNextSlot,
            _ => Self::Release,
        }
    }

    fn from_processed_code(code: u8) -> Self {
        match code {
            processed_codes::MAX_WORKING_SLOT_EXCEEDED => Self::RequeueInCurrentSlot,
            _ => Self::Release,
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            progress_tracker::SchedulerState,
            tests::{insert, setup_with_workers},
        },
        agave_scheduler_bindings::{
            ExecutionResponseRegion, SharableTransactionBatchRegion,
            worker_message_types::execution_statuses,
        },
        agave_scheduling_utils::{
            responses_region::execution_responses_from_iter, thread_aware_account_locks::ThreadSet,
        },
        core::time::Duration,
        solana_pubkey::Pubkey,
        std::time::Instant,
    };

    fn batch(
        scheduler: &mut Scheduler,
        worker: usize,
        ids: &[usize],
    ) -> SharableTransactionBatchRegion {
        let mut batch = ExecutionBatch::allocate(&scheduler.allocator).unwrap();
        let mut cost = 0u64;
        for &id in ids {
            let priority_id = *scheduler
                .transactions
                .iter_by_priority()
                .find(|entry| entry.id == id)
                .unwrap();
            scheduler.transactions.dequeue(&priority_id);
            let state = scheduler.transactions.get_mut(id).unwrap();
            state.execution_worker = Some(worker);
            let writable = state.transaction.writable_accounts();
            let readonly = state.transaction.readonly_accounts();
            let account_keys = writable
                .map(|key| (key, true))
                .chain(readonly.map(|key| (key, false)));
            scheduler
                .account_locks
                .try_lock_accounts(
                    account_keys,
                    ThreadSet::any(scheduler.workers.len()),
                    |_| worker,
                )
                .unwrap();
            // SAFETY: the retained transaction belongs to this allocator.
            let region = unsafe { state.transaction.to_region(&scheduler.allocator) };
            // SAFETY: the initialized transaction remains retained until its response is handled.
            unsafe { batch.try_push(region, id) }.unwrap();
            cost = cost.saturating_add(state.cost);
        }
        scheduler.in_flight.track_batch(worker, ids.len(), cost);
        scheduler.scheduled_cost = scheduler.scheduled_cost.saturating_add(cost);
        batch.to_sharable_transaction_batch_region()
    }

    fn response(reason: u8, cost_units: u64) -> ExecutionResponse {
        ExecutionResponse {
            execution_slot: 100,
            not_included_reason: reason,
            execution_status: if reason == not_included_reasons::NONE {
                execution_statuses::SUCCESS
            } else {
                execution_statuses::NOT_INCLUDED
            },
            cost_units,
            fee_payer_balance: 0,
        }
    }

    #[test]
    fn completes_and_retries_returned_transactions() {
        let (mut scheduler, mut agave) = setup_with_workers(2, 2);
        scheduler.handle_slot_change();
        let included = insert(&mut scheduler, 4, 100, &[1]);
        let immediate = insert(&mut scheduler, 3, 100, &[2]);
        let deferred = insert(&mut scheduler, 2, 100, &[3]);
        let rejected = insert(&mut scheduler, 1, 100, &[4]);
        let batch = batch(
            &mut scheduler,
            0,
            &[included, immediate, deferred, rejected],
        );
        let responses = execution_responses_from_iter(
            &scheduler.allocator,
            [
                response(not_included_reasons::NONE, 60),
                response(not_included_reasons::ACCOUNT_IN_USE, 0),
                response(not_included_reasons::WOULD_EXCEED_MAX_ACCOUNT_COST_LIMIT, 0),
                response(not_included_reasons::BLOCKHASH_NOT_FOUND, 0),
            ]
            .into_iter(),
        )
        .unwrap();
        agave.workers[0]
            .worker_to_pack
            .try_write(ExecutionWorkerToPackMessage {
                batch,
                responses,
                processed_code: processed_codes::PROCESSED,
            })
            .unwrap();
        scheduler.handle_execution_worker_responses();

        assert!(scheduler.in_flight.is_empty());
        assert_eq!(scheduler.in_flight.worker_load(0).cost_units, 0);
        assert_eq!(scheduler.scheduled_cost, 60);
        assert!(scheduler.transactions.get(included).is_none());
        assert!(scheduler.transactions.get(rejected).is_none());
        assert_eq!(
            scheduler
                .transactions
                .get(immediate)
                .unwrap()
                .execution_worker,
            None
        );
        assert_eq!(
            scheduler
                .transactions
                .get(deferred)
                .unwrap()
                .execution_worker,
            None
        );
        assert_eq!(
            scheduler
                .transactions
                .iter_by_priority()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            [immediate]
        );
        assert_eq!(scheduler.deferred_execution, [deferred]);
        let keys = [1, 2, 3, 4].map(|key| Pubkey::from([key; 32]));
        scheduler
            .account_locks
            .try_lock_accounts(
                keys.iter().map(|key| (key, true)),
                ThreadSet::any(2),
                |eligible| {
                    assert!(eligible.contains(1));
                    1
                },
            )
            .unwrap();
        scheduler
            .account_locks
            .unlock_accounts(keys.iter().map(|key| (key, true)), 1);

        scheduler.handle_slot_change();
        assert_eq!(scheduler.deferred_execution, [deferred]);
        scheduler.state = SchedulerState::LeaderReady {
            slot: 101,
            remaining_cost_units: 1_000,
            slot_start: Instant::now(),
            slot_duration: Duration::from_millis(400),
        };
        scheduler.handle_slot_change();
        assert!(scheduler.deferred_execution.is_empty());
        assert_eq!(scheduler.scheduled_cost, 0);
        assert_eq!(scheduler.transactions.pop_highest(), Some(immediate));
        assert_eq!(scheduler.transactions.pop_highest(), Some(deferred));
    }

    #[test]
    fn unprocessed_batches_release_workers_without_reading_response_regions() {
        let (mut scheduler, mut agave) = setup_with_workers(2, 2);
        let retry = insert(&mut scheduler, 2, 100, &[1]);
        let invalid = insert(&mut scheduler, 1, 100, &[2]);
        for (worker, id, processed_code) in [
            (0, retry, processed_codes::MAX_WORKING_SLOT_EXCEEDED),
            (1, invalid, processed_codes::INVALID),
        ] {
            let batch = batch(&mut scheduler, worker, &[id]);
            agave.workers[worker]
                .worker_to_pack
                .try_write(ExecutionWorkerToPackMessage {
                    batch,
                    processed_code,
                    responses: ExecutionResponseRegion {
                        num_transaction_responses: 0,
                        transaction_responses_offset: usize::MAX,
                    },
                })
                .unwrap();
        }
        scheduler.handle_execution_worker_responses();
        assert!(scheduler.in_flight.is_empty());
        assert_eq!(scheduler.scheduled_cost, 0);
        assert_eq!(scheduler.transactions.pop_highest(), Some(retry));
        assert!(scheduler.transactions.get(invalid).is_none());
        assert!(scheduler.deferred_execution.is_empty());
    }
}
