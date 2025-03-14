use {
    super::{
        consumer::Consumer,
        immutable_deserialized_packet::ImmutableDeserializedPacket,
        latest_unprocessed_votes::{
            LatestUnprocessedVotes, LatestValidatorVotePacket, VoteBatchInsertionMetrics,
            VoteSource,
        },
        leader_slot_metrics::LeaderSlotMetricsTracker,
        read_write_account_set::ReadWriteAccountSet,
        BankingStageStats,
    },
    solana_accounts_db::account_locks::validate_account_locks,
    solana_measure::measure_us,
    solana_runtime::bank::Bank,
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_sdk::transaction::SanitizedTransaction,
    solana_svm::transaction_error_metrics::TransactionErrorMetrics,
    std::sync::{atomic::Ordering, Arc},
};

// Step-size set to be 64, equal to the maximum batch/entry size. With the
// multi-iterator change, there's no point in getting larger batches of
// non-conflicting transactions.
pub const UNPROCESSED_BUFFER_STEP_SIZE: usize = 64;
/// Maximum number of votes a single receive call will accept
const MAX_NUM_VOTES_RECEIVE: usize = 10_000;

#[derive(Debug)]
pub struct VoteStorage {
    latest_unprocessed_votes: Arc<LatestUnprocessedVotes>,
    vote_source: VoteSource,
    already_handled: Vec<bool>,
}

/// Output from the element checker used in `MultiIteratorScanner::iterate`.
#[derive(Debug)]
pub enum ProcessingDecision {
    /// Should be processed by the scanner.
    Now,
    /// Should be skipped and marked as handled so we don't try processing it again.
    Never,
}

/// Convenient wrapper for shared-state between banking stage processing and the
/// multi-iterator checking function.
pub struct ConsumeScannerPayload<'a> {
    pub reached_end_of_slot: bool,
    pub account_locks: ReadWriteAccountSet,
    pub sanitized_transactions: Vec<RuntimeTransaction<SanitizedTransaction>>,
    pub slot_metrics_tracker: &'a mut LeaderSlotMetricsTracker,
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
            return ProcessingDecision::Never;
        }

        payload.sanitized_transactions.push(sanitized_transaction);
        ProcessingDecision::Now
    } else {
        ProcessingDecision::Never
    }
}

pub fn get_payload<'b>(
    slot_metrics_tracker: &'b mut LeaderSlotMetricsTracker,
) -> ConsumeScannerPayload<'b> {
    let payload = ConsumeScannerPayload {
        reached_end_of_slot: false,
        account_locks: ReadWriteAccountSet::default(),
        sanitized_transactions: Vec::with_capacity(UNPROCESSED_BUFFER_STEP_SIZE),
        slot_metrics_tracker,
        error_counters: TransactionErrorMetrics::default(),
    };
    payload
}

impl VoteStorage {
    pub fn new(
        latest_unprocessed_votes: Arc<LatestUnprocessedVotes>,
        vote_source: VoteSource,
    ) -> Self {
        Self {
            latest_unprocessed_votes,
            vote_source,
            already_handled: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.latest_unprocessed_votes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.latest_unprocessed_votes.len()
    }

    pub fn max_receive_size(&self) -> usize {
        MAX_NUM_VOTES_RECEIVE
    }

    pub(crate) fn insert_batch(
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

    pub fn should_process_packet(
        &self,
        packet: &Arc<ImmutableDeserializedPacket>,
        payload: &mut ConsumeScannerPayload,
        bank: Arc<Bank>,
        banking_stage_stats: &BankingStageStats,
    ) -> ProcessingDecision {
        consume_scan_should_process_packet(&bank, banking_stage_stats, packet, payload)
    }

    // returns `true` if the end of slot is reached
    pub fn process_packets<F>(
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

        // Based on the stake distribution present in the supplied bank, drain the unprocessed votes
        // from each validator using a weighted random ordering. Votes from validators with
        // 0 stake are ignored.
        let all_vote_packets = self
            .latest_unprocessed_votes
            .drain_unprocessed(bank.clone());

        let deprecate_legacy_vote_ixs = self
            .latest_unprocessed_votes
            .should_deprecate_legacy_vote_ixs();

        let mut payload = get_payload(slot_metrics_tracker);
        self.already_handled.clear();
        self.already_handled.resize(all_vote_packets.len(), false);
        let mut starting_index = 0;
        loop {
            let (last_found, payload, vote_packets) = self.march_iterator(
                starting_index,
                &all_vote_packets,
                &mut payload,
                bank.clone(),
                banking_stage_stats,
            );
            if vote_packets.is_empty() || last_found.is_none() {
                break;
            }
            starting_index = last_found.unwrap() + 1;

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

        payload.reached_end_of_slot
    }

    pub fn clear(&mut self) {
        self.latest_unprocessed_votes.clear();
    }

    pub fn cache_epoch_boundary_info(&mut self, bank: &Bank) {
        if matches!(self.vote_source, VoteSource::Gossip) {
            panic!("Gossip vote thread should not be checking epoch boundary");
        }
        self.latest_unprocessed_votes
            .cache_epoch_boundary_info(bank);
    }

    pub fn should_not_process(&self) -> bool {
        // The gossip vote thread does not need to process or forward any votes, that is
        // handled by the tpu vote thread
        matches!(self.vote_source, VoteSource::Gossip)
    }

    /// Processes UNPROCESSED_BUFFER_STEP_SIZE packets at a time, returning the index of the first
    /// packet that should be processed, the updated payload, and the packets that should be
    /// processed.
    pub fn march_iterator<'a, 'b>(
        &mut self,
        starting_index: usize,
        packet: &Vec<Arc<ImmutableDeserializedPacket>>,
        payload: &'b mut ConsumeScannerPayload<'a>,
        bank: Arc<Bank>,
        banking_stage_stats: &BankingStageStats,
    ) -> (
        Option<usize>,
        &'b mut ConsumeScannerPayload<'a>,
        Vec<Arc<ImmutableDeserializedPacket>>,
    ) {
        let mut last_found = None;
        let mut current_items: Vec<Arc<ImmutableDeserializedPacket>> =
            Vec::with_capacity(UNPROCESSED_BUFFER_STEP_SIZE);
        for _ in 0..UNPROCESSED_BUFFER_STEP_SIZE {
            for index in starting_index..packet.len() {
                if !self.already_handled[index] {
                    match self.should_process_packet(
                        &packet[index],
                        payload,
                        bank.clone(),
                        &banking_stage_stats,
                    ) {
                        ProcessingDecision::Now => {
                            last_found = Some(index);
                            self.already_handled[index] = true;
                            current_items.push(packet[index].clone());
                            break;
                        }
                        ProcessingDecision::Never => {
                            self.already_handled[index] = true;
                        }
                    }
                }
            }
        }
        (last_found, payload, current_items)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        itertools::Itertools,
        solana_perf::packet::{Packet, PacketFlags},
        solana_runtime::genesis_utils,
        solana_sdk::{
            hash::Hash,
            signature::{Keypair, Signer},
        },
        solana_vote::vote_transaction::new_tower_sync_transaction,
        solana_vote_program::vote_state::TowerSync,
        std::error::Error,
    };

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
        let mut transaction_storage =
            VoteStorage::new(Arc::new(latest_unprocessed_votes), VoteSource::Tpu);

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

    #[test]
    fn test_march_iterator() -> Result<(), Box<dyn Error>> {
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
        let mut transaction_storage =
            VoteStorage::new(Arc::new(latest_unprocessed_votes), VoteSource::Tpu);

        transaction_storage.insert_batch(vec![ImmutableDeserializedPacket::new(vote.clone())?]);
        assert_eq!(1, transaction_storage.len());

        let mut slot_metrics_tracker = LeaderSlotMetricsTracker::new(0);
        let immutable_packet = Arc::new(ImmutableDeserializedPacket::new(vote.clone())?);
        let mut payload = get_payload(&mut slot_metrics_tracker);
        transaction_storage.already_handled = vec![false];

        let (found, _payload, packets) = transaction_storage.march_iterator(
            0,
            &vec![immutable_packet.clone()],
            &mut payload,
            bank.clone(),
            &BankingStageStats::default(),
        );

        assert!(found.is_some());
        assert_eq!(packets.len(), 1);
        Ok(())
    }
}
