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
            atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
            Arc,
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
                    return Err(ConsumeWorkerError::Recv(TryRecvError::Disconnected))
                }
            }
        }

        Ok(())
    }

    fn consume(
        &self,
        work: ConsumeWork<Tx>,
    ) -> Result<ProcessingStatus<Tx>, ConsumeWorkerError<Tx>> {
        let Some(leader_state) = active_leader_state_with_timeout(&self.shared_leader_state) else {
            return Ok(ProcessingStatus::CouldNotProcess(work));
        };
        let bank = leader_state
            .working_bank()
            .expect("active_leader_state_with_timeout should only return an active bank");
        self.metrics
            .count_metrics
            .num_messages_processed
            .fetch_add(1, Ordering::Relaxed);

        let output = self.consumer.process_and_record_aged_transactions(
            bank,
            &work.transactions,
            &work.max_ages,
            ExecutionFlags::default(),
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

#[allow(dead_code)]
#[cfg(unix)]
pub(crate) mod external {
    use {
        super::*,
        crate::banking_stage::{
            committer::CommitTransactionDetails,
            scheduler_messages::MaxAge,
            transaction_scheduler::receive_and_buffer::{
                translate_to_runtime_view, PacketHandlingError,
            },
        },
        agave_scheduler_bindings::{
            pack_message_flags::{self, check_flags, execution_flags},
            processed_codes,
            worker_message_types::{
                fee_payer_balance_flags, not_included_reasons, parsing_and_sanitization_flags,
                resolve_flags, status_check_flags, CheckResponse, ExecutionResponse,
            },
            PackToWorkerMessage, SharablePubkeys, TransactionResponseRegion, WorkerToPackMessage,
            MAX_TRANSACTIONS_PER_MESSAGE,
        },
        agave_scheduling_utils::{
            error::transaction_error_to_not_included_reason,
            responses_region::{allocate_check_response_region, execution_responses_from_iter},
            transaction_ptr::{TransactionPtr, TransactionPtrBatch},
        },
        agave_transaction_view::{
            resolved_transaction_view::ResolvedTransactionView, result::TransactionViewError,
            transaction_data::TransactionData, transaction_view::SanitizedTransactionView,
        },
        solana_account::ReadableAccount,
        solana_clock::{Slot, MAX_PROCESSING_AGE},
        solana_cost_model::cost_model::CostModel,
        solana_message::v0::LoadedAddresses,
        solana_pubkey::Pubkey,
        solana_runtime::{
            bank::Bank,
            bank_forks::{BankPair, SharableBanks},
        },
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_transaction::TransactionError,
        std::ptr::NonNull,
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
        sender: shaq::Producer<WorkerToPackMessage>,
        allocator: rts_alloc::Allocator,

        shared_leader_state: SharedLeaderState,
        sharable_banks: SharableBanks,
        metrics: Arc<ConsumeWorkerMetrics>,
    }

    type Tx = RuntimeTransaction<ResolvedTransactionView<TransactionPtr>>;
    type TxView = SanitizedTransactionView<TransactionPtr>;

    impl ExternalWorker {
        pub fn new(
            id: u32,
            exit: Arc<AtomicBool>,
            consumer: Consumer,
            sender: shaq::Producer<WorkerToPackMessage>,
            allocator: rts_alloc::Allocator,
            shared_leader_state: SharedLeaderState,
            sharable_banks: SharableBanks,
        ) -> Self {
            Self {
                exit,
                consumer,
                sender,
                allocator,
                shared_leader_state,
                sharable_banks,
                metrics: Arc::new(ConsumeWorkerMetrics::new(id)),
            }
        }

        pub fn metrics_handle(&self) -> Arc<ConsumeWorkerMetrics> {
            self.metrics.clone()
        }

        pub fn run(
            mut self,
            mut receiver: shaq::Consumer<PackToWorkerMessage>,
        ) -> Result<(), ExternalConsumeWorkerError> {
            let mut should_drain_executes = false;
            let mut did_work = false;
            let mut last_empty_time = Instant::now();
            let mut sleep_duration = STARTING_SLEEP_DURATION;

            while !self.exit.load(Ordering::Relaxed) {
                self.allocator.clean_remote_free_lists();
                if receiver.is_empty() {
                    receiver.sync();
                    should_drain_executes = false;
                }

                match receiver.try_read() {
                    Some(message) => {
                        did_work = true;
                        self.sender.sync();
                        should_drain_executes |=
                            self.process_message(message, should_drain_executes)?;
                        self.sender.commit();
                        receiver.finalize();
                    }
                    None => {
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

        /// Return true if fetching a bank for execution timed out.
        fn process_message(
            &mut self,
            message: &PackToWorkerMessage,
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

            if message.flags & pack_message_flags::EXECUTE == 1 {
                self.execute_batch(message, should_drain_executes)
            } else {
                self.check_batch(message).map(|()| false)
            }
        }

        /// Return true if fetching a bank for execution timed out.
        fn execute_batch(
            &mut self,
            message: &PackToWorkerMessage,
            should_drain_executes: bool,
        ) -> Result<bool, ExternalConsumeWorkerError> {
            if should_drain_executes {
                return self
                    .return_not_included_with_reason(
                        message,
                        not_included_reasons::BANK_NOT_AVAILABLE,
                    )
                    .map(|()| true);
            }

            // Loop here to avoid exposing internal error to external scheduler.
            // In the vast majority of cases, this will iterate a single time;
            // If we began execution when a slot was still in process, and could
            // not record at the end because the slot has ended, we will retry
            // on the next slot.
            for _ in 0..1 {
                let Some(leader_state) =
                    active_leader_state_with_timeout(&self.shared_leader_state)
                else {
                    return self
                        .return_not_included_with_reason(
                            message,
                            not_included_reasons::BANK_NOT_AVAILABLE,
                        )
                        .map(|()| true);
                };

                let bank = leader_state
                    .working_bank()
                    .expect("active_leader_state_with_timeout should only return an active bank");
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
                    drop_on_failure: message.flags & execution_flags::DROP_ON_FAILURE != 0,
                    all_or_nothing: message.flags & execution_flags::ALL_OR_NOTHING != 0,
                };
                if execution_flags.all_or_nothing && translation_results.len() != transactions.len()
                {
                    self.send_execution_response(
                        message,
                        Self::all_or_nothing_translate_iterator(&translation_results),
                    )?;

                    return Ok(false);
                }

                let output = self.consumer.process_and_record_aged_transactions(
                    bank,
                    &transactions,
                    &max_ages,
                    execution_flags,
                );

                self.metrics.update_for_consume(&output);
                self.metrics.has_data.store(true, Ordering::Relaxed);

                let Ok(commit_results) = output
                    .execute_and_commit_transactions_output
                    .commit_transactions_result
                else {
                    // If already ON the last possible execution slot,
                    // immediately give up instead of trying on next slot.
                    if bank.slot() == message.max_working_slot {
                        break;
                    }
                    continue; // recording failed, try again on next slot if possible.
                };

                self.send_execution_response(
                    message,
                    Self::consume_response_iterator(
                        &translation_results,
                        &transactions,
                        &commit_results,
                        bank,
                    ),
                )?;

                return Ok(false);
            }

            // If not successfully recorded even after second attempt, then we
            // just return immediately as if a bank is not available.
            self.return_not_included_with_reason(message, not_included_reasons::BANK_NOT_AVAILABLE)
                .map(|()| false)
        }

        fn check_batch(
            &mut self,
            message: &PackToWorkerMessage,
        ) -> Result<(), ExternalConsumeWorkerError> {
            let BankPair {
                root_bank,
                working_bank,
            } = self.sharable_banks.load();

            if working_bank.slot() > message.max_working_slot {
                return self.return_unprocessed_message(
                    message,
                    processed_codes::MAX_WORKING_SLOT_EXCEEDED,
                );
            }

            // SAFETY: Assumption that external scheduler does not pass messages with batch regions
            //         not pointing to valid regions in the allocator.
            let batch = unsafe {
                TransactionPtrBatch::from_sharable_transaction_batch_region(
                    &message.batch,
                    &self.allocator,
                )
            };

            // Allocate space for all responses.
            let (responses_ptr, responses) = allocate_check_response_region(
                &self.allocator,
                usize::from(message.batch.num_transactions),
            )
            .ok_or(ExternalConsumeWorkerError::AllocationFailure)?;

            // SAFETY: responses_ptr is sufficiently sized and aligned.
            let (parsing_results, parsed_transactions, response_slice) = unsafe {
                Self::parse_transactions_and_populate_initial_check_responses(
                    message,
                    &batch,
                    &root_bank,
                    responses_ptr,
                )
            };

            // Check fee-payer if requested.
            if message.flags & check_flags::LOAD_FEE_PAYER_BALANCE != 0 {
                Self::check_load_fee_payer_balance(
                    &parsing_results,
                    &parsed_transactions,
                    response_slice,
                    &working_bank,
                );
            }

            // Do resolving next since we (currently) need resolved transactions for status checks.
            let (parsing_and_resolve_results, txs, max_ages) =
                Self::translate_transaction_batch(&batch, &root_bank);

            if message.flags & check_flags::LOAD_ADDRESS_LOOKUP_TABLES != 0 {
                self.check_resolve_pubkeys(
                    &parsing_results,
                    &parsing_and_resolve_results,
                    &txs,
                    &max_ages,
                    response_slice,
                    root_bank.slot(),
                )?;
            }

            if message.flags & check_flags::STATUS_CHECKS != 0 {
                Self::check_status_checks(
                    &parsing_and_resolve_results,
                    &txs,
                    response_slice,
                    &working_bank,
                );
            }

            let response = WorkerToPackMessage {
                batch: message.batch,
                processed_code: agave_scheduler_bindings::processed_codes::PROCESSED,
                responses,
            };

            self.sender
                .try_write(response)
                .map_err(|_| ExternalConsumeWorkerError::SenderDisconnected)?;

            Ok(())
        }

        fn send_execution_response(
            &mut self,
            message: &PackToWorkerMessage,
            iter: impl ExactSizeIterator<Item = ExecutionResponse>,
        ) -> Result<(), ExternalConsumeWorkerError> {
            let responses = execution_responses_from_iter(&self.allocator, iter)
                .ok_or(ExternalConsumeWorkerError::AllocationFailure)?;
            let response = WorkerToPackMessage {
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
        ) -> impl ExactSizeIterator<Item = ExecutionResponse> + '_ {
            translation_results.iter().map(|res| ExecutionResponse {
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
                        Self::response_from_commit_details(tx, commit_details, bank)
                    }
                    Err(err) => ExecutionResponse {
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
            message: &PackToWorkerMessage,
            reason: u8,
        ) -> Result<(), ExternalConsumeWorkerError> {
            let response_region = execution_responses_from_iter(
                &self.allocator,
                (0..message.batch.num_transactions).map(|_| ExecutionResponse {
                    not_included_reason: reason,
                    cost_units: 0,
                    fee_payer_balance: 0,
                }),
            )
            .ok_or(ExternalConsumeWorkerError::AllocationFailure)?;

            let response_message = WorkerToPackMessage {
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

        fn check_resolve_pubkeys(
            &self,
            parsing_results: &[Result<(), TransactionViewError>],
            parsing_and_resolve_results: &[Result<(), PacketHandlingError>],
            txs: &[Tx],
            max_ages: &[MaxAge],
            responses: &mut [CheckResponse],
            resolution_slot: Slot,
        ) -> Result<(), ExternalConsumeWorkerError> {
            assert_eq!(parsing_results.len(), parsing_and_resolve_results.len());
            assert_eq!(parsing_results.len(), responses.len());

            let mut resolved_transaction_iter = txs.iter();
            let mut max_age_iter = max_ages.iter();
            for (transaction_index, (parsing_result, parsing_and_resolve_results)) in
                parsing_results
                    .iter()
                    .zip(parsing_and_resolve_results.iter())
                    .enumerate()
            {
                if parsing_result.is_err() {
                    continue;
                }

                let response = &mut responses[transaction_index];
                response.resolve_flags |= resolve_flags::PERFORMED;
                if parsing_and_resolve_results.is_err() {
                    response.resolve_flags |= resolve_flags::FAILED;
                    continue;
                }

                let transaction = resolved_transaction_iter.next().expect(
                    "resolved_transaction_iter iterator must contain element for each sent parsed \
                     transaction",
                );
                let max_age = max_age_iter.next().expect(
                    "max_age_iter iterator must contain element for each sent parsed transaction",
                );

                // There are 3 cases here:
                // 1. None - Tx format does not support ATL
                // 2. Some(empty) - V0 Tx with no ATL
                // 3. Some(keys) - V0 Tx with ATL
                // Only in case 3 will we create a shared allocation and copy keys.
                let (sharable_keys, alt_invalidation_slot) = match transaction.loaded_addresses() {
                    Some(loaded_addresses) if !loaded_addresses.is_empty() => {
                        let num_pubkeys = loaded_addresses.len();
                        let pubkeys_allocation = self
                            .allocator
                            .allocate(
                                num_pubkeys.wrapping_mul(core::mem::size_of::<Pubkey>()) as u32
                            )
                            .ok_or(ExternalConsumeWorkerError::AllocationFailure)?
                            .cast();
                        // SAFETY: non-overlapping and appropriately sized.
                        unsafe {
                            Self::copy_loaded_addresses(loaded_addresses, pubkeys_allocation)
                        };
                        // SAFETY: pubkeys_allocation was allocated by allocator
                        let offset = unsafe { self.allocator.offset(pubkeys_allocation.cast()) };
                        (
                            SharablePubkeys {
                                offset,
                                num_pubkeys: num_pubkeys as u32,
                            },
                            max_age.alt_invalidation_slot,
                        )
                    }
                    _ => (
                        SharablePubkeys {
                            offset: 0,
                            num_pubkeys: 0,
                        },
                        u64::MAX,
                    ),
                };

                response.resolution_slot = resolution_slot;
                response.resolved_pubkeys = sharable_keys;
                response.min_alt_deactivation_slot = alt_invalidation_slot;
            }

            Ok(())
        }

        fn return_unprocessed_message(
            &mut self,
            message: &PackToWorkerMessage,
            processed_code: u8,
        ) -> Result<(), ExternalConsumeWorkerError> {
            assert_ne!(
                processed_code,
                agave_scheduler_bindings::processed_codes::PROCESSED
            );
            let response = WorkerToPackMessage {
                batch: message.batch,
                processed_code,
                responses: TransactionResponseRegion {
                    tag: 0,
                    num_transaction_responses: 0,
                    transaction_responses_offset: 0,
                },
            };

            self.sender
                .try_write(response)
                .map_err(|_| ExternalConsumeWorkerError::SenderDisconnected)?;

            Ok(())
        }

        /// # Safety:
        /// - `responses_ptr` must be aligned and sufficiently sized.
        unsafe fn parse_transactions_and_populate_initial_check_responses<'a>(
            message: &PackToWorkerMessage,
            batch: &TransactionPtrBatch,
            bank: &Bank,
            responses_ptr: NonNull<CheckResponse>,
        ) -> (
            Vec<Result<(), TransactionViewError>>,
            Vec<TxView>,
            &'a mut [CheckResponse],
        ) {
            let enable_static_instruction_limit = bank
                .feature_set
                .is_active(&agave_feature_set::static_instruction_limit::ID);
            let mut parsing_results = Vec::with_capacity(MAX_TRANSACTIONS_PER_MESSAGE);
            let mut parsed_transactions = Vec::with_capacity(MAX_TRANSACTIONS_PER_MESSAGE);
            for (tx_ptr, _) in batch.iter() {
                // Parsing and basic sanitization checks
                match SanitizedTransactionView::try_new_sanitized(
                    tx_ptr,
                    enable_static_instruction_limit,
                ) {
                    Ok(view) => {
                        parsing_results.push(Ok(()));
                        parsed_transactions.push(view);
                    }
                    Err(err) => {
                        parsing_results.push(Err(err));
                    }
                }
            }

            // SAFETY: `response_ptr` is valid and of length message.batch.num_transactions
            unsafe {
                Self::check_populate_initial_messages(message, &parsing_results, responses_ptr)
            };
            // SAFETY: `response_ptr` is valid and of length message.batch.num_transactions
            //         initial messages populated immediately above, so not possible to have
            //         uninitialized values either.
            let response_slice = unsafe {
                core::slice::from_raw_parts_mut(
                    responses_ptr.as_ptr(),
                    usize::from(message.batch.num_transactions),
                )
            };

            (parsing_results, parsed_transactions, response_slice)
        }

        /// Write initial response value for each transaction.
        /// This allows simpler modification of individual responses in further checks.
        /// # Safety
        /// - `responses_ptr` is valid ptr for a slice of [`CheckResponse`] with at least
        ///   length `message.batch.num_transactions`
        unsafe fn check_populate_initial_messages(
            message: &PackToWorkerMessage,
            parsing_results: &[Result<(), TransactionViewError>],
            responses_ptr: NonNull<CheckResponse>,
        ) {
            // Populate initial responses with the result of parsing/sanitization.
            assert_eq!(
                parsing_results.len(),
                usize::from(message.batch.num_transactions)
            );
            let initial_status_check_flags = if message.flags & check_flags::STATUS_CHECKS != 0 {
                status_check_flags::REQUESTED
            } else {
                0
            };
            let initial_fee_payer_balance_flags =
                if message.flags & check_flags::LOAD_FEE_PAYER_BALANCE != 0 {
                    fee_payer_balance_flags::REQUESTED
                } else {
                    0
                };
            let initial_resolve_flags =
                if message.flags & check_flags::LOAD_ADDRESS_LOOKUP_TABLES != 0 {
                    resolve_flags::REQUESTED
                } else {
                    0
                };
            // Setup initial responses with requested checks.
            // Values only filled in when check is performed.
            for (transaction_index, parsing_result) in parsing_results.iter().enumerate() {
                // NOTE: this includes resolution failures since this is a necessary check for further checks.
                let parsing_and_sanitization_flags = if parsing_result.is_err() {
                    parsing_and_sanitization_flags::FAILED
                } else {
                    0
                };

                // SAFETY: transaaction_index is in bounds.
                unsafe {
                    responses_ptr.add(transaction_index).write(CheckResponse {
                        parsing_and_sanitization_flags,
                        status_check_flags: initial_status_check_flags,
                        fee_payer_balance_flags: initial_fee_payer_balance_flags,
                        resolve_flags: initial_resolve_flags,
                        included_slot: 0,
                        balance_slot: 0,
                        fee_payer_balance: 0,
                        resolution_slot: 0,
                        min_alt_deactivation_slot: 0,
                        resolved_pubkeys: SharablePubkeys {
                            offset: 0,
                            num_pubkeys: 0,
                        },
                    })
                };
            }
        }

        fn check_load_fee_payer_balance<D: TransactionData>(
            parsing_results: &[Result<(), TransactionViewError>],
            parsed_transactions: &[SanitizedTransactionView<D>],
            responses: &mut [CheckResponse],
            working_bank: &Bank,
        ) {
            assert_eq!(responses.len(), parsing_results.len());

            let mut parsed_transaction_iter = parsed_transactions.iter();
            for (transaction_index, parsing_result) in parsing_results.iter().enumerate() {
                if parsing_result.is_err() {
                    continue;
                }

                let transaction = parsed_transaction_iter.next().expect(
                    "parsed_transaction_iter iterator must contain element for each sent parsed \
                     transaction",
                );

                let fee_payer_balance = working_bank
                    .rc
                    .accounts
                    .accounts_db
                    .load_with_fixed_root(
                        &working_bank.ancestors,
                        &transaction.static_account_keys()[0],
                    )
                    .map(|(account, _slot)| account.lamports())
                    .unwrap_or(0);

                let response = &mut responses[transaction_index];
                response.fee_payer_balance_flags |= fee_payer_balance_flags::PERFORMED;
                response.fee_payer_balance = fee_payer_balance;
                response.balance_slot = working_bank.slot();
            }
        }

        fn check_status_checks<D: TransactionData>(
            parsing_and_resolve_results: &[Result<(), PacketHandlingError>],
            txs: &[RuntimeTransaction<ResolvedTransactionView<D>>],
            responses: &mut [CheckResponse],
            working_bank: &Bank,
        ) {
            assert_eq!(parsing_and_resolve_results.len(), responses.len());

            let mut error_counters = TransactionErrorMetrics::default();
            let (status_check_results, included_slots) = working_bank
                .check_transactions_with_processed_slots(
                    txs,
                    &[const { Ok(()) }; MAX_TRANSACTIONS_PER_MESSAGE],
                    MAX_PROCESSING_AGE,
                    true,
                    &mut error_counters,
                );
            let included_slots = included_slots.expect("requested to collect processed slots");

            let mut status_check_results_iter =
                status_check_results.iter().zip(included_slots.iter());
            for (transaction_index, parsing_and_resolve_result) in
                parsing_and_resolve_results.iter().enumerate()
            {
                if parsing_and_resolve_result.is_err() {
                    continue;
                }
                let (status_check_result, included_slot) = status_check_results_iter
                    .next()
                    .expect("status check results must have element for each sent transaction");

                let check_response = &mut responses[transaction_index];
                check_response.status_check_flags |= status_check_flags::PERFORMED;
                match status_check_result {
                    Err(TransactionError::BlockhashNotFound) => {
                        check_response.status_check_flags |= status_check_flags::TOO_OLD;
                    }
                    Err(TransactionError::AlreadyProcessed) => {
                        check_response.status_check_flags |= status_check_flags::ALREADY_PROCESSED;
                        check_response.included_slot =
                            included_slot.expect("included_slot must be set for already processed");
                    }
                    _ => {}
                }
            }
        }

        /// Translate batch of transactions into usable
        fn translate_transaction_batch(
            batch: &TransactionPtrBatch,
            bank: &Bank,
        ) -> (Vec<Result<(), PacketHandlingError>>, Vec<Tx>, Vec<MaxAge>) {
            let enable_static_instruction_limit = bank
                .feature_set
                .is_active(&agave_feature_set::static_instruction_limit::ID);
            let transaction_account_lock_limit = bank.get_transaction_account_lock_limit();

            let mut translation_results = Vec::with_capacity(MAX_TRANSACTIONS_PER_MESSAGE);
            let mut transactions = Vec::with_capacity(MAX_TRANSACTIONS_PER_MESSAGE);
            let mut max_ages = Vec::with_capacity(MAX_TRANSACTIONS_PER_MESSAGE);
            for (transaction_ptr, _) in batch.iter() {
                match Self::translate_transaction(
                    transaction_ptr,
                    bank,
                    enable_static_instruction_limit,
                    transaction_account_lock_limit,
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
            enable_static_instruction_limit: bool,
            transaction_account_lock_limit: usize,
        ) -> Result<(Tx, MaxAge), PacketHandlingError> {
            translate_to_runtime_view(
                transaction_ptr,
                bank,
                enable_static_instruction_limit,
                transaction_account_lock_limit,
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

        /// # Safety
        /// - destination is appropriately sized
        /// - destination does not overlap with loaded_addresses allocation
        unsafe fn copy_loaded_addresses(loaded_addresses: &LoadedAddresses, dest: NonNull<Pubkey>) {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    loaded_addresses.writable.as_ptr(),
                    dest.as_ptr(),
                    loaded_addresses.writable.len(),
                );
                core::ptr::copy_nonoverlapping(
                    loaded_addresses.readonly.as_ptr(),
                    dest.add(loaded_addresses.writable.len()).as_ptr(),
                    loaded_addresses.readonly.len(),
                );
            }
        }

        /// Returns `true` if a message is valid and can be processed.
        fn validate_message(message: &PackToWorkerMessage) -> bool {
            message.batch.num_transactions > 0
                && usize::from(message.batch.num_transactions) <= MAX_TRANSACTIONS_PER_MESSAGE
                && Self::validate_message_flags(message.flags)
        }

        fn validate_message_flags(flags: u16) -> bool {
            if flags & pack_message_flags::EXECUTE != 0 {
                const ALLOWED_EXECUTE_FLAGS: u16 = pack_message_flags::EXECUTE;

                flags & !ALLOWED_EXECUTE_FLAGS == 0
            } else {
                const ALLOWED_CHECK_BITS: u16 = pack_message_flags::CHECK
                    | check_flags::STATUS_CHECKS
                    | check_flags::LOAD_FEE_PAYER_BALANCE
                    | check_flags::LOAD_ADDRESS_LOOKUP_TABLES;

                flags != pack_message_flags::CHECK && flags & !ALLOWED_CHECK_BITS == 0
            }
        }

        fn response_from_commit_details(
            tx: &impl TransactionWithMeta,
            commit_details: &CommitTransactionDetails,
            bank: &Bank,
        ) -> ExecutionResponse {
            match commit_details {
                CommitTransactionDetails::Committed {
                    compute_units,
                    loaded_accounts_data_size,
                    fee_payer_post_balance,
                    ..
                } => ExecutionResponse {
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
                    not_included_reason: transaction_error_to_not_included_reason(
                        transaction_error,
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
            super::*, agave_scheduler_bindings::SharableTransactionBatchRegion,
            solana_account::AccountSharedData, solana_runtime::genesis_utils,
            solana_sdk_ids::system_program, solana_system_transaction::transfer,
            solana_transaction::TransactionError, std::collections::HashSet,
        };

        #[test]
        fn test_validate_message() {
            let mut message = PackToWorkerMessage {
                flags: agave_scheduler_bindings::pack_message_flags::EXECUTE,
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

            message.flags = pack_message_flags::EXECUTE;
            assert!(ExternalWorker::validate_message(&message));
        }

        #[test]
        fn test_validate_message_flags() {
            assert!(ExternalWorker::validate_message_flags(
                pack_message_flags::EXECUTE
            ));
            assert!(ExternalWorker::validate_message_flags(
                pack_message_flags::CHECK
                    | agave_scheduler_bindings::pack_message_flags::check_flags::LOAD_ADDRESS_LOOKUP_TABLES
            ));
            assert!(!ExternalWorker::validate_message_flags(
                pack_message_flags::CHECK
            ))
        }

        #[test]
        fn test_consume_response_iterator() {
            let simple_tx = bincode::serialize(&transfer(
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
                        true,
                        bank.get_transaction_account_lock_limit(),
                    )
                    .ok()
                    .unwrap()
                    .0
                })
                .collect::<Vec<_>>();

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
            )
            .collect::<Vec<_>>();

            assert_eq!(
                responses,
                &[
                    ExecutionResponse {
                        not_included_reason: not_included_reasons::SANITIZE_FAILURE,
                        cost_units: 0,
                        fee_payer_balance: 0
                    },
                    ExecutionResponse {
                        not_included_reason: not_included_reasons::NONE,
                        cost_units: 1337,
                        fee_payer_balance: 1_000_000,
                    },
                    ExecutionResponse {
                        not_included_reason: not_included_reasons::NONE,
                        cost_units: 1341,
                        fee_payer_balance: 2_000_000,
                    },
                    ExecutionResponse {
                        not_included_reason: not_included_reasons::INSUFFICIENT_FUNDS_FOR_FEE,
                        cost_units: 0,
                        fee_payer_balance: 0,
                    }
                ]
            )
        }

        #[test]
        fn test_all_or_nothing_translate_iterator() {
            let translation_results = vec![Ok(()), Err(PacketHandlingError::Sanitization), Ok(())];

            let responses = ExternalWorker::all_or_nothing_translate_iterator(&translation_results)
                .collect::<Vec<_>>();

            assert_eq!(
                responses,
                &[
                    ExecutionResponse {
                        not_included_reason: not_included_reasons::ALL_OR_NOTHING_BATCH_FAILURE,
                        cost_units: 0,
                        fee_payer_balance: 0
                    },
                    ExecutionResponse {
                        not_included_reason: not_included_reasons::SANITIZE_FAILURE,
                        cost_units: 0,
                        fee_payer_balance: 0,
                    },
                    ExecutionResponse {
                        not_included_reason: not_included_reasons::ALL_OR_NOTHING_BATCH_FAILURE,
                        cost_units: 0,
                        fee_payer_balance: 0,
                    },
                ]
            )
        }

        fn empty_check_responses(count: u8) -> Vec<CheckResponse> {
            (0..count)
                .map(|_| CheckResponse {
                    parsing_and_sanitization_flags: 0,
                    status_check_flags: 0,
                    fee_payer_balance_flags: 0,
                    resolve_flags: 0,
                    included_slot: 0,
                    balance_slot: 0,
                    fee_payer_balance: 0,
                    resolution_slot: 0,
                    min_alt_deactivation_slot: 0,
                    resolved_pubkeys: SharablePubkeys {
                        offset: 0,
                        num_pubkeys: 0,
                    },
                })
                .collect()
        }

        #[test]
        fn test_check_populate_initial_messages() {
            let message = PackToWorkerMessage {
                flags: pack_message_flags::CHECK | check_flags::LOAD_FEE_PAYER_BALANCE,
                max_working_slot: 1,
                batch: SharableTransactionBatchRegion {
                    num_transactions: 3,
                    transactions_offset: 0,
                },
            };

            let mut responses = empty_check_responses(message.batch.num_transactions);
            let responses_ptr = NonNull::new(responses.as_mut_ptr()).unwrap();

            unsafe {
                ExternalWorker::check_populate_initial_messages(
                    &message,
                    &[
                        Ok(()),
                        Err(TransactionViewError::ParseError),
                        Err(TransactionViewError::SanitizeError),
                    ],
                    responses_ptr,
                )
            };

            assert_eq!(responses[0].parsing_and_sanitization_flags, 0);
            assert_eq!(
                responses[1].parsing_and_sanitization_flags,
                parsing_and_sanitization_flags::FAILED
            );
            assert_eq!(
                responses[2].parsing_and_sanitization_flags,
                parsing_and_sanitization_flags::FAILED
            );
            for response in responses.iter() {
                assert_eq!(
                    response.fee_payer_balance_flags,
                    fee_payer_balance_flags::REQUESTED
                );
            }
        }

        fn test_serialized_transaction(recent_blockhash: solana_hash::Hash) -> Vec<u8> {
            let tx = transfer(
                &solana_keypair::Keypair::new(),
                &Pubkey::new_unique(),
                1,
                recent_blockhash,
            );
            bincode::serialize(&tx).unwrap()
        }

        #[test]
        fn test_check_load_fee_payer_balance() {
            let genesis = genesis_utils::create_genesis_config(1_000_000_000);
            let bank = Bank::new_for_tests(&genesis.genesis_config);

            let tx1 = test_serialized_transaction(bank.confirmed_last_blockhash());
            let tx2 = test_serialized_transaction(bank.confirmed_last_blockhash());

            let parsing_results = [Ok(()), Err(TransactionViewError::ParseError), Ok(())];
            let parsed_transactions = [
                SanitizedTransactionView::try_new_sanitized(&tx1[..], true).unwrap(),
                SanitizedTransactionView::try_new_sanitized(&tx2[..], true).unwrap(),
            ];
            bank.store_account(
                &parsed_transactions[1].static_account_keys()[0],
                &AccountSharedData::new(1_000_000_000, 0, &system_program::ID),
            );
            let mut responses = empty_check_responses(parsing_results.len() as u8);

            ExternalWorker::check_load_fee_payer_balance(
                &parsing_results[..],
                &parsed_transactions[..],
                &mut responses,
                &bank,
            );

            // test-note: requested is not set by this function.
            assert_eq!(
                responses[0].fee_payer_balance_flags,
                fee_payer_balance_flags::PERFORMED
            );
            assert_eq!(responses[0].balance_slot, bank.slot());
            assert_eq!(responses[0].fee_payer_balance, 0);

            assert_eq!(responses[1].fee_payer_balance_flags, 0);
            assert_eq!(responses[1].balance_slot, 0);
            assert_eq!(responses[1].fee_payer_balance, 0);

            assert_eq!(
                responses[2].fee_payer_balance_flags,
                fee_payer_balance_flags::PERFORMED
            );
            assert_eq!(responses[2].balance_slot, bank.slot());
            assert_eq!(responses[2].fee_payer_balance, 1_000_000_000);
        }

        #[test]
        fn test_check_status_checks() {
            let genesis = genesis_utils::create_genesis_config(1_000_000_000);
            let (bank, _bank_forks) =
                Bank::new_for_tests(&genesis.genesis_config).wrap_with_bank_forks_for_tests();

            let tx1 = test_serialized_transaction(bank.confirmed_last_blockhash());
            let tx2 = test_serialized_transaction(solana_hash::Hash::new_unique());
            let tx3 = test_serialized_transaction(bank.confirmed_last_blockhash());

            fn to_resolved_view(
                tx: &'_ [u8],
            ) -> RuntimeTransaction<ResolvedTransactionView<&'_ [u8]>> {
                RuntimeTransaction::<ResolvedTransactionView<_>>::try_from(
                    RuntimeTransaction::<SanitizedTransactionView<_>>::try_from(
                        SanitizedTransactionView::try_new_sanitized(tx, true).unwrap(),
                        solana_transaction::sanitized::MessageHash::Compute,
                        Some(false),
                    )
                    .unwrap(),
                    None,
                    &HashSet::new(),
                )
                .unwrap()
            }

            let parsing_and_resolve_results = [
                Ok(()),
                Err(PacketHandlingError::Sanitization),
                Ok(()),
                Ok(()),
            ];
            let parsed_transactions = [
                to_resolved_view(&tx1[..]),
                to_resolved_view(&tx2[..]),
                to_resolved_view(&tx3[..]),
            ];

            // Process transaction 3.
            bank.store_account(
                &parsed_transactions[2].static_account_keys()[0],
                &AccountSharedData::new(1_000_000_000, 0, &system_program::ID),
            );
            bank.process_transaction(&bincode::deserialize(&tx3).unwrap())
                .unwrap();

            // bank.store_account(
            //     &parsed_transactions[1].static_account_keys()[0],
            //     &AccountSharedData::new(1_000_000_000, 0, &system_program::ID),
            // );
            let mut responses = empty_check_responses(parsing_and_resolve_results.len() as u8);

            ExternalWorker::check_status_checks(
                &parsing_and_resolve_results,
                &parsed_transactions,
                &mut responses,
                &bank,
            );

            // test-note: requested is not set by this function.
            assert_eq!(
                responses[0].status_check_flags,
                status_check_flags::PERFORMED
            );

            assert_eq!(responses[1].status_check_flags, 0);

            assert_eq!(
                responses[2].status_check_flags,
                status_check_flags::PERFORMED | status_check_flags::TOO_OLD
            );

            assert_eq!(
                responses[3].status_check_flags,
                status_check_flags::PERFORMED | status_check_flags::ALREADY_PROCESSED
            );
            assert_eq!(responses[3].included_slot, bank.slot());
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
        fn test_copy_loaded_addresses() {
            let loaded_addresses = LoadedAddresses {
                writable: (0..5).map(|_| Pubkey::new_unique()).collect(),
                readonly: (0..2).map(|_| Pubkey::new_unique()).collect(),
            };
            let mut buffer = vec![Pubkey::default(); 7];
            unsafe {
                ExternalWorker::copy_loaded_addresses(
                    &loaded_addresses,
                    NonNull::new(buffer.as_mut_ptr()).unwrap(),
                )
            };

            assert_eq!(&loaded_addresses.writable, &buffer[0..5]);
            assert_eq!(&loaded_addresses.readonly, &buffer[5..7]);
        }
    }
}
/// Helper function to create an non-blocking iterator over work in the receiver,
/// starting with the given work item.
fn try_drain_iter<T>(work: T, receiver: &Receiver<T>) -> impl Iterator<Item = T> + '_ {
    std::iter::once(work).chain(receiver.try_iter())
}

/// Get active bank with timeout.
fn active_leader_state_with_timeout(
    shared_leader_state: &SharedLeaderState,
) -> Option<arc_swap::Guard<Arc<LeaderState>>> {
    // Do an initial bank load without sampling time. If we're in a hot loop
    // of work this saves us from checking the time at all and we'd only end up
    // checking between or after our leader slots.
    if let Some(guard) = active_leader_state(shared_leader_state) {
        return Some(guard);
    }

    // If the initial check above didn't find a bank, we will
    // spin up to some timeout to wait for a bank to execute on.
    // This is conservatively long because transitions between slots
    // can occasionally be slow.
    const TIMEOUT: Duration = Duration::from_millis(50);
    let now = Instant::now();
    while now.elapsed() < TIMEOUT {
        if let Some(guard) = active_leader_state(shared_leader_state) {
            return Some(guard);
        }
        core::hint::spin_loop();
    }

    None
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
            min_prioritization_fees,
            max_prioritization_fees,
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
        let min_prioritization_fees = self
            .count_metrics
            .min_prioritization_fees
            .fetch_min(*min_prioritization_fees, Ordering::Relaxed);
        let max_prioritization_fees = self
            .count_metrics
            .max_prioritization_fees
            .fetch_max(*max_prioritization_fees, Ordering::Relaxed);
        self.count_metrics
            .min_prioritization_fees
            .swap(min_prioritization_fees, Ordering::Relaxed);
        self.count_metrics
            .max_prioritization_fees
            .swap(max_prioritization_fees, Ordering::Relaxed);
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

struct ConsumeWorkerCountMetrics {
    max_queue_len: AtomicU64,
    num_messages_processed: AtomicU64,
    transactions_attempted_processing_count: AtomicU64,
    processed_transactions_count: AtomicU64,
    processed_with_successful_result_count: AtomicU64,
    retryable_transaction_count: AtomicUsize,
    retryable_expired_bank_count: AtomicUsize,
    cost_model_throttled_transactions_count: AtomicU64,
    min_prioritization_fees: AtomicU64,
    max_prioritization_fees: AtomicU64,
}

impl Default for ConsumeWorkerCountMetrics {
    fn default() -> Self {
        Self {
            max_queue_len: AtomicU64::default(),
            num_messages_processed: AtomicU64::default(),
            transactions_attempted_processing_count: AtomicU64::default(),
            processed_transactions_count: AtomicU64::default(),
            processed_with_successful_result_count: AtomicU64::default(),
            retryable_transaction_count: AtomicUsize::default(),
            retryable_expired_bank_count: AtomicUsize::default(),
            cost_model_throttled_transactions_count: AtomicU64::default(),
            min_prioritization_fees: AtomicU64::new(u64::MAX),
            max_prioritization_fees: AtomicU64::default(),
        }
    }
}

impl ConsumeWorkerCountMetrics {
    fn report_and_reset(&self, id: &str) {
        datapoint_info!(
            "banking_stage_worker_counts",
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
            (
                "min_prioritization_fees",
                self.min_prioritization_fees
                    .swap(u64::MAX, Ordering::Relaxed),
                i64
            ),
            (
                "max_prioritization_fees",
                self.max_prioritization_fees.swap(0, Ordering::Relaxed),
                i64
            ),
        );
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
        datapoint_info!(
            "banking_stage_worker_timing",
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
        datapoint_info!(
            "banking_stage_worker_error_metrics",
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
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::banking_stage::{
            committer::Committer,
            qos_service::QosService,
            scheduler_messages::{MaxAge, TransactionBatchId},
            tests::{create_slow_genesis_config, sanitize_transactions},
        },
        crossbeam_channel::unbounded,
        solana_clock::{Slot, MAX_PROCESSING_AGE},
        solana_genesis_config::GenesisConfig,
        solana_keypair::Keypair,
        solana_ledger::genesis_utils::GenesisConfigInfo,
        solana_message::{
            v0::{self, LoadedAddresses},
            AddressLookupTableAccount, SimpleAddressLoader, VersionedMessage,
        },
        solana_poh::{
            record_channels::{record_channels, RecordReceiver},
            transaction_recorder::TransactionRecorder,
        },
        solana_pubkey::Pubkey,
        solana_runtime::{
            bank::Bank, bank_forks::BankForks, prioritization_fee_cache::PrioritizationFeeCache,
            vote_sender_types::ReplayVoteReceiver,
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
            sync::{atomic::AtomicBool, RwLock},
        },
        test_case::test_case,
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

    fn setup_test_frame(
        relax_intrabatch_account_locks: bool,
    ) -> (
        TestFrame,
        ConsumeWorker<RuntimeTransaction<SanitizedTransaction>>,
    ) {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(10_000);
        let (bank, bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);
        // Warp to next epoch for MaxAge tests.
        let mut bank = Bank::new_from_parent(
            bank.clone(),
            &Pubkey::new_unique(),
            bank.get_epoch_info().slots_in_epoch,
        );
        if !relax_intrabatch_account_locks {
            bank.deactivate_feature(&agave_feature_set::relax_intrabatch_account_locks::id());
        }
        let bank = Arc::new(bank);

        let (record_sender, record_receiver) = record_channels(false);
        let recorder = TransactionRecorder::new(record_sender);

        let (replay_vote_sender, replay_vote_receiver) = unbounded();
        let committer = Committer::new(
            None,
            replay_vote_sender,
            Arc::new(PrioritizationFeeCache::new(0u64)),
        );
        let consumer = Consumer::new(committer, recorder, QosService::new(1), None);
        let shared_leader_state = SharedLeaderState::new(0, None, None);

        let (consume_sender, consume_receiver) = unbounded();
        let (consumed_sender, consumed_receiver) = unbounded();
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
        let (test_frame, worker) = setup_test_frame(true);
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
    fn test_worker_consume_simple() {
        let (mut test_frame, worker) = setup_test_frame(true);
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

    #[test_case(false; "old")]
    #[test_case(true; "simd83")]
    fn test_worker_consume_self_conflicting(relax_intrabatch_account_locks: bool) {
        let (mut test_frame, worker) = setup_test_frame(relax_intrabatch_account_locks);
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

        // id2 succeeds with simd83, or is retryable due to lock conflict without simd83
        assert_eq!(
            consumed.retryable_indexes,
            if relax_intrabatch_account_locks {
                vec![]
            } else {
                vec![RetryableIndex::new(1, true)]
            }
        );

        drop(test_frame);
        let _ = worker_thread.join().unwrap();
    }

    #[test]
    fn test_worker_consume_multiple_messages() {
        let (mut test_frame, worker) = setup_test_frame(true);
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
                batch_id: bid1,
                ids: vec![id1],
                transactions: txs1,
                max_ages: vec![max_age],
            })
            .unwrap();

        consume_sender
            .send(ConsumeWork {
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
        let (mut test_frame, worker) = setup_test_frame(true);
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
                bank.feature_set
                    .is_active(&agave_feature_set::static_instruction_limit::id()),
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

        let already_processed_results = bank
            .check_transactions(
                &sanitized_txs,
                &vec![Ok(()); sanitized_txs.len()],
                MAX_PROCESSING_AGE,
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
