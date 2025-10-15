use {
    crate::{
        nonblocking::{
            qos::{ConnectionContext, QosController},
            quic::{
                get_connection_stake, update_open_connections_stat, ClientConnectionTracker,
                ConnectionHandlerError, ConnectionPeerType, ConnectionTable, ConnectionTableKey,
                ConnectionTableType,
            },
            stream_throttle::{ConnectionStreamCounter, STREAM_THROTTLING_INTERVAL},
        },
        quic::StreamerStats,
        streamer::StakedNodes,
    },
    quinn::Connection,
    solana_time_utils as timing,
    std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc, RwLock,
    },
    tokio::sync::{Mutex, MutexGuard},
    tokio_util::sync::CancellationToken,
};

pub struct SimpleQos {
    max_streams_per_second: u64,
    max_staked_connections: usize,
    max_connections_per_peer: usize,
    wait_for_chunk_timeout: std::time::Duration,
    stats: Arc<StreamerStats>,
    staked_connection_table: Arc<Mutex<ConnectionTable>>,
    staked_nodes: Arc<RwLock<StakedNodes>>,
}

impl SimpleQos {
    pub fn new(
        max_streams_per_second: u64,
        max_connections_per_peer: usize,
        max_staked_connections: usize,
        wait_for_chunk_timeout: std::time::Duration,
        stats: Arc<StreamerStats>,
        staked_nodes: Arc<RwLock<StakedNodes>>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            max_streams_per_second,
            max_connections_per_peer,
            max_staked_connections,
            wait_for_chunk_timeout,
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
}

#[derive(Clone)]
pub struct SimpleQosConnectionContext {
    peer_type: ConnectionPeerType,
    remote_pubkey: Option<solana_pubkey::Pubkey>,
    last_update: Arc<AtomicU64>,
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
    fn derive_connection_context(&self, connection: &Connection) -> SimpleQosConnectionContext {
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
            last_update: Arc::new(AtomicU64::new(timing::timestamp())),
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn try_cache_connection(
        &self,
        client_connection_tracker: ClientConnectionTracker,
        connection: &quinn::Connection,
        conn_context: &mut SimpleQosConnectionContext,
    ) -> impl std::future::Future<Output = Option<(CancellationToken, Arc<ConnectionStreamCounter>)>>
           + Send {
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
                            return Some((cancel_connection, stream_counter));
                        }
                    }
                    None
                }
                ConnectionPeerType::Unstaked => None,
            }
        }
    }

    fn on_stream_accepted(&self, _conn_context: &SimpleQosConnectionContext) {}

    fn on_stream_error(&self, _conn_context: &SimpleQosConnectionContext) {}

    fn on_stream_closed(&self, _conn_context: &SimpleQosConnectionContext) {}

    fn max_streams_per_throttling_interval(&self, _context: &SimpleQosConnectionContext) -> u64 {
        let interval_ms = STREAM_THROTTLING_INTERVAL.as_millis() as u64;
        (self.max_streams_per_second * interval_ms / 1000).max(1)
    }

    #[allow(clippy::manual_async_fn)]
    fn remove_connection(
        &self,
        conn_context: &SimpleQosConnectionContext,
        connection: Connection,
    ) -> impl std::future::Future<Output = usize> + Send {
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

    fn wait_for_chunk_timeout(&self) -> std::time::Duration {
        self.wait_for_chunk_timeout
    }

    fn total_stake(&self) -> u64 {
        self.staked_nodes.read().map_or(0, |sn| sn.total_stake())
    }

    fn on_stream_finished(&self, context: &SimpleQosConnectionContext) {
        context
            .last_update
            .store(timing::timestamp(), Ordering::Relaxed);
    }
}
