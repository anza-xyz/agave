use {
    super::{
        BankingStageStats, latest_validator_vote_packet::VoteSource,
        leader_slot_metrics::LeaderSlotMetricsTracker, vote_storage::VoteStorage,
    },
    crate::banking_stage::transaction_scheduler::transaction_state_container::SharedBytes,
    agave_banking_stage_ingress_types::BankingPacketReceiver,
    agave_transaction_view::{
        result::TransactionViewError, transaction_view::SanitizedTransactionView,
    },
    crossbeam_channel::RecvTimeoutError,
    solana_measure::{measure::Measure, measure_us},
    solana_pubkey::Pubkey,
    std::{
        collections::HashSet,
        num::Saturating,
        sync::{Arc, atomic::Ordering},
        time::{Duration, Instant},
    },
};

pub struct VotePacketReceiver {
    banking_packet_receiver: BankingPacketReceiver,
    filter_keys: Arc<HashSet<Pubkey>>,
}

impl VotePacketReceiver {
    pub fn new(
        banking_packet_receiver: BankingPacketReceiver,
        filter_keys: Arc<HashSet<Pubkey>>,
    ) -> Self {
        Self {
            banking_packet_receiver,
            filter_keys,
        }
    }

    /// Receive incoming packets, push into unprocessed buffer with packet indexes
    pub fn receive_and_buffer_packets(
        &mut self,
        vote_storage: &mut VoteStorage,
        banking_stage_stats: &mut BankingStageStats,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
        vote_source: VoteSource,
    ) -> Result<(), RecvTimeoutError> {
        let (result, recv_time_us) = measure_us!({
            let recv_timeout = Self::get_receive_timeout(vote_storage);
            let mut recv_and_buffer_measure = Measure::start("recv_and_buffer");
            self.receive_until(recv_timeout, vote_storage.max_receive_size())
                // Consumes results if Ok, otherwise we keep the Err
                .map(|(deserialized_packets, packet_stats)| {
                    self.buffer_packets(
                        deserialized_packets,
                        packet_stats,
                        vote_storage,
                        vote_source,
                        banking_stage_stats,
                        slot_metrics_tracker,
                    );
                    recv_and_buffer_measure.stop();

                    // Only incremented if packets are received
                    banking_stage_stats
                        .receive_and_buffer_packets_elapsed
                        .fetch_add(recv_and_buffer_measure.as_us(), Ordering::Relaxed);
                })
        });

        slot_metrics_tracker.increment_receive_and_buffer_packets_us(recv_time_us);

        result
    }

    // Copied from packet_deserializer.rs.
    fn receive_until(
        &self,
        recv_timeout: Duration,
        packet_count_upperbound: usize,
    ) -> Result<
        (
            Vec<SanitizedTransactionView<SharedBytes>>,
            PacketReceiverStats,
        ),
        RecvTimeoutError,
    > {
        let start = Instant::now();

        let packet_batches = self.banking_packet_receiver.recv_timeout(recv_timeout)?;
        let mut num_packets_received = packet_batches
            .iter()
            .map(|batch| batch.len())
            .sum::<usize>();
        let mut messages = vec![packet_batches];

        while let Ok(packet_batches) = self.banking_packet_receiver.try_recv() {
            num_packets_received += packet_batches
                .iter()
                .map(|batch| batch.len())
                .sum::<usize>();
            messages.push(packet_batches);

            if start.elapsed() >= recv_timeout || num_packets_received >= packet_count_upperbound {
                break;
            }
        }

        // Parse & collect transaction views.
        let mut packet_stats = PacketReceiverStats::default();
        let mut errors = Saturating::<usize>(0);
        let parsed_packets: Vec<_> = messages
            .iter()
            .flat_map(|batches| batches.iter())
            .flat_map(|batch| batch.iter())
            .filter_map(|pkt| {
                match SanitizedTransactionView::try_new_sanitized(
                    Arc::new(pkt.data(..)?.to_vec()),
                    // Vote instructions are created in the validator code, and they are not
                    // referencing more than 255 accounts, so it is safe to set this to true.
                    true,
                ) {
                    Ok(pkt) => {
                        if self.should_filter_packet(&pkt) {
                            packet_stats.filtered_account_key_count += 1;
                            None
                        } else {
                            Some(pkt)
                        }
                    }
                    Err(err) => {
                        errors += 1;
                        match err {
                            TransactionViewError::AddressLookupMismatch => {}
                            TransactionViewError::ParseError
                            | TransactionViewError::SanitizeError => {
                                packet_stats.failed_sanitization_count += 1
                            }
                        }

                        None
                    }
                }
            })
            .collect();
        let Saturating(errors) = errors;
        let filtered_account_key_count = packet_stats.filtered_account_key_count.0 as usize;
        let passed_sigverify_count = errors
            .saturating_add(parsed_packets.len())
            .saturating_add(filtered_account_key_count);
        packet_stats.passed_sigverify_count += passed_sigverify_count as u64;
        let failed_sigverify_count = num_packets_received
            .saturating_sub(parsed_packets.len())
            .saturating_sub(errors)
            .saturating_sub(filtered_account_key_count);
        packet_stats.failed_sigverify_count += failed_sigverify_count as u64;

        Ok((parsed_packets, packet_stats))
    }

    fn should_filter_packet(&self, packet: &SanitizedTransactionView<SharedBytes>) -> bool {
        // Vote transactions do not use address lookup tables, so static keys cover this path.
        !self.filter_keys.is_empty()
            && packet
                .static_account_keys()
                .iter()
                .any(|key| self.filter_keys.contains(key))
    }

    fn get_receive_timeout(vote_storage: &VoteStorage) -> Duration {
        if !vote_storage.is_empty() {
            // If there are buffered packets, run the equivalent of try_recv to try reading more
            // packets. This prevents starving BankingStage::consume_buffered_packets due to
            // buffered_packet_batches containing transactions that exceed the cost model for
            // the current bank.
            Duration::from_millis(0)
        } else {
            // Default wait time
            Duration::from_millis(100)
        }
    }

    fn buffer_packets(
        &self,
        deserialized_packets: Vec<SanitizedTransactionView<SharedBytes>>,
        packet_stats: PacketReceiverStats,
        vote_storage: &mut VoteStorage,
        vote_source: VoteSource,
        banking_stage_stats: &mut BankingStageStats,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
    ) {
        let packet_count = deserialized_packets.len();

        let filtered_account_key_count = packet_stats.filtered_account_key_count.0 as usize;
        slot_metrics_tracker.increment_received_packet_counts(packet_stats);

        let mut dropped_packets_count = Saturating(0);
        let mut newly_buffered_packets_count = 0;
        Self::push_unprocessed(
            vote_storage,
            vote_source,
            deserialized_packets,
            &mut dropped_packets_count,
            &mut newly_buffered_packets_count,
            banking_stage_stats,
            slot_metrics_tracker,
        );

        let vote_source_counts = match vote_source {
            VoteSource::Gossip => &banking_stage_stats.gossip_counts,
            VoteSource::Tpu => &banking_stage_stats.tpu_counts,
        };

        vote_source_counts
            .receive_and_buffer_packets_count
            .fetch_add(packet_count, Ordering::Relaxed);
        {
            let Saturating(dropped_packets_count) = dropped_packets_count;
            vote_source_counts.dropped_packets_count.fetch_add(
                dropped_packets_count.saturating_add(filtered_account_key_count),
                Ordering::Relaxed,
            );
        }
        vote_source_counts
            .newly_buffered_packets_count
            .fetch_add(newly_buffered_packets_count, Ordering::Relaxed);
        banking_stage_stats
            .current_buffered_packets_count
            .swap(vote_storage.len(), Ordering::Relaxed);
    }

    fn push_unprocessed(
        vote_storage: &mut VoteStorage,
        vote_source: VoteSource,
        deserialized_packets: Vec<SanitizedTransactionView<SharedBytes>>,
        dropped_packets_count: &mut Saturating<usize>,
        newly_buffered_packets_count: &mut usize,
        banking_stage_stats: &mut BankingStageStats,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
    ) {
        if !deserialized_packets.is_empty() {
            let _ = banking_stage_stats
                .batch_packet_indexes_len
                .increment(deserialized_packets.len() as u64);

            *newly_buffered_packets_count += deserialized_packets.len();
            slot_metrics_tracker
                .increment_newly_buffered_packets_count(deserialized_packets.len() as u64);

            let vote_batch_insertion_metrics =
                vote_storage.insert_batch(vote_source, deserialized_packets.into_iter());
            slot_metrics_tracker
                .accumulate_vote_batch_insertion_metrics(&vote_batch_insertion_metrics);
            *dropped_packets_count += vote_batch_insertion_metrics.total_dropped_packets();
        }
    }
}

#[derive(Default, Debug, PartialEq)]
pub struct PacketReceiverStats {
    /// Number of packets passing sigverify
    pub passed_sigverify_count: Saturating<u64>,
    /// Number of packets failing sigverify
    pub failed_sigverify_count: Saturating<u64>,
    /// Number of packets dropped due to sanitization error
    pub failed_sanitization_count: Saturating<u64>,
    /// Number of packets dropped due to prioritization error
    pub failed_prioritization_count: Saturating<u64>,
    /// Number of vote packets dropped
    pub invalid_vote_count: Saturating<u64>,
    /// Number of vote packets dropped due to account key filtering.
    pub filtered_account_key_count: Saturating<u64>,
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::banking_stage::{
            BankingStageStats,
            latest_validator_vote_packet::VoteSource,
            leader_slot_metrics::LeaderSlotMetricsTracker,
            vote_storage::{VoteStorage, tests::packet_from_slots},
        },
        crossbeam_channel::unbounded,
        solana_perf::packet::PacketBatch,
        solana_runtime::{
            bank::Bank,
            genesis_utils::{self, ValidatorVoteKeypairs},
        },
        solana_signer::Signer,
    };

    fn receive_vote_with_filter_keys(
        keypairs: &ValidatorVoteKeypairs,
        filter_keys: Arc<HashSet<Pubkey>>,
    ) -> VoteStorage {
        let vote_packet = packet_from_slots(vec![(1, 1)], keypairs, None);
        let (sender, receiver) = unbounded();
        sender
            .send(Arc::new(vec![PacketBatch::from(vec![vote_packet])]))
            .unwrap();

        let mut receiver = VotePacketReceiver::new(receiver, filter_keys);
        let genesis_config =
            genesis_utils::create_genesis_config_with_vote_accounts(100, &[keypairs], vec![200])
                .genesis_config;
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let mut vote_storage = VoteStorage::new(&bank);
        let mut banking_stage_stats = BankingStageStats::new();
        let mut slot_metrics_tracker = LeaderSlotMetricsTracker::default();

        receiver
            .receive_and_buffer_packets(
                &mut vote_storage,
                &mut banking_stage_stats,
                &mut slot_metrics_tracker,
                VoteSource::Tpu,
            )
            .unwrap();

        vote_storage
    }

    #[test]
    fn test_receive_and_buffer_filters_vote_account_key() {
        let keypairs = ValidatorVoteKeypairs::new_rand();
        let vote_pubkey = keypairs.vote_keypair.pubkey();
        let vote_storage =
            receive_vote_with_filter_keys(&keypairs, Arc::new(HashSet::from([vote_pubkey])));

        assert_eq!(vote_storage.len(), 0);
    }

    #[test]
    fn test_receive_and_buffer_does_not_filter_unmatched_vote_account_key() {
        let keypairs = ValidatorVoteKeypairs::new_rand();
        let vote_storage = receive_vote_with_filter_keys(
            &keypairs,
            Arc::new(HashSet::from([Pubkey::new_unique()])),
        );

        assert_eq!(vote_storage.len(), 1);
    }
}
