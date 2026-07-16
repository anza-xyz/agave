use {
    super::{
        BankingStageStats, latest_validator_vote_packet::VoteSource,
        leader_slot_metrics::LeaderSlotMetricsTracker, vote_storage::VoteStorage,
    },
    crate::banking_stage::transaction_scheduler::transaction_state_container::SharedBytes,
    agave_banking_stage_ingress_types::{BankingPacketBatch, BankingPacketReceiver},
    agave_transaction_view::{
        result::TransactionViewError, sanitize::SanitizeConfig,
        transaction_view::SanitizedTransactionView,
    },
    crossbeam_channel::RecvTimeoutError,
    solana_measure::{measure::Measure, measure_us},
    solana_pubkey::Pubkey,
    solana_runtime_transaction::sanitize_config::sanitize_config,
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
            self.receive_until_and_buffer(
                recv_timeout,
                vote_storage.max_receive_size(),
                vote_storage,
                vote_source,
                banking_stage_stats,
                slot_metrics_tracker,
            )
            .map(|()| {
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

    fn receive_until_and_buffer(
        &self,
        recv_timeout: Duration,
        packet_count_upperbound: usize,
        vote_storage: &mut VoteStorage,
        vote_source: VoteSource,
        banking_stage_stats: &mut BankingStageStats,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
    ) -> Result<(), RecvTimeoutError> {
        let start = Instant::now();
        let sanitize_config = sanitize_config();
        let mut stats = ReceiveAndBufferStats::default();

        let packet_batch = self.banking_packet_receiver.recv_timeout(recv_timeout)?;
        self.buffer_packet_batch(
            &packet_batch,
            &sanitize_config,
            vote_storage,
            vote_source,
            slot_metrics_tracker,
            &mut stats,
        );

        while let Ok(packet_batch) = self.banking_packet_receiver.try_recv() {
            self.buffer_packet_batch(
                &packet_batch,
                &sanitize_config,
                vote_storage,
                vote_source,
                slot_metrics_tracker,
                &mut stats,
            );

            if start.elapsed() >= recv_timeout
                || stats.num_packets_received >= packet_count_upperbound
            {
                break;
            }
        }

        self.update_receive_stats(
            stats,
            vote_storage,
            vote_source,
            banking_stage_stats,
            slot_metrics_tracker,
        );

        Ok(())
    }

    fn buffer_packet_batch(
        &self,
        packet_batch: &BankingPacketBatch,
        sanitize_config: &SanitizeConfig,
        vote_storage: &mut VoteStorage,
        vote_source: VoteSource,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
        stats: &mut ReceiveAndBufferStats,
    ) {
        stats.num_packets_received += packet_batch.len();

        for packet in packet_batch.iter() {
            let Some(packet_data) = packet.data(..) else {
                continue;
            };

            match SanitizedTransactionView::try_new_sanitized(
                Arc::new(packet_data.to_vec()),
                sanitize_config,
            ) {
                Ok(packet) => {
                    if self.should_filter_packet(&packet) {
                        stats.packet_stats.filtered_account_key_count += 1;
                        continue;
                    }

                    stats.num_buffered_packets += 1;
                    let vote_insertion_metrics = vote_storage.insert_packet(vote_source, packet);
                    slot_metrics_tracker.accumulate_vote_insertion_metrics(&vote_insertion_metrics);
                    stats.dropped_packets_count += vote_insertion_metrics.total_dropped_packets();
                }
                Err(err) => {
                    stats.errors += 1;
                    match err {
                        TransactionViewError::AddressLookupMismatch => {}
                        TransactionViewError::ParseError | TransactionViewError::SanitizeError => {
                            stats.packet_stats.failed_sanitization_count += 1
                        }
                    }
                }
            }
        }
    }

    fn update_receive_stats(
        &self,
        mut stats: ReceiveAndBufferStats,
        vote_storage: &VoteStorage,
        vote_source: VoteSource,
        banking_stage_stats: &mut BankingStageStats,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
    ) {
        let filtered_account_key_count = stats.packet_stats.filtered_account_key_count.0 as usize;
        let Saturating(errors) = stats.errors;
        let passed_sigverify_count = errors
            .saturating_add(stats.num_buffered_packets)
            .saturating_add(filtered_account_key_count);
        stats.packet_stats.passed_sigverify_count += passed_sigverify_count as u64;
        let failed_sigverify_count = stats
            .num_packets_received
            .saturating_sub(stats.num_buffered_packets)
            .saturating_sub(errors)
            .saturating_sub(filtered_account_key_count);
        stats.packet_stats.failed_sigverify_count += failed_sigverify_count as u64;

        let vote_source_counts = match vote_source {
            VoteSource::Gossip => &banking_stage_stats.gossip_counts,
            VoteSource::Tpu => &banking_stage_stats.tpu_counts,
        };

        vote_source_counts
            .receive_and_buffer_packets_count
            .fetch_add(stats.num_buffered_packets, Ordering::Relaxed);
        {
            let Saturating(dropped_packets_count) = stats.dropped_packets_count;
            vote_source_counts.dropped_packets_count.fetch_add(
                dropped_packets_count.saturating_add(filtered_account_key_count),
                Ordering::Relaxed,
            );
        }
        vote_source_counts
            .newly_buffered_packets_count
            .fetch_add(stats.num_buffered_packets, Ordering::Relaxed);
        banking_stage_stats
            .current_buffered_packets_count
            .swap(vote_storage.len(), Ordering::Relaxed);

        if stats.num_buffered_packets != 0 {
            let _ = banking_stage_stats
                .batch_packet_indexes_len
                .increment(stats.num_buffered_packets as u64);
            slot_metrics_tracker
                .increment_newly_buffered_packets_count(stats.num_buffered_packets as u64);
        }
        slot_metrics_tracker.increment_received_packet_counts(stats.packet_stats);
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
            Duration::from_millis(10)
        }
    }
}

#[derive(Default)]
struct ReceiveAndBufferStats {
    packet_stats: PacketReceiverStats,
    num_packets_received: usize,
    num_buffered_packets: usize,
    dropped_packets_count: Saturating<usize>,
    errors: Saturating<usize>,
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
        crossbeam_channel::bounded,
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
        let (sender, receiver) = bounded(1024);
        sender
            .send(Arc::new(PacketBatch::from(vec![vote_packet])))
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
