//! The BLS signature verifier.

use {
    crate::{
        certs_verifier::CertsVerifier, generated_cert_types::GeneratedCertTypes,
        msg_receiver::MsgReceiver, rewards::RewardInput, votes_processor::VotesProcessor,
        votes_verifier::VotesVerifier,
    },
    agave_votor_messages::{
        VerifiedVotorSlotsMessage, consensus_message::Block,
        metric_types::ConsensusMetricsEventSender, migration::MigrationStatus,
        sig_verified_messages::SigVerifiedBatch, unverified_vote_message::UnverifiedCertificate,
    },
    agave_votor_transport::endpoint::{BanSender, Datagram},
    crossbeam_channel::{Receiver, Sender, bounded},
    rayon::ThreadPoolBuilder,
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_runtime::bank_forks::SharableBanks,
    solana_streamer::evicting_sender::EvictingSender,
    std::{
        sync::{Arc, RwLock, atomic::AtomicBool},
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
};

/// If a certificate is so many slots in the future relative to the root slot, it is considered
/// invalid and discarded.
///
/// At 200ms slot times, 30K slots is 100mins.  We do not expect a node to catch up if it has
/// fallen so far behind.
pub const NUM_SLOTS_FOR_VERIFY: Slot = 30_000;

/// If we receive an invalid certificate or vote from a QUIC connection, we ban the sender.
/// We ban the sender for 10 seconds which prevents DoS but allows for recovery in case of instability.
pub(super) const BAN_TIMEOUT: Duration = Duration::from_secs(10);

pub struct SigVerifierContext {
    pub migration_status: Arc<MigrationStatus>,
    /// Sends peer ban commands to the transport endpoint.
    pub ban_sender: BanSender,
    pub sharable_banks: SharableBanks,
    pub highest_parent_ready: Arc<RwLock<(Slot, Block)>>,
    pub cluster_info: Arc<ClusterInfo>,
    pub leader_schedule: Arc<LeaderScheduleCache>,
    pub num_threads: usize,
    pub generated_cert_types: Arc<GeneratedCertTypes>,
}

pub struct SigVerifierChannels {
    pub(crate) datagrams_receiver: Receiver<Datagram>,
    pub(crate) certificate_receiver: Receiver<(Slot, UnverifiedCertificate)>,
    pub(crate) channel_to_repair: EvictingSender<VerifiedVotorSlotsMessage>,
    pub(crate) channel_to_reward: Sender<RewardInput>,
    pub(crate) channel_to_pool: Sender<SigVerifiedBatch>,
    pub(crate) channel_to_metrics: ConsensusMetricsEventSender,
}

impl SigVerifierChannels {
    pub fn new(
        datagrams_receiver: Receiver<Datagram>,
        certificate_receiver: Receiver<(Slot, UnverifiedCertificate)>,
        channel_to_repair: EvictingSender<VerifiedVotorSlotsMessage>,
        channel_to_reward: Sender<RewardInput>,
        channel_to_pool: Sender<SigVerifiedBatch>,
        channel_to_metrics: ConsensusMetricsEventSender,
    ) -> Self {
        Self {
            datagrams_receiver,
            certificate_receiver,
            channel_to_repair,
            channel_to_reward,
            channel_to_pool,
            channel_to_metrics,
        }
    }
}

pub struct BlsSigverifyService {
    msg_receiver: JoinHandle<()>,
    votes_verifier: JoinHandle<()>,
    certs_verifier: JoinHandle<()>,
    votes_processor: JoinHandle<()>,
}

impl BlsSigverifyService {
    pub fn new(
        exit: Arc<AtomicBool>,
        context: SigVerifierContext,
        channels: SigVerifierChannels,
    ) -> Self {
        let (unverified_votes_sender, unverified_votes_receiver) = bounded(2);
        let (unverified_certs_sender, unverified_certs_receiver) = bounded(2);
        let (verified_votes_sender, verified_votes_receiver) = bounded(2);
        let msg_receiver = MsgReceiver::new(
            exit.clone(),
            context.migration_status,
            channels.datagrams_receiver,
            channels.certificate_receiver,
            context.highest_parent_ready,
            context.cluster_info.clone(),
            context.sharable_banks.clone(),
            context.leader_schedule.clone(),
            context.ban_sender.clone(),
            context.generated_cert_types,
            unverified_votes_sender,
            unverified_certs_sender,
        );

        let thread_pool = Arc::new(
            ThreadPoolBuilder::new()
                .num_threads(context.num_threads)
                .thread_name(|i| format!("solBls{i:02}"))
                .build()
                .unwrap(),
        );
        let votes_verifier = VotesVerifier::new(
            exit.clone(),
            unverified_votes_receiver,
            verified_votes_sender,
            context.ban_sender.clone(),
            thread_pool.clone(),
        );

        let votes_processor = VotesProcessor::new(
            exit.clone(),
            verified_votes_receiver,
            context.sharable_banks.clone(),
            context.cluster_info.clone(),
            context.leader_schedule,
            channels.channel_to_repair,
            channels.channel_to_reward,
            channels.channel_to_pool.clone(),
            channels.channel_to_metrics,
        );

        let certs_verifier = CertsVerifier::new(
            exit,
            unverified_certs_receiver,
            context.cluster_info,
            context.sharable_banks,
            channels.channel_to_pool,
            context.ban_sender,
            thread_pool,
        );

        let msg_receiver = Builder::new()
            .name("solBlsMsg".to_string())
            .spawn(move || msg_receiver.run())
            .unwrap();
        let votes_verifier = Builder::new()
            .name("solBlsVote".to_string())
            .spawn(move || votes_verifier.run())
            .unwrap();
        let votes_processor = Builder::new()
            .name("solBlsVoteP".to_string())
            .spawn(move || votes_processor.run())
            .unwrap();
        let certs_verifier = Builder::new()
            .name("solBlsCert".to_string())
            .spawn(move || certs_verifier.run())
            .unwrap();
        Self {
            msg_receiver,
            votes_verifier,
            certs_verifier,
            votes_processor,
        }
    }

    pub fn join(self) -> thread::Result<()> {
        self.msg_receiver.join()?;
        self.votes_verifier.join()?;
        self.certs_verifier.join()?;
        self.votes_processor.join()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::msg_receiver::{MAX_VOTE_SLOT_DISTANCE_FROM_PARENT_READY, max_admitted_vote_slot},
        agave_bls_cert_verify::cert_verify::{
            test_create_base2_certificate, test_create_base2_unverified_certificate,
            test_create_base3_certificate,
        },
        agave_votor_messages::{
            certificate::{Certificate, CertificateType},
            consensus_message::{Block, ConsensusMessage, VoteMessage},
            metric_types::ConsensusMetricsEventReceiver,
            sig_verified_messages::VoteAggregate,
            vote::Vote,
            wire::{VersionedWireConsensusMessage, get_vote_payload_to_sign},
        },
        agave_votor_transport::endpoint::{BanCommand, stub_ban_channel_for_tests},
        bitvec::prelude::{BitVec, Lsb0},
        bytes::Bytes,
        crossbeam_channel::{Receiver, RecvTimeoutError, bounded},
        solana_bls_signatures::{
            BLS_SIGNATURE_AFFINE_SIZE, Keypair as BLSKeypair, Signature, signature::SignatureAffine,
        },
        solana_epoch_schedule::EpochSchedule,
        solana_gossip::contact_info::ContactInfo,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_net_utils::SocketAddrSpace,
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
        std::{
            collections::HashSet,
            net::{Ipv4Addr, SocketAddr},
            num::NonZero,
            sync::{RwLock, atomic::Ordering},
        },
        tokio::sync::mpsc,
    };

    fn new_vote_aggregate(bank: &Bank, mut msg: VoteMessage) -> VoteAggregate {
        let rank_map = bank
            .epoch_stakes_from_slot(msg.vote.slot())
            .unwrap()
            .bls_pubkey_to_rank_map();
        msg.stake = rank_map
            .get_pubkey_stake_entry(msg.rank as usize)
            .unwrap()
            .stake;
        let max_validators = rank_map.len();
        VoteAggregate::new_from_verified_vote(max_validators, msg)
    }

    const RECEIVE_TIMEOUT: Duration = Duration::from_secs(5);
    const QUIET_TIMEOUT: Duration = Duration::from_millis(100);

    struct PipelineHarness {
        migration_status: Arc<MigrationStatus>,
        sharable_banks: SharableBanks,
        highest_parent_ready: Arc<RwLock<(Slot, Block)>>,
        cluster_info: Arc<ClusterInfo>,
        datagrams_sender: Sender<Datagram>,
        certificate_sender: Sender<(Slot, UnverifiedCertificate)>,
        exit: Arc<AtomicBool>,
        service: Option<BlsSigverifyService>,
    }

    impl PipelineHarness {
        fn new(
            context: SigVerifierContext,
            channels: SigVerifierChannels,
            datagrams_sender: Sender<Datagram>,
            certificate_sender: Sender<(Slot, UnverifiedCertificate)>,
        ) -> Self {
            let exit = Arc::new(AtomicBool::new(false));
            Self {
                migration_status: context.migration_status.clone(),
                sharable_banks: context.sharable_banks.clone(),
                highest_parent_ready: context.highest_parent_ready.clone(),
                cluster_info: context.cluster_info.clone(),
                datagrams_sender,
                certificate_sender,
                service: Some(BlsSigverifyService::new(exit.clone(), context, channels)),
                exit,
            }
        }

        fn verify_and_send_datagrams(&mut self, datagrams: Vec<Datagram>) -> Result<(), ()> {
            self.verify_and_send_inputs(datagrams, vec![])
        }

        fn verify_and_send_inputs(
            &mut self,
            datagrams: Vec<Datagram>,
            certificates: Vec<(Slot, UnverifiedCertificate)>,
        ) -> Result<(), ()> {
            for datagram in datagrams {
                self.datagrams_sender.send(datagram).map_err(|_| ())?;
            }
            for certificate in certificates {
                self.certificate_sender.send(certificate).map_err(|_| ())?;
            }
            Ok(())
        }
    }

    impl Drop for PipelineHarness {
        fn drop(&mut self) {
            self.exit.store(true, Ordering::Relaxed);
            let result = self.service.take().unwrap().join();
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }

    /// Count votes and certificates rather than batches, which depend on thread scheduling.
    fn receive_batches(
        receiver: &Receiver<SigVerifiedBatch>,
        expected_votes: usize,
        expected_certs: usize,
    ) -> Vec<SigVerifiedBatch> {
        let deadline = std::time::Instant::now()
            .checked_add(RECEIVE_TIMEOUT)
            .unwrap();
        let mut batches = Vec::new();
        let (mut votes, mut certs) = (0, 0);
        while votes < expected_votes || certs < expected_certs {
            let batch = receiver.recv_deadline(deadline).unwrap();
            match &batch {
                SigVerifiedBatch::Votes(aggregates) => {
                    votes = votes.saturating_add(
                        aggregates
                            .iter()
                            .map(VoteAggregate::num_votes)
                            .sum::<usize>(),
                    );
                }
                SigVerifiedBatch::Certificates(certificates) => {
                    certs = certs.saturating_add(certificates.len())
                }
            }
            batches.push(batch);
        }
        assert_eq!((votes, certs), (expected_votes, expected_certs));
        expect_no_receive(receiver);
        batches
    }

    struct TestContext {
        validator_keypairs: Vec<ValidatorVoteKeypairs>,
        ban_receiver: mpsc::Receiver<BanCommand>,
        repair_receiver: Receiver<VerifiedVotorSlotsMessage>,
        _reward_receiver: Receiver<RewardInput>,
        pool_receiver: Receiver<SigVerifiedBatch>,
        metrics_receiver: ConsensusMetricsEventReceiver,
        generated_cert_types: Arc<GeneratedCertTypes>,
        bank_forks: Arc<RwLock<BankForks>>,
        // Drop output receivers before joining workers that may be blocked on a send.
        verifier: PipelineHarness,
    }

    impl TestContext {
        fn new() -> Self {
            Self::new_with_migration_status(MigrationStatus::post_migration_status())
        }

        fn new_with_migration_status(migration_status: MigrationStatus) -> Self {
            let (channel_to_pool, pool_receiver) = bounded(1024);
            Self::new_with_channels_and_migration(channel_to_pool, pool_receiver, migration_status)
        }

        fn banned_pubkeys(&mut self, expected: usize) -> HashSet<Pubkey> {
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap()
                .block_on(async {
                    let mut banned = HashSet::new();
                    for _ in 0..expected {
                        let BanCommand { peer, .. } =
                            tokio::time::timeout(RECEIVE_TIMEOUT, self.ban_receiver.recv())
                                .await
                                .unwrap()
                                .unwrap();
                        assert!(banned.insert(peer), "duplicate ban");
                    }
                    assert!(
                        tokio::time::timeout(QUIET_TIMEOUT, self.ban_receiver.recv())
                            .await
                            .is_err()
                    );
                    banned
                })
        }

        fn new_with_pool_channel(
            channel_to_pool: Sender<SigVerifiedBatch>,
            pool_receiver: Receiver<SigVerifiedBatch>,
        ) -> Self {
            Self::new_with_channels_and_migration(
                channel_to_pool,
                pool_receiver,
                MigrationStatus::post_migration_status(),
            )
        }

        fn new_with_channels_and_migration(
            channel_to_pool: Sender<SigVerifiedBatch>,
            pool_receiver: Receiver<SigVerifiedBatch>,
            migration_status: MigrationStatus,
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

            let (channel_to_repair, repair_receiver) = EvictingSender::new_bounded(1024);
            let (channel_to_reward, reward_receiver) = bounded(1024);
            let (datagrams_sender, datagrams_receiver) = bounded(1024);
            let (certificate_sender, certificate_receiver) = bounded(1024);
            let (channel_to_metrics, metrics_receiver) = bounded(1024);

            let generated_cert_types = Arc::new(GeneratedCertTypes::default());
            let (ban_sender, ban_receiver) = stub_ban_channel_for_tests(1024);
            let highest_parent_ready = Arc::new(RwLock::new((
                NUM_SLOTS_FOR_VERIFY,
                Block::new_unique(NUM_SLOTS_FOR_VERIFY.saturating_sub(1)),
            )));
            let verifier = PipelineHarness::new(
                SigVerifierContext {
                    migration_status: Arc::new(migration_status),
                    ban_sender,
                    sharable_banks,
                    highest_parent_ready,
                    cluster_info,
                    leader_schedule,
                    num_threads: 4,
                    generated_cert_types: generated_cert_types.clone(),
                },
                SigVerifierChannels::new(
                    datagrams_receiver,
                    certificate_receiver,
                    channel_to_repair,
                    channel_to_reward,
                    channel_to_pool,
                    channel_to_metrics,
                ),
                datagrams_sender,
                certificate_sender,
            );
            Self {
                validator_keypairs,
                verifier,
                ban_receiver,
                repair_receiver,
                _reward_receiver: reward_receiver,
                pool_receiver,
                metrics_receiver,
                generated_cert_types,
                bank_forks,
            }
        }

        fn bls_keypairs(&self) -> Vec<BLSKeypair> {
            self.validator_keypairs
                .iter()
                .map(|k| k.bls_keypair.clone())
                .collect()
        }
    }

    fn create_signed_vote_message(
        root_bank: &Bank,
        validator_keypairs: &[ValidatorVoteKeypairs],
        shred_version: u16,
        vote: Vote,
        rank: usize,
    ) -> VoteMessage {
        let rank_map = root_bank.get_rank_map(vote.slot()).unwrap();
        let stake = rank_map.get_pubkey_stake_entry(rank).unwrap().stake;
        create_signed_vote_message_with_stake(validator_keypairs, shred_version, vote, rank, stake)
    }

    fn create_signed_vote_message_with_stake(
        validator_keypairs: &[ValidatorVoteKeypairs],
        shred_version: u16,
        vote: Vote,
        rank: usize,
        stake: NonZero<u64>,
    ) -> VoteMessage {
        let bls_keypair = &validator_keypairs[rank].bls_keypair;
        let payload = get_vote_payload_to_sign(vote, shred_version);
        let signature = SignatureAffine::from(bls_keypair.sign(&payload));
        VoteMessage {
            vote,
            signature,
            rank: rank as u16,
            stake,
        }
    }

    fn expect_no_receive<T: std::fmt::Debug>(receiver: &Receiver<T>) {
        match receiver.recv_timeout(QUIET_TIMEOUT).unwrap_err() {
            RecvTimeoutError::Timeout => (),
            e => {
                panic!("unexpected error {e:?}");
            }
        }
    }

    /// Builds a fake datagram carrying `message`, matching what transport would deliver to us.
    fn message_to_datagram(
        message: &ConsensusMessage,
        shred_version: u16,
        peer_pubkey: Pubkey,
    ) -> Datagram {
        let msg = VersionedWireConsensusMessage::new(message.clone(), shred_version);
        datagram_from_bytes(wincode::serialize(&msg).unwrap(), peer_pubkey)
    }

    fn datagram_from_bytes(message: impl Into<Bytes>, peer_pubkey: Pubkey) -> Datagram {
        Datagram {
            peer_pubkey,
            peer_address: SocketAddr::from((Ipv4Addr::LOCALHOST, 1)), // this does not bind
            message: message.into(),
        }
    }

    #[test]
    fn test_blockstore_certificate_requires_active_alpenglow() {
        let mut ctx = TestContext::new_with_migration_status(MigrationStatus::default());
        let shred_version = ctx.verifier.cluster_info.my_shred_version();
        let block = Block::new_unique(1);
        let certificate = test_create_base2_unverified_certificate(
            &ctx.bls_keypairs(),
            shred_version,
            CertificateType::FinalizeFast(block),
            &[0, 1, 2, 3, 4, 5, 6, 7],
        );
        let slot = 2;

        ctx.verifier
            .verify_and_send_inputs(vec![], vec![(slot, certificate.clone())])
            .unwrap();
        expect_no_receive(&ctx.pool_receiver);

        ctx.verifier.migration_status.enable_alpenglow_for_tests();
        ctx.verifier
            .verify_and_send_inputs(vec![], vec![(slot, certificate)])
            .unwrap();
        let SigVerifiedBatch::Certificates(certs) =
            ctx.pool_receiver.recv_timeout(RECEIVE_TIMEOUT).unwrap()
        else {
            panic!("expected a certificate batch");
        };
        assert_eq!(certs.len(), 1);
        assert_eq!(certs[0].cert_type, CertificateType::FinalizeFast(block));
    }

    #[test]
    fn test_old_blockstore_certificate_is_filtered() {
        let mut ctx = TestContext::new_with_migration_status(MigrationStatus::default());
        let shred_version = ctx.verifier.cluster_info.my_shred_version();
        let block = Block::new_unique(1);
        let certificate = test_create_base2_unverified_certificate(
            &ctx.bls_keypairs(),
            shred_version,
            CertificateType::FinalizeFast(block),
            &[0, 1, 2, 3, 4, 5, 6, 7],
        );
        let slot = 6;
        let root_bank =
            Bank::new_from_parent(ctx.verifier.sharable_banks.root(), SlotLeader::default(), 5);
        ctx.verifier.migration_status.enable_alpenglow_for_tests();

        {
            let mut bank_forks = ctx.bank_forks.write().unwrap();
            bank_forks.insert(root_bank);
            bank_forks.set_root(5, None, None);
        }
        ctx.verifier
            .verify_and_send_inputs(vec![], vec![(slot, certificate)])
            .unwrap();
        expect_no_receive(&ctx.pool_receiver);
    }

    #[test]
    fn test_blssigverifier_send_packets() {
        let mut ctx = TestContext::new();

        let vote_rank1 = 2;
        let cert_ranks = [0, 2, 3, 4, 5, 7, 8, 9];
        let cert_type = CertificateType::Finalize(4);
        let vote_message1 = create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_finalization_vote(5),
            vote_rank1,
        );
        let cert = test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
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
            .verify_and_send_datagrams(messages_to_datagrams(
                &messages1,
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();
        assert_eq!(receive_batches(&ctx.pool_receiver, 1, 1).len(), 2);
        let mut received_verified_votes1 =
            ctx.repair_receiver.recv_timeout(RECEIVE_TIMEOUT).unwrap();
        assert_eq!(
            received_verified_votes1.remove(&5).unwrap(),
            vec![ctx.validator_keypairs[vote_rank1].vote_keypair.pubkey()]
        );

        let vote_rank2 = 3;
        let vote_message2 = create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_unique_notar(6),
            vote_rank2,
        );
        let messages2 = [(
            ConsensusMessage::Vote(vote_message2),
            ctx.validator_keypairs[vote_rank2].node_keypair.pubkey(),
        )];
        ctx.verifier
            .verify_and_send_datagrams(messages_to_datagrams(
                &messages2,
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();

        assert_eq!(receive_batches(&ctx.pool_receiver, 1, 0).len(), 1);
        let mut received_verified_votes2 =
            ctx.repair_receiver.recv_timeout(RECEIVE_TIMEOUT).unwrap();
        assert_eq!(
            received_verified_votes2.remove(&6).unwrap(),
            vec![ctx.validator_keypairs[vote_rank2].vote_keypair.pubkey()]
        );

        let vote_rank3 = 9;
        let vote_message3 = create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_unique_notar_fallback(7),
            vote_rank3,
        );
        let messages3 = [(
            ConsensusMessage::Vote(vote_message3),
            ctx.validator_keypairs[vote_rank3].node_keypair.pubkey(),
        )];
        ctx.verifier
            .verify_and_send_datagrams(messages_to_datagrams(
                &messages3,
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();
        assert_eq!(receive_batches(&ctx.pool_receiver, 1, 0).len(), 1);
        let mut received_verified_votes3 =
            ctx.repair_receiver.recv_timeout(RECEIVE_TIMEOUT).unwrap();
        assert_eq!(
            received_verified_votes3.remove(&7).unwrap(),
            vec![ctx.validator_keypairs[vote_rank3].vote_keypair.pubkey()]
        );
    }

    #[test]
    fn test_blssigverifier_verify_malformed() {
        let mut ctx = TestContext::new();

        let datagrams = vec![datagram_from_bytes(Bytes::new(), Pubkey::new_unique())];
        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();

        // Expect no messages since the packet was malformed
        expect_no_receive(&ctx.pool_receiver);

        // Send a packet too far in the future
        let rank = 0;
        let vote_message_no_stakes = create_signed_vote_message_with_stake(
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_finalization_vote(5_000_000_000), // very high slot
            rank,
            NonZero::new(123).unwrap(),
        );
        let messages_no_stakes = [(
            ConsensusMessage::Vote(vote_message_no_stakes),
            ctx.validator_keypairs[rank].node_keypair.pubkey(),
        )];

        ctx.verifier
            .verify_and_send_datagrams(messages_to_datagrams(
                &messages_no_stakes,
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();

        // Expect no messages since the packet was malformed
        expect_no_receive(&ctx.pool_receiver);

        // Send a packet with invalid rank
        let vote = Vote::new_finalization_vote(5);
        let payload = get_vote_payload_to_sign(vote, ctx.verifier.cluster_info.my_shred_version());
        let signature = SignatureAffine::from(ctx.validator_keypairs[0].bls_keypair.sign(&payload));
        let messages_invalid_rank = [(
            ConsensusMessage::Vote(VoteMessage {
                vote: Vote::new_finalization_vote(5),
                signature,
                rank: 1000, // Invalid rank
                stake: NonZero::new(123).unwrap(),
            }),
            Pubkey::new_unique(),
        )];
        ctx.verifier
            .verify_and_send_datagrams(messages_to_datagrams(
                &messages_invalid_rank,
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();

        // Expect no messages since the packet was malformed
        expect_no_receive(&ctx.pool_receiver);
    }

    #[test]
    fn test_shred_version_mismatch() {
        let mut ctx = TestContext::new();
        let rank = 0;
        let shred_version = ctx.verifier.cluster_info.my_shred_version();
        let sender = ctx.validator_keypairs[rank].node_keypair.pubkey();
        let vote = Vote::new_finalization_vote(5);
        let valid_message = create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            shred_version,
            vote,
            rank,
        );
        let wrong_version = shred_version.wrapping_add(1);
        let wrong_message = create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            wrong_version,
            vote,
            rank,
        );
        ctx.verifier
            .verify_and_send_datagrams(vec![
                message_to_datagram(
                    &ConsensusMessage::Vote(wrong_message),
                    wrong_version,
                    sender,
                ),
                // The same vote with the correct version must still be accepted.
                message_to_datagram(
                    &ConsensusMessage::Vote(valid_message.clone()),
                    shred_version,
                    sender,
                ),
            ])
            .unwrap();
        assert_eq!(
            receive_batches(&ctx.pool_receiver, 1, 0),
            vec![SigVerifiedBatch::Votes(vec![new_vote_aggregate(
                &ctx.verifier.sharable_banks.root(),
                valid_message,
            )])]
        );
        assert!(ctx.banned_pubkeys(0).is_empty());
    }

    #[test]
    fn test_blssigverifier_send_packets_channel_full() {
        agave_logger::setup();
        let (channel_to_pool, pool_receiver) = crossbeam_channel::bounded(1);
        let mut ctx = TestContext::new_with_pool_channel(channel_to_pool, pool_receiver);

        let msg1_rank = 0;
        let msg2_rank = 2;
        let msg1 = create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_finalization_vote(5),
            msg1_rank,
        );
        let msg2 = create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_unique_notar_fallback(6),
            msg2_rank,
        );
        ctx.verifier
            .verify_and_send_datagrams(messages_to_datagrams(
                &[(
                    ConsensusMessage::Vote(msg1.clone()),
                    ctx.validator_keypairs[msg1_rank].node_keypair.pubkey(),
                )],
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();

        let deadline = std::time::Instant::now() + RECEIVE_TIMEOUT;
        while !ctx.pool_receiver.is_full() {
            assert!(
                std::time::Instant::now() < deadline,
                "pool channel did not fill"
            );
            std::thread::sleep(Duration::from_millis(1));
        }

        // Queue another vote while the pool channel is full, then drain both deliveries.
        ctx.verifier
            .verify_and_send_datagrams(messages_to_datagrams(
                &[(
                    ConsensusMessage::Vote(msg2.clone()),
                    ctx.validator_keypairs[msg2_rank].node_keypair.pubkey(),
                )],
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();

        let m1_recv = ctx.pool_receiver.recv_timeout(RECEIVE_TIMEOUT).unwrap();
        let m2_recv = ctx.pool_receiver.recv_timeout(RECEIVE_TIMEOUT).unwrap();
        expect_no_receive(&ctx.pool_receiver);
        // Both messages were eventually delivered (no silent drop).
        let bank = ctx.verifier.sharable_banks.root();
        let batch1 = SigVerifiedBatch::Votes(vec![new_vote_aggregate(&bank, msg1)]);
        let batch2 = SigVerifiedBatch::Votes(vec![new_vote_aggregate(&bank, msg2)]);
        assert_eq!(m1_recv, batch1);
        assert_eq!(m2_recv, batch2);
    }

    #[test]
    fn test_blssigverifier_send_packets_receiver_closed() {
        let mut ctx = TestContext::new();

        // Close the pool receiver to simulate a disconnected channel.
        drop(ctx.pool_receiver);

        let rank = 0;
        let msg = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_finalization_vote(5),
            rank,
        ));
        let messages = [(msg, ctx.validator_keypairs[rank].node_keypair.pubkey())];
        let result = ctx
            .verifier
            .verify_and_send_datagrams(messages_to_datagrams(
                &messages,
                ctx.verifier.cluster_info.my_shred_version(),
            ));
        result.unwrap();
        let deadline = std::time::Instant::now() + RECEIVE_TIMEOUT;
        while !ctx
            .verifier
            .service
            .as_ref()
            .unwrap()
            .votes_processor
            .is_finished()
        {
            assert!(
                std::time::Instant::now() < deadline,
                "pool worker did not exit"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
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
            let signature = SignatureAffine::from(bls_keypair.sign(&vote_payload));
            let consensus_message = ConsensusMessage::Vote(VoteMessage {
                vote,
                signature,
                rank,
                stake: NonZero::new(123).unwrap(),
            });
            packets.push(message_to_datagram(
                &consensus_message,
                ctx.verifier.cluster_info.my_shred_version(),
                validator_keypair.node_keypair.pubkey(),
            ));
        }

        ctx.verifier.verify_and_send_datagrams(packets).unwrap();
        let batches = receive_batches(&ctx.pool_receiver, num_votes, 0);
        for batch in batches {
            let SigVerifiedBatch::Votes(aggregates) = batch else {
                panic!("expected votes");
            };
            assert!(aggregates.iter().all(|aggregate| aggregate.vote() == &vote));
        }
        let deadline = std::time::Instant::now() + RECEIVE_TIMEOUT;
        let mut voter_ids = Vec::new();
        while voter_ids.len() < num_votes {
            let (_, events) = ctx.metrics_receiver.recv_deadline(deadline).unwrap();
            for event in events {
                let agave_votor_messages::metric_types::ConsensusMetricsEvent::Vote {
                    id,
                    vote: received_vote,
                } = event
                else {
                    panic!("expected a vote metric");
                };
                assert_eq!(received_vote, vote);
                voter_ids.push(id);
            }
        }
        voter_ids.sort_unstable();
        let mut expected_ids = ctx
            .validator_keypairs
            .iter()
            .take(num_votes)
            .map(|keys| keys.vote_keypair.pubkey())
            .collect::<Vec<_>>();
        expected_ids.sort_unstable();
        assert_eq!(voter_ids, expected_ids);
        expect_no_receive(&ctx.metrics_receiver);
    }

    #[test]
    fn test_blssigverifier_verify_votes_two_distinct_messages() {
        let mut ctx = TestContext::new();

        let num_votes_group1 = 3;
        let num_votes_group2 = 4;
        let num_votes = num_votes_group1 + num_votes_group2;
        let mut packets = Vec::with_capacity(num_votes);

        let vote1 = Vote::new_skip_vote(42);
        let vote2 = Vote::new_unique_notar(43);

        // Group 1 votes
        for (i, validator_keypair) in ctx
            .validator_keypairs
            .iter()
            .enumerate()
            .take(num_votes_group1)
        {
            let msg = ConsensusMessage::Vote(create_signed_vote_message(
                &ctx.verifier.sharable_banks.root(),
                &ctx.validator_keypairs,
                ctx.verifier.cluster_info.my_shred_version(),
                vote1,
                i,
            ));
            packets.push(message_to_datagram(
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
                &ctx.verifier.sharable_banks.root(),
                &ctx.validator_keypairs,
                ctx.verifier.cluster_info.my_shred_version(),
                vote2,
                i,
            ));
            packets.push(message_to_datagram(
                &msg,
                ctx.verifier.cluster_info.my_shred_version(),
                validator_keypair.node_keypair.pubkey(),
            ));
        }

        ctx.verifier.verify_and_send_datagrams(packets).unwrap();
        let batches = receive_batches(&ctx.pool_receiver, num_votes, 0);
        let mut verified_votes = Vec::new();
        for batch in batches {
            let SigVerifiedBatch::Votes(aggregates) = batch else {
                panic!("expected votes");
            };
            for aggregate in aggregates {
                verified_votes.extend(
                    aggregate
                        .ranks()
                        .iter_ones()
                        .map(|rank| (rank, *aggregate.vote())),
                );
            }
        }
        verified_votes.sort_unstable_by_key(|(rank, _)| *rank);
        let expected_votes = (0..num_votes)
            .map(|rank| {
                (
                    rank,
                    if rank < num_votes_group1 {
                        vote1
                    } else {
                        vote2
                    },
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(verified_votes, expected_votes);
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
                stake: NonZero::new(123).unwrap(),
            });
            packets.push(message_to_datagram(
                &consensus_message,
                ctx.verifier.cluster_info.my_shred_version(),
                validator_keypair.node_keypair.pubkey(),
            ));
        }

        ctx.verifier.verify_and_send_datagrams(packets).unwrap();
        let batches = receive_batches(&ctx.pool_receiver, num_votes - 1, 0);
        let mut verified_votes = Vec::new();
        for batch in batches {
            let SigVerifiedBatch::Votes(aggregates) = batch else {
                panic!("expected votes");
            };
            for aggregate in aggregates {
                verified_votes.extend(
                    aggregate
                        .ranks()
                        .iter_ones()
                        .map(|rank| (rank, *aggregate.vote())),
                );
            }
        }
        verified_votes.sort_unstable_by_key(|(rank, _)| *rank);
        let expected_votes = (0..num_votes)
            .filter(|rank| *rank != invalid_rank as usize)
            .map(|rank| (rank, if rank < 2 { vote1 } else { vote2 }))
            .collect::<Vec<_>>();
        assert_eq!(verified_votes, expected_votes);
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
                stake: NonZero::new(123).unwrap(),
            });

            consensus_messages.push(consensus_message.clone());

            packets.push(message_to_datagram(
                &consensus_message,
                ctx.verifier.cluster_info.my_shred_version(),
                validator_keypair.node_keypair.pubkey(),
            ));
        }

        ctx.verifier.verify_and_send_datagrams(packets).unwrap();
        let batches: Vec<_> = receive_batches(&ctx.pool_receiver, num_votes - 1, 0);
        for batch in batches {
            let SigVerifiedBatch::Votes(aggregates) = batch else {
                panic!("expected votes");
            };
            for aggregate in aggregates {
                assert_eq!(aggregate.vote(), &vote);
                assert!(!*aggregate.ranks().get(invalid_rank as usize).unwrap());
            }
        }
    }

    #[test]
    fn test_verify_certificate_base2_valid() {
        let mut ctx = TestContext::new();

        // 2/3 of validators sign the cert.
        let num_signers = (ctx.validator_keypairs.len() * 2).div_ceil(3);
        let cert_type = CertificateType::new_unique_notar(10);
        let cert = test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &(0..num_signers).collect::<Vec<_>>(),
        );
        let consensus_message = ConsensusMessage::Certificate(cert);
        let datagrams = messages_to_datagrams(
            &[(consensus_message, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();
        assert_eq!(
            receive_batches(&ctx.pool_receiver, 0, 1).len(),
            1,
            "Valid Base2 certificate should be sent"
        );
    }

    #[test]
    fn test_verify_certificate_base2_just_enough_stake() {
        let mut ctx = TestContext::new();

        // 60% of validators sign the cert.
        let num_signers = (ctx.validator_keypairs.len() * 6).div_ceil(10);
        let cert_type = CertificateType::new_unique_notar(10);
        let cert = test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &(0..num_signers).collect::<Vec<_>>(),
        );
        let consensus_message = ConsensusMessage::Certificate(cert);
        let datagrams = messages_to_datagrams(
            &[(consensus_message, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();
        assert_eq!(
            receive_batches(&ctx.pool_receiver, 0, 1).len(),
            1,
            "Valid Base2 certificate should be sent"
        );
    }

    #[test]
    fn test_verify_certificate_base3_valid() {
        let mut ctx = TestContext::new();

        let slot = 20;
        let cert_type = CertificateType::new_unique_notar_fallback(slot);
        let cert = test_create_base3_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &[0, 1, 2, 3],
            &[4, 5, 6],
        );
        let consensus_message = ConsensusMessage::Certificate(cert);
        let datagrams = messages_to_datagrams(
            &[(consensus_message, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();
        assert_eq!(
            receive_batches(&ctx.pool_receiver, 0, 1).len(),
            1,
            "Valid Base3 certificate should be sent"
        );
    }

    #[test]
    fn test_verify_certificate_base3_just_enough_stake() {
        let mut ctx = TestContext::new();
        let slot = 20;
        let cert_type = CertificateType::new_unique_notar_fallback(slot);
        let cert = test_create_base3_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &[0, 1, 2, 3],
            &[4, 5],
        );
        let consensus_message = ConsensusMessage::Certificate(cert);
        let datagrams = messages_to_datagrams(
            &[(consensus_message, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();
        assert_eq!(
            receive_batches(&ctx.pool_receiver, 0, 1).len(),
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
        let cert_type = CertificateType::new_unique_notar(slot);
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
        let datagrams = messages_to_datagrams(
            &[(consensus_message, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();
        expect_no_receive(&ctx.pool_receiver);
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
            let signature = bls_keypair.sign(&vote_payload).into();
            let consensus_message = ConsensusMessage::Vote(VoteMessage {
                vote,
                signature,
                rank,
                stake: NonZero::new(123).unwrap(),
            });
            packets.push(message_to_datagram(
                &consensus_message,
                ctx.verifier.cluster_info.my_shred_version(),
                validator_keypair.node_keypair.pubkey(),
            ));
        }

        // 70% of validators sign.
        let num_signers = (ctx.validator_keypairs.len() * 7).div_ceil(10);
        let cert_type = CertificateType::new_unique_notar(10);
        let cert = test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &(0..num_signers).into_iter().collect::<Vec<_>>(),
        );
        let consensus_message_cert = ConsensusMessage::Certificate(cert);
        packets.push(message_to_datagram(
            &consensus_message_cert,
            ctx.verifier.cluster_info.my_shred_version(),
            Pubkey::new_unique(),
        ));

        ctx.verifier.verify_and_send_datagrams(packets).unwrap();
        let batches = receive_batches(&ctx.pool_receiver, num_votes, 1);
        for batch in batches {
            match batch {
                SigVerifiedBatch::Votes(aggregates) => {
                    assert!(aggregates.iter().all(|aggregate| aggregate.vote() == &vote));
                }
                SigVerifiedBatch::Certificates(certs) => {
                    assert!(certs.iter().all(|cert| cert.cert_type == cert_type));
                }
            }
        }
    }

    #[test]
    fn test_verify_vote_with_invalid_rank() {
        let mut ctx = TestContext::new();

        let invalid_rank = 999;
        let vote = Vote::new_skip_vote(42);
        let vote_payload =
            get_vote_payload_to_sign(vote, ctx.verifier.cluster_info.my_shred_version());
        let bls_keypair = &ctx.validator_keypairs[0].bls_keypair;
        let signature = SignatureAffine::from(bls_keypair.sign(&vote_payload));

        let consensus_message = ConsensusMessage::Vote(VoteMessage {
            vote,
            signature,
            rank: invalid_rank,
            stake: NonZero::new(123).unwrap(),
        });

        let datagrams = messages_to_datagrams(
            &[(consensus_message, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );
        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();
        expect_no_receive(&ctx.pool_receiver);
    }

    #[test]
    fn test_verify_old_vote_and_cert() {
        let mut ctx = TestContext::new();
        let bank5 =
            Bank::new_from_parent(ctx.verifier.sharable_banks.root(), SlotLeader::default(), 5);
        {
            let mut bank_forks = ctx.bank_forks.write().unwrap();
            bank_forks.insert(bank5);
            bank_forks.set_root(5, None, None);
        }

        let rank = 0;
        let shred_version = ctx.verifier.cluster_info.my_shred_version();
        let vote = create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            shred_version,
            Vote::new_skip_vote(2),
            rank,
        );
        let datagrams_vote = messages_to_datagrams(
            &[(
                ConsensusMessage::Vote(vote),
                ctx.validator_keypairs[rank].node_keypair.pubkey(),
            )],
            shred_version,
        );

        ctx.verifier
            .verify_and_send_datagrams(datagrams_vote)
            .unwrap();
        expect_no_receive(&ctx.pool_receiver);

        let cert = test_create_base2_certificate(
            &ctx.bls_keypairs(),
            shred_version,
            CertificateType::Finalize(3),
            &[0, 1, 2, 3, 4, 5, 6],
        );
        let datagrams_cert = messages_to_datagrams(
            &[(ConsensusMessage::Certificate(cert), Pubkey::new_unique())],
            shred_version,
        );

        ctx.verifier
            .verify_and_send_datagrams(datagrams_cert)
            .unwrap();
        expect_no_receive(&ctx.pool_receiver);
    }

    #[test]
    fn test_verified_certs_are_skipped() {
        let mut ctx = TestContext::new();

        // 80% of validators sign.
        let num_signers = (ctx.validator_keypairs.len() * 8).div_ceil(10);
        let slot = 10;
        let cert_type = CertificateType::new_unique_notar(slot);
        let cert1 = test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &(0..num_signers).into_iter().collect::<Vec<_>>(),
        );
        let consensus_message1 = ConsensusMessage::Certificate(cert1);
        let datagrams1 = messages_to_datagrams(
            &[(consensus_message1, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier.verify_and_send_datagrams(datagrams1).unwrap();

        assert_eq!(receive_batches(&ctx.pool_receiver, 0, 1).len(), 1);

        let cert2 = test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &(0..num_signers - 1).into_iter().collect::<Vec<_>>(),
        );
        let consensus_message2 = ConsensusMessage::Certificate(cert2);
        let datagrams2 = messages_to_datagrams(
            &[(consensus_message2, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier.verify_and_send_datagrams(datagrams2).unwrap();
        expect_no_receive(&ctx.pool_receiver);
    }

    #[test]
    fn test_same_type_certs_verify_until_first_valid() {
        let mut ctx = TestContext::new();

        let cert_type = CertificateType::new_unique_notar(10);
        let cert1 = test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &(0..7).collect::<Vec<_>>(),
        );
        let cert2 = test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &(1..8).collect::<Vec<_>>(),
        );
        let datagrams = messages_to_datagrams(
            &[
                (ConsensusMessage::Certificate(cert1), Pubkey::new_unique()),
                (ConsensusMessage::Certificate(cert2), Pubkey::new_unique()),
            ],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();

        let batches = receive_batches(&ctx.pool_receiver, 0, 1);
        assert_eq!(batches.len(), 1);
        match &batches[0] {
            SigVerifiedBatch::Certificates(certs) => assert_eq!(certs.len(), 1),
            rest => panic!("unexpected type: {rest:?}"),
        }
    }

    #[test]
    fn test_same_type_certs_try_next_candidate_after_failure() {
        let mut ctx = TestContext::new();

        let cert_type = CertificateType::new_unique_notar(10);
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
        let valid_cert = test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &(0..num_signers).collect::<Vec<_>>(),
        );
        let invalid_sender = Pubkey::new_unique();
        let valid_sender = Pubkey::new_unique();
        let redundant_sender = Pubkey::new_unique();
        let datagrams = messages_to_datagrams(
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

        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();

        let batches = receive_batches(&ctx.pool_receiver, 0, 1);
        assert_eq!(batches.len(), 1);
        match &batches[0] {
            SigVerifiedBatch::Certificates(certs) => assert_eq!(certs.len(), 1),
            rest => panic!("unexpected type: {rest:?}"),
        }
        let banlist = ctx.banned_pubkeys(1);
        assert!(banlist.contains(&invalid_sender), "Invalid cert -> ban");
        assert!(!banlist.contains(&valid_sender), "Valid certs ok");
        assert!(!banlist.contains(&redundant_sender), "Redundant certs ok");
    }

    #[test]
    fn test_banlist_not_updated_for_valid_vote_and_cert() {
        let mut ctx = TestContext::new();

        let rank = 0;
        let vote_message = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_skip_vote(42),
            rank,
        ));
        let cert_message = ConsensusMessage::Certificate(test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            CertificateType::new_unique_notar(43),
            &(0..7).collect::<Vec<_>>(),
        ));
        let vote_sender = ctx.validator_keypairs[rank].node_keypair.pubkey();
        let cert_sender = Pubkey::new_unique();
        let datagrams = messages_to_datagrams(
            &[(vote_message, vote_sender), (cert_message, cert_sender)],
            ctx.verifier.cluster_info.my_shred_version(),
        );

        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();
        assert_eq!(receive_batches(&ctx.pool_receiver, 1, 1).len(), 2);
        let banned = ctx.banned_pubkeys(0);
        assert!(!banned.contains(&vote_sender));
        assert!(!banned.contains(&cert_sender));
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
                    stake: NonZero::new(123).unwrap(),
                });
                (message, keypair.node_keypair.pubkey())
            })
            .collect();

        ctx.verifier
            .verify_and_send_datagrams(messages_to_datagrams(
                &messages,
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();
        receive_batches(&ctx.pool_receiver, 3, 0);

        let banned = ctx.banned_pubkeys(2);
        for (i, (_, sender)) in messages.iter().enumerate() {
            if invalid_indexes.contains(&i) {
                assert!(
                    banned.contains(sender),
                    "invalid sender {i} should be banned"
                );
            } else {
                assert!(
                    !banned.contains(sender),
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
                let cert_type = CertificateType::new_unique_notar(slot);
                let mut cert = test_create_base2_certificate(
                    &ctx.bls_keypairs(),
                    ctx.verifier.cluster_info.my_shred_version(),
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
            .verify_and_send_datagrams(messages_to_datagrams(
                &messages,
                ctx.verifier.cluster_info.my_shred_version(),
            ))
            .unwrap();
        receive_batches(&ctx.pool_receiver, 0, 3);

        let banned = ctx.banned_pubkeys(2);
        for (i, (_, sender)) in messages.iter().enumerate() {
            if invalid_indexes.contains(&i) {
                assert!(
                    banned.contains(sender),
                    "invalid sender {i} should be banned"
                );
            } else {
                assert!(
                    !banned.contains(sender),
                    "valid sender {i} should not be banned"
                );
            }
        }
    }

    #[test]
    fn generated_certs_are_filtered() {
        let mut ctx = TestContext::new();
        let shred_version = ctx.verifier.cluster_info.my_shred_version();
        let generated_type = CertificateType::Finalize(5);
        let control_type = CertificateType::Finalize(6);
        ctx.generated_cert_types.insert_cert(generated_type);
        let messages = [generated_type, control_type].map(|cert_type| {
            (
                ConsensusMessage::Certificate(test_create_base2_certificate(
                    &ctx.bls_keypairs(),
                    shred_version,
                    cert_type,
                    &(0..ctx.validator_keypairs.len()).collect::<Vec<_>>(),
                )),
                Pubkey::new_unique(),
            )
        });
        ctx.verifier
            .verify_and_send_datagrams(messages_to_datagrams(&messages, shred_version))
            .unwrap();
        let batches = receive_batches(&ctx.pool_receiver, 0, 1);
        let SigVerifiedBatch::Certificates(certs) = &batches[0] else {
            panic!("expected certificates");
        };
        assert_eq!(certs[0].cert_type, control_type);
        assert!(ctx.banned_pubkeys(0).is_empty());
    }

    #[test]
    fn duplicate_votes_across_batches_are_not_banned() {
        let mut ctx = TestContext::new();
        let shred_version = ctx.verifier.cluster_info.my_shred_version();
        let sender = ctx.validator_keypairs[0].node_keypair.pubkey();
        let message = create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            shred_version,
            Vote::new_skip_vote(5),
            0,
        );
        ctx.verifier
            .verify_and_send_datagrams(messages_to_datagrams(
                &[(ConsensusMessage::Vote(message.clone()), sender)],
                shred_version,
            ))
            .unwrap();
        receive_batches(&ctx.pool_receiver, 1, 0);

        let control = create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            shred_version,
            Vote::new_skip_vote(6),
            0,
        );
        ctx.verifier
            .verify_and_send_datagrams(messages_to_datagrams(
                &[
                    (ConsensusMessage::Vote(message), sender),
                    (ConsensusMessage::Vote(control.clone()), sender),
                ],
                shred_version,
            ))
            .unwrap();
        assert_eq!(
            receive_batches(&ctx.pool_receiver, 1, 0),
            vec![SigVerifiedBatch::Votes(vec![new_vote_aggregate(
                &ctx.verifier.sharable_banks.root(),
                control,
            )])]
        );
        assert!(ctx.banned_pubkeys(0).is_empty());
    }

    #[test]
    fn conflicting_votes_across_batches_are_banned() {
        let mut ctx = TestContext::new();
        let shred_version = ctx.verifier.cluster_info.my_shred_version();
        let sender = ctx.validator_keypairs[0].node_keypair.pubkey();
        for (index, vote) in [Vote::new_skip_vote(5), Vote::new_finalization_vote(5)]
            .into_iter()
            .enumerate()
        {
            let message = create_signed_vote_message(
                &ctx.verifier.sharable_banks.root(),
                &ctx.validator_keypairs,
                shred_version,
                vote,
                0,
            );
            ctx.verifier
                .verify_and_send_datagrams(messages_to_datagrams(
                    &[(ConsensusMessage::Vote(message), sender)],
                    shred_version,
                ))
                .unwrap();
            if index == 0 {
                receive_batches(&ctx.pool_receiver, 1, 0);
            } else {
                assert_eq!(ctx.banned_pubkeys(1), HashSet::from([sender]));
                expect_no_receive(&ctx.pool_receiver);
            }
        }
    }

    #[test]
    fn votes_are_bounded_by_highest_parent_ready() {
        let mut ctx = TestContext::new();
        let highest_parent_ready_slot = 100;
        *ctx.verifier.highest_parent_ready.write().unwrap() = (
            highest_parent_ready_slot,
            // The ParentReady target slot, rather than the parent block's slot, sets the bound.
            Block::new_unique(7),
        );
        let max_vote_slot = highest_parent_ready_slot + MAX_VOTE_SLOT_DISTANCE_FROM_PARENT_READY;
        let first_rejected_vote_slot = max_vote_slot + 1;

        let accepted_vote_rank = 0;
        let accepted_vote = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_finalization_vote(max_vote_slot),
            accepted_vote_rank,
        ));
        let rejected_vote_rank = 1;
        let rejected_vote = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_skip_vote(first_rejected_vote_slot),
            rejected_vote_rank,
        ));

        // Certificates retain the root-relative bound and are not limited by ParentReady.
        let cert_type = CertificateType::Finalize(first_rejected_vote_slot);
        let cert = test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &(0..ctx.validator_keypairs.len()).collect::<Vec<usize>>(),
        );
        let cert = ConsensusMessage::Certificate(cert);
        let datagrams = messages_to_datagrams(
            &[
                (
                    accepted_vote,
                    ctx.validator_keypairs[accepted_vote_rank]
                        .node_keypair
                        .pubkey(),
                ),
                (
                    rejected_vote,
                    ctx.validator_keypairs[rejected_vote_rank]
                        .node_keypair
                        .pubkey(),
                ),
                (cert, Pubkey::new_unique()),
            ],
            ctx.verifier.cluster_info.my_shred_version(),
        );
        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();

        assert_eq!(receive_batches(&ctx.pool_receiver, 1, 1).len(), 2);
        let mut repaired_slots = ctx.repair_receiver.recv_timeout(RECEIVE_TIMEOUT).unwrap();
        assert_eq!(repaired_slots.len(), 1);
        assert_eq!(
            repaired_slots.remove(&max_vote_slot).unwrap(),
            vec![
                ctx.validator_keypairs[accepted_vote_rank]
                    .vote_keypair
                    .pubkey(),
            ]
        );
        expect_no_receive(&ctx.repair_receiver);
    }

    #[test]
    fn genesis_votes_bypass_future_bound_during_migration() {
        let mut ctx = TestContext::new_with_migration_status(MigrationStatus::default());
        let highest_parent_ready_slot = 100;
        *ctx.verifier.highest_parent_ready.write().unwrap() = (
            highest_parent_ready_slot,
            Block {
                slot: highest_parent_ready_slot,
                block_id: Hash::new_unique(),
            },
        );
        let max_vote_slot = highest_parent_ready_slot + MAX_VOTE_SLOT_DISTANCE_FROM_PARENT_READY;
        let migration_slot = ctx.verifier.migration_status.record_feature_activation(200);
        let genesis_slot = migration_slot.saturating_sub(1);
        assert!(genesis_slot > max_vote_slot);

        let genesis_block = Block {
            slot: genesis_slot,
            block_id: Hash::new_unique(),
        };
        ctx.verifier
            .migration_status
            .set_genesis_block(genesis_block);
        let genesis_vote_rank = 0;
        let genesis_vote = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_genesis_vote(genesis_block),
            genesis_vote_rank,
        ));

        // Normal votes remain bounded by ParentReady even when they target the exact block.
        let normal_vote_rank = 1;
        let normal_vote = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_notarization_vote(genesis_block),
            normal_vote_rank,
        ));

        // The migration slot itself cannot be the Genesis slot.
        let different_slot_genesis_vote_rank = 2;
        let different_slot_genesis_vote = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_genesis_vote(Block {
                slot: migration_slot,
                block_id: genesis_block.block_id,
            }),
            different_slot_genesis_vote_rank,
        ));

        // The Genesis exception does not require the locally discovered block's hash either.
        let different_hash_genesis_vote_rank = 3;
        let different_hash_genesis_vote = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.verifier.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.verifier.cluster_info.my_shred_version(),
            Vote::new_genesis_vote(Block {
                slot: genesis_slot,
                block_id: Hash::new_unique(),
            }),
            different_hash_genesis_vote_rank,
        ));

        let datagrams = messages_to_datagrams(
            &[
                (
                    genesis_vote,
                    ctx.validator_keypairs[genesis_vote_rank]
                        .node_keypair
                        .pubkey(),
                ),
                (
                    normal_vote,
                    ctx.validator_keypairs[normal_vote_rank]
                        .node_keypair
                        .pubkey(),
                ),
                (
                    different_slot_genesis_vote,
                    ctx.validator_keypairs[different_slot_genesis_vote_rank]
                        .node_keypair
                        .pubkey(),
                ),
                (
                    different_hash_genesis_vote,
                    ctx.validator_keypairs[different_hash_genesis_vote_rank]
                        .node_keypair
                        .pubkey(),
                ),
            ],
            ctx.verifier.cluster_info.my_shred_version(),
        );
        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();

        let aggregates = receive_batches(&ctx.pool_receiver, 2, 0)
            .into_iter()
            .flat_map(|batch| match batch {
                SigVerifiedBatch::Votes(aggregates) => aggregates,
                SigVerifiedBatch::Certificates(_) => panic!("expected only vote batches"),
            })
            .collect::<Vec<_>>();
        assert_eq!(aggregates.len(), 2);
        assert!(
            aggregates
                .iter()
                .all(|aggregate| aggregate.vote().is_genesis_vote())
        );
        expect_no_receive(&ctx.repair_receiver);
    }

    #[test]
    fn max_admitted_vote_slot_handles_startup_and_overflow() {
        assert_eq!(max_admitted_vote_slot(500, 0), 540);
        assert_eq!(max_admitted_vote_slot(0, Slot::MAX), Slot::MAX);
    }

    #[test]
    fn certs_too_far_in_future_are_dropped() {
        let mut ctx = TestContext::new();
        let slot = ctx.verifier.sharable_banks.root().slot() + NUM_SLOTS_FOR_VERIFY + 1;
        let cert_type = CertificateType::Finalize(slot);
        let cert = test_create_base2_certificate(
            &ctx.bls_keypairs(),
            ctx.verifier.cluster_info.my_shred_version(),
            cert_type,
            &(0..ctx.validator_keypairs.len()).collect::<Vec<usize>>(),
        );
        let cert = ConsensusMessage::Certificate(cert);
        let datagrams = messages_to_datagrams(
            &[(cert, Pubkey::new_unique())],
            ctx.verifier.cluster_info.my_shred_version(),
        );
        ctx.verifier.verify_and_send_datagrams(datagrams).unwrap();

        expect_no_receive(&ctx.pool_receiver);
    }

    fn messages_to_datagrams(
        messages: &[(ConsensusMessage, Pubkey)],
        shred_version: u16,
    ) -> Vec<Datagram> {
        messages
            .iter()
            .map(|(message, peer_pubkey)| message_to_datagram(message, shred_version, *peer_pubkey))
            .collect()
    }
}
