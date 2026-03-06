use {
    crate::{
        bridge::{
            Bridge, KeyedTransactionMeta, RuntimeState, ScheduleBatch, TransactionKey,
            TransactionState, TxDecision, Worker, WorkerAction, WorkerResponse,
        },
        handshake::client::{ClientSession, ClientWorkerSession},
        pubkeys_ptr::PubkeysPtr,
        transaction_ptr::{TransactionPtr, TransactionPtrBatch},
    },
    agave_feature_set::FeatureSet,
    agave_scheduler_bindings::{
        MAX_TRANSACTIONS_PER_MESSAGE, PackToWorkerMessage, ProgressMessage,
        SharableTransactionBatchRegion, SharableTransactionRegion, TpuToPackMessage,
        WorkerToPackMessage, processed_codes,
        worker_message_types::{self, CheckResponse, ExecutionResponse},
    },
    agave_transaction_view::{
        result::TransactionViewError, transaction_view::SanitizedTransactionView,
    },
    metrics::{Counter, Gauge, counter, gauge},
    rts_alloc::Allocator,
    slotmap::SlotMap,
    solana_fee::FeeFeatures,
    solana_packet::PACKET_DATA_SIZE,
    std::ptr::NonNull,
};

pub struct SchedulerBindingsBridge<M> {
    allocator: Allocator,
    tpu_to_pack: shaq::spsc::Consumer<TpuToPackMessage>,
    progress_tracker: shaq::spsc::Consumer<ProgressMessage>,
    workers: Vec<SchedulerWorker>,

    progress: ProgressMessage,
    runtime: RuntimeState,
    state: SlotMap<TransactionKey, TransactionState>,

    metrics: SchedulerBindingsMetrics,

    _marker: core::marker::PhantomData<M>,
}

type Batch<'a, M> = TransactionPtrBatch<'a, KeyedTransactionMeta<M>>;

impl<M> SchedulerBindingsBridge<M>
where
    M: Copy,
{
    const TX_BATCH_META_OFFSET: usize = Batch::<M>::TX_META_START;
    const TX_BATCH_SIZE: usize = Batch::<M>::TX_META_END;

    #[must_use]
    pub fn new(
        ClientSession {
            mut allocators,
            tpu_to_pack,
            progress_tracker,
            workers,
        }: ClientSession,
    ) -> Self {
        assert_eq!(allocators.len(), 1, "invalid number of allocators");

        Self {
            allocator: allocators.remove(0),
            tpu_to_pack,
            progress_tracker,
            workers: workers.into_iter().map(SchedulerWorker).collect(),

            progress: ProgressMessage {
                leader_state: 0,
                current_slot: 0,
                next_leader_slot: u64::MAX,
                leader_range_end: u64::MAX,
                remaining_cost_units: 0,
                current_slot_progress: 0,
            },
            // TODO: Load this properly.
            runtime: RuntimeState {
                feature_set: FeatureSet::all_enabled(),
                fee_features: FeeFeatures {
                    enable_secp256r1_precompile: true,
                },
                lamports_per_signature: 5000,
                burn_percent: 50,
            },
            state: SlotMap::default(),

            metrics: SchedulerBindingsMetrics::new(),
            _marker: core::marker::PhantomData,
        }
    }

    fn collect_batch(
        allocator: &Allocator,
        state: &mut SlotMap<TransactionKey, TransactionState>,
        batch: &[KeyedTransactionMeta<M>],
    ) -> SharableTransactionBatchRegion {
        assert!(batch.len() <= MAX_TRANSACTIONS_PER_MESSAGE);

        // Allocate a batch that can hold all our transaction pointers.
        let transactions = allocator.allocate(Self::TX_BATCH_SIZE as u32).unwrap();
        let transactions_offset = unsafe { allocator.offset(transactions) };

        // Get our two pointers to the TX region & meta region.
        let tx_ptr = unsafe {
            allocator
                .ptr_from_offset(transactions_offset)
                .cast::<SharableTransactionRegion>()
        };
        // SAFETY
        // - Pointer is guaranteed to not overrun the allocation as we just created it
        //   with a sufficient size.
        let meta_ptr = unsafe {
            allocator
                .ptr_from_offset(transactions_offset)
                .byte_add(Self::TX_BATCH_META_OFFSET)
                .cast::<KeyedTransactionMeta<M>>()
        };

        // Fill in the batch with transaction pointers.
        for (i, meta) in batch.iter().copied().enumerate() {
            let tx = &mut state[meta.key];
            assert!(!tx.dead);

            // We are sending a copy to Agave, we track this as a new borrow.
            tx.borrows = tx.borrows.checked_add(1).unwrap();

            // SAFETY
            // - We have allocated the transaction batch to support at least
            //   `MAX_TRANSACTIONS_PER_MESSAGE`, we terminate the loop before we overrun the
            //   region.
            unsafe {
                tx_ptr.add(i).write(
                    tx.data
                        .inner_data()
                        .to_sharable_transaction_region(allocator),
                );
                meta_ptr.add(i).write(meta);
            };
        }

        SharableTransactionBatchRegion {
            num_transactions: batch.len().try_into().unwrap(),
            transactions_offset,
        }
    }
}

impl<M> Bridge for SchedulerBindingsBridge<M>
where
    M: Copy,
{
    type Worker = SchedulerWorker;
    type Meta = M;

    fn runtime(&self) -> &RuntimeState {
        &self.runtime
    }

    fn progress(&self) -> &ProgressMessage {
        &self.progress
    }

    fn worker_count(&self) -> usize {
        self.workers.len()
    }

    fn worker(&mut self, id: usize) -> &mut Self::Worker {
        &mut self.workers[id]
    }

    fn tx(&self, key: TransactionKey) -> &TransactionState {
        &self.state[key]
    }

    fn tx_insert(&mut self, tx: &[u8]) -> Result<TransactionKey, TransactionViewError> {
        assert!(tx.len() <= PACKET_DATA_SIZE);

        let ptr = self
            .allocator
            .allocate(tx.len().try_into().unwrap())
            .unwrap();
        // SAFETY:
        // - We own this pointer exclusively.
        // - The allocated region is at least `tx.len()` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(tx.as_ptr(), ptr.as_ptr(), tx.len());
        }
        // SAFETY:
        // - We own this pointer and the size is correct.
        let tx = unsafe { TransactionPtr::from_raw_parts(ptr, tx.len()) };

        // Sanitize the transaction, drop it immediately if it fails sanitization.
        //
        // TODO: Don't hardcode true, true.
        match SanitizedTransactionView::try_new_sanitized(tx, true, true) {
            Ok(tx) => {
                let key = self.state.insert(TransactionState {
                    dead: false,
                    borrows: 0,
                    flags: 0,
                    data: tx,
                    keys: None,
                });
                self.metrics.state_len.set(self.state.len() as f64);

                Ok(key)
            }
            Err(err) => {
                // SAFETY:
                // - We own `tx` exclusively.
                // - The previous `TransactionPtr` has been dropped by `try_new_sanitized`.
                unsafe {
                    self.allocator.free(ptr);
                }

                Err(err)
            }
        }
    }

    fn tx_drop(&mut self, key: TransactionKey) {
        // If we have requests that have borrowed this shared transaction region, then
        // we can't immediately clean up and must instead flag it as dead.
        match self.state[key].borrows {
            0 => {
                let state = self.state.remove(key).unwrap();
                self.metrics.state_len.set(self.state.len() as f64);

                if let Some(keys) = state.keys {
                    // SAFETY
                    // - We own these pointers/allocations exclusively.
                    unsafe {
                        keys.free(&self.allocator);
                    }
                }

                // SAFETY
                // - We own the allocation exclusively.
                unsafe {
                    state.data.into_inner_data().free(&self.allocator);
                }
            }
            _ => self.state[key].dead = true,
        }
    }

    fn drain_progress(&mut self) -> Option<ProgressMessage> {
        self.progress_tracker.sync();

        let mut received = false;
        while let Some(msg) = self.progress_tracker.try_read() {
            self.progress = *msg;
            received = true;
        }
        self.progress_tracker.finalize();

        received.then_some(self.progress)
    }

    fn tpu_len(&mut self) -> usize {
        self.tpu_to_pack.sync();

        self.tpu_to_pack.len()
    }

    fn tpu_drain(
        &mut self,
        mut cb: impl FnMut(&mut Self, TransactionKey) -> TxDecision,
        max_count: usize,
    ) {
        self.tpu_to_pack.sync();

        let additional = std::cmp::min(self.tpu_to_pack.len(), max_count);
        for _ in 0..additional {
            let msg = self.tpu_to_pack.try_read().unwrap();

            // SAFETY:
            // - Trust Agave to have properly transferred ownership to use & not to
            //   free/access this.
            // - We are only creating a single exclusive pointer.
            let tx = unsafe {
                TransactionPtr::from_sharable_transaction_region(&msg.transaction, &self.allocator)
            };

            // Sanitize the transaction, drop it immediately if it fails sanitization.
            //
            // TODO: Don't hardcode true, true,
            let Ok(tx) = SanitizedTransactionView::try_new_sanitized(tx, true, true) else {
                // SAFETY:
                // - We own `tx` exclusively.
                // - The previous `TransactionPtr` has been dropped by `try_new_sanitized`.
                unsafe {
                    self.allocator.free_offset(msg.transaction.offset);
                }

                self.metrics.recv_tpu_err_sanitize.increment(1);

                continue;
            };

            // Get the ID so the caller can store it for later use.
            let key = self.state.insert(TransactionState {
                dead: false,
                borrows: 0,
                flags: msg.flags,
                data: tx,
                keys: None,
            });
            self.metrics.state_len.set(self.state.len() as f64);

            // Remove & free the TX if the scheduler doesn't want it.
            if cb(self, key) == TxDecision::Drop {
                let state = self.state.remove(key).unwrap();
                assert!(state.keys.is_none());
                assert_eq!(state.borrows, 0);
                self.metrics.state_len.set(self.state.len() as f64);

                // SAFETY:
                // - We own `tx` exclusively.
                unsafe { state.data.into_inner_data().free(&self.allocator) };
            }
        }

        self.tpu_to_pack.finalize();
    }

    fn worker_drain(
        &mut self,
        worker: usize,
        mut cb: impl FnMut(&mut Self, WorkerResponse<'_, Self::Meta>) -> TxDecision,
        max_count: usize,
    ) {
        self.workers[worker].0.worker_to_pack.sync();
        for _ in 0..max_count {
            let Some(rep) = self.workers[worker].0.worker_to_pack.try_read().copied() else {
                break;
            };
            self.handle_worker_response(rep, &mut cb);
        }
        self.workers[worker].0.worker_to_pack.finalize();
    }

    fn schedule(
        &mut self,
        ScheduleBatch {
            worker,
            transactions: batch,
            max_working_slot,
            flags,
        }: ScheduleBatch<&[KeyedTransactionMeta<M>]>,
    ) {
        let queue = &mut self.workers[worker].0.pack_to_worker;

        queue.sync();
        queue
            .try_write(PackToWorkerMessage {
                flags,
                max_working_slot,
                batch: Self::collect_batch(&self.allocator, &mut self.state, batch),
            })
            .unwrap();
        queue.commit();
    }
}

impl<M> SchedulerBindingsBridge<M>
where
    M: Copy,
{
    fn handle_worker_response(
        &mut self,
        rep: WorkerToPackMessage,
        cb: &mut impl FnMut(&mut Self, WorkerResponse<'_, M>) -> TxDecision,
    ) {
        // Get transaction & meta pointers.
        let transactions = unsafe {
            self.allocator
                .ptr_from_offset(rep.batch.transactions_offset)
                .cast::<SharableTransactionRegion>()
        };
        // SAFETY:
        // - We ensured that this batch was originally allocated to support M.
        let metas = unsafe { transactions.byte_add(Batch::<M>::TX_META_START).cast() };

        let responses = match (rep.processed_code, rep.responses.tag) {
            (processed_codes::PROCESSED, worker_message_types::EXECUTION_RESPONSE) => {
                assert_eq!(
                    rep.batch.num_transactions,
                    rep.responses.num_transaction_responses
                );
                WorkerResponseBatch::Execution(unsafe {
                    self.allocator
                        .ptr_from_offset(rep.responses.transaction_responses_offset)
                        .cast()
                })
            }
            (processed_codes::PROCESSED, worker_message_types::CHECK_RESPONSE) => {
                assert_eq!(
                    rep.batch.num_transactions,
                    rep.responses.num_transaction_responses
                );
                WorkerResponseBatch::Check(unsafe {
                    self.allocator
                        .ptr_from_offset(rep.responses.transaction_responses_offset)
                        .cast()
                })
            }
            (processed_codes::MAX_WORKING_SLOT_EXCEEDED, _) => WorkerResponseBatch::Unprocessed,
            _ => panic!("Unexpected response; rep={rep:?}"),
        };

        for index in 0..usize::from(rep.batch.num_transactions) {
            // SAFETY
            // - We took care to allocate these correctly originally.
            let KeyedTransactionMeta::<M> { key, meta } = unsafe { metas.add(index).read() };
            let decision = self.handle_transaction_response(key, meta, index, &responses, cb);

            // Remove the tx from state & drop the allocation if requested.
            if decision == TxDecision::Drop {
                self.tx_drop(key);
            }
        }

        // SAFETY:
        // - It is our responsibility to free the response pointers. The transaction
        //   lifetimes we are already managing separately via Keep/Drop.
        unsafe {
            self.allocator.free_offset(rep.batch.transactions_offset);
            match responses {
                WorkerResponseBatch::Unprocessed => {}
                WorkerResponseBatch::Execution(ptr) => self.allocator.free(ptr.cast()),
                WorkerResponseBatch::Check(ptr) => self.allocator.free(ptr.cast()),
            }
        }
    }

    fn handle_transaction_response(
        &mut self,
        key: TransactionKey,
        meta: M,
        index: usize,
        responses: &WorkerResponseBatch,
        cb: &mut impl FnMut(&mut Self, WorkerResponse<'_, M>) -> TxDecision,
    ) -> TxDecision {
        // Decrease the borrow counter as Agave has returned ownership to us.
        let state = &mut self.state[key];
        state.borrows = state.borrows.checked_sub(1).unwrap();

        // Only callback if this state is not already dead (scheduler requested drop).
        match (state.dead, responses) {
            (true, _) => TxDecision::Drop,
            (false, WorkerResponseBatch::Unprocessed) => {
                let rep = WorkerResponse {
                    key,
                    meta,
                    response: WorkerAction::Unprocessed,
                };

                cb(self, rep)
            }
            (false, WorkerResponseBatch::Execution(rep)) => {
                // SAFETY
                // - We trust Agave to have correctly allocated the responses.
                let rep = unsafe { rep.add(index).read() };
                let rep = WorkerResponse {
                    key,
                    meta,
                    response: WorkerAction::Execute(rep),
                };

                cb(self, rep)
            }
            (false, WorkerResponseBatch::Check(rep)) => {
                // SAFETY
                // - We trust Agave to have correctly allocated the responses.
                let rep = unsafe { rep.add(index).read() };

                // Load shared pubkeys if there are any.
                let keys = (rep.resolved_pubkeys.num_pubkeys > 0).then(|| unsafe {
                    // SAFETY
                    // - Region exists as `num_pubkeys > 0`.
                    // - Trust Agave to have allocated this region correctly.
                    PubkeysPtr::from_sharable_pubkeys(&rep.resolved_pubkeys, &self.allocator)
                });

                // Callback holding keys ref, defer storing keys on state.
                let decision = cb(
                    self,
                    WorkerResponse {
                        key,
                        meta,
                        response: WorkerAction::Check(rep, keys.as_ref()),
                    },
                );

                // Free old keys if present before storing new keys.
                if let Some(old_keys) = self.state[key].keys.take() {
                    // SAFETY
                    // - We own this allocation exclusively.
                    unsafe { old_keys.free(&self.allocator) }
                }

                // Store the keys on state.
                self.state[key].keys = keys;

                decision
            }
        }
    }
}

struct SchedulerBindingsMetrics {
    state_len: Gauge,
    recv_tpu_err_sanitize: Counter,
}

impl SchedulerBindingsMetrics {
    fn new() -> Self {
        Self {
            state_len: gauge!("container_len", "label" => "state"),
            recv_tpu_err_sanitize: counter!("recv_tpu", "label" => "err", "err" => "sanitize"),
        }
    }
}

pub struct SchedulerWorker(ClientWorkerSession);

impl Worker for SchedulerWorker {
    fn len(&mut self) -> usize {
        self.0.pack_to_worker.sync();

        self.0.pack_to_worker.len()
    }

    fn rem(&mut self) -> usize {
        self.0.pack_to_worker.sync();
        let cap = self.0.pack_to_worker.capacity();
        let len = self.0.pack_to_worker.len();

        cap.checked_sub(len).unwrap()
    }
}

enum WorkerResponseBatch {
    Unprocessed,
    Execution(NonNull<ExecutionResponse>),
    Check(NonNull<CheckResponse>),
}
