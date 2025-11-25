use {
    crossbeam_channel::unbounded,
    log::info,
    solana_keypair::Keypair,
    solana_local_cluster::{
        cluster::ClusterValidatorInfo,
        local_cluster::{ClusterConfig, LocalCluster},
    },
    solana_native_token::LAMPORTS_PER_SOL,
    solana_net_utils::{SocketAddrSpace, VALIDATOR_PORT_RANGE},
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    solana_streamer::{
        nonblocking::testing_utilities::check_multiple_streams,
        quic::{
            DEFAULT_MAX_CONNECTIONS_PER_IPADDR_PER_MINUTE, DEFAULT_MAX_STAKED_CONNECTIONS,
            DEFAULT_MAX_STREAMS_PER_MS, DEFAULT_MAX_UNSTAKED_CONNECTIONS,
        },
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
        time::Duration,
    },
    tokio_util::sync::CancellationToken,
    url::Url,
};

#[tokio::test(flavor = "multi_thread")]
async fn test_vortexor() {
    agave_logger::setup();

    let bind_address = solana_net_utils::parse_host("127.0.0.1").expect("invalid bind_address");
    let keypair = Keypair::new();
    let cancel = CancellationToken::new();

    let (tpu_sender, tpu_receiver) = unbounded();
    let (tpu_fwd_sender, tpu_fwd_receiver) = unbounded();
    let tpu_sockets = Vortexor::create_tpu_sockets(
        bind_address,
        VALIDATOR_PORT_RANGE,
        None, // tpu_address
        None, // tpu_forward_address
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
        &keypair,
        cancel.clone(),
    );

    check_multiple_streams(tpu_receiver, tpu_address, Some(&keypair)).await;
    check_multiple_streams(tpu_fwd_receiver, tpu_fwd_address, Some(&keypair)).await;

    cancel.cancel();
    vortexor.join().unwrap();
}

fn get_server_urls(validator: &ClusterValidatorInfo) -> (Url, Url) {
    let rpc_addr = validator.info.contact_info.rpc().unwrap();
    let rpc_pubsub_addr = validator.info.contact_info.rpc_pubsub().unwrap();
    let rpc_url = Url::parse(format!("http://{rpc_addr}").as_str()).unwrap();
    let ws_url = Url::parse(format!("ws://{rpc_pubsub_addr}").as_str()).unwrap();
    (rpc_url, ws_url)
}

#[test]
fn test_stake_update() {
    agave_logger::setup();

    // Create a local cluster with 3 validators
    let default_node_stake = 10 * LAMPORTS_PER_SOL; // Define a default value for node stake
    let mint_lamports = 100 * LAMPORTS_PER_SOL;
    let mut config = ClusterConfig::new_with_equal_stakes(3, mint_lamports, default_node_stake);

    let mut cluster = LocalCluster::new(&mut config, SocketAddrSpace::Unspecified);
    info!(
        "Cluster created with {} validators",
        cluster.validators.len()
    );
    assert_eq!(cluster.validators.len(), 3);

    let pubkey = cluster.entry_point_info.pubkey();
    let validator = &cluster.validators[pubkey];

    let mut servers = vec![get_server_urls(validator)];
    // add one more RPC subscription to another validator
    for validator in cluster.validators.values() {
        if validator.info.keypair.pubkey() != *pubkey {
            servers.push(get_server_urls(validator));
            break;
        }
    }
    let exit = Arc::new(AtomicBool::new(false));

    let (rpc_load_balancer, slot_receiver) =
        rpc_load_balancer::RpcLoadBalancer::new(&servers, &exit);

    // receive 2 slot updates
    let mut i = 0;
    let slot_receive_timeout = Duration::from_secs(5); // conservative timeout to ensure stable test
    while i < 2 {
        let slot = slot_receiver
            .recv_timeout(slot_receive_timeout)
            .unwrap_or_else(|_| panic!("Expected a slot within {slot_receive_timeout:?}"));
        i += 1;
        info!("Received a slot update: {slot}");
    }

    let rpc_load_balancer = Arc::new(rpc_load_balancer);
    // Now create a stake updater service
    let shared_staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));
    let staked_nodes_updater_service = StakeUpdater::new(
        exit.clone(),
        rpc_load_balancer.clone(),
        shared_staked_nodes.clone(),
        Duration::from_millis(100), // short sleep to speed up the test for service exit
    );

    // Waiting for the stake map to be populated by the stake updater service
    let start_of_stake_updater = std::time::Instant::now();
    let stake_updater_timeout = Duration::from_secs(10); // conservative timeout to ensure stable test
    loop {
        let stakes = shared_staked_nodes.read().unwrap();
        if let Some(stake) = stakes.get_node_stake(pubkey) {
            info!("Stake for {pubkey}: {stake}");
            assert_eq!(stake, default_node_stake);
            let total_stake = stakes.total_stake();
            info!("total_stake: {total_stake}");
            assert!(total_stake >= default_node_stake);
            break;
        }
        info!("Waiting for stake map to be populated for {pubkey:?}...");
        drop(stakes); // Drop the read lock before sleeping so the writer side can proceed
        std::thread::sleep(std::time::Duration::from_millis(100));
        if start_of_stake_updater.elapsed() > stake_updater_timeout {
            panic!("Timeout waiting for stake map to be populated");
        }
    }
    info!("Test done, exiting stake updater service");
    exit.store(true, Ordering::Relaxed);
    staked_nodes_updater_service.join().unwrap();
    info!("Stake updater service exited successfully, shutting down cluster");
    cluster.exit();
    info!("Cluster exited successfully");
}
