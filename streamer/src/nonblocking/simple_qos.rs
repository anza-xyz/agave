use {
    crate::{
        nonblocking::{
            qos::{ConnectionContext, QosController},
            quic::{
                get_connection_stake, update_open_connections_stat, ClientConnectionTracker,
                ConnectionHandlerError, ConnectionPeerType, ConnectionTable, ConnectionTableKey,
                ConnectionTableType,
            },
            stream_throttle::{
                throttle_stream, ConnectionStreamCounter, STREAM_THROTTLING_INTERVAL,
            },
        },
        quic::{
            StreamerStats, DEFAULT_MAX_QUIC_CONNECTIONS_PER_STAKED_PEER,
            DEFAULT_MAX_STAKED_CONNECTIONS, DEFAULT_MAX_STREAMS_PER_MS,
        },
        streamer::StakedNodes,
    },
    quinn::Connection,
    solana_time_utils as timing,
    std::{
        future::Future,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc, RwLock,
        },
    },
    tokio::sync::{Mutex, MutexGuard},
    tokio_util::sync::CancellationToken,
};

#[derive(Clone)]
pub struct SimpleQosConfig {
    pub max_streams_per_second: u64,
    pub max_staked_connections: usize,
    pub max_connections_per_peer: usize,
}

impl Default for SimpleQosConfig {
    fn default() -> Self {
        SimpleQosConfig {
            max_streams_per_second: DEFAULT_MAX_STREAMS_PER_MS * 1000,
            max_staked_connections: DEFAULT_MAX_STAKED_CONNECTIONS,
            max_connections_per_peer: DEFAULT_MAX_QUIC_CONNECTIONS_PER_STAKED_PEER,
        }
    }
}

pub struct SimpleQos {
    config: SimpleQosConfig,
    stats: Arc<StreamerStats>,
    staked_connection_table: Arc<Mutex<ConnectionTable>>,
    staked_nodes: Arc<RwLock<StakedNodes>>,
}

impl SimpleQos {
    pub fn new(
        config: SimpleQosConfig,
        stats: Arc<StreamerStats>,
        staked_nodes: Arc<RwLock<StakedNodes>>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            config,
            stats,
            staked_nodes,
            staked_connection_table: Arc::new(Mutex::new(ConnectionTable::new(
                ConnectionTableType::Staked,
                cancel,
            ))),
        }
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
                self.config.max_connections_per_peer,
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
        (self.config.max_streams_per_second * interval_ms / 1000).max(1)
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
            const PRUNE_RANDOM_SAMPLE_SIZE: usize = 2;
            match conn_context.peer_type() {
                ConnectionPeerType::Staked(stake) => {
                    let mut connection_table_l = self.staked_connection_table.lock().await;

                    if connection_table_l.total_size >= self.config.max_staked_connections {
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

                    if connection_table_l.total_size < self.config.max_staked_connections {
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

    fn max_concurrent_connections(&self) -> usize {
        // Allow 25% more connections than required to allow for handshake
        self.config.max_staked_connections * 5 / 4
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
            stats.clone(),
            staked_nodes,
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
            SimpleQosConfig {
                max_connections_per_peer: 1,
                ..Default::default()
            },
            stats.clone(),
            staked_nodes,
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
            stats.clone(),
            staked_nodes,
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
            stats.clone(),
            staked_nodes,
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
            stats.clone(),
            staked_nodes,
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
            stats.clone(),
            staked_nodes,
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
            stats.clone(),
            staked_nodes,
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
            SimpleQosConfig {
                max_staked_connections: 1,
                ..Default::default()
            },
            stats.clone(),
            staked_nodes,
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
            SimpleQosConfig {
                max_staked_connections: 1,
                ..Default::default()
            },
            stats.clone(),
            staked_nodes,
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
            stats.clone(),
            staked_nodes,
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
            stats.clone(),
            staked_nodes,
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
            stats.clone(),
            staked_nodes,
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
            max_streams_per_second: 10,
            max_staked_connections: 100,
            max_connections_per_peer: 10,
        };

        let simple_qos = SimpleQos::new(qos_config, stats.clone(), staked_nodes, cancel.clone());

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
