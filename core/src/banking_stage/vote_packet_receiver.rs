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
    std::{
        num::Saturating,
        sync::{Arc, atomic::Ordering},
        time::{Duration, Instant},
    },
};

pub struct VotePacketReceiver {
    banking_packet_receiver: BankingPacketReceiver,
}

impl VotePacketReceiver {
    pub fn new(banking_packet_receiver: BankingPacketReceiver) -> Self {
        Self {
            banking_packet_receiver,
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
                    // NB: It's safe to always pass false in here as simple vote
                    // transactions are guaranteed to be a single instruction.
                    false,
                    // Vote instructions are created in the validator code, and they are not
                    // referencing more than 255 accounts, so it is safe to set this to true.
                    true,
                ) {
                    Ok(pkt) => Some(pkt),
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
        packet_stats.passed_sigverify_count += errors.saturating_add(parsed_packets.len()) as u64;
        packet_stats.failed_sigverify_count += num_packets_received
            .saturating_sub(parsed_packets.len())
            .saturating_sub(errors) as u64;

        Ok((parsed_packets, packet_stats))
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
            vote_source_counts
                .dropped_packets_count
                .fetch_add(dropped_packets_count, Ordering::Relaxed);
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
}
