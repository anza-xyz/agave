use {
    crate::{
        Scheduler,
        resolved_transaction::ResolvedTransaction,
        tpu_ingress::{RawCheckBatch, TpuTransactionMeta},
    },
    agave_scheduler_bindings::{
        CheckWorkerToPackMessage, processed_codes, tpu_message_flags,
        worker_message_types::{
            CheckResponse, age_nonce_check_flags, fee_payer_balance_flags,
            parsing_and_sanitization_flags, resolve_flags, scheduling_details_flags,
        },
    },
    agave_scheduling_utils::{
        pubkeys_ptr::OwnedPubkeysPtr, responses_region::CheckResponsesPtr,
        transaction_ptr::OwnedTransactionPtr,
    },
};

const MAX_CHECK_RESPONSE_PACKETS_PER_ITERATION: usize = 512;
const BURN_PERCENT: u64 = 50;
const PRIORITY_MULTIPLIER: u64 = 1_000_000;

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "metadata retained for scheduling")
)]
pub(super) struct TransactionState {
    pub(super) transaction: ResolvedTransaction,
    pub(super) metadata: TpuTransactionMeta,
    pub(super) cost: u64,
    pub(super) allocated_accounts_data_size: u64,
    pub(super) execution_worker: Option<usize>,
}

impl Scheduler {
    pub(super) fn handle_check_worker_responses(&mut self) {
        let mut received_packets = 0;
        while received_packets < MAX_CHECK_RESPONSE_PACKETS_PER_ITERATION {
            let Some(message) = self.check_receiver.try_read() else {
                break;
            };
            self.outstanding_check_batches = self
                .outstanding_check_batches
                .checked_sub(1)
                .expect("check response must have an outstanding batch");
            received_packets =
                received_packets.saturating_add(usize::from(message.batch.num_transactions));
            self.handle_check_worker_response(message);
        }
    }

    fn handle_check_worker_response(&mut self, message: CheckWorkerToPackMessage) {
        // SAFETY: workers return the batch sent by ingress and have finished accessing it.
        let batch = unsafe {
            RawCheckBatch::from_sharable_transaction_batch_region(&message.batch, &self.allocator)
        };
        self.outstanding_check_packets = self.outstanding_check_packets.saturating_sub(batch.len());
        if message.processed_code != processed_codes::PROCESSED {
            self.leader_slot_metrics.dropped_transactions = self
                .leader_slot_metrics
                .dropped_transactions
                .saturating_add(batch.len());
            // SAFETY: the worker has released this batch. An unprocessed response has no
            // defined response region, so only the original allocations can be freed.
            unsafe { batch.free_with_transactions() };
            return;
        }

        assert_eq!(
            usize::from(message.responses.num_transaction_responses),
            batch.len(),
            "check worker invariant violated: response count must match batch length"
        );
        // SAFETY: processed responses transfer an initialized response allocation to us.
        let responses = unsafe {
            CheckResponsesPtr::from_transaction_response_region(&message.responses, &self.allocator)
        };
        let mut buffered = 0usize;
        let mut dropped = batch.len();
        for ((transaction, metadata), response) in batch.iter().zip(responses.iter()) {
            // SAFETY: the worker has finished using this exclusively owned transaction.
            let transaction = unsafe { OwnedTransactionPtr::new(transaction, &self.allocator) };
            let region = (response.resolve_flags & resolve_flags::PERFORMED != 0)
                .then_some(response.resolved_pubkeys);
            // SAFETY: performed resolution returns exclusive ownership of any pubkey allocation.
            let pubkeys = unsafe { OwnedPubkeysPtr::from_region(region, &self.allocator) };
            if !response_is_valid(response) {
                continue;
            }
            if !has_sufficient_fee_payer_balance(
                response.transaction_fee,
                response.prioritization_fee,
                response.fee_payer_balance,
            ) {
                continue;
            }
            // SAFETY: the transaction belongs to this allocator.
            let Ok(transaction) = (unsafe {
                ResolvedTransaction::try_new(
                    transaction,
                    pubkeys,
                    &self.allocator,
                    &self.reserved_account_keys,
                )
            }) else {
                continue;
            };
            let priority = calculate_priority(response, metadata.flags);
            let transaction = TransactionState {
                execution_worker: None,
                transaction,
                metadata,
                cost: response.estimated_cost_units,
                allocated_accounts_data_size: response.allocated_accounts_data_size,
            };
            let inserted = self.transactions.insert(priority, transaction);
            if inserted.is_ok() {
                buffered = buffered.saturating_add(1);
            }
            match inserted {
                Ok((_, Some(transaction))) | Err(transaction) => {
                    // SAFETY: rejection or eviction returns exclusive ownership.
                    unsafe { transaction.transaction.free(&self.allocator) };
                }
                Ok((_, None)) => {
                    dropped = dropped
                        .checked_sub(1)
                        .expect("retained count exceeds batch length");
                }
            }
        }
        self.leader_slot_metrics.buffered_transactions = self
            .leader_slot_metrics
            .buffered_transactions
            .saturating_add(buffered);
        self.leader_slot_metrics.dropped_transactions = self
            .leader_slot_metrics
            .dropped_transactions
            .saturating_add(dropped);
        // SAFETY: each transaction has been retained or freed; only the container remains.
        unsafe { batch.free() };
        // SAFETY: all responses have been consumed and their nested allocations handled.
        unsafe { responses.free(&self.allocator) };
    }
}

fn response_is_valid(response: &CheckResponse) -> bool {
    const AGE_NONCE_FAILURE_FLAGS: u8 = age_nonce_check_flags::TOO_OLD
        | age_nonce_check_flags::INVALID_NONCE
        | age_nonce_check_flags::UNSUPPORTED_VERSION;

    response.parsing_and_sanitization_flags & parsing_and_sanitization_flags::FAILED == 0
        && response.age_nonce_check_flags & age_nonce_check_flags::PERFORMED != 0
        && response.age_nonce_check_flags & AGE_NONCE_FAILURE_FLAGS == 0
        && response.fee_payer_balance_flags & fee_payer_balance_flags::PERFORMED != 0
        && response.resolve_flags & resolve_flags::PERFORMED != 0
        && response.resolve_flags & resolve_flags::FAILED == 0
        && response.scheduling_details_flags & scheduling_details_flags::PERFORMED != 0
        && response.scheduling_details_flags & scheduling_details_flags::FAILED == 0
}

fn has_sufficient_fee_payer_balance(
    transaction_fee: u64,
    prioritization_fee: u64,
    balance: u64,
) -> bool {
    transaction_fee
        .checked_add(prioritization_fee)
        .is_some_and(|total_fee| total_fee <= balance)
}

fn calculate_priority(response: &CheckResponse, tpu_flags: u8) -> u64 {
    if tpu_flags & tpu_message_flags::IS_SIMPLE_VOTE != 0 {
        return u64::MAX;
    }
    let reward = response.prioritization_fee.saturating_add(
        response.transaction_fee.saturating_sub(
            response
                .transaction_fee
                .saturating_mul(BURN_PERCENT)
                .wrapping_div(100),
        ),
    );
    #[expect(clippy::arithmetic_side_effects, reason = "cost plus one is nonzero")]
    reward
        .saturating_mul(PRIORITY_MULTIPLIER)
        .wrapping_div(response.estimated_cost_units.saturating_add(1))
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{tests::setup, transaction_container::TransactionContainer},
        agave_scheduler_bindings::{
            CheckResponseRegion, SharablePubkeys, SharableTransactionRegion,
        },
        agave_scheduling_utils::responses_region::resolve_responses_from_iter,
        rts_alloc::Allocator,
        solana_message::{VersionedMessage, v0},
        solana_pubkey::Pubkey,
        solana_signature::Signature,
        solana_svm_transaction::svm_message::SVMMessage,
        solana_transaction::versioned::VersionedTransaction,
    };

    #[test]
    fn fee_payer_balance_boundaries() {
        assert!(has_sufficient_fee_payer_balance(0, 0, 0));
        assert!(!has_sufficient_fee_payer_balance(100, 50, 149));
        assert!(has_sufficient_fee_payer_balance(100, 50, 150));
        assert!(has_sufficient_fee_payer_balance(100, 50, 151));
        assert!(!has_sufficient_fee_payer_balance(100, 0, 0));
        assert!(!has_sufficient_fee_payer_balance(0, 50, 49));
        assert!(has_sufficient_fee_payer_balance(u64::MAX, 0, u64::MAX));
        assert!(!has_sufficient_fee_payer_balance(u64::MAX, 1, u64::MAX));
    }

    fn make_response(allocator: &Allocator) -> CheckResponse {
        let ptr = allocator.allocate(size_of::<[Pubkey; 2]>() as u32).unwrap();
        // SAFETY: the allocation is aligned and sized for two pubkeys.
        unsafe {
            ptr.cast::<[Pubkey; 2]>()
                .write([Pubkey::from([3; 32]), Pubkey::from([4; 32])])
        };
        CheckResponse {
            parsing_and_sanitization_flags: 0,
            status_cache_check_flags: 0,
            age_nonce_check_flags: age_nonce_check_flags::PERFORMED,
            fee_payer_balance_flags: fee_payer_balance_flags::PERFORMED,
            resolve_flags: resolve_flags::PERFORMED,
            scheduling_details_flags: scheduling_details_flags::PERFORMED,
            included_slot: 0,
            transaction_fee: 100,
            prioritization_fee: 50,
            estimated_cost_units: 99,
            allocated_accounts_data_size: 123,
            balance_slot: 100,
            fee_payer_balance: 1_000_000,
            resolution_slot: 100,
            min_alt_deactivation_slot: u64::MAX,
            resolved_pubkeys: SharablePubkeys {
                // SAFETY: the pointer belongs to this allocator.
                offset: unsafe { allocator.offset(ptr) },
                num_pubkeys: 2,
            },
        }
    }

    fn make_message(
        scheduler: &mut Scheduler,
        response: Option<CheckResponse>,
    ) -> CheckWorkerToPackMessage {
        let bytes = wincode::serialize(&VersionedTransaction {
            signatures: vec![Signature::default()],
            message: VersionedMessage::V0(v0::Message {
                header: solana_message::MessageHeader {
                    num_required_signatures: 1,
                    ..Default::default()
                },
                account_keys: vec![Pubkey::from([1; 32])],
                address_table_lookups: vec![v0::MessageAddressTableLookup {
                    account_key: Pubkey::from([2; 32]),
                    writable_indexes: vec![0],
                    readonly_indexes: vec![1],
                }],
                ..Default::default()
            }),
        })
        .unwrap();
        let allocator = &scheduler.allocator;
        let ptr = allocator.allocate(bytes.len() as u32).unwrap();
        // SAFETY: the allocation has space for these bytes and does not overlap the source.
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.as_ptr(), bytes.len()) };
        let transaction = SharableTransactionRegion {
            // SAFETY: the pointer belongs to this allocator.
            offset: unsafe { allocator.offset(ptr) },
            length: bytes.len() as u32,
        };
        let mut batch = RawCheckBatch::allocate(allocator).unwrap();
        // SAFETY: the initialized transaction allocation remains live until the handler frees it.
        unsafe {
            batch.try_push(
                transaction,
                TpuTransactionMeta {
                    flags: 0,
                    src_addr: [7; 16],
                },
            )
        }
        .unwrap();
        scheduler.outstanding_check_packets =
            scheduler.outstanding_check_packets.checked_add(1).unwrap();
        scheduler.outstanding_check_batches = scheduler.outstanding_check_batches.wrapping_add(1);
        CheckWorkerToPackMessage {
            batch: batch.to_sharable_transaction_batch_region(),
            processed_code: if response.is_some() {
                processed_codes::PROCESSED
            } else {
                processed_codes::INVALID
            },
            responses: response.map_or(
                CheckResponseRegion {
                    num_transaction_responses: 0,
                    transaction_responses_offset: 0,
                },
                |response| {
                    resolve_responses_from_iter(allocator, core::iter::once(response)).unwrap()
                },
            ),
        }
    }

    #[test]
    fn retains_transaction_and_metadata() {
        let (mut scheduler, _agave) = setup(2);
        let response = make_response(&scheduler.allocator);
        let message = make_message(&mut scheduler, Some(response));
        scheduler.handle_check_worker_response(message);

        assert_eq!(scheduler.outstanding_check_packets, 0);
        assert_eq!(scheduler.transactions.len(), 1);
        let id = scheduler.transactions.pop_highest().unwrap();
        let state = scheduler.transactions.get(id).unwrap();
        assert_eq!(state.metadata.src_addr, [7; 16]);
        assert_eq!(state.cost, response.estimated_cost_units);
        assert_eq!(
            state.allocated_accounts_data_size,
            response.allocated_accounts_data_size
        );
        assert_eq!(
            state.transaction.view.account_keys().get(1),
            Some(&Pubkey::from([3; 32]))
        );
        assert!(state.transaction.view.is_writable(1));
        assert_eq!(
            state.transaction.view.account_keys().get(2),
            Some(&Pubkey::from([4; 32]))
        );
        assert!(!state.transaction.view.is_writable(2));
    }

    #[test]
    fn rejected_response_frees_allocations() {
        let (mut scheduler, _agave) = setup(2);
        scheduler.handle_leader_slot_metrics();
        let mut response = make_response(&scheduler.allocator);
        response.age_nonce_check_flags |= age_nonce_check_flags::TOO_OLD;
        let message = make_message(&mut scheduler, Some(response));
        scheduler.handle_check_worker_response(message);

        assert_eq!(scheduler.outstanding_check_packets, 0);
        assert_eq!(scheduler.transactions.len(), 0);
        assert_eq!(scheduler.allocator.outstanding_allocation_bytes(), 0);
        let metrics = &scheduler.leader_slot_metrics;
        assert_eq!(metrics.buffered_transactions, 0);
        assert_eq!(metrics.dropped_transactions, 1);
    }

    #[test]
    fn insufficient_balance_frees_allocations() {
        for (transaction_fee, prioritization_fee, balance) in
            [(100, 50, 149), (100, 0, 0), (u64::MAX, 1, u64::MAX)]
        {
            let (mut scheduler, _agave) = setup(2);
            let mut response = make_response(&scheduler.allocator);
            response.transaction_fee = transaction_fee;
            response.prioritization_fee = prioritization_fee;
            response.fee_payer_balance = balance;
            let message = make_message(&mut scheduler, Some(response));
            scheduler.handle_check_worker_response(message);

            assert_eq!(scheduler.outstanding_check_packets, 0);
            assert_eq!(scheduler.transactions.len(), 0);
            assert_eq!(scheduler.allocator.outstanding_allocation_bytes(), 0);
        }
    }

    #[test]
    fn exact_balance_covers_fees() {
        let (mut scheduler, _agave) = setup(2);
        let mut response = make_response(&scheduler.allocator);
        response.fee_payer_balance = 150;
        let message = make_message(&mut scheduler, Some(response));
        scheduler.handle_check_worker_response(message);

        assert_eq!(scheduler.outstanding_check_packets, 0);
        assert_eq!(scheduler.transactions.len(), 1);
        let id = scheduler.transactions.pop_highest().unwrap();
        let state = scheduler.transactions.remove(id).unwrap();
        // SAFETY: removal returns exclusive ownership of the transaction's allocations.
        unsafe { state.transaction.free(&scheduler.allocator) };
        assert_eq!(scheduler.allocator.outstanding_allocation_bytes(), 0);
    }

    #[test]
    fn insufficient_balance_does_not_evict() {
        let (mut scheduler, _agave) = setup(2);
        scheduler.transactions = TransactionContainer::with_capacity(1);
        let response = make_response(&scheduler.allocator);
        let message = make_message(&mut scheduler, Some(response));
        scheduler.handle_check_worker_response(message);
        let retained_bytes = scheduler.allocator.outstanding_allocation_bytes();

        let mut response = make_response(&scheduler.allocator);
        response.prioritization_fee = 100;
        response.fee_payer_balance = 199;
        response.estimated_cost_units = 1;
        let message = make_message(&mut scheduler, Some(response));
        scheduler.handle_check_worker_response(message);

        assert_eq!(scheduler.outstanding_check_packets, 0);
        assert_eq!(scheduler.transactions.len(), 1);
        assert_eq!(
            scheduler.allocator.outstanding_allocation_bytes(),
            retained_bytes
        );
        let id = scheduler.transactions.pop_highest().unwrap();
        let state = scheduler.transactions.remove(id).unwrap();
        assert_eq!(state.cost, 99);
        // SAFETY: removal returns exclusive ownership of the transaction's allocations.
        unsafe { state.transaction.free(&scheduler.allocator) };
        assert_eq!(scheduler.allocator.outstanding_allocation_bytes(), 0);
    }

    #[test]
    fn unprocessed_message_frees_batch() {
        let (mut scheduler, _agave) = setup(2);
        scheduler.handle_leader_slot_metrics();
        let message = make_message(&mut scheduler, None);
        scheduler.handle_check_worker_response(message);

        assert_eq!(scheduler.outstanding_check_packets, 0);
        assert_eq!(scheduler.transactions.len(), 0);
        assert_eq!(scheduler.allocator.outstanding_allocation_bytes(), 0);
        assert_eq!(scheduler.leader_slot_metrics.dropped_transactions, 1);
    }

    #[test]
    fn eviction_and_rejection_do_not_leak() {
        let (mut scheduler, _agave) = setup(2);
        scheduler.handle_leader_slot_metrics();
        scheduler.transactions = TransactionContainer::with_capacity(1);
        let response = make_response(&scheduler.allocator);
        let message = make_message(&mut scheduler, Some(response));
        scheduler.handle_check_worker_response(message);
        let retained_bytes = scheduler.allocator.outstanding_allocation_bytes();

        // The higher fee evicts the original; the equal fee is rejected.
        for _ in 0..2 {
            let mut response = make_response(&scheduler.allocator);
            response.prioritization_fee = 100;
            let message = make_message(&mut scheduler, Some(response));
            scheduler.handle_check_worker_response(message);
            assert_eq!(scheduler.transactions.len(), 1);
            assert_eq!(
                scheduler.allocator.outstanding_allocation_bytes(),
                retained_bytes
            );
        }
        let metrics = &scheduler.leader_slot_metrics;
        assert_eq!(metrics.buffered_transactions, 2);
        assert_eq!(metrics.dropped_transactions, 2);
    }

    #[test]
    fn limits_packets_per_iteration() {
        const TOTAL: usize = MAX_CHECK_RESPONSE_PACKETS_PER_ITERATION + 1;
        let (mut scheduler, agave) = setup(TOTAL.next_power_of_two());
        for _ in 0..TOTAL {
            let message = make_message(&mut scheduler, None);
            agave.check_workers[0]
                .check_worker_to_pack
                .try_write(message)
                .unwrap();
        }

        scheduler.handle_check_worker_responses();
        assert_eq!(scheduler.outstanding_check_packets, 1);
        scheduler.handle_check_worker_responses();
        assert_eq!(scheduler.outstanding_check_packets, 0);
        assert_eq!(scheduler.allocator.outstanding_allocation_bytes(), 0);
    }
}
