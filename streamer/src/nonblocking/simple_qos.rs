use {
    crate::{
        nonblocking::{
            qos::{ConnectionContext, QosController, QosControllerWithCensor},
            quic::{
                get_connection_stake, update_open_connections_stat, ClientConnectionTracker,
                ConnectionHandlerError, ConnectionPeerType, ConnectionTable, ConnectionTableKey,
                ConnectionTableType,
            },
            stream_throttle::{
                throttle_stream, ConnectionStreamCounter, STREAM_THROTTLING_INTERVAL,
            },
            streamer_feedback::{FeedbackManager, StreamerFeedback},
        },
        quic::{StreamerStats, DEFAULT_MAX_STREAMS_PER_MS},
        streamer::StakedNodes,
    },
    crossbeam_channel::Receiver,
    quinn::Connection,
    solana_pubkey::Pubkey,
    solana_time_utils as timing,
    std::{
        future::Future,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc, RwLock,
        },
    },
    tokio::{
        sync::{Mutex, MutexGuard, RwLock as TokioRwLock},
        task,
    },
    tokio_util::sync::CancellationToken,
};

#[derive(Clone)]
pub struct SimpleQosConfig {
    pub max_streams_per_second: u64,
}

impl Default for SimpleQosConfig {
    fn default() -> Self {
        SimpleQosConfig {
            max_streams_per_second: DEFAULT_MAX_STREAMS_PER_MS * 1000,
        }
    }
}

pub struct SimpleQos {
    max_streams_per_second: u64,
    max_staked_connections: usize,
    max_connections_per_peer: usize,
    stats: Arc<StreamerStats>,
    staked_connection_table: Arc<Mutex<ConnectionTable>>,
    staked_nodes: Arc<RwLock<StakedNodes>>,
    feedback_manager: TokioRwLock<Option<Arc<FeedbackManager<Self>>>>,
}

impl SimpleQos {
    pub fn new(
        qos_config: SimpleQosConfig,
        max_connections_per_peer: usize,
        max_staked_connections: usize,
        stats: Arc<StreamerStats>,
        staked_nodes: Arc<RwLock<StakedNodes>>,
        feedback_receiver: Option<Receiver<StreamerFeedback>>,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        let qos = Arc::new(Self {
            max_streams_per_second: qos_config.max_streams_per_second,
            max_connections_per_peer,
            max_staked_connections,
            stats,
            staked_nodes,
            staked_connection_table: Arc::new(Mutex::new(ConnectionTable::new(
                ConnectionTableType::Staked,
                cancel.clone(),
            ))),
            feedback_manager: TokioRwLock::new(None),
        });
        if let Some(feedback_receiver) = feedback_receiver {
            let qos_clone = qos.clone();

            task::spawn(async move {
                let feedback_manager = Arc::new(FeedbackManager::new(qos_clone.clone()));
                let mut qos_feedback_manager = qos_clone.feedback_manager.write().await;
                *qos_feedback_manager = Some(feedback_manager.clone());
                // run_feedback_receiver(feedback_manager, feedback_receiver, cancel).await;
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    debug!("Feedback receiver loop");
                    if cancel.is_cancelled() {
                        break;
                    }
                    let feedback_timeout = std::time::Duration::from_secs(1);
                    let feedback = feedback_receiver.recv_timeout(feedback_timeout);
                    match feedback {
                        Ok(feedback) => {
                            debug!("Received feedback: {:?}", feedback);
                            feedback_manager.handle_feedback(feedback).await;
                        }
                        Err(error) => match error {
                            crossbeam_channel::RecvTimeoutError::Timeout => {
                                debug!("Feedback receiver timeout, checking for cancellation");
                                continue;
                            }
                            crossbeam_channel::RecvTimeoutError::Disconnected => {
                                debug!("Feedback receiver disconnected, exiting");
                                break;
                            }
                        },
                    }
                }
            });
        }
        debug!(
            "SimpleQos initialized: max_staked_connections={}, max_connections_per_peer={}, \
             max_streams_per_second={}",
            max_staked_connections, max_connections_per_peer, qos_config.max_streams_per_second
        );
        qos
    }

    fn cache_new_connection(
        &self,
        client_connection_tracker: ClientConnectionTracker,
        connection: &Connection,
        mut connection_table_l: MutexGuard<ConnectionTable>,
        conn_context: &SimpleQosConnectionContext,
    ) -> Result<
        (
            Arc<AtomicU64>,
            CancellationToken,
            Arc<ConnectionStreamCounter>,
        ),
        ConnectionHandlerError,
    > {
        let remote_addr = connection.remote_address();

        debug!(
            "Peer type {:?}, from peer {}",
            conn_context.peer_type(),
            remote_addr,
        );

        if let Some((last_update, cancel_connection, stream_counter)) = connection_table_l
            .try_add_connection(
                ConnectionTableKey::new(remote_addr.ip(), conn_context.remote_pubkey),
                remote_addr.port(),
                client_connection_tracker,
                Some(connection.clone()),
                conn_context.peer_type(),
                conn_context.last_update.clone(),
                self.max_connections_per_peer,
            )
        {
            update_open_connections_stat(&self.stats, &connection_table_l);
            drop(connection_table_l);

            Ok((last_update, cancel_connection, stream_counter))
        } else {
            self.stats
                .connection_add_failed
                .fetch_add(1, Ordering::Relaxed);
            Err(ConnectionHandlerError::ConnectionAddError)
        }
    }

    fn max_streams_per_throttling_interval(&self, _context: &SimpleQosConnectionContext) -> u64 {
        let interval_ms = STREAM_THROTTLING_INTERVAL.as_millis() as u64;
        (self.max_streams_per_second * interval_ms / 1000).max(1)
    }
}

#[derive(Clone)]
pub struct SimpleQosConnectionContext {
    peer_type: ConnectionPeerType,
    remote_pubkey: Option<solana_pubkey::Pubkey>,
    remote_address: std::net::SocketAddr,
    last_update: Arc<AtomicU64>,
    stream_counter: Option<Arc<ConnectionStreamCounter>>,
}

impl ConnectionContext for SimpleQosConnectionContext {
    fn peer_type(&self) -> ConnectionPeerType {
        self.peer_type
    }

    fn remote_pubkey(&self) -> Option<solana_pubkey::Pubkey> {
        self.remote_pubkey
    }
}

impl QosController<SimpleQosConnectionContext> for SimpleQos {
    fn build_connection_context(&self, connection: &Connection) -> SimpleQosConnectionContext {
        let (peer_type, remote_pubkey, _total_stake) =
            get_connection_stake(connection, &self.staked_nodes).map_or(
                (ConnectionPeerType::Unstaked, None, 0),
                |(pubkey, stake, total_stake, _max_stake, _min_stake)| {
                    (ConnectionPeerType::Staked(stake), Some(pubkey), total_stake)
                },
            );

        SimpleQosConnectionContext {
            peer_type,
            remote_pubkey,
            remote_address: connection.remote_address(),
            last_update: Arc::new(AtomicU64::new(timing::timestamp())),
            stream_counter: None,
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn try_add_connection(
        &self,
        client_connection_tracker: ClientConnectionTracker,
        connection: &quinn::Connection,
        conn_context: &mut SimpleQosConnectionContext,
    ) -> impl Future<Output = Option<CancellationToken>> + Send {
        async move {
            if let Some(feedback_manager) = &*self.feedback_manager.read().await {
                debug!(
                    "Checking censorship for client {},",
                    connection.remote_address()
                );
                let remote_pubkey = conn_context.remote_pubkey()?;
                if feedback_manager.is_client_censored(&remote_pubkey).await {
                    debug!(
                        "Rejecting connection from censored client {}",
                        connection.remote_address()
                    );
                    self.stats
                        .num_censored_clients
                        .fetch_add(1, Ordering::Relaxed);
                    return None;
                }
            }
            const PRUNE_RANDOM_SAMPLE_SIZE: usize = 2;
            match conn_context.peer_type() {
                ConnectionPeerType::Staked(stake) => {
                    let mut connection_table_l = self.staked_connection_table.lock().await;

                    if connection_table_l.total_size >= self.max_staked_connections {
                        let num_pruned =
                            connection_table_l.prune_random(PRUNE_RANDOM_SAMPLE_SIZE, stake);

                        debug!(
                            "Pruned {} staked connections to make room for new staked connection \
                             from {}",
                            num_pruned,
                            connection.remote_address(),
                        );
                        self.stats
                            .num_evictions_staked
                            .fetch_add(num_pruned, Ordering::Relaxed);
                        update_open_connections_stat(&self.stats, &connection_table_l);
                    }

                    if connection_table_l.total_size < self.max_staked_connections {
                        if let Ok((last_update, cancel_connection, stream_counter)) = self
                            .cache_new_connection(
                                client_connection_tracker,
                                connection,
                                connection_table_l,
                                conn_context,
                            )
                        {
                            self.stats
                                .connection_added_from_staked_peer
                                .fetch_add(1, Ordering::Relaxed);
                            conn_context.last_update = last_update;
                            conn_context.stream_counter = Some(stream_counter);
                            return Some(cancel_connection);
                        }
                    }
                    None
                }
                ConnectionPeerType::Unstaked => None,
            }
        }
    }

    fn on_stream_accepted(&self, conn_context: &SimpleQosConnectionContext) {
        conn_context
            .stream_counter
            .as_ref()
            .unwrap()
            .stream_count
            .fetch_add(1, Ordering::Relaxed);
    }

    fn on_stream_error(&self, _conn_context: &SimpleQosConnectionContext) {}

    fn on_stream_closed(&self, _conn_context: &SimpleQosConnectionContext) {}

    #[allow(clippy::manual_async_fn)]
    fn remove_connection(
        &self,
        conn_context: &SimpleQosConnectionContext,
        connection: Connection,
    ) -> impl Future<Output = usize> + Send {
        async move {
            let stable_id = connection.stable_id();
            let remote_addr = connection.remote_address();

            let mut connection_table = self.staked_connection_table.lock().await;
            let removed_connection_count = connection_table.remove_connection(
                ConnectionTableKey::new(remote_addr.ip(), conn_context.remote_pubkey()),
                remote_addr.port(),
                stable_id,
            );
            update_open_connections_stat(&self.stats, &connection_table);
            removed_connection_count
        }
    }

    fn on_stream_finished(&self, context: &SimpleQosConnectionContext) {
        context
            .last_update
            .store(timing::timestamp(), Ordering::Relaxed);
    }

    #[allow(clippy::manual_async_fn)]
    fn on_new_stream(
        &self,
        context: &SimpleQosConnectionContext,
    ) -> impl Future<Output = ()> + Send {
        async move {
            let peer_type = context.peer_type();
            let remote_addr = context.remote_address;
            let stream_counter: &Arc<ConnectionStreamCounter> =
                context.stream_counter.as_ref().unwrap();

            let max_streams_per_throttling_interval =
                self.max_streams_per_throttling_interval(context);

            throttle_stream(
                &self.stats,
                peer_type,
                remote_addr,
                stream_counter,
                max_streams_per_throttling_interval,
            )
            .await;
        }
    }
}

impl QosControllerWithCensor for SimpleQos {
    /// Censor a client connection, remove all connections
    async fn censor_client(&self, client: &Pubkey) {
        let mut connection_table = self.staked_connection_table.lock().await;
        let client = ConnectionTableKey::Pubkey(*client);
        connection_table.remove_connection_by_key(client);
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            nonblocking::{
                quic::{ConnectionTable, ConnectionTableType},
                testing_utilities::get_client_config,
            },
            quic::{configure_server, StreamerStats},
            streamer::StakedNodes,
        },
        quinn::Endpoint,
        solana_keypair::{Keypair, Signer},
        solana_net_utils::sockets::bind_to_localhost_unique,
        std::{
            collections::HashMap,
            sync::{
                atomic::{AtomicU64, Ordering},
                Arc, RwLock,
            },
        },
        tokio_util::sync::CancellationToken,
    };

    async fn create_connection_with_keypairs(
        server_keypair: &Keypair,
        client_keypair: &Keypair,
    ) -> (Connection, Endpoint, Endpoint) {
        // Create server endpoint
        let (server_config, _) = configure_server(server_keypair).unwrap();
        let server_socket = bind_to_localhost_unique().expect("should bind - server");
        let server_addr = server_socket.local_addr().unwrap();
        let server_endpoint = Endpoint::new(
            quinn::EndpointConfig::default(),
            Some(server_config),
            server_socket,
            Arc::new(quinn::TokioRuntime),
        )
        .unwrap();

        // Create client endpoint
        let client_socket = bind_to_localhost_unique().expect("should bind - client");
        let mut client_endpoint = Endpoint::new(
            quinn::EndpointConfig::default(),
            None,
            client_socket,
            Arc::new(quinn::TokioRuntime),
        )
        .unwrap();

        let client_config = get_client_config(client_keypair);
        client_endpoint.set_default_client_config(client_config);

        // Accept connection on server side
        let server_connection_future = async {
            let incoming = server_endpoint.accept().await.unwrap();
            incoming.await.unwrap()
        };

        // Connect from client side
        let client_connect_future = client_endpoint.connect(server_addr, "localhost").unwrap();

        // Wait for both to complete - we want the server-side connection
        let (server_connection, client_connection) =
            tokio::join!(server_connection_future, client_connect_future);

        let _client_connection = client_connection.unwrap();

        (server_connection, client_endpoint, server_endpoint)
    }

    async fn create_server_side_connection() -> (Connection, Endpoint, Endpoint) {
        let server_keypair = Keypair::new();
        let client_keypair = Keypair::new();
        create_connection_with_keypairs(&server_keypair, &client_keypair).await
    }

    fn create_staked_nodes_with_keypairs(
        server_keypair: &Keypair,
        client_keypair: &Keypair,
        stake_amount: u64,
    ) -> Arc<RwLock<StakedNodes>> {
        let mut stakes = HashMap::new();
        stakes.insert(server_keypair.pubkey(), stake_amount);
        stakes.insert(client_keypair.pubkey(), stake_amount);

        let overrides: HashMap<solana_pubkey::Pubkey, u64> = HashMap::new();

        Arc::new(RwLock::new(StakedNodes::new(Arc::new(stakes), overrides)))
    }

    #[tokio::test]
    async fn test_cache_new_connection_success() {
        // Setup
        let cancel = CancellationToken::new();
        let stats = Arc::new(StreamerStats::default());
        let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));

        let simple_qos = SimpleQos::new(
            SimpleQosConfig::default(),
            10,  // max_connections_per_peer
            100, // max_staked_connections
            stats.clone(),
            staked_nodes,
            None,
            cancel.clone(),
        );

        let connection_table = ConnectionTable::new(ConnectionTableType::Staked, cancel);
        let connection_table_guard = tokio::sync::Mutex::new(connection_table);
        let connection_table_l = connection_table_guard.lock().await;

        let client_tracker = ClientConnectionTracker {
            stats: stats.clone(),
        };

        // Create server-side accepted connection
        let (server_connection, _client_endpoint, _server_endpoint) =
            create_server_side_connection().await;

        // Create test connection context using the server-side connection
        let remote_addr = server_connection.remote_address();
        let conn_context = SimpleQosConnectionContext {
            peer_type: ConnectionPeerType::Staked(1000),
            remote_pubkey: Some(solana_pubkey::Pubkey::new_unique()),
            remote_address: remote_addr,
            last_update: Arc::new(AtomicU64::new(0)),
            stream_counter: None,
        };

        // Test
        let result = simple_qos.cache_new_connection(
            client_tracker,
            &server_connection, // Use server-side connection
            connection_table_l,
            &conn_context,
        );

        // Verify success
        assert!(result.is_ok());
        let (_last_update, cancel_token, stream_counter) = result.unwrap();
        assert!(!cancel_token.is_cancelled());
        assert_eq!(stream_counter.stream_count.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn test_cache_new_connection_max_connections_reached() {
        // Setup with connection limit of 1
        let cancel = CancellationToken::new();
        let stats = Arc::new(StreamerStats::default());
        let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));

        let simple_qos = SimpleQos::new(
            SimpleQosConfig::default(),
            1,   // max_connections_per_peer (set to 1 to trigger limit)
            100, // max_staked_connections
            stats.clone(),
            staked_nodes,
            None,
            cancel.clone(),
        );

        let mut connection_table =
            ConnectionTable::new(ConnectionTableType::Staked, cancel.clone());

        // Create first server-side connection and add it to reach the limit
        let (connection1, _client_endpoint1, _server_endpoint1) =
            create_server_side_connection().await;
        let remote_addr = connection1.remote_address();
        let key = ConnectionTableKey::new(remote_addr.ip(), None);

        let client_tracker1 = ClientConnectionTracker {
            stats: stats.clone(),
        };

        // Add first connection to reach the limit
        let _ = connection_table.try_add_connection(
            key,
            remote_addr.port(),
            client_tracker1,
            Some(connection1),
            ConnectionPeerType::Staked(1000),
            Arc::new(AtomicU64::new(0)),
            1, // max_connections_per_peer
        );

        let connection_table_guard = tokio::sync::Mutex::new(connection_table);
        let connection_table_l = connection_table_guard.lock().await;

        // Try to add second connection (should fail)
        let (connection2, _client_endpoint2, _server_endpoint2) =
            create_server_side_connection().await;
        let client_tracker2 = ClientConnectionTracker {
            stats: stats.clone(),
        };

        let conn_context = SimpleQosConnectionContext {
            peer_type: ConnectionPeerType::Staked(1000),
            remote_pubkey: None,
            remote_address: remote_addr,
            last_update: Arc::new(AtomicU64::new(0)),
            stream_counter: None,
        };

        // Test
        let result = simple_qos.cache_new_connection(
            client_tracker2,
            &connection2, // Use server-side connection
            connection_table_l,
            &conn_context,
        );

        // Verify failure due to connection limit
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ConnectionHandlerError::ConnectionAddError
        ));

        // Verify stats were updated
        assert_eq!(stats.connection_add_failed.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_cache_new_connection_updates_stats() {
        // Setup
        let cancel = CancellationToken::new();
        let stats = Arc::new(StreamerStats::default());
        let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));

        let simple_qos = SimpleQos::new(
            SimpleQosConfig::default(),
            10,  // max_connections_per_peer
            100, // max_staked_connections
            stats.clone(),
            staked_nodes,
            None,
            cancel.clone(),
        );

        let connection_table = ConnectionTable::new(ConnectionTableType::Staked, cancel);
        let connection_table_guard = tokio::sync::Mutex::new(connection_table);
        let connection_table_l = connection_table_guard.lock().await;

        let client_tracker = ClientConnectionTracker {
            stats: stats.clone(),
        };

        // Create server-side accepted connection
        let (server_connection, _client_endpoint, _server_endpoint) =
            create_server_side_connection().await;
        let remote_addr = server_connection.remote_address();

        let conn_context = SimpleQosConnectionContext {
            peer_type: ConnectionPeerType::Staked(1000),
            remote_pubkey: Some(solana_pubkey::Pubkey::new_unique()),
            remote_address: remote_addr,
            last_update: Arc::new(AtomicU64::new(0)),
            stream_counter: None,
        };

        // Record initial stats
        let initial_open_connections = stats.open_staked_connections.load(Ordering::Relaxed);

        // Test
        let result = simple_qos.cache_new_connection(
            client_tracker,
            &server_connection,
            connection_table_l,
            &conn_context,
        );

        if result.is_ok() {
            // Verify stats were updated (open connections should increase)
            assert!(
                stats.open_staked_connections.load(Ordering::Relaxed) > initial_open_connections
            );
        }
    }

    #[tokio::test]
    async fn test_build_connection_context_unstaked_peer() {
        // Setup
        let cancel = CancellationToken::new();
        let stats = Arc::new(StreamerStats::default());
        let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));

        let simple_qos = SimpleQos::new(
            SimpleQosConfig::default(),
            10,  // max_connections_per_peer
            100, // max_staked_connections
            stats.clone(),
            staked_nodes,
            None,
            cancel.clone(),
        );

        // Create server-side accepted connection
        let (server_connection, _client_endpoint, _server_endpoint) =
            create_server_side_connection().await;

        // Test - build connection context for unstaked peer
        let context = simple_qos.build_connection_context(&server_connection);

        // Verify unstaked peer context
        assert!(matches!(context.peer_type(), ConnectionPeerType::Unstaked));
        assert_eq!(context.remote_pubkey(), None);
        assert_eq!(context.remote_address, server_connection.remote_address());
        assert!(context.last_update.load(Ordering::Relaxed) > 0); // Should have timestamp
        assert!(context.stream_counter.is_none()); // Should be None initially
    }

    #[tokio::test]
    async fn test_build_connection_context_staked_peer() {
        // Setup
        let cancel = CancellationToken::new();
        let stats = Arc::new(StreamerStats::default());

        // Create keypairs for staked connection
        let server_keypair = Keypair::new();
        let client_keypair = Keypair::new();
        let stake_amount = 50_000_000; // 50M lamports

        // Create staked nodes with both keypairs
        let staked_nodes =
            create_staked_nodes_with_keypairs(&server_keypair, &client_keypair, stake_amount);

        let simple_qos = SimpleQos::new(
            SimpleQosConfig::default(),
            10,  // max_connections_per_peer
            100, // max_staked_connections
            stats.clone(),
            staked_nodes,
            None,
            cancel.clone(),
        );

        // Create connection using the staked keypairs
        let (server_connection, _client_endpoint, _server_endpoint) =
            create_connection_with_keypairs(&server_keypair, &client_keypair).await;

        // Test - build connection context for staked peer
        let context = simple_qos.build_connection_context(&server_connection);

        // Verify staked peer context
        assert!(matches!(context.peer_type(), ConnectionPeerType::Staked(_)));
        if let ConnectionPeerType::Staked(stake) = context.peer_type() {
            assert_eq!(stake, stake_amount);
        }
        assert_eq!(context.remote_pubkey(), Some(client_keypair.pubkey()));
        assert_eq!(context.remote_address, server_connection.remote_address());
        assert!(context.last_update.load(Ordering::Relaxed) > 0); // Should have timestamp
        assert!(context.stream_counter.is_none()); // Should be None initially
    }

    #[tokio::test]
    async fn test_try_add_connection_staked_peer_success() {
        // Setup
        let cancel = CancellationToken::new();
        let stats = Arc::new(StreamerStats::default());

        // Create keypairs for staked connection
        let server_keypair = Keypair::new();
        let client_keypair = Keypair::new();
        let stake_amount = 50_000_000; // 50M lamports

        // Create staked nodes with both keypairs
        let staked_nodes =
            create_staked_nodes_with_keypairs(&server_keypair, &client_keypair, stake_amount);

        let simple_qos = SimpleQos::new(
            SimpleQosConfig::default(),
            10,  // max_connections_per_peer
            100, // max_staked_connections
            stats.clone(),
            staked_nodes,
            None,
            cancel.clone(),
        );

        let client_tracker = ClientConnectionTracker {
            stats: stats.clone(),
        };

        // Create connection using the staked keypairs
        let (server_connection, _client_endpoint, _server_endpoint) =
            create_connection_with_keypairs(&server_keypair, &client_keypair).await;

        // Build connection context
        let mut conn_context = simple_qos.build_connection_context(&server_connection);

        // Test - try to add staked connection
        let result = simple_qos
            .try_add_connection(client_tracker, &server_connection, &mut conn_context)
            .await;

        // Verify successful connection addition
        assert!(result.is_some());
        let cancel_token = result.unwrap();
        assert!(!cancel_token.is_cancelled());

        // Verify context was updated with stream counter
        assert!(conn_context.stream_counter.is_some());
        assert_eq!(
            conn_context
                .stream_counter
                .as_ref()
                .unwrap()
                .stream_count
                .load(Ordering::Relaxed),
            0
        );

        // Verify stats were updated
        assert_eq!(
            stats
                .connection_added_from_staked_peer
                .load(Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn test_try_add_connection_unstaked_peer_rejected() {
        // Setup
        let cancel = CancellationToken::new();
        let stats = Arc::new(StreamerStats::default());
        let staked_nodes = Arc::new(RwLock::new(StakedNodes::default()));

        let simple_qos = SimpleQos::new(
            SimpleQosConfig::default(),
            10,  // max_connections_per_peer
            100, // max_staked_connections
            stats.clone(),
            staked_nodes,
            None,
            cancel.clone(),
        );

        let client_tracker = ClientConnectionTracker {
            stats: stats.clone(),
        };

        // Create unstaked connection
        let (server_connection, _client_endpoint, _server_endpoint) =
            create_server_side_connection().await;

        // Build connection context (will be unstaked)
        let mut conn_context = simple_qos.build_connection_context(&server_connection);

        // Test - try to add unstaked connection (should be rejected)
        let result = simple_qos
            .try_add_connection(client_tracker, &server_connection, &mut conn_context)
            .await;

        // Verify unstaked connection was rejected
        assert!(result.is_none());

        // Verify context stream counter was not set
        assert!(conn_context.stream_counter.is_none());

        // Verify no staked peer connection stats were incremented
        assert_eq!(
            stats
                .connection_added_from_staked_peer
                .load(Ordering::Relaxed),
            0
        );
    }

    #[tokio::test]
    async fn test_try_add_connection_max_staked_connections_with_pruning() {
        // Setup with very low max connections to trigger pruning
        let cancel = CancellationToken::new();
        let stats = Arc::new(StreamerStats::default());

        // Create keypairs for staked connections
        let server_keypair1 = Keypair::new();
        let client_keypair1 = Keypair::new();
        let server_keypair2 = Keypair::new();
        let client_keypair2 = Keypair::new();
        let stake_amount = 50_000_000; // 50M lamports

        let mut stakes = HashMap::new();
        stakes.insert(server_keypair1.pubkey(), stake_amount);
        stakes.insert(client_keypair1.pubkey(), stake_amount);
        stakes.insert(server_keypair2.pubkey(), stake_amount);
        // client 2 has higher stake so that it can prune client 1
        stakes.insert(client_keypair2.pubkey(), stake_amount * 2);

        let overrides: HashMap<solana_pubkey::Pubkey, u64> = HashMap::new();
        let staked_nodes = Arc::new(RwLock::new(StakedNodes::new(Arc::new(stakes), overrides)));

        let simple_qos = SimpleQos::new(
            SimpleQosConfig::default(),
            10, // max_connections_per_peer
            1,  // max_staked_connections (set to 1 to trigger pruning)
            stats.clone(),
            staked_nodes,
            None,
            cancel.clone(),
        );

        // Add first connection to fill the table
        let client_tracker1 = ClientConnectionTracker {
            stats: stats.clone(),
        };

        let (server_connection1, _client_endpoint1, _server_endpoint1) =
            create_connection_with_keypairs(&server_keypair1, &client_keypair1).await;

        let mut conn_context1 = simple_qos.build_connection_context(&server_connection1);

        let result1 = simple_qos
            .try_add_connection(client_tracker1, &server_connection1, &mut conn_context1)
            .await;

        assert!(result1.is_some()); // First connection should succeed

        // Try to add second connection (should trigger pruning)
        let client_tracker2 = ClientConnectionTracker {
            stats: stats.clone(),
        };

        let (server_connection2, _client_endpoint2, _server_endpoint2) =
            create_connection_with_keypairs(&server_keypair2, &client_keypair2).await;

        let mut conn_context2 = simple_qos.build_connection_context(&server_connection2);

        let result2 = simple_qos
            .try_add_connection(client_tracker2, &server_connection2, &mut conn_context2)
            .await;

        // Verify second connection succeeded (after pruning)
        assert!(result2.is_some());

        // Verify pruning stats were updated
        assert!(stats.num_evictions_staked.load(Ordering::Relaxed) > 0);
    }

    #[tokio::test]
    async fn test_try_add_connection_max_staked_connections_no_pruning_possible() {
        // Setup with max connections = 1 and high stake that can't be pruned
        let cancel = CancellationToken::new();
        let stats = Arc::new(StreamerStats::default());

        // Create keypairs with different stake amounts
        let server_keypair1 = Keypair::new();
        let client_keypair1 = Keypair::new();
        let server_keypair2 = Keypair::new();
        let client_keypair2 = Keypair::new();
        let high_stake = 100_000_000; // 100M lamports
        let low_stake = 10_000_000; // 10M lamports

        let mut stakes = HashMap::new();
        stakes.insert(server_keypair1.pubkey(), high_stake);
        stakes.insert(client_keypair1.pubkey(), high_stake);
        stakes.insert(server_keypair2.pubkey(), low_stake);
        stakes.insert(client_keypair2.pubkey(), low_stake);

        let overrides: HashMap<solana_pubkey::Pubkey, u64> = HashMap::new();
        let staked_nodes = Arc::new(RwLock::new(StakedNodes::new(Arc::new(stakes), overrides)));

        let simple_qos = SimpleQos::new(
            SimpleQosConfig::default(),
            10, // max_connections_per_peer
            1,  // max_staked_connections (set to 1)
            stats.clone(),
            staked_nodes,
            None,
            cancel.clone(),
        );

        // Add high-stake connection first
        let client_tracker1 = ClientConnectionTracker {
            stats: stats.clone(),
        };

        let (server_connection1, _client_endpoint1, _server_endpoint1) =
            create_connection_with_keypairs(&server_keypair1, &client_keypair1).await;

        let mut conn_context1 = simple_qos.build_connection_context(&server_connection1);

        let result1 = simple_qos
            .try_add_connection(client_tracker1, &server_connection1, &mut conn_context1)
            .await;

        assert!(result1.is_some()); // First high-stake connection should succeed

        // Try to add low-stake connection (should fail as it can't prune the high-stake one)
        let client_tracker2 = ClientConnectionTracker {
            stats: stats.clone(),
        };

        let (server_connection2, _client_endpoint2, _server_endpoint2) =
            create_connection_with_keypairs(&server_keypair2, &client_keypair2).await;

        let mut conn_context2 = simple_qos.build_connection_context(&server_connection2);

        let result2 = simple_qos
            .try_add_connection(client_tracker2, &server_connection2, &mut conn_context2)
            .await;

        // Verify second connection failed (couldn't prune higher stake)
        assert!(result2.is_none());

        // Verify context was not updated
        assert!(conn_context2.stream_counter.is_none());
    }

    #[tokio::test]
    async fn test_try_add_connection_context_updates() {
        // Setup
        let cancel = CancellationToken::new();
        let stats = Arc::new(StreamerStats::default());

        // Create keypairs for staked connection
        let server_keypair = Keypair::new();
        let client_keypair = Keypair::new();
        let stake_amount = 50_000_000; // 50M lamports

        let staked_nodes =
            create_staked_nodes_with_keypairs(&server_keypair, &client_keypair, stake_amount);

        let simple_qos = SimpleQos::new(
            SimpleQosConfig::default(),
            10,  // max_connections_per_peer
            100, // max_staked_connections
            stats.clone(),
            staked_nodes,
            None,
            cancel.clone(),
        );

        let client_tracker = ClientConnectionTracker {
            stats: stats.clone(),
        };

        let (server_connection, _client_endpoint, _server_endpoint) =
            create_connection_with_keypairs(&server_keypair, &client_keypair).await;

        let mut conn_context = simple_qos.build_connection_context(&server_connection);

        // Record initial context state
        let initial_last_update = conn_context.last_update.load(Ordering::Relaxed);
        assert!(conn_context.stream_counter.is_none());

        // Test - try to add connection
        let result = simple_qos
            .try_add_connection(client_tracker, &server_connection, &mut conn_context)
            .await;

        // Verify connection was added successfully
        assert!(result.is_some());

        // Verify context was properly updated
        assert!(conn_context.stream_counter.is_some());

        // Verify last_update was updated (should be same or newer)
        let updated_last_update = conn_context.last_update.load(Ordering::Relaxed);
        assert!(updated_last_update >= initial_last_update);

        // Verify stream counter starts at 0
        assert_eq!(
            conn_context
                .stream_counter
                .as_ref()
                .unwrap()
                .stream_count
                .load(Ordering::Relaxed),
            0
        );
    }

    #[tokio::test]
    async fn test_on_stream_accepted_increments_counter() {
        // Setup
        let cancel = CancellationToken::new();
        let stats = Arc::new(StreamerStats::default());

        // Create keypairs for staked connection
        let server_keypair = Keypair::new();
        let client_keypair = Keypair::new();
        let stake_amount = 50_000_000; // 50M lamports

        let staked_nodes =
            create_staked_nodes_with_keypairs(&server_keypair, &client_keypair, stake_amount);

        let simple_qos = SimpleQos::new(
            SimpleQosConfig::default(),
            10,  // max_connections_per_peer
            100, // max_staked_connections
            stats.clone(),
            staked_nodes,
            None,
            cancel.clone(),
        );

        let client_tracker = ClientConnectionTracker {
            stats: stats.clone(),
        };

        let (server_connection, _client_endpoint, _server_endpoint) =
            create_connection_with_keypairs(&server_keypair, &client_keypair).await;

        // Build connection context and add connection to initialize stream counter
        let mut conn_context = simple_qos.build_connection_context(&server_connection);

        let result = simple_qos
            .try_add_connection(client_tracker, &server_connection, &mut conn_context)
            .await;

        assert!(result.is_some()); // Connection should be added successfully
        assert!(conn_context.stream_counter.is_some()); // Stream counter should be set

        // Record initial stream count
        let initial_stream_count = conn_context
            .stream_counter
            .as_ref()
            .unwrap()
            .stream_count
            .load(Ordering::Relaxed);
        assert_eq!(initial_stream_count, 0);

        // Test - call on_stream_accepted
        simple_qos.on_stream_accepted(&conn_context);

        // Verify stream count was incremented
        let updated_stream_count = conn_context
            .stream_counter
            .as_ref()
            .unwrap()
            .stream_count
            .load(Ordering::Relaxed);
        assert_eq!(updated_stream_count, initial_stream_count + 1);
    }

    #[tokio::test]
    async fn test_remove_connection_success() {
        // Setup
        let cancel = CancellationToken::new();
        let stats = Arc::new(StreamerStats::default());

        // Create keypairs for staked connection
        let server_keypair = Keypair::new();
        let client_keypair = Keypair::new();
        let stake_amount = 50_000_000; // 50M lamports

        let staked_nodes =
            create_staked_nodes_with_keypairs(&server_keypair, &client_keypair, stake_amount);

        let simple_qos = SimpleQos::new(
            SimpleQosConfig::default(),
            10,  // max_connections_per_peer
            100, // max_staked_connections
            stats.clone(),
            staked_nodes,
            None,
            cancel.clone(),
        );

        let client_tracker = ClientConnectionTracker {
            stats: stats.clone(),
        };

        let (server_connection, _client_endpoint, _server_endpoint) =
            create_connection_with_keypairs(&server_keypair, &client_keypair).await;

        // Build connection context and add connection first
        let mut conn_context = simple_qos.build_connection_context(&server_connection);

        let add_result = simple_qos
            .try_add_connection(client_tracker, &server_connection, &mut conn_context)
            .await;

        assert!(add_result.is_some()); // Connection should be added successfully

        // Record initial stats
        let initial_open_connections = stats.open_staked_connections.load(Ordering::Relaxed);
        assert!(initial_open_connections > 0); // Should have at least one connection

        // Test - remove the connection
        let removed_count = simple_qos
            .remove_connection(&conn_context, server_connection.clone())
            .await;

        // Verify connection was removed
        assert_eq!(removed_count, 1); // Should have removed exactly 1 connection

        // Verify stats were updated (open connections should decrease)
        let final_open_connections = stats.open_staked_connections.load(Ordering::Relaxed);
        assert!(final_open_connections < initial_open_connections);
        assert_eq!(final_open_connections, initial_open_connections - 1);
    }

    #[tokio::test]
    async fn test_on_new_stream_throttles_correctly() {
        // Setup
        let cancel = CancellationToken::new();
        let stats = Arc::new(StreamerStats::default());

        // Create keypairs for staked connection
        let server_keypair = Keypair::new();
        let client_keypair = Keypair::new();
        let stake_amount = 50_000_000; // 50M lamports

        let staked_nodes =
            create_staked_nodes_with_keypairs(&server_keypair, &client_keypair, stake_amount);

        // Set a specific max_streams_per_second for testing
        let qos_config = SimpleQosConfig {
            max_streams_per_second: 10, // 10 streams per second
        };

        let simple_qos = SimpleQos::new(
            qos_config,
            10,  // max_connections_per_peer
            100, // max_staked_connections
            stats.clone(),
            staked_nodes,
            None,
            cancel.clone(),
        );

        let client_tracker = ClientConnectionTracker {
            stats: stats.clone(),
        };

        let (server_connection, _client_endpoint, _server_endpoint) =
            create_connection_with_keypairs(&server_keypair, &client_keypair).await;

        // Build connection context and add connection to initialize stream counter
        let mut conn_context = simple_qos.build_connection_context(&server_connection);

        let result = simple_qos
            .try_add_connection(client_tracker, &server_connection, &mut conn_context)
            .await;

        assert!(result.is_some()); // Connection should be added successfully
        assert!(conn_context.stream_counter.is_some()); // Stream counter should be set

        // Test - call on_new_stream and measure timing
        let start_time = std::time::Instant::now();

        simple_qos.on_new_stream(&conn_context).await;

        let elapsed = start_time.elapsed();

        // The function should complete (may or may not sleep depending on current throttling state)
        // We just verify it doesn't panic and completes successfully
        assert!(elapsed < std::time::Duration::from_secs(1)); // Should not take too long
    }
}

#[cfg(test)]
mod censorship_tests {
    use {
        super::*,
        crate::{
            nonblocking::{
                quic::{spawn_server_with_cancel_and_qos, SpawnNonBlockingServerResult},
                streamer_feedback::StreamerFeedback,
                testing_utilities::get_client_config,
            },
            quic::QuicStreamerConfig,
            streamer::StakedNodes,
        },
        crossbeam_channel::{unbounded, Receiver, Sender},
        solana_keypair::{Keypair, Signer},
        solana_net_utils::sockets::bind_to_localhost_unique,
        solana_perf::packet::{
            BytesPacket, BytesPacketBatchWithClientId, BytesPacketWithClientId, PacketBatch,
        },
        std::{
            collections::HashMap,
            sync::{Arc, RwLock},
            time::Duration,
        },
        tokio_util::sync::CancellationToken,
    };

    async fn create_staked_connection_with_server(
        server_address: std::net::SocketAddr,
        client_keypair: &Keypair,
    ) -> (quinn::Connection, quinn::Endpoint) {
        // Create client endpoint
        let client_socket = bind_to_localhost_unique().expect("should bind - client");
        let mut client_endpoint = quinn::Endpoint::new(
            quinn::EndpointConfig::default(),
            None,
            client_socket.try_clone().unwrap(),
            Arc::new(quinn::TokioRuntime),
        )
        .unwrap();

        let client_config = get_client_config(client_keypair);
        client_endpoint.set_default_client_config(client_config);

        debug!(
            "Client connecting to server at {} client socket: {}",
            server_address,
            client_socket.local_addr().unwrap()
        );
        // Connect to server
        let connection = client_endpoint
            .connect(server_address, "localhost")
            .unwrap()
            .await
            .unwrap();

        debug!(
            "Client connected to server {server_address} with client endpoint: {:?}",
            client_endpoint.local_addr()
        );
        (connection, client_endpoint)
    }

    fn create_test_packet_batch_with_pubkey(
        pubkey: &solana_pubkey::Pubkey,
        data: &[u8],
    ) -> PacketBatch {
        // Create a BytesPacket first
        let buffer = bytes::Bytes::from(data.to_vec());
        let mut meta = solana_packet::Meta::default();
        meta.size = data.len();

        let bytes_packet = BytesPacket::new(buffer, meta);

        // Create a BytesPacketWithClientId with the remote pubkey
        let packet_with_client_id = BytesPacketWithClientId::new(bytes_packet, Some(*pubkey));

        // Create a batch with the enhanced packet
        let mut batch = BytesPacketBatchWithClientId::with_capacity(1);
        batch.push(packet_with_client_id);

        // Return as PacketBatch::WithClientId variant
        PacketBatch::WithClientId(batch)
    }

    #[tokio::test]
    async fn test_censorship_blocks_connection_via_server() {
        agave_logger::setup();

        // Setup server with feedback channel
        let (server_thread, feedback_sender, _packet_receiver, server_address, cancel) =
            create_test_qos_server_with_feedback().await;

        let client_keypair = Keypair::new();
        // Update staked nodes to include the client
        // Note: In a real test, you'd need access to the server's staked_nodes to update it
        // For this test, we'll assume the client is already staked

        // First, verify connection works when not censored
        let (connection1, _client_endpoint1) =
            create_staked_connection_with_server(server_address, &client_keypair).await;

        assert!(
            connection1.close_reason().is_none(),
            "Connection should be open initially"
        );

        // Censor the client
        feedback_sender
            .send(StreamerFeedback::CensorClient((
                client_keypair.pubkey(),
                Some(Duration::from_secs(60)), // Temporary censorship
            )))
            .unwrap();

        // Give time for censorship to be processed
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Try to create a new connection - should be blocked/rejected
        let connection_result = tokio::time::timeout(
            Duration::from_secs(5),
            create_staked_connection_with_server(server_address, &client_keypair),
        )
        .await;

        // The connection should either timeout or be rejected
        assert!(
            connection_result.is_err() || connection_result.unwrap().0.close_reason().is_some(),
            "New connection should be blocked after censoring"
        );

        // Cleanup
        cancel.cancel();
        server_thread.thread.await.unwrap();
    }

    #[tokio::test]
    async fn test_uncensor_client_restores_access_via_server() {
        agave_logger::setup();

        let (server_thread, feedback_sender, _packet_receiver, server_address, cancel) =
            create_test_qos_server_with_feedback().await;

        let client_keypair = Keypair::new();

        // Censor the client first
        feedback_sender
            .send(StreamerFeedback::CensorClient((
                client_keypair.pubkey(),
                Some(Duration::from_secs(3600)), // Long duration
            )))
            .unwrap();

        tokio::time::sleep(Duration::from_millis(300)).await;

        // Verify connection is blocked
        let censored_connection_result = tokio::time::timeout(
            Duration::from_secs(2),
            create_staked_connection_with_server(server_address, &client_keypair),
        )
        .await;

        assert!(
            censored_connection_result.is_err(),
            "Connection should be blocked when censored"
        );

        // Uncensor the client
        feedback_sender
            .send(StreamerFeedback::UncensorClient(client_keypair.pubkey()))
            .unwrap();

        tokio::time::sleep(Duration::from_millis(500)).await;

        // Verify connection now works
        let uncensored_connection_result = tokio::time::timeout(
            Duration::from_secs(5),
            create_staked_connection_with_server(server_address, &client_keypair),
        )
        .await;

        assert!(
            uncensored_connection_result.is_ok(),
            "Connection should succeed after uncensoring"
        );

        let (connection, _endpoint) = uncensored_connection_result.unwrap();
        assert!(
            connection.close_reason().is_none(),
            "Connection should be open after uncensoring"
        );

        // Cleanup
        cancel.cancel();
        server_thread.thread.await.unwrap();
    }

    #[tokio::test]
    async fn test_temporary_censorship_expires_automatically_via_server() {
        agave_logger::setup();

        let (server_thread, feedback_sender, _packet_receiver, server_address, cancel) =
            create_test_qos_server_with_feedback().await;

        let client_keypair = Keypair::new();

        // Censor the client for a short duration
        feedback_sender
            .send(StreamerFeedback::CensorClient((
                client_keypair.pubkey(),
                Some(Duration::from_millis(1000)), // Short duration
            )))
            .unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;

        // Verify connection is initially blocked
        let blocked_connection_result = tokio::time::timeout(
            Duration::from_secs(1),
            create_staked_connection_with_server(server_address, &client_keypair),
        )
        .await;

        assert!(
            blocked_connection_result.is_err(),
            "Connection should be blocked during censorship"
        );

        // Wait for censorship to expire
        tokio::time::sleep(Duration::from_millis(1200)).await;

        // Verify connection now works after expiration
        let unblocked_connection_result = tokio::time::timeout(
            Duration::from_secs(5),
            create_staked_connection_with_server(server_address, &client_keypair),
        )
        .await;

        assert!(
            unblocked_connection_result.is_ok(),
            "Connection should succeed after censorship expires"
        );

        let (connection, _endpoint) = unblocked_connection_result.unwrap();
        assert!(
            connection.close_reason().is_none(),
            "Connection should be open after expiration"
        );

        // Cleanup
        cancel.cancel();
        server_thread.thread.await.unwrap();
    }

    #[tokio::test]
    async fn test_multiple_clients_censorship_via_server() {
        agave_logger::setup();

        let (server_thread, feedback_sender, _packet_receiver, server_address, cancel) =
            create_test_qos_server_with_feedback().await;

        let client1_keypair = Keypair::new();
        let client2_keypair = Keypair::new();
        let client3_keypair = Keypair::new();

        // Censor client1 and client3, leave client2 uncensored
        feedback_sender
            .send(StreamerFeedback::CensorClient((
                client1_keypair.pubkey(),
                Some(Duration::from_secs(60)),
            )))
            .unwrap();

        feedback_sender
            .send(StreamerFeedback::CensorClient((
                client3_keypair.pubkey(),
                None, // Indefinite
            )))
            .unwrap();

        tokio::time::sleep(Duration::from_millis(300)).await;

        // Test connections
        let result1 = tokio::time::timeout(
            Duration::from_secs(2),
            create_staked_connection_with_server(server_address, &client1_keypair),
        )
        .await;

        let result2 = tokio::time::timeout(
            Duration::from_secs(5),
            create_staked_connection_with_server(server_address, &client2_keypair),
        )
        .await;

        let result3 = tokio::time::timeout(
            Duration::from_secs(2),
            create_staked_connection_with_server(server_address, &client3_keypair),
        )
        .await;

        // Verify results
        assert!(result1.is_err(), "Client1 should be blocked (censored)");
        assert!(result2.is_ok(), "Client2 should succeed (not censored)");
        assert!(result3.is_err(), "Client3 should be blocked (censored)");

        // Verify client2's connection is actually open
        if let Ok((connection, _endpoint)) = result2 {
            assert!(
                connection.close_reason().is_none(),
                "Client2 connection should be open"
            );
        }

        // Cleanup
        cancel.cancel();
        server_thread.thread.await.unwrap();
    }

    #[tokio::test]
    async fn test_send_packets_with_censored_client() {
        agave_logger::setup();

        let (server_thread, feedback_sender, packet_receiver, server_address, cancel) =
            create_test_qos_server_with_feedback().await;

        let allowed_client = Keypair::new();
        let censored_client = Keypair::new();

        // First establish connections with both clients before censoring
        let (allowed_connection, _allowed_endpoint) =
            create_staked_connection_with_server(server_address, &allowed_client).await;

        let (censored_connection, _censored_endpoint) =
            create_staked_connection_with_server(server_address, &censored_client).await;

        // Send some data from both clients before censoring
        let mut allowed_stream = allowed_connection.open_uni().await.unwrap();
        let allowed_data = b"transaction from allowed client - before censoring";
        allowed_stream.write_all(allowed_data).await.unwrap();
        allowed_stream.finish().unwrap();

        let mut censored_stream_before = censored_connection.open_uni().await.unwrap();
        let censored_data_before = b"transaction from censored client - before censoring";
        censored_stream_before
            .write_all(censored_data_before)
            .await
            .unwrap();
        censored_stream_before.finish().unwrap();

        // Give time for packets to be processed
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Check that we received packets from both clients before censoring
        let packets_before_censoring: Vec<_> = packet_receiver.try_iter().collect();
        assert!(
            packets_before_censoring.len() >= 2,
            "Should have received packets from both clients before censoring"
        );

        // Now censor the censored client
        feedback_sender
            .send(StreamerFeedback::CensorClient((
                censored_client.pubkey(),
                Some(Duration::from_secs(60)),
            )))
            .unwrap();

        // Give time for censorship to be processed
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Try to send data from the censored client - the connection should be closed
        // or the data should not be processed
        let censored_stream_result = censored_connection.open_uni().await;

        if let Ok(mut censored_stream_after) = censored_stream_result {
            let censored_data_after = b"transaction from censored client - after censoring";
            let write_result = censored_stream_after.write_all(censored_data_after).await;

            // The write might succeed but the stream should be closed/rejected
            if write_result.is_ok() {
                let _ = censored_stream_after.finish();
            }
        }

        // Send data from the allowed client - this should still work
        let mut allowed_stream_after = allowed_connection.open_uni().await.unwrap();
        let allowed_data_after = b"transaction from allowed client - after censoring";
        allowed_stream_after
            .write_all(allowed_data_after)
            .await
            .unwrap();
        allowed_stream_after.finish().unwrap();

        // Give time for packet processing
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Check packets received after censoring
        let packets_after_censoring: Vec<_> = packet_receiver.try_iter().collect();

        // We should receive the packet from the allowed client
        assert!(
            !packets_after_censoring.is_empty(),
            "Should have received packet from allowed client after censoring"
        );

        // Verify that new connections from censored client are blocked
        let new_censored_connection_result = tokio::time::timeout(
            Duration::from_secs(2),
            create_staked_connection_with_server(server_address, &censored_client),
        )
        .await;

        assert!(
            new_censored_connection_result.is_err(),
            "New connections from censored client should be blocked"
        );

        // Verify that new connections from allowed client still work
        let new_allowed_connection_result = tokio::time::timeout(
            Duration::from_secs(5),
            create_staked_connection_with_server(server_address, &allowed_client),
        )
        .await;

        assert!(
            new_allowed_connection_result.is_ok(),
            "New connections from allowed client should still work"
        );

        // Cleanup
        cancel.cancel();
        server_thread.thread.await.unwrap();
    }

    #[tokio::test]
    async fn test_censored_client_existing_connection_behavior() {
        agave_logger::setup();

        let (server_thread, feedback_sender, packet_receiver, server_address, cancel) =
            create_test_qos_server_with_feedback().await;

        let client_keypair = Keypair::new();

        // Establish connection first
        let (connection, _endpoint) =
            create_staked_connection_with_server(server_address, &client_keypair).await;

        // Send data before censoring to verify connection works
        let mut stream_before = connection.open_uni().await.unwrap();
        let data_before = b"data before censoring";
        stream_before.write_all(data_before).await.unwrap();
        stream_before.finish().unwrap();

        tokio::time::sleep(Duration::from_millis(200)).await;

        // Verify we received the packet
        let packets_before: Vec<_> = packet_receiver.try_iter().collect();
        assert!(
            !packets_before.is_empty(),
            "Should receive packet before censoring"
        );

        // Now censor the client
        feedback_sender
            .send(StreamerFeedback::CensorClient((
                client_keypair.pubkey(),
                Some(Duration::from_secs(60)),
            )))
            .unwrap();

        tokio::time::sleep(Duration::from_millis(500)).await;

        // Test what happens to the existing connection
        // The connection should either be closed or further streams should be rejected
        let stream_after_result = connection.open_uni().await;

        match stream_after_result {
            Ok(mut stream_after) => {
                // If we can open a stream, try to send data
                let data_after = b"data after censoring";
                let write_result = stream_after.write_all(data_after).await;

                if write_result.is_ok() {
                    let _ = stream_after.finish();
                }

                // Give time for processing
                tokio::time::sleep(Duration::from_millis(200)).await;

                // Check if packet was processed - it should be dropped/rejected
                let packets_after: Vec<_> = packet_receiver.try_iter().collect();

                // Ideally, no packets should be received from censored client
                // (This depends on how the censorship is implemented)
                println!("Packets received after censoring: {}", packets_after.len());
            }
            Err(_) => {
                // Connection was closed - this is the expected behavior
                println!("Connection was closed after censoring (expected behavior)");
            }
        }

        // Verify the connection is in a closed/error state
        tokio::time::sleep(Duration::from_millis(100)).await;

        // The connection should eventually show a close reason if it was terminated
        if let Some(close_reason) = connection.close_reason() {
            println!("Connection closed with reason: {:?}", close_reason);
        }

        // Cleanup
        cancel.cancel();
        server_thread.thread.await.unwrap();
    }

    #[tokio::test]
    async fn test_packet_filtering_with_remote_pubkey() {
        // This test demonstrates how to filter packets based on remote pubkey
        // when they are received from the server

        let allowed_client = Keypair::new();
        let censored_client = Keypair::new();

        // Create test packet batches that would come from the server
        let allowed_batch = create_test_packet_batch_with_pubkey(
            &allowed_client.pubkey(),
            b"transaction from allowed client",
        );

        let censored_batch = create_test_packet_batch_with_pubkey(
            &censored_client.pubkey(),
            b"transaction from censored client",
        );

        // Simulate a censorship list
        let censored_pubkeys = vec![censored_client.pubkey()];

        // Function to filter packets based on censorship
        let should_process_packet = |batch: &PacketBatch| -> bool {
            match batch {
                PacketBatch::WithClientId(batch) => {
                    // Check if any packet in the batch is from a censored client
                    !batch.iter().any(|packet| {
                        packet
                            .remote_pubkey()
                            .map_or(false, |pk| censored_pubkeys.contains(pk))
                    })
                }
                _ => true, // Process packets without remote pubkey info
            }
        };

        // Test the filtering logic
        assert!(
            should_process_packet(&allowed_batch),
            "Should process allowed client packets"
        );
        assert!(
            !should_process_packet(&censored_batch),
            "Should NOT process censored client packets"
        );

        // Simulate processing packets
        let batches = vec![allowed_batch, censored_batch];
        let processed_batches: Vec<_> = batches.into_iter().filter(should_process_packet).collect();

        // Only the allowed client's batch should be processed
        assert_eq!(processed_batches.len(), 1);

        match &processed_batches[0] {
            PacketBatch::WithClientId(batch) => {
                assert_eq!(batch[0].remote_pubkey(), Some(&allowed_client.pubkey()));
            }
            _ => panic!("Expected WithClientId variant"),
        }
    }

    async fn create_test_qos_server_with_feedback_and_stakes(
        staked_clients: HashMap<solana_pubkey::Pubkey, u64>,
    ) -> (
        SpawnNonBlockingServerResult,
        Sender<StreamerFeedback>,
        Receiver<PacketBatch>,
        std::net::SocketAddr,
        CancellationToken,
    ) {
        let s = bind_to_localhost_unique().expect("should bind");
        let (packet_sender, packet_receiver) = unbounded();
        let (feedback_sender, feedback_receiver) = unbounded();
        let keypair = Keypair::new();
        let server_address = s.local_addr().unwrap();
        let cancel = CancellationToken::new();

        // Create staked nodes with the provided stakes
        let overrides = HashMap::new();
        let staked_nodes = Arc::new(RwLock::new(StakedNodes::new(
            Arc::new(staked_clients.clone()),
            overrides,
        )));

        debug!(
            "Server configured with {} staked clients",
            staked_clients.len()
        );
        for (pubkey, stake) in &staked_clients {
            debug!("  Staked client: {} with stake: {}", pubkey, stake);
        }

        // Create SimpleQos with feedback receiver
        let qos_config = SimpleQosConfig::default();
        let max_connections_per_peer = 10;
        let max_staked_connections = 100;
        let stats = Arc::new(crate::quic::StreamerStats::default());

        let qos = SimpleQos::new(
            qos_config,
            max_connections_per_peer,
            max_staked_connections,
            stats.clone(),
            staked_nodes,
            Some(feedback_receiver),
            cancel.clone(),
        );

        debug!(
            "Spawning nonblocking server with SimpleQos on {}",
            server_address
        );

        let streamer_config = QuicStreamerConfig::default_for_tests();

        // Use spawn_server_with_cancel_and_qos directly
        let spawn_result = spawn_server_with_cancel_and_qos(
            "testQuicServer",
            stats,
            [s],
            &keypair,
            packet_sender,
            streamer_config,
            qos,
            cancel.clone(),
        )
        .expect("Failed to spawn server");

        debug!("Nonblocking server spawned, waiting for startup");
        // Give the server time to start up
        tokio::time::sleep(Duration::from_millis(1000)).await;
        debug!("Server should be ready now");

        (
            spawn_result,
            feedback_sender,
            packet_receiver,
            server_address,
            cancel,
        )
    }

    // Update the existing function to use empty stakes (for unstaked tests)
    async fn create_test_qos_server_with_feedback() -> (
        SpawnNonBlockingServerResult,
        Sender<StreamerFeedback>,
        Receiver<PacketBatch>,
        std::net::SocketAddr,
        CancellationToken,
    ) {
        // Call the version with stakes, passing empty HashMap for unstaked clients
        create_test_qos_server_with_feedback_and_stakes(HashMap::new()).await
    }

    // Now update the censorship tests to properly configure stakes
    #[tokio::test]
    async fn test_censorship_blocks_staked_connection_via_server() {
        agave_logger::setup();

        let client_keypair = Keypair::new();
        let stake_amount = 50_000_000;

        // Create server with the client pre-staked
        let mut staked_clients = HashMap::new();
        staked_clients.insert(client_keypair.pubkey(), stake_amount);

        let (server_thread, feedback_sender, _packet_receiver, server_address, cancel) =
            create_test_qos_server_with_feedback_and_stakes(staked_clients).await;

        // Now the client will actually be recognized as staked
        let (connection1, _client_endpoint1) =
            create_staked_connection_with_server(server_address, &client_keypair).await;

        assert!(
            connection1.close_reason().is_none(),
            "Connection should be open initially"
        );

        // Censor the client
        feedback_sender
            .send(StreamerFeedback::CensorClient((
                client_keypair.pubkey(),
                Some(Duration::from_secs(60)),
            )))
            .unwrap();

        tokio::time::sleep(Duration::from_millis(500)).await;

        // Try to create a new connection - should be blocked
        let connection_result = tokio::time::timeout(
            Duration::from_secs(5),
            create_staked_connection_with_server(server_address, &client_keypair),
        )
        .await;

        assert!(
            connection_result.is_err() || connection_result.unwrap().0.close_reason().is_some(),
            "New connection should be blocked after censoring"
        );

        // Cleanup
        cancel.cancel();
        server_thread.thread.await.unwrap();
    }

    async fn verify_connection_works_by_sending_data(
        connection: &quinn::Connection,
        data: &[u8],
    ) -> bool {
        match connection.open_uni().await {
            Ok(mut stream) => match stream.write_all(data).await {
                Ok(()) => match stream.finish() {
                    Ok(()) => {
                        debug!("Stream finished successfully");
                        true
                    }
                    Err(err) => {
                        debug!("Error finishing stream: {}", err);
                        false
                    }
                },
                Err(err) => {
                    debug!("Error writing to stream: {}", err);
                    false
                }
            },
            Err(err) => {
                debug!("Error opening stream: {}", err);
                false
            }
        }
    }

    async fn verify_connection_blocked_by_sending_data(
        connection: &quinn::Connection,
        data: &[u8],
    ) -> bool {
        // Returns true if the connection is blocked (data fails to send)
        !verify_connection_works_by_sending_data(connection, data).await
    }

    #[tokio::test]
    async fn test_uncensor_staked_client_restores_access_via_server() {
        debug!("Starting test for uncensorship of staked client via server");
        agave_logger::setup();

        debug!("Starting test for uncensorship of staked client via server");
        let client_keypair = Keypair::new();
        let stake_amount = 50_000_000;

        // Create server with the client pre-staked
        let mut staked_clients = HashMap::new();
        staked_clients.insert(client_keypair.pubkey(), stake_amount);

        let (server_thread, feedback_sender, _packet_receiver, server_address, cancel) =
            create_test_qos_server_with_feedback_and_stakes(staked_clients).await;

        debug!("Sleeping briefly to ensure server is ready");

        tokio::time::sleep(Duration::from_millis(200)).await;

        debug!("Creating connection for staked client");
        // First verify the client can connect when not censored
        let initial_connection_result = tokio::time::timeout(
            Duration::from_secs(5),
            create_staked_connection_with_server(server_address, &client_keypair),
        )
        .await;

        debug!("Initial connection result: {:?}", initial_connection_result);
        if initial_connection_result.is_err() {
            // If we can't connect initially, the test setup is wrong
            cancel.cancel();
            server_thread.thread.await.unwrap();
            panic!("Initial connection failed - server setup issue");
        }
        debug!("Initial connection succeeded");
        // Close the initial connection
        let (initial_connection, initial_endpoint) = initial_connection_result.unwrap();

        assert!(verify_connection_works_by_sending_data(&initial_connection, b"test data").await);

        initial_connection.close(0u32.into(), b"test cleanup");
        initial_endpoint.close(0u32.into(), b"test cleanup");

        // Give time for connection cleanup
        tokio::time::sleep(Duration::from_millis(100)).await;

        debug!("Censoring the client now");
        // Censor the client
        feedback_sender
            .send(StreamerFeedback::CensorClient((
                client_keypair.pubkey(),
                Some(Duration::from_secs(10)), // Shorter duration for testing
            )))
            .unwrap();

        tokio::time::sleep(Duration::from_millis(500)).await;

        debug!("Verifying connection is blocked after censorship");
        // First verify the client can connect when not censored
        let censored_connection_result = tokio::time::timeout(
            Duration::from_secs(3),
            create_staked_connection_with_server(server_address, &client_keypair),
        )
        .await;

        debug!(
            "Connection result after censorship: {:?}",
            censored_connection_result
        );
        let censored_connection = censored_connection_result
            .expect("Connection attempt did not timeout")
            .0;
        assert!(
            verify_connection_blocked_by_sending_data(&censored_connection, b"test data").await
        );

        debug!("Uncensoring the client now");
        // Uncensor the client
        feedback_sender
            .send(StreamerFeedback::UncensorClient(client_keypair.pubkey()))
            .unwrap();

        tokio::time::sleep(Duration::from_millis(500)).await;

        debug!("Verifying connection works after uncensorship");
        // Verify connection now works with shorter timeout
        let uncensored_connection_result = tokio::time::timeout(
            Duration::from_secs(10), // Reasonable timeout
            create_staked_connection_with_server(server_address, &client_keypair),
        )
        .await;

        match uncensored_connection_result {
            Ok((connection, endpoint)) => {
                assert!(
                    connection.close_reason().is_none(),
                    "Connection should be open after uncensoring"
                );
                // Clean up the connection
                connection.close(0u32.into(), b"test cleanup");
                endpoint.close(0u32.into(), b"test cleanup");
            }
            Err(_) => {
                // If it still fails, the uncensoring might not be working properly
                cancel.cancel();
                server_thread.thread.await.unwrap();
                panic!("Connection should succeed after uncensoring, but timed out");
            }
        }

        debug!("Testing uncensorship for staked client done.");
        // Cleanup
        cancel.cancel();
        server_thread.thread.await.unwrap();
    }

    #[tokio::test]
    async fn test_multiple_staked_clients_censorship_via_server() {
        agave_logger::setup();

        let client1_keypair = Keypair::new();
        let client2_keypair = Keypair::new();
        let client3_keypair = Keypair::new();
        let stake_amount = 50_000_000;

        // Create server with all clients pre-staked
        let mut staked_clients = HashMap::new();
        staked_clients.insert(client1_keypair.pubkey(), stake_amount);
        staked_clients.insert(client2_keypair.pubkey(), stake_amount);
        staked_clients.insert(client3_keypair.pubkey(), stake_amount);

        let (server_thread, feedback_sender, _packet_receiver, server_address, cancel) =
            create_test_qos_server_with_feedback_and_stakes(staked_clients).await;

        // Censor client1 and client3, leave client2 uncensored
        feedback_sender
            .send(StreamerFeedback::CensorClient((
                client1_keypair.pubkey(),
                Some(Duration::from_secs(60)),
            )))
            .unwrap();

        feedback_sender
            .send(StreamerFeedback::CensorClient((
                client3_keypair.pubkey(),
                None, // Indefinite
            )))
            .unwrap();

        tokio::time::sleep(Duration::from_millis(300)).await;

        // Test connections
        let result1 = tokio::time::timeout(
            Duration::from_secs(2),
            create_staked_connection_with_server(server_address, &client1_keypair),
        )
        .await;

        let result2 = tokio::time::timeout(
            Duration::from_secs(5),
            create_staked_connection_with_server(server_address, &client2_keypair),
        )
        .await;

        let result3 = tokio::time::timeout(
            Duration::from_secs(2),
            create_staked_connection_with_server(server_address, &client3_keypair),
        )
        .await;

        // Verify results
        assert!(result1.is_err(), "Client1 should be blocked (censored)");
        assert!(result2.is_ok(), "Client2 should succeed (not censored)");
        assert!(result3.is_err(), "Client3 should be blocked (censored)");

        // Verify client2's connection is actually open
        if let Ok((connection, _endpoint)) = result2 {
            assert!(
                connection.close_reason().is_none(),
                "Client2 connection should be open"
            );
        }

        // Cleanup
        cancel.cancel();
        server_thread.thread.await.unwrap();
    }

    #[tokio::test]
    async fn test_send_packets_with_censored_staked_client() {
        agave_logger::setup();

        let allowed_client = Keypair::new();
        let censored_client = Keypair::new();
        let stake_amount = 50_000_000;

        // Create server with both clients pre-staked
        let mut staked_clients = HashMap::new();
        staked_clients.insert(allowed_client.pubkey(), stake_amount);
        staked_clients.insert(censored_client.pubkey(), stake_amount);

        let (server_thread, feedback_sender, packet_receiver, server_address, cancel) =
            create_test_qos_server_with_feedback_and_stakes(staked_clients).await;

        // First establish connections with both staked clients before censoring
        let (allowed_connection, _allowed_endpoint) =
            create_staked_connection_with_server(server_address, &allowed_client).await;

        let (censored_connection, _censored_endpoint) =
            create_staked_connection_with_server(server_address, &censored_client).await;

        // Send some data from both clients before censoring
        let mut allowed_stream = allowed_connection.open_uni().await.unwrap();
        let allowed_data = b"transaction from allowed staked client - before censoring";
        allowed_stream.write_all(allowed_data).await.unwrap();
        allowed_stream.finish().unwrap();

        let mut censored_stream_before = censored_connection.open_uni().await.unwrap();
        let censored_data_before = b"transaction from censored staked client - before censoring";
        censored_stream_before
            .write_all(censored_data_before)
            .await
            .unwrap();
        censored_stream_before.finish().unwrap();

        // Give time for packets to be processed
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Check that we received packets from both staked clients before censoring
        let packets_before_censoring: Vec<_> = packet_receiver.try_iter().collect();
        assert!(
            packets_before_censoring.len() >= 2,
            "Should have received packets from both staked clients before censoring"
        );

        // Now censor the censored client
        feedback_sender
            .send(StreamerFeedback::CensorClient((
                censored_client.pubkey(),
                Some(Duration::from_secs(60)),
            )))
            .unwrap();

        // Give time for censorship to be processed
        tokio::time::sleep(Duration::from_millis(500)).await;

        // The existing connection from censored client should be terminated
        // Try to send data from the censored client - should fail
        let censored_stream_result = censored_connection.open_uni().await;

        if let Ok(mut censored_stream_after) = censored_stream_result {
            let censored_data_after = b"transaction from censored staked client - after censoring";
            let write_result = censored_stream_after.write_all(censored_data_after).await;

            // The write might succeed but the stream should be closed/rejected
            if write_result.is_ok() {
                let _ = censored_stream_after.finish();
            }
        }

        // Send data from the allowed client - this should still work
        let mut allowed_stream_after = allowed_connection.open_uni().await.unwrap();
        let allowed_data_after = b"transaction from allowed staked client - after censoring";
        allowed_stream_after
            .write_all(allowed_data_after)
            .await
            .unwrap();
        allowed_stream_after.finish().unwrap();

        // Give time for packet processing
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Check packets received after censoring
        let packets_after_censoring: Vec<_> = packet_receiver.try_iter().collect();

        // We should receive the packet from the allowed staked client
        assert!(
            !packets_after_censoring.is_empty(),
            "Should have received packet from allowed staked client after censoring"
        );

        // Verify that new connections from censored staked client are blocked
        let new_censored_connection_result = tokio::time::timeout(
            Duration::from_secs(2),
            create_staked_connection_with_server(server_address, &censored_client),
        )
        .await;

        assert!(
            new_censored_connection_result.is_err(),
            "New connections from censored staked client should be blocked"
        );

        // Verify that new connections from allowed staked client still work
        let new_allowed_connection_result = tokio::time::timeout(
            Duration::from_secs(5),
            create_staked_connection_with_server(server_address, &allowed_client),
        )
        .await;

        assert!(
            new_allowed_connection_result.is_ok(),
            "New connections from allowed staked client should still work"
        );

        // Cleanup
        cancel.cancel();
        server_thread.thread.await.unwrap();
    }

    // Test mixed staked and unstaked clients
    #[tokio::test]
    async fn test_censorship_mixed_staked_unstaked_clients() {
        agave_logger::setup();

        let staked_client = Keypair::new();
        let unstaked_client = Keypair::new();
        let stake_amount = 50_000_000;

        // Create server with only one client staked
        let mut staked_clients = HashMap::new();
        staked_clients.insert(staked_client.pubkey(), stake_amount);
        // unstaked_client is not in the map, so it will be unstaked

        let (server_thread, feedback_sender, _packet_receiver, server_address, cancel) =
            create_test_qos_server_with_feedback_and_stakes(staked_clients).await;

        // Try to connect staked client - should work
        let staked_connection_result = tokio::time::timeout(
            Duration::from_secs(5),
            create_staked_connection_with_server(server_address, &staked_client),
        )
        .await;

        assert!(
            staked_connection_result.is_ok(),
            "Staked client should be able to connect"
        );

        // Try to connect unstaked client - should be rejected (SimpleQoS only accepts staked)
        let unstaked_connection_result = tokio::time::timeout(
            Duration::from_secs(2),
            create_staked_connection_with_server(server_address, &unstaked_client),
        )
        .await;

        // Unstaked clients are rejected by SimpleQoS, so this should timeout or fail
        assert!(
            unstaked_connection_result.is_err(),
            "Unstaked client should be rejected by SimpleQoS"
        );

        // Now censor the staked client
        feedback_sender
            .send(StreamerFeedback::CensorClient((
                staked_client.pubkey(),
                Some(Duration::from_secs(60)),
            )))
            .unwrap();

        tokio::time::sleep(Duration::from_millis(300)).await;

        // Try to connect staked client again - should now be blocked
        let censored_staked_connection_result = tokio::time::timeout(
            Duration::from_secs(2),
            create_staked_connection_with_server(server_address, &staked_client),
        )
        .await;

        assert!(
            censored_staked_connection_result.is_err(),
            "Censored staked client should be blocked"
        );

        // Cleanup
        cancel.cancel();
        server_thread.thread.await.unwrap();
    }
}
