use {
    crossbeam_channel::Receiver as CrossbeamReceiver,
    futures::future::BoxFuture,
    solana_keypair::Keypair,
    solana_net_utils::sockets::{
        bind_to, localhost_port_range_for_tests, unique_port_range_for_tests,
    },
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    solana_streamer::{
        nonblocking::{
            swqos::SwQosConfig,
            testing_utilities::{make_client_endpoint, setup_quic_server, SpawnTestServerResult},
        },
        packet::PacketBatch,
        quic::QuicStreamerConfig,
        streamer::StakedNodes,
    },
    solana_tpu_client_next::{
        connection_workers_scheduler::{
            BindTarget, ConnectionWorkersSchedulerConfig, Fanout, StakeIdentity,
        },
        leader_updater::create_pinned_leader_updater,
        send_transaction_stats::SendTransactionStatsNonAtomic,
        transaction_batch::TransactionBatch,
        ClientBuilder, ConnectionWorkersScheduler, ConnectionWorkersSchedulerError,
        SendTransactionStats,
    },
    std::{
        collections::HashMap,
        net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
        num::Saturating,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
        time::Duration,
    },
    tokio::{
        sync::{
            mpsc::{channel, Receiver},
            oneshot, watch,
        },
        task::JoinHandle,
        time::{interval, sleep, Instant},
    },
    tokio_util::sync::CancellationToken,
};

fn test_config(stake_identity: Option<Keypair>) -> ConnectionWorkersSchedulerConfig {
    let address = SocketAddr::new(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        unique_port_range_for_tests(1).start,
    );
    ConnectionWorkersSchedulerConfig {
        bind: BindTarget::Address(address),
        stake_identity: stake_identity.map(|identity| StakeIdentity::new(&identity)),
        num_connections: 1,
        skip_check_transaction_age: false,
        // At the moment we have only one strategy to send transactions: we try
        // to put to worker channel transaction batch and in case of failure
        // just drop it. This requires to use large channels here. In the
        // future, we are planning to add an option to send with backpressure at
        // the speed of fastest leader.
        worker_channel_size: 100,
        max_reconnect_attempts: 4,
        leaders_fanout: Fanout {
            send: 1,
            connect: 1,
        },
    }
}

async fn setup_connection_worker_scheduler(
    tpu_address: SocketAddr,
    transaction_receiver: Receiver<TransactionBatch>,
    stake_identity: Option<Keypair>,
) -> (
    JoinHandle<Result<Arc<SendTransactionStats>, ConnectionWorkersSchedulerError>>,
    watch::Sender<Option<StakeIdentity>>,
    CancellationToken,
) {
    let config = test_config(stake_identity);

    // Setup sending txs
    let cancel = CancellationToken::new();
    let leader_updater = create_pinned_leader_updater(tpu_address);

    let (update_identity_sender, update_identity_receiver) = watch::channel(None);
    let scheduler = ConnectionWorkersScheduler::new(
        leader_updater,
        transaction_receiver,
        update_identity_receiver,
        cancel.clone(),
    );
    let scheduler = tokio::spawn(scheduler.run(config));

    (scheduler, update_identity_sender, cancel)
}

async fn join_scheduler(
    scheduler_handle: JoinHandle<
        Result<Arc<SendTransactionStats>, ConnectionWorkersSchedulerError>,
    >,
) -> SendTransactionStatsNonAtomic {
    let scheduler_stats = scheduler_handle
        .await
        .unwrap()
        .expect("Scheduler should stop successfully.");
    scheduler_stats.read_and_reset()
}

// Specify the pessimistic time to finish generation and result checks.
const TEST_MAX_TIME: Duration = Duration::from_millis(2500);

struct SpawnTxGenerator {
    tx_receiver: Receiver<TransactionBatch>,
    tx_sender_shutdown: BoxFuture<'static, ()>,
    tx_sender_done: oneshot::Receiver<()>,
}

/// Generates `num_tx_batches` batches of transactions, each holding a single transaction of
/// `tx_size` bytes.
///
/// It will not close the returned `tx_receiver` until `tx_sender_shutdown` is invoked.  Otherwise,
/// there is a race condition, that exists between the last transaction being scheduled for delivery
/// and the server connection being closed.
fn spawn_tx_sender(
    tx_size: usize,
    num_tx_batches: usize,
    time_per_tx: Duration,
) -> SpawnTxGenerator {
    let num_tx_batches: u32 = num_tx_batches
        .try_into()
        .expect("`num_tx_batches` fits into u32 for all the tests");
    let (tx_sender, tx_receiver) = channel(1);
    let cancel = CancellationToken::new();
    let (done_sender, tx_sender_done) = oneshot::channel();

    let sender = tokio::spawn({
        let start = Instant::now();

        let tx_sender = tx_sender.clone();

        let main_loop = async move {
            for i in 0..num_tx_batches {
                let txs = vec![vec![i as u8; tx_size]; 1];
                tx_sender
                    .send(TransactionBatch::new(txs))
                    .await
                    .expect("Receiver should not close their side");

                // Pretend the client runs at the specified TPS.
                let sleep_time = time_per_tx
                    .saturating_mul(i)
                    .saturating_sub(start.elapsed());
                if !sleep_time.is_zero() {
                    sleep(sleep_time).await;
                }
            }

            // It is OK if the receiver has disconnected.
            let _ = done_sender.send(());
        };

        let cancel = cancel.clone();
        async move {
            tokio::select! {
                () = main_loop => (),
                () = cancel.cancelled() => (),
            }
        }
    });

    let tx_sender_shutdown = Box::pin(async move {
        cancel.cancel();
        // This makes sure that the sender exists up until the shutdown is invoked.
        drop(tx_sender);

        sender.await.unwrap();
    });

    SpawnTxGenerator {
        tx_receiver,
        tx_sender_shutdown,
        tx_sender_done,
    }
}

#[tokio::test]
async fn test_basic_transactions_sending() {
    let SpawnTestServerResult {
        join_handle: server_handle,
        receiver,
        server_address,
        stats: _stats,
        cancel,
    } = setup_quic_server(
        None,
        QuicStreamerConfig::default_for_tests(),
        SwQosConfig::default(),
    );

    // Setup sending txs
    let tx_size = 1;
    let expected_num_txs: usize = 100;
    // Pretend that we are running at ~100 TPS.
    let SpawnTxGenerator {
        tx_receiver,
        tx_sender_shutdown,
        ..
    } = spawn_tx_sender(tx_size, expected_num_txs, Duration::from_millis(10));

    let (scheduler_handle, update_identity_sender, _scheduler_cancel) =
        setup_connection_worker_scheduler(server_address, tx_receiver, None).await;
    // dropping sender will not lead to stop the scheduler.
    drop(update_identity_sender);

    // Check results
    let mut received_data = Vec::with_capacity(expected_num_txs);
    let now = Instant::now();
    let mut actual_num_packets = 0;
    while actual_num_packets < expected_num_txs {
        {
            let elapsed = now.elapsed();
            assert!(
                elapsed < TEST_MAX_TIME,
                "Failed to send {expected_num_txs} transaction in {elapsed:?}. Only sent \
                 {actual_num_packets}",
            );
        }

        let Ok(packets) = receiver.try_recv() else {
            sleep(Duration::from_millis(10)).await;
            continue;
        };

        actual_num_packets += packets.len();
        for p in packets.iter() {
            let packet_id = p.data(0).expect("Data should not be lost by server.");
            received_data.push(*packet_id);
            assert_eq!(p.meta().size, 1);
        }
    }

    received_data.sort_unstable();
    for i in 1..received_data.len() {
        assert_eq!(received_data[i - 1] + 1, received_data[i]);
    }

    // Stop sending
    tx_sender_shutdown.await;
    let stats = join_scheduler(scheduler_handle).await;
    assert_eq!(stats.successfully_sent, expected_num_txs as u64,);

    // Stop server
    cancel.cancel();
    server_handle.await.unwrap();
}

async fn count_received_packets_for(
    receiver: CrossbeamReceiver<PacketBatch>,
    expected_tx_size: usize,
    receive_duration: Duration,
) -> usize {
    let now = Instant::now();
    let mut num_packets_received = Saturating(0usize);

    while now.elapsed() < receive_duration {
        if let Ok(packets) = receiver.try_recv() {
            num_packets_received += packets.len();
            for p in packets.iter() {
                assert_eq!(p.meta().size, expected_tx_size);
            }
        } else {
            sleep(Duration::from_millis(100)).await;
        }
    }

    num_packets_received.0
}

// Check that client can create connection even if the first several attempts were unsuccessful.
#[tokio::test]
async fn test_connection_denied_until_allowed() {
    let SpawnTestServerResult {
        join_handle: server_handle,
        receiver,
        server_address,
        stats: _stats,
        cancel,
    } = setup_quic_server(
        None,
        QuicStreamerConfig::default_for_tests(),
        SwQosConfig {
            // To prevent server from accepting a new connection, we
            // set max_connections_per_peer == 1
            max_connections_per_unstaked_peer: 1,
            ..Default::default()
        },
    );

    // If we create a blocking connection and try to create connections to send TXs,
    // the new connections will be immediately closed.
    // Since client is not retrying sending failed transactions, this leads to the packets loss.
    let blocking_connection = make_client_endpoint(&server_address, None).await;

    let time_per_tx = Duration::from_millis(100);
    // Setup sending txs
    let tx_size = 1;
    let num_tx_batches: usize = 30;
    let SpawnTxGenerator {
        tx_receiver,
        tx_sender_shutdown,
        ..
    } = spawn_tx_sender(tx_size, num_tx_batches, time_per_tx);

    let (scheduler_handle, _update_identity_sender, _scheduler_cancel) =
        setup_connection_worker_scheduler(server_address, tx_receiver, None).await;

    // Check results
    let received_num_packets = count_received_packets_for(
        receiver.clone(),
        tx_size,
        time_per_tx * num_tx_batches as u32 / 2,
    )
    .await;
    assert_eq!(
        received_num_packets, 0,
        "Expected to lose all packets, got {received_num_packets} out of {num_tx_batches} sent"
    );
    // drop blocking connection, allow packets to get in now
    drop(blocking_connection);
    let received_num_packets =
        count_received_packets_for(receiver, tx_size, time_per_tx * num_tx_batches as u32 / 2)
            .await;
    assert!(
        received_num_packets > 0,
        "Expected to get some packets, got {received_num_packets} out of {num_tx_batches} sent"
    );
    // Wait for the exchange to finish.
    tx_sender_shutdown.await;
    let stats = join_scheduler(scheduler_handle).await;
    // Expect at least some errors.
    assert!(
        stats.connection_error_application_closed > 0,
        "Expected at least 1 connection error, connection_error_application_closed: {}",
        stats.connection_error_application_closed
    );

    // Exit server
    cancel.cancel();
    server_handle.await.unwrap();
}

// Check that if the client connection has been pruned, client manages to
// reestablish it. With more packets, we can observe the impact of pruning
// even with proactive detection.
#[tokio::test]
async fn test_connection_pruned_and_reopened() {
    let SpawnTestServerResult {
        join_handle: server_handle,
        receiver,
        server_address,
        stats: _stats,
        cancel,
    } = setup_quic_server(
        None,
        QuicStreamerConfig {
            ..QuicStreamerConfig::default_for_tests()
        },
        SwQosConfig {
            max_connections_per_unstaked_peer: 100,
            max_unstaked_connections: 1,
            ..Default::default()
        },
    );

    // Setup sending txs
    let tx_size = 1;
    let expected_num_txs: usize = 48;
    let SpawnTxGenerator {
        tx_receiver,
        tx_sender_shutdown,
        ..
    } = spawn_tx_sender(tx_size, expected_num_txs, Duration::from_millis(100));

    let (scheduler_handle, _update_identity_sender, _scheduler_cancel) =
        setup_connection_worker_scheduler(server_address, tx_receiver, None).await;

    sleep(Duration::from_millis(400)).await;
    let _connection_to_prune_client = make_client_endpoint(&server_address, None).await;

    // Check results
    let actual_num_packets = count_received_packets_for(receiver, tx_size, TEST_MAX_TIME).await;
    assert!(actual_num_packets < expected_num_txs);

    // Wait for the exchange to finish.
    tx_sender_shutdown.await;
    let stats = join_scheduler(scheduler_handle).await;
    // Proactive detection catches pruning immediately, expect multiple retries.
    assert!(
        stats.connection_error_application_closed + stats.write_error_connection_lost >= 1,
        "Expected at least 1 connection error from pruning and retries. Stats: {stats:?}"
    );

    // Exit server
    cancel.cancel();
    server_handle.await.unwrap();
}

/// Check that client creates staked connection. To do that prohibit unstaked
/// connection and verify that all the txs has been received.
#[tokio::test]
async fn test_staked_connection() {
    let stake_identity = Keypair::new();
    let stakes = HashMap::from([(stake_identity.pubkey(), 100_000)]);
    let staked_nodes = StakedNodes::new(Arc::new(stakes), HashMap::<Pubkey, u64>::default());

    let SpawnTestServerResult {
        join_handle: server_handle,
        receiver,
        server_address,
        stats: _stats,
        cancel,
    } = setup_quic_server(
        Some(staked_nodes),
        QuicStreamerConfig {
            ..QuicStreamerConfig::default()
        },
        SwQosConfig {
            // Must use at least the number of endpoints (10) because
            // `max_staked_connections` and `max_unstaked_connections` are
            // cumulative for all the endpoints.
            max_staked_connections: 10,
            max_unstaked_connections: 0,
            ..Default::default()
        },
    );

    // Setup sending txs
    let tx_size = 1;
    let expected_num_txs: usize = 10;
    let SpawnTxGenerator {
        tx_receiver,
        tx_sender_shutdown,
        ..
    } = spawn_tx_sender(tx_size, expected_num_txs, Duration::from_millis(100));

    let (scheduler_handle, _update_certificate_sender, _scheduler_cancel) =
        setup_connection_worker_scheduler(server_address, tx_receiver, Some(stake_identity)).await;

    // Check results
    let actual_num_packets = count_received_packets_for(receiver, tx_size, TEST_MAX_TIME).await;
    assert_eq!(actual_num_packets, expected_num_txs);

    // Wait for the exchange to finish.
    tx_sender_shutdown.await;
    let stats = join_scheduler(scheduler_handle).await;
    assert_eq!(
        stats,
        SendTransactionStatsNonAtomic {
            successfully_sent: expected_num_txs as u64,
            ..Default::default()
        }
    );

    // Exit server
    cancel.cancel();
    server_handle.await.unwrap();
}

// Check that if client sends transactions at a reasonably high rate that is
// higher than what the server accepts, nevertheless all the transactions are
// delivered and there are no errors on the client side.
#[tokio::test]
async fn test_connection_throttling() {
    let SpawnTestServerResult {
        join_handle: server_handle,
        receiver,
        server_address,
        stats: _stats,
        cancel,
    } = setup_quic_server(
        None,
        QuicStreamerConfig::default_for_tests(),
        SwQosConfig::default(),
    );

    // Setup sending txs
    let tx_size = 1;
    let expected_num_txs: usize = 50;
    // Send at 1000 TPS - x10 more than the throttling interval of 10ms used in other tests allows.
    let SpawnTxGenerator {
        tx_receiver,
        tx_sender_shutdown,
        ..
    } = spawn_tx_sender(tx_size, expected_num_txs, Duration::from_millis(1));

    let (scheduler_handle, _update_certificate_sender, _scheduler_cancel) =
        setup_connection_worker_scheduler(server_address, tx_receiver, None).await;

    // Check results
    let actual_num_packets =
        count_received_packets_for(receiver, tx_size, Duration::from_secs(1)).await;
    assert_eq!(actual_num_packets, expected_num_txs);

    // Stop sending
    tx_sender_shutdown.await;
    let stats = join_scheduler(scheduler_handle).await;
    assert_eq!(
        stats,
        SendTransactionStatsNonAtomic {
            successfully_sent: expected_num_txs as u64,
            ..Default::default()
        }
    );

    // Exit server
    cancel.cancel();
    server_handle.await.unwrap();
}

// Check that when the host cannot be reached, the client exits gracefully.
#[tokio::test]
async fn test_no_host() {
    // A "black hole" address for the TPU.
    let server_ip = IpAddr::V6(Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, 1));
    let server_address = SocketAddr::new(server_ip, 49151);

    // Setup sending side.
    let tx_size = 1;
    let max_send_attempts: usize = 10;

    let SpawnTxGenerator {
        tx_receiver,
        tx_sender_shutdown,
        tx_sender_done,
        ..
    } = spawn_tx_sender(tx_size, max_send_attempts, Duration::from_millis(10));

    let (scheduler_handle, _update_certificate_sender, _scheduler_cancel) =
        setup_connection_worker_scheduler(server_address, tx_receiver, None).await;

    // Wait for all the transactions to be sent, and some extra time for the delivery to be
    // attempted.
    tx_sender_done.await.unwrap();
    sleep(Duration::from_millis(100)).await;

    // Wait for the generator to finish.
    tx_sender_shutdown.await;

    // For each transaction, we will check if worker exists and active. In this
    // case, worker will never be active because when failed creating
    // connection, we stop it. So scheduler will `max_send_attempts` try to
    // create worker and fail each time.
    let stats = join_scheduler(scheduler_handle).await;
    assert_eq!(
        stats.connect_error_invalid_remote_address,
        max_send_attempts as u64
    );
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
        receiver,
        server_address,
        stats: _stats,
        cancel,
    } = setup_quic_server(
        None,
        QuicStreamerConfig {
            max_connections_per_ipaddr_per_min: 1,
            ..QuicStreamerConfig::default_for_tests()
        },
        SwQosConfig {
            max_connections_per_unstaked_peer: 100,
            ..Default::default()
        },
    );

    // open a connection to consume the limit
    let connection_to_reach_limit = make_client_endpoint(&server_address, None).await;
    drop(connection_to_reach_limit);

    // Setup sending txs which are full packets in size
    let tx_size = 1024;
    let expected_num_txs: usize = 16;
    let SpawnTxGenerator {
        tx_receiver,
        tx_sender_shutdown,
        ..
    } = spawn_tx_sender(tx_size, expected_num_txs, Duration::from_millis(100));

    let (scheduler_handle, _update_certificate_sender, scheduler_cancel) =
        setup_connection_worker_scheduler(server_address, tx_receiver, None).await;

    let actual_num_packets = count_received_packets_for(receiver, tx_size, TEST_MAX_TIME).await;
    assert_eq!(actual_num_packets, 0);

    // Stop the sender.
    tx_sender_shutdown.await;

    // And the scheduler.
    scheduler_cancel.cancel();
    let stats = join_scheduler(scheduler_handle).await;

    assert!(
        stats
            == SendTransactionStatsNonAtomic {
                connection_error_timed_out: 1,
                ..Default::default()
            }
    );

    // Stop the server.
    cancel.cancel();
    server_handle.await.unwrap();
}

// The same as test_rate_limiting but here we wait for 1 min to check that the
// connection has been established.
#[tokio::test]
// TODO Provide an alternative testing interface for `streamer::nonblocking::quic::spawn_server`
// that would accept throttling at a granularity below 1 minute.
#[ignore = "takes 70s to complete"]
async fn test_rate_limiting_establish_connection() {
    let SpawnTestServerResult {
        join_handle: server_handle,
        receiver,
        server_address,
        stats: _stats,
        cancel,
    } = setup_quic_server(
        None,
        QuicStreamerConfig {
            max_connections_per_ipaddr_per_min: 1,
            ..QuicStreamerConfig::default_for_tests()
        },
        SwQosConfig {
            max_connections_per_unstaked_peer: 100,
            ..Default::default()
        },
    );

    let connection_to_reach_limit = make_client_endpoint(&server_address, None).await;
    drop(connection_to_reach_limit);

    // Setup sending txs
    let tx_size = 1;
    let expected_num_txs: usize = 65;
    let SpawnTxGenerator {
        tx_receiver,
        tx_sender_shutdown,
        ..
    } = spawn_tx_sender(tx_size, expected_num_txs, Duration::from_millis(1000));

    let (scheduler_handle, _update_certificate_sender, scheduler_cancel) =
        setup_connection_worker_scheduler(server_address, tx_receiver, None).await;

    let actual_num_packets =
        count_received_packets_for(receiver, tx_size, Duration::from_secs(70)).await;
    assert!(
        actual_num_packets > 0,
        "As we wait longer than 1 minute, at least one transaction should be delivered. After 1 \
         minute the server is expected to accept our connection. Actual packets delivered: \
         {actual_num_packets}"
    );

    // Stop the sender.
    tx_sender_shutdown.await;

    // And the scheduler.
    scheduler_cancel.cancel();
    let mut stats = join_scheduler(scheduler_handle).await;
    assert!(
        stats.connection_error_timed_out > 0,
        "As the quinn timeout is below 1 minute, a few connections will fail to connect during \
         the 1 minute delay. Actual connection_error_timed_out: {}",
        stats.connection_error_timed_out
    );
    assert!(
        stats.successfully_sent > 0,
        "As we run the test for longer than 1 minute, we expect a connection to be established, \
         and a number of transactions to be delivered.\nActual successfully_sent: {}",
        stats.successfully_sent
    );

    // All the rest of the error counters should be 0.
    stats.connection_error_timed_out = 0;
    stats.successfully_sent = 0;
    assert_eq!(stats, SendTransactionStatsNonAtomic::default());

    // Stop the server.
    cancel.cancel();
    server_handle.await.unwrap();
}

// Check that identity is updated successfully using corresponding channel.
//
// Since the identity update and the transactions are sent concurrently to their channels
// and scheduler selects randomly which channel to handle first, we cannot
// guarantee in this test that the identity has been updated before we start
// sending transactions. Hence, instead of checking that all the transactions
// have been delivered, we check that at least some have been.
#[tokio::test]
async fn test_update_identity() {
    let stake_identity = Keypair::new();
    let stakes = HashMap::from([(stake_identity.pubkey(), 100_000)]);
    let staked_nodes = StakedNodes::new(Arc::new(stakes), HashMap::<Pubkey, u64>::default());

    let SpawnTestServerResult {
        join_handle: server_handle,
        receiver,
        server_address,
        stats: _stats,
        cancel,
    } = setup_quic_server(
        Some(staked_nodes),
        QuicStreamerConfig {
            ..QuicStreamerConfig::default_for_tests()
        },
        SwQosConfig {
            // Must use at least the number of endpoints (10) because
            // `max_staked_connections` and `max_unstaked_connections` are
            // cumulative for all the endpoints.
            max_staked_connections: 10,
            // Deny all unstaked connections.
            max_unstaked_connections: 0,
            ..Default::default()
        },
    );

    // Setup sending txs
    let tx_size = 1;
    let num_txs: usize = 100;
    let SpawnTxGenerator {
        tx_receiver,
        tx_sender_shutdown,
        ..
    } = spawn_tx_sender(tx_size, num_txs, Duration::from_millis(50));

    let (scheduler_handle, update_identity_sender, scheduler_cancel) =
        setup_connection_worker_scheduler(
            server_address,
            tx_receiver,
            // Create scheduler with unstaked identity.
            None,
        )
        .await;
    // Update identity.
    update_identity_sender
        .send(Some(StakeIdentity::new(&stake_identity)))
        .unwrap();

    let actual_num_packets = count_received_packets_for(receiver, tx_size, TEST_MAX_TIME).await;
    assert!(actual_num_packets > 0);

    // Stop the sender.
    tx_sender_shutdown.await;

    // And the scheduler.
    scheduler_cancel.cancel();

    let stats = join_scheduler(scheduler_handle).await;
    assert!(stats.successfully_sent > 0);

    // Exit server
    cancel.cancel();
    server_handle.await.unwrap();
}

// Test that connection close events are detected immediately via
// connection.closed() monitoring, not only when send operations fail.
#[tokio::test]
#[ignore = "Enable after we introduce TaskTracker to streamer."]
async fn test_proactive_connection_close_detection() {
    let SpawnTestServerResult {
        join_handle: server_handle,
        receiver,
        server_address,
        stats: _stats,
        cancel,
    } = setup_quic_server(
        None,
        QuicStreamerConfig {
            ..QuicStreamerConfig::default_for_tests()
        },
        SwQosConfig {
            max_connections_per_unstaked_peer: 1,
            max_unstaked_connections: 1,
            ..Default::default()
        },
    );

    // Setup controlled transaction sending
    let tx_size = 1;
    let (tx_sender, tx_receiver) = channel(10);

    let (scheduler_handle, _update_identity_sender, scheduler_cancel) =
        setup_connection_worker_scheduler(server_address, tx_receiver, None).await;

    // Send first transaction to establish connection
    tx_sender
        .send(TransactionBatch::new(vec![vec![1u8; tx_size]]))
        .await
        .expect("Send first batch");

    // Verify first packet received
    let mut first_packet_received = false;
    let start = Instant::now();
    while !first_packet_received && start.elapsed() < Duration::from_secs(1) {
        if let Ok(packets) = receiver.try_recv() {
            if !packets.is_empty() {
                first_packet_received = true;
            }
        } else {
            sleep(Duration::from_millis(10)).await;
        }
    }
    assert!(first_packet_received, "First packet should be received");

    // Exit server
    cancel.cancel();
    server_handle.await.unwrap();

    tx_sender
        .send(TransactionBatch::new(vec![vec![2u8; tx_size]]))
        .await
        .expect("Send second batch");
    tx_sender
        .send(TransactionBatch::new(vec![vec![3u8; tx_size]]))
        .await
        .expect("Send third batch");

    // Clean up
    scheduler_cancel.cancel();
    let stats = join_scheduler(scheduler_handle).await;

    // Verify proactive close detection
    assert!(
        stats.connection_error_application_closed > 0 || stats.write_error_connection_lost > 0,
        "Should detect connection close proactively. Stats: {stats:?}"
    );
}

#[tokio::test]
async fn test_client_builder() {
    let SpawnTestServerResult {
        join_handle: server_handle,
        receiver,
        server_address,
        stats: _stats,
        cancel,
    } = setup_quic_server(
        None,
        QuicStreamerConfig::default_for_tests(),
        SwQosConfig::default(),
    );

    let _drop_guard = cancel.clone().drop_guard();

    let successfully_sent = Arc::new(AtomicU64::new(0));

    let port_range = localhost_port_range_for_tests();
    let socket = bind_to(IpAddr::V4(Ipv4Addr::LOCALHOST), port_range.0)
        .expect("Should be able to open UdpSocket for tests.");

    let leader_updater = create_pinned_leader_updater(server_address);

    let builder = ClientBuilder::new(leader_updater)
        .cancel_token(cancel.child_token())
        .bind_socket(socket)
        .leader_send_fanout(1)
        .identity(None)
        .max_cache_size(1)
        .worker_channel_size(100)
        .metric_reporter({
            let successfully_sent = successfully_sent.clone();
            |stats: Arc<SendTransactionStats>, cancel: CancellationToken| async move {
                let mut interval = interval(Duration::from_millis(10));
                cancel
                    .run_until_cancelled(async {
                        loop {
                            interval.tick().await;
                            let view = stats.read_and_reset();
                            successfully_sent.fetch_add(view.successfully_sent, Ordering::Relaxed);
                        }
                    })
                    .await;
            }
        });

    let (tx_sender, client) = builder
        .build()
        .expect("Client should be built successfully.");

    // Setup sending txs
    let tx_size = 1;
    let expected_num_txs: usize = 2;

    let txs = vec![vec![0_u8; tx_size]; 1];
    tx_sender
        .send_transactions_in_batch(txs.clone())
        .await
        .expect("Client should accept the transaction batch");
    tx_sender
        .try_send_transactions_in_batch(txs.clone())
        .expect("Client should accept the transaction batch");

    // Check results
    let now = Instant::now();
    let mut actual_num_packets = 0;
    while actual_num_packets < expected_num_txs {
        {
            let elapsed = now.elapsed();
            assert!(
                elapsed < TEST_MAX_TIME,
                "Failed to send {expected_num_txs} transaction in {elapsed:?}. Only sent \
                 {actual_num_packets}",
            );
        }

        let Ok(packets) = receiver.try_recv() else {
            sleep(Duration::from_millis(10)).await;
            continue;
        };

        actual_num_packets += packets.len();
        for p in packets.iter() {
            assert_eq!(p.meta().size, 1);
        }
    }

    // Stop client
    client
        .shutdown()
        .await
        .expect("Client should shutdown successfully.");
    assert_eq!(
        successfully_sent.load(Ordering::Relaxed),
        expected_num_txs as u64,
    );

    // Stop server
    cancel.cancel();
    server_handle.await.unwrap();
}
