use {
    crate::{
        bridge::{
            Bridge, KeyedTransactionMeta, RuntimeState, ScheduleBatch, TransactionKey,
            TransactionState, TxDecision, Worker, WorkerAction, WorkerResponse,
        },
        pubkeys_ptr::PubkeysPtr,
        transaction_ptr::TransactionPtr,
    },
    agave_feature_set::FeatureSet,
    agave_scheduler_bindings::{
        ProgressMessage, SharablePubkeys, pack_message_flags,
        worker_message_types::{
            CheckResponse, ExecutionResponse, fee_payer_balance_flags, not_included_reasons,
            resolve_flags, status_check_flags,
        },
    },
    agave_transaction_view::{
        result::TransactionViewError, transaction_data::TransactionData,
        transaction_view::SanitizedTransactionView,
    },
    slotmap::SlotMap,
    solana_fee::FeeFeatures,
    solana_pubkey::Pubkey,
    solana_transaction::versioned::VersionedTransaction,
    std::{collections::VecDeque, ptr::NonNull},
};

pub struct TestBridge<M> {
    progress_queue: VecDeque<ProgressMessage>,
    tpu_queue: VecDeque<TransactionKey>,
    worker_queues: Vec<VecDeque<(KeyedTransactionMeta<M>, WorkerActionLite)>>,
    workers: Vec<TestWorker>,
    scheduled: VecDeque<ScheduleBatch<Vec<KeyedTransactionMeta<M>>>>,

    progress: ProgressMessage,
    runtime: RuntimeState,
    state: SlotMap<TransactionKey, TransactionState>,
}

impl<M> TestBridge<M>
where
    M: Copy,
{
    #[must_use]
    pub fn new(worker_count: usize, worker_req_cap: usize) -> Self {
        Self {
            progress_queue: VecDeque::default(),
            tpu_queue: VecDeque::default(),
            worker_queues: vec![VecDeque::default(); worker_count],
            workers: vec![
                TestWorker {
                    len: 0,
                    cap: worker_req_cap
                };
                worker_count
            ],
            scheduled: VecDeque::default(),

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
                fee_features: FeeFeatures {
                    enable_secp256r1_precompile: true,
                },
                lamports_per_signature: 5000,
                burn_percent: 50,
            },
            state: SlotMap::default(),
        }
    }

    pub fn queue_progress(&mut self, progress: ProgressMessage) {
        self.progress_queue.push_back(progress);
    }

    pub fn queue_tpu(&mut self, tx: &VersionedTransaction) {
        // Serialize the transaction & get a raw pointer.
        let mut serialized = bincode::serialize(tx).unwrap();
        let len = serialized.len();
        let data = NonNull::new(serialized.as_mut_ptr()).unwrap();
        core::mem::forget(serialized);

        // Construct our TransactionPtr & sanitized view.
        //
        // SAFETY
        // - We own this allocation exclusively & len is accurate.
        let data = unsafe { TransactionPtr::from_raw_parts(data, len) };
        let data = SanitizedTransactionView::try_new_sanitized(data, true, true).unwrap();

        // Insert into state & store the key in the tpu queue.
        let key = self.state.insert(TransactionState {
            dead: false,
            borrows: 0,
            flags: 0,
            data,
            keys: None,
        });
        self.tpu_queue.push_back(key);
    }

    pub fn queue_check_response(
        &mut self,
        batch: &ScheduleBatch<Vec<KeyedTransactionMeta<M>>>,
        index: usize,
        keys: Option<Vec<Pubkey>>,
    ) {
        self.queue_check_response_with(batch, index, keys, self.check_ok());
    }

    pub fn queue_check_response_with(
        &mut self,
        batch: &ScheduleBatch<Vec<KeyedTransactionMeta<M>>>,
        index: usize,
        keys: Option<Vec<Pubkey>>,
        response: CheckResponse,
    ) {
        let tx = batch.transactions[index];

        // Insert the keys (if any).
        self.state[tx.key].keys = keys.map(Self::allocate_pubkeys_ptr);

        let rep = (tx, WorkerActionLite::Check(response));
        self.worker_queues[batch.worker].push_back(rep);
    }

    pub fn queue_all_checks_ok(&mut self) {
        while let Some(batch) = self.pop_schedule() {
            assert_eq!(batch.flags & 1, pack_message_flags::CHECK);
            assert!(batch.max_working_slot >= self.progress.current_slot);

            for i in 0..batch.transactions.len() {
                self.queue_check_response(&batch, i, None);
            }
        }
    }

    pub fn queue_execute_response(
        &mut self,
        batch: &ScheduleBatch<Vec<KeyedTransactionMeta<M>>>,
        index: usize,
        response: ExecutionResponse,
    ) {
        let tx = batch.transactions[index];
        let rep = (tx, WorkerActionLite::Execute(response));
        self.worker_queues[batch.worker].push_back(rep);
    }

    pub fn queue_unprocessed_response(
        &mut self,
        batch: &ScheduleBatch<Vec<KeyedTransactionMeta<M>>>,
        index: usize,
    ) {
        let tx = batch.transactions[index];
        let rep = (tx, WorkerActionLite::Unprocessed);
        self.worker_queues[batch.worker].push_back(rep);
    }

    #[must_use]
    pub fn execute_ok(&self) -> ExecutionResponse {
        ExecutionResponse {
            execution_slot: self.progress.current_slot,
            not_included_reason: not_included_reasons::NONE,
            cost_units: 0,
            fee_payer_balance: u64::from(u32::MAX),
        }
    }

    #[must_use]
    pub fn execute_err(&self, reason: u8) -> ExecutionResponse {
        ExecutionResponse {
            execution_slot: self.progress.current_slot,
            not_included_reason: reason,
            cost_units: 0,
            fee_payer_balance: u64::from(u32::MAX),
        }
    }

    pub fn pop_schedule(&mut self) -> Option<ScheduleBatch<Vec<KeyedTransactionMeta<M>>>> {
        self.scheduled.pop_front()
    }

    #[must_use]
    pub fn tx_count(&self) -> usize {
        self.state.len()
    }

    #[must_use]
    pub fn contains_tx(&self, key: TransactionKey) -> bool {
        self.state.contains_key(key)
    }

    #[must_use]
    pub fn check_ok(&self) -> CheckResponse {
        CheckResponse {
            parsing_and_sanitization_flags: 0,
            status_check_flags: status_check_flags::REQUESTED | status_check_flags::PERFORMED,
            fee_payer_balance_flags: fee_payer_balance_flags::REQUESTED
                | fee_payer_balance_flags::PERFORMED,
            resolve_flags: resolve_flags::REQUESTED | resolve_flags::PERFORMED,
            included_slot: self.progress.current_slot,
            balance_slot: self.progress.current_slot,
            fee_payer_balance: u64::from(u32::MAX),
            resolution_slot: self.progress.current_slot,
            min_alt_deactivation_slot: u64::MAX,
            resolved_pubkeys: SharablePubkeys {
                offset: 0,
                num_pubkeys: 0,
            },
        }
    }

    fn allocate_pubkeys_ptr(mut keys: Vec<Pubkey>) -> PubkeysPtr {
        // Get the raw pointer components.
        let len = keys.len();
        let data = NonNull::new(keys.as_mut_ptr()).unwrap();
        core::mem::forget(keys);

        // Construct our PubkeysPtr.
        //
        // SAFETY
        // - We own this allocation exclusively & len is accurate.
        unsafe { PubkeysPtr::from_raw_parts(data, len) }
    }
}

impl<M> Bridge for TestBridge<M>
where
    M: Copy,
{
    type Worker = TestWorker;
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
        // Copy the transaction bytes & get a raw pointer.
        let mut serialized = tx.to_vec();
        let len = serialized.len();
        let data = NonNull::new(serialized.as_mut_ptr()).unwrap();
        core::mem::forget(serialized);

        // Construct our TransactionPtr & sanitized view.
        //
        // SAFETY
        // - We own this allocation exclusively & len is accurate.
        let data = unsafe { TransactionPtr::from_raw_parts(data, len) };
        let data = SanitizedTransactionView::try_new_sanitized(data, true, true)?;

        // Insert into state & return the key.
        let key = self.state.insert(TransactionState {
            dead: false,
            borrows: 0,
            flags: 0,
            data,
            keys: None,
        });

        Ok(key)
    }

    fn tx_drop(&mut self, key: TransactionKey) {
        self.state.remove(key).unwrap();
    }

    fn drain_progress(&mut self) -> Option<ProgressMessage> {
        let latest = self.progress_queue.back().copied();
        if let Some(progress) = latest {
            self.progress = progress;
        }
        self.progress_queue.clear();

        latest
    }

    fn tpu_len(&mut self) -> usize {
        self.tpu_queue.len()
    }

    fn tpu_drain(
        &mut self,
        mut cb: impl FnMut(&mut Self, TransactionKey) -> TxDecision,
        max_count: usize,
    ) {
        for _ in 0..max_count {
            let Some(tx) = self.tpu_queue.pop_front() else {
                return;
            };

            if cb(self, tx) == TxDecision::Drop {
                self.state.remove(tx).unwrap();
            }
        }
    }

    fn worker_drain(
        &mut self,
        worker: usize,
        mut cb: impl FnMut(&mut Self, WorkerResponse<'_, Self::Meta>) -> TxDecision,
        max_count: usize,
    ) {
        for _ in 0..max_count {
            let Some((KeyedTransactionMeta { key, meta }, rep)) =
                self.worker_queues[worker].pop_front()
            else {
                return;
            };

            // Temporarily take keys to avoid mutable aliasing self.
            let keys = self.state[key].keys.take();
            let response = match rep {
                WorkerActionLite::Unprocessed => WorkerAction::Unprocessed,
                WorkerActionLite::Check(rep) => WorkerAction::Check(rep, keys.as_ref()),
                WorkerActionLite::Execute(rep) => WorkerAction::Execute(rep),
            };

            match cb(
                self,
                WorkerResponse {
                    key,
                    meta,
                    response,
                },
            ) {
                // Restore keys.
                TxDecision::Keep => self.state[key].keys = keys,
                TxDecision::Drop => {
                    let state = self.state.remove(key).unwrap();

                    // Drop the underlying transaction allocation.
                    let data = state.data.inner_data().data();
                    let len = data.len();
                    let ptr = data.as_ptr();
                    drop(state);
                    // SAFETY
                    // - We original allocated this and exclusively own it, so it's safe for us to
                    //   deallocate.
                    unsafe {
                        let allocation = core::slice::from_raw_parts_mut(ptr.cast_mut(), len);
                        core::ptr::drop_in_place(allocation);
                    }

                    // Drop the underlying pubkeys allocation.
                    if let Some(keys) = keys {
                        let slice = keys.as_slice();
                        let len = slice.len();
                        let ptr = slice.as_ptr();
                        // SAFETY
                        // - We original allocated this and exclusively own it, so it's safe for us
                        //   to deallocate.
                        unsafe {
                            let allocation = core::slice::from_raw_parts_mut(ptr.cast_mut(), len);
                            core::ptr::drop_in_place(allocation);
                        }
                    }
                }
            }
        }
    }

    fn schedule(
        &mut self,
        ScheduleBatch {
            worker,
            transactions,
            max_working_slot,
            flags,
        }: ScheduleBatch<&[KeyedTransactionMeta<M>]>,
    ) {
        self.scheduled.push_back(ScheduleBatch {
            worker,
            transactions: transactions.to_vec(),
            max_working_slot,
            flags,
        });
    }
}

#[derive(Debug, Clone)]
pub struct TestWorker {
    len: usize,
    cap: usize,
}

impl Worker for TestWorker {
    fn len(&mut self) -> usize {
        self.len
    }

    fn rem(&mut self) -> usize {
        self.cap - self.len
    }
}

#[derive(Debug, Clone)]
enum WorkerActionLite {
    Unprocessed,
    Check(CheckResponse),
    Execute(ExecutionResponse),
}
