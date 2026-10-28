use {
    crate::{Scheduler, progress_tracker::SchedulerState},
    agave_scheduler_bindings::{PackToCheckWorkerMessage, check_message_flags},
    agave_scheduling_utils::transaction_ptr::{OwnedTransactionPtr, TransactionPtrBatch},
    core::{mem::ManuallyDrop, num::NonZeroUsize, time::Duration},
    rts_alloc::Allocator,
};

const TPU_READABLE_TIMEOUT: Duration = Duration::from_millis(10);
pub(super) const MAX_TPU_PACKETS_PER_ITERATION: NonZeroUsize = NonZeroUsize::new(256).unwrap();
pub(super) const MAX_PACKETS_PER_CHECK_BATCH: usize = 16;
const TPU_ACCEPTANCE_SLOT_WINDOW: u64 = 20;
const CHECK_FLAGS: u16 = check_message_flags::AGE_NONCE_CHECKS
    | check_message_flags::LOAD_FEE_PAYER_BALANCE
    | check_message_flags::LOAD_ADDRESS_LOOKUP_TABLES
    | check_message_flags::CALCULATE_SCHEDULING_DETAILS;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub(super) struct TpuTransactionMeta {
    pub(super) flags: u8,
    pub(super) src_addr: [u8; 16],
}

pub(super) type RawCheckBatch<'a> =
    TransactionPtrBatch<'a, TpuTransactionMeta, MAX_PACKETS_PER_CHECK_BATCH>;

impl Scheduler {
    pub(super) fn handle_tpu_ingress(&mut self) {
        let accept_packets = should_accept_packets(&self.state);
        if !accept_packets && self.outstanding_check_packets == 0 && self.transactions.len() == 0 {
            let _ = self
                .tpu_receiver
                .wait_readable_timeout(TPU_READABLE_TIMEOUT);
        }
        let Some(messages) = self
            .tpu_receiver
            .try_reserve_read_batch(MAX_TPU_PACKETS_PER_ITERATION)
        else {
            return;
        };

        let mut sent_packets = 0usize;
        let mut transactions = messages.iter().map(|message| {
            // SAFETY: Agave transfers exclusive ownership of an initialized transaction allocation.
            let transaction =
                unsafe { OwnedTransactionPtr::from_region(message.transaction, &self.allocator) };
            let metadata = TpuTransactionMeta {
                flags: message.flags,
                src_addr: message.src_addr,
            };
            (transaction, metadata)
        });
        // Stop dispatching when every possible response has a reserved queue slot.
        // Still drain the TPU batch below so ingress does not fall behind.
        'batches: while accept_packets
            && self.outstanding_check_batches < self.check_response_capacity
            && transactions.len() != 0
        {
            let Some(mut batch) = CheckBatch::allocate(&self.allocator) else {
                break;
            };
            for (transaction, metadata) in transactions.by_ref().take(MAX_PACKETS_PER_CHECK_BATCH) {
                if !batch.try_push(transaction, metadata) {
                    break 'batches;
                }
            }
            let Some(count) = batch.try_send(&self.check_sender) else {
                break;
            };
            self.outstanding_check_packets = self.outstanding_check_packets.saturating_add(count);
            self.outstanding_check_batches = self.outstanding_check_batches.wrapping_add(1);
            sent_packets = sent_packets.saturating_add(count);
        }
        transactions.for_each(drop);
        self.leader_slot_metrics.received_packets = self
            .leader_slot_metrics
            .received_packets
            .saturating_add(messages.len());
        self.leader_slot_metrics.dropped_transactions = self
            .leader_slot_metrics
            .dropped_transactions
            .saturating_add(messages.len().saturating_sub(sent_packets));
    }
}

fn should_accept_packets(state: &SchedulerState) -> bool {
    match state {
        SchedulerState::LeaderStarting { .. } | SchedulerState::LeaderReady { .. } => true,
        SchedulerState::NotLeader {
            current_slot,
            next_leader_slot,
        } => next_leader_slot
            .checked_sub(*current_slot)
            .is_some_and(|offset| offset < TPU_ACCEPTANCE_SLOT_WINDOW),
    }
}

/// Owns a batch container and its transactions, freeing both on drop unless sent to a queue.
struct CheckBatch<'a> {
    batch: ManuallyDrop<RawCheckBatch<'a>>,
}

impl<'a> CheckBatch<'a> {
    fn allocate(allocator: &'a Allocator) -> Option<Self> {
        RawCheckBatch::allocate(allocator).map(|batch| Self {
            batch: ManuallyDrop::new(batch),
        })
    }

    /// Transfers ownership to the batch, or drops the transaction if full.
    fn try_push(&mut self, transaction: OwnedTransactionPtr<'a>, meta: TpuTransactionMeta) -> bool {
        // SAFETY: the transaction owns initialized bytes in this allocator. On success the
        // batch takes ownership and keeps those bytes alive.
        if unsafe { self.batch.try_push(transaction.to_region(), meta) }.is_err() {
            return false;
        }
        let _ = transaction.into_inner();
        true
    }

    /// Returns the number of packets sent, or drops the batch on failure.
    ///
    /// On success, the scheduler must free the batch and transactions only after receiving
    /// the worker response.
    fn try_send(self, queue: &shaq::mpmc::Producer<PackToCheckWorkerMessage>) -> Option<usize> {
        let count = self.batch.len();
        let message = PackToCheckWorkerMessage {
            batch: self.batch.to_sharable_transaction_batch_region(),
            flags: CHECK_FLAGS,
        };
        queue.try_write(message).ok()?;
        core::mem::forget(self);
        Some(count)
    }
}

impl Drop for CheckBatch<'_> {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns the container and its transactions. The inner batch is taken
        // only during drop and is never accessed again.
        let batch = unsafe { ManuallyDrop::take(&mut self.batch) };
        // SAFETY: this wrapper exclusively owns the batch and its transaction allocations.
        unsafe { batch.free_with_transactions() };
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::tests::{setup, setup_with_capacities},
        agave_scheduler_bindings::{
            CheckResponseRegion, CheckWorkerToPackMessage, SharableTransactionRegion,
            TpuToPackMessage, processed_codes,
        },
        agave_scheduler_handshake::AgaveSession,
        std::time::Instant,
    };

    fn enqueue(scheduler: &Scheduler, agave: &mut AgaveSession, count: usize) {
        for index in 0..count {
            let transaction = transaction(&scheduler.allocator);
            agave
                .tpu_to_pack
                .producer
                .try_write(TpuToPackMessage {
                    transaction,
                    flags: index as u8,
                    src_addr: [index as u8; 16],
                })
                .unwrap();
        }
    }

    fn transaction(allocator: &Allocator) -> SharableTransactionRegion {
        let ptr = allocator.allocate(1).unwrap();
        // SAFETY: the freshly allocated byte is writable and belongs to this allocator.
        unsafe {
            ptr.write(0);
            SharableTransactionRegion {
                offset: allocator.offset(ptr),
                length: 1,
            }
        }
    }

    #[test]
    fn ingress_limits_work_per_iteration() {
        const TOTAL: usize = MAX_TPU_PACKETS_PER_ITERATION.get() + MAX_PACKETS_PER_CHECK_BATCH + 1;
        let limit = MAX_TPU_PACKETS_PER_ITERATION.get();
        let (mut scheduler, mut agave) = setup(TOTAL.div_ceil(MAX_PACKETS_PER_CHECK_BATCH).max(2));
        enqueue(&scheduler, &mut agave, TOTAL);
        let receiver = &agave.check_workers[0].pack_to_check_worker;
        for start in (0..TOTAL).step_by(limit) {
            scheduler.handle_tpu_ingress();
            let mut received = 0usize;
            while let Some(message) = receiver.try_read() {
                received = received.wrapping_add(usize::from(message.batch.num_transactions));
                assert_eq!(message.flags, CHECK_FLAGS);
                // SAFETY: no workers run here; the received batch is exclusively owned by this test.
                let batch = unsafe {
                    RawCheckBatch::from_sharable_transaction_batch_region(
                        &message.batch,
                        &scheduler.allocator,
                    )
                };
                // SAFETY: this test exclusively owns the batch and its transactions.
                unsafe { batch.free_with_transactions() };
            }
            assert_eq!(received, TOTAL.saturating_sub(start).min(limit));
        }
        assert_eq!(scheduler.outstanding_check_packets, TOTAL);
        assert_eq!(scheduler.allocator.outstanding_allocation_bytes(), 0);
    }

    #[test]
    fn ingress_drops_packets_when_check_response_capacity_is_full() {
        let (mut scheduler, mut agave) = setup_with_capacities(4, 2, 1, 2, 2);
        enqueue(&scheduler, &mut agave, 3 * MAX_PACKETS_PER_CHECK_BATCH);

        scheduler.handle_tpu_ingress();
        assert_eq!(scheduler.outstanding_check_batches, 2);
        assert_eq!(
            scheduler.leader_slot_metrics.received_packets,
            3 * MAX_PACKETS_PER_CHECK_BATCH
        );
        assert_eq!(
            scheduler.leader_slot_metrics.dropped_transactions,
            MAX_PACKETS_PER_CHECK_BATCH
        );
        assert!(scheduler.tpu_receiver.try_read().is_none());

        let first = agave.check_workers[0]
            .pack_to_check_worker
            .try_read()
            .unwrap();
        agave.check_workers[0]
            .check_worker_to_pack
            .try_write(CheckWorkerToPackMessage {
                batch: first.batch,
                processed_code: processed_codes::INVALID,
                responses: CheckResponseRegion {
                    num_transaction_responses: 0,
                    transaction_responses_offset: 0,
                },
            })
            .unwrap();
        scheduler.handle_check_worker_responses();
        assert_eq!(scheduler.outstanding_check_batches, 1);

        enqueue(&scheduler, &mut agave, MAX_PACKETS_PER_CHECK_BATCH);
        scheduler.handle_tpu_ingress();
        assert_eq!(scheduler.outstanding_check_batches, 2);
        assert_eq!(
            scheduler.leader_slot_metrics.received_packets,
            4 * MAX_PACKETS_PER_CHECK_BATCH
        );
        assert_eq!(
            scheduler.leader_slot_metrics.dropped_transactions,
            2 * MAX_PACKETS_PER_CHECK_BATCH
        );
        assert!(scheduler.tpu_receiver.try_read().is_none());

        while let Some(message) = agave.check_workers[0].pack_to_check_worker.try_read() {
            // SAFETY: no worker runs in this test, and this test owns the remaining batches.
            let batch = unsafe {
                RawCheckBatch::from_sharable_transaction_batch_region(
                    &message.batch,
                    &scheduler.allocator,
                )
            };
            // SAFETY: this test exclusively owns each batch and its transactions.
            unsafe { batch.free_with_transactions() };
        }
        assert_eq!(scheduler.allocator.outstanding_allocation_bytes(), 0);
    }

    #[test]
    fn accepts_packets_only_near_leadership() {
        assert!(!should_accept_packets(&SchedulerState::new()));
        assert!(!should_accept_packets(&SchedulerState::NotLeader {
            current_slot: 100,
            next_leader_slot: 120,
        }));
        assert!(should_accept_packets(&SchedulerState::NotLeader {
            current_slot: 100,
            next_leader_slot: 119,
        }));
        assert!(should_accept_packets(&SchedulerState::LeaderStarting {
            slot: 100
        }));
        assert!(should_accept_packets(&SchedulerState::LeaderReady {
            slot: 100,
            remaining_cost_units: 0,
            slot_start: Instant::now(),
            slot_duration: Duration::from_millis(400),
        }));
        assert!(should_accept_packets(&SchedulerState::NotLeader {
            current_slot: 100,
            next_leader_slot: 100,
        }));
        assert!(!should_accept_packets(&SchedulerState::NotLeader {
            current_slot: 100,
            next_leader_slot: 99,
        }));
        assert!(!should_accept_packets(&SchedulerState::NotLeader {
            current_slot: 100,
            next_leader_slot: 4,
        }));
    }

    #[test]
    fn ingress_frees_packets_when_not_accepting() {
        let (mut scheduler, mut agave) = setup(2);
        scheduler.state = SchedulerState::new();
        enqueue(&scheduler, &mut agave, 3);
        scheduler.handle_tpu_ingress();
        assert_eq!(scheduler.outstanding_check_packets, 0);
        assert!(
            agave.check_workers[0]
                .pack_to_check_worker
                .try_read()
                .is_none()
        );
        assert_eq!(scheduler.allocator.outstanding_allocation_bytes(), 0);
        assert_eq!(scheduler.leader_slot_metrics.slot, None);
        assert_eq!(scheduler.leader_slot_metrics.received_packets, 3);
        assert_eq!(scheduler.leader_slot_metrics.dropped_transactions, 3);

        scheduler.state = SchedulerState::LeaderStarting { slot: 100 };
        scheduler.handle_leader_slot_metrics();
        assert_eq!(scheduler.leader_slot_metrics.slot, Some(100));
        assert_eq!(scheduler.leader_slot_metrics.received_packets, 0);
        assert_eq!(scheduler.leader_slot_metrics.dropped_transactions, 0);
    }

    #[test]
    fn ingress_frees_unsent_packets_when_check_queue_is_full() {
        const AVAILABLE_BATCH_SLOTS: usize = 2;
        const EXPECTED_SENT_PACKETS: usize = AVAILABLE_BATCH_SLOTS * MAX_PACKETS_PER_CHECK_BATCH;
        // Exercise cleanup of the failed batch, a remaining full batch, and a trailing packet.
        const UNSENT_PACKETS: usize = 2 * MAX_PACKETS_PER_CHECK_BATCH + 1;
        const TOTAL_PACKETS: usize = EXPECTED_SENT_PACKETS + UNSENT_PACKETS;

        let (mut scheduler, mut agave) = setup_with_capacities(AVAILABLE_BATCH_SLOTS, 8, 1, 2, 2);
        scheduler.handle_leader_slot_metrics();
        let placeholder = PackToCheckWorkerMessage {
            batch: agave_scheduler_bindings::SharableTransactionBatchRegion {
                transactions_offset: 0,
                num_transactions: 0,
            },
            flags: 0,
        };
        while scheduler.check_sender.try_write(placeholder).is_ok() {}
        for _ in 0..AVAILABLE_BATCH_SLOTS {
            assert_eq!(
                agave.check_workers[0].pack_to_check_worker.try_read(),
                Some(placeholder)
            );
        }
        enqueue(&scheduler, &mut agave, TOTAL_PACKETS);
        scheduler.handle_tpu_ingress();
        assert_eq!(scheduler.outstanding_check_packets, EXPECTED_SENT_PACKETS);
        let metrics = &scheduler.leader_slot_metrics;
        assert_eq!(metrics.received_packets, TOTAL_PACKETS);
        assert_eq!(metrics.buffered_transactions, 0);
        assert_eq!(metrics.dropped_transactions, UNSENT_PACKETS);
        while let Some(message) = agave.check_workers[0].pack_to_check_worker.try_read() {
            if message != placeholder {
                // SAFETY: this test owns each dequeued ingress batch; placeholders have no allocation.
                let batch = unsafe {
                    RawCheckBatch::from_sharable_transaction_batch_region(
                        &message.batch,
                        &scheduler.allocator,
                    )
                };
                // SAFETY: this test exclusively owns the batch and its transactions.
                unsafe { batch.free_with_transactions() };
            }
        }
        assert_eq!(scheduler.allocator.outstanding_allocation_bytes(), 0);
    }

    #[test]
    fn ingress_frees_packets_when_batch_allocation_fails() {
        let (mut scheduler, mut agave) = setup(2);
        enqueue(&scheduler, &mut agave, 17);
        let mut allocations = Vec::new();
        while let Some(allocation) = scheduler
            .allocator
            .allocate(RawCheckBatch::TRANSACTION_META_END as u32)
        {
            allocations.push(allocation);
        }
        scheduler.handle_tpu_ingress();
        assert_eq!(scheduler.outstanding_check_packets, 0);
        assert!(
            agave.check_workers[0]
                .pack_to_check_worker
                .try_read()
                .is_none()
        );
        for allocation in allocations {
            // SAFETY: each allocation is uniquely owned by this test and has not been freed.
            unsafe { scheduler.allocator.free(allocation) };
        }
        assert_eq!(scheduler.allocator.outstanding_allocation_bytes(), 0);
    }
}
