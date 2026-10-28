use {
    super::*,
    crate::schedule::ExecutionBatch,
    agave_scheduler_bindings::{
        ExecutionResponseRegion, ExecutionWorkerToPackMessage, LEADER_READY,
        PackToExecutionWorkerMessage, SharablePubkeys, check_message_flags, processed_codes,
        worker_message_types::{
            CheckResponse, ExecutionResponse, age_nonce_check_flags, execution_statuses,
            fee_payer_balance_flags, not_included_reasons, resolve_flags, scheduling_details_flags,
        },
    },
    agave_scheduling_utils::responses_region::{
        execution_responses_from_iter, resolve_responses_from_iter,
    },
};

const ESTIMATED_COST: u64 = 100;
const ACTUAL_COST: u64 = 60;

fn leader_ready(agave: &mut AgaveSession, slot: u64) {
    agave
        .progress_tracker
        .try_write(ProgressMessage {
            leader_state: LEADER_READY,
            // Release the full pacing budget without sleeping.
            current_slot_progress: 100,
            epoch: 0,
            current_slot: slot,
            next_leader_slot: u64::MAX,
            leader_range_end: slot,
            remaining_cost_units: 10_000,
            remaining_allocated_accounts_data_size: 0,
            latest_blockhash: [0; 32],
            target_bank_time_ms: 400,
        })
        .unwrap();
}

fn enqueue(agave: &mut AgaveSession, keys: &[u8]) -> SharableTransactionRegion {
    let bytes = wincode::serialize(&VersionedTransaction {
        signatures: vec![Signature::default()],
        message: VersionedMessage::Legacy(Message {
            header: MessageHeader {
                num_required_signatures: 1,
                ..Default::default()
            },
            account_keys: keys.iter().map(|&key| Pubkey::from([key; 32])).collect(),
            ..Default::default()
        }),
    })
    .unwrap();
    let allocator = &agave.tpu_to_pack.allocator;
    let ptr = allocator.allocate(bytes.len() as u32).unwrap();
    // SAFETY: the fresh allocation has space for the serialized transaction and does not overlap it.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.as_ptr(), bytes.len()) };
    let transaction = SharableTransactionRegion {
        // SAFETY: the pointer belongs to the TPU allocator.
        offset: unsafe { allocator.offset(ptr) },
        length: bytes.len() as u32,
    };
    agave
        .tpu_to_pack
        .producer
        .try_write(TpuToPackMessage {
            transaction,
            flags: 0,
            src_addr: [0; 16],
        })
        .unwrap();
    transaction
}

fn reply_checks(agave: &mut AgaveSession) {
    let worker = &agave.check_workers[0];
    let message = worker.pack_to_check_worker.try_read().unwrap();
    assert_eq!(
        message.flags,
        check_message_flags::AGE_NONCE_CHECKS
            | check_message_flags::LOAD_FEE_PAYER_BALANCE
            | check_message_flags::LOAD_ADDRESS_LOOKUP_TABLES
            | check_message_flags::CALCULATE_SCHEDULING_DETAILS
    );
    let response = CheckResponse {
        parsing_and_sanitization_flags: 0,
        status_cache_check_flags: 0,
        age_nonce_check_flags: age_nonce_check_flags::PERFORMED,
        fee_payer_balance_flags: fee_payer_balance_flags::PERFORMED,
        resolve_flags: resolve_flags::PERFORMED,
        scheduling_details_flags: scheduling_details_flags::PERFORMED,
        included_slot: 0,
        transaction_fee: 100,
        prioritization_fee: 0,
        estimated_cost_units: ESTIMATED_COST,
        allocated_accounts_data_size: 0,
        balance_slot: 100,
        fee_payer_balance: 1_000_000,
        resolution_slot: 100,
        min_alt_deactivation_slot: u64::MAX,
        resolved_pubkeys: SharablePubkeys {
            offset: 0,
            num_pubkeys: 0,
        },
    };
    let responses = resolve_responses_from_iter(
        &worker.allocator,
        (0..message.batch.num_transactions).map(|_| response),
    )
    .unwrap();
    worker
        .check_worker_to_pack
        .try_write(CheckWorkerToPackMessage {
            batch: message.batch,
            processed_code: processed_codes::PROCESSED,
            responses,
        })
        .unwrap();
}

fn receive_execution(
    agave: &mut AgaveSession,
    worker: usize,
    slot: u64,
    expected: &[SharableTransactionRegion],
) -> PackToExecutionWorkerMessage {
    let worker = &mut agave.workers[worker];
    let message = worker.pack_to_worker.try_read().unwrap();
    assert_eq!(message.max_working_slot, slot);
    // SAFETY: this worker owns the published batch until returning it to the scheduler.
    let batch = unsafe {
        ExecutionBatch::from_sharable_transaction_batch_region(&message.batch, &worker.allocator)
    };
    assert_eq!(batch.len(), expected.len());
    for (index, &region) in expected.iter().enumerate() {
        assert_eq!(batch.transaction_region(index), region);
    }
    message
}

fn reply_execution(
    agave: &mut AgaveSession,
    worker: usize,
    message: PackToExecutionWorkerMessage,
    reasons: &[u8],
) {
    assert_eq!(usize::from(message.batch.num_transactions), reasons.len());
    let worker = &mut agave.workers[worker];
    let responses = execution_responses_from_iter(
        &worker.allocator,
        reasons.iter().map(|&reason| ExecutionResponse {
            execution_slot: message.max_working_slot,
            not_included_reason: reason,
            execution_status: if reason == not_included_reasons::NONE {
                execution_statuses::SUCCESS
            } else {
                execution_statuses::NOT_INCLUDED
            },
            cost_units: if reason == not_included_reasons::NONE {
                ACTUAL_COST
            } else {
                0
            },
            fee_payer_balance: 0,
        }),
    )
    .unwrap();
    worker
        .worker_to_pack
        .try_write(ExecutionWorkerToPackMessage {
            batch: message.batch,
            processed_code: processed_codes::PROCESSED,
            responses,
        })
        .unwrap();
}

fn assert_finished(scheduler: &Scheduler, agave: &mut AgaveSession) {
    assert_eq!(scheduler.transactions.len(), 0);
    assert_eq!(scheduler.outstanding_check_packets, 0);
    assert_eq!(scheduler.outstanding_check_batches, 0);
    assert!(scheduler.in_flight.is_empty());
    assert!(scheduler.deferred_execution.is_empty());
    assert_eq!(scheduler.allocator.outstanding_allocation_bytes(), 0);
    // Each simulated Agave producer reclaims allocations remotely freed by the scheduler.
    agave.tpu_to_pack.allocator.clean_remote_frees();
    assert_eq!(
        agave.tpu_to_pack.allocator.outstanding_allocation_bytes(),
        0
    );
    for worker in &agave.check_workers {
        worker.allocator.clean_remote_frees();
        assert_eq!(worker.allocator.outstanding_allocation_bytes(), 0);
        assert!(worker.pack_to_check_worker.try_read().is_none());
    }
    for worker in &mut agave.workers {
        worker.allocator.clean_remote_frees();
        assert_eq!(worker.allocator.outstanding_allocation_bytes(), 0);
        assert!(worker.pack_to_worker.try_read().is_none());
    }
}

#[test]
fn balances_workers_and_releases_conflicting_accounts_on_completion() {
    let (mut scheduler, mut agave) = setup_with_workers(2, 2);
    leader_ready(&mut agave, 100);
    let first = enqueue(&mut agave, &[1]);
    let independent = enqueue(&mut agave, &[2]);
    let same_worker = enqueue(&mut agave, &[1, 3]);
    let blocked = enqueue(&mut agave, &[1, 2]);
    scheduler.run_iteration();
    reply_checks(&mut agave);
    scheduler.run_iteration();

    let worker_zero = receive_execution(&mut agave, 0, 100, &[first, same_worker]);
    let worker_one = receive_execution(&mut agave, 1, 100, &[independent]);
    scheduler.run_iteration();
    assert!(agave.workers[0].pack_to_worker.try_read().is_none());
    assert!(agave.workers[1].pack_to_worker.try_read().is_none());

    // Releasing account 2 lets the blocked transaction join account 1's worker.
    reply_execution(&mut agave, 1, worker_one, &[not_included_reasons::NONE]);
    scheduler.run_iteration();
    let unblocked = receive_execution(&mut agave, 0, 100, &[blocked]);
    assert!(agave.workers[1].pack_to_worker.try_read().is_none());
    assert_eq!(scheduler.in_flight.worker_load(0).transactions, 3);
    assert_eq!(scheduler.in_flight.worker_load(1).transactions, 0);

    reply_execution(&mut agave, 0, worker_zero, &[not_included_reasons::NONE; 2]);
    reply_execution(&mut agave, 0, unblocked, &[not_included_reasons::NONE]);
    scheduler.run_iteration();
    assert_eq!(scheduler.scheduled_cost, 4 * ACTUAL_COST);
    let metrics = &scheduler.leader_slot_metrics;
    assert_eq!(metrics.starting_transactions, 0);
    assert_eq!(metrics.received_packets, 4);
    assert_eq!(metrics.buffered_transactions, 4);
    assert_eq!(metrics.dropped_transactions, 0);
    assert_eq!(metrics.scheduled_transactions, 4);
    assert_eq!(metrics.completed_transactions, 4);
    assert_finished(&scheduler, &mut agave);
}

#[test]
fn ingress_to_execution_resumes_after_worker_capacity_is_released() {
    let (mut scheduler, mut agave) = setup(2);
    scheduler.max_cost_units_per_batch = ESTIMATED_COST;
    scheduler.max_cost_units_per_worker = 2 * ESTIMATED_COST;
    leader_ready(&mut agave, 100);
    let first = enqueue(&mut agave, &[1]);
    let second = enqueue(&mut agave, &[2]);
    let third = enqueue(&mut agave, &[3]);
    scheduler.run_iteration();
    assert_eq!(scheduler.outstanding_check_packets, 3);
    reply_checks(&mut agave);
    scheduler.run_iteration();
    assert_eq!(scheduler.outstanding_check_packets, 0);
    assert_eq!(scheduler.in_flight.worker_load(0).batches, 2);
    let first = receive_execution(&mut agave, 0, 100, &[first]);
    let second = receive_execution(&mut agave, 0, 100, &[second]);
    scheduler.run_iteration();
    assert!(agave.workers[0].pack_to_worker.try_read().is_none());

    reply_execution(&mut agave, 0, first, &[not_included_reasons::NONE]);
    scheduler.run_iteration();
    let third = receive_execution(&mut agave, 0, 100, &[third]);
    assert_eq!(scheduler.scheduled_cost, ACTUAL_COST + 2 * ESTIMATED_COST);
    reply_execution(&mut agave, 0, second, &[not_included_reasons::NONE]);
    reply_execution(&mut agave, 0, third, &[not_included_reasons::NONE]);
    scheduler.run_iteration();
    assert_eq!(scheduler.scheduled_cost, 3 * ACTUAL_COST);
    assert_finished(&scheduler, &mut agave);
}

#[test]
fn retries_immediately_or_on_slot_roll_without_mixing_slot_costs() {
    let (mut scheduler, mut agave) = setup(2);
    leader_ready(&mut agave, 100);
    let immediate = enqueue(&mut agave, &[1]);
    let deferred = enqueue(&mut agave, &[2]);
    let rejected = enqueue(&mut agave, &[3]);
    scheduler.run_iteration();
    reply_checks(&mut agave);
    scheduler.run_iteration();
    let batch = receive_execution(&mut agave, 0, 100, &[immediate, deferred, rejected]);
    reply_execution(
        &mut agave,
        0,
        batch,
        &[
            not_included_reasons::ACCOUNT_IN_USE,
            not_included_reasons::WOULD_EXCEED_MAX_ACCOUNT_COST_LIMIT,
            not_included_reasons::BLOCKHASH_NOT_FOUND,
        ],
    );
    scheduler.run_iteration();
    let retry = receive_execution(&mut agave, 0, 100, &[immediate]);
    let metrics = &scheduler.leader_slot_metrics;
    assert_eq!(metrics.scheduled_transactions, 4);
    assert_eq!(metrics.completed_transactions, 0);
    assert_eq!(metrics.dropped_transactions, 1);
    assert_eq!(scheduler.transactions.len(), 2);
    assert_eq!(scheduler.deferred_execution.len(), 1);
    assert_eq!(scheduler.scheduled_cost, ESTIMATED_COST);

    leader_ready(&mut agave, 101);
    scheduler.run_iteration();
    assert_eq!(scheduler.scheduling_slot, Some(100));
    assert_eq!(scheduler.leader_slot_metrics.slot, Some(100));
    assert!(agave.workers[0].pack_to_worker.try_read().is_none());
    reply_execution(&mut agave, 0, retry, &[not_included_reasons::NONE]);
    scheduler.run_iteration();
    let retry = receive_execution(&mut agave, 0, 101, &[deferred]);
    assert_eq!(scheduler.scheduling_slot, Some(101));
    assert_eq!(scheduler.scheduled_cost, ESTIMATED_COST);
    assert!(scheduler.deferred_execution.is_empty());
    let metrics = &scheduler.leader_slot_metrics;
    assert_eq!(metrics.slot, Some(101));
    assert_eq!(metrics.starting_transactions, 1);
    assert_eq!(metrics.completed_transactions, 0);
    reply_execution(&mut agave, 0, retry, &[not_included_reasons::NONE]);
    scheduler.run_iteration();
    assert_eq!(scheduler.scheduled_cost, ACTUAL_COST);
    let metrics = &scheduler.leader_slot_metrics;
    assert_eq!(metrics.received_packets, 0);
    assert_eq!(metrics.buffered_transactions, 0);
    assert_eq!(metrics.dropped_transactions, 0);
    assert_eq!(metrics.scheduled_transactions, 1);
    assert_eq!(metrics.completed_transactions, 1);
    assert_finished(&scheduler, &mut agave);
}

#[test]
fn expired_execution_batch_is_dispatched_in_the_next_slot() {
    let (mut scheduler, mut agave) = setup(2);
    leader_ready(&mut agave, 100);
    let transaction = enqueue(&mut agave, &[1]);
    scheduler.run_iteration();
    reply_checks(&mut agave);
    scheduler.run_iteration();
    let batch = receive_execution(&mut agave, 0, 100, &[transaction]);
    leader_ready(&mut agave, 101);
    agave.workers[0]
        .worker_to_pack
        .try_write(ExecutionWorkerToPackMessage {
            batch: batch.batch,
            processed_code: processed_codes::MAX_WORKING_SLOT_EXCEEDED,
            responses: ExecutionResponseRegion {
                num_transaction_responses: 0,
                transaction_responses_offset: 0,
            },
        })
        .unwrap();
    scheduler.run_iteration();
    let batch = receive_execution(&mut agave, 0, 101, &[transaction]);
    reply_execution(&mut agave, 0, batch, &[not_included_reasons::NONE]);
    scheduler.run_iteration();
    assert_eq!(scheduler.scheduled_cost, ACTUAL_COST);
    assert_finished(&scheduler, &mut agave);
}
