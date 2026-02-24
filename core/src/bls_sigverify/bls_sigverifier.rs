//! The BLS signature verifier.

use {
    super::{
        bls_cert_sigverify::verify_and_send_certificates,
        bls_vote_sigverify::{VoteToVerify, verify_and_send_votes},
        errors::SigVerifyError,
        stats::{SigVerifierStats, StreamerRecvStats},
    },
    crate::cluster_info_vote_listener::VerifiedVoterSlotsSender,
    agave_votor::{
        consensus_metrics::ConsensusMetricsEventSender,
        consensus_rewards::{self},
    },
    agave_votor_messages::{
        consensus_message::{Certificate, CertificateType, ConsensusMessage, VoteMessage},
        migration::MigrationStatus,
        reward_certificate::AddVoteMessage,
    },
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender},
    rayon::{ThreadPool, ThreadPoolBuilder},
    solana_bls_signatures::pubkey::Pubkey as BlsPubkey,
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_measure::measure_us,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::SharableBanks},
    solana_streamer::{
        packet::PacketBatch,
        streamer::{self, StreamerError},
    },
    std::{
        collections::HashSet,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder},
    },
};

/// Starts the BLS sigverifier service in its own dedicated thread.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_service(
    exit: Arc<AtomicBool>,
    migration_status: Arc<MigrationStatus>,
    packet_receiver: Receiver<PacketBatch>,
    sharable_banks: SharableBanks,
    channel_to_repair: VerifiedVoterSlotsSender,
    channel_to_reward: Sender<AddVoteMessage>,
    channel_to_pool: Sender<Vec<ConsensusMessage>>,
    channel_to_metrics: ConsensusMetricsEventSender,
    cluster_info: Arc<ClusterInfo>,
    leader_schedule: Arc<LeaderScheduleCache>,
    num_threads: usize,
) -> thread::JoinHandle<()> {
    let verifier = SigVerifier::new(
        migration_status,
        sharable_banks,
        channel_to_repair,
        channel_to_reward,
        channel_to_pool,
        channel_to_metrics,
        cluster_info,
        leader_schedule,
        num_threads,
    );

    Builder::new()
        .name("solSigVerBLS".to_string())
        .spawn(move || verifier.run(exit, packet_receiver))
        .unwrap()
}

struct SigVerifier {
    migration_status: Arc<MigrationStatus>,
    /// Channel to send msgs to repair on.
    channel_to_repair: VerifiedVoterSlotsSender,
    /// Channel to send msgs to consensus rewards container to.
    channel_to_reward: Sender<AddVoteMessage>,
    /// Channel to send msgs to consensus pool to.
    channel_to_pool: Sender<Vec<ConsensusMessage>>,
    /// Channel to send msgs to consensus metrics container to.
    channel_to_metrics: ConsensusMetricsEventSender,
    /// Container to look up root banks from.
    sharable_banks: SharableBanks,
    stats: SigVerifierStats,
    /// Set of recently verified certs to avoid duplicate work.
    verified_certs: HashSet<CertificateType>,
    /// Tracks when the cache was last pruned.
    last_checked_root_slot: Slot,
    cluster_info: Arc<ClusterInfo>,
    leader_schedule: Arc<LeaderScheduleCache>,
    /// Buffer to collect votes to verify.  Stored here to reduce reallocations.
    votes_buffer: Vec<VoteToVerify>,
    /// thread pool to use for all parallel tasks
    thread_pool: ThreadPool,
}

impl SigVerifier {
    fn new(
        migration_status: Arc<MigrationStatus>,
        sharable_banks: SharableBanks,
        channel_to_repair: VerifiedVoterSlotsSender,
        channel_to_reward: Sender<AddVoteMessage>,
        channel_to_pool: Sender<Vec<ConsensusMessage>>,
        channel_to_metrics: ConsensusMetricsEventSender,
        cluster_info: Arc<ClusterInfo>,
        leader_schedule: Arc<LeaderScheduleCache>,
        num_threads: usize,
    ) -> Self {
        let thread_pool = ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .thread_name(|i| format!("solSigVerBLS{i:02}"))
            .build()
            .unwrap();
        Self {
            migration_status,
            sharable_banks,
            channel_to_repair,
            channel_to_reward,
            channel_to_pool,
            stats: SigVerifierStats::default(),
            verified_certs: HashSet::new(),
            channel_to_metrics,
            last_checked_root_slot: 0,
            cluster_info,
            leader_schedule,
            votes_buffer: Vec::new(),
            thread_pool,
        }
    }

    fn run(mut self, exit: Arc<AtomicBool>, packet_receiver: Receiver<PacketBatch>) {
        const SOFT_RECEIVE_CAP: usize = 5_000;
        let mut stats = StreamerRecvStats::default();
        while !exit.load(Ordering::Relaxed) {
            let (batches, num_packets, recv_duration) =
                match streamer::recv_packet_batches(&packet_receiver, SOFT_RECEIVE_CAP) {
                    Ok(res) => res,
                    Err(e) => match e {
                        StreamerError::RecvTimeout(RecvTimeoutError::Disconnected) => break,
                        StreamerError::RecvTimeout(RecvTimeoutError::Timeout) => continue,
                        _ => {
                            error!("Error receiving packets: {e:?}");
                            continue;
                        }
                    },
                };

            // Ignore packets if alpenglow is not active yet.
            if self.migration_status.is_pre_feature_activation() {
                continue;
            }

            let num_batches = batches.len();
            let (verify_res, verify_time_us) = measure_us!(self.verify_and_send_batches(batches));

            stats
                .streamer_recv_us
                .increment(recv_duration.as_micros().try_into().unwrap_or(u64::MAX))
                .unwrap();
            stats.num_batches.increment(num_batches as u64).unwrap();
            stats.num_packets.increment(num_packets as u64).unwrap();
            stats
                .verify_and_send_batches_us
                .increment(verify_time_us)
                .unwrap();
            stats.maybe_report();

            if let Err(e) = verify_res {
                error!("verify_and_send_batches() failed with {e}. Exiting.");
                break;
            }
        }
    }

    fn verify_and_send_batches(&mut self, batches: Vec<PacketBatch>) -> Result<(), SigVerifyError> {
        let root_bank = self.sharable_banks.root();
        self.maybe_prune_caches(root_bank.slot());

        let votes_buffer = std::mem::take(&mut self.votes_buffer);
        let ((certs_to_verify, votes_to_verify), extract_msgs_us) =
            measure_us!(self.extract_and_filter_msgs(batches, &root_bank, votes_buffer));
        self.stats
            .extract_filter_msgs_us
            .increment(extract_msgs_us)
            .unwrap();

        let (votes_result, certs_result) = self.thread_pool.join(
            || {
                verify_and_send_votes(
                    votes_to_verify,
                    &root_bank,
                    &self.cluster_info,
                    &self.leader_schedule,
                    &self.channel_to_pool,
                    &self.channel_to_repair,
                    &self.channel_to_reward,
                    &self.channel_to_metrics,
                )
            },
            || {
                verify_and_send_certificates(
                    &mut self.verified_certs,
                    certs_to_verify,
                    &root_bank,
                    &self.channel_to_pool,
                )
            },
        );

        let (votes_buffer, vote_stats) = votes_result?;
        self.votes_buffer = votes_buffer;
        let cert_stats = certs_result?;

        self.stats.vote_stats.merge(vote_stats);
        self.stats.cert_stats.merge(cert_stats);
        self.stats.maybe_report();
        Ok(())
    }

    fn maybe_prune_caches(&mut self, root_slot: Slot) {
        if self.last_checked_root_slot < root_slot {
            self.last_checked_root_slot = root_slot;
            self.verified_certs.retain(|cert| cert.slot() > root_slot);
        }
    }

    fn extract_and_filter_msgs(
        &mut self,
        batches: Vec<PacketBatch>,
        root_bank: &Bank,
        mut votes_buffer: Vec<VoteToVerify>,
    ) -> (Vec<Certificate>, Vec<VoteToVerify>) {
        let root_slot = root_bank.slot();
        let mut certs = Vec::new();
        votes_buffer.clear();
        for packet in batches.iter().flatten() {
            self.stats.num_pkts_received += 1;
            if packet.meta().discard() {
                self.stats.num_discarded_pkts += 1;
                continue;
            }
            let Ok(msg) = packet.deserialize_slice::<ConsensusMessage, _>(..) else {
                self.stats.num_malformed_pkts += 1;
                continue;
            };
            match msg {
                ConsensusMessage::Vote(vote) => {
                    if let Some((pubkey, bls_pubkey)) = self.keep_vote(&vote, root_bank) {
                        votes_buffer.push(VoteToVerify {
                            vote_message: vote,
                            bls_pubkey,
                            pubkey,
                        });
                    }
                }
                ConsensusMessage::Certificate(cert) => {
                    if cert.cert_type.slot() <= root_slot {
                        self.stats.num_old_certs_received += 1;
                        continue;
                    }
                    if self.verified_certs.contains(&cert.cert_type) {
                        self.stats.num_verified_certs_received += 1;
                        continue;
                    }
                    certs.push(cert);
                }
            }
        }
        (certs, votes_buffer)
    }

    /// If this vote should be verified, then returns the sender's Pubkey and BlsPubkey.
    fn keep_vote(&mut self, vote: &VoteMessage, root_bank: &Bank) -> Option<(Pubkey, BlsPubkey)> {
        let root_slot = root_bank.slot();
        let Some(rank_map) = root_bank.get_rank_map(vote.vote.slot()) else {
            self.stats.discard_vote_no_epoch_stakes += 1;
            return None;
        };
        let entry = rank_map
            .get_pubkey_stake_entry(vote.rank.into())
            .or_else(|| {
                self.stats.discard_vote_invalid_rank += 1;
                None
            })?;
        let ret = Some((entry.pubkey, entry.bls_pubkey));
        if vote.vote.slot() > root_slot {
            return ret;
        }
        if consensus_rewards::wants_vote(&self.cluster_info, &self.leader_schedule, root_slot, vote)
        {
            return ret;
        }
        self.stats.num_old_votes_received += 1;
        None
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            bls_sigverify::stats::STATS_INTERVAL_DURATION,
            cluster_info_vote_listener::VerifiedVoterSlotsReceiver,
        },
        agave_votor::consensus_pool::certificate_builder::CertificateBuilder,
        agave_votor_messages::{
            consensus_message::{Certificate, CertificateType, ConsensusMessage, VoteMessage},
            vote::Vote,
        },
        bitvec::prelude::{BitVec, Lsb0},
        crossbeam_channel::{Receiver, TryRecvError},
        solana_bls_signatures::{Signature, Signature as BLSSignature},
        solana_gossip::contact_info::ContactInfo,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_net_utils::SocketAddrSpace,
        solana_perf::packet::{Packet, RecycledPacketBatch},
        solana_runtime::{
            bank::Bank,
            bank_forks::BankForks,
            genesis_utils::{
                ValidatorVoteKeypairs, create_genesis_config_with_alpenglow_vote_accounts,
            },
        },
        solana_signer::Signer,
        solana_signer_store::encode_base2,
        std::time::Instant,
    };

    fn create_keypairs_and_bls_sig_verifier_with_channels(
        votes_for_repair_sender: VerifiedVoterSlotsSender,
        message_sender: Sender<Vec<ConsensusMessage>>,
        consensus_metrics_sender: ConsensusMetricsEventSender,
        reward_votes_sender: Sender<AddVoteMessage>,
    ) -> (Vec<ValidatorVoteKeypairs>, SigVerifier) {
        // Create 10 node validatorvotekeypairs vec
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let stakes_vec = (0..validator_keypairs.len())
            .map(|i| 1_000 - i as u64)
            .collect::<Vec<_>>();
        let genesis = create_genesis_config_with_alpenglow_vote_accounts(
            1_000_000_000,
            &validator_keypairs,
            stakes_vec,
        );
        let bank0 = Bank::new_for_tests(&genesis.genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank0);
        let sharable_banks = bank_forks.read().unwrap().sharable_banks();
        let keypair = Keypair::new();
        let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), 0);
        let cluster_info = Arc::new(ClusterInfo::new(
            contact_info,
            Arc::new(keypair),
            SocketAddrSpace::Unspecified,
        ));
        let leader_schedule = Arc::new(LeaderScheduleCache::new_from_bank(&sharable_banks.root()));
        (
            validator_keypairs,
            SigVerifier::new(
                Arc::new(MigrationStatus::default()),
                sharable_banks,
                votes_for_repair_sender,
                reward_votes_sender,
                message_sender,
                consensus_metrics_sender,
                cluster_info,
                leader_schedule,
                4,
            ),
        )
    }

    fn create_keypairs_and_bls_sig_verifier() -> (
        Vec<ValidatorVoteKeypairs>,
        SigVerifier,
        VerifiedVoterSlotsReceiver,
        Receiver<Vec<ConsensusMessage>>,
    ) {
        let (votes_for_repair_sender, votes_for_repair_receiver) = crossbeam_channel::unbounded();
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (consensus_metrics_sender, consensus_metrics_receiver) = crossbeam_channel::unbounded();
        let (reward_votes_sender, reward_votes_receiver) = crossbeam_channel::unbounded();
        // the sigverifier sends msgs on some channels which the tests do not inspect.
        // use a thread to keep the receive side of these channels alive so that the sending of msgs doesn't fail.
        // the thread does not need to be joined and will exit when the sigverifier is dropped.
        std::thread::spawn(move || {
            while consensus_metrics_receiver.recv().is_ok() {}
            while reward_votes_receiver.recv().is_ok() {}
        });
        let (keypairs, verifier) = create_keypairs_and_bls_sig_verifier_with_channels(
            votes_for_repair_sender,
            message_sender,
            consensus_metrics_sender,
            reward_votes_sender,
        );
        (
            keypairs,
            verifier,
            votes_for_repair_receiver,
            message_receiver,
        )
    }

    fn create_signed_vote_message(
        validator_keypairs: &[ValidatorVoteKeypairs],
        vote: Vote,
        rank: usize,
    ) -> VoteMessage {
        let bls_keypair = &validator_keypairs[rank].bls_keypair;
        let payload = bincode::serialize(&vote).expect("Failed to serialize vote");
        let signature: BLSSignature = bls_keypair.sign(&payload).into();
        VoteMessage {
            vote,
            signature,
            rank: rank as u16,
        }
    }

    fn create_signed_certificate_message(
        validator_keypairs: &[ValidatorVoteKeypairs],
        cert_type: CertificateType,
        ranks: &[usize],
    ) -> Certificate {
        let mut builder = CertificateBuilder::new(cert_type);
        // Assumes Base2 encoding (single vote type) for simplicity in this helper.
        let vote = cert_type.to_source_vote();
        let vote_messages: Vec<VoteMessage> = ranks
            .iter()
            .map(|&rank| create_signed_vote_message(validator_keypairs, vote, rank))
            .collect();

        builder
            .aggregate(&vote_messages)
            .expect("Failed to aggregate votes");
        builder.build().expect("Failed to build certificate")
    }

    fn expect_no_receive<T: std::fmt::Debug>(receiver: &Receiver<T>) {
        match receiver.try_recv().unwrap_err() {
            TryRecvError::Empty => (),
            e => {
                panic!("unexpected error {e:?}");
            }
        }
    }

    #[test]
    fn test_blssigverifier_send_packets() {
        let (validator_keypairs, mut verifier, votes_for_repair_receiver, receiver) =
            create_keypairs_and_bls_sig_verifier();

        let vote_rank1 = 2;
        let cert_ranks = [0, 2, 3, 4, 5, 7, 8, 9];
        let cert_type = CertificateType::Finalize(4);
        let vote_message1 = create_signed_vote_message(
            &validator_keypairs,
            Vote::new_finalization_vote(5),
            vote_rank1,
        );
        let cert = create_signed_certificate_message(&validator_keypairs, cert_type, &cert_ranks);
        let messages1 = vec![
            ConsensusMessage::Vote(vote_message1),
            ConsensusMessage::Certificate(cert),
        ];

        verifier
            .verify_and_send_batches(messages_to_batches(&messages1))
            .unwrap();
        assert_eq!(receiver.try_iter().flatten().count(), 2);
        assert_eq!(verifier.stats.vote_stats.pool_sent, 1);
        assert_eq!(verifier.stats.cert_stats.pool_sent, 1);
        assert_eq!(verifier.stats.num_pkts_received, 2);
        let received_verified_votes1 = votes_for_repair_receiver.try_recv().unwrap();
        assert_eq!(
            received_verified_votes1,
            (
                validator_keypairs[vote_rank1].vote_keypair.pubkey(),
                vec![5]
            )
        );

        let vote_rank2 = 3;
        let vote_message2 = create_signed_vote_message(
            &validator_keypairs,
            Vote::new_notarization_vote(6, Hash::new_unique()),
            vote_rank2,
        );
        let messages2 = vec![ConsensusMessage::Vote(vote_message2)];
        verifier
            .verify_and_send_batches(messages_to_batches(&messages2))
            .unwrap();

        assert_eq!(receiver.try_iter().flatten().count(), 1);
        assert_eq!(verifier.stats.vote_stats.pool_sent, 2);
        assert_eq!(verifier.stats.cert_stats.pool_sent, 1);
        assert_eq!(verifier.stats.num_pkts_received, 3); // 2 + 1 = 3
        let received_verified_votes2 = votes_for_repair_receiver.try_recv().unwrap();
        assert_eq!(
            received_verified_votes2,
            (
                validator_keypairs[vote_rank2].vote_keypair.pubkey(),
                vec![6]
            )
        );

        verifier.stats.last_report = Instant::now() - STATS_INTERVAL_DURATION;
        let vote_rank3 = 9;
        let vote_message3 = create_signed_vote_message(
            &validator_keypairs,
            Vote::new_notarization_fallback_vote(7, Hash::new_unique()),
            vote_rank3,
        );
        let messages3 = vec![ConsensusMessage::Vote(vote_message3)];
        verifier
            .verify_and_send_batches(messages_to_batches(&messages3))
            .unwrap();
        assert_eq!(receiver.try_iter().flatten().count(), 1);
        assert_eq!(verifier.stats.vote_stats.pool_sent, 0);
        assert_eq!(verifier.stats.num_pkts_received, 0);
        let received_verified_votes3 = votes_for_repair_receiver.try_recv().unwrap();
        assert_eq!(
            received_verified_votes3,
            (
                validator_keypairs[vote_rank3].vote_keypair.pubkey(),
                vec![7]
            )
        );
    }

    #[test]
    fn test_blssigverifier_verify_malformed() {
        let (validator_keypairs, mut verifier, _, receiver) =
            create_keypairs_and_bls_sig_verifier();

        let packets = vec![Packet::default()];
        let packet_batches = vec![RecycledPacketBatch::new(packets).into()];
        verifier.verify_and_send_batches(packet_batches).unwrap();

        assert_eq!(verifier.stats.num_pkts_received, 1);
        assert_eq!(verifier.stats.num_malformed_pkts, 1);

        // Expect no messages since the packet was malformed
        expect_no_receive(&receiver);

        // Send a packet with no epoch stakes
        let vote_message_no_stakes = create_signed_vote_message(
            &validator_keypairs,
            Vote::new_finalization_vote(5_000_000_000), // very high slot
            0,
        );
        let messages_no_stakes = vec![ConsensusMessage::Vote(vote_message_no_stakes)];

        verifier
            .verify_and_send_batches(messages_to_batches(&messages_no_stakes))
            .unwrap();

        assert_eq!(verifier.stats.discard_vote_no_epoch_stakes, 1);

        // Expect no messages since the packet was malformed
        expect_no_receive(&receiver);

        // Send a packet with invalid rank
        let messages_invalid_rank = vec![ConsensusMessage::Vote(VoteMessage {
            vote: Vote::new_finalization_vote(5),
            signature: Signature::default(),
            rank: 1000, // Invalid rank
        })];
        verifier
            .verify_and_send_batches(messages_to_batches(&messages_invalid_rank))
            .unwrap();
        assert_eq!(verifier.stats.discard_vote_invalid_rank, 1);

        // Expect no messages since the packet was malformed
        expect_no_receive(&receiver);
    }

    #[test]
    fn test_blssigverifier_send_packets_channel_full() {
        agave_logger::setup();
        let (votes_for_repair_sender, _votes_for_repair_receiver) = crossbeam_channel::unbounded();
        let (message_sender, message_receiver) = crossbeam_channel::bounded(1);
        let (consensus_metrics_sender, _consensus_metrics_receiver) =
            crossbeam_channel::unbounded();
        let (reward_votes_sender, _reward_votes_receiver) = crossbeam_channel::unbounded();
        let (validator_keypairs, mut verifier) = create_keypairs_and_bls_sig_verifier_with_channels(
            votes_for_repair_sender,
            message_sender,
            consensus_metrics_sender,
            reward_votes_sender,
        );

        let msg1 = ConsensusMessage::Vote(create_signed_vote_message(
            &validator_keypairs,
            Vote::new_finalization_vote(5),
            0,
        ));
        let msg2 = ConsensusMessage::Vote(create_signed_vote_message(
            &validator_keypairs,
            Vote::new_notarization_fallback_vote(6, Hash::new_unique()),
            2,
        ));
        verifier
            .verify_and_send_batches(messages_to_batches(std::slice::from_ref(&msg1)))
            .unwrap();
        verifier
            .verify_and_send_batches(messages_to_batches(&[msg2]))
            .unwrap();

        // We failed to send the second message because the channel is full.
        let msgs = message_receiver.try_iter().flatten().collect::<Vec<_>>();
        assert_eq!(msgs, vec![msg1]);
        assert_eq!(verifier.stats.vote_stats.pool_sent, 1);
        assert_eq!(verifier.stats.vote_stats.pool_channel_full, 1);
    }

    #[test]
    fn test_blssigverifier_send_packets_receiver_closed() {
        let (validator_keypairs, mut verifier, _, receiver) =
            create_keypairs_and_bls_sig_verifier();

        // Close the receiver to simulate a disconnected channel.
        drop(receiver);

        let msg = ConsensusMessage::Vote(create_signed_vote_message(
            &validator_keypairs,
            Vote::new_finalization_vote(5),
            0,
        ));
        let messages = vec![msg];
        let result = verifier.verify_and_send_batches(messages_to_batches(&messages));
        assert!(result.is_err());
    }

    #[test]
    fn test_blssigverifier_send_discarded_packets() {
        let (validator_keypairs, mut verifier, _, receiver) =
            create_keypairs_and_bls_sig_verifier();

        let message = ConsensusMessage::Vote(create_signed_vote_message(
            &validator_keypairs,
            Vote::new_finalization_vote(5),
            0,
        ));
        let mut packet = Packet::default();
        packet
            .populate_packet(None, &message)
            .expect("Failed to populate packet");
        packet.meta_mut().set_discard(true); // Manually discard

        let packets = vec![packet];
        let packet_batches = vec![RecycledPacketBatch::new(packets).into()];

        verifier.verify_and_send_batches(packet_batches).unwrap();
        expect_no_receive(&receiver);
        assert_eq!(verifier.stats.vote_stats.pool_sent, 0);
        assert_eq!(verifier.stats.num_pkts_received, 1);
        assert_eq!(verifier.stats.num_discarded_pkts, 1);
    }

    #[test]
    fn test_blssigverifier_verify_votes_all_valid() {
        let (validator_keypairs, mut verifier, _, message_receiver) =
            create_keypairs_and_bls_sig_verifier();

        let num_votes = 5;
        let mut packets = Vec::with_capacity(num_votes);
        let vote = Vote::new_skip_vote(42);
        let vote_payload = bincode::serialize(&vote).expect("Failed to serialize vote");

        for (i, validator_keypair) in validator_keypairs.iter().enumerate().take(num_votes) {
            let rank = i as u16;
            let bls_keypair = &validator_keypair.bls_keypair;
            let signature: BLSSignature = bls_keypair.sign(&vote_payload).into();
            let consensus_message = ConsensusMessage::Vote(VoteMessage {
                vote,
                signature,
                rank,
            });
            let mut packet = Packet::default();
            packet.populate_packet(None, &consensus_message).unwrap();
            packets.push(packet);
        }

        let packet_batches = vec![RecycledPacketBatch::new(packets).into()];
        verifier.verify_and_send_batches(packet_batches).unwrap();
        assert_eq!(
            message_receiver.try_iter().flatten().count(),
            num_votes,
            "Did not send all valid packets"
        );
    }

    #[test]
    fn test_blssigverifier_verify_votes_two_distinct_messages() {
        let (validator_keypairs, mut verifier, _repair_channel, message_receiver) =
            create_keypairs_and_bls_sig_verifier();

        let num_votes_group1 = 3;
        let num_votes_group2 = 4;
        let num_votes = num_votes_group1 + num_votes_group2;
        let mut packets = Vec::with_capacity(num_votes);

        let vote1 = Vote::new_skip_vote(42);
        let _vote1_payload = bincode::serialize(&vote1).expect("Failed to serialize vote");
        let vote2 = Vote::new_notarization_vote(43, Hash::new_unique());
        let _vote2_payload = bincode::serialize(&vote2).expect("Failed to serialize vote");

        // Group 1 votes
        for (i, _) in validator_keypairs.iter().enumerate().take(num_votes_group1) {
            let msg =
                ConsensusMessage::Vote(create_signed_vote_message(&validator_keypairs, vote1, i));
            let mut p = Packet::default();
            p.populate_packet(None, &msg).unwrap();
            packets.push(p);
        }

        // Group 2 votes
        for (i, _) in validator_keypairs
            .iter()
            .enumerate()
            .skip(num_votes_group1)
            .take(num_votes_group2)
        {
            let msg =
                ConsensusMessage::Vote(create_signed_vote_message(&validator_keypairs, vote2, i));
            let mut p = Packet::default();
            p.populate_packet(None, &msg).unwrap();
            packets.push(p);
        }

        let packet_batches = vec![RecycledPacketBatch::new(packets).into()];
        verifier.verify_and_send_batches(packet_batches).unwrap();
        assert_eq!(
            message_receiver.try_iter().flatten().count(),
            num_votes,
            "Did not send all valid packets"
        );
        assert_eq!(verifier.stats.vote_stats.distinct_votes_stats.entries(), 1);
        assert_eq!(
            verifier
                .stats
                .vote_stats
                .distinct_votes_stats
                .mean()
                .unwrap(),
            2
        );
    }

    #[test]
    fn test_blssigverifier_verify_votes_invalid_in_two_distinct_messages() {
        let (validator_keypairs, mut verifier, _, message_receiver) =
            create_keypairs_and_bls_sig_verifier();

        let num_votes = 5;
        let invalid_rank = 3; // This voter will sign vote 2 with an invalid signature.
        let mut packets = Vec::with_capacity(num_votes);

        let vote1 = Vote::new_skip_vote(42);
        let vote1_payload = bincode::serialize(&vote1).expect("Failed to serialize vote");
        let vote2 = Vote::new_skip_vote(43);
        let vote2_payload = bincode::serialize(&vote2).expect("Failed to serialize vote");
        let invalid_payload =
            bincode::serialize(&Vote::new_skip_vote(99)).expect("Failed to serialize vote");

        for (i, validator_keypair) in validator_keypairs.iter().enumerate().take(num_votes) {
            let rank = i as u16;
            let bls_keypair = &validator_keypair.bls_keypair;

            // Split the votes: Ranks 0, 1 sign vote 1. Ranks 2, 3, 4 sign vote 2.
            let (vote, payload) = if i < 2 {
                (vote1, &vote1_payload)
            } else {
                (vote2, &vote2_payload)
            };

            let signature = if rank == invalid_rank {
                bls_keypair.sign(&invalid_payload).into() // Invalid signature
            } else {
                bls_keypair.sign(payload).into()
            };

            let consensus_message = ConsensusMessage::Vote(VoteMessage {
                vote,
                signature,
                rank,
            });
            let mut packet = Packet::default();
            packet.populate_packet(None, &consensus_message).unwrap();
            packets.push(packet);
        }

        let packet_batches = vec![RecycledPacketBatch::new(packets).into()];
        verifier.verify_and_send_batches(packet_batches).unwrap();
        let sent_messages: Vec<_> = message_receiver.try_iter().flatten().collect();
        assert_eq!(
            sent_messages.len(),
            num_votes - 1,
            "Only valid votes should be sent"
        );
        assert!(!sent_messages.iter().any(|msg| {
            if let ConsensusMessage::Vote(vm) = msg {
                vm.vote == vote2 && vm.rank == invalid_rank
            } else {
                false
            }
        }));
    }

    #[test]
    fn test_blssigverifier_verify_votes_one_invalid_signature() {
        let (validator_keypairs, mut verifier, _, message_receiver) =
            create_keypairs_and_bls_sig_verifier();

        let num_votes = 5;
        let invalid_rank = 2;
        let mut packets = Vec::with_capacity(num_votes);
        let mut consensus_messages = Vec::with_capacity(num_votes); // ADDED: To hold messages for later comparison.

        let vote = Vote::new_skip_vote(42);
        let valid_vote_payload = bincode::serialize(&vote).expect("Failed to serialize vote");
        let invalid_vote_payload =
            bincode::serialize(&Vote::new_skip_vote(99)).expect("Failed to serialize vote");

        for (i, validator_keypair) in validator_keypairs.iter().enumerate().take(num_votes) {
            let rank = i as u16;
            let bls_keypair = &validator_keypair.bls_keypair;

            let signature = if rank == invalid_rank {
                bls_keypair.sign(&invalid_vote_payload).into() // Invalid signature
            } else {
                bls_keypair.sign(&valid_vote_payload).into() // Valid signature
            };

            let consensus_message = ConsensusMessage::Vote(VoteMessage {
                vote,
                signature,
                rank,
            });

            consensus_messages.push(consensus_message.clone());

            let mut packet = Packet::default();
            packet.populate_packet(None, &consensus_message).unwrap();
            packets.push(packet);
        }

        let packet_batches = vec![RecycledPacketBatch::new(packets).into()];
        verifier.verify_and_send_batches(packet_batches).unwrap();
        let sent_messages: Vec<_> = message_receiver.try_iter().flatten().collect();
        assert_eq!(
            sent_messages.len(),
            num_votes - 1,
            "Only valid votes should be sent"
        );

        // Ensure the message with the invalid rank is not in the sent messages.
        assert!(!sent_messages.iter().any(|msg| {
            if let ConsensusMessage::Vote(vm) = msg {
                vm.rank == invalid_rank
            } else {
                false
            }
        }));
    }

    #[test]
    fn test_blssigverifier_verify_votes_empty_batch() {
        let (_, mut verifier, _r, _a) = create_keypairs_and_bls_sig_verifier();

        let packet_batches = vec![];
        verifier.verify_and_send_batches(packet_batches).unwrap();
        assert_eq!(verifier.stats.num_pkts_received, 0);
    }

    #[test]
    fn test_verify_certificate_base2_valid() {
        let (validator_keypairs, mut verifier, _, message_receiver) =
            create_keypairs_and_bls_sig_verifier();

        let num_signers = 7; // > 2/3 of 10 validators
        let cert_type = CertificateType::Notarize(10, Hash::new_unique());
        let cert = create_signed_certificate_message(
            &validator_keypairs,
            cert_type,
            &(0..num_signers).collect::<Vec<_>>(),
        );
        let consensus_message = ConsensusMessage::Certificate(cert);
        let packet_batches = messages_to_batches(&[consensus_message]);

        verifier.verify_and_send_batches(packet_batches).unwrap();
        assert_eq!(
            message_receiver.try_iter().flatten().count(),
            1,
            "Valid Base2 certificate should be sent"
        );
    }

    #[test]
    fn test_verify_certificate_base2_just_enough_stake() {
        let (validator_keypairs, mut verifier, _, message_receiver) =
            create_keypairs_and_bls_sig_verifier();

        let num_signers = 6; // = 60% of 10 validators
        let cert_type = CertificateType::Notarize(10, Hash::new_unique());
        let cert = create_signed_certificate_message(
            &validator_keypairs,
            cert_type,
            &(0..num_signers).collect::<Vec<_>>(),
        );
        let consensus_message = ConsensusMessage::Certificate(cert);
        let packet_batches = messages_to_batches(&[consensus_message]);

        verifier.verify_and_send_batches(packet_batches).unwrap();
        assert_eq!(
            message_receiver.try_iter().flatten().count(),
            1,
            "Valid Base2 certificate should be sent"
        );
    }

    #[test]
    fn test_verify_certificate_base2_not_enough_stake() {
        let (validator_keypairs, mut verifier, _, message_receiver) =
            create_keypairs_and_bls_sig_verifier();

        let num_signers = 5; // < 60% of 10 validators
        let cert_type = CertificateType::Notarize(10, Hash::new_unique());
        let cert = create_signed_certificate_message(
            &validator_keypairs,
            cert_type,
            &(0..num_signers).collect::<Vec<_>>(),
        );
        let consensus_message = ConsensusMessage::Certificate(cert);
        let packet_batches = messages_to_batches(&[consensus_message]);

        // The call still succeeds, but the packet is marked for discard.
        verifier.verify_and_send_batches(packet_batches).unwrap();
        assert_eq!(
            message_receiver.try_iter().flatten().count(),
            0,
            "This certificate should be invalid"
        );
        assert_eq!(verifier.stats.cert_stats.stake_verification_failed, 1);
    }

    #[test]
    fn test_verify_certificate_base3_valid() {
        let (validator_keypairs, mut verifier, _, message_receiver) =
            create_keypairs_and_bls_sig_verifier();

        let slot = 20;
        let block_hash = Hash::new_unique();
        let notarize_vote = Vote::new_notarization_vote(slot, block_hash);
        let notarize_fallback_vote = Vote::new_notarization_fallback_vote(slot, block_hash);
        let mut all_vote_messages = Vec::new();
        (0..4).for_each(|i| {
            all_vote_messages.push(create_signed_vote_message(
                &validator_keypairs,
                notarize_vote,
                i,
            ))
        });
        (4..7).for_each(|i| {
            all_vote_messages.push(create_signed_vote_message(
                &validator_keypairs,
                notarize_fallback_vote,
                i,
            ))
        });
        let cert_type = CertificateType::NotarizeFallback(slot, block_hash);
        let mut builder = CertificateBuilder::new(cert_type);
        builder
            .aggregate(&all_vote_messages)
            .expect("Failed to aggregate votes");
        let cert = builder.build().expect("Failed to build certificate");
        let consensus_message = ConsensusMessage::Certificate(cert);
        let packet_batches = messages_to_batches(&[consensus_message]);

        verifier.verify_and_send_batches(packet_batches).unwrap();
        assert_eq!(
            message_receiver.try_iter().flatten().count(),
            1,
            "Valid Base3 certificate should be sent"
        );
    }

    #[test]
    fn test_verify_certificate_base3_just_enough_stake() {
        let (validator_keypairs, mut verifier, _, message_receiver) =
            create_keypairs_and_bls_sig_verifier();

        let slot = 20;
        let block_hash = Hash::new_unique();
        let notarize_vote = Vote::new_notarization_vote(slot, block_hash);
        let notarize_fallback_vote = Vote::new_notarization_fallback_vote(slot, block_hash);
        let mut all_vote_messages = Vec::new();
        (0..4).for_each(|i| {
            all_vote_messages.push(create_signed_vote_message(
                &validator_keypairs,
                notarize_vote,
                i,
            ))
        });
        (4..6).for_each(|i| {
            all_vote_messages.push(create_signed_vote_message(
                &validator_keypairs,
                notarize_fallback_vote,
                i,
            ))
        });
        let cert_type = CertificateType::NotarizeFallback(slot, block_hash);
        let mut builder = CertificateBuilder::new(cert_type);
        builder
            .aggregate(&all_vote_messages)
            .expect("Failed to aggregate votes");
        let cert = builder.build().expect("Failed to build certificate");
        let consensus_message = ConsensusMessage::Certificate(cert);
        let packet_batches = messages_to_batches(&[consensus_message]);

        verifier.verify_and_send_batches(packet_batches).unwrap();
        assert_eq!(
            message_receiver.try_iter().flatten().count(),
            1,
            "Valid Base3 certificate should be sent"
        );
    }

    #[test]
    fn test_verify_certificate_base3_not_enough_stake() {
        let (validator_keypairs, mut verifier, _, message_receiver) =
            create_keypairs_and_bls_sig_verifier();

        let slot = 20;
        let block_hash = Hash::new_unique();
        let notarize_vote = Vote::new_notarization_vote(slot, block_hash);
        let notarize_fallback_vote = Vote::new_notarization_fallback_vote(slot, block_hash);
        let mut all_vote_messages = Vec::new();
        (0..4).for_each(|i| {
            all_vote_messages.push(create_signed_vote_message(
                &validator_keypairs,
                notarize_vote,
                i,
            ))
        });
        (4..5).for_each(|i| {
            all_vote_messages.push(create_signed_vote_message(
                &validator_keypairs,
                notarize_fallback_vote,
                i,
            ))
        });
        let cert_type = CertificateType::NotarizeFallback(slot, block_hash);
        let mut builder = CertificateBuilder::new(cert_type);
        builder
            .aggregate(&all_vote_messages)
            .expect("Failed to aggregate votes");
        let cert = builder.build().expect("Failed to build certificate");
        let consensus_message = ConsensusMessage::Certificate(cert);
        let packet_batches = messages_to_batches(&[consensus_message]);

        verifier.verify_and_send_batches(packet_batches).unwrap();
        assert_eq!(
            message_receiver.try_iter().flatten().count(),
            0,
            "This certificate should be invalid"
        );
        assert_eq!(verifier.stats.cert_stats.stake_verification_failed, 1);
    }

    #[test]
    fn test_verify_certificate_invalid_signature() {
        let (_validator_keypairs, mut verifier, _, message_receiver) =
            create_keypairs_and_bls_sig_verifier();

        let num_signers = 7;
        let slot = 10;
        let block_hash = Hash::new_unique();
        let cert_type = CertificateType::Notarize(slot, block_hash);
        let mut bitmap = BitVec::<u8, Lsb0>::new();
        bitmap.resize(num_signers, false);
        for i in 0..num_signers {
            bitmap.set(i, true);
        }
        let encoded_bitmap = encode_base2(&bitmap).unwrap();

        let cert = Certificate {
            cert_type,
            signature: BLSSignature::default(), // Use a default/wrong signature
            bitmap: encoded_bitmap,
        };
        let consensus_message = ConsensusMessage::Certificate(cert);
        let packet_batches = messages_to_batches(&[consensus_message]);

        verifier.verify_and_send_batches(packet_batches).unwrap();
        expect_no_receive(&message_receiver);
        assert_eq!(verifier.stats.cert_stats.signature_verification_failed, 1);
    }

    #[test]
    fn test_verify_mixed_valid_batch() {
        let (validator_keypairs, mut verifier, _, message_receiver) =
            create_keypairs_and_bls_sig_verifier();

        let mut packets = Vec::new();
        let num_votes = 2;

        let vote = Vote::new_skip_vote(42);
        let vote_payload = bincode::serialize(&vote).unwrap();
        for (i, validator_keypair) in validator_keypairs.iter().enumerate().take(num_votes) {
            let rank = i as u16;
            let bls_keypair = &validator_keypair.bls_keypair;
            let signature: BLSSignature = bls_keypair.sign(&vote_payload).into();
            let consensus_message = ConsensusMessage::Vote(VoteMessage {
                vote,
                signature,
                rank,
            });
            let mut packet = Packet::default();
            packet.populate_packet(None, &consensus_message).unwrap();
            packets.push(packet);
        }

        let num_cert_signers = 7;
        let cert_type = CertificateType::Notarize(10, Hash::new_unique());
        let cert_original_vote = Vote::new_notarization_vote(10, cert_type.to_block().unwrap().1);
        let cert_payload = bincode::serialize(&cert_original_vote).unwrap();

        let cert_vote_messages: Vec<VoteMessage> = (0..num_cert_signers)
            .map(|i| {
                let signature = validator_keypairs[i].bls_keypair.sign(&cert_payload);
                VoteMessage {
                    vote: cert_original_vote,
                    signature: signature.into(),
                    rank: i as u16,
                }
            })
            .collect();
        let mut builder = CertificateBuilder::new(cert_type);
        builder
            .aggregate(&cert_vote_messages)
            .expect("Failed to aggregate votes for certificate");
        let cert = builder.build().expect("Failed to build certificate");
        let consensus_message_cert = ConsensusMessage::Certificate(cert);
        let mut cert_packet = Packet::default();
        cert_packet
            .populate_packet(None, &consensus_message_cert)
            .unwrap();
        packets.push(cert_packet);

        let packet_batches = vec![RecycledPacketBatch::new(packets).into()];
        verifier.verify_and_send_batches(packet_batches).unwrap();
        assert_eq!(
            message_receiver.try_iter().flatten().count(),
            num_votes + 1,
            "All valid messages in a mixed batch should be sent"
        );
        assert_eq!(verifier.stats.vote_stats.pool_sent, num_votes as u64);
        assert_eq!(verifier.stats.cert_stats.pool_sent, 1);
    }

    #[test]
    fn test_verify_vote_with_invalid_rank() {
        let (validator_keypairs, mut verifier, _, message_receiver) =
            create_keypairs_and_bls_sig_verifier();

        let invalid_rank = 999;
        let vote = Vote::new_skip_vote(42);
        let vote_payload = bincode::serialize(&vote).unwrap();
        let bls_keypair = &validator_keypairs[0].bls_keypair;
        let signature: BLSSignature = bls_keypair.sign(&vote_payload).into();

        let consensus_message = ConsensusMessage::Vote(VoteMessage {
            vote,
            signature,
            rank: invalid_rank,
        });

        let packet_batches = messages_to_batches(&[consensus_message]);
        verifier.verify_and_send_batches(packet_batches).unwrap();
        expect_no_receive(&message_receiver);
        assert_eq!(verifier.stats.discard_vote_invalid_rank, 1);
    }

    #[test]
    fn test_verify_old_vote_and_cert() {
        let (message_sender, message_receiver) = crossbeam_channel::unbounded();
        let (votes_for_repair_sender, _) = crossbeam_channel::unbounded();
        let (consensus_metrics_sender, _) = crossbeam_channel::unbounded();
        let (reward_votes_sender, _reward_votes_receiver) = crossbeam_channel::unbounded();
        let validator_keypairs = (0..10)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let stakes_vec = (0..validator_keypairs.len())
            .map(|i| 1_000 - i as u64)
            .collect::<Vec<_>>();
        let genesis = create_genesis_config_with_alpenglow_vote_accounts(
            1_000_000_000,
            &validator_keypairs,
            stakes_vec,
        );
        let bank0 = Bank::new_for_tests(&genesis.genesis_config);
        let bank5 = Bank::new_from_parent(Arc::new(bank0), &Pubkey::default(), 5);
        let bank_forks = BankForks::new_rw_arc(bank5);

        bank_forks.write().unwrap().set_root(5, None, None);

        let sharable_banks = bank_forks.read().unwrap().sharable_banks();
        let keypair = Keypair::new();
        let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), 0);
        let cluster_info = Arc::new(ClusterInfo::new(
            contact_info,
            Arc::new(keypair),
            SocketAddrSpace::Unspecified,
        ));
        let leader_schedule = Arc::new(LeaderScheduleCache::new_from_bank(&sharable_banks.root()));
        let mut sig_verifier = SigVerifier::new(
            Arc::new(MigrationStatus::default()),
            sharable_banks,
            votes_for_repair_sender,
            reward_votes_sender,
            message_sender,
            consensus_metrics_sender,
            cluster_info,
            leader_schedule,
            4,
        );

        let vote = Vote::new_skip_vote(2);
        let vote_payload = bincode::serialize(&vote).unwrap();
        let bls_keypair = &validator_keypairs[0].bls_keypair;
        let signature: BLSSignature = bls_keypair.sign(&vote_payload).into();
        let consensus_message_vote = ConsensusMessage::Vote(VoteMessage {
            vote,
            signature,
            rank: 0,
        });
        let packet_batches_vote = messages_to_batches(&[consensus_message_vote]);

        sig_verifier
            .verify_and_send_batches(packet_batches_vote)
            .unwrap();
        expect_no_receive(&message_receiver);
        assert_eq!(sig_verifier.stats.num_old_votes_received, 1);

        let cert = create_signed_certificate_message(
            &validator_keypairs,
            CertificateType::Finalize(3),
            &[0], // Signer rank 0
        );
        let consensus_message_cert = ConsensusMessage::Certificate(cert);
        let packet_batches_cert = messages_to_batches(&[consensus_message_cert]);

        sig_verifier
            .verify_and_send_batches(packet_batches_cert)
            .unwrap();
        expect_no_receive(&message_receiver);
        assert_eq!(sig_verifier.stats.num_old_certs_received, 1);
        assert_eq!(sig_verifier.stats.num_old_votes_received, 1);
    }

    #[test]
    fn test_verified_certs_are_skipped() {
        let (validator_keypairs, mut verifier, _, message_receiver) =
            create_keypairs_and_bls_sig_verifier();

        let num_signers = 8;
        let slot = 10;
        let block_hash = Hash::new_unique();
        let cert_type = CertificateType::Notarize(slot, block_hash);
        let original_vote = Vote::new_notarization_vote(slot, block_hash);
        let signed_payload = bincode::serialize(&original_vote).unwrap();
        let mut vote_messages: Vec<VoteMessage> = (0..num_signers)
            .map(|i| {
                let signature = validator_keypairs[i].bls_keypair.sign(&signed_payload);
                VoteMessage {
                    vote: original_vote,
                    signature: signature.into(),
                    rank: i as u16,
                }
            })
            .collect();

        let mut builder1 = CertificateBuilder::new(cert_type);
        builder1
            .aggregate(&vote_messages)
            .expect("Failed to aggregate votes");
        let cert1 = builder1.build().expect("Failed to build certificate");
        let consensus_message1 = ConsensusMessage::Certificate(cert1);
        let packet_batches1 = messages_to_batches(&[consensus_message1]);

        verifier.verify_and_send_batches(packet_batches1).unwrap();

        assert_eq!(
            message_receiver.try_iter().flatten().count(),
            1,
            "First certificate should be sent"
        );
        assert_eq!(verifier.stats.num_verified_certs_received, 0);

        vote_messages.pop(); // Remove one signature
        let mut builder2 = CertificateBuilder::new(cert_type);
        builder2
            .aggregate(&vote_messages)
            .expect("Failed to aggregate votes");
        let cert2 = builder2.build().expect("Failed to build certificate");
        let consensus_message2 = ConsensusMessage::Certificate(cert2);
        let packet_batches2 = messages_to_batches(&[consensus_message2]);

        verifier.verify_and_send_batches(packet_batches2).unwrap();
        expect_no_receive(&message_receiver);
        assert_eq!(
            verifier.stats.num_pkts_received, 2,
            "Should have received two packets in total"
        );
        assert_eq!(
            verifier.stats.num_verified_certs_received, 1,
            "Should have detected one already-verified cert"
        );
    }

    fn messages_to_batches(messages: &[ConsensusMessage]) -> Vec<PacketBatch> {
        let packets: Vec<_> = messages
            .iter()
            .map(|msg| {
                let mut p = Packet::default();
                p.populate_packet(None, msg).unwrap();
                p
            })
            .collect();
        vec![RecycledPacketBatch::new(packets).into()]
    }
}
