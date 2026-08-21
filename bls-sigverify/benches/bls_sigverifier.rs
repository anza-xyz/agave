#![allow(clippy::arithmetic_side_effects)]
use {
    agave_bls_sigverify::{
        bls_sigverifier::{SigVerifier, SigVerifierChannels, SigVerifierContext},
        generated_cert_types::GeneratedCertTypes,
        rewards::RewardInput,
    },
    agave_votor_messages::{
        VerifiedVoterSlotsReceiver,
        consensus_message::{ConsensusMessage, VoteMessage},
        metric_types::ConsensusMetricsEventReceiver,
        migration::MigrationStatus,
        sig_verified_messages::SigVerifiedBatch,
        unverified_vote_message::UnverifiedCertificate,
        vote::Vote,
        wire::{VersionedWireConsensusMessage, get_vote_payload_to_sign},
    },
    criterion::{BatchSize, Criterion, criterion_group, criterion_main},
    crossbeam_channel::{Receiver, Sender, bounded},
    solana_clock::Slot,
    solana_epoch_schedule::EpochSchedule,
    solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo},
    solana_keypair::Keypair,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_net_utils::SocketAddrSpace,
    solana_perf::packet::{BytesPacket, BytesPacketBatch, PacketBatch},
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank,
        bank_forks::BankForks,
        genesis_utils::{
            ValidatorVoteKeypairs, create_genesis_config_with_alpenglow_vote_accounts,
        },
    },
    solana_signer::Signer,
    solana_streamer::nonblocking::simple_qos::SimpleQosBanlist,
    std::{
        hint::black_box,
        num::NonZero,
        sync::{Arc, RwLock},
    },
};

fn new_test_banlist() -> Arc<SimpleQosBanlist> {
    let (banlist, _banlist_eviction_receiver) = SimpleQosBanlist::new();
    Arc::new(banlist)
}

struct TestContext {
    verifier: SigVerifier,
    bank_forks: Arc<RwLock<BankForks>>,
    cluster_info: Arc<ClusterInfo>,
    _packet_sender: Sender<PacketBatch>,
    _repair_receiver: VerifiedVoterSlotsReceiver,
    _reward_receiver: Receiver<RewardInput>,
    _pool_receiver: Receiver<SigVerifiedBatch>,
    _metrics_receiver: ConsensusMetricsEventReceiver,
    _certificate_sender: Sender<(Slot, UnverifiedCertificate)>,
}

impl TestContext {
    fn new() -> (Self, Vec<ValidatorVoteKeypairs>) {
        let (channel_to_pool, pool_receiver) = bounded(1024);
        let num_validators = 1000;
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
        let leader_schedule = Arc::new(LeaderScheduleCache::new_from_bank(&sharable_banks.root()));

        let (channel_to_repair, repair_receiver) = bounded(1024);
        let (channel_to_reward, reward_receiver) = bounded(1024);
        let (packet_sender, packet_receiver) = bounded(1024);
        let (certificate_sender, certificate_receiver) = bounded(1024);
        let (channel_to_metrics, metrics_receiver) = bounded(1024);

        let generated_cert_types = Arc::new(GeneratedCertTypes::default());
        let banlist = new_test_banlist();
        let verifier = SigVerifier::new(
            SigVerifierContext {
                migration_status: Arc::new(MigrationStatus::default()),
                banlist,
                sharable_banks,
                cluster_info: cluster_info.clone(),
                leader_schedule,
                num_threads: 4,
                generated_cert_types,
            },
            SigVerifierChannels {
                packet_receiver,
                certificate_receiver,
                channel_to_repair,
                channel_to_reward,
                channel_to_pool,
                channel_to_metrics,
            },
        );
        (
            Self {
                verifier,
                bank_forks,
                cluster_info,
                _packet_sender: packet_sender,
                _repair_receiver: repair_receiver,
                _reward_receiver: reward_receiver,
                _pool_receiver: pool_receiver,
                _metrics_receiver: metrics_receiver,
                _certificate_sender: certificate_sender,
            },
            validator_keypairs,
        )
    }
}

fn generate_vote_batches(
    root_bank: &Bank,
    cluster_info: &ClusterInfo,
    keypairs: &[ValidatorVoteKeypairs],
    batch_size: usize,
) -> Vec<PacketBatch> {
    let mut packets = vec![];
    for (n, (rank, keypair)) in keypairs
        .iter()
        .enumerate()
        .cycle()
        .enumerate()
        .take(batch_size)
    {
        let slot = (n / keypairs.len()) as u64 + root_bank.slot() + 1;
        let vote = Vote::new_skip_vote(slot);
        let vote_payload = get_vote_payload_to_sign(vote, cluster_info.my_shred_version());
        let rank = rank as u16;
        let signature = keypair.bls_keypair.sign(&vote_payload).into();
        let consensus_message = ConsensusMessage::Vote(VoteMessage {
            vote,
            signature,
            rank,
            stake: NonZero::new(123).unwrap(),
        });
        let packet = message_to_packet(
            &consensus_message,
            cluster_info.my_shred_version(),
            keypair.node_keypair.pubkey(),
        );
        let packet = PacketBatch::Bytes(BytesPacketBatch::from(vec![packet]));
        packets.push(packet);
    }
    packets
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

fn bench_extract_votes(c: &mut Criterion) {
    let (mut ctx, keypairs) = TestContext::new();
    let mut group = c.benchmark_group("bench_extract_votes");
    let batch_size = 5_000;
    let label = format!("batch_{batch_size}");
    group.bench_function(&label, |b| {
        b.iter_batched(
            || {
                let certs = vec![];
                let root_bank = ctx.bank_forks.read().unwrap().root_bank();
                let cluster_info = ctx.cluster_info.clone();
                let batches =
                    generate_vote_batches(&root_bank, &cluster_info, &keypairs, batch_size);
                (batches, certs, root_bank)
            },
            |(batches, certs, root_bank)| {
                let _extracted_msgs = ctx.verifier.extract_and_filter_msgs(
                    black_box(batches),
                    black_box(certs),
                    &root_bank,
                );
            },
            BatchSize::SmallInput,
        )
    });
    group.finish();
}

criterion_group!(benches, bench_extract_votes);
criterion_main!(benches);
