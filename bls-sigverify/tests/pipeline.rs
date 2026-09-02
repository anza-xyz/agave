use {
    agave_bls_cert_verify::cert_verify::test_create_base2_certificate,
    agave_bls_sigverify::{
        bls_sigverifier::{self, NUM_SLOTS_FOR_VERIFY, SigVerifierChannels, SigVerifierContext},
        generated_cert_types::GeneratedCertTypes,
    },
    agave_votor_messages::{
        certificate::CertificateType,
        consensus_message::{Block, ConsensusMessage, VoteMessage},
        metric_types::ConsensusMetricsEvent,
        migration::MigrationStatus,
        sig_verified_messages::SigVerifiedBatch,
        vote::Vote,
        wire::{VersionedWireConsensusMessage, get_vote_payload_to_sign},
    },
    agave_votor_transport::endpoint::{Datagram, stub_ban_channel_for_tests},
    bytes::Bytes,
    crossbeam_channel::{TryRecvError, bounded},
    solana_bls_signatures::{Keypair as BlsKeypair, signature::SignatureAffine},
    solana_epoch_schedule::EpochSchedule,
    solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo},
    solana_keypair::Keypair,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_net_utils::SocketAddrSpace,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank,
        bank_forks::BankForks,
        genesis_utils::{
            ValidatorVoteKeypairs, create_genesis_config_with_alpenglow_vote_accounts,
        },
    },
    solana_signer::Signer,
    std::{
        net::{Ipv4Addr, SocketAddr},
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    },
};

const TIMEOUT: Duration = Duration::from_secs(5);

fn datagram(message: ConsensusMessage, shred_version: u16, peer_pubkey: Pubkey) -> Datagram {
    let message = VersionedWireConsensusMessage::new(message, shred_version);
    Datagram {
        peer_pubkey,
        peer_address: SocketAddr::from((Ipv4Addr::LOCALHOST, 1)),
        message: Bytes::from(wincode::serialize(&message).unwrap()),
    }
}

#[test]
fn verifies_and_processes_vote_and_certificate_through_pipeline() {
    let validator_keypairs = (0..10)
        .map(|_| ValidatorVoteKeypairs::new_rand())
        .collect::<Vec<_>>();
    let stakes = (0..validator_keypairs.len())
        .map(|index| 1_000u64.saturating_sub(index as u64))
        .collect();
    let mut genesis = create_genesis_config_with_alpenglow_vote_accounts(
        1_000_000_000,
        &validator_keypairs,
        stakes,
    );
    genesis.genesis_config.epoch_schedule = EpochSchedule::without_warmup();
    let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis.genesis_config));
    let sharable_banks = bank_forks.read().unwrap().sharable_banks();
    let root_bank = sharable_banks.root();

    let identity = Keypair::new();
    let cluster_info = Arc::new(ClusterInfo::new(
        ContactInfo::new_localhost(&identity.pubkey(), 0),
        Arc::new(identity),
        SocketAddrSpace::Unspecified,
    ));
    let shred_version = cluster_info.my_shred_version();
    let leader_schedule = Arc::new(LeaderScheduleCache::new_from_bank(&root_bank));
    let migration_status = Arc::new(MigrationStatus::default());
    migration_status.enable_alpenglow_for_tests();

    let (packet_sender, packet_receiver) = bounded(8);
    let (_certificate_sender, certificate_receiver) = bounded(8);
    let (repair_sender, repair_receiver) = bounded(8);
    let (reward_sender, reward_receiver) = bounded(8);
    let (pool_sender, pool_receiver) = bounded(8);
    let (metrics_sender, metrics_receiver) = bounded(8);
    let (ban_sender, mut ban_receiver) = stub_ban_channel_for_tests(8);
    let exit = Arc::new(AtomicBool::new(false));

    let service = bls_sigverifier::spawn_service(
        exit.clone(),
        SigVerifierContext {
            migration_status,
            ban_sender,
            sharable_banks,
            highest_parent_ready: Arc::new(RwLock::new((
                NUM_SLOTS_FOR_VERIFY,
                Block::new_unique(NUM_SLOTS_FOR_VERIFY.saturating_sub(1)),
            ))),
            cluster_info,
            leader_schedule,
            num_threads: 2,
            generated_cert_types: Arc::new(GeneratedCertTypes::default()),
        },
        SigVerifierChannels::new(
            packet_receiver,
            certificate_receiver,
            repair_sender,
            reward_sender,
            pool_sender,
            metrics_sender,
        ),
    );

    let vote_rank = 2usize;
    let vote = Vote::new_finalization_vote(5);
    let rank_map = root_bank.get_rank_map(vote.slot()).unwrap();
    let vote_message = VoteMessage {
        vote,
        signature: SignatureAffine::from(
            validator_keypairs[vote_rank]
                .bls_keypair
                .sign(&get_vote_payload_to_sign(vote, shred_version)),
        ),
        rank: vote_rank as u16,
        stake: rank_map.get_pubkey_stake_entry(vote_rank).unwrap().stake,
    };

    let cert_type = CertificateType::Finalize(4);
    let bls_keypairs = validator_keypairs
        .iter()
        .map(|keypairs| keypairs.bls_keypair.clone())
        .collect::<Vec<BlsKeypair>>();
    let certificate = test_create_base2_certificate(
        &bls_keypairs,
        shred_version,
        cert_type,
        &[0, 1, 2, 3, 4, 5, 6, 7],
    );

    packet_sender
        .send(datagram(
            ConsensusMessage::Vote(vote_message),
            shred_version,
            validator_keypairs[vote_rank].node_keypair.pubkey(),
        ))
        .unwrap();
    packet_sender
        .send(datagram(
            ConsensusMessage::Certificate(certificate),
            shred_version,
            Pubkey::new_unique(),
        ))
        .unwrap();

    let mut received_vote = false;
    let mut received_certificate = false;
    for _ in 0..2 {
        match pool_receiver.recv_timeout(TIMEOUT).unwrap() {
            SigVerifiedBatch::Votes(votes) => {
                assert_eq!(votes.len(), 1);
                assert_eq!(*votes[0].vote(), vote);
                assert!(votes[0].ranks()[vote_rank]);
                received_vote = true;
            }
            SigVerifiedBatch::Certificates(certificates) => {
                assert_eq!(certificates.len(), 1);
                assert_eq!(certificates[0].cert_type, cert_type);
                received_certificate = true;
            }
        }
    }
    assert!(received_vote);
    assert!(received_certificate);

    assert_eq!(
        repair_receiver.recv_timeout(TIMEOUT).unwrap(),
        (
            validator_keypairs[vote_rank].vote_keypair.pubkey(),
            vec![vote.slot()],
        )
    );
    let (_, metric_events) = metrics_receiver.recv_timeout(TIMEOUT).unwrap();
    assert_eq!(
        metric_events,
        vec![ConsensusMetricsEvent::Vote {
            id: validator_keypairs[vote_rank].vote_keypair.pubkey(),
            vote,
        }]
    );
    // Finalize votes are intentionally not forwarded to rewards.
    assert!(matches!(
        reward_receiver.try_recv(),
        Err(TryRecvError::Empty)
    ));
    assert!(ban_receiver.try_recv().is_err());

    exit.store(true, Ordering::Relaxed);
    drop(packet_sender);
    service.join().unwrap();
}
