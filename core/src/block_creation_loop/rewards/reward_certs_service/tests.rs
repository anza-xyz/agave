use {
    super::*,
    crate::block_creation_loop::{
        SlotMetrics,
        rewards::msg_types::{RewardRespSucc, RewardResponse},
    },
    agave_bls_sigverify::{
        bls_sigverifier::{SigVerifierChannels, SigVerifierContext, spawn_service},
        generated_cert_types::GeneratedCertTypes,
        metric_types::ConsensusMetricsEventReceiver,
        sig_verified_messages::{SigVerifiedBatch, VoteAggregate},
    },
    agave_votor::aggregate_accumulator::AggregateAccumulator,
    agave_votor_messages::{
        certificate::{Certificate, CertificateType},
        consensus_message::{Block, VoteMessage},
        migration::MigrationStatus,
        reward_certificate::NUM_SLOTS_FOR_REWARD,
        stake::NonZeroStake,
        unverified_vote_message::UnverifiedCertificate,
        vote::Vote,
        wire::{VersionedWireConsensusMessage, get_vote_payload_to_sign},
    },
    agave_votor_transport::{
        PeerList, PeerListSender,
        endpoint::{Datagram, QuicDatagramEndpoint},
    },
    bytes::Bytes,
    solana_clock::Slot,
    solana_epoch_schedule::EpochSchedule,
    solana_gossip::contact_info::ContactInfo,
    solana_leader_schedule::{FixedSchedule, LeaderSchedule, SlotLeader},
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_net_utils::{
        SocketAddrSpace, quic_socket::QuicSocket, sockets::bind_to_localhost_unique,
    },
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank,
        bank_forks::BankForks,
        genesis_utils::{ValidatorVoteKeypairs, create_genesis_config_with_vote_accounts},
        validated_reward_certificate::ValidatedRewardCert,
    },
    solana_signer::Signer,
    solana_signer_store::{Decoded, decode},
    solana_streamer::evicting_sender::EvictingSender,
    std::{
        collections::{HashMap, HashSet},
        net::SocketAddr,
        num::NonZeroUsize,
        sync::RwLock,
        time::{Duration, Instant},
    },
    tokio::{
        runtime::{Handle, Runtime},
        sync::{mpsc, watch},
    },
    tokio_util::sync::CancellationToken,
};

const SHRED_VERSION: u16 = 7;
const FIRST_SLOT: Slot = 100;
const RECEIVE_TIMEOUT: Duration = Duration::from_secs(10);

fn wait_for_recovery(response: RewardResponse) -> RewardRespSucc {
    let baseline = response.result.unwrap();
    match response.recovery {
        Some(recovery) => match recovery.recv_timeout(RECEIVE_TIMEOUT) {
            Ok(recovered) => recovered,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => baseline,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                panic!("optional reward response did not complete")
            }
        },
        None => baseline,
    }
}

/// The peer list enforces the missing A -> C path at the actual QUIC admission layer.
struct Node {
    id: Pubkey,
    address: SocketAddr,
    runtime: Handle,
    endpoint: Option<QuicDatagramEndpoint>,
    egress: mpsc::Sender<Bytes>,
    ingress: Receiver<Datagram>,
    peers: PeerListSender,
}

impl Node {
    fn new(runtime: &Runtime, keys: &ValidatorVoteKeypairs) -> Self {
        let inbound = bind_to_localhost_unique().unwrap();
        let address = inbound.local_addr().unwrap();
        let outbound = bind_to_localhost_unique().unwrap();
        let (ingress_sender, ingress) = bounded(64);
        let (peers, peer_receiver) = watch::channel(Arc::new(PeerList {
            peers: HashMap::default(),
            push_enabled: true,
        }));
        let (egress, endpoint) = QuicDatagramEndpoint::spawn(
            runtime.handle(),
            &keys.node_keypair,
            vec![QuicSocket::Kernel(inbound)],
            QuicSocket::Kernel(outbound),
            ingress_sender,
            peer_receiver,
            SocketAddrSpace::Unspecified,
            100,
            CancellationToken::new(),
        )
        .unwrap();
        Self {
            id: keys.node_keypair.pubkey(),
            address,
            runtime: runtime.handle().clone(),
            endpoint: Some(endpoint),
            egress,
            ingress,
            peers,
        }
    }

    fn set_peers(&self, peers: &[&Self]) {
        self.peers
            .send(Arc::new(PeerList {
                peers: peers
                    .iter()
                    .map(|peer| (peer.id, Some(peer.address)))
                    .collect(),
                push_enabled: true,
            }))
            .unwrap();
    }

    fn send(&self, message: VersionedWireConsensusMessage) {
        self.egress
            .try_send(wincode::serialize(&message).unwrap().into())
            .unwrap();
    }

    fn connect_to(&self, receiver: &Self) {
        // Establish delivery before sending the votes under test. QUIC datagrams sent while
        // the initial connection is still opening may be dropped by the transport.
        let probe = Bytes::from_static(b"reward recovery transport ready");
        let deadline = Instant::now() + RECEIVE_TIMEOUT;
        loop {
            self.egress.try_send(probe.clone()).unwrap();
            if let Ok(datagram) = receiver.ingress.recv_timeout(Duration::from_millis(50)) {
                assert_eq!(datagram.peer_pubkey, self.id);
                assert_eq!(datagram.message, probe);
                return;
            }
            assert!(Instant::now() < deadline, "QUIC connection did not deliver");
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if let Some(endpoint) = self.endpoint.take() {
            endpoint.stop();
            self.runtime.block_on(endpoint.join());
        }
    }
}

struct Verifier {
    exit: Arc<AtomicBool>,
    handler: Option<JoinHandle<()>>,
    rewards: Receiver<RewardInput>,
    pool: Receiver<SigVerifiedBatch>,
    // Keep both verifier input channels and its metrics destination connected until shutdown.
    _certificate_sender: Sender<(Slot, UnverifiedCertificate)>,
    _metrics_receiver: ConsensusMetricsEventReceiver,
}

impl Verifier {
    fn new(
        node: &Node,
        cluster_info: Arc<ClusterInfo>,
        sharable_banks: SharableBanks,
        leader_schedule: Arc<LeaderScheduleCache>,
    ) -> Self {
        let exit = Arc::new(AtomicBool::new(false));
        let (certificate_sender, certificate_receiver) = bounded(64);
        let (channel_to_repair, _) = EvictingSender::new_bounded(64);
        let (channel_to_reward, rewards) = bounded(64);
        let (channel_to_pool, pool) = bounded(64);
        let (channel_to_metrics, metrics_receiver) = bounded(64);
        let context = SigVerifierContext {
            migration_status: Arc::new(MigrationStatus::post_migration_status()),
            ban_sender: node.endpoint.as_ref().unwrap().ban_sender(),
            sharable_banks,
            highest_parent_ready: Arc::new(RwLock::new((FIRST_SLOT, Block::new_unique(99)))),
            cluster_info,
            leader_schedule,
            num_threads: 1,
            generated_cert_types: Arc::new(GeneratedCertTypes::default()),
        };
        let channels = SigVerifierChannels::new(
            node.ingress.clone(),
            certificate_receiver,
            channel_to_repair,
            channel_to_reward,
            channel_to_pool,
            channel_to_metrics,
        );
        let handler = spawn_service(exit.clone(), context, channels);
        Self {
            exit,
            handler: Some(handler),
            rewards,
            pool,
            _certificate_sender: certificate_sender,
            _metrics_receiver: metrics_receiver,
        }
    }

    fn receive_vote(&self, message: &VoteMessage) -> VoteAggregate {
        let SigVerifiedBatch::Votes(mut votes) = self.pool.recv_timeout(RECEIVE_TIMEOUT).unwrap()
        else {
            panic!("expected a verified raw vote");
        };
        assert_eq!(votes.len(), 1);
        let aggregate = votes.pop().unwrap();
        assert_eq!(*aggregate.vote(), message.vote);
        assert_eq!(
            aggregate.ranks().iter_ones().collect::<Vec<_>>(),
            vec![usize::from(message.rank)]
        );
        assert_eq!(aggregate.stake(), message.stake);
        aggregate
    }
}

impl Drop for Verifier {
    fn drop(&mut self) {
        self.exit.store(true, Ordering::Relaxed);
        if let Some(handler) = self.handler.take() {
            while !handler.is_finished() {
                // Also unblock a verifier whose output was left unread by a failed assertion.
                let _ = self.pool.recv_timeout(Duration::from_millis(10));
            }
            let result = handler.join();
            if !thread::panicking() {
                result.unwrap();
            }
        }
    }
}

struct Rewards {
    exit: Arc<AtomicBool>,
    handler: Option<JoinHandle<()>>,
    input: Sender<RewardInput>,
    requests: Sender<RewardRequest>,
    prepared: Receiver<Slot>,
}

impl Rewards {
    fn new(cluster_info: Arc<ClusterInfo>, sharable_banks: SharableBanks) -> Self {
        let exit = Arc::new(AtomicBool::new(false));
        let (input, input_receiver) = bounded(0);
        let (requests, request_receiver) = bounded(1);
        let mut builder = CertsBuilder::new(cluster_info.clone());
        let prepared = builder.observe_recovery_completions();
        let context = Context::new(
            exit.clone(),
            cluster_info,
            input_receiver,
            request_receiver,
            sharable_banks,
            builder,
        );
        let handler = thread::spawn(move || context.run());
        Self {
            exit,
            handler: Some(handler),
            input,
            requests,
            prepared,
        }
    }

    fn forward(&self, input: RewardInput) {
        self.input.send_timeout(input, RECEIVE_TIMEOUT).unwrap();
    }

    fn flush(&self) {
        // Rendezvous with the service only after it has finished handling the preceding
        // input. This avoids a build request overtaking it in select_biased!.
        self.forward(RewardInput::Certificates(vec![]));
    }

    fn forward_vote(&self, verifier: &Verifier, message: &VoteMessage) {
        let input = verifier.rewards.recv_timeout(RECEIVE_TIMEOUT).unwrap();
        let RewardInput::External(aggregates) = &input else {
            panic!("expected the verified aggregate before its raw signature sidecar");
        };
        assert_eq!(aggregates.len(), 1);
        assert_eq!(*aggregates[0].vote(), message.vote);
        assert_eq!(
            aggregates[0].ranks().iter_ones().collect::<Vec<_>>(),
            vec![usize::from(message.rank)]
        );
        self.forward(input);
        let input = verifier.rewards.recv_timeout(RECEIVE_TIMEOUT).unwrap();
        let RewardInput::VoteSignatures { vote, signatures } = &input else {
            panic!("expected the original raw signature sidecar");
        };
        assert_eq!(*vote, message.vote);
        assert_eq!(
            signatures,
            &vec![(
                message.rank,
                solana_bls_signatures::Signature::from(message.signature)
            )]
        );
        self.forward(input);
        self.flush();
    }

    fn forward_certificate(&self, verifier: &Verifier, expected: &Certificate) {
        let input = verifier.rewards.recv_timeout(RECEIVE_TIMEOUT).unwrap();
        let RewardInput::Certificates(certificates) = &input else {
            panic!("expected a certificate verified from the QUIC input");
        };
        assert_eq!(certificates, &vec![expected.clone()]);
        self.forward(input);
        self.flush();
        // Finality has passed the source slot, so the certificate is only reward evidence. The
        // verifier hands it off after its pool send, which therefore never carried it.
        assert!(verifier.pool.is_empty());
    }

    fn wait_prepared(&self, source_slot: Slot) {
        assert_eq!(
            self.prepared.recv_timeout(RECEIVE_TIMEOUT).unwrap(),
            source_slot
        );
    }

    fn request(&self, source_slot: Slot) -> RewardRespSucc {
        let (reply_sender, reply_receiver) = bounded(1);
        self.requests
            .send_timeout(
                RewardRequest {
                    bank_slot: source_slot + NUM_SLOTS_FOR_REWARD,
                    reply_sender,
                    request_sent: Instant::now(),
                },
                RECEIVE_TIMEOUT,
            )
            .unwrap();
        wait_for_recovery(reply_receiver.recv_timeout(RECEIVE_TIMEOUT).unwrap())
    }
}

impl Drop for Rewards {
    fn drop(&mut self) {
        self.exit.store(true, Ordering::Relaxed);
        if let Some(handler) = self.handler.take() {
            let result = handler.join();
            if !thread::panicking() {
                result.unwrap();
            }
        }
    }
}

fn cluster_info(keys: &ValidatorVoteKeypairs) -> Arc<ClusterInfo> {
    let keypair = Arc::new(keys.node_keypair.insecure_clone());
    let mut contact_info = ContactInfo::new_localhost(&keypair.pubkey(), 0);
    contact_info.set_shred_version(SHRED_VERSION);
    Arc::new(ClusterInfo::new(
        contact_info,
        keypair,
        SocketAddrSpace::Unspecified,
    ))
}

fn signed_vote(bank: &Bank, keys: &ValidatorVoteKeypairs, block: Block) -> VoteMessage {
    let rank_map = bank.get_rank_map(block.slot).unwrap();
    let rank = *rank_map
        .get_rank_for_vote_pubkey(&keys.vote_keypair.pubkey())
        .unwrap();
    let entry = rank_map.get_pubkey_stake_entry(usize::from(rank)).unwrap();
    assert_eq!(entry.node_pubkey, keys.node_keypair.pubkey());
    let vote = Vote::new_notarization_vote(block);
    VoteMessage {
        vote,
        signature: keys
            .bls_keypair
            .sign(&get_vote_payload_to_sign(vote, SHRED_VERSION))
            .into(),
        rank,
        stake: entry.stake,
    }
}

fn assert_rewards(
    bank_forks: &RwLock<BankForks>,
    root: Arc<Bank>,
    response: &RewardRespSucc,
    block: Block,
    expected: &[&ValidatorVoteKeypairs],
) {
    assert!(response.skip.is_none());
    let notar = response.notar.as_ref().unwrap();
    assert_eq!(notar.slot, block.slot);
    assert_eq!(notar.block_id, block.block_id);
    let rank_map = root.get_rank_map(block.slot).unwrap();
    let Decoded::Base2(ranks) = decode(notar.bitmap(), rank_map.len()).unwrap() else {
        panic!("reward output must use base2");
    };
    let mut expected_ranks: Vec<_> = expected
        .iter()
        .map(|keys| {
            usize::from(
                *rank_map
                    .get_rank_for_vote_pubkey(&keys.vote_keypair.pubkey())
                    .unwrap(),
            )
        })
        .collect();
    expected_ranks.sort_unstable();
    assert_eq!(ranks.iter_ones().collect::<Vec<_>>(), expected_ranks);
    let expected_validators: HashSet<_> = expected
        .iter()
        .map(|keys| keys.vote_keypair.pubkey())
        .collect();
    assert_eq!(response.validators.len(), expected_validators.len());
    assert_eq!(
        response.validators.iter().copied().collect::<HashSet<_>>(),
        expected_validators
    );
    let submission_bank = Bank::new_from_parent_with_bank_forks(
        bank_forks,
        root,
        SlotLeader::default(),
        block.slot + NUM_SLOTS_FOR_REWARD,
    );
    assert!(
        ValidatedRewardCert::try_new(
            &submission_bank,
            SHRED_VERSION,
            &response.skip,
            &response.notar,
        )
        .unwrap()
        .is_some()
    );
}

struct ServiceFixture {
    keys: Vec<ValidatorVoteKeypairs>,
    bank_forks: Arc<RwLock<BankForks>>,
    banks: SharableBanks,
    root: Arc<Bank>,
    block: Block,
}

impl ServiceFixture {
    fn new() -> Self {
        let keys: Vec<_> = (0..3).map(|_| ValidatorVoteKeypairs::new_rand()).collect();
        let mut genesis =
            create_genesis_config_with_vote_accounts(1_000_000_000, &keys, vec![450, 350, 200])
                .genesis_config;
        genesis.epoch_schedule = EpochSchedule::without_warmup();
        let (bank, bank_forks) = Bank::new_for_tests(&genesis).wrap_with_bank_forks_for_tests();
        Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            bank,
            SlotLeader::default(),
            FIRST_SLOT - 1,
        );
        bank_forks
            .write()
            .unwrap()
            .set_root(FIRST_SLOT - 1, None, None);
        let banks = bank_forks.read().unwrap().sharable_banks();
        let root = banks.root();
        Self {
            keys,
            bank_forks,
            banks,
            root,
            block: Block::new_unique(FIRST_SLOT),
        }
    }

    fn certificate(&self, block: Block, voters: &[usize]) -> Certificate {
        let mut accumulator = AggregateAccumulator::new(self.keys.len());
        for &voter in voters {
            accumulator
                .add_own_vote_message(&signed_vote(&self.root, &self.keys[voter], block))
                .unwrap();
        }
        accumulator
            .try_build_base2_cert(
                CertificateType::Notarize(block),
                self.root.get_rank_map(block.slot).unwrap().total_stake(),
            )
            .unwrap()
            .unwrap()
    }

    fn direct_vote(&self, block: Block) -> RewardInput {
        RewardInput::External(vec![VoteAggregate::new_from_verified_vote(
            self.root.get_rank_map(block.slot).unwrap().len(),
            signed_vote(&self.root, &self.keys[1], block),
        )])
    }

    fn assert_direct_reward(&self, response: &RewardRespSucc, block: Block) {
        assert_rewards(
            &self.bank_forks,
            self.root.clone(),
            response,
            block,
            &[&self.keys[1]],
        );
    }
}

/// Always release a paused controller before Context's drop joins it, including on assertion failure.
struct ReleaseRecoveryGate(Sender<()>);

impl Drop for ReleaseRecoveryGate {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

#[test]
fn test_blocked_recovery_admission_and_full_build_queue_preserve_direct_baselines() {
    let fixture = ServiceFixture::new();
    let cluster_info = cluster_info(&fixture.keys[2]);
    let mut builder = CertsBuilder::new(cluster_info.clone());
    // Fill the actual source-slot seed cap before moving Recovery to its controller. The gated
    // incoming proof is a broader signer set, so release resumes real admission scoring at the cap.
    for block in std::iter::once(fixture.block).chain((0..3).map(|_| Block::new_unique(FIRST_SLOT)))
    {
        builder.handle_input(
            &fixture.root,
            RewardInput::Certificates(vec![fixture.certificate(block, &[0, 1])]),
        );
    }
    let (input, input_receiver) = bounded(1);
    let (mut requestor, request_receiver) = CertsRequestor::new();
    let (admission_started, started) = bounded(1);
    let (release, admission_release) = bounded(1);
    let mut context = Context::new_with_gates(
        Arc::new(AtomicBool::new(false)),
        cluster_info,
        input_receiver,
        request_receiver,
        fixture.banks.clone(),
        builder,
        Some((admission_started, admission_release)),
        None,
    );
    let release = ReleaseRecoveryGate(release);
    input
        .send(RewardInput::Certificates(vec![
            fixture.certificate(fixture.block, &[0, 1, 2]),
        ]))
        .unwrap();
    context.process_next_input().unwrap();
    context.process_next_input().unwrap();
    started.recv_timeout(RECEIVE_TIMEOUT).unwrap();

    // The paused controller cannot drain its sixty-four evidence slots. The sixty-fifth raw
    // signature input therefore takes the full path without blocking the direct-vote service.
    let message = signed_vote(&fixture.root, &fixture.keys[1], fixture.block);
    for _ in 0..65 {
        input
            .send(RewardInput::VoteSignatures {
                vote: message.vote,
                signatures: vec![(message.rank, message.signature.into())],
            })
            .unwrap();
        context.process_next_input().unwrap();
    }

    // The controller's build queue holds sixteen requests. Twenty requests while admission is
    // paused exercise both its occupied and full paths, using the actual one-shot cutoff receiver.
    // No optional gate is released until every baseline has been consumed and checked.
    for offset in 0..20 {
        let block = if offset == 0 {
            fixture.block
        } else {
            Block::new_unique(FIRST_SLOT + offset)
        };
        input.send(fixture.direct_vote(block)).unwrap();
        context.process_next_input().unwrap();
        let mut metrics = SlotMetrics::new(block.slot + NUM_SLOTS_FOR_REWARD, true);
        let token = requestor
            .request_reward_certs(
                &mut metrics,
                fixture.keys[2].node_keypair.pubkey(),
                block.slot + NUM_SLOTS_FOR_REWARD,
            )
            .unwrap();
        assert!(token.is_some());
        context.process_next_input().unwrap();
        let baseline = requestor
            .recv_reward_certs(&mut metrics, fixture.keys[2].node_keypair.pubkey(), token)
            .unwrap();
        fixture.assert_direct_reward(&baseline, block);
        assert_eq!(metrics.reward_certs_skipped, 0);
    }

    // Prove the optional queue really is saturated, rather than only testing a slow consumer:
    // a further published baseline carries an already disconnected upgrade receiver.
    let block = Block::new_unique(FIRST_SLOT + 20);
    input.send(fixture.direct_vote(block)).unwrap();
    context.process_next_input().unwrap();
    let mut metrics = SlotMetrics::new(block.slot + NUM_SLOTS_FOR_REWARD, true);
    let token = requestor
        .request_reward_certs(
            &mut metrics,
            fixture.keys[2].node_keypair.pubkey(),
            block.slot + NUM_SLOTS_FOR_REWARD,
        )
        .unwrap()
        .unwrap();
    context.process_next_input().unwrap();
    let response = token.reply_receiver.try_recv().unwrap();
    assert_eq!(
        response.recovery.unwrap().try_recv(),
        Err(crossbeam_channel::TryRecvError::Disconnected),
    );
    fixture.assert_direct_reward(&response.result.unwrap(), block);

    drop(release);
    context.shutdown_recovery();
    // None of the work released after the consumed cutoff can recreate the sealed first source.
    let (reply_sender, reply_receiver) = bounded(1);
    context
        .builder
        .build_request(
            Ok(RewardRequest {
                bank_slot: FIRST_SLOT + NUM_SLOTS_FOR_REWARD,
                reply_sender,
                request_sent: Instant::now(),
            }),
            None,
        )
        .unwrap();
    assert_eq!(
        wait_for_recovery(reply_receiver.try_recv().unwrap()),
        RewardRespSucc::default(),
    );
}

#[test]
fn test_blocked_recovery_selection_preserves_direct_baseline_at_requestor_cutoff() {
    let fixture = ServiceFixture::new();
    let cluster_info = cluster_info(&fixture.keys[2]);
    let mut builder = CertsBuilder::new(cluster_info.clone());
    let prepared = builder.observe_recovery_completions();
    builder.handle_input(
        &fixture.root,
        RewardInput::Certificates(vec![fixture.certificate(fixture.block, &[0, 1])]),
    );
    let (input, input_receiver) = bounded(1);
    let (mut requestor, request_receiver) = CertsRequestor::new();
    let (selection_started, started) = bounded(1);
    let (release, selection_release) = bounded(1);
    let mut context = Context::new_with_gates(
        Arc::new(AtomicBool::new(false)),
        cluster_info,
        input_receiver,
        request_receiver,
        fixture.banks.clone(),
        builder,
        None,
        Some((selection_started, selection_release)),
    );
    let release = ReleaseRecoveryGate(release);
    assert_eq!(prepared.recv_timeout(RECEIVE_TIMEOUT).unwrap(), FIRST_SLOT);
    input.send(fixture.direct_vote(fixture.block)).unwrap();
    context.process_next_input().unwrap();
    let mut metrics = SlotMetrics::new(FIRST_SLOT + NUM_SLOTS_FOR_REWARD, true);
    let token = requestor
        .request_reward_certs(
            &mut metrics,
            fixture.keys[2].node_keypair.pubkey(),
            FIRST_SLOT + NUM_SLOTS_FOR_REWARD,
        )
        .unwrap();
    assert!(token.is_some());
    context.process_next_input().unwrap();
    started.recv_timeout(RECEIVE_TIMEOUT).unwrap();
    // A checked A+B improvement exists, but its selection is paused. The real cutoff call must
    // still return B's complete direct response, without waiting for that improvement.
    let baseline = requestor
        .recv_reward_certs(&mut metrics, fixture.keys[2].node_keypair.pubkey(), token)
        .unwrap();
    fixture.assert_direct_reward(&baseline, fixture.block);
    assert_eq!(metrics.reward_certs_skipped, 0);
    drop(release);
    context.shutdown_recovery();
}

#[test]
fn test_direct_votes_publish_baseline_with_recovery_running() {
    let fixture = ServiceFixture::new();
    let cluster_info = cluster_info(&fixture.keys[2]);
    let builder = CertsBuilder::new(cluster_info.clone());
    let (input, input_receiver) = bounded(1);
    let (requests, request_receiver) = bounded(1);
    let mut context = Context::new(
        Arc::new(AtomicBool::new(false)),
        cluster_info,
        input_receiver,
        request_receiver,
        fixture.banks.clone(),
        builder,
    );
    input.send(fixture.direct_vote(fixture.block)).unwrap();
    context.process_next_input().unwrap();
    let (reply_sender, reply_receiver) = bounded(1);
    requests
        .send(RewardRequest {
            bank_slot: FIRST_SLOT + NUM_SLOTS_FOR_REWARD,
            reply_sender,
            request_sent: Instant::now(),
        })
        .unwrap();
    context.process_next_input().unwrap();
    let response = reply_receiver.try_recv().unwrap();
    assert!(response.recovery.is_some());
    fixture.assert_direct_reward(&response.result.unwrap(), fixture.block);
    context.shutdown_recovery();
}

#[test]
fn test_certificate_batch_yields_to_build_requests_and_drops_sealed_remainder() {
    let keys: Vec<_> = (0..3).map(|_| ValidatorVoteKeypairs::new_rand()).collect();
    let mut genesis =
        create_genesis_config_with_vote_accounts(1_000_000_000, &keys, vec![450, 350, 200])
            .genesis_config;
    genesis.epoch_schedule = EpochSchedule::without_warmup();
    let (bank, bank_forks) = Bank::new_for_tests(&genesis).wrap_with_bank_forks_for_tests();
    Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        bank,
        SlotLeader::default(),
        FIRST_SLOT - 1,
    );
    bank_forks
        .write()
        .unwrap()
        .set_root(FIRST_SLOT - 1, None, None);
    let banks = bank_forks.read().unwrap().sharable_banks();
    let root = banks.root();
    let block = Block::new_unique(FIRST_SLOT);
    let certificate = |voters: &[&ValidatorVoteKeypairs]| {
        let mut accumulator = AggregateAccumulator::new(keys.len());
        for voter in voters {
            accumulator
                .add_own_vote_message(&signed_vote(&root, voter, block))
                .unwrap();
        }
        accumulator
            .try_build_base2_cert(
                CertificateType::Notarize(block),
                root.get_rank_map(block.slot).unwrap().total_stake(),
            )
            .unwrap()
            .unwrap()
    };
    let initial = certificate(&[&keys[0], &keys[1]]);
    let broader = certificate(&[&keys[0], &keys[1], &keys[2]]);
    let cluster_info = cluster_info(&keys[2]);
    let mut builder = CertsBuilder::new(cluster_info.clone());
    let prepared = builder.observe_recovery_completions();
    let (input, input_receiver) = bounded(1);
    let (requests, request_receiver) = bounded(1);
    let mut context = Context::new(
        Arc::new(AtomicBool::new(false)),
        cluster_info,
        input_receiver,
        request_receiver,
        banks,
        builder,
    );
    let batch: Vec<_> = std::iter::once(initial)
        .chain(std::iter::repeat_n(broader, 31))
        .collect();
    input.send(RewardInput::Certificates(batch)).unwrap();
    context.process_next_input().unwrap();
    assert_eq!(context.pending_certificates.len(), 32);
    context.process_next_input().unwrap();
    assert_eq!(context.pending_certificates.len(), 31);

    // Seed verification runs separately from admission. Prepare just the first proof before
    // testing whether the request interrupts the remaining certificates in this same batch.
    assert_eq!(prepared.recv_timeout(RECEIVE_TIMEOUT).unwrap(), FIRST_SLOT);

    // A request arriving after the first certificate must interrupt this batch.
    // Its result excludes the broader proof that remains in the same input.
    let (reply_sender, reply_receiver) = bounded(1);
    requests
        .send(RewardRequest {
            bank_slot: FIRST_SLOT + NUM_SLOTS_FOR_REWARD,
            reply_sender,
            request_sent: Instant::now(),
        })
        .unwrap();
    context.process_next_input().unwrap();
    assert_eq!(context.pending_certificates.len(), 31);
    let response = wait_for_recovery(reply_receiver.try_recv().unwrap());
    assert_rewards(&bank_forks, root, &response, block, &[&keys[0], &keys[1]]);
    // No later input is received until the current batch has been consumed.
    input.send(RewardInput::Certificates(vec![])).unwrap();
    for remaining in (0..31).rev() {
        context.process_next_input().unwrap();
        assert_eq!(context.pending_certificates.len(), remaining);
        assert_eq!(context.aggregates_receiver.len(), 1);
    }
    context.process_next_input().unwrap();
    assert!(context.aggregates_receiver.is_empty());

    // The rest of the batch arrived after the build sealed this source slot.
    let (reply_sender, reply_receiver) = bounded(1);
    requests
        .send(RewardRequest {
            bank_slot: FIRST_SLOT + NUM_SLOTS_FOR_REWARD,
            reply_sender,
            request_sent: Instant::now(),
        })
        .unwrap();
    context.process_next_input().unwrap();
    assert_eq!(
        wait_for_recovery(reply_receiver.try_recv().unwrap()),
        RewardRespSucc::default(),
    );
    context.shutdown_recovery();
}

#[test]
fn test_reward_certificate_recovers_vote_lost_on_direct_quic_path_before_cutoff() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let keys: Vec<_> = (0..3).map(|_| ValidatorVoteKeypairs::new_rand()).collect();
    let mut genesis =
        create_genesis_config_with_vote_accounts(1_000_000_000, &keys, vec![450, 350, 200])
            .genesis_config;
    genesis.epoch_schedule = EpochSchedule::without_warmup();
    let (bank, bank_forks) = Bank::new_for_tests(&genesis).wrap_with_bank_forks_for_tests();
    Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        bank,
        SlotLeader::default(),
        FIRST_SLOT - 1,
    );
    bank_forks
        .write()
        .unwrap()
        .set_root(FIRST_SLOT - 1, None, None);
    let banks = bank_forks.read().unwrap().sharable_banks();
    let mut leader_schedule = LeaderScheduleCache::new_from_bank(&banks.root());
    leader_schedule.set_fixed_leader_schedule(Some(FixedSchedule {
        leader_schedule: Arc::new(LeaderSchedule::new_from_schedule(
            vec![SlotLeader {
                id: keys[2].node_keypair.pubkey(),
                vote_address: keys[2].vote_keypair.pubkey(),
            }],
            NonZeroUsize::new(1).unwrap(),
        )),
    }));
    let leader_schedule = Arc::new(leader_schedule);

    let a = Node::new(&runtime, &keys[0]);
    let b = Node::new(&runtime, &keys[1]);
    let c = Node::new(&runtime, &keys[2]);
    a.set_peers(&[&b]);
    b.set_peers(&[&a, &c]);
    c.set_peers(&[&b]);
    a.connect_to(&b);
    b.connect_to(&c);
    let b_verifier = Verifier::new(
        &b,
        cluster_info(&keys[1]),
        banks.clone(),
        leader_schedule.clone(),
    );
    let c_cluster_info = cluster_info(&keys[2]);
    let c_verifier = Verifier::new(&c, c_cluster_info.clone(), banks.clone(), leader_schedule);
    let rewards = Rewards::new(c_cluster_info, banks.clone());

    let mut certificates = Vec::new();
    for source_slot in FIRST_SLOT..FIRST_SLOT + 3 {
        let block = Block::new_unique(source_slot);
        let a_vote = signed_vote(&banks.root(), &keys[0], block);
        a.send(VersionedWireConsensusMessage::new_from_vote(
            a_vote.clone(),
            SHRED_VERSION,
        ));
        // B's certificate must contain the signature actually delivered and verified from A.
        let verified_a = b_verifier.receive_vote(&a_vote);
        let b_vote = signed_vote(&banks.root(), &keys[1], block);
        b.send(VersionedWireConsensusMessage::new_from_vote(
            b_vote.clone(),
            SHRED_VERSION,
        ));
        // C receives B's raw vote, and its verifier/reward sidecar contains B's rank only.
        // A cannot deliver to C because neither endpoint admits the missing A -> C path.
        c_verifier.receive_vote(&b_vote);
        rewards.forward_vote(&c_verifier, &b_vote);
        let root = banks.root();
        let mut accumulator =
            AggregateAccumulator::new(root.get_rank_map(source_slot).unwrap().len());
        accumulator.add_aggregate(&verified_a).unwrap();
        accumulator.add_own_vote_message(&b_vote).unwrap();
        let certificate = accumulator
            .try_build_base2_cert(
                CertificateType::Notarize(block),
                NonZeroStake::new(
                    root.epoch_stakes_from_slot(source_slot)
                        .unwrap()
                        .total_stake(),
                )
                .unwrap(),
            )
            .unwrap()
            .unwrap();
        certificates.push((block, certificate));
    }
    assert!(b_verifier.rewards.is_empty());
    assert!(c_verifier.pool.is_empty());
    assert!(c_verifier.rewards.is_empty());

    // Finality advances beyond all three source slots before C receives any certificate.
    let finalized_root = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        banks.root(),
        SlotLeader::default(),
        FIRST_SLOT + 3,
    );
    bank_forks
        .write()
        .unwrap()
        .set_root(FIRST_SLOT + 3, None, None);

    // No certificate reaches C for the first source slot: its output rewards B alone.
    let baseline = rewards.request(FIRST_SLOT);
    assert_rewards(
        bank_forks.as_ref(),
        finalized_root.clone(),
        &baseline,
        certificates[0].0,
        &[&keys[1]],
    );

    // A verified certificate from B repairs C's missing A vote even after source-slot finality.
    b.send(VersionedWireConsensusMessage::new_from_cert(
        certificates[1].1.clone(),
        SHRED_VERSION,
    ));
    rewards.forward_certificate(&c_verifier, &certificates[1].1);
    rewards.wait_prepared(FIRST_SLOT + 1);
    let recovered = rewards.request(FIRST_SLOT + 1);
    assert_rewards(
        bank_forks.as_ref(),
        finalized_root.clone(),
        &recovered,
        certificates[1].0,
        &[&keys[0], &keys[1]],
    );

    // Building the third slot seals its result while the shared root still lags the cutoff.
    let sealed = rewards.request(FIRST_SLOT + 2);
    assert_rewards(
        bank_forks.as_ref(),
        finalized_root,
        &sealed,
        certificates[2].0,
        &[&keys[1]],
    );
    b.send(VersionedWireConsensusMessage::new_from_cert(
        certificates[2].1.clone(),
        SHRED_VERSION,
    ));
    rewards.forward_certificate(&c_verifier, &certificates[2].1);
    // Deliberately probe the same cutoff again to detect a late input recreating sealed state.
    assert_eq!(rewards.request(FIRST_SLOT + 2), RewardRespSucc::default());
    assert!(c_verifier.pool.is_empty());
    assert!(c_verifier.rewards.is_empty());
}
