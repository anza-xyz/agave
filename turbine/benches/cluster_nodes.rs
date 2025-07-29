use {
    bencher::{benchmark_group, benchmark_main, Bencher},
    rand::{seq::SliceRandom, Rng},
    solana_clock::Slot,
    solana_cluster_type::ClusterType,
    solana_gossip::contact_info::ContactInfo,
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::shred::{ProcessShredsStats, ReedSolomonCache, Shredder},
    solana_pubkey::Pubkey,
    solana_streamer::socket::SocketAddrSpace,
    solana_turbine::{
        cluster_nodes::{make_test_cluster, new_cluster_nodes, ClusterNodes},
        retransmit_stage::RetransmitStage,
    },
};

fn make_cluster_nodes<R: Rng>(
    rng: &mut R,
    unstaked_ratio: Option<(u32, u32)>,
) -> (Vec<ContactInfo>, ClusterNodes<RetransmitStage>) {
    let (nodes, stakes, cluster_info) = make_test_cluster(rng, 5_000, unstaked_ratio);
    let cluster_nodes =
        new_cluster_nodes::<RetransmitStage>(&cluster_info, ClusterType::Development, &stakes);
    (nodes, cluster_nodes)
}

#[allow(clippy::arithmetic_side_effects)]
fn get_retransmit_peers_deterministic(
    cluster_nodes: &ClusterNodes<RetransmitStage>,
    slot: Slot,
    slot_leader: &Pubkey,
) {
    let keypair = Keypair::new();
    let merkle_root = Some(Hash::default());
    let reed_solomon_cache = ReedSolomonCache::default();
    let mut stats = ProcessShredsStats::default();
    let parent_slot = if slot > 0 { slot - 1 } else { 0 };
    let shredder = Shredder::new(slot, parent_slot, 0, 0).unwrap();

    let shreds = shredder.make_merkle_shreds_from_entries(
        &keypair,
        &[],  // entries
        true, // is_last_in_slot
        merkle_root,
        0, // next_shred_index
        0, // next_code_index
        &reed_solomon_cache,
        &mut stats,
    );

    for shred in shreds {
        let _retransmit_peers = cluster_nodes.get_retransmit_addrs(
            slot_leader,
            &shred.id(),
            200, // fanout
            &SocketAddrSpace::Unspecified,
        );
    }
}

fn get_retransmit_peers_deterministic_wrapper(b: &mut Bencher, unstaked_ratio: Option<(u32, u32)>) {
    let mut rng = rand::thread_rng();
    let (nodes, cluster_nodes) = make_cluster_nodes(&mut rng, unstaked_ratio);
    let slot_leader = *nodes[1..].choose(&mut rng).unwrap().pubkey();
    let slot = rand::random::<u64>();
    b.iter(|| get_retransmit_peers_deterministic(&cluster_nodes, slot, &slot_leader));
}

fn bench_get_retransmit_peers_deterministic_unstaked_ratio_1_2(b: &mut Bencher) {
    get_retransmit_peers_deterministic_wrapper(b, Some((1, 2)));
}

fn bench_get_retransmit_peers_deterministic_unstaked_ratio_1_32(b: &mut Bencher) {
    get_retransmit_peers_deterministic_wrapper(b, Some((1, 32)));
}

benchmark_group!(
    benches,
    bench_get_retransmit_peers_deterministic_unstaked_ratio_1_2,
    bench_get_retransmit_peers_deterministic_unstaked_ratio_1_32
);
benchmark_main!(benches);
