use {
    crossbeam_channel::unbounded,
    log::info,
    solana_local_cluster::local_cluster::{ClusterConfig, LocalCluster},
    solana_net_utils::VALIDATOR_PORT_RANGE,
    solana_sdk::{
        native_token::LAMPORTS_PER_SOL, net::DEFAULT_TPU_COALESCE, pubkey::Pubkey,
        signature::Keypair, signer::Signer,
    },
    solana_streamer::{
        nonblocking::testing_utilities::check_multiple_streams,
        quic::{
            DEFAULT_MAX_CONNECTIONS_PER_IPADDR_PER_MINUTE, DEFAULT_MAX_STAKED_CONNECTIONS,
            DEFAULT_MAX_STREAMS_PER_MS, DEFAULT_MAX_UNSTAKED_CONNECTIONS,
        },
        socket::SocketAddrSpace,
        streamer::StakedNodes,
    },
    solana_vortexor::{
        cli::{DEFAULT_MAX_QUIC_CONNECTIONS_PER_PEER, DEFAULT_NUM_QUIC_ENDPOINTS},
        rpc_load_balancer,
        stake_updater::StakeUpdater,
        vortexor::Vortexor,
    },
    std::{
        collections::HashMap,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, RwLock,
        },
    },
    url::Url,
};

#[tokio::test(flavor = "multi_thread")]
async fn test_vortexor() {
    solana_logger::setup();

    let bind_address = solana_net_utils::parse_host("127.0.0.1").expect("invalid bind_address");
    let keypair = Keypair::new();
    let exit = Arc::new(AtomicBool::new(false));

    let (tpu_sender, tpu_receiver) = unbounded();
    let (tpu_fwd_sender, tpu_fwd_receiver) = unbounded();
    let tpu_sockets = Vortexor::create_tpu_sockets(
        bind_address,
        VALIDATOR_PORT_RANGE,
        DEFAULT_NUM_QUIC_ENDPOINTS,
    );

    let tpu_address = tpu_sockets.tpu_quic[0].local_addr().unwrap();
    let tpu_fwd_address = tpu_sockets.tpu_quic_fwd[0].local_addr().unwrap();

    let stakes = HashMap::from([(keypair.pubkey(), 10000)]);
    let staked_nodes = Arc::new(RwLock::new(StakedNodes::new(
        Arc::new(stakes),
        HashMap::<Pubkey, u64>::default(), // overrides
    )));

    let vortexor = Vortexor::create_vortexor(
        tpu_sockets,
        staked_nodes,
        tpu_sender,
        tpu_fwd_sender,
        DEFAULT_MAX_QUIC_CONNECTIONS_PER_PEER,
        DEFAULT_MAX_STAKED_CONNECTIONS,
        DEFAULT_MAX_UNSTAKED_CONNECTIONS,
        DEFAULT_MAX_STAKED_CONNECTIONS.saturating_add(DEFAULT_MAX_UNSTAKED_CONNECTIONS), // max_fwd_staked_connections
        0, // max_fwd_unstaked_connections
        DEFAULT_MAX_STREAMS_PER_MS,
        DEFAULT_MAX_CONNECTIONS_PER_IPADDR_PER_MINUTE,
        DEFAULT_TPU_COALESCE,
        &keypair,
        exit.clone(),
    );

    check_multiple_streams(tpu_receiver, tpu_address, Some(&keypair)).await;
    check_multiple_streams(tpu_fwd_receiver, tpu_fwd_address, Some(&keypair)).await;

    exit.store(true, Ordering::Relaxed);
    vortexor.join().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_stake_update() {
    solana_logger::setup();

    let default_node_stake = 10 * LAMPORTS_PER_SOL; // Define a default value for node stake
    let mint_lamports = 100 * LAMPORTS_PER_SOL;
    let mut config = ClusterConfig::new_with_equal_stakes(3, mint_lamports, default_node_stake);

    let mut cluster = LocalCluster::new(&mut config, SocketAddrSpace::Unspecified);
    assert_eq!(cluster.validators.len(), 3);

    let pubkey = cluster.entry_point_info.pubkey();
    let validator = &cluster.validators[pubkey];
    let rpc_addr = validator.info.contact_info.rpc().unwrap();
    let rpc_pubsub_addr = validator.info.contact_info.rpc_pubsub().unwrap();
    let rpc_url = Url::parse(format!("http://{}", rpc_addr.to_string()).as_str()).unwrap();
    let ws_url = Url::parse(format!("ws://{}", rpc_pubsub_addr.to_string()).as_str()).unwrap();
    let exit = Arc::new(AtomicBool::new(false));

    let (rpc_load_balancer, slot_receiver) =
        rpc_load_balancer::RpcLoadBalancer::new(&vec![(rpc_url, ws_url)], &exit);

    // receive 2 slots
    let mut i = 0;
    while i < 2 {
        let slot = slot_receiver.recv().unwrap();
        i += 1;
        info!("Received slot: {}", slot);
    }

    let rpc_load_balancer = Arc::new(rpc_load_balancer);
    // Now create a stake updater service
    let shared_staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));
    let staked_nodes_updater_service = StakeUpdater::new(
        exit.clone(),
        rpc_load_balancer.clone(),
        shared_staked_nodes.clone(),
    );

    loop {
        let stakes = shared_staked_nodes.read().unwrap();
        if let Some(stake) = stakes.get_node_stake(pubkey) {
            info!("Stake for {}: {}", pubkey, stake);
            let total_stake = stakes.total_stake();
            info!("total_stake: {}", total_stake);
            break;
        }
        info!("Waiting for stake map to be populated for {pubkey:?}...");
        drop(stakes); // Drop the read lock before sleeping
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    exit.store(true, Ordering::Relaxed);
    staked_nodes_updater_service.join().unwrap();
    cluster.exit();
    info!("Cluster exited successfully");
}
