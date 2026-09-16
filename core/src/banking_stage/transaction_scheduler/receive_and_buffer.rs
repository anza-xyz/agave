use {
    super::{
        transaction_state::TransactionState,
        transaction_state_container::{
            StateContainer, TransactionViewState, TransactionViewStateContainer,
        },
    },
    crate::{
        banking_stage::{
            consumer::Consumer, decision_maker::BufferedPacketsDecision, packet_bytes,
            scheduler_messages::MaxAge,
        },
        transaction_priority::calculate_priority_and_cost,
    },
    agave_banking_stage_ingress_types::{BankingPacketBatch, BankingPacketReceiver},
    agave_transaction_view::{
        resolved_transaction_view::ResolvedTransactionView, sanitize::SanitizeConfig,
        transaction_data::TransactionData, transaction_version::TransactionVersion,
        transaction_view::SanitizedTransactionView,
    },
    core::time::Duration,
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError, TrySendError},
    solana_accounts_db::account_locks::validate_account_locks,
    solana_address_lookup_table_interface::state::estimate_last_valid_slot,
    solana_clock::{Epoch, Slot},
    solana_message::v0::LoadedAddresses,
    solana_perf::packet::bytes::Bytes,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_runtime_transaction::{
        runtime_transaction::RuntimeTransaction, sanitize_config::sanitize_config,
        transaction_meta::TransactionMeta, transaction_with_meta::TransactionWithMeta,
    },
    solana_svm::transaction_error_metrics::TransactionErrorMetrics,
    solana_svm_transaction::svm_message::SVMMessage,
    solana_transaction::sanitized::MessageHash,
    solana_transaction_error::TransactionError,
    std::{collections::HashSet, time::Instant},
};

#[derive(Debug)]
pub(crate) enum IngressCheckError {
    PacketHandling(PacketHandlingError),
    Transaction(TransactionError),
    FeePayer,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PacketHandlingError {
    Sanitization,
    LockValidation,
    ComputeBudget,
    ALTResolution,
    FilterKey,
}

pub(crate) struct PrecheckedTransaction {
    pub(crate) state: TransactionViewState,
    pub(crate) validated_nonce_address: Option<Pubkey>,
}

pub(crate) type PrecheckResult = Result<PrecheckedTransaction, IngressCheckError>;

pub(crate) fn precheck_transaction(
    bytes: Bytes,
    root_bank: &Bank,
    working_bank: &Bank,
    filter_keys: &HashSet<Pubkey>,
) -> PrecheckResult {
    let sanitize_config = sanitize_config();
    let transaction_account_lock_limit = working_bank.get_transaction_account_lock_limit();
    let (view, deactivation_slot) = translate_to_runtime_view(
        bytes,
        root_bank,
        transaction_account_lock_limit,
        &sanitize_config,
    )
    .map_err(IngressCheckError::PacketHandling)?;

    if !filter_keys.is_empty()
        && view
            .account_keys()
            .iter()
            .any(|key| filter_keys.contains(key))
    {
        return Err(IngressCheckError::PacketHandling(
            PacketHandlingError::FilterKey,
        ));
    }

    let transaction_configuration = view
        .transaction_configuration(&working_bank.feature_set)
        .map_err(|_| IngressCheckError::PacketHandling(PacketHandlingError::ComputeBudget))?;
    let max_age = calculate_max_age(root_bank.epoch(), deactivation_slot, root_bank.slot());
    let (priority, cost) =
        calculate_priority_and_cost(working_bank, &view, &transaction_configuration);
    let state = TransactionState::new(view, max_age, priority, cost);

    let mut error_counters = TransactionErrorMetrics::default();
    let validated_nonce_address = working_bank
        .check_transaction_without_status_cache(
            state.transaction(),
            working_bank.max_processing_age(),
            &mut error_counters,
        )
        .map_err(IngressCheckError::Transaction)?;

    Consumer::check_fee_payer_unlocked(working_bank, state.transaction(), &mut error_counters)
        .map_err(|_| IngressCheckError::FeePayer)?;

    Ok(PrecheckedTransaction {
        state,
        validated_nonce_address,
    })
}

/// Perform sanitization checks and transition from data to an executable
/// [`RuntimeTransaction`]. This additionally returns the minimum slot for
/// ALT deactivation, if any. If no minimum slot, Slot::MAX is returned.
pub(crate) fn translate_to_runtime_view<D: TransactionData>(
    data: D,
    bank: &Bank,
    transaction_account_lock_limit: usize,
    sanitize_config: &SanitizeConfig,
) -> Result<(RuntimeTransaction<ResolvedTransactionView<D>>, u64), PacketHandlingError> {
    let Ok(view) = SanitizedTransactionView::try_new_sanitized(data, sanitize_config) else {
        return Err(PacketHandlingError::Sanitization);
    };

    let Ok(view) = RuntimeTransaction::<SanitizedTransactionView<_>>::try_new(
        view,
        MessageHash::Compute,
        None,
    ) else {
        return Err(PacketHandlingError::Sanitization);
    };

    if bank.vote_only_bank() && !view.is_simple_vote_transaction() {
        return Err(PacketHandlingError::Sanitization);
    }

    if usize::from(view.total_num_accounts()) > transaction_account_lock_limit {
        return Err(PacketHandlingError::LockValidation);
    }

    let (loaded_addresses, deactivation_slot) = load_addresses_for_view(&view, bank)?;

    let Ok(view) = RuntimeTransaction::<ResolvedTransactionView<_>>::try_new(
        view,
        loaded_addresses,
        bank.get_reserved_account_keys(),
    ) else {
        return Err(PacketHandlingError::Sanitization);
    };

    if validate_account_locks(view.account_keys(), transaction_account_lock_limit).is_err() {
        return Err(PacketHandlingError::LockValidation);
    }

    Ok((view, deactivation_slot))
}

fn load_addresses_for_view<D: TransactionData>(
    view: &SanitizedTransactionView<D>,
    bank: &Bank,
) -> Result<(Option<LoadedAddresses>, Slot), PacketHandlingError> {
    match view.version() {
        TransactionVersion::Legacy | TransactionVersion::V1 => Ok((None, u64::MAX)),
        TransactionVersion::V0 => bank
            .load_addresses_from_ref(view.address_table_lookup_iter())
            .map(|(loaded_addresses, deactivation_slot)| {
                (Some(loaded_addresses), deactivation_slot)
            })
            .map_err(|_| PacketHandlingError::ALTResolution),
    }
}

fn calculate_max_age(
    sanitized_epoch: Epoch,
    deactivation_slot: Slot,
    current_slot: Slot,
) -> MaxAge {
    let alt_min_expire_slot = estimate_last_valid_slot(deactivation_slot.min(current_slot));
    MaxAge {
        sanitized_epoch,
        alt_invalidation_slot: alt_min_expire_slot,
    }
}

#[derive(Debug)]
pub(crate) enum DisconnectedError {
    Receiver,
    CheckWorker,
}

/// Stats/metrics returned by `receive_and_buffer_packets`.
#[derive(Debug, Default)]
pub(crate) struct ReceivingStats {
    pub num_received: usize,
    /// Count of packets that passed sigverify but were dropped
    /// without further checks because we were outside the holding
    /// window.
    pub num_dropped_without_parsing: usize,

    pub num_dropped_on_parsing_and_sanitization: usize,
    pub num_dropped_on_lock_validation: usize,
    pub num_dropped_on_compute_budget: usize,
    pub num_dropped_on_age: usize,
    pub num_dropped_on_already_processed: usize,
    pub num_dropped_on_fee_payer: usize,
    pub num_dropped_on_filter_key: usize,
    pub num_dropped_on_check_work_queue_full: usize,
    pub num_dropped_on_capacity: usize,
    pub num_dropped_on_nonce_dedup: usize,

    pub num_buffered: usize,
    pub num_evicted_on_nonce_dedup: usize,

    pub receive_time_us: u64,
    pub buffer_time_us: u64,
}

impl ReceivingStats {
    fn add_ingress_check_error(&mut self, err: &IngressCheckError) {
        match err {
            IngressCheckError::PacketHandling(err) => self.add_packet_handling_error(err),
            IngressCheckError::Transaction(err) => self.add_transaction_error(err),
            IngressCheckError::FeePayer => self.num_dropped_on_fee_payer += 1,
        }
    }

    fn add_packet_handling_error(&mut self, err: &PacketHandlingError) {
        match err {
            PacketHandlingError::Sanitization | PacketHandlingError::ALTResolution => {
                self.num_dropped_on_parsing_and_sanitization += 1;
            }
            PacketHandlingError::LockValidation => {
                self.num_dropped_on_lock_validation += 1;
            }
            PacketHandlingError::ComputeBudget => {
                self.num_dropped_on_compute_budget += 1;
            }
            PacketHandlingError::FilterKey => {
                self.num_dropped_on_filter_key += 1;
            }
        }
    }

    fn add_transaction_error(&mut self, err: &TransactionError) {
        match err {
            TransactionError::BlockhashNotFound => {
                self.num_dropped_on_age += 1;
            }
            TransactionError::AlreadyProcessed => {
                self.num_dropped_on_already_processed += 1;
            }
            _ => {}
        }
    }

    pub(crate) fn accumulate(&mut self, other: ReceivingStats) {
        self.num_received += other.num_received;
        self.num_dropped_without_parsing += other.num_dropped_without_parsing;
        self.num_dropped_on_parsing_and_sanitization +=
            other.num_dropped_on_parsing_and_sanitization;
        self.num_dropped_on_lock_validation += other.num_dropped_on_lock_validation;
        self.num_dropped_on_compute_budget += other.num_dropped_on_compute_budget;
        self.num_dropped_on_age += other.num_dropped_on_age;
        self.num_dropped_on_already_processed += other.num_dropped_on_already_processed;
        self.num_dropped_on_fee_payer += other.num_dropped_on_fee_payer;
        self.num_dropped_on_filter_key += other.num_dropped_on_filter_key;
        self.num_dropped_on_check_work_queue_full += other.num_dropped_on_check_work_queue_full;
        self.num_dropped_on_capacity += other.num_dropped_on_capacity;
        self.num_dropped_on_nonce_dedup += other.num_dropped_on_nonce_dedup;
        self.num_buffered += other.num_buffered;
        self.num_evicted_on_nonce_dedup += other.num_evicted_on_nonce_dedup;
        self.receive_time_us += other.receive_time_us;
        self.buffer_time_us += other.buffer_time_us;
    }
}

pub(crate) trait ReceiveAndBuffer {
    type Transaction: TransactionWithMeta + Send + Sync;
    type Container: StateContainer<Self::Transaction> + Send + Sync;

    /// Return Err if the receiver is disconnected AND no packets were
    /// received. Otherwise return Ok(num_received).
    fn receive_and_buffer_packets(
        &mut self,
        container: &mut Self::Container,
        decision: &BufferedPacketsDecision,
    ) -> Result<ReceivingStats, DisconnectedError>;

    fn drain_check_results(
        &mut self,
        container: &mut Self::Container,
        decision: &BufferedPacketsDecision,
    ) -> ReceivingStats;
}

pub(crate) struct TransactionViewReceiveAndBuffer {
    receiver: BankingPacketReceiver,
    check_work_sender: Sender<Bytes>,
    check_result_receiver: Receiver<PrecheckResult>,
}

impl TransactionViewReceiveAndBuffer {
    pub(crate) fn new(
        receiver: BankingPacketReceiver,
        check_work_sender: Sender<Bytes>,
        check_result_receiver: Receiver<PrecheckResult>,
    ) -> Self {
        Self {
            receiver,
            check_work_sender,
            check_result_receiver,
        }
    }
}

impl ReceiveAndBuffer for TransactionViewReceiveAndBuffer {
    type Transaction = RuntimeTransaction<ResolvedTransactionView<Bytes>>;
    type Container = TransactionViewStateContainer;

    fn receive_and_buffer_packets(
        &mut self,
        container: &mut Self::Container,
        decision: &BufferedPacketsDecision,
    ) -> Result<ReceivingStats, DisconnectedError> {
        const RECV_TIMEOUT: Duration = Duration::from_millis(10);
        const MAX_PACKET_RECEIVES: usize = 512;

        let mut received_message = false;
        let mut stats = ReceivingStats::default();
        let should_check = !matches!(decision, BufferedPacketsDecision::Forward);

        // If not leader/unknown, do a blocking-receive initially. This lets
        // the thread sleep until a message is received, or until the timeout.
        // Do not pop from ingress when the check-work queue is full.
        let mut timed_out = false;
        let should_block = match decision {
            BufferedPacketsDecision::Forward => true,
            BufferedPacketsDecision::ForwardAndHold => self.check_capacity() > 0,
            BufferedPacketsDecision::Consume(_) | BufferedPacketsDecision::Hold => false,
        };
        if container.is_empty() && should_block {
            // TODO: Is it better to manually sleep instead, avoiding the locking
            //       overhead for wakers? But then risk not waking up when message
            //       received - as long as sleep is somewhat short, this should be
            //       fine.
            match self.receiver.recv_timeout(RECV_TIMEOUT) {
                Ok(batch) => {
                    received_message = true;
                    self.process_packet_batch(batch, should_check, &mut stats)?;
                }
                Err(RecvTimeoutError::Timeout) => timed_out = true,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(DisconnectedError::Receiver);
                }
            }
        }

        if !timed_out {
            // This is a soft limit: finish the current batch before checking it again.
            while stats.num_received < MAX_PACKET_RECEIVES {
                if should_check && self.check_capacity() == 0 {
                    break;
                }

                let receive_start = Instant::now();
                match self.receiver.try_recv() {
                    Ok(batch) => {
                        stats.receive_time_us += receive_start.elapsed().as_micros() as u64;
                        received_message = true;
                        self.process_packet_batch(batch, should_check, &mut stats)?;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        if !received_message && stats.num_received == 0 {
                            return Err(DisconnectedError::Receiver);
                        }
                        break;
                    }
                }
            }
        }

        Ok(stats)
    }

    fn drain_check_results(
        &mut self,
        container: &mut Self::Container,
        decision: &BufferedPacketsDecision,
    ) -> ReceivingStats {
        const MAX_CHECK_RESULTS: usize = 1024;

        let start = Instant::now();
        let mut stats = ReceivingStats::default();
        let should_buffer = !matches!(decision, BufferedPacketsDecision::Forward);

        for _ in 0..MAX_CHECK_RESULTS {
            let Ok(result) = self.check_result_receiver.try_recv() else {
                break;
            };
            if !should_buffer {
                stats.num_dropped_without_parsing += 1;
                continue;
            }

            match result {
                Ok(transaction) => {
                    Self::admit_prechecked_transaction(transaction, container, &mut stats)
                }
                Err(err) => stats.add_ingress_check_error(&err),
            }
        }

        stats.buffer_time_us = start.elapsed().as_micros() as u64;
        stats
    }
}

impl TransactionViewReceiveAndBuffer {
    fn check_capacity(&self) -> usize {
        self.check_work_sender
            .capacity()
            .expect("check-work queue must be bounded")
            .saturating_sub(self.check_work_sender.len())
    }

    fn process_packet_batch(
        &self,
        batch: BankingPacketBatch,
        should_check: bool,
        stats: &mut ReceivingStats,
    ) -> Result<(), DisconnectedError> {
        for packet in batch.iter() {
            let Some(packet_data) = packet.data(..) else {
                continue;
            };

            stats.num_received += 1;
            if !should_check {
                stats.num_dropped_without_parsing += 1;
                continue;
            }

            let work = packet_bytes(packet, packet_data);
            match self.check_work_sender.try_send(work) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    stats.num_dropped_on_check_work_queue_full += 1;
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err(DisconnectedError::CheckWorker);
                }
            }
        }

        Ok(())
    }

    fn admit_prechecked_transaction(
        prechecked_transaction: PrecheckedTransaction,
        container: &mut TransactionViewStateContainer,
        stats: &mut ReceivingStats,
    ) {
        let PrecheckedTransaction {
            state,
            validated_nonce_address,
        } = prechecked_transaction;
        let priority = state.priority();

        // Reject a nonce-like transaction if a higher-or-equal-priority conflict
        // is queued, or any conflict is already in flight.
        let nonce_priority = state
            .transaction()
            .get_durable_nonce()
            .map(|address| (*address, priority));
        if Self::should_drop_nonce(nonce_priority, container) {
            stats.num_dropped_on_nonce_dedup += 1;
            return;
        }

        let priority_id = container.insert_map_only(state);

        // A validated nonce transaction is higher priority than any queued
        // conflict. Evict that conflict only after all ingress checks passed.
        if let Some(nonce_address) = validated_nonce_address {
            if let Some(existing_nonce_priority_id) =
                container.get_nonce_transaction_priority_id(&nonce_address)
            {
                stats.num_evicted_on_nonce_dedup += 1;
                container.remove_by_id(existing_nonce_priority_id.id);
            }
            container.set_nonce_transaction_priority_id(&nonce_address, priority_id);
        }

        stats.num_dropped_on_capacity +=
            container.push_ids_into_queue(std::iter::once(priority_id));
        stats.num_buffered += 1;
    }

    fn should_drop_nonce(
        nonce_priority: Option<(Pubkey, u64)>,
        container: &TransactionViewStateContainer,
    ) -> bool {
        nonce_priority.is_some_and(|(address, priority)| {
            container
                .get_nonce_transaction_priority_id(&address)
                .is_some_and(|existing| {
                    existing.priority >= priority || !container.is_queued(existing)
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::banking_stage::{
            tests::create_slow_genesis_config,
            transaction_scheduler::{
                check_worker::spawn_check_workers,
                transaction_state_container::RuntimeTransactionView,
            },
        },
        agave_banking_stage_ingress_types::{
            BankingPacketBatch, to_banking_packet_batch, to_single_banking_packet_batch,
        },
        crossbeam_channel::{Receiver, Sender, bounded},
        solana_account::AccountSharedData,
        solana_compute_budget_interface::ComputeBudgetInstruction,
        solana_fee_calculator::FeeRateGovernor,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_ledger::genesis_utils::GenesisConfigInfo,
        solana_message::{
            AccountMeta, AddressLookupTableAccount, Instruction, Message, VersionedMessage, v0,
        },
        solana_nonce::{self as nonce, state::DurableNonce},
        solana_packet::{Meta, PACKET_DATA_SIZE},
        solana_perf::packet::{BytesPacket, Packet, PacketBatch, RecycledPacketBatch},
        solana_pubkey::Pubkey,
        solana_runtime::{bank::Bank, bank_forks::BankForks},
        solana_sdk_ids::system_program,
        solana_signer::Signer,
        solana_slot_hashes::get_entries,
        solana_system_interface::instruction as system_instruction,
        solana_system_transaction::transfer,
        solana_transaction::{Transaction, versioned::VersionedTransaction},
        std::{
            collections::HashSet,
            num::NonZeroUsize,
            sync::{Arc, RwLock},
        },
        test_case::test_case,
    };

    fn test_bank_forks() -> (Arc<RwLock<BankForks>>, Keypair) {
        _test_bank_forks(0)
    }

    fn test_bank_forks_with_fee() -> (Arc<RwLock<BankForks>>, Keypair) {
        _test_bank_forks(5_000)
    }

    fn _test_bank_forks(fee: u64) -> (Arc<RwLock<BankForks>>, Keypair) {
        let GenesisConfigInfo {
            mut genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(u64::MAX);
        genesis_config.fee_rate_governor = FeeRateGovernor::new(fee, 0);

        let (_bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        (bank_forks, mint_keypair)
    }

    #[test]
    fn test_calculate_max_age() {
        let current_slot = 100;
        let sanitized_epoch = 10;

        assert_eq!(
            calculate_max_age(sanitized_epoch, current_slot - 1, current_slot),
            MaxAge {
                sanitized_epoch,
                alt_invalidation_slot: current_slot - 1 + get_entries() as u64,
            }
        );
        assert_eq!(
            calculate_max_age(sanitized_epoch, u64::MAX, current_slot),
            MaxAge {
                sanitized_epoch,
                alt_invalidation_slot: current_slot + get_entries() as u64,
            }
        );
    }

    fn assert_runtime_view(_: &RuntimeTransactionView) {}

    #[test]
    fn prechecked_transaction_owns_packet_bytes() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(u64::MAX);
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let transaction = transfer(
            &mint_keypair,
            &Pubkey::new_unique(),
            1,
            bank.last_blockhash(),
        );
        let packet = BytesPacket::from_data(transaction).unwrap();
        let bytes = packet.buffer().clone();
        let bytes_ptr = bytes.as_ptr();
        drop(packet);

        let PrecheckedTransaction {
            state,
            validated_nonce_address,
        } = precheck_transaction(bytes, &bank, &bank, &HashSet::new()).unwrap();

        assert_runtime_view(state.transaction());
        assert_eq!(state.transaction().data().as_ptr(), bytes_ptr);
        assert_eq!(state.transaction().get_durable_nonce(), None);
        assert_eq!(validated_nonce_address, None);
        assert_eq!(state.nonce_address(), None);
    }

    const TEST_CONTAINER_CAPACITY: usize = 100;

    fn setup_transaction_view_receive_and_buffer(
        receiver: Receiver<BankingPacketBatch>,
        bank_forks: Arc<RwLock<BankForks>>,
    ) -> (
        TransactionViewReceiveAndBuffer,
        TransactionViewStateContainer,
    ) {
        setup_transaction_view_receive_and_buffer_with_filter_keys(
            receiver,
            bank_forks,
            Arc::default(),
        )
    }

    fn setup_transaction_view_receive_and_buffer_with_filter_keys(
        receiver: Receiver<BankingPacketBatch>,
        bank_forks: Arc<RwLock<BankForks>>,
        filter_keys: Arc<HashSet<Pubkey>>,
    ) -> (
        TransactionViewReceiveAndBuffer,
        TransactionViewStateContainer,
    ) {
        let (check_work_sender, check_work_receiver) = bounded(10_000);
        let (check_result_sender, check_result_receiver) = bounded(10_000);
        let _check_worker_handles = spawn_check_workers(
            NonZeroUsize::new(1).unwrap(),
            check_work_receiver,
            check_result_sender,
            bank_forks.read().unwrap().sharable_banks(),
            filter_keys,
        );
        let receive_and_buffer = TransactionViewReceiveAndBuffer::new(
            receiver,
            check_work_sender,
            check_result_receiver,
        );
        let container = TransactionViewStateContainer::with_capacity(TEST_CONTAINER_CAPACITY);
        (receive_and_buffer, container)
    }

    // verify container state makes sense:
    // 1. Number of transactions matches expectation
    // 2. All transactions IDs in priority queue exist in the map
    // 3. Nonce transactions have a matching nonces-in-use entry.
    #[track_caller]
    fn verify_container<Tx: TransactionWithMeta>(
        container: &mut impl StateContainer<Tx>,
        expected_length: usize,
    ) {
        let mut actual_length: usize = 0;
        while let Some(id) = container.pop() {
            if let Some(state) = container.get_mut_transaction_state(id.id) {
                if let Some(nonce) = state.nonce_address().cloned() {
                    assert_eq!(
                        id,
                        *container.get_nonce_transaction_priority_id(&nonce).unwrap()
                    );
                }
            } else {
                panic!(
                    "transaction in queue position {} with id {} must exist.",
                    actual_length, id.id
                );
            };
            actual_length += 1;
        }

        assert_eq!(actual_length, expected_length);
    }

    fn send_transactions(sender: &Sender<BankingPacketBatch>, transactions: &[Transaction]) {
        sender.send(to_banking_packet_batch(transactions)).unwrap();
    }

    fn num_check_results(stats: &ReceivingStats) -> usize {
        stats.num_dropped_without_parsing
            + stats.num_dropped_on_parsing_and_sanitization
            + stats.num_dropped_on_lock_validation
            + stats.num_dropped_on_compute_budget
            + stats.num_dropped_on_age
            + stats.num_dropped_on_already_processed
            + stats.num_dropped_on_fee_payer
            + stats.num_dropped_on_filter_key
            + stats.num_dropped_on_nonce_dedup
            + stats.num_buffered
    }

    fn receive(
        receive_and_buffer: &mut TransactionViewReceiveAndBuffer,
        container: &mut TransactionViewStateContainer,
    ) -> ReceivingStats {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stats = ReceivingStats::default();
        loop {
            stats.accumulate(
                receive_and_buffer.drain_check_results(container, &BufferedPacketsDecision::Hold),
            );
            stats.accumulate(
                receive_and_buffer
                    .receive_and_buffer_packets(container, &BufferedPacketsDecision::Hold)
                    .unwrap(),
            );

            if receive_and_buffer.receiver.is_empty()
                && num_check_results(&stats) == stats.num_received
            {
                return stats;
            }

            assert!(
                Instant::now() < deadline,
                "timed out waiting for check-worker results"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn test_receive_and_buffer_disconnected_channel() {
        let (sender, receiver) = bounded(1024);
        let (bank_forks, _mint_keypair) = test_bank_forks();
        let (mut receive_and_buffer, mut container) =
            setup_transaction_view_receive_and_buffer(receiver, bank_forks);

        drop(sender); // disconnect channel
        let r = receive_and_buffer
            .receive_and_buffer_packets(&mut container, &BufferedPacketsDecision::Hold);
        assert!(r.is_err());
    }

    #[test]
    fn full_check_queue_does_not_pop_ingress() {
        let (packet_sender, receiver) = bounded(1);
        packet_sender
            .send(to_single_banking_packet_batch(&Transaction::default()))
            .unwrap();
        let (work_sender, _work_receiver) = bounded(1);
        work_sender.send(Bytes::new()).unwrap();
        let (_result_sender, result_receiver) = bounded(1);
        let mut receive_and_buffer = TransactionViewReceiveAndBuffer {
            receiver,
            check_work_sender: work_sender,
            check_result_receiver: result_receiver,
        };
        let mut container = TransactionViewStateContainer::with_capacity(1);

        let stats = receive_and_buffer
            .receive_and_buffer_packets(&mut container, &BufferedPacketsDecision::Hold)
            .unwrap();

        assert_eq!(stats.num_received, 0);
        assert_eq!(receive_and_buffer.receiver.len(), 1);
    }

    #[test]
    fn partial_batch_tail_is_dropped_when_check_queue_fills() {
        let (packet_sender, receiver) = bounded(1);
        packet_sender
            .send(to_banking_packet_batch(&[
                Transaction::default(),
                Transaction::default(),
            ]))
            .unwrap();
        let (work_sender, work_receiver) = bounded(1);
        let (_result_sender, result_receiver) = bounded(1);
        let mut receive_and_buffer = TransactionViewReceiveAndBuffer {
            receiver,
            check_work_sender: work_sender,
            check_result_receiver: result_receiver,
        };
        let mut container = TransactionViewStateContainer::with_capacity(1);

        let first = receive_and_buffer
            .receive_and_buffer_packets(&mut container, &BufferedPacketsDecision::Hold)
            .unwrap();
        assert_eq!(first.num_received, 2);
        assert_eq!(first.num_dropped_on_check_work_queue_full, 1);
        assert_eq!(first.num_dropped_on_capacity, 0);
        assert!(receive_and_buffer.receiver.is_empty());
        assert_eq!(work_receiver.len(), 1);
    }

    #[test]
    fn test_receive_and_buffer_no_hold() {
        let (sender, receiver) = bounded(1024);
        let (bank_forks, mint_keypair) = test_bank_forks();
        let (mut receive_and_buffer, mut container) =
            setup_transaction_view_receive_and_buffer(receiver, bank_forks.clone());

        let transaction = transfer(
            &mint_keypair,
            &Pubkey::new_unique(),
            1,
            bank_forks.read().unwrap().root_bank().last_blockhash(),
        );
        let packet_batch = to_single_banking_packet_batch(&transaction);
        sender.send(packet_batch).unwrap();

        let ReceivingStats {
            num_received,
            num_dropped_without_parsing,
            num_dropped_on_parsing_and_sanitization,
            num_dropped_on_lock_validation,
            num_dropped_on_compute_budget,
            num_dropped_on_age,
            num_dropped_on_already_processed,
            num_dropped_on_fee_payer,
            num_dropped_on_filter_key: _,
            num_dropped_on_check_work_queue_full: _,
            num_dropped_on_capacity,
            num_dropped_on_nonce_dedup,
            num_buffered,
            num_evicted_on_nonce_dedup,
            receive_time_us: _,
            buffer_time_us: _,
        } = receive_and_buffer
            .receive_and_buffer_packets(
                &mut container,
                &BufferedPacketsDecision::Forward, // no packets should be held
            )
            .unwrap();

        assert_eq!(num_received, 1);
        assert_eq!(num_dropped_without_parsing, 1);
        assert_eq!(num_dropped_on_parsing_and_sanitization, 0);
        assert_eq!(num_dropped_on_lock_validation, 0);
        assert_eq!(num_dropped_on_compute_budget, 0);
        assert_eq!(num_dropped_on_age, 0);
        assert_eq!(num_dropped_on_already_processed, 0);
        assert_eq!(num_dropped_on_fee_payer, 0);
        assert_eq!(num_dropped_on_capacity, 0);
        assert_eq!(num_dropped_on_nonce_dedup, 0);
        assert_eq!(num_buffered, 0);
        assert_eq!(num_evicted_on_nonce_dedup, 0);
        verify_container(&mut container, 0);
    }

    #[test]
    fn test_receive_and_buffer_discard() {
        let (sender, receiver) = bounded(1024);
        let (bank_forks, mint_keypair) = test_bank_forks();
        let (mut receive_and_buffer, mut container) =
            setup_transaction_view_receive_and_buffer(receiver, bank_forks.clone());

        let transaction = transfer(
            &mint_keypair,
            &Pubkey::new_unique(),
            1,
            bank_forks.read().unwrap().root_bank().last_blockhash(),
        );
        let mut packet_batch = to_single_banking_packet_batch(&transaction);
        Arc::make_mut(&mut packet_batch)
            .first_mut()
            .unwrap()
            .meta_mut()
            .set_discard(true);
        sender.send(packet_batch).unwrap();

        let ReceivingStats {
            num_received,
            num_dropped_without_parsing,
            num_dropped_on_parsing_and_sanitization,
            num_dropped_on_lock_validation,
            num_dropped_on_compute_budget,
            num_dropped_on_age,
            num_dropped_on_already_processed,
            num_dropped_on_fee_payer,
            num_dropped_on_filter_key: _,
            num_dropped_on_check_work_queue_full: _,
            num_dropped_on_capacity,
            num_dropped_on_nonce_dedup,
            num_buffered,
            num_evicted_on_nonce_dedup,
            receive_time_us: _,
            buffer_time_us: _,
        } = receive(&mut receive_and_buffer, &mut container);

        assert_eq!(num_received, 0);
        assert_eq!(num_dropped_without_parsing, 0);
        assert_eq!(num_dropped_on_parsing_and_sanitization, 0);
        assert_eq!(num_dropped_on_lock_validation, 0);
        assert_eq!(num_dropped_on_compute_budget, 0);
        assert_eq!(num_dropped_on_age, 0);
        assert_eq!(num_dropped_on_already_processed, 0);
        assert_eq!(num_dropped_on_fee_payer, 0);
        assert_eq!(num_dropped_on_capacity, 0);
        assert_eq!(num_dropped_on_nonce_dedup, 0);
        assert_eq!(num_buffered, 0);
        assert_eq!(num_evicted_on_nonce_dedup, 0);

        verify_container(&mut container, 0);
    }

    #[test]
    fn test_receive_and_buffer_invalid_transaction_format() {
        let (sender, receiver) = bounded(1024);
        let (bank_forks, _mint_keypair) = test_bank_forks();
        let (mut receive_and_buffer, mut container) =
            setup_transaction_view_receive_and_buffer(receiver, bank_forks);

        let packet_batch = Arc::new(PacketBatch::from(RecycledPacketBatch::new(vec![
            Packet::new([1u8; PACKET_DATA_SIZE], Meta::default()),
        ])));
        sender.send(packet_batch).unwrap();

        let ReceivingStats {
            num_received,
            num_dropped_without_parsing,
            num_dropped_on_parsing_and_sanitization,
            num_dropped_on_lock_validation,
            num_dropped_on_compute_budget,
            num_dropped_on_age,
            num_dropped_on_already_processed,
            num_dropped_on_fee_payer,
            num_dropped_on_filter_key: _,
            num_dropped_on_check_work_queue_full: _,
            num_dropped_on_capacity,
            num_dropped_on_nonce_dedup,
            num_buffered,
            num_evicted_on_nonce_dedup,
            receive_time_us: _,
            buffer_time_us: _,
        } = receive(&mut receive_and_buffer, &mut container);

        assert_eq!(num_received, 1);
        assert_eq!(num_dropped_without_parsing, 0);
        assert_eq!(num_dropped_on_parsing_and_sanitization, 1);
        assert_eq!(num_dropped_on_lock_validation, 0);
        assert_eq!(num_dropped_on_compute_budget, 0);
        assert_eq!(num_dropped_on_age, 0);
        assert_eq!(num_dropped_on_already_processed, 0);
        assert_eq!(num_dropped_on_fee_payer, 0);
        assert_eq!(num_dropped_on_capacity, 0);
        assert_eq!(num_dropped_on_nonce_dedup, 0);
        assert_eq!(num_buffered, 0);
        assert_eq!(num_evicted_on_nonce_dedup, 0);

        verify_container(&mut container, 0);
    }

    #[test]
    fn test_receive_and_buffer_invalid_blockhash() {
        let (sender, receiver) = bounded(1024);
        let (bank_forks, mint_keypair) = test_bank_forks();
        let (mut receive_and_buffer, mut container) =
            setup_transaction_view_receive_and_buffer(receiver, bank_forks);

        let transaction = transfer(&mint_keypair, &Pubkey::new_unique(), 1, Hash::new_unique());
        let packet_batch = to_single_banking_packet_batch(&transaction);
        sender.send(packet_batch).unwrap();

        let ReceivingStats {
            num_received,
            num_dropped_without_parsing,
            num_dropped_on_parsing_and_sanitization,
            num_dropped_on_lock_validation,
            num_dropped_on_compute_budget,
            num_dropped_on_age,
            num_dropped_on_already_processed,
            num_dropped_on_fee_payer,
            num_dropped_on_filter_key: _,
            num_dropped_on_check_work_queue_full: _,
            num_dropped_on_capacity,
            num_dropped_on_nonce_dedup,
            num_buffered,
            num_evicted_on_nonce_dedup,
            receive_time_us: _,
            buffer_time_us: _,
        } = receive(&mut receive_and_buffer, &mut container);

        assert_eq!(num_received, 1);
        assert_eq!(num_dropped_without_parsing, 0);
        assert_eq!(num_dropped_on_parsing_and_sanitization, 0);
        assert_eq!(num_dropped_on_lock_validation, 0);
        assert_eq!(num_dropped_on_compute_budget, 0);
        assert_eq!(num_dropped_on_age, 1);
        assert_eq!(num_dropped_on_already_processed, 0);
        assert_eq!(num_dropped_on_fee_payer, 0);
        assert_eq!(num_dropped_on_capacity, 0);
        assert_eq!(num_dropped_on_nonce_dedup, 0);
        assert_eq!(num_buffered, 0);
        assert_eq!(num_evicted_on_nonce_dedup, 0);

        verify_container(&mut container, 0);
    }

    #[test]
    fn test_receive_and_buffer_simple_transfer_unfunded_fee_payer() {
        let (sender, receiver) = bounded(1024);
        let (bank_forks, _mint_keypair) = test_bank_forks();
        let (mut receive_and_buffer, mut container) =
            setup_transaction_view_receive_and_buffer(receiver, bank_forks.clone());

        let transaction = transfer(
            &Keypair::new(),
            &Pubkey::new_unique(),
            1,
            bank_forks.read().unwrap().root_bank().last_blockhash(),
        );
        let packet_batch = to_single_banking_packet_batch(&transaction);
        sender.send(packet_batch).unwrap();

        let ReceivingStats {
            num_received,
            num_dropped_without_parsing,
            num_dropped_on_parsing_and_sanitization,
            num_dropped_on_lock_validation,
            num_dropped_on_compute_budget,
            num_dropped_on_age,
            num_dropped_on_already_processed,
            num_dropped_on_fee_payer,
            num_dropped_on_filter_key: _,
            num_dropped_on_check_work_queue_full: _,
            num_dropped_on_capacity,
            num_dropped_on_nonce_dedup,
            num_buffered,
            num_evicted_on_nonce_dedup,
            receive_time_us: _,
            buffer_time_us: _,
        } = receive(&mut receive_and_buffer, &mut container);

        assert_eq!(num_received, 1);
        assert_eq!(num_dropped_without_parsing, 0);
        assert_eq!(num_dropped_on_parsing_and_sanitization, 0);
        assert_eq!(num_dropped_on_lock_validation, 0);
        assert_eq!(num_dropped_on_compute_budget, 0);
        assert_eq!(num_dropped_on_age, 0);
        assert_eq!(num_dropped_on_already_processed, 0);
        assert_eq!(num_dropped_on_fee_payer, 1);
        assert_eq!(num_dropped_on_capacity, 0);
        assert_eq!(num_dropped_on_nonce_dedup, 0);
        assert_eq!(num_buffered, 0);
        assert_eq!(num_evicted_on_nonce_dedup, 0);

        verify_container(&mut container, 0);
    }

    #[test]
    fn test_receive_and_buffer_failed_alt_resolve() {
        let (sender, receiver) = bounded(1024);
        let (bank_forks, mint_keypair) = test_bank_forks();
        let (mut receive_and_buffer, mut container) =
            setup_transaction_view_receive_and_buffer(receiver, bank_forks.clone());

        let to_pubkey = Pubkey::new_unique();
        let transaction = VersionedTransaction::try_new(
            VersionedMessage::V0(
                v0::Message::try_compile(
                    &mint_keypair.pubkey(),
                    &[system_instruction::transfer(
                        &mint_keypair.pubkey(),
                        &to_pubkey,
                        1,
                    )],
                    &[AddressLookupTableAccount {
                        key: Pubkey::new_unique(), // will fail if using **bank** to lookup
                        addresses: vec![to_pubkey],
                    }],
                    bank_forks.read().unwrap().root_bank().last_blockhash(),
                )
                .unwrap(),
            ),
            &[&mint_keypair],
        )
        .unwrap();
        let packet_batch = to_single_banking_packet_batch(&transaction);
        sender.send(packet_batch).unwrap();

        let ReceivingStats {
            num_received,
            num_dropped_without_parsing,
            num_dropped_on_parsing_and_sanitization,
            num_dropped_on_lock_validation,
            num_dropped_on_compute_budget,
            num_dropped_on_age,
            num_dropped_on_already_processed,
            num_dropped_on_fee_payer,
            num_dropped_on_filter_key: _,
            num_dropped_on_check_work_queue_full: _,
            num_dropped_on_capacity,
            num_dropped_on_nonce_dedup,
            num_buffered,
            num_evicted_on_nonce_dedup,
            receive_time_us: _,
            buffer_time_us: _,
        } = receive(&mut receive_and_buffer, &mut container);

        assert_eq!(num_received, 1);
        assert_eq!(num_dropped_without_parsing, 0);
        assert_eq!(num_dropped_on_parsing_and_sanitization, 1);
        assert_eq!(num_dropped_on_lock_validation, 0);
        assert_eq!(num_dropped_on_compute_budget, 0);
        assert_eq!(num_dropped_on_age, 0);
        assert_eq!(num_dropped_on_already_processed, 0);
        assert_eq!(num_dropped_on_fee_payer, 0);
        assert_eq!(num_dropped_on_capacity, 0);
        assert_eq!(num_dropped_on_nonce_dedup, 0);
        assert_eq!(num_buffered, 0);
        assert_eq!(num_evicted_on_nonce_dedup, 0);

        verify_container(&mut container, 0);
    }

    #[test]
    fn test_receive_and_buffer_simple_transfer() {
        let (sender, receiver) = bounded(1024);
        let (bank_forks, mint_keypair) = test_bank_forks();
        let (mut receive_and_buffer, mut container) =
            setup_transaction_view_receive_and_buffer(receiver, bank_forks.clone());

        let transaction = transfer(
            &mint_keypair,
            &Pubkey::new_unique(),
            1,
            bank_forks.read().unwrap().root_bank().last_blockhash(),
        );
        let packet_batch = to_single_banking_packet_batch(&transaction);
        sender.send(packet_batch).unwrap();

        let ReceivingStats {
            num_received,
            num_dropped_without_parsing,
            num_dropped_on_parsing_and_sanitization,
            num_dropped_on_lock_validation,
            num_dropped_on_compute_budget,
            num_dropped_on_age,
            num_dropped_on_already_processed,
            num_dropped_on_fee_payer,
            num_dropped_on_filter_key: _,
            num_dropped_on_check_work_queue_full: _,
            num_dropped_on_capacity,
            num_dropped_on_nonce_dedup,
            num_buffered,
            num_evicted_on_nonce_dedup,
            receive_time_us: _,
            buffer_time_us: _,
        } = receive(&mut receive_and_buffer, &mut container);

        assert_eq!(num_received, 1);
        assert_eq!(num_dropped_without_parsing, 0);
        assert_eq!(num_dropped_on_parsing_and_sanitization, 0);
        assert_eq!(num_dropped_on_lock_validation, 0);
        assert_eq!(num_dropped_on_compute_budget, 0);
        assert_eq!(num_dropped_on_age, 0);
        assert_eq!(num_dropped_on_already_processed, 0);
        assert_eq!(num_dropped_on_fee_payer, 0);
        assert_eq!(num_dropped_on_capacity, 0);
        assert_eq!(num_dropped_on_nonce_dedup, 0);
        assert_eq!(num_buffered, 1);
        assert_eq!(num_evicted_on_nonce_dedup, 0);

        verify_container(&mut container, 1);
    }

    #[test]
    fn test_receive_and_buffer_pinned_packet() {
        let (sender, receiver) = bounded(1024);
        let (bank_forks, mint_keypair) = test_bank_forks();
        let (mut receive_and_buffer, mut container) =
            setup_transaction_view_receive_and_buffer(receiver, bank_forks.clone());

        let transaction = transfer(
            &mint_keypair,
            &Pubkey::new_unique(),
            1,
            bank_forks.read().unwrap().root_bank().last_blockhash(),
        );
        let packet = Packet::from_data(None, transaction).unwrap();
        sender
            .send(Arc::new(PacketBatch::from(RecycledPacketBatch::new(vec![
                packet,
            ]))))
            .unwrap();

        let stats = receive(&mut receive_and_buffer, &mut container);
        assert_eq!(stats.num_received, 1);
        assert_eq!(stats.num_buffered, 1);
        verify_container(&mut container, 1);
    }

    #[test]
    fn test_receive_and_buffer_buffers_already_processed() {
        let (sender, receiver) = bounded(1024);
        let (bank_forks, mint_keypair) = test_bank_forks();
        let (mut receive_and_buffer, mut container) =
            setup_transaction_view_receive_and_buffer(receiver, bank_forks.clone());

        let bank = bank_forks.read().unwrap().root_bank();
        let transaction = transfer(
            &mint_keypair,
            &Pubkey::new_unique(),
            1,
            bank.last_blockhash(),
        );
        bank.process_transaction(&transaction).unwrap();
        drop(bank);

        let packet_batch = to_single_banking_packet_batch(&transaction);
        sender.send(packet_batch).unwrap();

        let stats = receive(&mut receive_and_buffer, &mut container);

        assert_eq!(stats.num_received, 1);
        assert_eq!(stats.num_dropped_on_age, 0);
        assert_eq!(stats.num_dropped_on_already_processed, 0);
        assert_eq!(stats.num_buffered, 1);
        verify_container(&mut container, 1);
    }

    #[test]
    fn test_receive_and_buffer_filters_fee_payer() {
        let (sender, receiver) = bounded(1024);
        let (bank_forks, mint_keypair) = test_bank_forks();
        let (mut receive_and_buffer, mut container) =
            setup_transaction_view_receive_and_buffer_with_filter_keys(
                receiver,
                bank_forks.clone(),
                Arc::new(HashSet::from([mint_keypair.pubkey()])),
            );

        let transaction = transfer(
            &mint_keypair,
            &Pubkey::new_unique(),
            1,
            bank_forks.read().unwrap().root_bank().last_blockhash(),
        );
        let packet_batch = to_single_banking_packet_batch(&transaction);
        sender.send(packet_batch).unwrap();

        let stats = receive(&mut receive_and_buffer, &mut container);

        assert_eq!(stats.num_received, 1);
        assert_eq!(stats.num_dropped_on_filter_key, 1);
        assert_eq!(stats.num_buffered, 0);
        verify_container(&mut container, 0);
    }

    #[test]
    fn test_receive_and_buffer_filters_account_key() {
        let (sender, receiver) = bounded(1024);
        let (bank_forks, mint_keypair) = test_bank_forks();
        let filtered_key = Pubkey::new_unique();
        let (mut receive_and_buffer, mut container) =
            setup_transaction_view_receive_and_buffer_with_filter_keys(
                receiver,
                bank_forks.clone(),
                Arc::new(HashSet::from([filtered_key])),
            );

        let transaction = transfer(
            &mint_keypair,
            &filtered_key,
            1,
            bank_forks.read().unwrap().root_bank().last_blockhash(),
        );
        let packet_batch = to_single_banking_packet_batch(&transaction);
        sender.send(packet_batch).unwrap();

        let stats = receive(&mut receive_and_buffer, &mut container);

        assert_eq!(stats.num_received, 1);
        assert_eq!(stats.num_dropped_on_filter_key, 1);
        assert_eq!(stats.num_buffered, 0);
        verify_container(&mut container, 0);
    }

    #[test]
    fn test_receive_and_buffer_does_not_filter_unmatched_keys() {
        let (sender, receiver) = bounded(1024);
        let (bank_forks, mint_keypair) = test_bank_forks();
        let (mut receive_and_buffer, mut container) =
            setup_transaction_view_receive_and_buffer_with_filter_keys(
                receiver,
                bank_forks.clone(),
                Arc::new(HashSet::from([Pubkey::new_unique()])),
            );

        let transaction = transfer(
            &mint_keypair,
            &Pubkey::new_unique(),
            1,
            bank_forks.read().unwrap().root_bank().last_blockhash(),
        );
        let packet_batch = to_single_banking_packet_batch(&transaction);
        sender.send(packet_batch).unwrap();

        let stats = receive(&mut receive_and_buffer, &mut container);

        assert_eq!(stats.num_received, 1);
        assert_eq!(stats.num_dropped_on_filter_key, 0);
        assert_eq!(stats.num_buffered, 1);
        verify_container(&mut container, 1);
    }

    #[test]
    fn test_receive_and_buffer_overfull() {
        let (sender, receiver) = bounded(1024);
        let (bank_forks, mint_keypair) = test_bank_forks();
        let (mut receive_and_buffer, mut container) =
            setup_transaction_view_receive_and_buffer(receiver, bank_forks.clone());

        let num_transactions = 3 * TEST_CONTAINER_CAPACITY;
        let transactions = Vec::from_iter((0..num_transactions).map(|_| {
            transfer(
                &mint_keypair,
                &Pubkey::new_unique(),
                1,
                bank_forks.read().unwrap().root_bank().last_blockhash(),
            )
        }));

        let packet_batch = to_banking_packet_batch(&transactions);
        sender.send(packet_batch).unwrap();

        let ReceivingStats {
            num_received,
            num_dropped_without_parsing,
            num_dropped_on_parsing_and_sanitization,
            num_dropped_on_lock_validation,
            num_dropped_on_compute_budget,
            num_dropped_on_age,
            num_dropped_on_already_processed,
            num_dropped_on_fee_payer,
            num_dropped_on_filter_key: _,
            num_dropped_on_check_work_queue_full: _,
            num_dropped_on_capacity,
            num_dropped_on_nonce_dedup,
            num_buffered,
            num_evicted_on_nonce_dedup,
            receive_time_us: _,
            buffer_time_us: _,
        } = receive(&mut receive_and_buffer, &mut container);

        assert_eq!(num_received, num_transactions);
        assert_eq!(num_dropped_without_parsing, 0);
        assert_eq!(num_dropped_on_parsing_and_sanitization, 0);
        assert_eq!(num_dropped_on_lock_validation, 0);
        assert_eq!(num_dropped_on_compute_budget, 0);
        assert_eq!(num_dropped_on_age, 0);
        assert_eq!(num_dropped_on_already_processed, 0);
        assert_eq!(num_dropped_on_fee_payer, 0);
        assert!(num_dropped_on_capacity > 0);
        assert_eq!(num_dropped_on_nonce_dedup, 0);
        assert_eq!(num_buffered, num_transactions);
        assert_eq!(num_evicted_on_nonce_dedup, 0);

        verify_container(&mut container, TEST_CONTAINER_CAPACITY);
    }

    #[test]
    fn test_receive_and_buffer_too_many_keys() {
        fn create_tx_with_n_keys(payer: &Keypair, n: usize) -> VersionedTransaction {
            let alt_keys = (0..n - 2).map(|_| Pubkey::new_unique()).collect::<Vec<_>>();
            VersionedTransaction::try_new(
                VersionedMessage::V0(
                    v0::Message::try_compile(
                        &payer.pubkey(),
                        &[Instruction::new_with_bytes(
                            Pubkey::new_unique(),
                            &[],
                            alt_keys
                                .iter()
                                .map(|k| AccountMeta::new(*k, false))
                                .collect::<Vec<_>>(),
                        )],
                        &[AddressLookupTableAccount {
                            key: Pubkey::new_unique(),
                            addresses: alt_keys,
                        }],
                        Hash::new_unique(),
                    )
                    .unwrap(),
                ),
                &[payer],
            )
            .unwrap()
        }

        let (sender, receiver) = bounded(1024);
        let (bank_forks, mint_keypair) = test_bank_forks();
        let (mut receive_and_buffer, mut container) =
            setup_transaction_view_receive_and_buffer(receiver, bank_forks.clone());

        let transaction_account_lock_limit = bank_forks
            .read()
            .unwrap()
            .root_bank()
            .get_transaction_account_lock_limit();

        // ALTs do not actually exist in the bank for this transaction - sanitization would cause failure if
        // lock validation was not done first.
        let bad_tx = create_tx_with_n_keys(&mint_keypair, transaction_account_lock_limit + 1);
        let transactions = [bad_tx];

        let packet_batch = to_banking_packet_batch(&transactions);
        sender.send(packet_batch).unwrap();

        let ReceivingStats {
            num_received,
            num_dropped_without_parsing,
            num_dropped_on_parsing_and_sanitization,
            num_dropped_on_lock_validation,
            num_dropped_on_compute_budget,
            num_dropped_on_age,
            num_dropped_on_already_processed,
            num_dropped_on_fee_payer,
            num_dropped_on_filter_key: _,
            num_dropped_on_check_work_queue_full: _,
            num_dropped_on_capacity,
            num_dropped_on_nonce_dedup,
            num_buffered,
            num_evicted_on_nonce_dedup,
            receive_time_us: _,
            buffer_time_us: _,
        } = receive(&mut receive_and_buffer, &mut container);

        assert_eq!(num_received, 1);
        assert_eq!(num_dropped_without_parsing, 0);
        assert_eq!(num_dropped_on_parsing_and_sanitization, 0);
        assert_eq!(num_dropped_on_lock_validation, 1);
        assert_eq!(num_dropped_on_compute_budget, 0);
        assert_eq!(num_dropped_on_age, 0);
        assert_eq!(num_dropped_on_already_processed, 0);
        assert_eq!(num_dropped_on_fee_payer, 0);
        assert_eq!(num_dropped_on_capacity, 0);
        assert_eq!(num_dropped_on_nonce_dedup, 0);
        assert_eq!(num_buffered, 0);
        assert_eq!(num_evicted_on_nonce_dedup, 0);

        verify_container(&mut container, 0);
    }

    const LOW_FEE: u64 = 1;
    const HIGH_FEE: u64 = 1_000_000;

    // sets up a nonce account in the bank for a true nonce transaction
    fn create_nonce_identity(
        bank_forks: &RwLock<BankForks>,
        nonce_authority: &Pubkey,
    ) -> (Pubkey, Hash) {
        let nonce_pubkey = Pubkey::new_unique();
        let bank = bank_forks.read().unwrap().root_bank();
        let nonce_data = nonce::state::Data::new(
            *nonce_authority,
            DurableNonce::from_blockhash(&Hash::new_unique()),
            5_000,
        );
        let nonce_account = AccountSharedData::new_data(
            bank.get_minimum_balance_for_rent_exemption(nonce::state::State::size()),
            &nonce::versions::Versions::new(nonce::state::State::Initialized(nonce_data.clone())),
            &system_program::id(),
        )
        .unwrap();
        bank.store_account(&nonce_pubkey, &nonce_account);
        (nonce_pubkey, nonce_data.blockhash())
    }

    // build a nonce-like transaction, which may be nonce- or blockhash-based, depending
    // on the value of `lifetime`
    fn create_nonce_transaction(
        fee_payer: &Keypair,
        nonce_pubkey: &Pubkey,
        compute_unit_price: u64,
        lifetime: Hash,
    ) -> Transaction {
        let ixs = [
            system_instruction::advance_nonce_account(nonce_pubkey, &fee_payer.pubkey()),
            ComputeBudgetInstruction::set_compute_unit_price(compute_unit_price),
            system_instruction::transfer(&fee_payer.pubkey(), &Pubkey::new_unique(), 1),
        ];
        let message = Message::new(&ixs, Some(&fee_payer.pubkey()));
        Transaction::new(&[fee_payer], message, lifetime)
    }

    // adding nonce transactions works normally. different nonces dont conflict,
    // removing a nonce transaction removes its nonces-in-use entry
    #[test]
    fn test_receive_and_buffer_nonce_tracked() {
        let (sender, receiver) = bounded(1024);
        let (bank_forks, mint_keypair) = test_bank_forks_with_fee();
        let (mut receive_and_buffer, mut container) =
            setup_transaction_view_receive_and_buffer(receiver, bank_forks.clone());
        let (nonce_pubkey1, durable1) = create_nonce_identity(&bank_forks, &mint_keypair.pubkey());
        let (nonce_pubkey2, durable2) = create_nonce_identity(&bank_forks, &mint_keypair.pubkey());

        send_transactions(
            &sender,
            &[
                create_nonce_transaction(&mint_keypair, &nonce_pubkey1, LOW_FEE, durable1),
                create_nonce_transaction(&mint_keypair, &nonce_pubkey2, HIGH_FEE, durable2),
            ],
        );

        let stats = receive(&mut receive_and_buffer, &mut container);
        assert_eq!(stats.num_buffered, 2);
        assert_eq!(stats.num_dropped_on_nonce_dedup, 0);
        assert_eq!(stats.num_evicted_on_nonce_dedup, 0);

        let nonce_entry1 = *container
            .get_nonce_transaction_priority_id(&nonce_pubkey1)
            .unwrap();
        let nonce_entry2 = *container
            .get_nonce_transaction_priority_id(&nonce_pubkey2)
            .unwrap();

        assert!(container.is_queued(&nonce_entry1));
        assert!(container.is_queued(&nonce_entry2));

        container.remove_by_id(nonce_entry1.id);
        assert!(!container.is_queued(&nonce_entry1));
        assert!(container.is_queued(&nonce_entry2));
        assert!(
            container
                .get_nonce_transaction_priority_id(&nonce_pubkey1)
                .is_none()
        );

        container.remove_by_id(nonce_entry2.id);
        assert!(!container.is_queued(&nonce_entry1));
        assert!(!container.is_queued(&nonce_entry2));
        assert!(
            container
                .get_nonce_transaction_priority_id(&nonce_pubkey2)
                .is_none()
        );

        verify_container(&mut container, 0);
    }

    // a higher priority incoming nonce transaction evicts the existing transaction,
    // a lower or equal priority incoming nonce transaction is dropped
    #[test_case(HIGH_FEE, LOW_FEE; "hilo_drop")]
    #[test_case(HIGH_FEE, HIGH_FEE; "hihi_drop")]
    #[test_case(LOW_FEE, HIGH_FEE; "lohi_evict")]
    fn test_receive_and_buffer_nonce_dedup_drop_evict(old_fee: u64, new_fee: u64) {
        let (sender, receiver) = bounded(1024);
        let (bank_forks, mint_keypair) = test_bank_forks_with_fee();
        let (mut receive_and_buffer, mut container) =
            setup_transaction_view_receive_and_buffer(receiver, bank_forks.clone());
        let (nonce_pubkey, durable) = create_nonce_identity(&bank_forks, &mint_keypair.pubkey());
        let new_has_priority = new_fee > old_fee;

        send_transactions(
            &sender,
            &[create_nonce_transaction(
                &mint_keypair,
                &nonce_pubkey,
                old_fee,
                durable,
            )],
        );
        assert_eq!(
            receive(&mut receive_and_buffer, &mut container).num_buffered,
            1
        );
        let prior_nonce_entry = *container
            .get_nonce_transaction_priority_id(&nonce_pubkey)
            .unwrap();

        send_transactions(
            &sender,
            &[create_nonce_transaction(
                &mint_keypair,
                &nonce_pubkey,
                new_fee,
                durable,
            )],
        );

        let stats = receive(&mut receive_and_buffer, &mut container);
        let current_nonce_entry = *container
            .get_nonce_transaction_priority_id(&nonce_pubkey)
            .unwrap();

        if new_has_priority {
            assert_eq!(stats.num_dropped_on_nonce_dedup, 0);
            assert_eq!(stats.num_evicted_on_nonce_dedup, 1);
            assert_eq!(stats.num_buffered, 1);

            assert_ne!(prior_nonce_entry, current_nonce_entry);
            assert!(current_nonce_entry.priority > prior_nonce_entry.priority);
            assert!(
                container
                    .get_mut_transaction_state(prior_nonce_entry.id)
                    .is_none()
            );
        } else {
            assert_eq!(stats.num_dropped_on_nonce_dedup, 1);
            assert_eq!(stats.num_evicted_on_nonce_dedup, 0);
            assert_eq!(stats.num_buffered, 0);
            assert_eq!(prior_nonce_entry, current_nonce_entry);
        }

        assert!(container.is_queued(&current_nonce_entry));

        verify_container(&mut container, 1);
    }

    // Workers validate the blockhash/nonce before scheduler-side nonce deduplication.
    #[test]
    fn test_receive_and_buffer_bank_validation_precedes_nonce_dedup() {
        let (sender, receiver) = bounded(1024);
        let (bank_forks, mint_keypair) = test_bank_forks_with_fee();
        let (mut receive_and_buffer, mut container) =
            setup_transaction_view_receive_and_buffer(receiver, bank_forks.clone());
        let (nonce_pubkey, durable) = create_nonce_identity(&bank_forks, &mint_keypair.pubkey());

        send_transactions(
            &sender,
            &[create_nonce_transaction(
                &mint_keypair,
                &nonce_pubkey,
                HIGH_FEE,
                durable,
            )],
        );
        assert_eq!(
            receive(&mut receive_and_buffer, &mut container).num_buffered,
            1
        );

        send_transactions(
            &sender,
            &[create_nonce_transaction(
                &mint_keypair,
                &nonce_pubkey,
                LOW_FEE,
                Hash::new_unique(),
            )],
        );
        let stats = receive(&mut receive_and_buffer, &mut container);

        assert_eq!(stats.num_dropped_on_nonce_dedup, 0);
        assert_eq!(stats.num_dropped_on_age, 1);
        assert_eq!(stats.num_dropped_on_fee_payer, 0);
        assert_eq!(stats.num_buffered, 0);
        verify_container(&mut container, 1);
    }

    // a scheduled or held nonce transaction is never evicted regardless of priority
    #[test_case(false; "held")]
    #[test_case(true; "scheduled")]
    fn test_receive_and_buffer_nonce_dedup_preserves_in_flight(is_scheduled: bool) {
        let (sender, receiver) = bounded(1024);
        let (bank_forks, mint_keypair) = test_bank_forks_with_fee();
        let (mut receive_and_buffer, mut container) =
            setup_transaction_view_receive_and_buffer(receiver, bank_forks.clone());
        let (nonce_pubkey, durable) = create_nonce_identity(&bank_forks, &mint_keypair.pubkey());

        send_transactions(
            &sender,
            &[create_nonce_transaction(
                &mint_keypair,
                &nonce_pubkey,
                LOW_FEE,
                durable,
            )],
        );
        assert_eq!(
            receive(&mut receive_and_buffer, &mut container).num_buffered,
            1
        );

        let queue_entry = container.pop().unwrap();
        if is_scheduled {
            container
                .get_mut_transaction_state(queue_entry.id)
                .unwrap()
                .take_transaction_for_scheduling();
        } else {
            container.hold_transaction(queue_entry);
        }

        send_transactions(
            &sender,
            &[create_nonce_transaction(
                &mint_keypair,
                &nonce_pubkey,
                HIGH_FEE,
                durable,
            )],
        );
        let stats = receive(&mut receive_and_buffer, &mut container);
        assert_eq!(stats.num_dropped_on_nonce_dedup, 1);
        assert_eq!(stats.num_evicted_on_nonce_dedup, 0);
        assert_eq!(stats.num_buffered, 0);

        let nonce_entry = *container
            .get_nonce_transaction_priority_id(&nonce_pubkey)
            .unwrap();
        assert_eq!(queue_entry, nonce_entry);
        assert!(
            container
                .get_mut_transaction_state(nonce_entry.id)
                .is_some()
        );
    }

    // a higher priority nonce transaction that fails validation does not evict the existing one,
    // and does not affect its nonces-in-use entry
    #[test]
    fn test_receive_and_buffer_nonce_dedup_validation_failure() {
        let (sender, receiver) = bounded(1024);
        let (bank_forks, mint_keypair) = test_bank_forks_with_fee();
        let (mut receive_and_buffer, mut container) =
            setup_transaction_view_receive_and_buffer(receiver, bank_forks.clone());
        let (nonce_pubkey, durable) = create_nonce_identity(&bank_forks, &mint_keypair.pubkey());

        send_transactions(
            &sender,
            &[create_nonce_transaction(
                &mint_keypair,
                &nonce_pubkey,
                LOW_FEE,
                durable,
            )],
        );
        assert_eq!(
            receive(&mut receive_and_buffer, &mut container).num_buffered,
            1
        );
        let prior_nonce_entry = *container
            .get_nonce_transaction_priority_id(&nonce_pubkey)
            .unwrap();

        // bad blockhash
        send_transactions(
            &sender,
            &[create_nonce_transaction(
                &mint_keypair,
                &nonce_pubkey,
                HIGH_FEE,
                Hash::new_unique(),
            )],
        );
        let stats = receive(&mut receive_and_buffer, &mut container);
        assert_eq!(stats.num_dropped_on_age, 1);
        assert_eq!(stats.num_evicted_on_nonce_dedup, 0);
        assert_eq!(stats.num_dropped_on_nonce_dedup, 0);
        assert_eq!(stats.num_buffered, 0);

        let current_nonce_entry = *container
            .get_nonce_transaction_priority_id(&nonce_pubkey)
            .unwrap();
        assert_eq!(prior_nonce_entry, current_nonce_entry);
        assert!(container.is_queued(&current_nonce_entry));

        // bad authority
        let bad_authority = Keypair::new();
        let message = Message::new(
            &[
                system_instruction::advance_nonce_account(&nonce_pubkey, &bad_authority.pubkey()),
                ComputeBudgetInstruction::set_compute_unit_price(HIGH_FEE),
                system_instruction::transfer(&mint_keypair.pubkey(), &Pubkey::new_unique(), 1),
            ],
            Some(&mint_keypair.pubkey()),
        );
        let transaction = Transaction::new(&[&mint_keypair, &bad_authority], message, durable);
        send_transactions(&sender, &[transaction]);
        let stats = receive(&mut receive_and_buffer, &mut container);
        assert_eq!(stats.num_dropped_on_age, 1);
        assert_eq!(stats.num_evicted_on_nonce_dedup, 0);
        assert_eq!(stats.num_dropped_on_nonce_dedup, 0);
        assert_eq!(stats.num_buffered, 0);

        let current_nonce_entry = *container
            .get_nonce_transaction_priority_id(&nonce_pubkey)
            .unwrap();
        assert_eq!(prior_nonce_entry, current_nonce_entry);
        assert!(container.is_queued(&current_nonce_entry));

        // bad feepayer
        let bad_payer = Keypair::new();
        let message = Message::new(
            &[
                system_instruction::advance_nonce_account(&nonce_pubkey, &mint_keypair.pubkey()),
                ComputeBudgetInstruction::set_compute_unit_price(HIGH_FEE),
                system_instruction::transfer(&mint_keypair.pubkey(), &Pubkey::new_unique(), 1),
            ],
            Some(&bad_payer.pubkey()),
        );
        let transaction = Transaction::new(&[&bad_payer, &mint_keypair], message, durable);
        send_transactions(&sender, &[transaction]);
        let stats = receive(&mut receive_and_buffer, &mut container);
        assert_eq!(stats.num_dropped_on_fee_payer, 1);
        assert_eq!(stats.num_evicted_on_nonce_dedup, 0);
        assert_eq!(stats.num_dropped_on_nonce_dedup, 0);
        assert_eq!(stats.num_buffered, 0);

        let current_nonce_entry = *container
            .get_nonce_transaction_priority_id(&nonce_pubkey)
            .unwrap();
        assert_eq!(prior_nonce_entry, current_nonce_entry);
        assert!(container.is_queued(&current_nonce_entry));

        verify_container(&mut container, 1);
    }

    // when a nonce transaction is bumped for capacity, its nonce map entry is cleared
    #[test]
    fn test_receive_and_buffer_nonce_capacity_eviction() {
        let (sender, receiver) = bounded(1024);
        let (bank_forks, mint_keypair) = test_bank_forks_with_fee();
        let (mut receive_and_buffer, _container) =
            setup_transaction_view_receive_and_buffer(receiver, bank_forks.clone());
        let mut container = TransactionViewStateContainer::with_capacity(1);
        let (nonce_pubkey, durable) = create_nonce_identity(&bank_forks, &mint_keypair.pubkey());

        send_transactions(
            &sender,
            &[create_nonce_transaction(
                &mint_keypair,
                &nonce_pubkey,
                0,
                durable,
            )],
        );
        assert_eq!(
            receive(&mut receive_and_buffer, &mut container).num_buffered,
            1
        );

        let nonce_entry = container
            .get_nonce_transaction_priority_id(&nonce_pubkey)
            .cloned()
            .unwrap();

        // the previous txn is 0 priority, so this evicts it, even with 0 priority
        let transaction = transfer(
            &mint_keypair,
            &Pubkey::new_unique(),
            1,
            bank_forks.read().unwrap().root_bank().last_blockhash(),
        );

        send_transactions(&sender, &[transaction]);
        let stats = receive(&mut receive_and_buffer, &mut container);
        assert_eq!(stats.num_buffered, 1);
        assert_eq!(stats.num_dropped_on_capacity, 1);
        assert_eq!(stats.num_evicted_on_nonce_dedup, 0);

        assert!(
            container
                .get_nonce_transaction_priority_id(&nonce_pubkey)
                .is_none()
        );
        assert!(!container.is_queued(&nonce_entry));

        verify_container(&mut container, 1);
    }

    // nonce-like blockhash transactions are queued but not tracked
    #[test]
    fn test_receive_and_buffer_pseudo_nonce_untracked() {
        let (sender, receiver) = bounded(1024);
        let (bank_forks, mint_keypair) = test_bank_forks_with_fee();
        let (mut receive_and_buffer, mut container) =
            setup_transaction_view_receive_and_buffer(receiver, bank_forks.clone());
        let (nonce_pubkey, durable) = create_nonce_identity(&bank_forks, &mint_keypair.pubkey());
        let blockhash = bank_forks.read().unwrap().root_bank().last_blockhash();

        send_transactions(
            &sender,
            &[create_nonce_transaction(
                &mint_keypair,
                &nonce_pubkey,
                HIGH_FEE,
                blockhash,
            )],
        );
        assert_eq!(
            receive(&mut receive_and_buffer, &mut container).num_buffered,
            1
        );
        assert!(
            container
                .get_nonce_transaction_priority_id(&nonce_pubkey)
                .is_none()
        );

        // a real nonce for the same account is still admitted and tracked
        send_transactions(
            &sender,
            &[create_nonce_transaction(
                &mint_keypair,
                &nonce_pubkey,
                LOW_FEE,
                durable,
            )],
        );
        let stats = receive(&mut receive_and_buffer, &mut container);
        assert_eq!(stats.num_buffered, 1);
        assert_eq!(stats.num_evicted_on_nonce_dedup, 0);
        assert!(
            container
                .get_nonce_transaction_priority_id(&nonce_pubkey)
                .is_some()
        );

        verify_container(&mut container, 2);
    }

    // nonce-like blockhash transactions never evict real nonce transactions
    #[test_case(LOW_FEE, HIGH_FEE; "lohi_coexist")]
    #[test_case(HIGH_FEE, LOW_FEE; "hilo_drop")]
    #[test_case(HIGH_FEE, HIGH_FEE; "hihi_drop")]
    fn test_receive_and_buffer_pseudo_nonce_never_evicts(real_fee: u64, pseudo_fee: u64) {
        let (sender, receiver) = bounded(1024);
        let (bank_forks, mint_keypair) = test_bank_forks_with_fee();
        let (mut receive_and_buffer, mut container) =
            setup_transaction_view_receive_and_buffer(receiver, bank_forks.clone());
        let (nonce_pubkey, durable) = create_nonce_identity(&bank_forks, &mint_keypair.pubkey());
        let blockhash = bank_forks.read().unwrap().root_bank().last_blockhash();
        let pseudo_has_priority = pseudo_fee > real_fee;

        send_transactions(
            &sender,
            &[create_nonce_transaction(
                &mint_keypair,
                &nonce_pubkey,
                real_fee,
                durable,
            )],
        );
        assert_eq!(
            receive(&mut receive_and_buffer, &mut container).num_buffered,
            1
        );
        let nonce_entry = *container
            .get_nonce_transaction_priority_id(&nonce_pubkey)
            .unwrap();

        send_transactions(
            &sender,
            &[create_nonce_transaction(
                &mint_keypair,
                &nonce_pubkey,
                pseudo_fee,
                blockhash,
            )],
        );
        let stats = receive(&mut receive_and_buffer, &mut container);
        assert_eq!(stats.num_evicted_on_nonce_dedup, 0);
        let expected_len = if pseudo_has_priority {
            assert_eq!(stats.num_dropped_on_nonce_dedup, 0);
            assert_eq!(stats.num_buffered, 1);
            2
        } else {
            assert_eq!(stats.num_dropped_on_nonce_dedup, 1);
            assert_eq!(stats.num_buffered, 0);
            1
        };

        // the real nonce transaction still holds the nonce and is queued
        assert_eq!(
            *container
                .get_nonce_transaction_priority_id(&nonce_pubkey)
                .unwrap(),
            nonce_entry
        );
        assert!(container.is_queued(&nonce_entry));

        verify_container(&mut container, expected_len);
    }
}
