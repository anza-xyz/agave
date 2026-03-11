use {
    crate::{
        handshake::{ClientSession, ClientWorkerSession},
        pubkeys_ptr::PubkeysPtr,
        transaction_ptr::{TransactionPtr, TransactionPtrBatch},
    },
    agave_feature_set::FeatureSet,
    agave_scheduler_bindings::{
        MAX_TRANSACTIONS_PER_MESSAGE, PackToWorkerMessage, ProgressMessage,
        SharableTransactionBatchRegion, SharableTransactionRegion, TpuToPackMessage,
        WorkerToPackMessage, processed_codes, tpu_message_flags,
        worker_message_types::{self, CheckResponse, ExecutionResponse},
    },
    agave_transaction_view::{
        result::TransactionViewError, transaction_view::SanitizedTransactionView,
    },
    rts_alloc::Allocator,
    slotmap::SlotMap,
    solana_fee::FeeFeatures,
    solana_pubkey::Pubkey,
    std::ptr::NonNull,
    thiserror::Error,
};

pub struct SchedulerBindingsBridge<M> {
    allocator: Allocator,
    tpu_to_pack: shaq::spsc::Consumer<TpuToPackMessage>,
    progress_tracker: shaq::spsc::Consumer<ProgressMessage>,
    workers: Vec<SchedulerWorker>,

    progress: ProgressMessage,
    runtime: RuntimeState,
    state: SlotMap<TransactionKey, TransactionState>,

    _marker: core::marker::PhantomData<M>,
}

type Batch<'a, M> = TransactionPtrBatch<'a, KeyedTransactionMeta<M>>;

impl<M> SchedulerBindingsBridge<M>
where
    M: Copy,
{
    const TRANSACTION_BATCH_META_OFFSET: usize = Batch::<M>::TRANSACTION_META_START;
    const TRANSACTION_BATCH_SIZE: usize = Batch::<M>::TRANSACTION_META_END;

    /// Creates a new [`SchedulerBindingsBridge`] from a [`ClientSession`].
    ///
    /// # Note
    ///
    /// This bridge will leak any contained transaction allocations and in
    /// flight message batch allocations on drop. It is intended to have a 1 to
    /// 1 lifetime with the [`Allocator`] it owns (you should be dropping the
    /// allocator shortly after you drop the bridge).
    ///
    /// # Panics
    ///
    /// - If the session contains more than one allocator.
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
            runtime: RuntimeState {
                feature_set: FeatureSet::all_enabled(),
                fee_features: FeeFeatures {},
                lamports_per_signature: 5000,
                burn_percent: 50,
            },
            state: SlotMap::default(),

            _marker: core::marker::PhantomData,
        }
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn allocator(&self) -> &Allocator {
        &self.allocator
    }

    pub fn state(&self) -> &SlotMap<TransactionKey, TransactionState> {
        &self.state
    }

    pub fn runtime(&self) -> &RuntimeState {
        &self.runtime
    }

    pub fn progress(&self) -> &ProgressMessage {
        &self.progress
    }

    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    /// Looks up the given worker by ID.
    ///
    /// # Panics
    ///
    /// - If the worker does not exist.
    pub fn worker(&mut self, id: usize) -> &mut SchedulerWorker {
        &mut self.workers[id]
    }

    /// Looks up the given transaction key.
    ///
    /// # Panics
    ///
    /// - If the transaction does not exist.
    pub fn transaction(&self, key: TransactionKey) -> &TransactionState {
        let tx = &self.state[key];
        assert!(!tx.dead);

        tx
    }

    /// Inserts a non TPU transaction into the bridge.
    ///
    /// # Panics
    ///
    /// - If the transaction exceeds 4096 bytes.
    pub fn insert_transaction(
        &mut self,
        tx: &[u8],
    ) -> Result<TransactionKey, TransactionInsertError> {
        // TODO: Move to rts_alloc::MAX_ALLOC_SIZE once exposed.
        assert!(tx.len() <= 4096);

        let ptr = self
            .allocator
            .allocate(tx.len().try_into().expect("4096 fits in u32"))
            .ok_or(TransactionInsertError::Allocate)?;
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
        match SanitizedTransactionView::try_new_sanitized(tx, true) {
            Ok(tx) => {
                let key = self.state.insert(TransactionState {
                    dead: false,
                    borrows: 0,
                    flags: 0,
                    data: tx,
                    keys: None,
                });

                Ok(key)
            }
            Err(err) => {
                // SAFETY:
                // - We own `tx` exclusively.
                // - The previous `TransactionPtr` has been dropped by `try_new_sanitized`.
                unsafe {
                    self.allocator.free(ptr);
                }

                Err(TransactionInsertError::ParseSanitize(err))
            }
        }
    }

    pub fn drop_transaction(&mut self, key: TransactionKey) {
        // If we have requests that have borrowed this shared transaction region, then
        // we can't immediately clean up and must instead flag it as dead.
        match self.state[key].borrows {
            0 => {
                let state = self.state.remove(key).unwrap();

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

    pub fn drain_progress(&mut self) -> Option<ProgressMessage> {
        self.progress_tracker.sync();

        let mut received = false;
        while let Some(msg) = self.progress_tracker.try_read() {
            self.progress = *msg;
            received = true;
        }
        self.progress_tracker.finalize();

        received.then_some(self.progress)
    }

    pub fn tpu_len(&mut self) -> usize {
        self.tpu_to_pack.sync();

        self.tpu_to_pack.len()
    }

    pub fn drain_tpu(
        &mut self,
        mut callback: impl FnMut(&mut Self, TransactionKey) -> TxDecision,
        max_count: usize,
    ) -> usize {
        self.tpu_to_pack.sync();

        let additional = std::cmp::min(self.tpu_to_pack.len(), max_count);
        let mut sanitize_failures = 0usize;
        for _ in 0..additional {
            let msg = self.tpu_to_pack.try_read().expect("len checked above");

            // SAFETY:
            // - Trust Agave to have properly transferred ownership to use & not to
            //   free/access this.
            // - We are only creating a single exclusive pointer.
            let tx = unsafe {
                TransactionPtr::from_sharable_transaction_region(&msg.transaction, &self.allocator)
            };

            // Sanitize the transaction, drop it immediately if it fails sanitization.
            let Ok(tx) = SanitizedTransactionView::try_new_sanitized(tx, true) else {
                // SAFETY:
                // - We own `tx` exclusively.
                // - The previous `TransactionPtr` has been dropped by `try_new_sanitized`.
                unsafe {
                    self.allocator.free_offset(msg.transaction.offset);
                }

                sanitize_failures = sanitize_failures.wrapping_add(1);

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

            // Remove & free the TX if the scheduler doesn't want it.
            if callback(self, key) == TxDecision::Drop {
                let state = self.state.remove(key).unwrap();
                assert!(state.keys.is_none());
                assert_eq!(state.borrows, 0);

                // SAFETY:
                // - We own `tx` exclusively.
                unsafe { state.data.into_inner_data().free(&self.allocator) };
            }
        }

        self.tpu_to_pack.finalize();

        sanitize_failures
    }

    pub fn drain_worker(
        &mut self,
        worker: usize,
        mut callback: impl FnMut(&mut Self, WorkerResponse<'_, M>) -> TxDecision,
        max_count: usize,
    ) {
        self.workers[worker].0.worker_to_pack.sync();
        for _ in 0..max_count {
            let Some(rep) = self.workers[worker].0.worker_to_pack.try_read().copied() else {
                break;
            };
            self.handle_worker_response(rep, &mut callback);
        }
        self.workers[worker].0.worker_to_pack.finalize();
    }

    /// Builds & schedules the provided batch.
    ///
    /// # Panics
    ///
    /// - If the worker index does not exist.
    /// - If any transaction in the batch does not exist.
    /// - If the batch size exceeds [`MAX_TRANSACTIONS_PER_MESSAGE`].
    pub fn schedule(
        &mut self,
        ScheduleBatch {
            worker,
            transactions: batch,
            max_working_slot,
            flags,
        }: ScheduleBatch<&[KeyedTransactionMeta<M>]>,
    ) -> Result<(), ScheduleError> {
        let queue = &mut self.workers[worker].0.pack_to_worker;

        // Check we have space.
        queue.sync();
        if queue.len() == queue.capacity() {
            return Err(ScheduleError::Queue);
        }

        // Try allocate the batch.
        let batch = Self::collect_batch(&self.allocator, &mut self.state, batch)?;

        // Write the batch.
        queue
            .try_write(PackToWorkerMessage {
                flags,
                max_working_slot,
                batch,
            })
            .expect("space checked above");
        queue.commit();

        Ok(())
    }

    fn collect_batch(
        allocator: &Allocator,
        state: &mut SlotMap<TransactionKey, TransactionState>,
        batch: &[KeyedTransactionMeta<M>],
    ) -> Result<SharableTransactionBatchRegion, ScheduleError> {
        assert!(batch.len() <= MAX_TRANSACTIONS_PER_MESSAGE);

        // Allocate a batch that can hold all our transaction pointers.
        let transactions = allocator
            .allocate(Self::TRANSACTION_BATCH_SIZE as u32)
            .ok_or(ScheduleError::Allocation)?;
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
                .byte_add(Self::TRANSACTION_BATCH_META_OFFSET)
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

        Ok(SharableTransactionBatchRegion {
            num_transactions: batch.len().try_into().unwrap(),
            transactions_offset,
        })
    }

    fn handle_worker_response(
        &mut self,
        rep: WorkerToPackMessage,
        callback: &mut impl FnMut(&mut Self, WorkerResponse<'_, M>) -> TxDecision,
    ) {
        // Get transaction & meta pointers.
        let transactions = unsafe {
            self.allocator
                .ptr_from_offset(rep.batch.transactions_offset)
                .cast::<SharableTransactionRegion>()
        };
        // SAFETY:
        // - We ensured that this batch was originally allocated to support M.
        let metas = unsafe {
            transactions
                .byte_add(Batch::<M>::TRANSACTION_META_START)
                .cast()
        };

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
            let decision = self.handle_transaction_response(key, meta, index, &responses, callback);

            // Remove the tx from state & drop the allocation if requested.
            if decision == TxDecision::Drop {
                self.drop_transaction(key);
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
        callback: &mut impl FnMut(&mut Self, WorkerResponse<'_, M>) -> TxDecision,
    ) -> TxDecision {
        // Decrease the borrow counter as Agave has returned ownership to us.
        let state = &mut self.state[key];
        state.borrows = state.borrows.checked_sub(1).unwrap();

        // Only callback if this state is not already dead (scheduler requested drop).
        match (state.dead, responses) {
            (true, WorkerResponseBatch::Check(rep)) => {
                // SAFETY
                // - We trust Agave to have correctly allocated the responses.
                let rep = unsafe { rep.add(index).read() };

                // Free shared pubkeys if there are any.
                if rep.resolved_pubkeys.num_pubkeys > 0 {
                    // SAFETY
                    // - Region exists as `num_pubkeys > 0`.
                    // - Trust Agave to have allocated this region correctly.
                    // - We now own it exclusively.
                    unsafe {
                        let keys = PubkeysPtr::from_sharable_pubkeys(
                            &rep.resolved_pubkeys,
                            &self.allocator,
                        );
                        keys.free(&self.allocator);
                    };
                }

                TxDecision::Drop
            }
            (true, _) => TxDecision::Drop,
            (false, WorkerResponseBatch::Unprocessed) => {
                let rep = WorkerResponse {
                    key,
                    meta,
                    response: WorkerAction::Unprocessed,
                };

                callback(self, rep)
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

                callback(self, rep)
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
                let decision = callback(
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

pub struct SchedulerWorker(ClientWorkerSession);

impl SchedulerWorker {
    pub fn is_empty(&mut self) -> bool {
        self.len() == 0
    }

    pub fn len(&mut self) -> usize {
        self.0.pack_to_worker.sync();

        self.0.pack_to_worker.len()
    }

    pub fn rem(&mut self) -> usize {
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

pub struct RuntimeState {
    pub feature_set: FeatureSet,
    pub fee_features: FeeFeatures,
    pub lamports_per_signature: u64,
    pub burn_percent: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduleBatch<T> {
    pub worker: usize,
    pub transactions: T,
    pub max_working_slot: u64,
    pub flags: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ScheduleError {
    #[error("Queue full")]
    Queue,
    #[error("Allocation failed")]
    Allocation,
}

#[derive(Debug, Clone)]
pub struct WorkerResponse<'a, M> {
    pub key: TransactionKey,
    pub meta: M,
    pub response: WorkerAction<'a>,
}

#[derive(Debug, Clone)]
pub enum WorkerAction<'a> {
    Unprocessed,
    Check(CheckResponse, Option<&'a PubkeysPtr>),
    Execute(ExecutionResponse),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyedTransactionMeta<M> {
    pub key: TransactionKey,
    pub meta: M,
}

slotmap::new_key_type! {
    pub struct TransactionKey;
}

#[derive(Debug)]
pub struct TransactionState {
    pub dead: bool,
    pub borrows: u64,
    pub flags: u8,
    pub data: SanitizedTransactionView<TransactionPtr>,
    pub keys: Option<PubkeysPtr>,
}

impl TransactionState {
    #[must_use]
    pub const fn is_simple_vote(&self) -> bool {
        self.flags & tpu_message_flags::IS_SIMPLE_VOTE != 0
    }

    pub fn locks(&self) -> impl Iterator<Item = (&Pubkey, bool)> {
        self.write_locks()
            .map(|lock| (lock, true))
            .chain(self.read_locks().map(|lock| (lock, false)))
    }

    pub fn write_locks(&self) -> impl Iterator<Item = &Pubkey> {
        self.data
            .static_account_keys()
            .iter()
            .chain(self.keys.iter().flat_map(|keys| keys.as_slice().iter()))
            .enumerate()
            .filter(|(i, _)| self.is_writable(*i as u8))
            .map(|(_, key)| key)
    }

    pub fn read_locks(&self) -> impl Iterator<Item = &Pubkey> {
        self.data
            .static_account_keys()
            .iter()
            .chain(self.keys.iter().flat_map(|keys| keys.as_slice().iter()))
            .enumerate()
            .filter(|(i, _)| !self.is_writable(*i as u8))
            .map(|(_, key)| key)
    }

    /// Determines if the account at `index` is writable based on its position
    /// in the transaction header. This is a simplified version of the canonical
    /// implementation in `ResolvedTransactionView::cache_is_writable` that
    /// intentionally omits:
    ///
    /// - **Reserved account key demotion**: reserved accounts (sysvars, builtins)
    ///   in writable positions are not demoted to read-only.
    /// - **Program account demotion**: writable program accounts are not demoted
    ///   when `bpf_loader_upgradeable` is absent.
    ///
    /// Both omissions make this implementation **conservatively over-report**
    /// writability, which may reduce scheduling parallelism but cannot cause
    /// incorrect lock conflicts.
    fn is_writable(&self, index: u8) -> bool {
        if index >= self.data.num_static_account_keys() {
            let loaded_address_index = index.wrapping_sub(self.data.num_static_account_keys());
            loaded_address_index < self.data.total_writable_lookup_accounts() as u8
        } else {
            index
                < self
                    .data
                    .num_required_signatures()
                    .wrapping_sub(self.data.num_readonly_signed_static_accounts())
                || (index >= self.data.num_required_signatures()
                    && index
                        < (self.data.static_account_keys().len() as u8)
                            .wrapping_sub(self.data.num_readonly_unsigned_static_accounts()))
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum TxDecision {
    Keep,
    Drop,
}

#[derive(Debug, PartialEq, Eq, Error)]
pub enum TransactionInsertError {
    #[error("Failed to parse or sanitize; err={0:?}")]
    ParseSanitize(TransactionViewError),
    #[error("Failed to allocate")]
    Allocate,
}
