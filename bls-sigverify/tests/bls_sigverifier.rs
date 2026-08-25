#![allow(clippy::arithmetic_side_effects)]
use {
    agave_bls_cert_verify::cert_verify::{
        test_create_base2_certificate, test_create_base2_unverified_certificate,
        test_create_base3_certificate,
    },
    agave_bls_sigverify::{
        bls_sigverifier::{
            MAX_VOTE_SLOT_DISTANCE_FROM_ROOT, NUM_SLOTS_FOR_VERIFY, SigVerifierChannels,
            SigVerifierContext, spawn_service,
        },
        generated_cert_types::GeneratedCertTypes,
        rewards::RewardInput,
        sig_verified_messages::{SigVerifiedBatch, VoteAggregate},
    },
    agave_votor_messages::{
        VerifiedVotorSlotsMessage,
        certificate::{Certificate, CertificateType},
        consensus_message::{Block, BlockId, ConsensusMessage, VoteMessage},
        metric_types::ConsensusMetricsEventReceiver,
        migration::MigrationStatus,
        unverified_vote_message::UnverifiedCertificate,
        vote::Vote,
        wire::{VersionedWireConsensusMessage, get_vote_payload_to_sign},
    },
    agave_votor_transport::endpoint::{BanCommand, Datagram, stub_ban_channel_for_tests},
    bitvec::prelude::{BitVec, Lsb0},
    bytes::Bytes,
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded},
    solana_bls_signatures::{
        BLS_SIGNATURE_AFFINE_SIZE, Keypair as BLSKeypair, Signature, signature::SignatureAffine,
    },
    solana_clock::Slot,
    solana_epoch_schedule::EpochSchedule,
    solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo},
    solana_keypair::Keypair,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_net_utils::SocketAddrSpace,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::{Bank, SlotLeader},
        bank_forks::{BankForks, SharableBanks},
        genesis_utils::{ValidatorVoteKeypairs, create_genesis_config_with_vote_accounts},
    },
    solana_signer::Signer,
    solana_signer_store::encode_base2,
    solana_streamer::evicting_sender::EvictingSender,
    std::{
        collections::HashSet,
        net::{Ipv4Addr, SocketAddr},
        num::NonZero,
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        thread::JoinHandle,
        time::{Duration, Instant},
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

struct TestContext {
    service: Option<JoinHandle<()>>,
    exit: Arc<AtomicBool>,
    sharable_banks: SharableBanks,
    cluster_info: Arc<ClusterInfo>,
    highest_parent_ready: Arc<RwLock<(Slot, Block)>>,
    migration_status: Arc<MigrationStatus>,
    validator_keypairs: Vec<ValidatorVoteKeypairs>,
    ban_receiver: mpsc::Receiver<BanCommand>,
    packet_sender: Sender<Datagram>,
    repair_receiver: Receiver<VerifiedVotorSlotsMessage>,
    _reward_receiver: Receiver<RewardInput>,
    pool_receiver: Receiver<SigVerifiedBatch>,
    metrics_receiver: ConsensusMetricsEventReceiver,
    generated_cert_types: Arc<GeneratedCertTypes>,
    certificate_sender: Sender<(Slot, UnverifiedCertificate)>,
    bank_forks: Arc<RwLock<BankForks>>,
}

impl TestContext {
    fn new() -> Self {
        Self::new_with_config(1024, MigrationStatus::post_migration_status())
    }

    fn new_with_config(pool_capacity: usize, migration_status: MigrationStatus) -> Self {
        let (channel_to_pool, pool_receiver) = bounded(pool_capacity);
        let num_validators = 10;
        let validator_keypairs = (0..num_validators)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let stakes_vec = (0..validator_keypairs.len())
            .map(|i| 1_000u64.saturating_sub(i as u64))
            .collect::<Vec<_>>();
        let mut genesis = create_genesis_config_with_vote_accounts(
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
        let leader_schedule = Arc::new(LeaderScheduleCache::new_from_bank(&sharable_banks.root()));

        let (channel_to_repair, repair_receiver) = EvictingSender::new_bounded(1024);
        let (channel_to_reward, reward_receiver) = bounded(1024);
        let (packet_sender, packet_receiver) = bounded(1024);
        let (certificate_sender, certificate_receiver) = bounded(1024);
        let (channel_to_metrics, metrics_receiver) = bounded(1024);

        let generated_cert_types = Arc::new(GeneratedCertTypes::default());
        let (ban_sender, ban_receiver) = stub_ban_channel_for_tests(1024);
        let highest_parent_ready = Arc::new(RwLock::new((
            NUM_SLOTS_FOR_VERIFY,
            Block::new_unique(NUM_SLOTS_FOR_VERIFY.saturating_sub(1)),
        )));
        let migration_status = Arc::new(migration_status);
        let context = SigVerifierContext {
            migration_status: migration_status.clone(),
            ban_sender,
            sharable_banks: sharable_banks.clone(),
            highest_parent_ready: highest_parent_ready.clone(),
            cluster_info: cluster_info.clone(),
            leader_schedule,
            num_threads: 4,
            generated_cert_types: generated_cert_types.clone(),
        };
        let channels = SigVerifierChannels::new(
            packet_receiver,
            certificate_receiver,
            channel_to_repair,
            channel_to_reward,
            channel_to_pool,
            channel_to_metrics,
        );
        let exit = Arc::new(AtomicBool::new(false));
        let service = spawn_service(exit.clone(), context, channels);
        Self {
            validator_keypairs,
            service: Some(service),
            exit,
            sharable_banks,
            cluster_info,
            highest_parent_ready,
            migration_status,
            ban_receiver,
            packet_sender,
            repair_receiver,
            _reward_receiver: reward_receiver,
            pool_receiver,
            metrics_receiver,
            generated_cert_types,
            certificate_sender,
            bank_forks,
        }
    }

    /// Drain pending ban requests and collect the banned pubkeys.
    fn banned_pubkeys(&mut self) -> HashSet<Pubkey> {
        let mut banned = HashSet::new();
        while let Ok(BanCommand { peer, .. }) = self.ban_receiver.try_recv() {
            banned.insert(peer);
        }
        banned
    }

    fn send_datagrams(&self, datagrams: Vec<Datagram>) {
        for datagram in datagrams {
            self.packet_sender.send(datagram).unwrap();
        }
    }

    fn send_certificate(&self, slot: Slot, certificate: UnverifiedCertificate) {
        self.certificate_sender.send((slot, certificate)).unwrap();
    }

    fn bls_keypairs(&self) -> Vec<BLSKeypair> {
        self.validator_keypairs
            .iter()
            .map(|k| k.bls_keypair.clone())
            .collect()
    }
}

impl Drop for TestContext {
    fn drop(&mut self) {
        self.exit.store(true, Ordering::Relaxed);
        if let Some(service) = self.service.take() {
            while !service.is_finished() {
                // A failed assertion may leave the service blocked on a full pool.
                let _ = self.pool_receiver.recv_timeout(NO_RECEIVE_TIMEOUT);
            }
            service.join().unwrap();
        }
    }
}

const RECEIVE_TIMEOUT: Duration = Duration::from_secs(5);
const NO_RECEIVE_TIMEOUT: Duration = Duration::from_millis(200);

fn receive_batches(receiver: &Receiver<SigVerifiedBatch>, count: usize) -> Vec<SigVerifiedBatch> {
    let batches = (0..count)
        .map(|_| {
            receiver
                .recv_timeout(RECEIVE_TIMEOUT)
                .expect("verified batch")
        })
        .collect();
    expect_no_receive(receiver);
    batches
}

// The running service may split inputs across any number of batches. Wait for
// individual votes and certificates rather than a particular batch layout.
fn receive_messages(
    receiver: &Receiver<SigVerifiedBatch>,
    expected_votes: usize,
    expected_certificates: usize,
) -> (Vec<VoteAggregate>, Vec<Certificate>) {
    let deadline = Instant::now() + RECEIVE_TIMEOUT;
    let mut votes = Vec::new();
    let mut certificates = Vec::new();
    let mut num_votes = 0;
    while num_votes < expected_votes || certificates.len() < expected_certificates {
        match receiver.recv_deadline(deadline).expect("verified messages") {
            SigVerifiedBatch::Votes(aggregates) => {
                num_votes += aggregates
                    .iter()
                    .map(VoteAggregate::num_votes)
                    .sum::<usize>();
                votes.extend(aggregates);
            }
            SigVerifiedBatch::Certificates(certs) => certificates.extend(certs),
        }
        assert!(num_votes <= expected_votes, "unexpected verified votes");
        assert!(
            certificates.len() <= expected_certificates,
            "unexpected certificates"
        );
    }
    expect_no_receive(receiver);
    (votes, certificates)
}

fn assert_verified_votes(aggregates: &[VoteAggregate], expected: &[(Vote, usize)]) {
    let mut actual = HashSet::new();
    for aggregate in aggregates {
        for rank in aggregate.ranks().iter_ones() {
            assert!(
                actual.insert((*aggregate.vote(), rank)),
                "duplicate verified vote"
            );
        }
    }
    assert_eq!(actual, expected.iter().copied().collect());
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
    match receiver.recv_timeout(NO_RECEIVE_TIMEOUT).unwrap_err() {
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
    let ctx = TestContext::new_with_config(1024, MigrationStatus::default());
    let shred_version = ctx.cluster_info.my_shred_version();
    let block = Block::new_unique(1);
    let certificate = test_create_base2_unverified_certificate(
        &ctx.bls_keypairs(),
        shred_version,
        CertificateType::FinalizeFast(block),
        &[0, 1, 2, 3, 4, 5, 6, 7],
    );
    let slot = 2;

    ctx.send_certificate(slot, certificate.clone());
    expect_no_receive(&ctx.pool_receiver);

    ctx.migration_status.enable_alpenglow_for_tests();
    ctx.send_certificate(slot, certificate);
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
    let ctx = TestContext::new();
    let shred_version = ctx.cluster_info.my_shred_version();
    let block = Block::new_unique(1);
    let certificate = test_create_base2_unverified_certificate(
        &ctx.bls_keypairs(),
        shred_version,
        CertificateType::FinalizeFast(block),
        &[0, 1, 2, 3, 4, 5, 6, 7],
    );
    let slot = 6;
    let root_bank = Bank::new_from_parent(ctx.sharable_banks.root(), SlotLeader::default(), 5);

    {
        let mut bank_forks = ctx.bank_forks.write().unwrap();
        bank_forks.insert(root_bank);
        bank_forks.set_root(5, None, None);
    }
    ctx.send_certificate(slot, certificate);
    expect_no_receive(&ctx.pool_receiver);
}

#[test]
fn test_blssigverifier_send_packets() {
    let ctx = TestContext::new();

    let vote_rank1 = 2;
    let cert_ranks = [0, 2, 3, 4, 5, 7, 8, 9];
    let cert_type = CertificateType::Finalize(4);
    let vote_message1 = create_signed_vote_message(
        &ctx.sharable_banks.root(),
        &ctx.validator_keypairs,
        ctx.cluster_info.my_shred_version(),
        Vote::new_finalization_vote(5),
        vote_rank1,
    );
    let cert = test_create_base2_certificate(
        &ctx.bls_keypairs(),
        ctx.cluster_info.my_shred_version(),
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

    ctx.send_datagrams(messages_to_datagrams(
        &messages1,
        ctx.cluster_info.my_shred_version(),
    ));
    assert_eq!(receive_batches(&ctx.pool_receiver, 2).len(), 2);
    {
        let (slot, pubkeys) = ctx.repair_receiver.recv_timeout(RECEIVE_TIMEOUT).unwrap();
        assert_eq!(slot, 5);
        assert_eq!(
            pubkeys.as_slice(),
            &[ctx.validator_keypairs[vote_rank1].vote_keypair.pubkey()]
        );
    }

    let vote_rank2 = 3;
    let vote_message2 = create_signed_vote_message(
        &ctx.sharable_banks.root(),
        &ctx.validator_keypairs,
        ctx.cluster_info.my_shred_version(),
        Vote::new_unique_notar(6),
        vote_rank2,
    );
    let messages2 = [(
        ConsensusMessage::Vote(vote_message2),
        ctx.validator_keypairs[vote_rank2].node_keypair.pubkey(),
    )];
    ctx.send_datagrams(messages_to_datagrams(
        &messages2,
        ctx.cluster_info.my_shred_version(),
    ));

    assert_eq!(receive_batches(&ctx.pool_receiver, 1).len(), 1);
    {
        let (slot, pubkeys) = ctx.repair_receiver.recv_timeout(RECEIVE_TIMEOUT).unwrap();
        assert_eq!(slot, 6);
        assert_eq!(
            pubkeys.as_slice(),
            &[ctx.validator_keypairs[vote_rank2].vote_keypair.pubkey()]
        );
    }

    let vote_rank3 = 9;
    let vote_message3 = create_signed_vote_message(
        &ctx.sharable_banks.root(),
        &ctx.validator_keypairs,
        ctx.cluster_info.my_shred_version(),
        Vote::new_unique_notar_fallback(7),
        vote_rank3,
    );
    let messages3 = [(
        ConsensusMessage::Vote(vote_message3),
        ctx.validator_keypairs[vote_rank3].node_keypair.pubkey(),
    )];
    ctx.send_datagrams(messages_to_datagrams(
        &messages3,
        ctx.cluster_info.my_shred_version(),
    ));
    assert_eq!(receive_batches(&ctx.pool_receiver, 1).len(), 1);
    {
        let (slot, pubkeys) = ctx.repair_receiver.recv_timeout(RECEIVE_TIMEOUT).unwrap();
        assert_eq!(slot, 7);
        assert_eq!(
            pubkeys.as_slice(),
            &[ctx.validator_keypairs[vote_rank3].vote_keypair.pubkey()]
        );
    }
}

#[test]
fn test_blssigverifier_verify_malformed() {
    let ctx = TestContext::new();

    let datagrams = vec![datagram_from_bytes(Bytes::new(), Pubkey::new_unique())];
    ctx.send_datagrams(datagrams);

    // Expect no messages since the packet was malformed
    expect_no_receive(&ctx.pool_receiver);

    // Send a packet too far in the future
    let rank = 0;
    let vote_message_no_stakes = create_signed_vote_message_with_stake(
        &ctx.validator_keypairs,
        ctx.cluster_info.my_shred_version(),
        Vote::new_finalization_vote(5_000_000_000), // very high slot
        rank,
        NonZero::new(123).unwrap(),
    );
    let messages_no_stakes = [(
        ConsensusMessage::Vote(vote_message_no_stakes),
        ctx.validator_keypairs[rank].node_keypair.pubkey(),
    )];

    ctx.send_datagrams(messages_to_datagrams(
        &messages_no_stakes,
        ctx.cluster_info.my_shred_version(),
    ));

    // Expect no messages since the packet was malformed
    expect_no_receive(&ctx.pool_receiver);

    // Send a packet with invalid rank
    let vote = Vote::new_finalization_vote(5);
    let payload = get_vote_payload_to_sign(vote, ctx.cluster_info.my_shred_version());
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
    ctx.send_datagrams(messages_to_datagrams(
        &messages_invalid_rank,
        ctx.cluster_info.my_shred_version(),
    ));

    // Expect no messages since the packet was malformed
    expect_no_receive(&ctx.pool_receiver);
}

#[test]
fn test_shred_version_mismatch() {
    let ctx = TestContext::new();
    let rank = 0;
    let msgs = [(
        ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.cluster_info.my_shred_version() + 1,
            Vote::new_finalization_vote(5),
            rank,
        )),
        ctx.validator_keypairs[rank].node_keypair.pubkey(),
    )];
    // creating a datagram with the wrong shred version
    let datagrams = messages_to_datagrams(&msgs, ctx.cluster_info.my_shred_version() + 1);
    ctx.send_datagrams(datagrams);
    expect_no_receive(&ctx.pool_receiver);
    expect_no_receive(&ctx.repair_receiver);
}

#[test]
fn test_blssigverifier_send_packets_channel_full() {
    agave_logger::setup();
    let ctx = TestContext::new_with_config(1, MigrationStatus::post_migration_status());

    let msg1_rank = 0;
    let msg2_rank = 2;
    let msg1 = create_signed_vote_message(
        &ctx.sharable_banks.root(),
        &ctx.validator_keypairs,
        ctx.cluster_info.my_shred_version(),
        Vote::new_finalization_vote(5),
        msg1_rank,
    );
    let msg2 = create_signed_vote_message(
        &ctx.sharable_banks.root(),
        &ctx.validator_keypairs,
        ctx.cluster_info.my_shred_version(),
        Vote::new_unique_notar_fallback(6),
        msg2_rank,
    );
    ctx.send_datagrams(messages_to_datagrams(
        &[(
            ConsensusMessage::Vote(msg1.clone()),
            ctx.validator_keypairs[msg1_rank].node_keypair.pubkey(),
        )],
        ctx.cluster_info.my_shred_version(),
    ));

    // Repair is notified after the pool send, so this confirms the cap-1
    // channel is full before the second vote is submitted.
    let (slot, _) = ctx.repair_receiver.recv_timeout(RECEIVE_TIMEOUT).unwrap();
    assert_eq!(slot, 5);
    assert_eq!(ctx.pool_receiver.len(), 1);

    ctx.send_datagrams(messages_to_datagrams(
        &[(
            ConsensusMessage::Vote(msg2.clone()),
            ctx.validator_keypairs[msg2_rank].node_keypair.pubkey(),
        )],
        ctx.cluster_info.my_shred_version(),
    ));
    // The second vote cannot reach repair until its blocked pool send completes.
    expect_no_receive(&ctx.repair_receiver);
    let m1_recv = ctx.pool_receiver.recv_timeout(RECEIVE_TIMEOUT).unwrap();
    let m2_recv = ctx.pool_receiver.recv_timeout(RECEIVE_TIMEOUT).unwrap();
    let (slot, _) = ctx.repair_receiver.recv_timeout(RECEIVE_TIMEOUT).unwrap();
    assert_eq!(slot, 6);
    expect_no_receive(&ctx.pool_receiver);

    // Both messages were eventually delivered (no silent drop).
    let bank = ctx.sharable_banks.root();
    let batch1 = SigVerifiedBatch::Votes(vec![new_vote_aggregate(&bank, msg1)]);
    let batch2 = SigVerifiedBatch::Votes(vec![new_vote_aggregate(&bank, msg2)]);
    assert_eq!(m1_recv, batch1);
    assert_eq!(m2_recv, batch2);
}

#[test]
fn test_blssigverifier_send_packets_receiver_closed() {
    let mut ctx = TestContext::new();

    // Close the pool receiver to simulate a disconnected channel.
    let (_, replacement) = bounded(1);
    drop(std::mem::replace(&mut ctx.pool_receiver, replacement));

    let rank = 0;
    let msg = ConsensusMessage::Vote(create_signed_vote_message(
        &ctx.sharable_banks.root(),
        &ctx.validator_keypairs,
        ctx.cluster_info.my_shred_version(),
        Vote::new_finalization_vote(5),
        rank,
    ));
    let messages = [(msg, ctx.validator_keypairs[rank].node_keypair.pubkey())];
    ctx.send_datagrams(messages_to_datagrams(
        &messages,
        ctx.cluster_info.my_shred_version(),
    ));
    let deadline = Instant::now() + RECEIVE_TIMEOUT;
    while !ctx.service.as_ref().unwrap().is_finished() {
        assert!(
            Instant::now() < deadline,
            "service should exit when the pool disconnects"
        );
        std::thread::yield_now();
    }
    ctx.service.take().unwrap().join().unwrap();
}

#[test]
fn test_blssigverifier_verify_votes_all_valid() {
    let ctx = TestContext::new();

    let num_votes = 5;
    let mut packets = Vec::with_capacity(num_votes);
    let vote = Vote::new_skip_vote(42);
    let vote_payload = get_vote_payload_to_sign(vote, ctx.cluster_info.my_shred_version());

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
            ctx.cluster_info.my_shred_version(),
            validator_keypair.node_keypair.pubkey(),
        ));
    }

    ctx.send_datagrams(packets);
    let (aggregates, _) = receive_messages(&ctx.pool_receiver, num_votes, 0);
    assert_verified_votes(
        &aggregates,
        &(0..num_votes).map(|rank| (vote, rank)).collect::<Vec<_>>(),
    );
}

#[test]
fn test_blssigverifier_verify_votes_across_batches() {
    let ctx = TestContext::new();
    let vote = Vote::new_skip_vote(42);
    let num_votes = 3;
    for rank in 0..num_votes {
        let message = ConsensusMessage::Vote(create_signed_vote_message(
            &ctx.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.cluster_info.my_shred_version(),
            vote,
            rank,
        ));
        ctx.send_datagrams(vec![message_to_datagram(
            &message,
            ctx.cluster_info.my_shred_version(),
            ctx.validator_keypairs[rank].node_keypair.pubkey(),
        )]);
        // Metrics are sent after the pool output. Wait before submitting the
        // next vote to ensure it is processed in a separate batch.
        ctx.metrics_receiver.recv_timeout(RECEIVE_TIMEOUT).unwrap();
    }
    let (aggregates, _) = receive_messages(&ctx.pool_receiver, num_votes, 0);
    assert_verified_votes(
        &aggregates,
        &(0..num_votes).map(|rank| (vote, rank)).collect::<Vec<_>>(),
    );
}

#[test]
fn test_blssigverifier_verify_votes_two_distinct_messages() {
    let ctx = TestContext::new();

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
            &ctx.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.cluster_info.my_shred_version(),
            vote1,
            i,
        ));
        packets.push(message_to_datagram(
            &msg,
            ctx.cluster_info.my_shred_version(),
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
            &ctx.sharable_banks.root(),
            &ctx.validator_keypairs,
            ctx.cluster_info.my_shred_version(),
            vote2,
            i,
        ));
        packets.push(message_to_datagram(
            &msg,
            ctx.cluster_info.my_shred_version(),
            validator_keypair.node_keypair.pubkey(),
        ));
    }

    ctx.send_datagrams(packets);
    let (aggregates, _) = receive_messages(&ctx.pool_receiver, num_votes, 0);
    let expected = (0..num_votes)
        .map(|rank| {
            (
                if rank < num_votes_group1 {
                    vote1
                } else {
                    vote2
                },
                rank,
            )
        })
        .collect::<Vec<_>>();
    assert_verified_votes(&aggregates, &expected);
}

#[test]
fn test_blssigverifier_verify_votes_invalid_in_two_distinct_messages() {
    let ctx = TestContext::new();

    let num_votes = 5;
    let invalid_rank = 3; // This voter will sign vote 2 with an invalid signature.
    let mut packets = Vec::with_capacity(num_votes);

    let vote1 = Vote::new_skip_vote(42);
    let vote1_payload = get_vote_payload_to_sign(vote1, ctx.cluster_info.my_shred_version());
    let vote2 = Vote::new_skip_vote(43);
    let vote2_payload = get_vote_payload_to_sign(vote2, ctx.cluster_info.my_shred_version());
    let invalid_payload =
        get_vote_payload_to_sign(Vote::new_skip_vote(99), ctx.cluster_info.my_shred_version());

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
            ctx.cluster_info.my_shred_version(),
            validator_keypair.node_keypair.pubkey(),
        ));
    }

    ctx.send_datagrams(packets);
    let (aggregates, _) = receive_messages(&ctx.pool_receiver, num_votes - 1, 0);
    let expected = (0..num_votes)
        .filter(|&rank| rank != invalid_rank as usize)
        .map(|rank| (if rank < 2 { vote1 } else { vote2 }, rank))
        .collect::<Vec<_>>();
    assert_verified_votes(&aggregates, &expected);
}

#[test]
fn test_blssigverifier_verify_votes_one_invalid_signature() {
    let ctx = TestContext::new();

    let num_votes = 5;
    let invalid_rank = 2;
    let mut packets = Vec::with_capacity(num_votes);

    let vote = Vote::new_skip_vote(42);
    let valid_vote_payload = get_vote_payload_to_sign(vote, ctx.cluster_info.my_shred_version());
    let invalid_vote_payload =
        get_vote_payload_to_sign(Vote::new_skip_vote(99), ctx.cluster_info.my_shred_version());

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

        packets.push(message_to_datagram(
            &consensus_message,
            ctx.cluster_info.my_shred_version(),
            validator_keypair.node_keypair.pubkey(),
        ));
    }

    ctx.send_datagrams(packets);
    let (aggregates, _) = receive_messages(&ctx.pool_receiver, num_votes - 1, 0);
    let expected = (0..num_votes)
        .filter(|&rank| rank != invalid_rank as usize)
        .map(|rank| (vote, rank))
        .collect::<Vec<_>>();
    assert_verified_votes(&aggregates, &expected);
}

#[test]
fn test_verify_certificate_base2_valid() {
    let ctx = TestContext::new();

    // 2/3 of validators sign the cert.
    let num_signers = (ctx.validator_keypairs.len() * 2).div_ceil(3);
    let cert_type = CertificateType::new_unique_notar(10);
    let cert = test_create_base2_certificate(
        &ctx.bls_keypairs(),
        ctx.cluster_info.my_shred_version(),
        cert_type,
        &(0..num_signers).collect::<Vec<_>>(),
    );
    let consensus_message = ConsensusMessage::Certificate(cert);
    let datagrams = messages_to_datagrams(
        &[(consensus_message, Pubkey::new_unique())],
        ctx.cluster_info.my_shred_version(),
    );

    ctx.send_datagrams(datagrams);
    assert_eq!(
        receive_batches(&ctx.pool_receiver, 1).len(),
        1,
        "Valid Base2 certificate should be sent"
    );
}

#[test]
fn test_verify_certificate_base2_just_enough_stake() {
    let ctx = TestContext::new();

    // 60% of validators sign the cert.
    let num_signers = (ctx.validator_keypairs.len() * 6).div_ceil(10);
    let cert_type = CertificateType::new_unique_notar(10);
    let cert = test_create_base2_certificate(
        &ctx.bls_keypairs(),
        ctx.cluster_info.my_shred_version(),
        cert_type,
        &(0..num_signers).collect::<Vec<_>>(),
    );
    let consensus_message = ConsensusMessage::Certificate(cert);
    let datagrams = messages_to_datagrams(
        &[(consensus_message, Pubkey::new_unique())],
        ctx.cluster_info.my_shred_version(),
    );

    ctx.send_datagrams(datagrams);
    assert_eq!(
        receive_batches(&ctx.pool_receiver, 1).len(),
        1,
        "Valid Base2 certificate should be sent"
    );
}

#[test]
fn test_verify_certificate_base3_valid() {
    let ctx = TestContext::new();

    let slot = 20;
    let cert_type = CertificateType::new_unique_notar_fallback(slot);
    let cert = test_create_base3_certificate(
        &ctx.bls_keypairs(),
        ctx.cluster_info.my_shred_version(),
        cert_type,
        &[0, 1, 2, 3],
        &[4, 5, 6],
    );
    let consensus_message = ConsensusMessage::Certificate(cert);
    let datagrams = messages_to_datagrams(
        &[(consensus_message, Pubkey::new_unique())],
        ctx.cluster_info.my_shred_version(),
    );

    ctx.send_datagrams(datagrams);
    assert_eq!(
        receive_batches(&ctx.pool_receiver, 1).len(),
        1,
        "Valid Base3 certificate should be sent"
    );
}

#[test]
fn test_verify_certificate_base3_just_enough_stake() {
    let ctx = TestContext::new();
    let slot = 20;
    let cert_type = CertificateType::new_unique_notar_fallback(slot);
    let cert = test_create_base3_certificate(
        &ctx.bls_keypairs(),
        ctx.cluster_info.my_shred_version(),
        cert_type,
        &[0, 1, 2, 3],
        &[4, 5],
    );
    let consensus_message = ConsensusMessage::Certificate(cert);
    let datagrams = messages_to_datagrams(
        &[(consensus_message, Pubkey::new_unique())],
        ctx.cluster_info.my_shred_version(),
    );

    ctx.send_datagrams(datagrams);
    assert_eq!(
        receive_batches(&ctx.pool_receiver, 1).len(),
        1,
        "Valid Base3 certificate should be sent"
    );
}

#[test]
fn test_verify_certificate_invalid_signature() {
    let ctx = TestContext::new();

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
        ctx.cluster_info.my_shred_version(),
    );

    ctx.send_datagrams(datagrams);
    expect_no_receive(&ctx.pool_receiver);
}

#[test]
fn test_verify_mixed_valid_batch() {
    let ctx = TestContext::new();

    let mut packets = Vec::new();
    let num_votes = 2;

    let vote = Vote::new_skip_vote(42);
    let vote_payload = get_vote_payload_to_sign(vote, ctx.cluster_info.my_shred_version());
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
            ctx.cluster_info.my_shred_version(),
            validator_keypair.node_keypair.pubkey(),
        ));
    }

    // 70% of validators sign.
    let num_signers = (ctx.validator_keypairs.len() * 7).div_ceil(10);
    let cert_type = CertificateType::new_unique_notar(10);
    let cert = test_create_base2_certificate(
        &ctx.bls_keypairs(),
        ctx.cluster_info.my_shred_version(),
        cert_type,
        &(0..num_signers).into_iter().collect::<Vec<_>>(),
    );
    let consensus_message_cert = ConsensusMessage::Certificate(cert);
    packets.push(message_to_datagram(
        &consensus_message_cert,
        ctx.cluster_info.my_shred_version(),
        Pubkey::new_unique(),
    ));

    ctx.send_datagrams(packets);
    let (aggregates, certificates) = receive_messages(&ctx.pool_receiver, num_votes, 1);
    assert_verified_votes(
        &aggregates,
        &(0..num_votes).map(|rank| (vote, rank)).collect::<Vec<_>>(),
    );
    assert_eq!(certificates[0].cert_type, cert_type);
}

#[test]
fn test_verify_vote_with_invalid_rank() {
    let ctx = TestContext::new();

    let invalid_rank = 999;
    let vote = Vote::new_skip_vote(42);
    let vote_payload = get_vote_payload_to_sign(vote, ctx.cluster_info.my_shred_version());
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
        ctx.cluster_info.my_shred_version(),
    );
    ctx.send_datagrams(datagrams);
    expect_no_receive(&ctx.pool_receiver);
}

#[test]
fn test_verify_old_vote_and_cert() {
    let ctx = TestContext::new();
    let bank5 = Bank::new_from_parent(ctx.sharable_banks.root(), SlotLeader::default(), 5);
    {
        let mut bank_forks = ctx.bank_forks.write().unwrap();
        bank_forks.insert(bank5);
        bank_forks.set_root(5, None, None);
    }

    let rank = 0;
    let vote = Vote::new_skip_vote(2);
    let vote_payload = get_vote_payload_to_sign(vote, ctx.cluster_info.my_shred_version());
    let bls_keypair = &ctx.validator_keypairs[rank].bls_keypair;
    let signature = SignatureAffine::from(bls_keypair.sign(&vote_payload));
    let consensus_message_vote = ConsensusMessage::Vote(VoteMessage {
        vote,
        signature,
        rank: rank.try_into().unwrap(),
        stake: NonZero::new(123).unwrap(),
    });
    let datagrams_vote = messages_to_datagrams(
        &[(
            consensus_message_vote,
            ctx.validator_keypairs[rank].node_keypair.pubkey(),
        )],
        ctx.cluster_info.my_shred_version(),
    );

    ctx.send_datagrams(datagrams_vote);
    expect_no_receive(&ctx.pool_receiver);

    let cert = test_create_base2_certificate(
        &ctx.bls_keypairs(),
        ctx.cluster_info.my_shred_version(),
        CertificateType::Finalize(3),
        &[0], // Signer rank 0
    );
    let consensus_message_cert = ConsensusMessage::Certificate(cert);
    let datagrams_cert = messages_to_datagrams(
        &[(consensus_message_cert, Pubkey::new_unique())],
        ctx.cluster_info.my_shred_version(),
    );

    ctx.send_datagrams(datagrams_cert);
    expect_no_receive(&ctx.pool_receiver);
}

#[test]
fn test_verified_certs_are_skipped() {
    let ctx = TestContext::new();

    // 80% of validators sign.
    let num_signers = (ctx.validator_keypairs.len() * 8).div_ceil(10);
    let slot = 10;
    let cert_type = CertificateType::new_unique_notar(slot);
    let cert1 = test_create_base2_certificate(
        &ctx.bls_keypairs(),
        ctx.cluster_info.my_shred_version(),
        cert_type,
        &(0..num_signers).into_iter().collect::<Vec<_>>(),
    );
    let consensus_message1 = ConsensusMessage::Certificate(cert1);
    let datagrams1 = messages_to_datagrams(
        &[(consensus_message1, Pubkey::new_unique())],
        ctx.cluster_info.my_shred_version(),
    );

    ctx.send_datagrams(datagrams1);

    assert_eq!(receive_batches(&ctx.pool_receiver, 1).len(), 1);

    let cert2 = test_create_base2_certificate(
        &ctx.bls_keypairs(),
        ctx.cluster_info.my_shred_version(),
        cert_type,
        &(0..num_signers - 1).into_iter().collect::<Vec<_>>(),
    );
    let consensus_message2 = ConsensusMessage::Certificate(cert2);
    let datagrams2 = messages_to_datagrams(
        &[(consensus_message2, Pubkey::new_unique())],
        ctx.cluster_info.my_shred_version(),
    );

    ctx.send_datagrams(datagrams2);
    expect_no_receive(&ctx.pool_receiver);
}

#[test]
fn test_same_type_certs_verify_until_first_valid() {
    let mut ctx = TestContext::new();

    let cert_type = CertificateType::new_unique_notar(10);
    let cert1 = test_create_base2_certificate(
        &ctx.bls_keypairs(),
        ctx.cluster_info.my_shred_version(),
        cert_type,
        &(0..7).collect::<Vec<_>>(),
    );
    let mut cert2 = test_create_base2_certificate(
        &ctx.bls_keypairs(),
        ctx.cluster_info.my_shred_version(),
        cert_type,
        &(1..8).collect::<Vec<_>>(),
    );
    cert2.signature = Signature([0; BLS_SIGNATURE_AFFINE_SIZE]);
    let redundant_sender = Pubkey::new_unique();
    let datagrams = messages_to_datagrams(
        &[
            (ConsensusMessage::Certificate(cert1), Pubkey::new_unique()),
            (ConsensusMessage::Certificate(cert2), redundant_sender),
        ],
        ctx.cluster_info.my_shred_version(),
    );

    ctx.send_datagrams(datagrams);

    let batches = receive_batches(&ctx.pool_receiver, 1);
    assert_eq!(batches.len(), 1);
    match &batches[0] {
        SigVerifiedBatch::Certificates(certs) => assert_eq!(certs.len(), 1),
        rest => panic!("unexpected type: {rest:?}"),
    }
    assert!(!ctx.banned_pubkeys().contains(&redundant_sender));
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
        ctx.cluster_info.my_shred_version(),
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
        ctx.cluster_info.my_shred_version(),
    );

    ctx.send_datagrams(datagrams);

    let batches = receive_batches(&ctx.pool_receiver, 1);
    assert_eq!(batches.len(), 1);
    match &batches[0] {
        SigVerifiedBatch::Certificates(certs) => assert_eq!(certs.len(), 1),
        rest => panic!("unexpected type: {rest:?}"),
    }
    let banlist = ctx.banned_pubkeys();
    assert!(banlist.contains(&invalid_sender), "Invalid cert -> ban");
    assert!(!banlist.contains(&valid_sender), "Valid certs ok");
    assert!(!banlist.contains(&redundant_sender), "Redundant certs ok");
}

#[test]
fn test_banlist_not_updated_for_valid_vote_and_cert() {
    let mut ctx = TestContext::new();

    let rank = 0;
    let vote_message = ConsensusMessage::Vote(create_signed_vote_message(
        &ctx.sharable_banks.root(),
        &ctx.validator_keypairs,
        ctx.cluster_info.my_shred_version(),
        Vote::new_skip_vote(42),
        rank,
    ));
    let cert_message = ConsensusMessage::Certificate(test_create_base2_certificate(
        &ctx.bls_keypairs(),
        ctx.cluster_info.my_shred_version(),
        CertificateType::new_unique_notar(43),
        &(0..7).collect::<Vec<_>>(),
    ));
    let vote_sender = ctx.validator_keypairs[rank].node_keypair.pubkey();
    let cert_sender = Pubkey::new_unique();
    let datagrams = messages_to_datagrams(
        &[(vote_message, vote_sender), (cert_message, cert_sender)],
        ctx.cluster_info.my_shred_version(),
    );

    ctx.send_datagrams(datagrams);
    assert_eq!(receive_batches(&ctx.pool_receiver, 2).len(), 2);
    let banned = ctx.banned_pubkeys();
    assert!(!banned.contains(&vote_sender));
    assert!(!banned.contains(&cert_sender));
}

#[test]
fn test_banlist_updates_for_invalid_votes() {
    let mut ctx = TestContext::new();

    let vote = Vote::new_skip_vote(42);
    let valid_payload = get_vote_payload_to_sign(vote, ctx.cluster_info.my_shred_version());
    let invalid_payload = get_vote_payload_to_sign(
        Vote::new_skip_vote(999),
        ctx.cluster_info.my_shred_version(),
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

    ctx.send_datagrams(messages_to_datagrams(
        &messages,
        ctx.cluster_info.my_shred_version(),
    ));
    let (aggregates, _) = receive_messages(
        &ctx.pool_receiver,
        messages.len() - invalid_indexes.len(),
        0,
    );
    let expected = (0..messages.len())
        .filter(|rank| !invalid_indexes.contains(rank))
        .map(|rank| (vote, rank))
        .collect::<Vec<_>>();
    assert_verified_votes(&aggregates, &expected);

    let banned = ctx.banned_pubkeys();
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
                ctx.cluster_info.my_shred_version(),
                cert_type,
                &(0..7).collect::<Vec<_>>(),
            );
            if invalid_indexes.contains(&i) {
                cert.signature = Signature([0; BLS_SIGNATURE_AFFINE_SIZE]);
            }
            (ConsensusMessage::Certificate(cert), Pubkey::new_unique())
        })
        .collect();

    ctx.send_datagrams(messages_to_datagrams(
        &messages,
        ctx.cluster_info.my_shred_version(),
    ));
    let (_, certificates) = receive_messages(
        &ctx.pool_receiver,
        0,
        messages.len() - invalid_indexes.len(),
    );
    let expected = messages
        .iter()
        .enumerate()
        .filter(|(index, _)| !invalid_indexes.contains(index))
        .map(|(_, (message, _))| {
            let ConsensusMessage::Certificate(cert) = message else {
                panic!("expected certificate");
            };
            cert.cert_type
        })
        .collect::<HashSet<_>>();
    assert_eq!(
        certificates
            .iter()
            .map(|cert| cert.cert_type)
            .collect::<HashSet<_>>(),
        expected
    );

    let banned = ctx.banned_pubkeys();
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
    let slot = 1235;
    let cert_type = CertificateType::Finalize(slot);
    ctx.generated_cert_types.insert_cert(cert_type);
    let cert = test_create_base2_certificate(
        &ctx.bls_keypairs(),
        ctx.cluster_info.my_shred_version(),
        cert_type,
        &(0..ctx.validator_keypairs.len()).collect::<Vec<usize>>(),
    );
    let consensus_message = ConsensusMessage::Certificate(cert);
    let datagrams = messages_to_datagrams(
        &[(consensus_message, Pubkey::new_unique())],
        ctx.cluster_info.my_shred_version(),
    );
    ctx.send_datagrams(datagrams);
    expect_no_receive(&ctx.pool_receiver);
    assert!(ctx.banned_pubkeys().is_empty());
}

#[test]
fn votes_are_bounded_by_highest_parent_ready() {
    let ctx = TestContext::new();
    let highest_parent_ready_slot = 100;
    *ctx.highest_parent_ready.write().unwrap() = (
        highest_parent_ready_slot,
        // The ParentReady target slot, rather than the parent block's slot, sets the bound.
        Block::new_unique(7),
    );
    let max_vote_slot = highest_parent_ready_slot + 40;
    let first_rejected_vote_slot = max_vote_slot + 1;

    let accepted_vote_rank = 0;
    let accepted_vote = ConsensusMessage::Vote(create_signed_vote_message(
        &ctx.sharable_banks.root(),
        &ctx.validator_keypairs,
        ctx.cluster_info.my_shred_version(),
        Vote::new_finalization_vote(max_vote_slot),
        accepted_vote_rank,
    ));
    let rejected_vote_rank = 1;
    let rejected_vote = ConsensusMessage::Vote(create_signed_vote_message(
        &ctx.sharable_banks.root(),
        &ctx.validator_keypairs,
        ctx.cluster_info.my_shred_version(),
        Vote::new_skip_vote(first_rejected_vote_slot),
        rejected_vote_rank,
    ));

    // Certificates retain the root-relative bound and are not limited by ParentReady.
    let cert_type = CertificateType::Finalize(first_rejected_vote_slot);
    let cert = test_create_base2_certificate(
        &ctx.bls_keypairs(),
        ctx.cluster_info.my_shred_version(),
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
        ctx.cluster_info.my_shred_version(),
    );
    ctx.send_datagrams(datagrams);

    assert_eq!(receive_batches(&ctx.pool_receiver, 2).len(), 2);
    {
        let (slot, pubkeys) = ctx.repair_receiver.recv_timeout(RECEIVE_TIMEOUT).unwrap();
        assert_eq!(slot, max_vote_slot);
        assert_eq!(
            pubkeys.as_slice(),
            &[ctx.validator_keypairs[accepted_vote_rank]
                .vote_keypair
                .pubkey(),]
        );
    }
    expect_no_receive(&ctx.repair_receiver);
}

#[test]
fn votes_are_bounded_by_root() {
    let ctx = TestContext::new();
    let root_bank = ctx.sharable_banks.root();
    let root_slot = root_bank.slot();
    *ctx.highest_parent_ready.write().unwrap() = (Slot::MAX, Block::new_unique(Slot::MAX));
    let max_vote_slot = root_slot + MAX_VOTE_SLOT_DISTANCE_FROM_ROOT;

    let messages = [max_vote_slot, max_vote_slot + 1]
        .into_iter()
        .enumerate()
        .map(|(rank, slot)| {
            let vote = ConsensusMessage::Vote(create_signed_vote_message(
                &root_bank,
                &ctx.validator_keypairs,
                ctx.cluster_info.my_shred_version(),
                Vote::new_skip_vote(slot),
                rank,
            ));
            (vote, ctx.validator_keypairs[rank].node_keypair.pubkey())
        })
        .collect::<Vec<_>>();
    let datagrams = messages_to_datagrams(&messages, ctx.cluster_info.my_shred_version());

    ctx.send_datagrams(datagrams);

    let SigVerifiedBatch::Votes(votes) = ctx.pool_receiver.recv_timeout(RECEIVE_TIMEOUT).unwrap()
    else {
        panic!("expected verified votes");
    };
    assert_eq!(votes.len(), 1);
    assert_eq!(votes[0].vote().slot(), max_vote_slot);
}

#[test]
fn genesis_votes_bypass_future_bound_during_migration() {
    let ctx = TestContext::new_with_config(1024, MigrationStatus::default());
    let highest_parent_ready_slot = 100;
    *ctx.highest_parent_ready.write().unwrap() = (
        highest_parent_ready_slot,
        Block {
            slot: highest_parent_ready_slot,
            block_id: BlockId::new_unique(),
        },
    );
    let max_vote_slot = highest_parent_ready_slot + 40;
    let migration_slot = ctx.migration_status.record_feature_activation(200);
    let genesis_slot = migration_slot.saturating_sub(1);
    assert!(genesis_slot > max_vote_slot);

    let genesis_block = Block {
        slot: genesis_slot,
        block_id: BlockId::new_unique(),
    };
    ctx.migration_status.set_genesis_block(genesis_block);
    let genesis_vote_rank = 0;
    let genesis_vote = ConsensusMessage::Vote(create_signed_vote_message(
        &ctx.sharable_banks.root(),
        &ctx.validator_keypairs,
        ctx.cluster_info.my_shred_version(),
        Vote::new_genesis_vote(genesis_block),
        genesis_vote_rank,
    ));

    // Normal votes remain bounded by ParentReady even when they target the exact block.
    let normal_vote_rank = 1;
    let normal_vote = ConsensusMessage::Vote(create_signed_vote_message(
        &ctx.sharable_banks.root(),
        &ctx.validator_keypairs,
        ctx.cluster_info.my_shred_version(),
        Vote::new_notarization_vote(genesis_block),
        normal_vote_rank,
    ));

    // The migration slot itself cannot be the Genesis slot.
    let different_slot_genesis_vote_rank = 2;
    let different_slot_genesis_vote = ConsensusMessage::Vote(create_signed_vote_message(
        &ctx.sharable_banks.root(),
        &ctx.validator_keypairs,
        ctx.cluster_info.my_shred_version(),
        Vote::new_genesis_vote(Block {
            slot: migration_slot,
            block_id: genesis_block.block_id,
        }),
        different_slot_genesis_vote_rank,
    ));

    // The Genesis exception does not require the locally discovered block's hash either.
    let different_hash_genesis_vote_rank = 3;
    let different_hash_genesis_block = Block {
        slot: genesis_slot,
        block_id: BlockId::new_unique(),
    };
    let different_hash_genesis_vote = ConsensusMessage::Vote(create_signed_vote_message(
        &ctx.sharable_banks.root(),
        &ctx.validator_keypairs,
        ctx.cluster_info.my_shred_version(),
        Vote::new_genesis_vote(different_hash_genesis_block),
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
        ctx.cluster_info.my_shred_version(),
    );
    ctx.send_datagrams(datagrams);

    let (aggregates, _) = receive_messages(&ctx.pool_receiver, 2, 0);
    assert_verified_votes(
        &aggregates,
        &[
            (Vote::new_genesis_vote(genesis_block), genesis_vote_rank),
            (
                Vote::new_genesis_vote(different_hash_genesis_block),
                different_hash_genesis_vote_rank,
            ),
        ],
    );
    expect_no_receive(&ctx.repair_receiver);
}

#[test]
fn votes_are_bounded_by_root_before_parent_ready() {
    let ctx = TestContext::new();
    let bank = Bank::new_from_parent(ctx.sharable_banks.root(), SlotLeader::default(), 500);
    {
        let mut bank_forks = ctx.bank_forks.write().unwrap();
        bank_forks.insert(bank);
        bank_forks.set_root(500, None, None);
    }
    *ctx.highest_parent_ready.write().unwrap() = (0, Block::new_unique(0));
    let messages = [540, 541]
        .into_iter()
        .enumerate()
        .map(|(rank, slot)| {
            let vote = create_signed_vote_message(
                &ctx.sharable_banks.root(),
                &ctx.validator_keypairs,
                ctx.cluster_info.my_shred_version(),
                Vote::new_finalization_vote(slot),
                rank,
            );
            (
                ConsensusMessage::Vote(vote),
                ctx.validator_keypairs[rank].node_keypair.pubkey(),
            )
        })
        .collect::<Vec<_>>();
    ctx.send_datagrams(messages_to_datagrams(
        &messages,
        ctx.cluster_info.my_shred_version(),
    ));
    let batches = receive_batches(&ctx.pool_receiver, 1);
    let SigVerifiedBatch::Votes(votes) = &batches[0] else {
        panic!("expected votes");
    };
    assert_eq!(votes.len(), 1);
    assert_eq!(votes[0].vote().slot(), 540);
}

#[test]
fn certs_too_far_in_future_are_dropped() {
    let ctx = TestContext::new();
    let slot = ctx.sharable_banks.root().slot() + NUM_SLOTS_FOR_VERIFY + 1;
    let cert_type = CertificateType::Finalize(slot);
    let cert = test_create_base2_certificate(
        &ctx.bls_keypairs(),
        ctx.cluster_info.my_shred_version(),
        cert_type,
        &(0..ctx.validator_keypairs.len()).collect::<Vec<usize>>(),
    );
    let cert = ConsensusMessage::Certificate(cert);
    let datagrams = messages_to_datagrams(
        &[(cert, Pubkey::new_unique())],
        ctx.cluster_info.my_shred_version(),
    );
    ctx.send_datagrams(datagrams);

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
