use {
    super::{
        consumer::{Consumer, ExecuteAndCommitTransactionsOutput, ProcessTransactionBatchOutput},
        leader_slot_timing_metrics::LeaderExecuteAndCommitTimings,
        scheduler_messages::{ConsumeWork, FinishedConsumeWork},
    },
    crate::banking_stage::consumer::{ExecutionFlags, RetryableIndex},
    crossbeam_channel::{Receiver, SendError, Sender, TryRecvError},
    solana_poh::poh_recorder::{LeaderState, SharedLeaderState},
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    solana_svm::transaction_error_metrics::TransactionErrorMetrics,
    solana_time_utils::AtomicInterval,
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    },
    thiserror::Error,
};

#[derive(Debug, Error)]
pub enum ConsumeWorkerError<Tx> {
    #[error("Failed to receive work from scheduler: {0}")]
    Recv(#[from] TryRecvError),
    #[error("Failed to send finalized consume work to scheduler: {0}")]
    Send(#[from] SendError<FinishedConsumeWork<Tx>>),
}

enum ProcessingStatus<Tx> {
    Processed,
    /// Work could not be processed due to lack of bank.
    CouldNotProcess(ConsumeWork<Tx>),
}

pub(crate) struct ConsumeWorker<Tx> {
    exit: Arc<AtomicBool>,
    consume_receiver: Receiver<ConsumeWork<Tx>>,
    consumer: Consumer,
    consumed_sender: Sender<FinishedConsumeWork<Tx>>,

    shared_leader_state: SharedLeaderState,
    metrics: Arc<ConsumeWorkerMetrics>,
}

impl<Tx: TransactionWithMeta> ConsumeWorker<Tx> {
    pub fn new(
        id: u32,
        exit: Arc<AtomicBool>,
        consume_receiver: Receiver<ConsumeWork<Tx>>,
        consumer: Consumer,
        consumed_sender: Sender<FinishedConsumeWork<Tx>>,
        shared_leader_state: SharedLeaderState,
    ) -> Self {
        Self {
            exit,
            consume_receiver,
            consumer,
            consumed_sender,
            shared_leader_state,
            metrics: Arc::new(ConsumeWorkerMetrics::new(id)),
        }
    }

    pub fn metrics_handle(&self) -> Arc<ConsumeWorkerMetrics> {
        self.metrics.clone()
    }

    pub fn run(self) -> Result<(), ConsumeWorkerError<Tx>> {
        let mut did_work = false;
        let mut last_empty_time = Instant::now();
        let mut sleep_duration = STARTING_SLEEP_DURATION;

        while !self.exit.load(Ordering::Relaxed) {
            match self.consume_receiver.try_recv() {
                Ok(work) => {
                    did_work = true;
                    match self.consume(work)? {
                        ProcessingStatus::Processed => {}
                        ProcessingStatus::CouldNotProcess(work) => {
                            self.retry_drain(work)?;
                        }
                    }
                }
                Err(TryRecvError::Empty) => {
                    let now = Instant::now();

                    if did_work {
                        last_empty_time = now;
                    }
                    did_work = false;
                    let idle_duration = now.duration_since(last_empty_time);
                    sleep_duration = backoff(idle_duration, &sleep_duration);
                }
                Err(TryRecvError::Disconnected) => {
                    return Err(ConsumeWorkerError::Recv(TryRecvError::Disconnected));
                }
            }
        }

        Ok(())
    }

    fn consume(
        &self,
        work: ConsumeWork<Tx>,
    ) -> Result<ProcessingStatus<Tx>, ConsumeWorkerError<Tx>> {
        let Some(leader_state) = active_leader_state(&self.shared_leader_state) else {
            return Ok(ProcessingStatus::CouldNotProcess(work));
        };
        let bank = leader_state
            .working_bank()
            .expect("active_leader_state should only return an active bank");
        if bank.slot() != work.target_slot {
            return Ok(ProcessingStatus::CouldNotProcess(work));
        }
        self.metrics
            .count_metrics
            .num_messages_processed
            .fetch_add(1, Ordering::Relaxed);

        let output = self.consumer.process_and_record_aged_transactions(
            bank,
            &work.transactions,
            &work.max_ages,
            &ExecutionFlags {
                drop_on_failure: false,
                all_or_nothing: false,
            },
        );
        self.metrics.update_for_consume(&output);
        self.metrics.has_data.store(true, Ordering::Relaxed);

        self.consumed_sender.send(FinishedConsumeWork {
            work,
            retryable_indexes: output
                .execute_and_commit_transactions_output
                .retryable_transaction_indexes,
        })?;
        Ok(ProcessingStatus::Processed)
    }

    /// Retry current batch and all outstanding batches.
    fn retry_drain(&self, work: ConsumeWork<Tx>) -> Result<(), ConsumeWorkerError<Tx>> {
        for work in try_drain_iter(work, &self.consume_receiver) {
            if self.exit.load(Ordering::Relaxed) {
                return Ok(());
            }
            self.retry(work)?;
        }
        Ok(())
    }

    /// Send transactions back to scheduler as retryable.
    fn retry(&self, work: ConsumeWork<Tx>) -> Result<(), ConsumeWorkerError<Tx>> {
        let retryable_indexes: Vec<_> = (0..work.transactions.len())
            .map(|index| RetryableIndex {
                index,
                immediately_retryable: true,
            })
            .collect();
        let num_retryable = retryable_indexes.len();
        self.metrics
            .count_metrics
            .retryable_transaction_count
            .fetch_add(num_retryable, Ordering::Relaxed);
        self.metrics
            .count_metrics
            .retryable_expired_bank_count
            .fetch_add(num_retryable, Ordering::Relaxed);
        self.metrics.has_data.store(true, Ordering::Relaxed);
        self.consumed_sender.send(FinishedConsumeWork {
            work,
            retryable_indexes,
        })?;
        Ok(())
    }
}

#[cfg(unix)]
pub(crate) mod external {
    use {
        super::*,
        crate::banking_stage::{
            committer::CommitTransactionDetails,
            scheduler_messages::MaxAge,
            transaction_scheduler::receive_and_buffer::{
                PacketHandlingError, translate_to_runtime_view,
            },
        },
        agave_scheduler_bindings::{
            ExecutionResponseRegion, ExecutionWorkerToPackMessage, MAX_TRANSACTIONS_PER_MESSAGE,
            PackToExecutionWorkerMessage, execution_message_flags,
            worker_message_types::{ExecutionResponse, not_included_reasons},
        },
        agave_scheduling_utils::{
            error::transaction_error_to_not_included_reason,
            responses_region::execution_responses_from_iter,
            transaction_ptr::{TransactionPtr, TransactionPtrBatch},
        },
        agave_transaction_view::{
            resolved_transaction_view::ResolvedTransactionView, sanitize::SanitizeConfig,
        },
        arrayvec::ArrayVec,
        solana_cost_model::cost_model::CostModel,
        solana_runtime::bank::Bank,
        solana_runtime_transaction::{
            runtime_transaction::RuntimeTransaction, sanitize_config::sanitize_config,
        },
        std::num::NonZeroUsize,
    };

    #[derive(Debug, Error)]
    pub enum ExternalConsumeWorkerError {
        #[error("Sender disconnected")]
        SenderDisconnected,
        #[error("Allocation failed")]
        AllocationFailure,
    }

    pub(crate) struct ExternalWorker {
        exit: Arc<AtomicBool>,
        consumer: Consumer,
        sender: shaq::spsc::Producer<ExecutionWorkerToPackMessage>,
        allocator: rts_alloc::Allocator,

        shared_leader_state: SharedLeaderState,
        metrics: Arc<ConsumeWorkerMetrics>,
    }

    type Tx = RuntimeTransaction<ResolvedTransactionView<TransactionPtr>>;
    enum IterationResult {
        ProcessedMessage,
        Idle,
    }

    impl ExternalWorker {
        pub fn new(
            id: u32,
            exit: Arc<AtomicBool>,
            consumer: Consumer,
            sender: shaq::spsc::Producer<ExecutionWorkerToPackMessage>,
            allocator: rts_alloc::Allocator,
            shared_leader_state: SharedLeaderState,
        ) -> Self {
            Self {
                exit,
                consumer,
                sender,
                allocator,
                shared_leader_state,
                metrics: Arc::new(ConsumeWorkerMetrics::new(id)),
            }
        }

        pub fn metrics_handle(&self) -> Arc<ConsumeWorkerMetrics> {
            self.metrics.clone()
        }

        pub fn run(
            mut self,
            mut receiver: shaq::spsc::Consumer<PackToExecutionWorkerMessage>,
        ) -> Result<(), ExternalConsumeWorkerError> {
            let mut should_drain_executes = false;
            let mut did_work = false;
            let mut last_empty_time = Instant::now();
            let mut sleep_duration = STARTING_SLEEP_DURATION;

            while !self.exit.load(Ordering::Relaxed) {
                match self.iterate(&mut receiver, &mut should_drain_executes)? {
                    IterationResult::ProcessedMessage => {
                        did_work = true;
                    }
                    IterationResult::Idle => {
                        let now = Instant::now();

                        if did_work {
                            last_empty_time = now;
                        }
                        did_work = false;
                        let idle_duration = now.duration_since(last_empty_time);
                        sleep_duration = backoff(idle_duration, &sleep_duration);
                    }
                }
            }

            Ok(())
        }

        fn iterate(
            &mut self,
            receiver: &mut shaq::spsc::Consumer<PackToExecutionWorkerMessage>,
            should_drain_executes: &mut bool,
        ) -> Result<IterationResult, ExternalConsumeWorkerError> {
            self.allocator.clean_remote_frees();
            let capacity = NonZeroUsize::new(receiver.capacity())
                .expect("shaq queue capacity must be non-zero");
            let Some(messages) = receiver.try_reserve_read_batch(capacity) else {
                return Ok(IterationResult::Idle);
            };

            *should_drain_executes = false;
            for message in messages {
                // If the bank is unavailable, drain executes for the remainder of the batch.
                *should_drain_executes |= self.process_message(&message, *should_drain_executes)?;
            }

            Ok(IterationResult::ProcessedMessage)
        }

        /// Return true if fetching a bank for execution timed out.
        fn process_message(
            &mut self,
            message: &PackToExecutionWorkerMessage,
            should_drain_executes: bool,
        ) -> Result<bool, ExternalConsumeWorkerError> {
            if !Self::validate_message(message) {
                return self
                    .return_unprocessed_message(
                        message,
                        agave_scheduler_bindings::processed_codes::INVALID,
                    )
                    .map(|()| false);
            }

            self.metrics
                .count_metrics
                .num_messages_processed
                .fetch_add(1, Ordering::Relaxed);

            self.execute_batch(message, should_drain_executes)
        }

        /// Return true if fetching a bank for execution timed out.
        fn execute_batch(
            &mut self,
            message: &PackToExecutionWorkerMessage,
            should_drain_executes: bool,
        ) -> Result<bool, ExternalConsumeWorkerError> {
            if should_drain_executes {
                return self
                    .return_not_included_with_reason(
                        message,
                        not_included_reasons::BANK_NOT_AVAILABLE,
                        0,
                    )
                    .map(|()| true);
            }

            let Some(leader_state) = active_leader_state(&self.shared_leader_state) else {
                return self
                    .return_not_included_with_reason(
                        message,
                        not_included_reasons::BANK_NOT_AVAILABLE,
                        0,
                    )
                    .map(|()| true);
            };

            let bank = leader_state
                .working_bank()
                .expect("active_leader_state should only return an active bank");
            if bank.slot() > message.max_working_slot {
                return self
                    .return_unprocessed_message(
                        message,
                        agave_scheduler_bindings::processed_codes::MAX_WORKING_SLOT_EXCEEDED,
                    )
                    .map(|()| false);
            }

            // SAFETY: Assumption that external scheduler does not pass messages with batch regions
            //         not pointing to valid regions in the allocator.
            let batch = unsafe {
                TransactionPtrBatch::from_sharable_transaction_batch_region(
                    &message.batch,
                    &self.allocator,
                )
            };
            let (translation_results, transactions, max_ages) =
                Self::translate_transaction_batch(&batch, bank);

            // Enforce all or nothing on translation_results.
            let execution_flags = ExecutionFlags {
                drop_on_failure: message.flags & execution_message_flags::DROP_ON_FAILURE != 0,
                all_or_nothing: message.flags & execution_message_flags::ALL_OR_NOTHING != 0,
            };
            if execution_flags.all_or_nothing && translation_results.len() != transactions.len() {
                self.send_execution_response(
                    message,
                    Self::all_or_nothing_translate_iterator(&translation_results, bank.slot()),
                )?;

                return Ok(false);
            }
            let output = self.consumer.process_and_record_aged_transactions(
                bank,
                &transactions,
                &max_ages,
                &execution_flags,
            );

            self.metrics.update_for_consume(&output);
            self.metrics.has_data.store(true, Ordering::Relaxed);

            let Ok(commit_results) = output
                .execute_and_commit_transactions_output
                .commit_transactions_result
            else {
                // Recording failed (slot ended during processing).
                // Return as bank not available so the scheduler can retry.
                return self
                    .return_not_included_with_reason(
                        message,
                        not_included_reasons::BANK_NOT_AVAILABLE,
                        bank.slot(),
                    )
                    .map(|()| true);
            };

            self.send_execution_response(
                message,
                Self::consume_response_iterator(
                    &translation_results,
                    &transactions,
                    &commit_results,
                    bank,
                    &execution_flags,
                ),
            )?;

            Ok(false)
        }

        fn send_execution_response(
            &mut self,
            message: &PackToExecutionWorkerMessage,
            iter: impl ExactSizeIterator<Item = ExecutionResponse>,
        ) -> Result<(), ExternalConsumeWorkerError> {
            let responses = execution_responses_from_iter(&self.allocator, iter)
                .ok_or(ExternalConsumeWorkerError::AllocationFailure)?;
            let response = ExecutionWorkerToPackMessage {
                batch: message.batch,
                processed_code: agave_scheduler_bindings::processed_codes::PROCESSED,
                responses,
            };

            self.sender
                .try_write(response)
                .map_err(|_| ExternalConsumeWorkerError::SenderDisconnected)?;

            Ok(())
        }

        fn all_or_nothing_translate_iterator(
            translation_results: &[Result<(), PacketHandlingError>],
            execution_slot: u64,
        ) -> impl ExactSizeIterator<Item = ExecutionResponse> + '_ {
            translation_results
                .iter()
                .map(move |res| ExecutionResponse {
                    execution_slot,
                    not_included_reason: match res {
                        Ok(_) => not_included_reasons::ALL_OR_NOTHING_BATCH_FAILURE,
                        Err(err) => Self::reason_from_packet_handling_error(err),
                    },
                    cost_units: 0,
                    fee_payer_balance: 0,
                })
        }

        fn consume_response_iterator<'a>(
            translation_results: &'a [Result<(), PacketHandlingError>],
            transactions: &'a [impl TransactionWithMeta],
            commit_results: &'a [CommitTransactionDetails],
            bank: &'a Bank,
            execution_flags: &'a ExecutionFlags,
        ) -> impl ExactSizeIterator<Item = ExecutionResponse> + 'a {
            assert_eq!(transactions.len(), commit_results.len());
            let mut transactions_iterator = transactions.iter();
            let mut commit_result_iterator = commit_results.iter();

            translation_results
                .iter()
                .map(move |translation_result| match translation_result {
                    Ok(()) => {
                        let tx = transactions_iterator.next().expect(
                            "transactions must contain element for each successfully translated \
                             result",
                        );
                        let commit_details = commit_result_iterator.next().expect(
                            "commit result iterator must contain element for each sent transaction",
                        );
                        Self::response_from_commit_details(
                            tx,
                            commit_details,
                            bank,
                            execution_flags,
                        )
                    }
                    Err(err) => ExecutionResponse {
                        execution_slot: bank.slot(),
                        not_included_reason: Self::reason_from_packet_handling_error(err),
                        cost_units: 0,
                        fee_payer_balance: 0,
                    },
                })
        }

        /// Return all transactions in the batch as not included with the provided
        /// reason.
        fn return_not_included_with_reason(
            &mut self,
            message: &PackToExecutionWorkerMessage,
            reason: u8,
            execution_slot: u64,
        ) -> Result<(), ExternalConsumeWorkerError> {
            let response_region = execution_responses_from_iter(
                &self.allocator,
                (0..message.batch.num_transactions).map(|_| ExecutionResponse {
                    execution_slot,
                    not_included_reason: reason,
                    cost_units: 0,
                    fee_payer_balance: 0,
                }),
            )
            .ok_or(ExternalConsumeWorkerError::AllocationFailure)?;

            let response_message = ExecutionWorkerToPackMessage {
                batch: message.batch,
                processed_code: agave_scheduler_bindings::processed_codes::PROCESSED,
                responses: response_region,
            };

            // Should de-allocate the memory, but this is a non-recoverable
            // error and so it's not needed.
            self.sender
                .try_write(response_message)
                .map_err(|_| ExternalConsumeWorkerError::SenderDisconnected)?;

            Ok(())
        }

        fn return_unprocessed_message(
            &mut self,
            message: &PackToExecutionWorkerMessage,
            processed_code: u8,
        ) -> Result<(), ExternalConsumeWorkerError> {
            assert_ne!(
                processed_code,
                agave_scheduler_bindings::processed_codes::PROCESSED
            );
            let response = ExecutionWorkerToPackMessage {
                batch: message.batch,
                processed_code,
                responses: ExecutionResponseRegion {
                    num_transaction_responses: 0,
                    transaction_responses_offset: 0,
                },
            };

            self.sender
                .try_write(response)
                .map_err(|_| ExternalConsumeWorkerError::SenderDisconnected)?;

            Ok(())
        }

        /// Translate batch of transactions into usable
        fn translate_transaction_batch(
            batch: &TransactionPtrBatch,
            bank: &Bank,
        ) -> (
            ArrayVec<Result<(), PacketHandlingError>, MAX_TRANSACTIONS_PER_MESSAGE>,
            ArrayVec<Tx, MAX_TRANSACTIONS_PER_MESSAGE>,
            ArrayVec<MaxAge, MAX_TRANSACTIONS_PER_MESSAGE>,
        ) {
            let sanitize_config = sanitize_config();
            let transaction_account_lock_limit = bank.get_transaction_account_lock_limit();

            let mut translation_results = ArrayVec::new();
            let mut transactions = ArrayVec::new();
            let mut max_ages = ArrayVec::new();
            for (transaction_ptr, _) in batch.iter() {
                match Self::translate_transaction(
                    transaction_ptr,
                    bank,
                    transaction_account_lock_limit,
                    &sanitize_config,
                ) {
                    Ok((tx, max_age)) => {
                        transactions.push(tx);
                        max_ages.push(max_age);
                        translation_results.push(Ok(()));
                    }
                    Err(err) => translation_results.push(Err(err)),
                }
            }

            (translation_results, transactions, max_ages)
        }

        fn translate_transaction(
            transaction_ptr: TransactionPtr,
            bank: &Bank,
            transaction_account_lock_limit: usize,
            sanitize_config: &SanitizeConfig,
        ) -> Result<(Tx, MaxAge), PacketHandlingError> {
            translate_to_runtime_view(
                transaction_ptr,
                bank,
                transaction_account_lock_limit,
                sanitize_config,
            )
            .map(|(view, deactivation_slot)| {
                (
                    view,
                    MaxAge {
                        sanitized_epoch: bank.epoch(),
                        alt_invalidation_slot: deactivation_slot,
                    },
                )
            })
        }

        /// Returns `true` if a message is valid and can be processed.
        fn validate_message(message: &PackToExecutionWorkerMessage) -> bool {
            message.batch.num_transactions > 0
                && usize::from(message.batch.num_transactions) <= MAX_TRANSACTIONS_PER_MESSAGE
                && Self::validate_message_flags(message.flags)
        }

        fn validate_message_flags(flags: u16) -> bool {
            const ALLOWED_EXECUTE_FLAGS: u16 =
                execution_message_flags::DROP_ON_FAILURE | execution_message_flags::ALL_OR_NOTHING;

            flags & !ALLOWED_EXECUTE_FLAGS == 0
        }

        fn response_from_commit_details(
            tx: &impl TransactionWithMeta,
            commit_details: &CommitTransactionDetails,
            bank: &Bank,
            execution_flags: &ExecutionFlags,
        ) -> ExecutionResponse {
            match commit_details {
                CommitTransactionDetails::Committed {
                    compute_units,
                    loaded_accounts_data_size,
                    fee_payer_post_balance,
                    ..
                } => ExecutionResponse {
                    execution_slot: bank.slot(),
                    not_included_reason: not_included_reasons::NONE,
                    cost_units: CostModel::calculate_cost_for_executed_transaction(
                        tx,
                        *compute_units,
                        *loaded_accounts_data_size,
                        &bank.feature_set,
                    )
                    .sum(),
                    fee_payer_balance: *fee_payer_post_balance,
                },
                CommitTransactionDetails::NotCommitted(transaction_error) => ExecutionResponse {
                    execution_slot: bank.slot(),
                    not_included_reason: transaction_error_to_not_included_reason(
                        transaction_error,
                        execution_flags.all_or_nothing,
                    ),
                    cost_units: 0,
                    fee_payer_balance: 0,
                },
            }
        }

        fn reason_from_packet_handling_error(err: &PacketHandlingError) -> u8 {
            match err {
                PacketHandlingError::ALTResolution => {
                    not_included_reasons::ADDRESS_LOOKUP_TABLE_NOT_FOUND
                }
                _ => not_included_reasons::SANITIZE_FAILURE,
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use {
            super::*,
            crate::banking_stage::{committer::Committer, tests::create_slow_genesis_config},
            agave_scheduler_bindings::{SharableTransactionBatchRegion, processed_codes},
            agave_scheduling_utils::{
                handshake::{ClientLogon, client, server::Server},
                responses_region::ExecutionResponsesPtr,
            },
            crossbeam_channel::bounded,
            solana_genesis_config::GenesisConfig,
            solana_keypair::Keypair,
            solana_leader_schedule::SlotLeader,
            solana_ledger::genesis_utils::GenesisConfigInfo,
            solana_poh::{
                record_channels::{RecordReceiver, record_channels},
                transaction_recorder::TransactionRecorder,
            },
            solana_pubkey::Pubkey,
            solana_runtime::{bank_forks::BankForks, vote_sender_types::ReplayVoteReceiver},
            solana_system_transaction::transfer,
            solana_transaction::TransactionError,
            std::sync::{RwLock, atomic::AtomicBool},
            test_case::test_case,
        };

        struct SharedBatch {
            region: SharableTransactionBatchRegion,
            transactions: Vec<agave_scheduler_bindings::SharableTransactionRegion>,
        }

        struct ExternalTestFrame {
            mint_keypair: Keypair,
            genesis_config: GenesisConfig,
            bank: Arc<Bank>,
            _bank_forks: Arc<RwLock<BankForks>>,
            _replay_vote_receiver: ReplayVoteReceiver,
            record_receiver: RecordReceiver,
            allocator: rts_alloc::Allocator,
            pack_to_worker: shaq::spsc::Producer<PackToExecutionWorkerMessage>,
            worker_to_pack: shaq::spsc::Consumer<ExecutionWorkerToPackMessage>,
            shared_leader_state: SharedLeaderState,
            worker: ExternalWorker,
            receiver: shaq::spsc::Consumer<PackToExecutionWorkerMessage>,
            should_drain_executes: bool,
        }

        impl ExternalTestFrame {
            fn set_active_bank(&mut self) {
                self.shared_leader_state.store(Arc::new(LeaderState::new(
                    Some(self.bank.clone()),
                    self.bank.tick_height(),
                    None,
                    None,
                )));
            }

            fn enable_execution(&mut self) {
                self.set_active_bank();
                self.record_receiver.restart(self.bank.bank_id());
            }

            fn send_message(&mut self, message: PackToExecutionWorkerMessage) {
                self.pack_to_worker.try_write(message).unwrap();
            }

            fn iterate(&mut self) -> Result<(), ExternalConsumeWorkerError> {
                let result = self
                    .worker
                    .iterate(&mut self.receiver, &mut self.should_drain_executes)?;
                assert!(matches!(result, IterationResult::ProcessedMessage));
                Ok(())
            }

            fn recv_response(&mut self) -> ExecutionWorkerToPackMessage {
                self.worker_to_pack.try_read().unwrap()
            }

            fn execution_responses(
                &self,
                region: &ExecutionResponseRegion,
            ) -> Vec<ExecutionResponse> {
                unsafe {
                    // SAFETY: `region` was produced by this worker using the same shared
                    // allocator and contains `ExecutionResponse` values.
                    let responses = ExecutionResponsesPtr::from_transaction_response_region(
                        region,
                        &self.allocator,
                    );
                    let decoded = responses.iter().copied().collect();
                    responses.free(&self.allocator);
                    decoded
                }
            }

            fn allocate_batch(&self, transactions: &[Vec<u8>]) -> SharedBatch {
                type Batch<'a> = TransactionPtrBatch<'a>;
                assert!(transactions.len() <= MAX_TRANSACTIONS_PER_MESSAGE);

                let batch_ptr = self
                    .allocator
                    .allocate(Batch::TRANSACTION_META_END as u32)
                    .unwrap();
                // SAFETY: `batch_ptr` came from this allocator immediately above, so translating
                // it back to an offset in the same allocator is valid.
                let batch_offset = unsafe { self.allocator.offset(batch_ptr) };
                let tx_ptr =
                    batch_ptr.cast::<agave_scheduler_bindings::SharableTransactionRegion>();

                let mut sharable_transactions = Vec::with_capacity(transactions.len());
                for (index, transaction) in transactions.iter().enumerate() {
                    let tx_allocation = self
                        .allocator
                        .allocate(transaction.len().try_into().unwrap())
                        .unwrap();
                    unsafe {
                        // SAFETY: `tx_allocation` points to a fresh allocation of exactly
                        // `transaction.len()` bytes, and `transaction.as_ptr()` is readable for
                        // that same length. The regions do not overlap.
                        std::ptr::copy_nonoverlapping(
                            transaction.as_ptr(),
                            tx_allocation.as_ptr(),
                            transaction.len(),
                        );
                    }
                    let tx_region = agave_scheduler_bindings::SharableTransactionRegion {
                        // SAFETY: `tx_allocation` came from this allocator immediately above, so
                        // translating it back to an offset in the same allocator is valid.
                        offset: unsafe { self.allocator.offset(tx_allocation) },
                        length: transaction.len().try_into().unwrap(),
                    };
                    unsafe {
                        // SAFETY: the batch allocation is sized for
                        // `TransactionPtrBatch::TRANSACTION_META_END`, which includes space for up
                        // to `MAX_TRANSACTIONS_PER_MESSAGE` transaction headers, and the assert
                        // above guarantees `index` is in-bounds.
                        tx_ptr.add(index).write(tx_region)
                    };
                    sharable_transactions.push(tx_region);
                }

                SharedBatch {
                    region: SharableTransactionBatchRegion {
                        num_transactions: transactions.len().try_into().unwrap(),
                        transactions_offset: batch_offset,
                    },
                    transactions: sharable_transactions,
                }
            }

            fn free_batch(&self, batch: SharedBatch) {
                for tx in batch.transactions {
                    unsafe {
                        // SAFETY: each `tx.offset` was allocated by this allocator in
                        // `allocate_batch`, and `SharedBatch` owns each allocation exactly once.
                        self.allocator
                            .free(self.allocator.ptr_from_offset(tx.offset));
                    }
                }
                unsafe {
                    // SAFETY: `transactions_offset` is the batch container allocation created by
                    // `allocate_batch`, and `SharedBatch` owns it exclusively here.
                    self.allocator.free(
                        self.allocator
                            .ptr_from_offset(batch.region.transactions_offset),
                    );
                }
            }
        }

        fn setup_external_test_frame() -> ExternalTestFrame {
            setup_external_test_frame_disable_features(&[])
        }

        fn setup_external_test_frame_disable_features(feature_ids: &[Pubkey]) -> ExternalTestFrame {
            let GenesisConfigInfo {
                mut genesis_config,
                mint_keypair,
                ..
            } = create_slow_genesis_config(10_000);
            for feature_id in feature_ids {
                genesis_config.accounts.remove(feature_id);
            }
            let (root_bank, _root_bank_forks) =
                Bank::new_with_bank_forks_for_tests(&genesis_config);
            let child_bank = Bank::new_from_parent(root_bank, SlotLeader::new_unique(), 1);
            let (bank, bank_forks) = child_bank.wrap_with_bank_forks_for_tests();

            let logon = ClientLogon {
                worker_count: 1,
                check_worker_count: 1,
                allocator_size: 64 * 1024 * 1024,
                allocator_handles: 1,
                tpu_to_pack_capacity: 16,
                progress_tracker_capacity: 16,
                pack_to_worker_capacity: 16,
                worker_to_pack_capacity: 16,
                flags: 0,
                pack_to_check_worker_capacity: 16,
                check_worker_to_pack_capacity: 16,
            };
            let (mut agave_session, files) = Server::setup_session(logon).unwrap();
            let mut client_session = client::setup_session(&logon, files).unwrap();
            let agave_worker = agave_session.workers.pop().unwrap();
            let client_worker = client_session.workers.pop().unwrap();

            let (record_sender, record_receiver) = record_channels(false);
            let recorder = TransactionRecorder::new(record_sender);
            let (replay_vote_sender, replay_vote_receiver) = bounded(1024);
            let committer = Committer::new(None, replay_vote_sender, None);
            let consumer = Consumer::new(committer, recorder, None);
            let shared_leader_state = SharedLeaderState::new(0, None, None);
            let exit = Arc::new(AtomicBool::new(false));

            let worker = ExternalWorker::new(
                0,
                exit.clone(),
                consumer,
                agave_worker.worker_to_pack,
                agave_worker.allocator,
                shared_leader_state.clone(),
            );

            ExternalTestFrame {
                mint_keypair,
                genesis_config,
                bank,
                _bank_forks: bank_forks,
                _replay_vote_receiver: replay_vote_receiver,
                record_receiver,
                allocator: client_session.allocators.pop().unwrap(),
                pack_to_worker: client_worker.pack_to_worker,
                worker_to_pack: client_worker.worker_to_pack,
                shared_leader_state,
                worker,
                receiver: agave_worker.pack_to_worker,
                should_drain_executes: false,
            }
        }

        #[test]
        fn test_validate_message() {
            let mut message = PackToExecutionWorkerMessage {
                flags: 0,
                max_working_slot: u64::MAX,
                batch: agave_scheduler_bindings::SharableTransactionBatchRegion {
                    num_transactions: 0,
                    transactions_offset: 0,
                },
            };

            // No transactions = invalid
            assert!(!ExternalWorker::validate_message(&message));

            // Too many transactions = invalid.
            message.batch.num_transactions = MAX_TRANSACTIONS_PER_MESSAGE as u8 + 1;
            assert!(!ExternalWorker::validate_message(&message));

            // Bad flags = invalid
            message.batch.num_transactions = 1;
            message.flags = u16::MAX;
            assert!(!ExternalWorker::validate_message(&message));

            message.flags = 0;
            assert!(ExternalWorker::validate_message(&message));
        }

        #[test]
        fn test_validate_message_flags() {
            assert!(ExternalWorker::validate_message_flags(0));
            assert!(ExternalWorker::validate_message_flags(
                execution_message_flags::DROP_ON_FAILURE
            ));
            assert!(ExternalWorker::validate_message_flags(
                execution_message_flags::ALL_OR_NOTHING
            ));
            assert!(ExternalWorker::validate_message_flags(
                execution_message_flags::DROP_ON_FAILURE | execution_message_flags::ALL_OR_NOTHING
            ));
            assert!(!ExternalWorker::validate_message_flags(1 << 15));
        }

        #[test]
        fn test_consume_response_iterator() {
            let simple_tx = wincode::serialize(&transfer(
                &solana_keypair::Keypair::new(),
                &solana_pubkey::Pubkey::new_unique(),
                1,
                solana_hash::Hash::default(),
            ))
            .unwrap();
            let bank = Bank::default_for_tests();
            let txs = (0..3)
                .map(|_| {
                    translate_to_runtime_view(
                        &simple_tx[..],
                        &bank,
                        bank.get_transaction_account_lock_limit(),
                        &sanitize_config(),
                    )
                    .ok()
                    .unwrap()
                    .0
                })
                .collect::<Vec<_>>();
            let execution_flags = ExecutionFlags {
                drop_on_failure: false,
                all_or_nothing: false,
            };

            let responses = ExternalWorker::consume_response_iterator(
                &[
                    Err(PacketHandlingError::Sanitization),
                    Ok(()),
                    Ok(()),
                    Ok(()),
                ],
                &txs,
                &[
                    CommitTransactionDetails::Committed {
                        compute_units: 6,
                        loaded_accounts_data_size: 1024,
                        fee_payer_post_balance: 1_000_000,
                        result: Err(TransactionError::InstructionError(
                            0,
                            solana_transaction::InstructionError::Custom(0),
                        )),
                    },
                    CommitTransactionDetails::Committed {
                        compute_units: 10,
                        loaded_accounts_data_size: 2048,
                        fee_payer_post_balance: 2_000_000,
                        result: Ok(()),
                    },
                    CommitTransactionDetails::NotCommitted(
                        TransactionError::InsufficientFundsForFee,
                    ),
                ],
                &bank,
                &execution_flags,
            )
            .collect::<Vec<_>>();

            assert_eq!(
                responses,
                &[
                    ExecutionResponse {
                        execution_slot: bank.slot(),
                        not_included_reason: not_included_reasons::SANITIZE_FAILURE,
                        cost_units: 0,
                        fee_payer_balance: 0
                    },
                    ExecutionResponse {
                        execution_slot: bank.slot(),
                        not_included_reason: not_included_reasons::NONE,
                        cost_units: 1337,
                        fee_payer_balance: 1_000_000,
                    },
                    ExecutionResponse {
                        execution_slot: bank.slot(),
                        not_included_reason: not_included_reasons::NONE,
                        cost_units: 1341,
                        fee_payer_balance: 2_000_000,
                    },
                    ExecutionResponse {
                        execution_slot: bank.slot(),
                        not_included_reason: not_included_reasons::INSUFFICIENT_FUNDS_FOR_FEE,
                        cost_units: 0,
                        fee_payer_balance: 0,
                    }
                ]
            )
        }

        #[test_case(
            true,
            not_included_reasons::ALL_OR_NOTHING_BATCH_FAILURE;
            "all_or_nothing"
        )]
        #[test_case(
            false,
            not_included_reasons::PARTIAL_BATCH_CANCELLED;
            "partial_batch"
        )]
        fn test_commit_cancelled_response_reason_uses_batch_mode(
            all_or_nothing: bool,
            expected_not_included_reason: u8,
        ) {
            let simple_tx = wincode::serialize(&transfer(
                &solana_keypair::Keypair::new(),
                &solana_pubkey::Pubkey::new_unique(),
                1,
                solana_hash::Hash::default(),
            ))
            .unwrap();
            let bank = Bank::default_for_tests();
            let tx = translate_to_runtime_view(
                &simple_tx[..],
                &bank,
                bank.get_transaction_account_lock_limit(),
                &sanitize_config(),
            )
            .ok()
            .unwrap()
            .0;
            let commit_details =
                CommitTransactionDetails::NotCommitted(TransactionError::CommitCancelled);
            let execution_flags = ExecutionFlags {
                drop_on_failure: false,
                all_or_nothing,
            };

            assert_eq!(
                ExternalWorker::response_from_commit_details(
                    &tx,
                    &commit_details,
                    &bank,
                    &execution_flags,
                )
                .not_included_reason,
                expected_not_included_reason
            );
        }

        #[test]
        fn test_all_or_nothing_translate_iterator() {
            let translation_results = vec![Ok(()), Err(PacketHandlingError::Sanitization), Ok(())];
            let test_slot = 42;

            let responses =
                ExternalWorker::all_or_nothing_translate_iterator(&translation_results, test_slot)
                    .collect::<Vec<_>>();

            assert_eq!(
                responses,
                &[
                    ExecutionResponse {
                        execution_slot: test_slot,
                        not_included_reason: not_included_reasons::ALL_OR_NOTHING_BATCH_FAILURE,
                        cost_units: 0,
                        fee_payer_balance: 0
                    },
                    ExecutionResponse {
                        execution_slot: test_slot,
                        not_included_reason: not_included_reasons::SANITIZE_FAILURE,
                        cost_units: 0,
                        fee_payer_balance: 0,
                    },
                    ExecutionResponse {
                        execution_slot: test_slot,
                        not_included_reason: not_included_reasons::ALL_OR_NOTHING_BATCH_FAILURE,
                        cost_units: 0,
                        fee_payer_balance: 0,
                    },
                ]
            )
        }

        fn test_serialized_transaction(recent_blockhash: solana_hash::Hash) -> Vec<u8> {
            let tx = transfer(
                &solana_keypair::Keypair::new(),
                &Pubkey::new_unique(),
                1,
                recent_blockhash,
            );
            wincode::serialize(&tx).unwrap()
        }

        #[test]
        fn test_reason_from_packet_handling_error() {
            assert_eq!(
                ExternalWorker::reason_from_packet_handling_error(
                    &PacketHandlingError::Sanitization
                ),
                not_included_reasons::SANITIZE_FAILURE
            );
            assert_eq!(
                ExternalWorker::reason_from_packet_handling_error(
                    &PacketHandlingError::LockValidation
                ),
                not_included_reasons::SANITIZE_FAILURE
            );
            assert_eq!(
                ExternalWorker::reason_from_packet_handling_error(
                    &PacketHandlingError::ComputeBudget
                ),
                not_included_reasons::SANITIZE_FAILURE
            );

            assert_eq!(
                ExternalWorker::reason_from_packet_handling_error(
                    &PacketHandlingError::ALTResolution
                ),
                not_included_reasons::ADDRESS_LOOKUP_TABLE_NOT_FOUND
            );
        }

        #[test]
        fn test_run_invalid_message() {
            let mut test_frame = setup_external_test_frame();

            test_frame.send_message(PackToExecutionWorkerMessage {
                flags: 0,
                max_working_slot: u64::MAX,
                batch: SharableTransactionBatchRegion {
                    num_transactions: 0,
                    transactions_offset: 0,
                },
            });
            test_frame.iterate().unwrap();
            let response = test_frame.recv_response();
            assert_eq!(response.processed_code, processed_codes::INVALID);
            assert_eq!(response.responses.num_transaction_responses, 0);
            assert_eq!(response.responses.transaction_responses_offset, 0);

            let batch = test_frame.allocate_batch(&[test_serialized_transaction(
                test_frame.bank.confirmed_last_blockhash(),
            )]);
            test_frame.send_message(PackToExecutionWorkerMessage {
                flags: u16::MAX,
                max_working_slot: u64::MAX,
                batch: batch.region,
            });
            test_frame.iterate().unwrap();
            let response = test_frame.recv_response();
            assert_eq!(response.processed_code, processed_codes::INVALID);
            assert_eq!(response.responses.num_transaction_responses, 0);
            assert_eq!(response.responses.transaction_responses_offset, 0);

            test_frame.free_batch(batch);
        }

        #[test]
        fn test_run_execute_without_active_bank() {
            let mut test_frame = setup_external_test_frame();
            let batch = test_frame.allocate_batch(&[wincode::serialize(&transfer(
                &test_frame.mint_keypair,
                &Pubkey::new_unique(),
                1,
                test_frame.genesis_config.hash(),
            ))
            .unwrap()]);

            test_frame.send_message(PackToExecutionWorkerMessage {
                flags: 0,
                max_working_slot: u64::MAX,
                batch: batch.region,
            });
            test_frame.iterate().unwrap();
            let response = test_frame.recv_response();
            assert_eq!(response.processed_code, processed_codes::PROCESSED);
            let responses = test_frame.execution_responses(&response.responses);
            assert_eq!(responses.len(), 1);
            assert_eq!(
                responses[0].not_included_reason,
                not_included_reasons::BANK_NOT_AVAILABLE
            );

            test_frame.free_batch(batch);
        }

        #[test]
        fn test_run_execute_max_working_slot_exceeded() {
            let mut test_frame = setup_external_test_frame();
            test_frame.enable_execution();

            let batch = test_frame.allocate_batch(&[wincode::serialize(&transfer(
                &test_frame.mint_keypair,
                &Pubkey::new_unique(),
                1,
                test_frame.genesis_config.hash(),
            ))
            .unwrap()]);
            test_frame.send_message(PackToExecutionWorkerMessage {
                flags: 0,
                max_working_slot: test_frame.bank.slot() - 1,
                batch: batch.region,
            });
            test_frame.iterate().unwrap();
            let response = test_frame.recv_response();
            assert_eq!(
                response.processed_code,
                processed_codes::MAX_WORKING_SLOT_EXCEEDED
            );
            assert_eq!(response.responses.num_transaction_responses, 0);
            assert_eq!(response.responses.transaction_responses_offset, 0);

            test_frame.free_batch(batch);
        }

        #[test]
        fn test_run_execute_all_or_nothing_translation_failure() {
            let mut test_frame = setup_external_test_frame();
            test_frame.enable_execution();

            let batch = test_frame.allocate_batch(&[
                wincode::serialize(&transfer(
                    &test_frame.mint_keypair,
                    &Pubkey::new_unique(),
                    1,
                    test_frame.genesis_config.hash(),
                ))
                .unwrap(),
                vec![0xff],
            ]);
            test_frame.send_message(PackToExecutionWorkerMessage {
                flags: execution_message_flags::ALL_OR_NOTHING,
                max_working_slot: test_frame.bank.slot(),
                batch: batch.region,
            });
            test_frame.iterate().unwrap();
            let response = test_frame.recv_response();
            assert_eq!(response.processed_code, processed_codes::PROCESSED);
            let responses = test_frame.execution_responses(&response.responses);
            assert_eq!(responses.len(), 2);
            assert_eq!(
                responses[0].not_included_reason,
                not_included_reasons::ALL_OR_NOTHING_BATCH_FAILURE
            );
            assert_eq!(
                responses[1].not_included_reason,
                not_included_reasons::SANITIZE_FAILURE
            );

            test_frame.free_batch(batch);
        }

        #[test]
        fn test_process_message_drains_execute_with_available_bank() {
            let mut test_frame = setup_external_test_frame();
            let batch1 = test_frame.allocate_batch(&[wincode::serialize(&transfer(
                &test_frame.mint_keypair,
                &Pubkey::new_unique(),
                1,
                test_frame.bank.confirmed_last_blockhash(),
            ))
            .unwrap()]);
            let batch2 = test_frame.allocate_batch(&[wincode::serialize(&transfer(
                &test_frame.mint_keypair,
                &Pubkey::new_unique(),
                1,
                test_frame.bank.confirmed_last_blockhash(),
            ))
            .unwrap()]);

            test_frame.send_message(PackToExecutionWorkerMessage {
                flags: 0,
                max_working_slot: test_frame.bank.slot(),
                batch: batch1.region,
            });
            test_frame.send_message(PackToExecutionWorkerMessage {
                flags: 0,
                max_working_slot: test_frame.bank.slot(),
                batch: batch2.region,
            });

            test_frame.iterate().unwrap();
            assert!(test_frame.should_drain_executes);
            let first = test_frame.recv_response();
            let first_responses = test_frame.execution_responses(&first.responses);
            assert_eq!(first_responses.len(), 1);
            assert_eq!(
                first_responses[0].not_included_reason,
                not_included_reasons::BANK_NOT_AVAILABLE
            );

            let response = test_frame.recv_response();
            assert_eq!(response.processed_code, processed_codes::PROCESSED);
            let responses = test_frame.execution_responses(&response.responses);
            assert_eq!(responses.len(), 1);
            assert_eq!(
                responses[0].not_included_reason,
                not_included_reasons::BANK_NOT_AVAILABLE
            );
            assert_eq!(responses[0].execution_slot, 0);

            test_frame.free_batch(batch1);
            test_frame.free_batch(batch2);
        }

        #[test]
        fn test_run_execute_happy_path() {
            let mut test_frame = setup_external_test_frame();
            test_frame.enable_execution();

            let recipient = Pubkey::new_unique();
            let batch = test_frame.allocate_batch(&[wincode::serialize(&transfer(
                &test_frame.mint_keypair,
                &recipient,
                1,
                test_frame.bank.confirmed_last_blockhash(),
            ))
            .unwrap()]);

            test_frame.send_message(PackToExecutionWorkerMessage {
                flags: 0,
                max_working_slot: test_frame.bank.slot(),
                batch: batch.region,
            });
            test_frame.iterate().unwrap();
            let response = test_frame.recv_response();
            assert_eq!(response.processed_code, processed_codes::PROCESSED);
            let responses = test_frame.execution_responses(&response.responses);
            assert_eq!(responses.len(), 1);
            assert_eq!(responses[0].execution_slot, test_frame.bank.slot());
            assert_eq!(responses[0].not_included_reason, not_included_reasons::NONE);
            assert!(responses[0].cost_units > 0);
            assert!(responses[0].fee_payer_balance > 0);
            assert_eq!(test_frame.bank.get_balance(&recipient), 1);

            test_frame.free_batch(batch);
        }

        #[test_case(false; "strict_fee_payer")]
        #[test_case(true; "relaxed_fee_payer")]
        fn test_run_execute_mixed_batch_results(relax_fee_payer_constraint: bool) {
            let feature_ids = if relax_fee_payer_constraint {
                vec![]
            } else {
                vec![agave_feature_set::relax_fee_payer_constraint::id()]
            };
            let mut test_frame = setup_external_test_frame_disable_features(&feature_ids);
            test_frame.enable_execution();

            let unfunded = Keypair::new();
            let batch = test_frame.allocate_batch(&[
                // valid transfer
                wincode::serialize(&transfer(
                    &test_frame.mint_keypair,
                    &Pubkey::new_unique(),
                    1,
                    test_frame.bank.confirmed_last_blockhash(),
                ))
                .unwrap(),
                // unfunded fee-payer: error regardless of `relax_fee_payer_constraint` in block production
                wincode::serialize(&transfer(
                    &unfunded,
                    &Pubkey::new_unique(),
                    1,
                    test_frame.bank.confirmed_last_blockhash(),
                ))
                .unwrap(),
            ]);

            test_frame.send_message(PackToExecutionWorkerMessage {
                flags: 0,
                max_working_slot: test_frame.bank.slot(),
                batch: batch.region,
            });
            test_frame.iterate().unwrap();
            let response = test_frame.recv_response();
            assert_eq!(response.processed_code, processed_codes::PROCESSED);
            let responses = test_frame.execution_responses(&response.responses);
            assert_eq!(responses.len(), 2);
            assert_eq!(responses[0].not_included_reason, not_included_reasons::NONE);
            assert_eq!(
                responses[1].not_included_reason,
                not_included_reasons::ACCOUNT_NOT_FOUND
            );

            test_frame.free_batch(batch);
        }

        #[test]
        fn test_run_multiple_messages_in_order() {
            let mut test_frame = setup_external_test_frame();
            test_frame.enable_execution();

            let first_batch = test_frame.allocate_batch(&[wincode::serialize(&transfer(
                &test_frame.mint_keypair,
                &Pubkey::new_unique(),
                1,
                test_frame.bank.confirmed_last_blockhash(),
            ))
            .unwrap()]);
            let second_batch = test_frame.allocate_batch(&[wincode::serialize(&transfer(
                &test_frame.mint_keypair,
                &Pubkey::new_unique(),
                1,
                test_frame.bank.confirmed_last_blockhash(),
            ))
            .unwrap()]);

            test_frame.send_message(PackToExecutionWorkerMessage {
                flags: 0,
                max_working_slot: test_frame.bank.slot(),
                batch: first_batch.region,
            });
            test_frame.send_message(PackToExecutionWorkerMessage {
                flags: 0,
                max_working_slot: test_frame.bank.slot(),
                batch: second_batch.region,
            });

            test_frame.iterate().unwrap();
            let first = test_frame.recv_response();
            assert_eq!(first.batch, first_batch.region);
            let first_responses = test_frame.execution_responses(&first.responses);
            assert_eq!(first_responses.len(), 1);
            assert_eq!(
                first_responses[0].not_included_reason,
                not_included_reasons::NONE
            );

            let second = test_frame.recv_response();
            assert_eq!(second.batch, second_batch.region);
            let second_responses = test_frame.execution_responses(&second.responses);
            assert_eq!(second_responses.len(), 1);
            assert_eq!(
                second_responses[0].not_included_reason,
                not_included_reasons::NONE
            );

            test_frame.free_batch(first_batch);
            test_frame.free_batch(second_batch);
        }

        #[test]
        fn test_run_execute_stale_blockhash() {
            let mut test_frame = setup_external_test_frame();
            test_frame.enable_execution();

            let batch = test_frame.allocate_batch(&[wincode::serialize(&transfer(
                &test_frame.mint_keypair,
                &Pubkey::new_unique(),
                1,
                solana_hash::Hash::new_unique(),
            ))
            .unwrap()]);

            test_frame.send_message(PackToExecutionWorkerMessage {
                flags: 0,
                max_working_slot: test_frame.bank.slot(),
                batch: batch.region,
            });
            test_frame.iterate().unwrap();
            let response = test_frame.recv_response();
            assert_eq!(response.processed_code, processed_codes::PROCESSED);
            let responses = test_frame.execution_responses(&response.responses);
            assert_eq!(responses.len(), 1);
            assert_eq!(
                responses[0].not_included_reason,
                not_included_reasons::BLOCKHASH_NOT_FOUND
            );

            test_frame.free_batch(batch);
        }
    }
}
/// Helper function to create an non-blocking iterator over work in the receiver,
/// starting with the given work item.
fn try_drain_iter<T>(work: T, receiver: &Receiver<T>) -> impl Iterator<Item = T> + '_ {
    std::iter::once(work).chain(receiver.try_iter())
}

/// Returns an active leader state if available, otherwise None.
fn active_leader_state(
    shared_leader_state: &SharedLeaderState,
) -> Option<arc_swap::Guard<Arc<LeaderState>>> {
    let guard = shared_leader_state.load();
    if guard
        .as_ref()
        .working_bank()
        .map(|bank| bank.is_complete())
        .unwrap_or(true)
    {
        None
    } else {
        Some(guard)
    }
}

const STARTING_SLEEP_DURATION: Duration = Duration::from_micros(250);
const MAX_SLEEP_DURATION: Duration = Duration::from_millis(1);
const IDLE_SLEEP_THRESHOLD: Duration = Duration::from_millis(1);

/// Sleeps for the specified time. Returns the next sleep duration to use.
fn backoff(idle_duration: Duration, sleep_duration: &Duration) -> Duration {
    if idle_duration < IDLE_SLEEP_THRESHOLD {
        core::hint::spin_loop();
        *sleep_duration
    } else {
        std::thread::sleep(*sleep_duration);
        sleep_duration.saturating_mul(2).min(MAX_SLEEP_DURATION)
    }
}

/// Metrics tracking number of packets processed by the consume worker.
/// These are atomic, and intended to be reported by the scheduling thread
/// since the consume worker thread is sleeping unless there is work to be
/// done.
pub(crate) struct ConsumeWorkerMetrics {
    id: String,
    interval: AtomicInterval,
    has_data: AtomicBool,

    count_metrics: ConsumeWorkerCountMetrics,
    error_metrics: ConsumeWorkerTransactionErrorMetrics,
    timing_metrics: ConsumeWorkerTimingMetrics,
}

impl ConsumeWorkerMetrics {
    /// Report and reset metrics iff the interval has elapsed and the worker did some work.
    pub fn maybe_report_and_reset(&self) {
        const REPORT_INTERVAL_MS: u64 = 20;
        if self.interval.should_update(REPORT_INTERVAL_MS)
            && self.has_data.swap(false, Ordering::Relaxed)
        {
            self.count_metrics.report_and_reset(&self.id);
            self.timing_metrics.report_and_reset(&self.id);
            self.error_metrics.report_and_reset(&self.id);
        }
    }

    fn new(id: u32) -> Self {
        Self {
            id: id.to_string(),
            interval: AtomicInterval::default(),
            has_data: AtomicBool::new(false),
            count_metrics: ConsumeWorkerCountMetrics::default(),
            error_metrics: ConsumeWorkerTransactionErrorMetrics::default(),
            timing_metrics: ConsumeWorkerTimingMetrics::default(),
        }
    }

    fn update_for_consume(
        &self,
        ProcessTransactionBatchOutput {
            cost_model_throttled_transactions_count,
            cost_model_us,
            execute_and_commit_transactions_output,
        }: &ProcessTransactionBatchOutput,
    ) {
        self.count_metrics
            .cost_model_throttled_transactions_count
            .fetch_add(*cost_model_throttled_transactions_count, Ordering::Relaxed);
        self.timing_metrics
            .cost_model_us
            .fetch_add(*cost_model_us, Ordering::Relaxed);
        self.update_on_execute_and_commit_transactions_output(
            execute_and_commit_transactions_output,
        );
    }

    fn update_on_execute_and_commit_transactions_output(
        &self,
        ExecuteAndCommitTransactionsOutput {
            transaction_counts,
            retryable_transaction_indexes,
            execute_and_commit_timings,
            error_counters,
            ..
        }: &ExecuteAndCommitTransactionsOutput,
    ) {
        self.count_metrics
            .transactions_attempted_processing_count
            .fetch_add(
                transaction_counts.attempted_processing_count,
                Ordering::Relaxed,
            );
        self.count_metrics
            .processed_transactions_count
            .fetch_add(transaction_counts.processed_count, Ordering::Relaxed);
        self.count_metrics
            .processed_with_successful_result_count
            .fetch_add(
                transaction_counts.processed_with_successful_result_count,
                Ordering::Relaxed,
            );
        self.count_metrics
            .retryable_transaction_count
            .fetch_add(retryable_transaction_indexes.len(), Ordering::Relaxed);
        self.update_on_execute_and_commit_timings(execute_and_commit_timings);
        self.update_on_error_counters(error_counters);
    }

    fn update_on_execute_and_commit_timings(
        &self,
        LeaderExecuteAndCommitTimings {
            load_execute_us,
            freeze_lock_us,
            record_us,
            commit_us,
            find_and_send_votes_us,
            ..
        }: &LeaderExecuteAndCommitTimings,
    ) {
        self.timing_metrics
            .load_execute_us_min
            .fetch_min(*load_execute_us, Ordering::Relaxed);
        self.timing_metrics
            .load_execute_us_max
            .fetch_max(*load_execute_us, Ordering::Relaxed);
        self.timing_metrics
            .load_execute_us
            .fetch_add(*load_execute_us, Ordering::Relaxed);
        self.timing_metrics
            .freeze_lock_us
            .fetch_add(*freeze_lock_us, Ordering::Relaxed);
        self.timing_metrics
            .record_us
            .fetch_add(*record_us, Ordering::Relaxed);
        self.timing_metrics
            .commit_us
            .fetch_add(*commit_us, Ordering::Relaxed);
        self.timing_metrics
            .find_and_send_votes_us
            .fetch_add(*find_and_send_votes_us, Ordering::Relaxed);
        self.timing_metrics
            .num_batches_processed
            .fetch_add(1, Ordering::Relaxed);
    }

    fn update_on_error_counters(
        &self,
        TransactionErrorMetrics {
            total,
            account_in_use,
            too_many_account_locks,
            account_loaded_twice,
            account_not_found,
            blockhash_not_found,
            blockhash_too_old,
            call_chain_too_deep,
            already_processed,
            instruction_error,
            insufficient_funds,
            invalid_account_for_fee,
            invalid_account_index,
            invalid_program_for_execution,
            invalid_compute_budget,
            not_allowed_during_cluster_maintenance,
            invalid_writable_account,
            invalid_rent_paying_account,
            would_exceed_max_block_cost_limit,
            would_exceed_max_account_cost_limit,
            would_exceed_max_vote_cost_limit,
            would_exceed_account_data_block_limit,
            max_loaded_accounts_data_size_exceeded,
            program_execution_temporarily_restricted,
        }: &TransactionErrorMetrics,
    ) {
        self.error_metrics
            .total
            .fetch_add(total.0, Ordering::Relaxed);
        self.error_metrics
            .account_in_use
            .fetch_add(account_in_use.0, Ordering::Relaxed);
        self.error_metrics
            .too_many_account_locks
            .fetch_add(too_many_account_locks.0, Ordering::Relaxed);
        self.error_metrics
            .account_loaded_twice
            .fetch_add(account_loaded_twice.0, Ordering::Relaxed);
        self.error_metrics
            .account_not_found
            .fetch_add(account_not_found.0, Ordering::Relaxed);
        self.error_metrics
            .blockhash_not_found
            .fetch_add(blockhash_not_found.0, Ordering::Relaxed);
        self.error_metrics
            .blockhash_too_old
            .fetch_add(blockhash_too_old.0, Ordering::Relaxed);
        self.error_metrics
            .call_chain_too_deep
            .fetch_add(call_chain_too_deep.0, Ordering::Relaxed);
        self.error_metrics
            .already_processed
            .fetch_add(already_processed.0, Ordering::Relaxed);
        self.error_metrics
            .instruction_error
            .fetch_add(instruction_error.0, Ordering::Relaxed);
        self.error_metrics
            .insufficient_funds
            .fetch_add(insufficient_funds.0, Ordering::Relaxed);
        self.error_metrics
            .invalid_account_for_fee
            .fetch_add(invalid_account_for_fee.0, Ordering::Relaxed);
        self.error_metrics
            .invalid_account_index
            .fetch_add(invalid_account_index.0, Ordering::Relaxed);
        self.error_metrics
            .invalid_program_for_execution
            .fetch_add(invalid_program_for_execution.0, Ordering::Relaxed);
        self.error_metrics
            .invalid_compute_budget
            .fetch_add(invalid_compute_budget.0, Ordering::Relaxed);
        self.error_metrics
            .not_allowed_during_cluster_maintenance
            .fetch_add(not_allowed_during_cluster_maintenance.0, Ordering::Relaxed);
        self.error_metrics
            .invalid_writable_account
            .fetch_add(invalid_writable_account.0, Ordering::Relaxed);
        self.error_metrics
            .invalid_rent_paying_account
            .fetch_add(invalid_rent_paying_account.0, Ordering::Relaxed);
        self.error_metrics
            .would_exceed_max_block_cost_limit
            .fetch_add(would_exceed_max_block_cost_limit.0, Ordering::Relaxed);
        self.error_metrics
            .would_exceed_max_account_cost_limit
            .fetch_add(would_exceed_max_account_cost_limit.0, Ordering::Relaxed);
        self.error_metrics
            .would_exceed_max_vote_cost_limit
            .fetch_add(would_exceed_max_vote_cost_limit.0, Ordering::Relaxed);
        self.error_metrics
            .would_exceed_account_data_block_limit
            .fetch_add(would_exceed_account_data_block_limit.0, Ordering::Relaxed);
        self.error_metrics
            .max_loaded_accounts_data_size_exceeded
            .fetch_add(max_loaded_accounts_data_size_exceeded.0, Ordering::Relaxed);
        self.error_metrics
            .program_execution_temporarily_restricted
            .fetch_add(
                program_execution_temporarily_restricted.0,
                Ordering::Relaxed,
            );
    }
}

#[derive(Default)]
struct ConsumeWorkerCountMetrics {
    max_queue_len: AtomicU64,
    num_messages_processed: AtomicU64,
    transactions_attempted_processing_count: AtomicU64,
    processed_transactions_count: AtomicU64,
    processed_with_successful_result_count: AtomicU64,
    retryable_transaction_count: AtomicUsize,
    retryable_expired_bank_count: AtomicUsize,
    cost_model_throttled_transactions_count: AtomicU64,
}

impl ConsumeWorkerCountMetrics {
    fn report_and_reset(&self, id: &str) {
        let datapoint = create_datapoint!(
            @point "banking_stage_worker_counts",
            "id" => id,
            ("max_queue_len", self.max_queue_len.swap(0, Ordering::Relaxed), i64),
            (
                "num_messages_processed",
                self.num_messages_processed.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "transactions_attempted_processing_count",
                self.transactions_attempted_processing_count
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "processed_transactions_count",
                self.processed_transactions_count.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "processed_with_successful_result_count",
                self.processed_with_successful_result_count
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "retryable_transaction_count",
                self.retryable_transaction_count.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "retryable_expired_bank_count",
                self.retryable_expired_bank_count.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "cost_model_throttled_transactions_count",
                self.cost_model_throttled_transactions_count
                    .swap(0, Ordering::Relaxed),
                i64
            ),
        );
        solana_metrics::submit(datapoint, log::Level::Trace);
    }
}

#[derive(Default)]
struct ConsumeWorkerTimingMetrics {
    cost_model_us: AtomicU64,
    load_execute_us: AtomicU64,
    load_execute_us_min: AtomicU64,
    load_execute_us_max: AtomicU64,
    freeze_lock_us: AtomicU64,
    record_us: AtomicU64,
    commit_us: AtomicU64,
    find_and_send_votes_us: AtomicU64,
    num_batches_processed: AtomicU64,
}

impl ConsumeWorkerTimingMetrics {
    fn report_and_reset(&self, id: &str) {
        let datapoint = create_datapoint!(
            @point "banking_stage_worker_timing",
            "id" => id,
            (
                "cost_model_us",
                self.cost_model_us.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "load_execute_us",
                self.load_execute_us.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "load_execute_us_min",
                self.load_execute_us_min.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "load_execute_us_max",
                self.load_execute_us_max.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "num_batches_processed",
                self.num_batches_processed.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "freeze_lock_us",
                self.freeze_lock_us.swap(0, Ordering::Relaxed),
                i64
            ),
            ("record_us", self.record_us.swap(0, Ordering::Relaxed), i64),
            ("commit_us", self.commit_us.swap(0, Ordering::Relaxed), i64),
            (
                "find_and_send_votes_us",
                self.find_and_send_votes_us.swap(0, Ordering::Relaxed),
                i64
            ),
        );
        solana_metrics::submit(datapoint, log::Level::Trace);
    }
}

#[derive(Default)]
struct ConsumeWorkerTransactionErrorMetrics {
    total: AtomicUsize,
    account_in_use: AtomicUsize,
    too_many_account_locks: AtomicUsize,
    account_loaded_twice: AtomicUsize,
    account_not_found: AtomicUsize,
    blockhash_not_found: AtomicUsize,
    blockhash_too_old: AtomicUsize,
    call_chain_too_deep: AtomicUsize,
    already_processed: AtomicUsize,
    instruction_error: AtomicUsize,
    insufficient_funds: AtomicUsize,
    invalid_account_for_fee: AtomicUsize,
    invalid_account_index: AtomicUsize,
    invalid_program_for_execution: AtomicUsize,
    invalid_compute_budget: AtomicUsize,
    not_allowed_during_cluster_maintenance: AtomicUsize,
    invalid_writable_account: AtomicUsize,
    invalid_rent_paying_account: AtomicUsize,
    would_exceed_max_block_cost_limit: AtomicUsize,
    would_exceed_max_account_cost_limit: AtomicUsize,
    would_exceed_max_vote_cost_limit: AtomicUsize,
    would_exceed_account_data_block_limit: AtomicUsize,
    max_loaded_accounts_data_size_exceeded: AtomicUsize,
    program_execution_temporarily_restricted: AtomicUsize,
}

impl ConsumeWorkerTransactionErrorMetrics {
    fn report_and_reset(&self, id: &str) {
        let datapoint = create_datapoint!(
            @point "banking_stage_worker_error_metrics",
            "id" => id,
            ("total", self.total.swap(0, Ordering::Relaxed), i64),
            (
                "account_in_use",
                self.account_in_use.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "too_many_account_locks",
                self.too_many_account_locks.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "account_loaded_twice",
                self.account_loaded_twice.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "account_not_found",
                self.account_not_found.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "blockhash_not_found",
                self.blockhash_not_found.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "blockhash_too_old",
                self.blockhash_too_old.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "call_chain_too_deep",
                self.call_chain_too_deep.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "already_processed",
                self.already_processed.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "instruction_error",
                self.instruction_error.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "insufficient_funds",
                self.insufficient_funds.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "invalid_account_for_fee",
                self.invalid_account_for_fee.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "invalid_account_index",
                self.invalid_account_index.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "invalid_program_for_execution",
                self.invalid_program_for_execution
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "invalid_compute_budget",
                self.invalid_compute_budget
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "not_allowed_during_cluster_maintenance",
                self.not_allowed_during_cluster_maintenance
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "invalid_writable_account",
                self.invalid_writable_account.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "invalid_rent_paying_account",
                self.invalid_rent_paying_account.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "would_exceed_max_block_cost_limit",
                self.would_exceed_max_block_cost_limit
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "would_exceed_max_account_cost_limit",
                self.would_exceed_max_account_cost_limit
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "would_exceed_max_vote_cost_limit",
                self.would_exceed_max_vote_cost_limit
                    .swap(0, Ordering::Relaxed),
                i64
            ),
        );
        solana_metrics::submit(datapoint, log::Level::Trace);
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::banking_stage::{
            committer::Committer,
            scheduler_messages::{MaxAge, TransactionBatchId},
            tests::{create_slow_genesis_config, sanitize_transactions},
        },
        crossbeam_channel::bounded,
        solana_clock::Slot,
        solana_genesis_config::GenesisConfig,
        solana_keypair::Keypair,
        solana_leader_schedule::SlotLeader,
        solana_ledger::genesis_utils::GenesisConfigInfo,
        solana_message::{
            AddressLookupTableAccount, SimpleAddressLoader, VersionedMessage,
            v0::{self, LoadedAddresses},
        },
        solana_poh::{
            record_channels::{RecordReceiver, record_channels},
            transaction_recorder::TransactionRecorder,
        },
        solana_pubkey::Pubkey,
        solana_runtime::{
            bank::Bank, bank_forks::BankForks, vote_sender_types::ReplayVoteReceiver,
        },
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_signer::Signer,
        solana_svm_transaction::svm_message::SVMMessage,
        solana_system_interface::instruction as system_instruction,
        solana_system_transaction as system_transaction,
        solana_transaction::{
            sanitized::{MessageHash, SanitizedTransaction},
            versioned::VersionedTransaction,
        },
        solana_transaction_error::TransactionError,
        std::{
            collections::HashSet,
            sync::{RwLock, atomic::AtomicBool},
        },
    };

    // Helper struct to create tests that hold channels, files, etc.
    // such that our tests can be more easily set up and run.
    struct TestFrame {
        mint_keypair: Keypair,
        genesis_config: GenesisConfig,
        bank: Arc<Bank>,
        _bank_forks: Arc<RwLock<BankForks>>,
        _replay_vote_receiver: ReplayVoteReceiver,
        record_receiver: RecordReceiver,
        shared_leader_state: SharedLeaderState,

        consume_sender: Sender<ConsumeWork<RuntimeTransaction<SanitizedTransaction>>>,
        consumed_receiver: Receiver<FinishedConsumeWork<RuntimeTransaction<SanitizedTransaction>>>,
    }

    fn setup_test_frame() -> (
        TestFrame,
        ConsumeWorker<RuntimeTransaction<SanitizedTransaction>>,
    ) {
        let GenesisConfigInfo {
            mut genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(10_000);
        genesis_config
            .accounts
            .remove(&agave_feature_set::remove_runtime_float_ops::id());
        let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        // Warp to next epoch for MaxAge tests.
        let bank = Bank::new_from_parent(
            bank.clone(),
            SlotLeader::new_unique(),
            bank.get_epoch_info().slots_in_epoch,
        );
        let bank = Arc::new(bank);

        let (record_sender, record_receiver) = record_channels(false);
        let recorder = TransactionRecorder::new(record_sender);

        let (replay_vote_sender, replay_vote_receiver) = bounded(1024);
        let committer = Committer::new(None, replay_vote_sender, None);
        let consumer = Consumer::new(committer, recorder, None);
        let shared_leader_state = SharedLeaderState::new(0, None, None);

        let (consume_sender, consume_receiver) = bounded(1024);
        let (consumed_sender, consumed_receiver) = bounded(1024);
        let worker = ConsumeWorker::new(
            0,
            Arc::new(AtomicBool::new(false)),
            consume_receiver,
            consumer,
            consumed_sender,
            shared_leader_state.clone(),
        );

        (
            TestFrame {
                mint_keypair,
                genesis_config,
                bank,
                _bank_forks: bank_forks,
                _replay_vote_receiver: replay_vote_receiver,
                record_receiver,
                shared_leader_state,
                consume_sender,
                consumed_receiver,
            },
            worker,
        )
    }

    #[test]
    fn test_worker_consume_no_bank() {
        let (test_frame, worker) = setup_test_frame();
        let TestFrame {
            mint_keypair,
            genesis_config,
            bank,
            consume_sender,
            consumed_receiver,
            ..
        } = &test_frame;
        let worker_thread = std::thread::spawn(move || worker.run());

        let pubkey1 = Pubkey::new_unique();

        let transactions = sanitize_transactions(vec![system_transaction::transfer(
            mint_keypair,
            &pubkey1,
            1,
            genesis_config.hash(),
        )]);
        let bid = TransactionBatchId::new(0);
        let id = 0;
        let max_age = MaxAge {
            sanitized_epoch: bank.epoch(),
            alt_invalidation_slot: bank.slot(),
        };
        let work = ConsumeWork {
            target_slot: bank.slot(),
            batch_id: bid,
            ids: vec![id],
            transactions,
            max_ages: vec![max_age],
        };
        consume_sender.send(work).unwrap();
        let consumed = consumed_receiver.recv().unwrap();
        assert_eq!(consumed.work.batch_id, bid);
        assert_eq!(consumed.work.ids, vec![id]);
        assert_eq!(consumed.work.max_ages, vec![max_age]);
        assert_eq!(
            consumed.retryable_indexes,
            vec![RetryableIndex::new(0, true)]
        );

        drop(test_frame);
        let _ = worker_thread.join().unwrap();
    }

    #[test]
    fn test_worker_consume_no_bank_drains_queue() {
        let (test_frame, worker) = setup_test_frame();
        let TestFrame {
            mint_keypair,
            genesis_config,
            bank,
            consume_sender,
            consumed_receiver,
            ..
        } = &test_frame;

        // Queue up 5 batches.
        let num_batches: usize = 5;
        for i in 0..num_batches {
            let transactions = sanitize_transactions(vec![system_transaction::transfer(
                mint_keypair,
                &Pubkey::new_unique(),
                1,
                genesis_config.hash(),
            )]);
            consume_sender
                .send(ConsumeWork {
                    target_slot: bank.slot(),
                    batch_id: TransactionBatchId::new(i as u64),
                    ids: vec![i],
                    transactions,
                    max_ages: vec![MaxAge {
                        sanitized_epoch: bank.epoch(),
                        alt_invalidation_slot: bank.slot(),
                    }],
                })
                .unwrap();
        }

        // Start the worker with 5 pending batches.
        let worker_thread = std::thread::spawn(move || worker.run());

        // All batches should be returned as retryable (no bank available).
        for i in 0..num_batches {
            let consumed = consumed_receiver.recv().unwrap();
            assert_eq!(consumed.work.batch_id, TransactionBatchId::new(i as u64));
            assert_eq!(
                consumed.retryable_indexes,
                vec![RetryableIndex::new(0, true)]
            );
        }

        // Cleanup.
        drop(test_frame);
        let _ = worker_thread.join().unwrap();
    }

    #[test]
    fn test_worker_consume_wrong_slot() {
        let (mut test_frame, worker) = setup_test_frame();
        let metrics = worker.metrics_handle();
        let TestFrame {
            mint_keypair,
            genesis_config,
            bank,
            shared_leader_state,
            consume_sender,
            consumed_receiver,
            ..
        } = &mut test_frame;
        shared_leader_state.store(Arc::new(LeaderState::new(
            Some(bank.clone()),
            bank.tick_height(),
            None,
            None,
        )));
        let worker_thread = std::thread::spawn(move || worker.run());

        let transactions = sanitize_transactions(vec![system_transaction::transfer(
            mint_keypair,
            &Pubkey::new_unique(),
            1,
            genesis_config.hash(),
        )]);
        consume_sender
            .send(ConsumeWork {
                target_slot: bank.slot() + 1,
                batch_id: TransactionBatchId::new(0),
                ids: vec![0],
                transactions,
                max_ages: vec![MaxAge {
                    sanitized_epoch: bank.epoch(),
                    alt_invalidation_slot: bank.slot(),
                }],
            })
            .unwrap();

        let consumed = consumed_receiver.recv().unwrap();
        assert_eq!(consumed.work.target_slot, bank.slot() + 1);
        assert_eq!(
            consumed.retryable_indexes,
            vec![RetryableIndex::new(0, true)]
        );
        assert_eq!(
            metrics
                .count_metrics
                .num_messages_processed
                .load(Ordering::Relaxed),
            0
        );

        drop(test_frame);
        let _ = worker_thread.join().unwrap();
    }

    #[test]
    fn test_worker_consume_simple() {
        let (mut test_frame, worker) = setup_test_frame();
        let TestFrame {
            mint_keypair,
            genesis_config,
            bank,
            record_receiver,
            shared_leader_state,
            consume_sender,
            consumed_receiver,
            ..
        } = &mut test_frame;
        let worker_thread = std::thread::spawn(move || worker.run());
        shared_leader_state.store(Arc::new(LeaderState::new(
            Some(bank.clone()),
            bank.tick_height(),
            None,
            None,
        )));
        record_receiver.restart(bank.bank_id());

        let pubkey1 = Pubkey::new_unique();

        let transactions = sanitize_transactions(vec![system_transaction::transfer(
            mint_keypair,
            &pubkey1,
            1,
            genesis_config.hash(),
        )]);
        let bid = TransactionBatchId::new(0);
        let id = 0;
        let max_age = MaxAge {
            sanitized_epoch: bank.epoch(),
            alt_invalidation_slot: bank.slot(),
        };
        let work = ConsumeWork {
            target_slot: bank.slot(),
            batch_id: bid,
            ids: vec![id],
            transactions,
            max_ages: vec![max_age],
        };
        consume_sender.send(work).unwrap();
        let consumed = consumed_receiver.recv().unwrap();
        assert_eq!(consumed.work.batch_id, bid);
        assert_eq!(consumed.work.ids, vec![id]);
        assert_eq!(consumed.work.max_ages, vec![max_age]);
        assert_eq!(consumed.retryable_indexes, Vec::new());

        drop(test_frame);
        let _ = worker_thread.join().unwrap();
    }

    #[test]
    fn test_worker_consume_self_conflicting() {
        let (mut test_frame, worker) = setup_test_frame();
        let TestFrame {
            mint_keypair,
            genesis_config,
            bank,
            record_receiver,
            shared_leader_state,
            consume_sender,
            consumed_receiver,
            ..
        } = &mut test_frame;
        let worker_thread = std::thread::spawn(move || worker.run());
        shared_leader_state.store(Arc::new(LeaderState::new(
            Some(bank.clone()),
            bank.tick_height(),
            None,
            None,
        )));
        record_receiver.restart(bank.bank_id());

        let pubkey1 = Pubkey::new_unique();
        let pubkey2 = Pubkey::new_unique();

        let txs = sanitize_transactions(vec![
            system_transaction::transfer(mint_keypair, &pubkey1, 2, genesis_config.hash()),
            system_transaction::transfer(mint_keypair, &pubkey2, 2, genesis_config.hash()),
        ]);

        let bid = TransactionBatchId::new(0);
        let id1 = 1;
        let id2 = 0;
        let max_age = MaxAge {
            sanitized_epoch: bank.epoch(),
            alt_invalidation_slot: bank.slot(),
        };
        consume_sender
            .send(ConsumeWork {
                target_slot: bank.slot(),
                batch_id: bid,
                ids: vec![id1, id2],
                transactions: txs,
                max_ages: vec![max_age, max_age],
            })
            .unwrap();

        let consumed = consumed_receiver.recv().unwrap();
        assert_eq!(consumed.work.batch_id, bid);
        assert_eq!(consumed.work.ids, vec![id1, id2]);
        assert_eq!(consumed.work.max_ages, vec![max_age, max_age]);

        assert_eq!(consumed.retryable_indexes, vec![]);

        drop(test_frame);
        let _ = worker_thread.join().unwrap();
    }

    #[test]
    fn test_worker_consume_multiple_messages() {
        let (mut test_frame, worker) = setup_test_frame();
        let TestFrame {
            mint_keypair,
            genesis_config,
            bank,
            record_receiver,
            shared_leader_state,
            consume_sender,
            consumed_receiver,
            ..
        } = &mut test_frame;
        let worker_thread = std::thread::spawn(move || worker.run());
        shared_leader_state.store(Arc::new(LeaderState::new(
            Some(bank.clone()),
            bank.tick_height(),
            None,
            None,
        )));
        record_receiver.restart(bank.bank_id());

        let pubkey1 = Pubkey::new_unique();
        let pubkey2 = Pubkey::new_unique();

        let txs1 = sanitize_transactions(vec![system_transaction::transfer(
            mint_keypair,
            &pubkey1,
            2,
            genesis_config.hash(),
        )]);
        let txs2 = sanitize_transactions(vec![system_transaction::transfer(
            mint_keypair,
            &pubkey2,
            2,
            genesis_config.hash(),
        )]);

        let bid1 = TransactionBatchId::new(0);
        let bid2 = TransactionBatchId::new(1);
        let id1 = 1;
        let id2 = 0;
        let max_age = MaxAge {
            sanitized_epoch: bank.epoch(),
            alt_invalidation_slot: bank.slot(),
        };
        consume_sender
            .send(ConsumeWork {
                target_slot: bank.slot(),
                batch_id: bid1,
                ids: vec![id1],
                transactions: txs1,
                max_ages: vec![max_age],
            })
            .unwrap();

        consume_sender
            .send(ConsumeWork {
                target_slot: bank.slot(),
                batch_id: bid2,
                ids: vec![id2],
                transactions: txs2,
                max_ages: vec![max_age],
            })
            .unwrap();
        let consumed = consumed_receiver.recv().unwrap();
        assert_eq!(consumed.work.batch_id, bid1);
        assert_eq!(consumed.work.ids, vec![id1]);
        assert_eq!(consumed.work.max_ages, vec![max_age]);
        assert_eq!(consumed.retryable_indexes, Vec::new());

        let consumed = consumed_receiver.recv().unwrap();
        assert_eq!(consumed.work.batch_id, bid2);
        assert_eq!(consumed.work.ids, vec![id2]);
        assert_eq!(consumed.work.max_ages, vec![max_age]);
        assert_eq!(consumed.retryable_indexes, Vec::new());

        drop(test_frame);
        let _ = worker_thread.join().unwrap();
    }

    #[test]
    fn test_worker_ttl() {
        let (mut test_frame, worker) = setup_test_frame();
        let TestFrame {
            mint_keypair,
            genesis_config,
            bank,
            record_receiver,
            shared_leader_state,
            consume_sender,
            consumed_receiver,
            ..
        } = &mut test_frame;
        let worker_thread = std::thread::spawn(move || worker.run());
        shared_leader_state.store(Arc::new(LeaderState::new(
            Some(bank.clone()),
            bank.tick_height(),
            None,
            None,
        )));
        record_receiver.restart(bank.bank_id());
        assert!(bank.slot() > 0);
        assert!(bank.epoch() > 0);

        // No conflicts between transactions. Test 6 cases.
        // 1. Epoch expiration, before slot => still succeeds due to resanitizing
        // 2. Epoch expiration, on slot => succeeds normally
        // 3. Epoch expiration, after slot => succeeds normally
        // 4. ALT expiration, before slot => fails
        // 5. ALT expiration, on slot => succeeds normally
        // 6. ALT expiration, after slot => succeeds normally
        let simple_transfer = || {
            system_transaction::transfer(
                &Keypair::new(),
                &Pubkey::new_unique(),
                1,
                genesis_config.hash(),
            )
        };
        let simple_v0_transfer = || {
            let payer = Keypair::new();
            let to_pubkey = Pubkey::new_unique();
            let loaded_addresses = LoadedAddresses {
                writable: vec![to_pubkey],
                readonly: vec![],
            };
            let loader = SimpleAddressLoader::Enabled(loaded_addresses);
            RuntimeTransaction::try_create(
                VersionedTransaction::try_new(
                    VersionedMessage::V0(
                        v0::Message::try_compile(
                            &payer.pubkey(),
                            &[system_instruction::transfer(&payer.pubkey(), &to_pubkey, 1)],
                            &[AddressLookupTableAccount {
                                key: Pubkey::new_unique(), // will fail if using **bank** to lookup
                                addresses: vec![to_pubkey],
                            }],
                            genesis_config.hash(),
                        )
                        .unwrap(),
                    ),
                    &[&payer],
                )
                .unwrap(),
                MessageHash::Compute,
                None,
                loader,
                &HashSet::default(),
            )
            .unwrap()
        };

        let mut txs = sanitize_transactions(vec![
            simple_transfer(),
            simple_transfer(),
            simple_transfer(),
        ]);
        txs.push(simple_v0_transfer());
        txs.push(simple_v0_transfer());
        txs.push(simple_v0_transfer());
        let sanitized_txs = txs.clone();

        // Fund the keypairs.
        for tx in &txs {
            bank.process_transaction(&system_transaction::transfer(
                mint_keypair,
                &tx.account_keys()[0],
                2,
                genesis_config.hash(),
            ))
            .unwrap();
        }

        consume_sender
            .send(ConsumeWork {
                target_slot: bank.slot(),
                batch_id: TransactionBatchId::new(1),
                ids: vec![0, 1, 2, 3, 4, 5],
                transactions: txs,
                max_ages: vec![
                    MaxAge {
                        sanitized_epoch: bank.epoch() - 1,
                        alt_invalidation_slot: Slot::MAX,
                    },
                    MaxAge {
                        sanitized_epoch: bank.epoch(),
                        alt_invalidation_slot: Slot::MAX,
                    },
                    MaxAge {
                        sanitized_epoch: bank.epoch() + 1,
                        alt_invalidation_slot: Slot::MAX,
                    },
                    MaxAge {
                        sanitized_epoch: bank.epoch(),
                        alt_invalidation_slot: bank.slot() - 1,
                    },
                    MaxAge {
                        sanitized_epoch: bank.epoch(),
                        alt_invalidation_slot: bank.slot(),
                    },
                    MaxAge {
                        sanitized_epoch: bank.epoch(),
                        alt_invalidation_slot: bank.slot() + 1,
                    },
                ],
            })
            .unwrap();

        let consumed = consumed_receiver.recv().unwrap();
        assert_eq!(consumed.retryable_indexes, Vec::new());
        // all but one succeed. 6 for initial funding
        assert_eq!(bank.transaction_count(), 6 + 5);

        let already_processed_results = Consumer::check_transactions_for_scheduling(
            bank,
            &sanitized_txs,
            &vec![Ok(()); sanitized_txs.len()],
            bank.max_processing_age(),
            &mut TransactionErrorMetrics::default(),
        )
        .into_iter()
        .map(|r| match r {
            Ok(_) => Ok(()),
            Err(err) => Err(err),
        })
        .collect::<Vec<_>>();
        assert_eq!(
            already_processed_results,
            vec![
                Err(TransactionError::AlreadyProcessed),
                Err(TransactionError::AlreadyProcessed),
                Err(TransactionError::AlreadyProcessed),
                Ok(()), // <--- this transaction was not processed
                Err(TransactionError::AlreadyProcessed),
                Err(TransactionError::AlreadyProcessed)
            ]
        );

        drop(test_frame);
        let _ = worker_thread.join().unwrap();
    }

    #[test]
    fn test_backoff() {
        let sleep_duration = STARTING_SLEEP_DURATION;

        // No idle time - does not increase duration for next sleep.
        let sleep_duration = backoff(Duration::ZERO, &sleep_duration);
        assert_eq!(sleep_duration, STARTING_SLEEP_DURATION);

        // Longer time idling we sleep and double the next time.
        let sleep_duration = backoff(IDLE_SLEEP_THRESHOLD, &sleep_duration);
        assert_eq!(sleep_duration, STARTING_SLEEP_DURATION.saturating_mul(2));

        // Maximum sleep time
        let sleep_duration = Duration::from_micros(900);
        let sleep_duration = backoff(IDLE_SLEEP_THRESHOLD, &sleep_duration);
        assert_eq!(sleep_duration, MAX_SLEEP_DURATION);
    }
}
