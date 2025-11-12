use {
    bencher::{benchmark_group, benchmark_main, Bencher},
    rand::{thread_rng, Rng},
    solana_entry::entry::Entry,
    solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo, node::Node},
    solana_hash::Hash,
    solana_keypair::Keypair,
    solana_ledger::{
        genesis_utils::{create_genesis_config, GenesisConfigInfo},
        shred::{ProcessShredsStats, ReedSolomonCache, Shredder},
    },
    solana_net_utils::{sockets::bind_to_localhost_unique, SocketAddrSpace},
    solana_pubkey as pubkey,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_signer::Signer,
    solana_time_utils::{timestamp, AtomicInterval},
    solana_turbine::{
        broadcast_stage::{
            broadcast_metrics::TransmitShredsStats, broadcast_shreds, BroadcastSocket,
            BroadcastStage,
        },
        cluster_nodes::ClusterNodesCache,
    },
    std::{collections::HashMap, sync::Arc, time::Duration},
};

fn broadcast_shreds_bench(b: &mut Bencher) {
    agave_logger::setup();
    let leader_keypair = Arc::new(Keypair::new());
    let (quic_endpoint_sender, _quic_endpoint_receiver) =
        tokio::sync::mpsc::channel(/*capacity:*/ 128);
    let leader_info = Node::new_localhost_with_pubkey(&leader_keypair.pubkey());
    let cluster_info = ClusterInfo::new(
        leader_info.info,
        leader_keypair.clone(),
        SocketAddrSpace::Unspecified,
    );
    let socket = bind_to_localhost_unique().expect("should bind");
    let socket = BroadcastSocket::Udp(&socket);
    let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
    let bank = Bank::new_for_benches(&genesis_config);
    let bank_forks = BankForks::new_rw_arc(bank);
    let root_bank = bank_forks.read().unwrap().root_bank();

    const NUM_SHREDS: usize = 32;

    let shredder = Shredder::new(
        root_bank.slot(),
        root_bank.parent_slot(),
        0, // reference_tick
        0, // version
    )
    .unwrap();

    let entries = vec![Entry::new(&Hash::default(), 0, vec![])];
    let data_shreds = shredder.make_merkle_shreds_from_entries(
        &leader_keypair,
        &entries,
        true,            // is_last_in_slot
        Hash::default(), // chained_merkle_root
        0,               // next_shred_index
        0,               // next_code_index
        &ReedSolomonCache::default(),
        &mut ProcessShredsStats::default(),
    );
    let shreds: Vec<_> = data_shreds.take(NUM_SHREDS).collect();

    let mut stakes = HashMap::new();
    const NUM_PEERS: usize = 200;
    for _ in 0..NUM_PEERS {
        let id = pubkey::new_rand();
        let contact_info = ContactInfo::new_localhost(&id, timestamp());
        cluster_info.insert_info(contact_info);
        stakes.insert(id, thread_rng().gen_range(1..NUM_PEERS) as u64);
    }
    let cluster_info = Arc::new(cluster_info);
    let cluster_nodes_cache = ClusterNodesCache::<BroadcastStage>::new(
        8,                      // cap
        Duration::from_secs(5), // ttl
    );
    let shreds = Arc::new(shreds);
    let last_datapoint = Arc::new(AtomicInterval::default());
    b.iter(move || {
        let shreds = shreds.clone();
        broadcast_shreds(
            socket,
            &shreds,
            &cluster_nodes_cache,
            &last_datapoint,
            &mut TransmitShredsStats::default(),
            &cluster_info,
            &bank_forks,
            &SocketAddrSpace::Unspecified,
            &quic_endpoint_sender,
        )
        .unwrap();
    });
}

benchmark_group!(benches, broadcast_shreds_bench);
benchmark_main!(benches);
