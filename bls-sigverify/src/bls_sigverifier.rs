//! The BLS signature verifier.

use {
    crate::{
        bls_cert_sigverify::{CertPayload, verify_and_send_certificates},
        bls_vote_sigverify::{UnverifiedVotePayload, verify_and_send_votes},
        errors::SigVerifyError,
        generated_cert_types::GeneratedCertTypes,
        rewards::rewards_wants_vote,
        sig_verified_messages::SigVerifiedBatch,
        stats::SigVerifierStats,
    },
    agave_votor_messages::{
        VerifiedVoterSlotsSender,
        certificate::CertificateType,
        metric_types::ConsensusMetricsEventSender,
        migration::MigrationStatus,
        reward_certificate::AddVoteMessage,
        unverified_vote_message::{DecodedWireConsensusMessage, UnverifiedVoteMessage},
        vote::Vote,
        wire::{VersionedWireConsensusMessage, VotePayloadToSign},
    },
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError},
    log::error,
    rayon::{ThreadPool, ThreadPoolBuilder},
    solana_bls_signatures::pubkey::{PopVerified, PubkeyAffine as BlsPubkeyAffine},
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_measure::measure_us,
    solana_perf::packet::packet_config,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::SharableBanks},
    solana_streamer::{nonblocking::simple_qos::SimpleQosBanlist, packet::PacketBatch},
    std::{
        collections::{HashMap, HashSet},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder},
        time::Duration,
    },
};

/// If a cert or vote is so many slots in the future relative to the root slot, it is considered
/// invalid and discarded.
///
/// This also sets an upper bound on how much storage the various structs in this module require.
pub(super) const NUM_SLOTS_FOR_VERIFY: Slot = 90_000;

/// If we receive an invalid certificate or vote from a QUIC connection, we ban the sender.
/// We ban the sender for 2 days which roughly corresponds to an epoch
pub(super) const BAN_TIMEOUT: Duration = Duration::from_hours(48);

pub struct SigVerifierContext {
    pub migration_status: Arc<MigrationStatus>,
    pub banlist: Arc<SimpleQosBanlist>,
    pub sharable_banks: SharableBanks,
    pub cluster_info: Arc<ClusterInfo>,
    pub leader_schedule: Arc<LeaderScheduleCache>,
    pub num_threads: usize,
    pub generated_cert_types: Arc<GeneratedCertTypes>,
}

pub struct SigVerifierChannels {
    pub packet_receiver: Receiver<PacketBatch>,
    pub channel_to_repair: VerifiedVoterSlotsSender,
    pub channel_to_reward: Sender<AddVoteMessage>,
    pub channel_to_pool: Sender<SigVerifiedBatch>,
    pub channel_to_metrics: ConsensusMetricsEventSender,
}

/// Starts the BLS sigverifier service in its own dedicated thread.
pub fn spawn_service(
    exit: Arc<AtomicBool>,
    context: SigVerifierContext,
    channels: SigVerifierChannels,
) -> thread::JoinHandle<()> {
    let verifier = SigVerifier::new(context, channels);

    Builder::new()
        .name("solSigVerBLS".to_string())
        .spawn(move || verifier.run(exit))
        .unwrap()
}

struct SigVerifier {
    migration_status: Arc<MigrationStatus>,
    banlist: Arc<SimpleQosBanlist>,
    channels: SigVerifierChannels,
    /// Container to look up root banks from.
    sharable_banks: SharableBanks,
    stats: SigVerifierStats,
    /// Set of recently verified certs to avoid duplicate work.
    verified_certs: HashSet<CertificateType>,
    /// Tracks when the cache was last pruned.
    last_checked_root_slot: Slot,
    cluster_info: Arc<ClusterInfo>,
    leader_schedule: Arc<LeaderScheduleCache>,
    /// thread pool to use for all parallel tasks
    thread_pool: ThreadPool,
    generated_cert_types: Arc<GeneratedCertTypes>,
}

impl SigVerifier {
    fn new(context: SigVerifierContext, channels: SigVerifierChannels) -> Self {
        let SigVerifierContext {
            migration_status,
            banlist,
            sharable_banks,
            cluster_info,
            leader_schedule,
            num_threads,
            generated_cert_types,
        } = context;
        let thread_pool = ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .thread_name(|i| format!("solSigVerBLS{i:02}"))
            .build()
            .unwrap();
        let root_slot = sharable_banks.root().slot();
        Self {
            migration_status,
            banlist,
            channels,
            sharable_banks,
            stats: SigVerifierStats::new(root_slot),
            verified_certs: HashSet::new(),
            last_checked_root_slot: 0,
            cluster_info,
            leader_schedule,
            thread_pool,
            generated_cert_types,
        }
    }

    fn run(mut self, exit: Arc<AtomicBool>) {
        while !exit.load(Ordering::Relaxed) {
            const SOFT_RECEIVE_CAP: usize = 5000;
            let Ok(batches) = recv_batches(&self.channels.packet_receiver, SOFT_RECEIVE_CAP) else {
                error!("packet_receiver disconnected:  Exiting.");
                break;
            };
            if batches.is_empty() || self.migration_status.is_pre_feature_activation() {
                continue;
            }

            let (verify_res, verify_time_us) = measure_us!(self.verify_and_send_batches(batches));
            self.stats
                .verify_and_send_batch_us
                .add_sample(verify_time_us);
            if let Err(e) = verify_res {
                error!("verify_and_send_batch() failed with {e}. Exiting.");
                break;
            }
            self.stats.maybe_report(self.sharable_banks.root().slot());
        }
        self.stats.do_report(self.sharable_banks.root().slot());
    }

    fn verify_and_send_batches(&mut self, batches: Vec<PacketBatch>) -> Result<(), SigVerifyError> {
        let root_bank = self.sharable_banks.root();
        self.maybe_prune_caches(root_bank.slot());

        let ((cert_groups, votes_to_verify), extract_msgs_us) =
            measure_us!(self.extract_and_filter_msgs(batches, &root_bank));
        self.stats
            .extract_filter_msgs_us
            .add_sample(extract_msgs_us);

        let (votes_result, certs_result) = self.thread_pool.join(
            || {
                verify_and_send_votes(
                    votes_to_verify,
                    &root_bank,
                    &self.cluster_info,
                    &self.leader_schedule,
                    &self.banlist,
                    &self.thread_pool,
                    &self.channels,
                )
            },
            || {
                verify_and_send_certificates(
                    &mut self.verified_certs,
                    cert_groups,
                    &root_bank,
                    &self.channels.channel_to_pool,
                    &self.banlist,
                    &self.thread_pool,
                )
            },
        );

        let vote_stats = votes_result?;
        let cert_stats = certs_result?;

        self.stats.vote_stats.merge(vote_stats);
        self.stats.cert_stats.merge(cert_stats);
        Ok(())
    }

    fn maybe_prune_caches(&mut self, root_slot: Slot) {
        if self.last_checked_root_slot < root_slot {
            self.last_checked_root_slot = root_slot;
            self.verified_certs.retain(|cert| cert.slot() >= root_slot);
        }
    }

    fn extract_and_filter_msgs(
        &mut self,
        batches: Vec<PacketBatch>,
        root_bank: &Bank,
    ) -> (
        HashMap<CertificateType, Vec<CertPayload>>,
        HashMap<VotePayloadToSign, Vec<UnverifiedVotePayload>>,
    ) {
        let root_slot = root_bank.slot();
        let mut cert_groups = HashMap::<CertificateType, Vec<CertPayload>>::new();
        let mut votes: HashMap<VotePayloadToSign, Vec<UnverifiedVotePayload>> = HashMap::new();
        let mut num_pkts = 0u64;
        let my_shred_version = self.cluster_info.my_shred_version();
        for packet in batches.iter().flatten() {
            num_pkts = num_pkts.saturating_add(1);
            if packet.meta().discard() {
                self.stats.num_discarded_pkts += 1;
                continue;
            }
            // TODO(#13227): Enforce shred_version matching during deserialization
            let Ok(msg) = wincode::config::deserialize_exact::<VersionedWireConsensusMessage, _>(
                packet.data(..).unwrap_or_default(),
                packet_config(),
            ) else {
                self.stats.num_malformed_pkts += 1;
                continue;
            };
            let Some(sender_identity_pubkey) = packet.meta().remote_pubkey() else {
                debug_assert!(false, "BLS packet missing remote pubkey");
                self.stats.num_malformed_pkts += 1;
                continue;
            };
            let Some(decoded_msg) = DecodedWireConsensusMessage::try_new(msg, my_shred_version)
            else {
                self.stats.num_malformed_pkts += 1;
                continue;
            };

            match decoded_msg {
                DecodedWireConsensusMessage::Vote(unverified_vote) => {
                    if let Some((sender_vote_account_pubkey, sender_bls_pubkey)) =
                        self.keep_vote(&unverified_vote.vote, &unverified_vote, root_bank)
                    {
                        let vote_payload_to_sign = VotePayloadToSign::new_from_vote(
                            unverified_vote.vote,
                            unverified_vote.shred_version,
                        );
                        votes.entry(vote_payload_to_sign).or_default().push(
                            UnverifiedVotePayload {
                                vote_message: unverified_vote,
                                sender_bls_pubkey,
                                sender_vote_account_pubkey,
                                sender_identity_pubkey,
                            },
                        );
                    }
                }
                DecodedWireConsensusMessage::Certificate(cert) => {
                    if cert.cert_type.slot() < root_slot {
                        self.stats.num_old_certs_received += 1;
                        continue;
                    }
                    if self.verified_certs.contains(&cert.cert_type) {
                        self.stats.num_verified_certs_received += 1;
                        continue;
                    }
                    if self.generated_cert_types.has_cert(&cert.cert_type) {
                        self.stats.num_generated_certs_received += 1;
                        continue;
                    }
                    cert_groups
                        .entry(cert.cert_type)
                        .or_default()
                        .push(CertPayload {
                            cert,
                            sender_identity_pubkey,
                        });
                }
            }
        }
        self.stats.num_pkts.add_sample(num_pkts);
        (cert_groups, votes)
    }

    /// If this vote should be verified, then returns the sender's Pubkey and BlsPubkey.
    fn keep_vote(
        &mut self,
        vote: &Vote,
        msg: &UnverifiedVoteMessage,
        root_bank: &Bank,
    ) -> Option<(Pubkey, PopVerified<BlsPubkeyAffine>)> {
        let root_slot = root_bank.slot();
        let Some(rank_map) = root_bank.get_rank_map(vote.slot()) else {
            self.stats.discard_vote_no_epoch_stakes += 1;
            return None;
        };
        let entry = rank_map
            .get_pubkey_stake_entry(msg.rank.into())
            .or_else(|| {
                self.stats.discard_vote_invalid_rank += 1;
                None
            })?;
        let ret = Some((entry.vote_account_pubkey, entry.bls_pubkey));
        if vote.slot() > root_slot
            // Genesis votes should be allowed on the TowerBFT root
         || (vote.is_genesis_vote() && vote.slot() >= root_slot)
        {
            return ret;
        }
        if rewards_wants_vote(&self.cluster_info, &self.leader_schedule, root_slot, vote) {
            return ret;
        }
        self.stats.num_old_votes_received += 1;
        None
    }
}

/// Receives a `Vec<PacketBatch>` from the `receiver` while adhering to the `soft_receive_cap` limit.
///
/// Returns `Err(())` if the channel disconnected.
fn recv_batches(
    receiver: &Receiver<PacketBatch>,
    soft_receive_cap: usize,
) -> Result<Vec<PacketBatch>, ()> {
    let batch = match receiver.recv_timeout(Duration::from_secs(1)) {
        Ok(b) => b,
        Err(e) => match e {
            RecvTimeoutError::Timeout => {
                return Ok(vec![]);
            }
            RecvTimeoutError::Disconnected => {
                return Err(());
            }
        },
    };
    let mut batches = Vec::with_capacity(soft_receive_cap);
    batches.push(batch);
    while batches.len() < soft_receive_cap {
        match receiver.try_recv() {
            Ok(b) => {
                batches.push(b);
            }
            Err(e) => match e {
                TryRecvError::Empty => return Ok(batches),
                TryRecvError::Disconnected => return Err(()),
            },
        }
    }
    Ok(batches)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_votor::consensus_pool::certificate_builder::CertificateBuilder,
        agave_votor_messages::{
            VerifiedVoterSlotsReceiver,
            certificate::{Certificate, CertificateType},
            consensus_message::{Block, ConsensusMessage, VoteMessage},
            metric_types::ConsensusMetricsEventReceiver,
            vote::Vote,
            wire::{VersionedWireConsensusMessage, get_vote_payload_to_sign},
        },
        bitvec::prelude::{BitVec, Lsb0},
        crossbeam_channel::{Receiver, TryRecvError, bounded},
        solana_bls_signatures::{BLS_SIGNATURE_AFFINE_SIZE, Signature},
        solana_epoch_schedule::EpochSchedule,
        solana_gossip::contact_info::ContactInfo,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_net_utils::SocketAddrSpace,
        solana_perf::packet::{BytesPacket, BytesPacketBatch, Packet, RecycledPacketBatch},
        solana_pubkey::Pubkey,
        solana_runtime::{
            bank::{Bank, SlotLeader},
            bank_forks::BankForks,
            genesis_utils::{
                ValidatorVoteKeypairs, create_genesis_config_with_alpenglow_vote_accounts,
            },
        },
        solana_signer::Signer,
        solana_signer_store::encode_base2,
    };

    fn new_test_banlist() -> Arc<SimpleQosBanlist> {
        let (banlist, _banlist_eviction_receiver) = SimpleQosBanlist::new();
        Arc::new(banlist)
    }

    struct TestContext {
        verifier: SigVerifier,
        validator_keypairs: Vec<ValidatorVoteKeypairs>,
        banlist: Arc<SimpleQosBanlist>,

        _packet_sender: Sender<PacketBatch>,
        repair_receiver: VerifiedVoterSlotsReceiver,
        _reward_receiver: Receiver<AddVoteMessage>,
        pool_receiver: Receiver<SigVerifiedBatch>,
        _metrics_receiver: ConsensusMetricsEventReceiver,
        generated_cert_types: Arc<GeneratedCertTypes>,
    }

    impl TestContext {
        fn new() -> Self {
            let (channel_to_pool, pool_receiver) = bounded(1024);
            Self::new_with_pool_channel(channel_to_pool, pool_receiver)
        }

        fn new_with_pool_channel(
            channel_to_pool: Sender<SigVerifiedBatch>,
            pool_receiver: Receiver<SigVerifiedBatch>,
        ) -> Self {
            let num_validators = 10;
            let validator_keypairs = (0..num_validators)
                .map(|_| ValidatorVoteKeypairs::new_rand())
                .collect::<Vec<_>>();
            let stakes_vec = (0..validator_keypairs.len())
                .map(|i| 1_000u64.saturating_sub(i as u64))
                .collect::<Vec<_>>();
            let mut genesis = create_genesis_config_with_alpenglow_vote_accounts(
                1_000_000_000,
                &validator_keypairs,
                stakes_vec,
            );
            genesis.genesis_config.epoch_schedule = EpochSchedule::without_warmup();
            let bank = Bank::new_for_tests(&genesis.genesis_config);
            let bank_forks = BankForks::new_rw_arc(bank);
            let sharable_banks = bank_forks.read().unwrap().sharable_banks();
            let keypair = Keypair::new();
            let contact_info = ContactInfo::new_localhost(&keypair.pubkey(), 0);
            let cluster_info = Arc::new(ClusterInfo::new(
                contact_info,
                Arc::new(keypair),
                SocketAddrSpace::Unspecified,
            ));
            let leader_schedule =
                Arc::new(LeaderScheduleCache::new_from_bank(&sharable_banks.root()));

            let (channel_to_repair, repair_receiver) = bounded(1024);
            let (channel_to_reward, reward_receiver) = bounded(1024);
            let (packet_sender, packet_receiver) = bounded(1024);
            let (channel_to_metrics, metrics_receiver) = bounded(1024);

            let generated_cert_types = Arc::new(GeneratedCertTypes::default());
            let banlist = new_test_banlist();
            let verifier = SigVerifier::new(
                SigVerifierContext {
                    migration_status: Arc::new(MigrationStatus::default()),
                    banlist: banlist.clone(),
                    sharable_banks,
                    cluster_info,
                    leader_schedule,
                    num_threads: 4,
                    generated_cert_types: generated_cert_types.clone(),
                },
                SigVerifierChannels {
                    packet_receiver,
                    channel_to_repair,
                    channel_to_reward,
                    channel_to_pool,
                    channel_to_metrics,
                },
            );
            Self {
                validator_keypairs,
                verifier,
                banlist,
                _packet_sender: packet_sender,
                repair_receiver,
                _reward_receiver: reward_receiver,
                pool_receiver,
                _metrics_receiver: metrics_receiver,
                generated_cert_types,
            }
        }
    }

    fn create_signed_vote_message(
        validator_keypairs: &[ValidatorVoteKeypairs],
        shred_version: u16,
        vote: Vote,
        rank: usize,
    ) -> VoteMessage {
        let bls_keypair = &validator_keypairs[rank].bls_keypair;
        let payload = get_vote_payload_to_sign(vote, shred_version);
        let signature: Signature = bls_keypair.sign(&payload).into();
        VoteMessage {
            vote,
            signature,
            rank: rank as u16,
        }
    }

    fn create_signed_certificate_message(
        shred_version: u16,
        validator_keypairs: &[ValidatorVoteKeypairs],
        cert_type: CertificateType,
        ranks: &[usize],
    ) -> Certificate {
        let mut builder = CertificateBuilder::new(cert_type);
        // Assumes Base2 encoding (single vote type) for simplicity in this helper.
        let vote = cert_type.to_source_vote();
        let vote_messages: Vec<VoteMessage> = ranks
            .iter()
            .map(|&rank| create_signed_vote_message(validator_keypairs, shred_version, vote, rank))
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

    fn message_to_packet(
        message: &ConsensusMessage,
        shred_version: u16,
        remote_pubkey: Pubkey,
    ) -> BytesPacket {
        let msg = VersionedWireConsensusMessage::new(message.clone(), shred_version);
        let mut packet = BytesPacket::from_data(&msg).unwrap();
        packet.meta_mut().set_remote_pubkey(remote_pubkey);
        packet
    }

    #[test]
    fn test_blssigverifier_send_packets() {
        let mut ctx = TestContext::new();

        let vote_rank1 = 2;
        let cert_ranks = [0, 2, 3, 4, 5, 7, 8, 9];
        let cert_type = CertificateType::Finalize(4);
        let vote_message1 = create_signed_vote_message(
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_finalization_vote(5),
            vote_rank1,
        );
        let cert = create_signed_certificate_message(
            ctx.verifier.cluster_info.my_shred_version(),
            &ctx.validator_keypairs,
            cert_type,
            &cert_ranks,
        );
        let messages1 = [
            (
                ConsensusMessage::Vote(vote_message1),
                ctx.validator_keypairs[vote_rank1].node_keypair.pubkey(),
            ),
            (ConsensusMessage::Certificate(cert), Pubkey::new_unique()),
        ];

        ctx.verifier
            .verify_and_send_batches(messages_to_batches(
                &messages1,
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();
        assert_eq!(ctx.pool_receiver.try_iter().count(), 2);
        assert_eq!(ctx.verifier.stats.vote_stats.pool_sent.0, 1);
        assert_eq!(ctx.verifier.stats.cert_stats.pool_sent.0, 1);
        let received_verified_votes1 = ctx.repair_receiver.try_recv().unwrap();
        assert_eq!(
            received_verified_votes1,
            (
                ctx.validator_keypairs[vote_rank1].vote_keypair.pubkey(),
                vec![5]
            )
        );

        let vote_rank2 = 3;
        let vote_message2 = create_signed_vote_message(
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_notarization_vote(Block {
                slot: 6,
                block_id: Hash::new_unique(),
            }),
            vote_rank2,
        );
        let messages2 = [(
            ConsensusMessage::Vote(vote_message2),
            ctx.validator_keypairs[vote_rank2].node_keypair.pubkey(),
        )];
        ctx.verifier.stats = SigVerifierStats::new(ctx.verifier.sharable_banks.root().slot());
        ctx.verifier
            .verify_and_send_batches(messages_to_batches(
                &messages2,
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();

        assert_eq!(ctx.pool_receiver.try_iter().count(), 1);
        assert_eq!(ctx.verifier.stats.vote_stats.pool_sent.0, 1);
        assert_eq!(ctx.verifier.stats.cert_stats.pool_sent.0, 0);
        let received_verified_votes2 = ctx.repair_receiver.try_recv().unwrap();
        assert_eq!(
            received_verified_votes2,
            (
                ctx.validator_keypairs[vote_rank2].vote_keypair.pubkey(),
                vec![6]
            )
        );

        let vote_rank3 = 9;
        let vote_message3 = create_signed_vote_message(
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_notarization_fallback_vote(Block {
                slot: 7,
                block_id: Hash::new_unique(),
            }),
            vote_rank3,
        );
        let messages3 = [(
            ConsensusMessage::Vote(vote_message3),
            ctx.validator_keypairs[vote_rank3].node_keypair.pubkey(),
        )];
        ctx.verifier.stats = SigVerifierStats::new(ctx.verifier.sharable_banks.root().slot());
        ctx.verifier
            .verify_and_send_batches(messages_to_batches(
                &messages3,
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();
        assert_eq!(ctx.pool_receiver.try_iter().count(), 1);
        assert_eq!(ctx.verifier.stats.vote_stats.pool_sent.0, 1);
        assert_eq!(ctx.verifier.stats.cert_stats.pool_sent.0, 0);
        let received_verified_votes3 = ctx.repair_receiver.try_recv().unwrap();
        assert_eq!(
            received_verified_votes3,
            (
                ctx.validator_keypairs[vote_rank3].vote_keypair.pubkey(),
                vec![7]
            )
        );
    }

    #[test]
    fn test_blssigverifier_verify_malformed() {
        let mut ctx = TestContext::new();

        let packets = vec![Packet::default()];
        let packet_batches = vec![RecycledPacketBatch::new(packets).into()];
        ctx.verifier
            .verify_and_send_batches(packet_batches)
            .unwrap();
        assert_eq!(ctx.verifier.stats.vote_stats.pool_sent.0, 0);
        assert_eq!(ctx.verifier.stats.cert_stats.pool_sent.0, 0);
        assert_eq!(ctx.verifier.stats.num_malformed_pkts.0, 1);

        // Expect no messages since the packet was malformed
        expect_no_receive(&ctx.pool_receiver);

        // Send a packet with no epoch stakes
        let rank = 0;
        let vote_message_no_stakes = create_signed_vote_message(
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_finalization_vote(5_000_000_000), // very high slot
            rank,
        );
        let messages_no_stakes = [(
            ConsensusMessage::Vote(vote_message_no_stakes),
            ctx.validator_keypairs[rank].node_keypair.pubkey(),
        )];

        ctx.verifier
            .verify_and_send_batches(messages_to_batches(
                &messages_no_stakes,
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();

        assert_eq!(ctx.verifier.stats.discard_vote_no_epoch_stakes.0, 1);

        // Expect no messages since the packet was malformed
        expect_no_receive(&ctx.pool_receiver);

        // Send a packet with invalid rank
        let messages_invalid_rank = [(
            ConsensusMessage::Vote(VoteMessage {
                vote: Vote::new_finalization_vote(5),
                signature: Signature([0; BLS_SIGNATURE_AFFINE_SIZE]),
                rank: 1000, // Invalid rank
            }),
            Pubkey::new_unique(),
        )];
        ctx.verifier
            .verify_and_send_batches(messages_to_batches(
                &messages_invalid_rank,
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();
        assert_eq!(ctx.verifier.stats.discard_vote_invalid_rank.0, 1);

        // Expect no messages since the packet was malformed
        expect_no_receive(&ctx.pool_receiver);
    }

    #[test]
    fn test_shred_version_mismatch() {
        let mut ctx = TestContext::new();
        let rank = 0;
        let msgs = [(
            ConsensusMessage::Vote(create_signed_vote_message(
                &ctx.validator_keypairs,
                ctx.verifier.cluster_info.my_shred_version() + 1,
                Vote::new_finalization_vote(5),
                rank,
            )),
            ctx.validator_keypairs[rank].node_keypair.pubkey(),
        )];
        // creating a packet with the wrong shred version
        let packets = messages_to_batches(&msgs, ctx.verifier.cluster_info.my_shred_version() + 1);
        ctx.verifier.verify_and_send_batches(packets).unwrap();
        assert_eq!(ctx.verifier.stats.vote_stats.pool_sent.0, 0);
        assert_eq!(ctx.verifier.stats.cert_stats.pool_sent.0, 0);
        assert_eq!(ctx.verifier.stats.num_malformed_pkts.0, 1);
    }

    #[test]
    fn test_blssigverifier_send_packets_channel_full() {
        agave_logger::setup();
        let (channel_to_pool, pool_receiver) = crossbeam_channel::bounded(1);
        let mut ctx = TestContext::new_with_pool_channel(channel_to_pool, pool_receiver);

        let msg1_rank = 0;
        let msg2_rank = 2;
        let msg1 = create_signed_vote_message(
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_finalization_vote(5),
            msg1_rank,
        );
        let msg2 = create_signed_vote_message(
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_notarization_fallback_vote(Block {
                slot: 6,
                block_id: Hash::new_unique(),
            }),
            msg2_rank,
        );
        ctx.verifier
            .verify_and_send_batches(messages_to_batches(
                &[(
                    ConsensusMessage::Vote(msg1.clone()),
                    ctx.validator_keypairs[msg1_rank].node_keypair.pubkey(),
                )],
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();

        // The cap-1 channel is now full.  The second send hits Full and falls
        // back to a blocking send (see `send_votes_to_pool`); drain in a
        // background thread so the blocking send can complete.
        let pool_receiver = ctx.pool_receiver.clone();
        let drain = std::thread::spawn(move || {
            let m1 = pool_receiver.recv().expect("recv msg1");
            let m2 = pool_receiver.recv().expect("recv msg2");
            // No leftover messages on the channel after both deliveries.
            assert!(matches!(
                pool_receiver.try_recv(),
                Err(crossbeam_channel::TryRecvError::Empty)
            ));
            (m1, m2)
        });

        ctx.verifier
            .verify_and_send_batches(messages_to_batches(
                &[(
                    ConsensusMessage::Vote(msg2.clone()),
                    ctx.validator_keypairs[msg2_rank].node_keypair.pubkey(),
                )],
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();

        let (m1_recv, m2_recv) = drain.join().expect("drain joined");
        // Both messages were eventually delivered (no silent drop).
        assert_eq!(m1_recv, SigVerifiedBatch::Votes(vec![msg1]));
        assert_eq!(m2_recv, SigVerifiedBatch::Votes(vec![msg2]));
        // pool_sent counts every message that made it onto the channel,
        // whether via try_send or the blocking fallback.
        assert_eq!(ctx.verifier.stats.vote_stats.pool_sent.0, 2);
    }

    #[test]
    fn test_blssigverifier_send_packets_receiver_closed() {
        let mut ctx = TestContext::new();

        // Close the pool receiver to simulate a disconnected channel.
        drop(ctx.pool_receiver);

        let rank = 0;
        let msg = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_finalization_vote(5),
            rank,
        ));
        let messages = [(msg, ctx.validator_keypairs[rank].node_keypair.pubkey())];
        let result = ctx.verifier.verify_and_send_batches(messages_to_batches(
            &messages,
            ctx.verifier.cluster_info.my_shred_version(),
        ));
        assert!(result.is_err());
    }

    #[test]
    fn test_blssigverifier_send_discarded_packets() {
        let mut ctx = TestContext::new();

        let rank = 0;
        let message = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_finalization_vote(5),
            rank,
        ));
        let mut packet = message_to_packet(
            &message,
            ctx.verifier.cluster_info.my_shred_version(),
            ctx.validator_keypairs[rank].node_keypair.pubkey(),
        );
        packet.meta_mut().set_discard(true); // Manually discard

        let packet_batches = vec![BytesPacketBatch::from(vec![packet]).into()];

        ctx.verifier
            .verify_and_send_batches(packet_batches)
            .unwrap();
        expect_no_receive(&ctx.pool_receiver);
        assert_eq!(ctx.verifier.stats.vote_stats.pool_sent.0, 0);
        assert_eq!(ctx.verifier.stats.num_discarded_pkts.0, 1);
    }

    #[test]
    fn test_blssigverifier_verify_votes_all_valid() {
        let mut ctx = TestContext::new();

        let num_votes = 5;
        let mut packets = Vec::with_capacity(num_votes);
        let vote = Vote::new_skip_vote(42);
        let vote_payload =
            get_vote_payload_to_sign(vote, ctx.verifier.cluster_info.my_shred_version());

        for (i, validator_keypair) in ctx.validator_keypairs.iter().enumerate().take(num_votes) {
            let rank = i as u16;
            let bls_keypair = &validator_keypair.bls_keypair;
            let signature: Signature = bls_keypair.sign(&vote_payload).into();
            let consensus_message = ConsensusMessage::Vote(VoteMessage {
                vote,
                signature,
                rank,
            });
            packets.push(message_to_packet(
                &consensus_message,
                ctx.verifier.cluster_info.my_shred_version(),
                validator_keypair.node_keypair.pubkey(),
            ));
        }

        let packet_batches = vec![BytesPacketBatch::from(packets).into()];
        ctx.verifier
            .verify_and_send_batches(packet_batches)
            .unwrap();
        let batches = ctx.pool_receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(batches.len(), 1);
        match &batches[0] {
            SigVerifiedBatch::Votes(votes) => {
                assert_eq!(votes.len(), num_votes);
            }
            rest => panic!("unexpected type: {rest:?}"),
        }
    }

    #[test]
    fn test_blssigverifier_verify_votes_two_distinct_messages() {
        let mut ctx = TestContext::new();

        let num_votes_group1 = 3;
        let num_votes_group2 = 4;
        let num_votes = num_votes_group1 + num_votes_group2;
        let mut packets = Vec::with_capacity(num_votes);

        let vote1 = Vote::new_skip_vote(42);
        let vote2 = Vote::new_notarization_vote(Block {
            slot: 43,
            block_id: Hash::new_unique(),
        });

        // Group 1 votes
        for (i, validator_keypair) in ctx
            .validator_keypairs
            .iter()
            .enumerate()
            .take(num_votes_group1)
        {
            let msg = ConsensusMessage::Vote(create_signed_vote_message(
                &ctx.validator_keypairs,
                ctx.verifier.cluster_info.my_shred_version(),
                vote1,
                i,
            ));
            packets.push(message_to_packet(
                &msg,
                ctx.verifier.cluster_info.my_shred_version(),
                validator_keypair.node_keypair.pubkey(),
            ));
        }

        // Group 2 votes
        for (i, validator_keypair) in ctx
            .validator_keypairs
            .iter()
            .enumerate()
            .skip(num_votes_group1)
            .take(num_votes_group2)
        {
            let msg = ConsensusMessage::Vote(create_signed_vote_message(
                &ctx.validator_keypairs,
                ctx.verifier.cluster_info.my_shred_version(),
                vote2,
                i,
            ));
            packets.push(message_to_packet(
                &msg,
                ctx.verifier.cluster_info.my_shred_version(),
                validator_keypair.node_keypair.pubkey(),
            ));
        }

        let packet_batches = vec![BytesPacketBatch::from(packets).into()];
        ctx.verifier
            .verify_and_send_batches(packet_batches)
            .unwrap();
        let batches = ctx.pool_receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(batches.len(), 2);
        let total_votes_verified = batches
            .into_iter()
            .map(|batch| match batch {
                SigVerifiedBatch::Votes(votes) => votes.len(),
                rest => panic!("unexpected type: {rest:?}"),
            })
            .sum::<usize>();
        assert_eq!(total_votes_verified, num_votes);
        assert_eq!(
            ctx.verifier.stats.vote_stats.distinct_votes_stats.count(),
            1
        );
        assert_eq!(
            ctx.verifier
                .stats
                .vote_stats
                .distinct_votes_stats
                .mean::<u64>()
                .unwrap(),
            2
        );
    }

    #[test]
    fn test_blssigverifier_verify_votes_invalid_in_two_distinct_messages() {
        let mut ctx = TestContext::new();

        let num_votes = 5;
        let invalid_rank = 3; // This voter will sign vote 2 with an invalid signature.
        let mut packets = Vec::with_capacity(num_votes);

        let vote1 = Vote::new_skip_vote(42);
        let vote1_payload =
            get_vote_payload_to_sign(vote1, ctx.verifier.cluster_info.my_shred_version());
        let vote2 = Vote::new_skip_vote(43);
        let vote2_payload =
            get_vote_payload_to_sign(vote2, ctx.verifier.cluster_info.my_shred_version());
        let invalid_payload = get_vote_payload_to_sign(
            Vote::new_skip_vote(99),
            ctx.verifier.cluster_info.my_shred_version(),
        );

        for (i, validator_keypair) in ctx.validator_keypairs.iter().enumerate().take(num_votes) {
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
            packets.push(message_to_packet(
                &consensus_message,
                ctx.verifier.cluster_info.my_shred_version(),
                validator_keypair.node_keypair.pubkey(),
            ));
        }

        let packet_batches = vec![BytesPacketBatch::from(packets).into()];
        ctx.verifier
            .verify_and_send_batches(packet_batches)
            .unwrap();
        let batches = ctx.pool_receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(batches.len(), 2);
        let total_votes_verified = batches
            .into_iter()
            .map(|batch| match batch {
                SigVerifiedBatch::Votes(votes) => {
                    for vote in &votes {
                        if vote.vote == vote2 && vote.rank == invalid_rank {
                            panic!("invalid vote verified");
                        }
                    }
                    votes.len()
                }
                rest => panic!("unexpected type: {rest:?}"),
            })
            .sum::<usize>();
        assert_eq!(total_votes_verified, num_votes - 1);
    }

    #[test]
    fn test_blssigverifier_verify_votes_one_invalid_signature() {
        let mut ctx = TestContext::new();

        let num_votes = 5;
        let invalid_rank = 2;
        let mut packets = Vec::with_capacity(num_votes);
        let mut consensus_messages = Vec::with_capacity(num_votes); // ADDED: To hold messages for later comparison.

        let vote = Vote::new_skip_vote(42);
        let valid_vote_payload =
            get_vote_payload_to_sign(vote, ctx.verifier.cluster_info.my_shred_version());
        let invalid_vote_payload = get_vote_payload_to_sign(
            Vote::new_skip_vote(99),
            ctx.verifier.cluster_info.my_shred_version(),
        );

        for (i, validator_keypair) in ctx.validator_keypairs.iter().enumerate().take(num_votes) {
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

            packets.push(message_to_packet(
                &consensus_message,
                ctx.verifier.cluster_info.my_shred_version(),
                validator_keypair.node_keypair.pubkey(),
            ));
        }

        let packet_batches = vec![BytesPacketBatch::from(packets).into()];
        ctx.verifier
            .verify_and_send_batches(packet_batches)
            .unwrap();
        let batches: Vec<_> = ctx.pool_receiver.try_iter().collect();
        assert_eq!(batches.len(), 1);
        match &batches[0] {
            SigVerifiedBatch::Votes(votes) => {
                assert_eq!(votes.len(), num_votes - 1);
            }
            rest => panic!("unexpected type: {rest:?}"),
        }

        // Ensure the message with the invalid rank is not in the sent messages.
        let mut found_msg = false;
        match &batches[0] {
            SigVerifiedBatch::Votes(votes) => {
                for vote in votes {
                    if vote.rank == invalid_rank {
                        found_msg = true;
                        break;
                    }
                }
            }
            rest => panic!("unexpected type: {rest:?}"),
        }
        assert!(!found_msg);
    }

    #[test]
    fn test_verify_certificate_base2_valid() {
        let mut ctx = TestContext::new();

        // 2/3 of validators sign the cert.
        let num_signers = (ctx.validator_keypairs.len() * 2).div_ceil(3);
        let cert_type = CertificateType::Notarize(Block {
            slot: 10,
            block_id: Hash::new_unique(),
        });
        let cert = create_signed_certificate_message(
            ctx.verifier.cluster_info.my_shred_version(),
            &ctx.validator_keypairs,
            cert_type,
            &(0..num_signers).collect::<Vec<_>>(),
        );
        let consensus_message = ConsensusMessage::Certificate(cert);
        let packet_batches = messages_to_batches(
            &[(consensus_message, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier
            .verify_and_send_batches(packet_batches)
            .unwrap();
        assert_eq!(
            ctx.pool_receiver.try_iter().count(),
            1,
            "Valid Base2 certificate should be sent"
        );
    }

    #[test]
    fn test_verify_certificate_base2_just_enough_stake() {
        let mut ctx = TestContext::new();

        // 60% of validators sign the cert.
        let num_signers = (ctx.validator_keypairs.len() * 6).div_ceil(10);
        let cert_type = CertificateType::Notarize(Block {
            slot: 10,
            block_id: Hash::new_unique(),
        });
        let cert = create_signed_certificate_message(
            ctx.verifier.cluster_info.my_shred_version(),
            &ctx.validator_keypairs,
            cert_type,
            &(0..num_signers).collect::<Vec<_>>(),
        );
        let consensus_message = ConsensusMessage::Certificate(cert);
        let packet_batches = messages_to_batches(
            &[(consensus_message, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier
            .verify_and_send_batches(packet_batches)
            .unwrap();
        assert_eq!(
            ctx.pool_receiver.try_iter().count(),
            1,
            "Valid Base2 certificate should be sent"
        );
    }

    #[test]
    fn test_verify_certificate_base3_valid() {
        let mut ctx = TestContext::new();

        let slot = 20;
        let block_hash = Hash::new_unique();
        let block = Block {
            slot,
            block_id: block_hash,
        };
        let notarize_vote = Vote::new_notarization_vote(block);
        let notarize_fallback_vote = Vote::new_notarization_fallback_vote(block);
        let mut all_vote_messages = Vec::new();
        (0..4).for_each(|i| {
            all_vote_messages.push(create_signed_vote_message(
                &ctx.validator_keypairs,
                ctx.verifier.cluster_info.my_shred_version(),
                notarize_vote,
                i,
            ))
        });
        (4..7).for_each(|i| {
            all_vote_messages.push(create_signed_vote_message(
                &ctx.validator_keypairs,
                ctx.verifier.cluster_info.my_shred_version(),
                notarize_fallback_vote,
                i,
            ))
        });
        let cert_type = CertificateType::NotarizeFallback(block);
        let mut builder = CertificateBuilder::new(cert_type);
        builder
            .aggregate(&all_vote_messages)
            .expect("Failed to aggregate votes");
        let cert = builder.build().expect("Failed to build certificate");
        let consensus_message = ConsensusMessage::Certificate(cert);
        let packet_batches = messages_to_batches(
            &[(consensus_message, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier
            .verify_and_send_batches(packet_batches)
            .unwrap();
        assert_eq!(
            ctx.pool_receiver.try_iter().count(),
            1,
            "Valid Base3 certificate should be sent"
        );
    }

    #[test]
    fn test_verify_certificate_base3_just_enough_stake() {
        let mut ctx = TestContext::new();

        let slot = 20;
        let block_hash = Hash::new_unique();
        let block = Block {
            slot,
            block_id: block_hash,
        };
        let notarize_vote = Vote::new_notarization_vote(block);
        let notarize_fallback_vote = Vote::new_notarization_fallback_vote(block);
        let mut all_vote_messages = Vec::new();
        (0..4).for_each(|i| {
            all_vote_messages.push(create_signed_vote_message(
                &ctx.validator_keypairs,
                ctx.verifier.cluster_info.my_shred_version(),
                notarize_vote,
                i,
            ))
        });
        (4..6).for_each(|i| {
            all_vote_messages.push(create_signed_vote_message(
                &ctx.validator_keypairs,
                ctx.verifier.cluster_info.my_shred_version(),
                notarize_fallback_vote,
                i,
            ))
        });
        let cert_type = CertificateType::NotarizeFallback(block);
        let mut builder = CertificateBuilder::new(cert_type);
        builder
            .aggregate(&all_vote_messages)
            .expect("Failed to aggregate votes");
        let cert = builder.build().expect("Failed to build certificate");
        let consensus_message = ConsensusMessage::Certificate(cert);
        let packet_batches = messages_to_batches(
            &[(consensus_message, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier
            .verify_and_send_batches(packet_batches)
            .unwrap();
        assert_eq!(
            ctx.pool_receiver.try_iter().count(),
            1,
            "Valid Base3 certificate should be sent"
        );
    }

    #[test]
    fn test_verify_certificate_invalid_signature() {
        let mut ctx = TestContext::new();

        // 70% of validators sign.
        let num_signers = (ctx.validator_keypairs.len() * 7).div_ceil(10);
        let slot = 10;
        let block_hash = Hash::new_unique();
        let cert_type = CertificateType::Notarize(Block {
            slot,
            block_id: block_hash,
        });
        let mut bitmap = BitVec::<u8, Lsb0>::new();
        bitmap.resize(num_signers, false);
        for i in 0..num_signers {
            bitmap.set(i, true);
        }
        let encoded_bitmap = encode_base2(&bitmap).unwrap();

        let cert = Certificate {
            cert_type,
            signature: Signature([0; BLS_SIGNATURE_AFFINE_SIZE]), // Use a default/wrong signature
            bitmap: encoded_bitmap,
        };
        let consensus_message = ConsensusMessage::Certificate(cert);
        let packet_batches = messages_to_batches(
            &[(consensus_message, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier
            .verify_and_send_batches(packet_batches)
            .unwrap();
        expect_no_receive(&ctx.pool_receiver);
        assert_eq!(
            ctx.verifier
                .stats
                .cert_stats
                .certificate_verification_failed
                .0,
            1
        );
    }

    #[test]
    fn test_verify_mixed_valid_batch() {
        let mut ctx = TestContext::new();

        let mut packets = Vec::new();
        let num_votes = 2;

        let vote = Vote::new_skip_vote(42);
        let vote_payload =
            get_vote_payload_to_sign(vote, ctx.verifier.cluster_info.my_shred_version());
        for (i, validator_keypair) in ctx.validator_keypairs.iter().enumerate().take(num_votes) {
            let rank = i as u16;
            let bls_keypair = &validator_keypair.bls_keypair;
            let signature: Signature = bls_keypair.sign(&vote_payload).into();
            let consensus_message = ConsensusMessage::Vote(VoteMessage {
                vote,
                signature,
                rank,
            });
            packets.push(message_to_packet(
                &consensus_message,
                ctx.verifier.cluster_info.my_shred_version(),
                validator_keypair.node_keypair.pubkey(),
            ));
        }

        // 70% of validators sign.
        let num_signers = (ctx.validator_keypairs.len() * 7).div_ceil(10);
        let cert_type = CertificateType::Notarize(Block {
            slot: 10,
            block_id: Hash::new_unique(),
        });
        let cert_original_vote = Vote::new_notarization_vote(cert_type.to_block().unwrap());
        let cert_payload = get_vote_payload_to_sign(
            cert_original_vote,
            ctx.verifier.cluster_info.my_shred_version(),
        );

        let cert_vote_messages: Vec<VoteMessage> = (0..num_signers)
            .map(|i| {
                let signature = ctx.validator_keypairs[i].bls_keypair.sign(&cert_payload);
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
        packets.push(message_to_packet(
            &consensus_message_cert,
            ctx.verifier.cluster_info.my_shred_version(),
            Pubkey::new_unique(),
        ));

        let packet_batches = vec![BytesPacketBatch::from(packets).into()];
        ctx.verifier
            .verify_and_send_batches(packet_batches)
            .unwrap();
        let batches = ctx.pool_receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(batches.len(), 2);

        let batch_0_was_votes = match &batches[0] {
            SigVerifiedBatch::Votes(votes) => {
                assert_eq!(votes.len(), num_votes);
                true
            }
            SigVerifiedBatch::Certificates(certs) => {
                assert_eq!(certs.len(), 1);
                false
            }
        };

        match &batches[1] {
            SigVerifiedBatch::Votes(votes) => {
                assert!(!batch_0_was_votes);
                assert_eq!(votes.len(), num_votes);
            }
            SigVerifiedBatch::Certificates(certs) => {
                assert!(batch_0_was_votes);
                assert_eq!(certs.len(), 1);
            }
        }
        assert_eq!(ctx.verifier.stats.vote_stats.pool_sent.0, num_votes as u64);
        assert_eq!(ctx.verifier.stats.cert_stats.pool_sent.0, 1);
    }

    #[test]
    fn test_verify_vote_with_invalid_rank() {
        let mut ctx = TestContext::new();

        let invalid_rank = 999;
        let vote = Vote::new_skip_vote(42);
        let vote_payload =
            get_vote_payload_to_sign(vote, ctx.verifier.cluster_info.my_shred_version());
        let bls_keypair = &ctx.validator_keypairs[0].bls_keypair;
        let signature: Signature = bls_keypair.sign(&vote_payload).into();

        let consensus_message = ConsensusMessage::Vote(VoteMessage {
            vote,
            signature,
            rank: invalid_rank,
        });

        let packet_batches = messages_to_batches(
            &[(consensus_message, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );
        ctx.verifier
            .verify_and_send_batches(packet_batches)
            .unwrap();
        expect_no_receive(&ctx.pool_receiver);
        assert_eq!(ctx.verifier.stats.discard_vote_invalid_rank.0, 1);
    }

    #[test]
    fn test_verify_old_vote_and_cert() {
        let (message_sender, message_receiver) = bounded(1024);
        let (votes_for_repair_sender, _) = bounded(1024);
        let (consensus_metrics_sender, _) = bounded(1024);
        let (reward_votes_sender, _reward_votes_receiver) = bounded(1024);
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
        let (bank0, _temp_bank_forks) = bank0.wrap_with_bank_forks_for_tests();
        let bank5 = Bank::new_from_parent(bank0, SlotLeader::default(), 5);
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
        let (_packet_sender, packet_receiver) = bounded(1024);
        let mut sig_verifier = SigVerifier::new(
            SigVerifierContext {
                migration_status: Arc::new(MigrationStatus::default()),
                banlist: new_test_banlist(),
                sharable_banks,
                cluster_info,
                leader_schedule,
                num_threads: 4,
                generated_cert_types: Arc::new(GeneratedCertTypes::default()),
            },
            SigVerifierChannels {
                packet_receiver,
                channel_to_repair: votes_for_repair_sender,
                channel_to_reward: reward_votes_sender,
                channel_to_pool: message_sender,
                channel_to_metrics: consensus_metrics_sender,
            },
        );

        let rank = 0;
        let vote = Vote::new_skip_vote(2);
        let vote_payload =
            get_vote_payload_to_sign(vote, sig_verifier.cluster_info.my_shred_version());
        let bls_keypair = &validator_keypairs[rank].bls_keypair;
        let signature: Signature = bls_keypair.sign(&vote_payload).into();
        let consensus_message_vote = ConsensusMessage::Vote(VoteMessage {
            vote,
            signature,
            rank: rank.try_into().unwrap(),
        });
        let packet_batches_vote = messages_to_batches(
            &[(
                consensus_message_vote,
                validator_keypairs[rank].node_keypair.pubkey(),
            )],
            sig_verifier.cluster_info.my_shred_version(),
        );

        sig_verifier
            .verify_and_send_batches(packet_batches_vote)
            .unwrap();
        expect_no_receive(&message_receiver);
        assert_eq!(sig_verifier.stats.num_old_votes_received.0, 1);

        let cert = create_signed_certificate_message(
            sig_verifier.cluster_info.my_shred_version(),
            &validator_keypairs,
            CertificateType::Finalize(3),
            &[0], // Signer rank 0
        );
        let consensus_message_cert = ConsensusMessage::Certificate(cert);
        let packet_batches_cert = messages_to_batches(
            &[(consensus_message_cert, Pubkey::new_unique())],
            sig_verifier.cluster_info.my_shred_version(),
        );

        sig_verifier
            .verify_and_send_batches(packet_batches_cert)
            .unwrap();
        expect_no_receive(&message_receiver);
        assert_eq!(sig_verifier.stats.num_old_certs_received.0, 1);
        assert_eq!(sig_verifier.stats.num_old_votes_received.0, 1);
    }

    #[test]
    fn test_verified_certs_are_skipped() {
        let mut ctx = TestContext::new();

        // 80% of validators sign.
        let num_signers = (ctx.validator_keypairs.len() * 8).div_ceil(10);
        let slot = 10;
        let block_hash = Hash::new_unique();
        let block = Block {
            slot,
            block_id: block_hash,
        };
        let cert_type = CertificateType::Notarize(block);
        let original_vote = Vote::new_notarization_vote(block);
        let signed_payload =
            get_vote_payload_to_sign(original_vote, ctx.verifier.cluster_info.my_shred_version());
        let mut vote_messages: Vec<VoteMessage> = (0..num_signers)
            .map(|i| {
                let signature = ctx.validator_keypairs[i].bls_keypair.sign(&signed_payload);
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
        let packet_batches1 = messages_to_batches(
            &[(consensus_message1, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier
            .verify_and_send_batches(packet_batches1)
            .unwrap();

        assert_eq!(ctx.pool_receiver.try_iter().count(), 1);
        assert_eq!(ctx.verifier.stats.num_verified_certs_received.0, 0);
        assert_eq!(ctx.verifier.stats.cert_stats.certs_to_sig_verify.0, 1);

        vote_messages.pop(); // Remove one signature
        let mut builder2 = CertificateBuilder::new(cert_type);
        builder2
            .aggregate(&vote_messages)
            .expect("Failed to aggregate votes");
        let cert2 = builder2.build().expect("Failed to build certificate");
        let consensus_message2 = ConsensusMessage::Certificate(cert2);
        let packet_batches2 = messages_to_batches(
            &[(consensus_message2, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier.stats = SigVerifierStats::new(ctx.verifier.sharable_banks.root().slot());
        ctx.verifier
            .verify_and_send_batches(packet_batches2)
            .unwrap();
        expect_no_receive(&ctx.pool_receiver);
        assert_eq!(ctx.verifier.stats.num_verified_certs_received.0, 1);
        assert_eq!(ctx.verifier.stats.cert_stats.certs_to_sig_verify.0, 0);
    }

    #[test]
    fn test_same_type_certs_verify_until_first_valid() {
        let mut ctx = TestContext::new();

        let cert_type = CertificateType::Notarize(Block {
            slot: 10,
            block_id: Hash::new_unique(),
        });
        let cert1 = create_signed_certificate_message(
            ctx.verifier.cluster_info.my_shred_version(),
            &ctx.validator_keypairs,
            cert_type,
            &(0..7).collect::<Vec<_>>(),
        );
        let cert2 = create_signed_certificate_message(
            ctx.verifier.cluster_info.my_shred_version(),
            &ctx.validator_keypairs,
            cert_type,
            &(1..8).collect::<Vec<_>>(),
        );
        let packet_batches = messages_to_batches(
            &[
                (ConsensusMessage::Certificate(cert1), Pubkey::new_unique()),
                (ConsensusMessage::Certificate(cert2), Pubkey::new_unique()),
            ],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier
            .verify_and_send_batches(packet_batches)
            .unwrap();

        let batches = ctx.pool_receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(batches.len(), 1);
        match &batches[0] {
            SigVerifiedBatch::Certificates(certs) => assert_eq!(certs.len(), 1),
            rest => panic!("unexpected type: {rest:?}"),
        }
        assert_eq!(ctx.verifier.stats.cert_stats.certs_to_sig_verify.0, 1);
        assert_eq!(ctx.verifier.stats.cert_stats.sig_verified_certs.0, 1);
        assert_eq!(ctx.verifier.stats.cert_stats.redundant_certs_skipped.0, 1);
        assert_eq!(
            ctx.verifier.stats.cert_stats.unnecessary_certs_verified.0,
            0
        );
    }

    #[test]
    fn test_same_type_certs_try_next_candidate_after_failure() {
        let mut ctx = TestContext::new();

        let cert_type = CertificateType::Notarize(Block {
            slot: 10,
            block_id: Hash::new_unique(),
        });
        let num_signers = 7;
        let mut bitmap = BitVec::<u8, Lsb0>::new();
        bitmap.resize(num_signers, false);
        for i in 0..num_signers {
            bitmap.set(i, true);
        }
        let invalid_cert = Certificate {
            cert_type,
            signature: Signature([0; BLS_SIGNATURE_AFFINE_SIZE]),
            bitmap: encode_base2(&bitmap).unwrap(),
        };
        let valid_cert = create_signed_certificate_message(
            ctx.verifier.cluster_info.my_shred_version(),
            &ctx.validator_keypairs,
            cert_type,
            &(0..num_signers).collect::<Vec<_>>(),
        );
        let invalid_sender = Pubkey::new_unique();
        let valid_sender = Pubkey::new_unique();
        let redundant_sender = Pubkey::new_unique();
        let packet_batches = messages_to_batches(
            &[
                (ConsensusMessage::Certificate(invalid_cert), invalid_sender),
                (
                    ConsensusMessage::Certificate(valid_cert.clone()),
                    valid_sender,
                ),
                (ConsensusMessage::Certificate(valid_cert), redundant_sender),
            ],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier
            .verify_and_send_batches(packet_batches)
            .unwrap();

        let batches = ctx.pool_receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(batches.len(), 1);
        match &batches[0] {
            SigVerifiedBatch::Certificates(certs) => assert_eq!(certs.len(), 1),
            rest => panic!("unexpected type: {rest:?}"),
        }
        assert!(ctx.banlist.is_banned(&invalid_sender));
        assert!(!ctx.banlist.is_banned(&valid_sender));
        assert!(!ctx.banlist.is_banned(&redundant_sender));
        assert_eq!(ctx.verifier.stats.cert_stats.certs_to_sig_verify.0, 2);
        assert_eq!(ctx.verifier.stats.cert_stats.sig_verified_certs.0, 1);
        assert_eq!(
            ctx.verifier
                .stats
                .cert_stats
                .certificate_verification_failed
                .0,
            1
        );
        assert_eq!(ctx.verifier.stats.cert_stats.redundant_certs_skipped.0, 1);
    }

    #[test]
    fn test_banlist_not_updated_for_valid_vote_and_cert() {
        let mut ctx = TestContext::new();

        let rank = 0;
        let vote_message = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_skip_vote(42),
            rank,
        ));
        let cert_message = ConsensusMessage::Certificate(create_signed_certificate_message(
            ctx.verifier.cluster_info.my_shred_version(),
            &ctx.validator_keypairs,
            CertificateType::Notarize(Block {
                slot: 43,
                block_id: Hash::new_unique(),
            }),
            &(0..7).collect::<Vec<_>>(),
        ));
        let vote_sender = ctx.validator_keypairs[rank].node_keypair.pubkey();
        let cert_sender = Pubkey::new_unique();
        let packet_batches = messages_to_batches(
            &[(vote_message, vote_sender), (cert_message, cert_sender)],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier
            .verify_and_send_batches(packet_batches)
            .unwrap();
        assert_eq!(ctx.pool_receiver.try_iter().count(), 2);
        assert!(!ctx.banlist.is_banned(&vote_sender));
        assert!(!ctx.banlist.is_banned(&cert_sender));
    }

    #[test]
    fn test_banlist_updates_for_invalid_votes() {
        let mut ctx = TestContext::new();

        let vote = Vote::new_skip_vote(42);
        let valid_payload =
            get_vote_payload_to_sign(vote, ctx.verifier.cluster_info.my_shred_version());
        let invalid_payload = get_vote_payload_to_sign(
            Vote::new_skip_vote(999),
            ctx.verifier.cluster_info.my_shred_version(),
        );
        let invalid_indexes = [1usize, 3usize];
        let messages: Vec<_> = ctx
            .validator_keypairs
            .iter()
            .enumerate()
            .take(5)
            .map(|(i, keypair)| {
                let signature = if invalid_indexes.contains(&i) {
                    keypair.bls_keypair.sign(&invalid_payload).into()
                } else {
                    keypair.bls_keypair.sign(&valid_payload).into()
                };
                let message = ConsensusMessage::Vote(VoteMessage {
                    vote,
                    signature,
                    rank: i as u16,
                });
                (message, keypair.node_keypair.pubkey())
            })
            .collect();

        ctx.verifier
            .verify_and_send_batches(messages_to_batches(
                &messages,
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();
        let batches = ctx.pool_receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(batches.len(), 1);
        match &batches[0] {
            SigVerifiedBatch::Votes(votes) => {
                assert_eq!(votes.len(), 3);
            }
            rest => panic!("unexpected type: {rest:?}"),
        }

        for (i, (_, sender)) in messages.iter().enumerate() {
            if invalid_indexes.contains(&i) {
                assert!(
                    ctx.banlist.is_banned(sender),
                    "invalid sender {i} should be banned"
                );
            } else {
                assert!(
                    !ctx.banlist.is_banned(sender),
                    "valid sender {i} should not be banned"
                );
            }
        }
    }

    #[test]
    fn test_banlist_updates_for_invalid_certificates() {
        let mut ctx = TestContext::new();

        let invalid_indexes = [0usize, 4usize];
        let messages: Vec<_> = (0..5)
            .map(|i| {
                let slot = 10 + i as u64;
                let cert_type = CertificateType::Notarize(Block {
                    slot,
                    block_id: Hash::new_unique(),
                });
                let mut cert = create_signed_certificate_message(
                    ctx.verifier.cluster_info.my_shred_version(),
                    &ctx.validator_keypairs,
                    cert_type,
                    &(0..7).collect::<Vec<_>>(),
                );
                if invalid_indexes.contains(&i) {
                    cert.signature = Signature([0; BLS_SIGNATURE_AFFINE_SIZE]);
                }
                (ConsensusMessage::Certificate(cert), Pubkey::new_unique())
            })
            .collect();

        ctx.verifier
            .verify_and_send_batches(messages_to_batches(
                &messages,
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();
        let batches = ctx.pool_receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(batches.len(), 1);
        match &batches[0] {
            SigVerifiedBatch::Certificates(certs) => {
                assert_eq!(certs.len(), 3);
            }
            rest => panic!("unexpected type: {rest:?}"),
        }

        for (i, (_, sender)) in messages.iter().enumerate() {
            if invalid_indexes.contains(&i) {
                assert!(
                    ctx.banlist.is_banned(sender),
                    "invalid sender {i} should be banned"
                );
            } else {
                assert!(
                    !ctx.banlist.is_banned(sender),
                    "valid sender {i} should not be banned"
                );
            }
        }
    }

    #[test]
    fn generated_certs_are_filtered() {
        let mut ctx = TestContext::new();
        let slot = 1235;
        let cert_type = CertificateType::Skip(slot);
        ctx.generated_cert_types.insert_cert(cert_type);
        let cert = create_signed_certificate_message(
            ctx.verifier.cluster_info.my_shred_version(),
            &ctx.validator_keypairs,
            cert_type,
            &(0..ctx.validator_keypairs.len()).collect::<Vec<usize>>(),
        );
        let consensus_message = ConsensusMessage::Certificate(cert);
        let packet_batches = messages_to_batches(
            &[(consensus_message, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );
        ctx.verifier
            .verify_and_send_batches(packet_batches)
            .unwrap();
        assert_eq!(ctx.verifier.stats.num_generated_certs_received.0, 1);
    }

    #[test]
    fn msgs_too_far_in_future_are_dropped() {
        let mut ctx = TestContext::new();
        let slot = ctx.verifier.sharable_banks.root().slot() + NUM_SLOTS_FOR_VERIFY + 1;
        let cert_type = CertificateType::Skip(slot);
        let cert = create_signed_certificate_message(
            ctx.verifier.cluster_info.my_shred_version(),
            &ctx.validator_keypairs,
            cert_type,
            &(0..ctx.validator_keypairs.len()).collect::<Vec<usize>>(),
        );
        let cert = ConsensusMessage::Certificate(cert);
        let rank = 0;
        let vote = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_skip_vote(slot),
            rank,
        ));
        let packet_batches = messages_to_batches(
            &[
                (cert, Pubkey::new_unique()),
                (vote, ctx.validator_keypairs[rank].node_keypair.pubkey()),
            ],
            ctx.verifier.cluster_info.my_shred_version(),
        );
        ctx.verifier
            .verify_and_send_batches(packet_batches)
            .unwrap();
        assert_eq!(ctx.verifier.stats.cert_stats.too_far_in_future.0, 1);
        assert_eq!(ctx.verifier.stats.vote_stats.too_far_in_future.0, 1);
    }

    fn messages_to_batches(
        messages: &[(ConsensusMessage, Pubkey)],
        shred_version: u16,
    ) -> Vec<PacketBatch> {
        let packets: Vec<_> = messages
            .iter()
            .map(|(message, remote_pubkey)| {
                message_to_packet(message, shred_version, *remote_pubkey)
            })
            .collect();
        vec![BytesPacketBatch::from(packets).into()]
    }
}
