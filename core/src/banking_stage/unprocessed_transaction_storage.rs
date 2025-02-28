use {
    super::{
        consumer::Consumer,
        immutable_deserialized_packet::ImmutableDeserializedPacket,
        latest_unprocessed_votes::{
            LatestUnprocessedVotes, LatestValidatorVotePacket, VoteBatchInsertionMetrics,
            VoteSource,
        },
        leader_slot_metrics::LeaderSlotMetricsTracker,
        multi_iterator_scanner::{MultiIteratorScanner, ProcessingDecision},
        read_write_account_set::ReadWriteAccountSet,
        unprocessed_packet_batches::{
            DeserializedPacket, PacketBatchInsertionMetrics, UnprocessedPacketBatches,
        },
        BankingStageStats,
    },
    itertools::Itertools,
    min_max_heap::MinMaxHeap,
    solana_accounts_db::account_locks::validate_account_locks,
    solana_measure::measure_us,
    solana_runtime::bank::Bank,
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_sdk::{hash::Hash, transaction::SanitizedTransaction},
    solana_svm::transaction_error_metrics::TransactionErrorMetrics,
    std::{
        collections::HashMap,
        sync::{atomic::Ordering, Arc},
    },
};

// Step-size set to be 64, equal to the maximum batch/entry size. With the
// multi-iterator change, there's no point in getting larger batches of
// non-conflicting transactions.
pub const UNPROCESSED_BUFFER_STEP_SIZE: usize = 64;
/// Maximum number of votes a single receive call will accept
const MAX_NUM_VOTES_RECEIVE: usize = 10_000;

#[derive(Debug)]
pub enum UnprocessedTransactionStorage {
    VoteStorage(VoteStorage),
    LocalTransactionStorage(ThreadLocalUnprocessedPackets),
}

#[derive(Debug)]
pub struct ThreadLocalUnprocessedPackets {
    unprocessed_packet_batches: UnprocessedPacketBatches,
    thread_type: ThreadType,
}

#[derive(Debug)]
pub struct VoteStorage {
    latest_unprocessed_votes: Arc<LatestUnprocessedVotes>,
    vote_source: VoteSource,
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum ThreadType {
    Voting(VoteSource),
    Transactions,
}

#[derive(Debug)]
pub(crate) enum InsertPacketBatchSummary {
    VoteBatchInsertionMetrics(VoteBatchInsertionMetrics),
    PacketBatchInsertionMetrics(PacketBatchInsertionMetrics),
}

impl InsertPacketBatchSummary {
    pub fn total_dropped_packets(&self) -> usize {
        match self {
            Self::VoteBatchInsertionMetrics(metrics) => {
                metrics.num_dropped_gossip + metrics.num_dropped_tpu
            }
            Self::PacketBatchInsertionMetrics(metrics) => metrics.num_dropped_packets,
        }
    }

    pub fn dropped_gossip_packets(&self) -> usize {
        match self {
            Self::VoteBatchInsertionMetrics(metrics) => metrics.num_dropped_gossip,
            _ => 0,
        }
    }

    pub fn dropped_tpu_packets(&self) -> usize {
        match self {
            Self::VoteBatchInsertionMetrics(metrics) => metrics.num_dropped_tpu,
            _ => 0,
        }
    }
}

impl From<VoteBatchInsertionMetrics> for InsertPacketBatchSummary {
    fn from(metrics: VoteBatchInsertionMetrics) -> Self {
        Self::VoteBatchInsertionMetrics(metrics)
    }
}

impl From<PacketBatchInsertionMetrics> for InsertPacketBatchSummary {
    fn from(metrics: PacketBatchInsertionMetrics) -> Self {
        Self::PacketBatchInsertionMetrics(metrics)
    }
}

fn filter_processed_packets<'a, F>(
    retryable_transaction_indexes: impl Iterator<Item = &'a usize>,
    mut f: F,
) where
    F: FnMut(usize, usize),
{
    let mut prev_retryable_index = 0;
    for (i, retryable_index) in retryable_transaction_indexes.enumerate() {
        let start = if i == 0 { 0 } else { prev_retryable_index + 1 };

        let end = *retryable_index;
        prev_retryable_index = *retryable_index;

        if start < end {
            f(start, end)
        }
    }
}

/// Convenient wrapper for shared-state between banking stage processing and the
/// multi-iterator checking function.
pub struct ConsumeScannerPayload<'a> {
    pub reached_end_of_slot: bool,
    pub account_locks: ReadWriteAccountSet,
    pub sanitized_transactions: Vec<RuntimeTransaction<SanitizedTransaction>>,
    pub slot_metrics_tracker: &'a mut LeaderSlotMetricsTracker,
    pub message_hash_to_transaction: &'a mut HashMap<Hash, DeserializedPacket>,
    pub error_counters: TransactionErrorMetrics,
}

fn consume_scan_should_process_packet(
    bank: &Bank,
    banking_stage_stats: &BankingStageStats,
    packet: &ImmutableDeserializedPacket,
    payload: &mut ConsumeScannerPayload,
) -> ProcessingDecision {
    // If end of the slot, return should process (quick loop after reached end of slot)
    if payload.reached_end_of_slot {
        return ProcessingDecision::Now;
    }

    // Try to sanitize the packet. Ignore deactivation slot since we are
    // immediately attempting to process the transaction.
    let (maybe_sanitized_transaction, sanitization_time_us) = measure_us!(packet
        .build_sanitized_transaction(
            bank.vote_only_bank(),
            bank,
            bank.get_reserved_account_keys(),
        )
        .map(|(tx, _deactivation_slot)| tx));

    payload
        .slot_metrics_tracker
        .increment_transactions_from_packets_us(sanitization_time_us);
    banking_stage_stats
        .packet_conversion_elapsed
        .fetch_add(sanitization_time_us, Ordering::Relaxed);

    if let Some(sanitized_transaction) = maybe_sanitized_transaction {
        let message = sanitized_transaction.message();

        // Check the number of locks and whether there are duplicates
        if validate_account_locks(
            message.account_keys(),
            bank.get_transaction_account_lock_limit(),
        )
        .is_err()
        {
            payload
                .message_hash_to_transaction
                .remove(packet.message_hash());
            return ProcessingDecision::Never;
        }

        // Only check fee-payer if we can actually take locks
        // We do not immediately discard on check lock failures here,
        // because the priority guard requires that we always take locks
        // except in the cases of discarding transactions (i.e. `Never`).
        if payload.account_locks.check_locks(message)
            && Consumer::check_fee_payer_unlocked(
                bank,
                &sanitized_transaction,
                &mut payload.error_counters,
            )
            .is_err()
        {
            payload
                .message_hash_to_transaction
                .remove(packet.message_hash());
            return ProcessingDecision::Never;
        }

        // NOTE:
        //   This must be the last operation before adding the transaction to the
        //   sanitized_transactions vector. Otherwise, a transaction could
        //   be blocked by a transaction that did not take batch locks. This
        //   will lead to some transactions never being processed, and a
        //   mismatch in the priority-queue and hash map sizes.
        //
        // Always take locks during batch creation.
        // This prevents lower-priority transactions from taking locks
        // needed by higher-priority txs that were skipped by this check.
        if !payload.account_locks.take_locks(message) {
            return ProcessingDecision::Later;
        }

        payload.sanitized_transactions.push(sanitized_transaction);
        ProcessingDecision::Now
    } else {
        payload
            .message_hash_to_transaction
            .remove(packet.message_hash());
        ProcessingDecision::Never
    }
}

fn create_consume_multi_iterator<'a, 'b, F>(
    packets: &'a [Arc<ImmutableDeserializedPacket>],
    slot_metrics_tracker: &'b mut LeaderSlotMetricsTracker,
    message_hash_to_transaction: &'b mut HashMap<Hash, DeserializedPacket>,
    should_process_packet: F,
) -> MultiIteratorScanner<'a, Arc<ImmutableDeserializedPacket>, ConsumeScannerPayload<'b>, F>
where
    F: FnMut(
        &Arc<ImmutableDeserializedPacket>,
        &mut ConsumeScannerPayload<'b>,
    ) -> ProcessingDecision,
    'b: 'a,
{
    let payload = ConsumeScannerPayload {
        reached_end_of_slot: false,
        account_locks: ReadWriteAccountSet::default(),
        sanitized_transactions: Vec::with_capacity(UNPROCESSED_BUFFER_STEP_SIZE),
        slot_metrics_tracker,
        message_hash_to_transaction,
        error_counters: TransactionErrorMetrics::default(),
    };
    MultiIteratorScanner::new(
        packets,
        UNPROCESSED_BUFFER_STEP_SIZE,
        payload,
        should_process_packet,
    )
}

impl UnprocessedTransactionStorage {
    pub fn new_transaction_storage(
        unprocessed_packet_batches: UnprocessedPacketBatches,
        thread_type: ThreadType,
    ) -> Self {
        Self::LocalTransactionStorage(ThreadLocalUnprocessedPackets {
            unprocessed_packet_batches,
            thread_type,
        })
    }

    pub fn new_vote_storage(
        latest_unprocessed_votes: Arc<LatestUnprocessedVotes>,
        vote_source: VoteSource,
    ) -> Self {
        Self::VoteStorage(VoteStorage {
            latest_unprocessed_votes,
            vote_source,
        })
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::VoteStorage(vote_storage) => vote_storage.is_empty(),
            Self::LocalTransactionStorage(transaction_storage) => transaction_storage.is_empty(),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::VoteStorage(vote_storage) => vote_storage.len(),
            Self::LocalTransactionStorage(transaction_storage) => transaction_storage.len(),
        }
    }

    pub fn get_min_priority(&self) -> Option<u64> {
        match self {
            Self::VoteStorage(_) => None,
            Self::LocalTransactionStorage(transaction_storage) => {
                transaction_storage.get_min_compute_unit_price()
            }
        }
    }

    pub fn get_max_priority(&self) -> Option<u64> {
        match self {
            Self::VoteStorage(_) => None,
            Self::LocalTransactionStorage(transaction_storage) => {
                transaction_storage.get_max_compute_unit_price()
            }
        }
    }

    /// Returns the maximum number of packets a receive should accept
    pub fn max_receive_size(&self) -> usize {
        match self {
            Self::VoteStorage(vote_storage) => vote_storage.max_receive_size(),
            Self::LocalTransactionStorage(transaction_storage) => {
                transaction_storage.max_receive_size()
            }
        }
    }

    pub fn should_not_process(&self) -> bool {
        // The gossip vote thread does not need to process any votes, that is
        // handled by the tpu vote thread
        if let Self::VoteStorage(vote_storage) = self {
            return matches!(vote_storage.vote_source, VoteSource::Gossip);
        }
        false
    }

    #[cfg(test)]
    pub fn iter(&mut self) -> impl Iterator<Item = &DeserializedPacket> {
        match self {
            Self::LocalTransactionStorage(transaction_storage) => transaction_storage.iter(),
            _ => panic!(),
        }
    }

    pub(crate) fn insert_batch(
        &mut self,
        deserialized_packets: Vec<ImmutableDeserializedPacket>,
    ) -> InsertPacketBatchSummary {
        match self {
            Self::VoteStorage(vote_storage) => {
                InsertPacketBatchSummary::from(vote_storage.insert_batch(deserialized_packets))
            }
            Self::LocalTransactionStorage(transaction_storage) => InsertPacketBatchSummary::from(
                transaction_storage.insert_batch(deserialized_packets),
            ),
        }
    }

    /// The processing function takes a stream of packets ready to process, and returns the indices
    /// of the unprocessed packets that are eligible for retry. A return value of None means that
    /// all packets are unprocessed and eligible for retry.
    #[must_use]
    pub fn process_packets<F>(
        &mut self,
        bank: Arc<Bank>,
        banking_stage_stats: &BankingStageStats,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
        processing_function: F,
    ) -> bool
    where
        F: FnMut(
            &Vec<Arc<ImmutableDeserializedPacket>>,
            &mut ConsumeScannerPayload,
        ) -> Option<Vec<usize>>,
    {
        match self {
            Self::LocalTransactionStorage(transaction_storage) => transaction_storage
                .process_packets(
                    &bank,
                    banking_stage_stats,
                    slot_metrics_tracker,
                    processing_function,
                ),
            Self::VoteStorage(vote_storage) => vote_storage.process_packets(
                bank,
                banking_stage_stats,
                slot_metrics_tracker,
                processing_function,
            ),
        }
    }

    pub(crate) fn clear(&mut self) {
        match self {
            Self::LocalTransactionStorage(_) => {}
            Self::VoteStorage(vote_storage) => vote_storage.clear(),
        }
    }

    pub(crate) fn cache_epoch_boundary_info(&mut self, bank: &Bank) {
        match self {
            Self::LocalTransactionStorage(_) => (),
            Self::VoteStorage(vote_storage) => vote_storage.cache_epoch_boundary_info(bank),
        }
    }
}

impl VoteStorage {
    fn is_empty(&self) -> bool {
        self.latest_unprocessed_votes.is_empty()
    }

    fn len(&self) -> usize {
        self.latest_unprocessed_votes.len()
    }

    fn max_receive_size(&self) -> usize {
        MAX_NUM_VOTES_RECEIVE
    }

    fn insert_batch(
        &mut self,
        deserialized_packets: Vec<ImmutableDeserializedPacket>,
    ) -> VoteBatchInsertionMetrics {
        self.latest_unprocessed_votes.insert_batch(
            deserialized_packets
                .into_iter()
                .filter_map(|deserialized_packet| {
                    LatestValidatorVotePacket::new_from_immutable(
                        Arc::new(deserialized_packet),
                        self.vote_source,
                        self.latest_unprocessed_votes
                            .should_deprecate_legacy_vote_ixs(),
                    )
                    .ok()
                }),
            false, // should_replenish_taken_votes
        )
    }

    // returns `true` if the end of slot is reached
    fn process_packets<F>(
        &mut self,
        bank: Arc<Bank>,
        banking_stage_stats: &BankingStageStats,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
        mut processing_function: F,
    ) -> bool
    where
        F: FnMut(
            &Vec<Arc<ImmutableDeserializedPacket>>,
            &mut ConsumeScannerPayload,
        ) -> Option<Vec<usize>>,
    {
        if matches!(self.vote_source, VoteSource::Gossip) {
            panic!("Gossip vote thread should not be processing transactions");
        }

        let should_process_packet =
            |packet: &Arc<ImmutableDeserializedPacket>, payload: &mut ConsumeScannerPayload| {
                consume_scan_should_process_packet(&bank, banking_stage_stats, packet, payload)
            };

        // Based on the stake distribution present in the supplied bank, drain the unprocessed votes
        // from each validator using a weighted random ordering. Votes from validators with
        // 0 stake are ignored.
        let all_vote_packets = self
            .latest_unprocessed_votes
            .drain_unprocessed(bank.clone());

        // vote storage does not have a message hash map, so pass in an empty one
        let mut dummy_message_hash_to_transaction = HashMap::new();
        let mut scanner = create_consume_multi_iterator(
            &all_vote_packets,
            slot_metrics_tracker,
            &mut dummy_message_hash_to_transaction,
            should_process_packet,
        );

        let deprecate_legacy_vote_ixs = self
            .latest_unprocessed_votes
            .should_deprecate_legacy_vote_ixs();

        while let Some((packets, payload)) = scanner.iterate() {
            let vote_packets = packets.iter().map(|p| (*p).clone()).collect_vec();

            if let Some(retryable_vote_indices) = processing_function(&vote_packets, payload) {
                self.latest_unprocessed_votes.insert_batch(
                    retryable_vote_indices.iter().filter_map(|i| {
                        LatestValidatorVotePacket::new_from_immutable(
                            vote_packets[*i].clone(),
                            self.vote_source,
                            deprecate_legacy_vote_ixs,
                        )
                        .ok()
                    }),
                    true, // should_replenish_taken_votes
                );
            } else {
                self.latest_unprocessed_votes.insert_batch(
                    vote_packets.into_iter().filter_map(|packet| {
                        LatestValidatorVotePacket::new_from_immutable(
                            packet,
                            self.vote_source,
                            deprecate_legacy_vote_ixs,
                        )
                        .ok()
                    }),
                    true, // should_replenish_taken_votes
                );
            }
        }

        scanner.finalize().payload.reached_end_of_slot
    }

    fn clear(&mut self) {
        self.latest_unprocessed_votes.clear();
    }

    fn cache_epoch_boundary_info(&mut self, bank: &Bank) {
        if matches!(self.vote_source, VoteSource::Gossip) {
            panic!("Gossip vote thread should not be checking epoch boundary");
        }
        self.latest_unprocessed_votes
            .cache_epoch_boundary_info(bank);
    }
}

impl ThreadLocalUnprocessedPackets {
    fn is_empty(&self) -> bool {
        self.unprocessed_packet_batches.is_empty()
    }

    pub fn thread_type(&self) -> ThreadType {
        self.thread_type
    }

    fn len(&self) -> usize {
        self.unprocessed_packet_batches.len()
    }

    pub fn get_min_compute_unit_price(&self) -> Option<u64> {
        self.unprocessed_packet_batches.get_min_compute_unit_price()
    }

    pub fn get_max_compute_unit_price(&self) -> Option<u64> {
        self.unprocessed_packet_batches.get_max_compute_unit_price()
    }

    fn max_receive_size(&self) -> usize {
        self.unprocessed_packet_batches.capacity() - self.unprocessed_packet_batches.len()
    }

    #[cfg(test)]
    fn iter(&mut self) -> impl Iterator<Item = &DeserializedPacket> {
        self.unprocessed_packet_batches.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut DeserializedPacket> {
        self.unprocessed_packet_batches.iter_mut()
    }

    fn insert_batch(
        &mut self,
        deserialized_packets: Vec<ImmutableDeserializedPacket>,
    ) -> PacketBatchInsertionMetrics {
        self.unprocessed_packet_batches.insert_batch(
            deserialized_packets
                .into_iter()
                .map(DeserializedPacket::from_immutable_section),
        )
    }

    /// Take self.unprocessed_packet_batches's priority_queue out, leave empty MinMaxHeap in its place.
    fn take_priority_queue(&mut self) -> MinMaxHeap<Arc<ImmutableDeserializedPacket>> {
        std::mem::replace(
            &mut self.unprocessed_packet_batches.packet_priority_queue,
            MinMaxHeap::new(), // <-- no need to reserve capacity as we will replace this
        )
    }

    /// Verify that the priority queue and map are consistent and that original capacity is maintained.
    fn verify_priority_queue(&self, original_capacity: usize) {
        // Assert unprocessed queue is still consistent and maintains original capacity
        assert_eq!(
            self.unprocessed_packet_batches
                .packet_priority_queue
                .capacity(),
            original_capacity
        );
        assert_eq!(
            self.unprocessed_packet_batches.packet_priority_queue.len(),
            self.unprocessed_packet_batches
                .message_hash_to_transaction
                .len()
        );
    }

    fn collect_retained_packets(
        message_hash_to_transaction: &mut HashMap<Hash, DeserializedPacket>,
        packets_to_process: &[Arc<ImmutableDeserializedPacket>],
        retained_packet_indexes: &[usize],
    ) -> Vec<Arc<ImmutableDeserializedPacket>> {
        Self::remove_non_retained_packets(
            message_hash_to_transaction,
            packets_to_process,
            retained_packet_indexes,
        );
        retained_packet_indexes
            .iter()
            .map(|i| packets_to_process[*i].clone())
            .collect_vec()
    }

    /// remove packets from UnprocessedPacketBatches.message_hash_to_transaction after they have
    /// been removed from UnprocessedPacketBatches.packet_priority_queue
    fn remove_non_retained_packets(
        message_hash_to_transaction: &mut HashMap<Hash, DeserializedPacket>,
        packets_to_process: &[Arc<ImmutableDeserializedPacket>],
        retained_packet_indexes: &[usize],
    ) {
        filter_processed_packets(
            retained_packet_indexes
                .iter()
                .chain(std::iter::once(&packets_to_process.len())),
            |start, end| {
                for processed_packet in &packets_to_process[start..end] {
                    message_hash_to_transaction.remove(processed_packet.message_hash());
                }
            },
        )
    }

    // returns `true` if reached end of slot
    fn process_packets<F>(
        &mut self,
        bank: &Bank,
        banking_stage_stats: &BankingStageStats,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
        mut processing_function: F,
    ) -> bool
    where
        F: FnMut(
            &Vec<Arc<ImmutableDeserializedPacket>>,
            &mut ConsumeScannerPayload,
        ) -> Option<Vec<usize>>,
    {
        let mut retryable_packets = self.take_priority_queue();
        let original_capacity = retryable_packets.capacity();
        let mut new_retryable_packets = MinMaxHeap::with_capacity(original_capacity);
        let all_packets_to_process = retryable_packets.drain_desc().collect_vec();

        let should_process_packet =
            |packet: &Arc<ImmutableDeserializedPacket>, payload: &mut ConsumeScannerPayload| {
                consume_scan_should_process_packet(bank, banking_stage_stats, packet, payload)
            };
        let mut scanner = create_consume_multi_iterator(
            &all_packets_to_process,
            slot_metrics_tracker,
            &mut self.unprocessed_packet_batches.message_hash_to_transaction,
            should_process_packet,
        );

        while let Some((packets_to_process, payload)) = scanner.iterate() {
            let packets_to_process = packets_to_process
                .iter()
                .map(|p| (*p).clone())
                .collect_vec();
            let retryable_packets = if let Some(retryable_transaction_indexes) =
                processing_function(&packets_to_process, payload)
            {
                Self::collect_retained_packets(
                    payload.message_hash_to_transaction,
                    &packets_to_process,
                    &retryable_transaction_indexes,
                )
            } else {
                packets_to_process
            };

            new_retryable_packets.extend(retryable_packets);
        }

        let reached_end_of_slot = scanner.finalize().payload.reached_end_of_slot;

        self.unprocessed_packet_batches.packet_priority_queue = new_retryable_packets;
        self.verify_priority_queue(original_capacity);

        reached_end_of_slot
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        itertools::iproduct,
        solana_perf::packet::{Packet, PacketFlags},
        solana_runtime::genesis_utils,
        solana_sdk::{
            hash::Hash,
            signature::{Keypair, Signer},
            system_transaction,
        },
        solana_vote::vote_transaction::new_tower_sync_transaction,
        solana_vote_program::vote_state::TowerSync,
        std::error::Error,
    };

    #[test]
    fn test_filter_processed_packets() {
        let retryable_indexes = [0, 1, 2, 3];
        let mut non_retryable_indexes = vec![];
        let f = |start, end| {
            non_retryable_indexes.push((start, end));
        };
        filter_processed_packets(retryable_indexes.iter(), f);
        assert!(non_retryable_indexes.is_empty());

        let retryable_indexes = [0, 1, 2, 3, 5];
        let mut non_retryable_indexes = vec![];
        let f = |start, end| {
            non_retryable_indexes.push((start, end));
        };
        filter_processed_packets(retryable_indexes.iter(), f);
        assert_eq!(non_retryable_indexes, vec![(4, 5)]);

        let retryable_indexes = [1, 2, 3];
        let mut non_retryable_indexes = vec![];
        let f = |start, end| {
            non_retryable_indexes.push((start, end));
        };
        filter_processed_packets(retryable_indexes.iter(), f);
        assert_eq!(non_retryable_indexes, vec![(0, 1)]);

        let retryable_indexes = [1, 2, 3, 5];
        let mut non_retryable_indexes = vec![];
        let f = |start, end| {
            non_retryable_indexes.push((start, end));
        };
        filter_processed_packets(retryable_indexes.iter(), f);
        assert_eq!(non_retryable_indexes, vec![(0, 1), (4, 5)]);

        let retryable_indexes = [1, 2, 3, 5, 8];
        let mut non_retryable_indexes = vec![];
        let f = |start, end| {
            non_retryable_indexes.push((start, end));
        };
        filter_processed_packets(retryable_indexes.iter(), f);
        assert_eq!(non_retryable_indexes, vec![(0, 1), (4, 5), (6, 8)]);

        let retryable_indexes = [1, 2, 3, 5, 8, 8];
        let mut non_retryable_indexes = vec![];
        let f = |start, end| {
            non_retryable_indexes.push((start, end));
        };
        filter_processed_packets(retryable_indexes.iter(), f);
        assert_eq!(non_retryable_indexes, vec![(0, 1), (4, 5), (6, 8)]);
    }

    #[test]
    fn test_unprocessed_transaction_storage_insert() -> Result<(), Box<dyn Error>> {
        let keypair = Keypair::new();
        let vote_keypair = Keypair::new();
        let pubkey = solana_pubkey::new_rand();

        let small_transfer = Packet::from_data(
            None,
            system_transaction::transfer(&keypair, &pubkey, 1, Hash::new_unique()),
        )?;
        let mut vote = Packet::from_data(
            None,
            new_tower_sync_transaction(
                TowerSync::default(),
                Hash::new_unique(),
                &keypair,
                &vote_keypair,
                &vote_keypair,
                None,
            ),
        )?;
        vote.meta_mut().flags.set(PacketFlags::SIMPLE_VOTE_TX, true);
        let big_transfer = Packet::from_data(
            None,
            system_transaction::transfer(&keypair, &pubkey, 1000000, Hash::new_unique()),
        )?;

        for thread_type in [
            ThreadType::Transactions,
            ThreadType::Voting(VoteSource::Gossip),
            ThreadType::Voting(VoteSource::Tpu),
        ] {
            let mut transaction_storage = UnprocessedTransactionStorage::new_transaction_storage(
                UnprocessedPacketBatches::with_capacity(100),
                thread_type,
            );
            transaction_storage.insert_batch(vec![
                ImmutableDeserializedPacket::new(small_transfer.clone())?,
                ImmutableDeserializedPacket::new(vote.clone())?,
                ImmutableDeserializedPacket::new(big_transfer.clone())?,
            ]);
            let deserialized_packets = transaction_storage
                .iter()
                .map(|packet| packet.immutable_section().original_packet().clone())
                .collect_vec();
            assert_eq!(3, deserialized_packets.len());
            assert!(deserialized_packets.contains(&small_transfer));
            assert!(deserialized_packets.contains(&vote));
            assert!(deserialized_packets.contains(&big_transfer));
        }

        for (vote_source, staked) in iproduct!(
            [VoteSource::Gossip, VoteSource::Tpu].into_iter(),
            [true, false].into_iter()
        ) {
            let staked_keys = if staked {
                vec![vote_keypair.pubkey()]
            } else {
                vec![]
            };
            let latest_unprocessed_votes = LatestUnprocessedVotes::new_for_tests(&staked_keys);
            let mut transaction_storage = UnprocessedTransactionStorage::new_vote_storage(
                Arc::new(latest_unprocessed_votes),
                vote_source,
            );
            transaction_storage.insert_batch(vec![
                ImmutableDeserializedPacket::new(small_transfer.clone())?,
                ImmutableDeserializedPacket::new(vote.clone())?,
                ImmutableDeserializedPacket::new(big_transfer.clone())?,
            ]);
            assert_eq!(if staked { 1 } else { 0 }, transaction_storage.len());
        }
        Ok(())
    }

    #[test]
    fn test_process_packets_retryable_indexes_reinserted() -> Result<(), Box<dyn Error>> {
        let node_keypair = Keypair::new();
        let genesis_config =
            genesis_utils::create_genesis_config_with_leader(100, &node_keypair.pubkey(), 200)
                .genesis_config;
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let vote_keypair = Keypair::new();
        let mut vote = Packet::from_data(
            None,
            new_tower_sync_transaction(
                TowerSync::default(),
                Hash::new_unique(),
                &node_keypair,
                &vote_keypair,
                &vote_keypair,
                None,
            ),
        )?;
        vote.meta_mut().flags.set(PacketFlags::SIMPLE_VOTE_TX, true);

        let latest_unprocessed_votes =
            LatestUnprocessedVotes::new_for_tests(&[vote_keypair.pubkey()]);
        let mut transaction_storage = UnprocessedTransactionStorage::new_vote_storage(
            Arc::new(latest_unprocessed_votes),
            VoteSource::Tpu,
        );

        transaction_storage.insert_batch(vec![ImmutableDeserializedPacket::new(vote.clone())?]);
        assert_eq!(1, transaction_storage.len());

        // When processing packets, return all packets as retryable so that they
        // are reinserted into storage
        let _ = transaction_storage.process_packets(
            bank.clone(),
            &BankingStageStats::default(),
            &mut LeaderSlotMetricsTracker::new(0),
            |packets, _payload| {
                // Return all packets indexes as retryable
                Some(
                    packets
                        .iter()
                        .enumerate()
                        .map(|(index, _packet)| index)
                        .collect_vec(),
                )
            },
        );

        // All packets should remain in the transaction storage
        assert_eq!(1, transaction_storage.len());
        Ok(())
    }
}
