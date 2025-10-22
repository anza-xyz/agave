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
        quic::{StreamerStats, DEFAULT_MAX_STREAMS_PER_MS},
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
}

impl SimpleQos {
    pub fn new(
        qos_config: SimpleQosConfig,
        max_connections_per_peer: usize,
        max_staked_connections: usize,
        stats: Arc<StreamerStats>,
        staked_nodes: Arc<RwLock<StakedNodes>>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            max_streams_per_second: qos_config.max_streams_per_second,
            max_connections_per_peer,
            max_staked_connections,
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
            const PRUNE_RANDOM_SAMPLE_SIZE: usize = 2;
            match conn_context.peer_type() {
                ConnectionPeerType::Staked(stake) => {
                    let mut connection_table_l = self.staked_connection_table.lock().await;

                    if connection_table_l.total_size >= self.max_staked_connections {
                        let num_pruned =
                            connection_table_l.prune_random(PRUNE_RANDOM_SAMPLE_SIZE, stake);
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
