use {
    crossbeam_channel::Receiver as CrossbeamReceiver,
    solana_cli_config::ConfigInput,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_sdk::{
        commitment_config::CommitmentConfig,
        pubkey::Pubkey,
        signer::{keypair::Keypair, Signer},
    },
    solana_streamer::{
        nonblocking::testing_utilities::{
            make_client_endpoint, setup_quic_server, SpawnTestServerResult, TestServerConfig,
        },
        packet::PacketBatch,
        streamer::StakedNodes,
    },
    solana_tpu_client_next::{
        leader_updater::create_leader_updater, transaction_batch::TransactionBatch,
        ConnectionWorkersScheduler, ConnectionWorkersSchedulerError, SendTransactionStats,
        SendTransactionStatsPerAddr,
    },
    std::{
        collections::HashMap,
        net::{IpAddr, Ipv6Addr, SocketAddr},
        str::FromStr,
        sync::{atomic::Ordering, Arc},
        time::Duration,
    },
    tokio::{
        sync::mpsc::{channel, Receiver, Sender},
        task::JoinHandle,
        time::{sleep, Instant},
    },
};

async fn setup_connection_worker_scheduler(
    tpu_address: SocketAddr,
    transaction_receiver: Receiver<TransactionBatch>,
    validator_identity: Option<Keypair>,
) -> JoinHandle<Result<SendTransactionStatsPerAddr, ConnectionWorkersSchedulerError>> {
    let json_rpc_url = "http://127.0.0.1:8899";
    let (_, websocket_url) = ConfigInput::compute_websocket_url_setting("", "", json_rpc_url, "");

    let rpc_client = Arc::new(RpcClient::new_with_commitment(
        json_rpc_url.to_string(),
        CommitmentConfig::confirmed(),
    ));

    // Setup sending txs
    let leader_updater =
        create_leader_updater(rpc_client, websocket_url, 1, Some(tpu_address)).await;
    assert!(leader_updater.is_ok());

    let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    tokio::spawn(async move {
        ConnectionWorkersScheduler::run(
            bind,
            validator_identity,
            leader_updater.unwrap(),
            transaction_receiver,
            1,
        )
        .await
    })
}

async fn join_scheduler(
    scheduler_handle: JoinHandle<
        Result<SendTransactionStatsPerAddr, ConnectionWorkersSchedulerError>,
    >,
) -> SendTransactionStats {
    let stats_per_ip = scheduler_handle
        .await
        .unwrap()
        .expect("Scheduler should stop successfully.");
    stats_per_ip
        .get(&IpAddr::from_str("127.0.0.1").unwrap())
        .expect("setup_connection_worker_scheduler() connected to a leader at 127.0.0.1")
        .clone()
}

// The unstaked connection rate is 100tps.
const SEND_EACH: Duration = Duration::from_millis(10);
// Specify the pessimistic time to finish generation and result checks.
const TEST_MAX_TIME: Duration = Duration::from_millis(2000);

async fn spawn_generator(
    transaction_sender: Sender<TransactionBatch>,
    tx_size: usize,
    num_txs: usize,
    send_each_interval: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        for i in 0..num_txs {
            let wire_txs_batch = vec![vec![i as u8; tx_size]; 1];
            let res = transaction_sender
                .send(TransactionBatch::new(wire_txs_batch))
                .await;
            assert_eq!(res, Ok(()));
            // to avoid throttling sleep the duration of the throttling period
            sleep(send_each_interval).await;
        }
        // We need to wait until all the packets have landed. Since we send here
        // a few bytes, all the packets will end up in receive window. Hence,
        // sending will be fast but it doesn't mean that they have been
        // delivered. If we drop sender too early, the connection will be closed
        // and all the pending packets will be lost. Since we already slept for
        // send_each_interval * num_txs, sleep the rest.
        sleep(TEST_MAX_TIME.saturating_sub(send_each_interval.saturating_mul(num_txs as u32)))
            .await;
    })
}

#[tokio::test]
async fn test_basic_transactions_sending() {
    let SpawnTestServerResult {
        join_handle: server_handle,
        exit,
        receiver,
        server_address,
        stats: _stats,
    } = setup_quic_server(None, TestServerConfig::default());
    let (transaction_sender, transaction_receiver) = channel(1);
    let scheduler_handle =
        setup_connection_worker_scheduler(server_address, transaction_receiver, None).await;

    // Setup sending txs
    let tx_size = 1;
    let expected_num_txs: usize = 100;
    let generator_handle =
        spawn_generator(transaction_sender, tx_size, expected_num_txs, SEND_EACH).await;

    // Check results
    let mut received_data = Vec::with_capacity(expected_num_txs);
    let now = Instant::now();
    let mut actual_num_packets = 0;
    while actual_num_packets < expected_num_txs && now.elapsed() < TEST_MAX_TIME {
        if let Ok(packets) = receiver.try_recv() {
            actual_num_packets += packets.len();
            for p in packets.iter() {
                let packet_id = p.data(0).expect("Data should not be lost by server.");
                received_data.push(*packet_id);
                assert_eq!(p.meta().size, tx_size);
            }
        } else {
            sleep(Duration::from_millis(10)).await;
        }
    }
    // check that we received what we have sent.
    assert_eq!(actual_num_packets, expected_num_txs);
    received_data.sort_unstable();
    for i in 1..received_data.len() {
        assert_eq!(received_data[i - 1] + 1, received_data[i]);
    }

    // Stop sending
    generator_handle.await.unwrap();
    let localhost_stats = join_scheduler(scheduler_handle).await;
    assert_eq!(
        localhost_stats,
        SendTransactionStats {
            successfully_sent: expected_num_txs as u64,
            ..Default::default()
        }
    );

    // Stop server
    exit.store(true, Ordering::Relaxed);
    server_handle.await.unwrap();
}

async fn count_server_received_packets(
    receiver: CrossbeamReceiver<PacketBatch>,
    expected_tx_size: usize,
) -> usize {
    let now = Instant::now();
    let mut num_packets_received: usize = 0;
    while now.elapsed() < TEST_MAX_TIME {
        if let Ok(packets) = receiver.try_recv() {
            num_packets_received = num_packets_received.saturating_add(packets.len());
            for p in packets.iter() {
                assert_eq!(p.meta().size, expected_tx_size);
            }
        } else {
            sleep(Duration::from_millis(100)).await;
        }
    }
    num_packets_received
}

// Check that client can create connection even if the first several attempts
// were unsuccessful. To prevent server from accepting a new connection, we use
// the following observation: since max_connections_per_peer == 1 (<
// max_unstaked_connections == 500), if we create a first connection and later
// try another one, the second connection will be immediately closed. Since
// client is not retrying sending failed transactions, this leads to the packets
// loss: the connection has been created and closed when we already have sent
// the data.
#[tokio::test]
async fn test_connection_denied_until_allowed() {
    let SpawnTestServerResult {
        join_handle: server_handle,
        exit,
        receiver,
        server_address,
        stats: _stats,
    } = setup_quic_server(None, TestServerConfig::default());

    let connection = make_client_endpoint(&server_address, None).await;

    let (transaction_sender, transaction_receiver) = channel(1);
    let scheduler_handle =
        setup_connection_worker_scheduler(server_address, transaction_receiver, None).await;

    // Setup sending txs
    let tx_size = 1;
    let expected_num_txs: usize = 10;
    let generator_handle = spawn_generator(
        transaction_sender,
        tx_size,
        expected_num_txs,
        Duration::from_millis(100),
    )
    .await;

    sleep(Duration::from_millis(1000)).await;
    drop(connection);

    // Check results
    let actual_num_packets = count_server_received_packets(receiver, tx_size).await;
    assert!(actual_num_packets > 0 && actual_num_packets < expected_num_txs);

    // Stop sending
    generator_handle.await.unwrap();
    let localhost_stats = join_scheduler(scheduler_handle).await;
    assert_eq!(
        localhost_stats.write_error_connection_lost
            + localhost_stats.connection_error_application_closed,
        1
    );

    // Exit server
    exit.store(true, Ordering::Relaxed);
    server_handle.await.unwrap();
}

// Check that if the client connection has been pruned, client manages to
// reestablish it. Pruning will lead to 1 packet loss, because when we send the
// next packet we will reestablish connection.
#[tokio::test]
async fn test_connection_pruned_and_reopened() {
    let SpawnTestServerResult {
        join_handle: server_handle,
        exit,
        receiver,
        server_address,
        stats: _stats,
    } = setup_quic_server(
        None,
        TestServerConfig {
            max_connections_per_peer: 100,
            max_unstaked_connections: 1,
            ..Default::default()
        },
    );

    let (transaction_sender, transaction_receiver) = channel(1);
    let scheduler_handle =
        setup_connection_worker_scheduler(server_address, transaction_receiver, None).await;

    // Setup sending txs
    let tx_size = 1;
    let expected_num_txs: usize = 16;
    let generator_handle = spawn_generator(
        transaction_sender,
        tx_size,
        expected_num_txs,
        Duration::from_millis(100),
    )
    .await;

    sleep(Duration::from_millis(400)).await;
    let _connection_to_prune_client = make_client_endpoint(&server_address, None).await;

    // Check results
    let actual_num_packets = count_server_received_packets(receiver, tx_size).await;
    assert!(actual_num_packets < expected_num_txs);

    // Stop sending
    generator_handle.await.unwrap();
    let localhost_stats = join_scheduler(scheduler_handle).await;
    // in case of pruning, server closes the connection with code 1 and error
    // message b"dropped". This might lead to connection error
    // (ApplicationClosed::ApplicationClose) or to stream error
    // (ConnectionLost::ApplicationClosed::ApplicationClose).
    assert_eq!(
        localhost_stats.connection_error_application_closed
            + localhost_stats.write_error_connection_lost,
        1,
    );

    // Exit server
    exit.store(true, Ordering::Relaxed);
    server_handle.await.unwrap();
}

/// Check that client creates staked connection. To do that prohibit unstaked
/// connection and verify that all the txs has been received.
#[tokio::test]
async fn test_staked_connection() {
    let validator_identity = Keypair::new();
    let stakes = HashMap::from([(validator_identity.pubkey(), 100_000)]);
    let staked_nodes = StakedNodes::new(Arc::new(stakes), HashMap::<Pubkey, u64>::default());

    let SpawnTestServerResult {
        join_handle: server_handle,
        exit,
        receiver,
        server_address,
        stats: _stats,
    } = setup_quic_server(
        Some(staked_nodes),
        TestServerConfig {
            // Must use at least the number of endpoints (10) because
            // `max_staked_connections` and `max_unstaked_connections` are
            // cumulative for all the endpoints.
            max_staked_connections: 10,
            max_unstaked_connections: 0,
            ..Default::default()
        },
    );

    let (transaction_sender, transaction_receiver) = channel(1);
    let scheduler_handle = setup_connection_worker_scheduler(
        server_address,
        transaction_receiver,
        Some(validator_identity),
    )
    .await;

    // Setup sending txs
    let tx_size = 1;
    let expected_num_txs: usize = 10;
    let generator_handle = spawn_generator(
        transaction_sender,
        tx_size,
        expected_num_txs,
        Duration::from_millis(100),
    )
    .await;

    // Check results
    let actual_num_packets = count_server_received_packets(receiver, tx_size).await;
    assert_eq!(actual_num_packets, expected_num_txs);

    // Stop sending
    generator_handle.await.unwrap();
    let localhost_stats = join_scheduler(scheduler_handle).await;
    assert_eq!(
        localhost_stats,
        SendTransactionStats {
            successfully_sent: expected_num_txs as u64,
            ..Default::default()
        }
    );

    // Exit server
    exit.store(true, Ordering::Relaxed);
    server_handle.await.unwrap();
}

// Check that if client sends transactions at a reasonably high rate that is
// higher than what the server accepts, nevertheless all the transactions are
// delivered and there are no errors on the client side.
#[tokio::test]
async fn test_connection_throttling() {
    let SpawnTestServerResult {
        join_handle: server_handle,
        exit,
        receiver,
        server_address,
        stats: _stats,
    } = setup_quic_server(None, TestServerConfig::default());

    let (transaction_sender, transaction_receiver) = channel(1);
    let scheduler_handle =
        setup_connection_worker_scheduler(server_address, transaction_receiver, None).await;

    // Setup sending txs
    let tx_size = 1;
    let expected_num_txs: usize = 100;
    let generator_handle = spawn_generator(
        transaction_sender,
        tx_size,
        expected_num_txs,
        // send x10 more than the the throttling interval (10ms) allows
        Duration::from_millis(1),
    )
    .await;

    // Check results
    let actual_num_packets = count_server_received_packets(receiver, tx_size).await;
    assert_eq!(actual_num_packets, expected_num_txs);

    // Stop sending
    let localhost_stats = join_scheduler(scheduler_handle).await;
    assert_eq!(
        localhost_stats,
        SendTransactionStats {
            successfully_sent: expected_num_txs as u64,
            ..Default::default()
        }
    );

    // Exit server
    assert!(generator_handle.await.is_ok());
    exit.store(true, Ordering::Relaxed);
    server_handle.await.unwrap();
}

// Check that when the host cannot be reached, the client exits gracefully.
#[tokio::test]
async fn test_no_host() {
    // A "black hole" address for the TPU.
    let tpu_address = SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, 1)), 49151);

    let (transaction_sender, transaction_receiver) = channel(1);
    let scheduler_handle =
        setup_connection_worker_scheduler(tpu_address, transaction_receiver, None).await;

    // Setup sending txs
    let tx_size = 1;
    let expected_num_txs: usize = 10;
    let generator_handle: JoinHandle<()> = tokio::spawn(async move {
        for i in 0..expected_num_txs {
            let wire_txs_batch = vec![vec![i as u8; tx_size]; 1];
            let res = transaction_sender
                .send(TransactionBatch::new(wire_txs_batch.clone()))
                .await;
            if res.is_err() {
                // Scheduler consumes transactions from the channel and tries to
                // create connection and send them. After several attempts, it
                // will fail and will decide to drop receiver on it's end. This
                // will trigger Err on send side.
                break;
            }
            // to avoid throttling sleep the duration of the throttling period
            sleep(Duration::from_millis(100)).await;
        }
    });

    // Stop sending
    generator_handle.await.unwrap();
    // While attempting to establish a connection with a nonexistent host, we
    // fill the worker's channel. Transactions from this channel will never be
    // sent and will eventually be dropped without increasing the
    // SendTransactionStats counters.
    let result = scheduler_handle.await.unwrap();
    assert_eq!(result.unwrap(), HashMap::default());
}

// Check that when the client is rate-limited by server, we update counters
// accordingly. To implement it we:
// * set the connection limit per minute to 1
// * create a dummy connection to reach the limit and immediately close it
// * set up client which will try to create a new connection which it will be
// rate-limited. This test doesn't check what happens when the rate-limiting
// period ends because it too long for test (1min).
#[tokio::test]
async fn test_rate_limiting() {
    let SpawnTestServerResult {
        join_handle: server_handle,
        exit,
        receiver,
        server_address,
        stats: _stats,
    } = setup_quic_server(
        None,
        TestServerConfig {
            max_connections_per_peer: 100,
            max_connections_per_ipaddr_per_minute: 1,
            ..Default::default()
        },
    );

    let connection_to_reach_limit = make_client_endpoint(&server_address, None).await;
    drop(connection_to_reach_limit);
    // give more time to endpoint to avoid failing test when several tests are
    // running in parallel.
    sleep(Duration::from_millis(100)).await;

    let (transaction_sender, transaction_receiver) = channel(1);
    let scheduler_handle =
        setup_connection_worker_scheduler(server_address, transaction_receiver, None).await;

    // Setup sending txs
    let tx_size = 1;
    let expected_num_txs: usize = 16;
    let generator_handle = spawn_generator(
        transaction_sender,
        tx_size,
        expected_num_txs,
        Duration::from_millis(100),
    )
    .await;

    // Stop sending
    let localhost_stats = join_scheduler(scheduler_handle).await;
    assert_eq!(
        localhost_stats.write_error_connection_lost
            + localhost_stats.connection_error_application_closed,
        expected_num_txs as u64
    );
    assert!(generator_handle.await.is_ok());

    // Check results
    let actual_num_packets = count_server_received_packets(receiver, tx_size).await;
    assert_eq!(actual_num_packets, 0);

    // Exit server
    exit.store(true, Ordering::Relaxed);
    server_handle.await.unwrap();
}

// The same as test_rate_limiting but here we wait for 1 min to check that the
// connection has been established.
#[tokio::test]
// Ignore because it takes more than 1 minute
#[ignore]
async fn test_rate_limiting_establish_connection() {
    let SpawnTestServerResult {
        join_handle: server_handle,
        exit,
        receiver,
        server_address,
        stats: _stats,
    } = setup_quic_server(
        None,
        TestServerConfig {
            max_connections_per_peer: 100,
            max_connections_per_ipaddr_per_minute: 1,
            ..Default::default()
        },
    );

    let connection_to_reach_limit = make_client_endpoint(&server_address, None).await;
    drop(connection_to_reach_limit);

    let (transaction_sender, transaction_receiver) = channel(1);
    let scheduler_handle =
        setup_connection_worker_scheduler(server_address, transaction_receiver, None).await;

    // Setup sending txs
    let tx_size = 1;
    let num_txs: usize = 65;
    let generator_handle = spawn_generator(
        transaction_sender,
        tx_size,
        num_txs,
        Duration::from_millis(1000),
    )
    .await;

    sleep(Duration::from_secs(60)).await;

    // Stop sending
    let localhost_stats = join_scheduler(scheduler_handle).await;
    assert!(
        localhost_stats.write_error_connection_lost
            + localhost_stats.connection_error_application_closed
            > 0
    );
    assert!(generator_handle.await.is_ok());

    // Check results
    let actual_num_packets = count_server_received_packets(receiver, tx_size).await;
    assert!(actual_num_packets > 0);

    // Exit server
    exit.store(true, Ordering::Relaxed);
    server_handle.await.unwrap();
}
