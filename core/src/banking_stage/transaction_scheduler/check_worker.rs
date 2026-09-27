use {
    super::receive_and_buffer::{PrecheckResult, precheck_transaction},
    crossbeam_channel::{Receiver, Sender},
    solana_perf::packet::bytes::Bytes,
    solana_pubkey::Pubkey,
    solana_runtime::bank_forks::SharableBanks,
    std::{
        collections::HashSet,
        num::NonZeroUsize,
        sync::Arc,
        thread::{Builder, JoinHandle},
    },
};

pub(crate) fn spawn_check_workers(
    num_workers: NonZeroUsize,
    work_receiver: Receiver<Bytes>,
    result_sender: Sender<PrecheckResult>,
    sharable_banks: SharableBanks,
    filter_keys: Arc<HashSet<Pubkey>>,
) -> Vec<JoinHandle<()>> {
    (0..num_workers.get())
        .map(|index| {
            let work_receiver = work_receiver.clone();
            let result_sender = result_sender.clone();
            let sharable_banks = sharable_banks.clone();
            let filter_keys = filter_keys.clone();
            Builder::new()
                .name(format!("solBnkChk{index:02}"))
                .spawn(move || {
                    run_check_worker(work_receiver, result_sender, sharable_banks, filter_keys);
                })
                .expect("check worker thread must spawn")
        })
        .collect()
}

fn run_check_worker(
    work_receiver: Receiver<Bytes>,
    result_sender: Sender<PrecheckResult>,
    sharable_banks: SharableBanks,
    filter_keys: Arc<HashSet<Pubkey>>,
) {
    while let Ok(bytes) = work_receiver.recv() {
        let banks = sharable_banks.load();
        let result =
            precheck_transaction(bytes, &banks.root_bank, &banks.working_bank, &filter_keys);

        // A result queue at capacity applies backpressure to check workers. Accepted
        // work is never dropped by a worker.
        if result_sender.send(result).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, crate::banking_stage::tests::create_slow_genesis_config,
        crossbeam_channel::bounded, solana_ledger::genesis_utils::GenesisConfigInfo,
        solana_perf::packet::BytesPacket, solana_runtime::bank::Bank,
        solana_system_transaction::transfer,
    };

    fn test_banks() -> (SharableBanks, solana_keypair::Keypair) {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(u64::MAX);
        let (_bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        (bank_forks.read().unwrap().sharable_banks(), mint_keypair)
    }

    fn transaction_bytes(
        sharable_banks: &SharableBanks,
        mint_keypair: &solana_keypair::Keypair,
    ) -> Bytes {
        let transaction = transfer(
            mint_keypair,
            &Pubkey::new_unique(),
            1,
            sharable_banks.working().last_blockhash(),
        );
        BytesPacket::from_data(transaction)
            .unwrap()
            .buffer()
            .clone()
    }

    #[test]
    fn bounded_result_queue_does_not_drop_results() {
        let (sharable_banks, mint_keypair) = test_banks();
        let (work_sender, work_receiver) = bounded(8);
        let (result_sender, result_receiver) = bounded(1);
        let worker_handles = spawn_check_workers(
            NonZeroUsize::new(2).unwrap(),
            work_receiver,
            result_sender,
            sharable_banks.clone(),
            Arc::default(),
        );
        for _ in 0..8 {
            work_sender
                .send(transaction_bytes(&sharable_banks, &mint_keypair))
                .unwrap();
        }

        for _ in 0..8 {
            let result = result_receiver.recv().unwrap();
            assert!(result.is_ok());
        }

        drop(work_sender);
        drop(result_receiver);
        worker_handles
            .into_iter()
            .for_each(|handle| assert!(handle.join().is_ok()));
    }
}

#[cfg(unix)]
pub(crate) mod external {
    use {
        crate::banking_stage::transaction_scheduler::receive_and_buffer::translate_sanitized_to_runtime_view,
        agave_scheduler_bindings::{
            CheckResponseRegion, CheckWorkerToPackMessage, MAX_TRANSACTIONS_PER_MESSAGE,
            PackToCheckWorkerMessage, SharablePubkeys, check_message_flags, processed_codes,
            worker_message_types::{
                CheckResponse, fee_payer_balance_flags, parsing_and_sanitization_flags,
                resolve_flags, scheduling_details_flags, status_check_flags,
            },
        },
        agave_scheduling_utils::{
            responses_region::resolve_responses_from_iter,
            transaction_ptr::{TransactionPtr, TransactionPtrBatch},
        },
        agave_transaction_view::{
            resolved_transaction_view::ResolvedTransactionView,
            transaction_view::SanitizedTransactionView,
        },
        arrayvec::ArrayVec,
        solana_account::ReadableAccount,
        solana_clock::Slot,
        solana_cost_model::cost_model::CostModel,
        solana_message::v0::LoadedAddresses,
        solana_poh::poh_recorder::{LeaderState, SharedLeaderState},
        solana_pubkey::Pubkey,
        solana_runtime::{
            bank::Bank,
            bank_forks::{BankPair, SharableBanks},
        },
        solana_runtime_transaction::{
            runtime_transaction::RuntimeTransaction, sanitize_config::sanitize_config,
            transaction_meta::TransactionMeta,
        },
        solana_svm::transaction_error_metrics::TransactionErrorMetrics,
        solana_svm_transaction::svm_message::SVMStaticMessage,
        solana_transaction::TransactionError,
        std::{
            ptr::NonNull,
            sync::{
                Arc,
                atomic::{AtomicBool, Ordering},
            },
            time::Duration,
        },
        thiserror::Error,
    };

    type Tx = RuntimeTransaction<ResolvedTransactionView<TransactionPtr>>;
    type TxView = SanitizedTransactionView<TransactionPtr>;

    #[derive(Debug, Error)]
    pub(crate) enum ExternalCheckWorkerError {
        #[error("Sender disconnected")]
        SenderDisconnected,
        #[error("Allocation failed")]
        AllocationFailure,
    }

    pub(crate) enum IterationResult {
        ProcessedMessage,
        Idle,
    }

    #[allow(dead_code)]
    pub(crate) struct ExternalCheckWorker {
        exit: Arc<AtomicBool>,
        receiver: shaq::mpmc::Consumer<PackToCheckWorkerMessage>,
        sender: shaq::mpmc::Producer<CheckWorkerToPackMessage>,
        allocator: rts_alloc::Allocator,

        shared_leader_state: SharedLeaderState,
        sharable_banks: SharableBanks,
    }

    #[allow(dead_code)]
    impl ExternalCheckWorker {
        const RECEIVE_TIMEOUT: Duration = Duration::from_millis(10);

        pub fn new(
            exit: Arc<AtomicBool>,
            receiver: shaq::mpmc::Consumer<PackToCheckWorkerMessage>,
            sender: shaq::mpmc::Producer<CheckWorkerToPackMessage>,
            allocator: rts_alloc::Allocator,
            shared_leader_state: SharedLeaderState,
            sharable_banks: SharableBanks,
        ) -> Self {
            Self {
                exit,
                receiver,
                sender,
                allocator,
                shared_leader_state,
                sharable_banks,
            }
        }

        pub fn run(mut self) -> Result<(), ExternalCheckWorkerError> {
            while !self.exit.load(Ordering::Relaxed) {
                self.iterate(Self::RECEIVE_TIMEOUT)?;
            }

            Ok(())
        }

        pub(crate) fn iterate(
            &mut self,
            timeout: Duration,
        ) -> Result<IterationResult, ExternalCheckWorkerError> {
            self.allocator.clean_remote_frees();

            match self.receiver.read_timeout(timeout) {
                Ok(message) => {
                    self.process_message(&message)?;
                    Ok(IterationResult::ProcessedMessage)
                }
                Err(shaq::error::WaitError::Timeout) => Ok(IterationResult::Idle),
            }
        }

        fn process_message(
            &mut self,
            message: &PackToCheckWorkerMessage,
        ) -> Result<(), ExternalCheckWorkerError> {
            if !Self::validate_message(message) {
                return self.return_unprocessed_message(message, processed_codes::INVALID);
            }

            self.check_batch(message)
        }

        fn check_batch(
            &mut self,
            message: &PackToCheckWorkerMessage,
        ) -> Result<(), ExternalCheckWorkerError> {
            let BankPair {
                root_bank,
                working_bank,
            } = self.sharable_banks.load();
            // Prefer the leader bank over the highest working fork when leader.
            let working_bank = active_leader_state(&self.shared_leader_state)
                .and_then(|leader_state| leader_state.working_bank().cloned())
                .unwrap_or(working_bank);

            // SAFETY: Assumption that external scheduler does not pass messages with batch regions
            //         not pointing to valid regions in the allocator.
            let batch: TransactionPtrBatch = unsafe {
                TransactionPtrBatch::from_sharable_transaction_batch_region(
                    &message.batch,
                    &self.allocator,
                )
            };

            let mut responses: ArrayVec<_, MAX_TRANSACTIONS_PER_MESSAGE> =
                core::iter::repeat_n(Self::initial_check_response(message.flags), batch.len())
                    .collect();

            let sanitize_config = sanitize_config();
            let mut status_transactions = ArrayVec::<_, MAX_TRANSACTIONS_PER_MESSAGE>::new();
            for (index, ((transaction_ptr, _), response)) in
                batch.iter().zip(responses.iter_mut()).enumerate()
            {
                let Ok(transaction) =
                    SanitizedTransactionView::try_new_sanitized(transaction_ptr, &sanitize_config)
                else {
                    response.parsing_and_sanitization_flags |=
                        parsing_and_sanitization_flags::FAILED;
                    continue;
                };

                if let Some(transaction) = self.check_transaction(
                    transaction,
                    response,
                    message.flags,
                    &root_bank,
                    &working_bank,
                )? && message.flags & check_message_flags::STATUS_CHECKS != 0
                {
                    status_transactions.push((index, transaction));
                }
            }

            if message.flags & check_message_flags::STATUS_CHECKS != 0 {
                Self::check_status_checks(&status_transactions, &mut responses, &working_bank);
            }

            let responses = resolve_responses_from_iter(&self.allocator, responses.into_iter())
                .ok_or(ExternalCheckWorkerError::AllocationFailure)?;
            self.sender
                .try_write(CheckWorkerToPackMessage {
                    batch: message.batch,
                    processed_code: processed_codes::PROCESSED,
                    responses,
                })
                .map_err(|_| ExternalCheckWorkerError::SenderDisconnected)?;

            Ok(())
        }

        fn check_transaction(
            &self,
            transaction: TxView,
            response: &mut CheckResponse,
            flags: u16,
            root_bank: &Bank,
            working_bank: &Bank,
        ) -> Result<Option<Tx>, ExternalCheckWorkerError> {
            if flags & check_message_flags::LOAD_FEE_PAYER_BALANCE != 0 {
                Self::check_load_fee_payer_balance(&transaction, response, working_bank);
            }
            if flags & check_message_flags::CALCULATE_SCHEDULING_DETAILS != 0 {
                response.scheduling_details_flags |= scheduling_details_flags::PERFORMED;
            }

            // Address loading does not depend on runtime metadata or fee calculation.
            let preloaded_addresses =
                if flags & check_message_flags::LOAD_ADDRESS_LOOKUP_TABLES != 0 {
                    response.resolve_flags |= resolve_flags::PERFORMED;
                    let Ok((addresses, deactivation_slot)) =
                        root_bank.load_addresses_for_view(&transaction)
                    else {
                        response.resolve_flags |= resolve_flags::FAILED;
                        if flags & check_message_flags::CALCULATE_SCHEDULING_DETAILS != 0 {
                            response.scheduling_details_flags |= scheduling_details_flags::FAILED;
                        }
                        return Ok(None);
                    };
                    self.export_resolved_pubkeys(
                        addresses.as_ref().unwrap_or(&LoadedAddresses::default()),
                        deactivation_slot,
                        response,
                        root_bank.slot(),
                    )?;
                    Some((addresses, deactivation_slot))
                } else {
                    None
                };

            if flags
                & (check_message_flags::CALCULATE_SCHEDULING_DETAILS
                    | check_message_flags::STATUS_CHECKS)
                == 0
            {
                return Ok(None);
            }
            let Ok((transaction, _)) = translate_sanitized_to_runtime_view(
                transaction,
                root_bank,
                root_bank.get_transaction_account_lock_limit(),
                preloaded_addresses,
            ) else {
                if flags & check_message_flags::CALCULATE_SCHEDULING_DETAILS != 0 {
                    response.scheduling_details_flags |= scheduling_details_flags::FAILED;
                }
                return Ok(None);
            };

            if flags & check_message_flags::CALCULATE_SCHEDULING_DETAILS != 0 {
                Self::check_scheduling_details(&transaction, response, working_bank);
            }

            // Status checks still apply when scheduling details failed.
            Ok(Some(transaction))
        }

        fn export_resolved_pubkeys(
            &self,
            addresses: &LoadedAddresses,
            alt_invalidation_slot: Slot,
            response: &mut CheckResponse,
            resolution_slot: Slot,
        ) -> Result<(), ExternalCheckWorkerError> {
            if addresses.is_empty() {
                response.min_alt_deactivation_slot = Slot::MAX;
            } else {
                let num_pubkeys = addresses.len();
                let allocation = self
                    .allocator
                    .allocate((num_pubkeys * core::mem::size_of::<Pubkey>()) as u32)
                    .ok_or(ExternalCheckWorkerError::AllocationFailure)?;
                // SAFETY: the fresh allocation is sized for all loaded addresses
                // and does not overlap with their source vectors.
                unsafe {
                    Self::copy_loaded_addresses(
                        addresses.writable.iter().chain(addresses.readonly.iter()),
                        allocation.cast(),
                    );
                }
                response.resolved_pubkeys = SharablePubkeys {
                    // SAFETY: allocation belongs to this allocator.
                    offset: unsafe { self.allocator.offset(allocation) },
                    num_pubkeys: num_pubkeys as u32,
                };
                response.min_alt_deactivation_slot = alt_invalidation_slot;
            }
            response.resolution_slot = resolution_slot;
            Ok(())
        }

        fn return_unprocessed_message(
            &mut self,
            message: &PackToCheckWorkerMessage,
            processed_code: u8,
        ) -> Result<(), ExternalCheckWorkerError> {
            assert_ne!(processed_code, processed_codes::PROCESSED);

            self.sender
                .try_write(CheckWorkerToPackMessage {
                    batch: message.batch,
                    processed_code,
                    responses: CheckResponseRegion {
                        num_transaction_responses: 0,
                        transaction_responses_offset: 0,
                    },
                })
                .map_err(|_| ExternalCheckWorkerError::SenderDisconnected)?;

            Ok(())
        }

        fn initial_check_response(flags: u16) -> CheckResponse {
            let initial_status_check_flags = if flags & check_message_flags::STATUS_CHECKS != 0 {
                status_check_flags::REQUESTED
            } else {
                0
            };
            let initial_fee_payer_balance_flags =
                if flags & check_message_flags::LOAD_FEE_PAYER_BALANCE != 0 {
                    fee_payer_balance_flags::REQUESTED
                } else {
                    0
                };
            let initial_resolve_flags =
                if flags & check_message_flags::LOAD_ADDRESS_LOOKUP_TABLES != 0 {
                    resolve_flags::REQUESTED
                } else {
                    0
                };
            let initial_scheduling_details_flags =
                if flags & check_message_flags::CALCULATE_SCHEDULING_DETAILS != 0 {
                    scheduling_details_flags::REQUESTED
                } else {
                    0
                };

            CheckResponse {
                parsing_and_sanitization_flags: 0,
                status_check_flags: initial_status_check_flags,
                fee_payer_balance_flags: initial_fee_payer_balance_flags,
                resolve_flags: initial_resolve_flags,
                scheduling_details_flags: initial_scheduling_details_flags,
                included_slot: 0,
                transaction_fee: 0,
                prioritization_fee: 0,
                estimated_cost_units: 0,
                allocated_accounts_data_size: 0,
                balance_slot: 0,
                fee_payer_balance: 0,
                resolution_slot: 0,
                min_alt_deactivation_slot: 0,
                resolved_pubkeys: SharablePubkeys {
                    offset: 0,
                    num_pubkeys: 0,
                },
            }
        }

        fn validate_message(message: &PackToCheckWorkerMessage) -> bool {
            message.batch.num_transactions > 0
                && usize::from(message.batch.num_transactions) <= MAX_TRANSACTIONS_PER_MESSAGE
                && Self::validate_message_flags(message.flags)
        }

        fn validate_message_flags(flags: u16) -> bool {
            const ALLOWED_CHECK_FLAGS: u16 = check_message_flags::STATUS_CHECKS
                | check_message_flags::LOAD_FEE_PAYER_BALANCE
                | check_message_flags::LOAD_ADDRESS_LOOKUP_TABLES
                | check_message_flags::CALCULATE_SCHEDULING_DETAILS;

            flags != 0 && flags & !ALLOWED_CHECK_FLAGS == 0
        }

        fn check_load_fee_payer_balance(
            transaction: &TxView,
            response: &mut CheckResponse,
            working_bank: &Bank,
        ) {
            response.fee_payer_balance_flags |= fee_payer_balance_flags::PERFORMED;
            response.fee_payer_balance = working_bank
                .get_account_with_fixed_root(transaction.fee_payer())
                .map(|account| account.lamports())
                .unwrap_or(0);
            response.balance_slot = working_bank.slot();
        }

        fn check_scheduling_details(
            transaction: &Tx,
            response: &mut CheckResponse,
            working_bank: &Bank,
        ) {
            let Ok(configuration) =
                transaction.transaction_configuration(&working_bank.feature_set)
            else {
                response.scheduling_details_flags |= scheduling_details_flags::FAILED;
                return;
            };

            let fee_details = solana_fee::calculate_fee_details(
                transaction,
                working_bank.fee_structure().lamports_per_signature,
                configuration.priority_fee_lamports,
                working_bank.fee_features(),
            );
            response.transaction_fee = fee_details.transaction_fee();
            response.prioritization_fee = fee_details.prioritization_fee();
            let cost = CostModel::calculate_cost_for_executed_transaction(
                transaction,
                u64::from(configuration.compute_unit_limit),
                configuration.loaded_accounts_data_size_limit,
                &working_bank.feature_set,
            );
            response.estimated_cost_units = cost.sum();
            response.allocated_accounts_data_size = cost.allocated_accounts_data_size();
        }

        fn check_status_checks(
            transactions: &[(usize, Tx)],
            responses: &mut [CheckResponse],
            working_bank: &Bank,
        ) {
            let txs: ArrayVec<_, MAX_TRANSACTIONS_PER_MESSAGE> = transactions
                .iter()
                .map(|(_, transaction)| transaction)
                .collect();
            let mut error_counters = TransactionErrorMetrics::default();
            let (status_check_results, included_slots) = working_bank
                .check_transactions_external::<Tx>(
                    &txs,
                    &[const { Ok(()) }; MAX_TRANSACTIONS_PER_MESSAGE][..txs.len()],
                    working_bank.max_processing_age(),
                    true,
                    &mut error_counters,
                );
            let included_slots = included_slots.expect("requested to collect processed slots");

            for (((response_index, _), status_check_result), included_slot) in transactions
                .iter()
                .zip(status_check_results)
                .zip(included_slots)
            {
                let check_response = &mut responses[*response_index];
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
                    Err(TransactionError::UnsupportedVersion) => {
                        check_response.status_check_flags |=
                            status_check_flags::UNSUPPORTED_VERSION;
                    }
                    _ => {}
                }
            }
        }

        /// # Safety
        /// - destination is appropriately sized
        /// - destination does not overlap with loaded_addresses allocation
        unsafe fn copy_loaded_addresses<'a>(
            loaded_addresses: impl Iterator<Item = &'a Pubkey>,
            dest: NonNull<Pubkey>,
        ) {
            for (index, pubkey) in loaded_addresses.enumerate() {
                unsafe { dest.add(index).write(*pubkey) };
            }
        }
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

    #[cfg(test)]
    mod tests {
        use {
            super::*,
            crate::banking_stage::tests::create_slow_genesis_config,
            agave_scheduler_bindings::{SharableTransactionBatchRegion, SharableTransactionRegion},
            agave_scheduler_handshake::{ClientLogon, setup_local_session},
            agave_scheduling_utils::{
                pubkeys_ptr::PubkeysPtr, responses_region::CheckResponsesPtr,
            },
            solana_account::AccountSharedData,
            solana_address_lookup_table_interface::{
                program,
                state::{AddressLookupTable, LookupTableMeta},
            },
            solana_compute_budget_interface::ComputeBudgetInstruction,
            solana_hash::Hash,
            solana_keypair::Keypair,
            solana_leader_schedule::SlotLeader,
            solana_ledger::genesis_utils::GenesisConfigInfo,
            solana_message::{AddressLookupTableAccount, Message, VersionedMessage, v0},
            solana_runtime::{bank::Bank, bank_forks::BankForks},
            solana_sdk_ids::system_program,
            solana_signer::Signer,
            solana_system_transaction::transfer,
            solana_transaction::{Transaction, versioned::VersionedTransaction},
            std::{
                sync::{Arc, RwLock},
                time::Duration,
            },
            test_case::test_case,
        };

        struct SharedBatch {
            region: SharableTransactionBatchRegion,
            transactions: Vec<SharableTransactionRegion>,
        }

        struct CheckWorkerTestFrame {
            bank: Arc<Bank>,
            _bank_forks: Arc<RwLock<BankForks>>,
            allocator: rts_alloc::Allocator,
            pack_to_check_worker: shaq::mpmc::Producer<PackToCheckWorkerMessage>,
            check_worker_to_pack: shaq::mpmc::Consumer<CheckWorkerToPackMessage>,
            worker: ExternalCheckWorker,
        }

        impl CheckWorkerTestFrame {
            fn send_message(&self, message: PackToCheckWorkerMessage) {
                self.pack_to_check_worker.try_write(message).unwrap();
            }

            fn iterate(&mut self) -> Result<(), ExternalCheckWorkerError> {
                let result = self.worker.iterate(Duration::ZERO)?;
                assert!(matches!(result, IterationResult::ProcessedMessage));
                Ok(())
            }

            fn iterate_idle(&mut self) -> Result<(), ExternalCheckWorkerError> {
                let result = self.worker.iterate(Duration::ZERO)?;
                assert!(matches!(result, IterationResult::Idle));
                Ok(())
            }

            fn recv_response(&self) -> CheckWorkerToPackMessage {
                self.check_worker_to_pack
                    .read_timeout(Duration::from_secs(1))
                    .unwrap()
            }

            fn check_responses(&self, region: &CheckResponseRegion) -> Vec<CheckResponse> {
                unsafe {
                    // SAFETY: `region` was produced by this worker using the same shared allocator,
                    // and the pointed-to allocation contains `CheckResponse` values.
                    let responses = CheckResponsesPtr::from_transaction_response_region(
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
                // SAFETY: `batch_ptr` came from this allocator immediately above, so translating it
                // back to an offset in the same allocator is valid.
                let batch_offset = unsafe { self.allocator.offset(batch_ptr) };
                let tx_ptr = batch_ptr.cast::<SharableTransactionRegion>();

                let mut sharable_transactions = Vec::with_capacity(transactions.len());
                for (index, transaction) in transactions.iter().enumerate() {
                    let tx_allocation = self
                        .allocator
                        .allocate(transaction.len().try_into().unwrap())
                        .unwrap();
                    unsafe {
                        // SAFETY: `tx_allocation` points to a fresh allocation of exactly
                        // `transaction.len()` bytes, and `transaction.as_ptr()` is readable for that
                        // same length. The regions do not overlap.
                        std::ptr::copy_nonoverlapping(
                            transaction.as_ptr(),
                            tx_allocation.as_ptr(),
                            transaction.len(),
                        );
                    }
                    let tx_region = SharableTransactionRegion {
                        // SAFETY: `tx_allocation` came from this allocator immediately above, so
                        // translating it back to an offset in the same allocator is valid.
                        offset: unsafe { self.allocator.offset(tx_allocation) },
                        length: transaction.len().try_into().unwrap(),
                    };
                    unsafe {
                        // SAFETY: the batch allocation is sized for
                        // `TransactionPtrBatch::TRANSACTION_META_END`, which includes space for up to
                        // `MAX_TRANSACTIONS_PER_MESSAGE` transaction headers, and the assert above
                        // guarantees `index` is in-bounds.
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

        fn setup_check_worker_test_frame() -> CheckWorkerTestFrame {
            let GenesisConfigInfo { genesis_config, .. } = create_slow_genesis_config(10_000);
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
            let (_agave_session, client_session) = setup_local_session(logon).unwrap();
            let allocator = client_session.allocator;

            let (pack_to_check_worker, receiver) = shaq::mpmc::pair(16).unwrap();
            let (check_worker_to_pack, response_receiver) = shaq::mpmc::pair(16).unwrap();
            let worker_allocator = rts_alloc::Allocator::join_from_existing(&allocator)
                .expect("join allocator from test allocator");
            let shared_leader_state = SharedLeaderState::new(0, None, None);
            let worker = ExternalCheckWorker::new(
                Arc::new(AtomicBool::new(false)),
                receiver,
                check_worker_to_pack,
                worker_allocator,
                shared_leader_state,
                bank_forks.read().unwrap().sharable_banks(),
            );

            CheckWorkerTestFrame {
                bank,
                _bank_forks: bank_forks,
                allocator,
                pack_to_check_worker,
                check_worker_to_pack: response_receiver,
                worker,
            }
        }

        fn test_serialized_transaction(recent_blockhash: solana_hash::Hash) -> Vec<u8> {
            wincode::serialize(&transfer(
                &Keypair::new(),
                &Pubkey::new_unique(),
                1,
                recent_blockhash,
            ))
            .unwrap()
        }

        #[test]
        fn test_idle_timeout() {
            let mut test_frame = setup_check_worker_test_frame();
            test_frame.iterate_idle().unwrap();
        }

        #[test]
        fn test_invalid_message() {
            let mut test_frame = setup_check_worker_test_frame();

            test_frame.send_message(PackToCheckWorkerMessage {
                flags: check_message_flags::STATUS_CHECKS,
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
            test_frame.send_message(PackToCheckWorkerMessage {
                flags: u16::MAX,
                batch: batch.region,
            });
            test_frame.iterate().unwrap();
            let response = test_frame.recv_response();
            assert_eq!(response.processed_code, processed_codes::INVALID);
            assert_eq!(response.responses.num_transaction_responses, 0);
            assert_eq!(response.responses.transaction_responses_offset, 0);

            test_frame.send_message(PackToCheckWorkerMessage {
                flags: 0,
                batch: batch.region,
            });
            test_frame.iterate().unwrap();
            let response = test_frame.recv_response();
            assert_eq!(response.processed_code, processed_codes::INVALID);
            assert_eq!(response.responses.num_transaction_responses, 0);
            assert_eq!(response.responses.transaction_responses_offset, 0);

            test_frame.free_batch(batch);
        }

        #[test_case(check_message_flags::LOAD_FEE_PAYER_BALANCE; "balance_only")]
        #[test_case(check_message_flags::STATUS_CHECKS; "status_only")]
        #[test_case(check_message_flags::STATUS_CHECKS | check_message_flags::LOAD_FEE_PAYER_BALANCE; "both")]
        fn test_happy_path(flags: u16) {
            let mut test_frame = setup_check_worker_test_frame();
            let fee_payer = Keypair::new();
            let fee_payer_balance = 123_456;
            test_frame.bank.store_account(
                &fee_payer.pubkey(),
                &AccountSharedData::new(fee_payer_balance, 0, &system_program::ID),
            );

            let batch = test_frame.allocate_batch(&[wincode::serialize(&transfer(
                &fee_payer,
                &Pubkey::new_unique(),
                1,
                test_frame.bank.confirmed_last_blockhash(),
            ))
            .unwrap()]);
            test_frame.send_message(PackToCheckWorkerMessage {
                flags,
                batch: batch.region,
            });
            test_frame.iterate().unwrap();
            let response = test_frame.recv_response();
            assert_eq!(response.processed_code, processed_codes::PROCESSED);
            let responses = test_frame.check_responses(&response.responses);
            assert_eq!(responses.len(), 1);
            if flags & check_message_flags::STATUS_CHECKS != 0 {
                assert_eq!(
                    responses[0].status_check_flags,
                    status_check_flags::REQUESTED | status_check_flags::PERFORMED
                );
            } else {
                assert_eq!(responses[0].status_check_flags, 0);
            }
            if flags & check_message_flags::LOAD_FEE_PAYER_BALANCE != 0 {
                assert_eq!(
                    responses[0].fee_payer_balance_flags,
                    fee_payer_balance_flags::REQUESTED | fee_payer_balance_flags::PERFORMED
                );
                assert_eq!(responses[0].balance_slot, test_frame.bank.slot());
                assert_eq!(responses[0].fee_payer_balance, fee_payer_balance);
            } else {
                assert_eq!(responses[0].fee_payer_balance_flags, 0);
            }
            assert_eq!(responses[0].scheduling_details_flags, 0);

            test_frame.free_batch(batch);
        }

        #[test_case(false; "translation_failure")]
        #[test_case(true; "scheduling_failure_keeps_resolution")]
        fn test_mixed_batch(check_scheduling: bool) {
            let mut frame = setup_check_worker_test_frame();
            let payer = Keypair::new();
            frame.bank.store_account(
                &payer.pubkey(),
                &AccountSharedData::new(123_456, 0, &system_program::ID),
            );
            // Duplicate compute-budget instructions pass parsing but fail translation.
            let translation_failure = Transaction::new(
                &[&payer],
                Message::new(
                    &[
                        ComputeBudgetInstruction::set_compute_unit_limit(1),
                        ComputeBudgetInstruction::set_compute_unit_limit(1),
                    ],
                    Some(&payer.pubkey()),
                ),
                frame.bank.confirmed_last_blockhash(),
            );
            let batch = frame.allocate_batch(&[
                vec![0], // Malformed transaction.
                wincode::serialize(&translation_failure).unwrap(),
                test_serialized_transaction(Hash::new_unique()), // Unknown blockhash.
            ]);
            let mut flags = check_message_flags::STATUS_CHECKS
                | check_message_flags::LOAD_FEE_PAYER_BALANCE
                | check_message_flags::LOAD_ADDRESS_LOOKUP_TABLES;
            if check_scheduling {
                flags |= check_message_flags::CALCULATE_SCHEDULING_DETAILS;
            }
            frame.send_message(PackToCheckWorkerMessage {
                flags,
                batch: batch.region,
            });
            frame.iterate().unwrap();
            let response = frame.recv_response();
            let responses = frame.check_responses(&response.responses);
            frame.free_batch(batch);
            let [malformed, untranslated, expired] = responses.as_slice() else {
                panic!("expected one response per input transaction");
            };

            assert_eq!(
                malformed.parsing_and_sanitization_flags,
                parsing_and_sanitization_flags::FAILED
            );
            assert_eq!(
                malformed.fee_payer_balance_flags,
                fee_payer_balance_flags::REQUESTED
            );
            assert_eq!(malformed.resolve_flags, resolve_flags::REQUESTED);
            assert_eq!(malformed.status_check_flags, status_check_flags::REQUESTED);

            assert_eq!(untranslated.parsing_and_sanitization_flags, 0);
            assert_eq!(
                untranslated.fee_payer_balance_flags,
                fee_payer_balance_flags::REQUESTED | fee_payer_balance_flags::PERFORMED
            );
            assert_eq!(untranslated.fee_payer_balance, 123_456);
            assert_eq!(
                untranslated.status_check_flags,
                status_check_flags::REQUESTED
            );
            if check_scheduling {
                assert_eq!(
                    untranslated.scheduling_details_flags,
                    scheduling_details_flags::REQUESTED
                        | scheduling_details_flags::PERFORMED
                        | scheduling_details_flags::FAILED
                );
            } else {
                assert_eq!(untranslated.scheduling_details_flags, 0);
            }
            assert_eq!(
                untranslated.resolve_flags,
                resolve_flags::REQUESTED | resolve_flags::PERFORMED
            );

            // The successful translation after both failures gets its own status result.
            assert_eq!(expired.parsing_and_sanitization_flags, 0);
            assert_eq!(
                expired.resolve_flags,
                resolve_flags::REQUESTED | resolve_flags::PERFORMED
            );
            assert_eq!(
                expired.status_check_flags,
                status_check_flags::REQUESTED
                    | status_check_flags::PERFORMED
                    | status_check_flags::TOO_OLD
            );
        }

        #[test]
        fn test_scheduling_details() {
            let mut test_frame = setup_check_worker_test_frame();
            let fee_payer = Keypair::new();
            let allocated_account = Keypair::new();
            let allocated_accounts_data_size = 1_234;
            let transaction = Transaction::new(
                &[&fee_payer, &allocated_account],
                Message::new(
                    &[
                        solana_system_interface::instruction::create_account(
                            &fee_payer.pubkey(),
                            &allocated_account.pubkey(),
                            1,
                            allocated_accounts_data_size,
                            &Pubkey::new_unique(),
                        ),
                        ComputeBudgetInstruction::set_compute_unit_price(1_000_000),
                    ],
                    Some(&fee_payer.pubkey()),
                ),
                test_frame.bank.confirmed_last_blockhash(),
            );
            let batch = test_frame.allocate_batch(&[wincode::serialize(&transaction).unwrap()]);

            test_frame.send_message(PackToCheckWorkerMessage {
                flags: check_message_flags::CALCULATE_SCHEDULING_DETAILS,
                batch: batch.region,
            });
            test_frame.iterate().unwrap();
            let response = test_frame.recv_response();
            let responses = test_frame.check_responses(&response.responses);

            assert_eq!(responses.len(), 1);
            assert_eq!(
                responses[0].scheduling_details_flags,
                scheduling_details_flags::REQUESTED | scheduling_details_flags::PERFORMED
            );
            assert_eq!(
                responses[0].transaction_fee,
                2 * test_frame.bank.fee_structure().lamports_per_signature
            );
            assert!(responses[0].prioritization_fee > 0);
            assert!(responses[0].estimated_cost_units > 0);
            assert_eq!(
                responses[0].allocated_accounts_data_size,
                allocated_accounts_data_size
            );

            test_frame.free_batch(batch);
        }

        #[test]
        fn test_scheduling_details_failure_keeps_pubkey_resolution() {
            let mut test_frame = setup_check_worker_test_frame();
            let fee_payer = Keypair::new();
            let blockhash = test_frame.bank.confirmed_last_blockhash();
            let recipients = [Pubkey::new_unique(), Pubkey::new_unique()];
            let table_key = Pubkey::new_unique();
            let table_data = AddressLookupTable {
                meta: LookupTableMeta::default(),
                addresses: recipients.to_vec().into(),
            }
            .serialize_for_tests()
            .unwrap();
            let mut table_account = AccountSharedData::new(1, table_data.len(), &program::id());
            table_account.set_data_from_slice(&table_data);
            test_frame.bank.store_account(&table_key, &table_account);
            let make_transaction = |recipient, data_limit| {
                VersionedTransaction::try_new(
                    VersionedMessage::V0(
                        v0::Message::try_compile(
                            &fee_payer.pubkey(),
                            &[
                                ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                                    data_limit,
                                ),
                                solana_system_interface::instruction::transfer(
                                    &fee_payer.pubkey(),
                                    &recipient,
                                    1,
                                ),
                            ],
                            &[AddressLookupTableAccount {
                                key: table_key,
                                addresses: recipients.to_vec(),
                            }],
                            blockhash,
                        )
                        .unwrap(),
                    ),
                    &[&fee_payer],
                )
                .unwrap()
            };
            let transaction = make_transaction(recipients[0], 0); // Scheduling failure.
            let next_transaction = make_transaction(recipients[1], 1024);
            let batch = test_frame.allocate_batch(&[
                wincode::serialize(&transaction).unwrap(),
                wincode::serialize(&next_transaction).unwrap(),
            ]);

            test_frame.send_message(PackToCheckWorkerMessage {
                flags: check_message_flags::CALCULATE_SCHEDULING_DETAILS
                    | check_message_flags::LOAD_ADDRESS_LOOKUP_TABLES
                    | check_message_flags::STATUS_CHECKS,
                batch: batch.region,
            });
            test_frame.iterate().unwrap();
            let response = test_frame.recv_response();
            let responses = test_frame.check_responses(&response.responses);
            test_frame.free_batch(batch);

            assert_eq!(responses.len(), 2);
            assert_eq!(
                responses[0].scheduling_details_flags,
                scheduling_details_flags::REQUESTED
                    | scheduling_details_flags::PERFORMED
                    | scheduling_details_flags::FAILED
            );
            assert_eq!(
                responses[0].status_check_flags,
                status_check_flags::REQUESTED | status_check_flags::PERFORMED
            );
            assert_eq!(
                responses[0].resolve_flags,
                resolve_flags::REQUESTED | resolve_flags::PERFORMED
            );

            assert_eq!(
                responses[1].scheduling_details_flags,
                scheduling_details_flags::REQUESTED | scheduling_details_flags::PERFORMED
            );
            for (response, recipient) in responses.iter().zip(recipients) {
                assert_eq!(response.resolved_pubkeys.num_pubkeys, 1);
                // SAFETY: each response exclusively owns its pubkey allocation.
                unsafe {
                    let keys = PubkeysPtr::from_sharable_pubkeys(
                        &response.resolved_pubkeys,
                        &test_frame.allocator,
                    );
                    assert_eq!(keys.as_slice(), &[recipient]);
                    keys.free(&test_frame.allocator);
                }
            }
        }

        #[test]
        fn test_missing_lookup_table_still_loads_balance() {
            let mut frame = setup_check_worker_test_frame();
            let payer = Keypair::new();
            let recipient = Pubkey::new_unique();
            let transaction = VersionedTransaction::try_new(
                VersionedMessage::V0(
                    v0::Message::try_compile(
                        &payer.pubkey(),
                        &[solana_system_interface::instruction::transfer(
                            &payer.pubkey(),
                            &recipient,
                            1,
                        )],
                        &[AddressLookupTableAccount {
                            key: Pubkey::new_unique(), // No table stored in the bank.
                            addresses: vec![recipient],
                        }],
                        frame.bank.confirmed_last_blockhash(),
                    )
                    .unwrap(),
                ),
                &[&payer],
            )
            .unwrap();
            let batch = frame.allocate_batch(&[wincode::serialize(&transaction).unwrap()]);
            frame.send_message(PackToCheckWorkerMessage {
                flags: check_message_flags::LOAD_FEE_PAYER_BALANCE
                    | check_message_flags::LOAD_ADDRESS_LOOKUP_TABLES
                    | check_message_flags::CALCULATE_SCHEDULING_DETAILS
                    | check_message_flags::STATUS_CHECKS,
                batch: batch.region,
            });
            frame.iterate().unwrap();
            let message = frame.recv_response();
            let responses = frame.check_responses(&message.responses);
            frame.free_batch(batch);
            let response = &responses[0];

            assert_eq!(
                response.fee_payer_balance_flags,
                fee_payer_balance_flags::REQUESTED | fee_payer_balance_flags::PERFORMED
            );
            assert_eq!(response.fee_payer_balance, 0); // Missing payer account.
            assert_eq!(
                response.resolve_flags,
                resolve_flags::REQUESTED | resolve_flags::PERFORMED | resolve_flags::FAILED
            );
            assert_eq!(
                response.scheduling_details_flags,
                scheduling_details_flags::REQUESTED
                    | scheduling_details_flags::PERFORMED
                    | scheduling_details_flags::FAILED
            );
            assert_eq!(response.status_check_flags, status_check_flags::REQUESTED);
        }

        #[test]
        fn test_resolve_without_loaded_addresses() {
            let mut test_frame = setup_check_worker_test_frame();
            let batch = test_frame.allocate_batch(&[test_serialized_transaction(
                test_frame.bank.confirmed_last_blockhash(),
            )]);

            test_frame.send_message(PackToCheckWorkerMessage {
                flags: check_message_flags::LOAD_ADDRESS_LOOKUP_TABLES,
                batch: batch.region,
            });
            test_frame.iterate().unwrap();
            let response = test_frame.recv_response();
            assert_eq!(response.processed_code, processed_codes::PROCESSED);
            let responses = test_frame.check_responses(&response.responses);
            assert_eq!(responses.len(), 1);
            assert_eq!(
                responses[0].resolve_flags,
                resolve_flags::REQUESTED | resolve_flags::PERFORMED
            );
            assert_eq!(responses[0].resolution_slot, test_frame.bank.slot());
            assert_eq!(responses[0].resolved_pubkeys.num_pubkeys, 0);
            assert_eq!(responses[0].min_alt_deactivation_slot, u64::MAX);

            test_frame.free_batch(batch);
        }
    }
}
