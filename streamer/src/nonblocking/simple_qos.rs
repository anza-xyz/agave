use {
    crate::{
        nonblocking::{
            quic::{
                get_connection_stake, ClientConnectionTracker, ConnectionHandlerError,
                ConnectionPeerType, ConnectionQosParams, ConnectionTable, ConnectionTableKey, Qos,
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
};

pub struct SimpleQos {
    max_streams_per_second: u64,
    max_staked_connections: usize,
    max_connections_per_peer: usize,
    stats: Arc<StreamerStats>,
    staked_connection_table: Arc<Mutex<ConnectionTable>>,
}

impl SimpleQos {
    pub fn new(
        max_streams_per_second: u64,
        max_connections_per_peer: usize,
        max_staked_connections: usize,
        stats: Arc<StreamerStats>,
    ) -> Self {
        Self {
            max_streams_per_second,
            max_connections_per_peer,
            max_staked_connections,
            stats,
            staked_connection_table: Arc::new(Mutex::new(ConnectionTable::new())),
        }
    }

    fn handle_and_cache_new_connection(
        &self,
        client_connection_tracker: ClientConnectionTracker,
        connection: &Connection,
        mut connection_table_l: MutexGuard<ConnectionTable>,
        params: &SimpleQosParams,
    ) -> Result<
        (
            Arc<AtomicU64>,
            tokio_util::sync::CancellationToken,
            Arc<ConnectionStreamCounter>,
        ),
        ConnectionHandlerError,
    > {
        let remote_addr = connection.remote_address();

        debug!(
            "Peer type {:?}, from peer {}",
            params.peer_type(),
            remote_addr,
        );

        if let Some((last_update, cancel_connection, stream_counter)) = connection_table_l
            .try_add_connection(
                ConnectionTableKey::new(remote_addr.ip(), params.remote_pubkey),
                remote_addr.port(),
                client_connection_tracker,
                Some(connection.clone()),
                params.peer_type(),
                timing::timestamp(),
                params.max_connections_per_peer(),
            )
        {
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
struct SimpleQosParams {
    peer_type: ConnectionPeerType,
    max_connections_per_peer: usize,
    remote_pubkey: Option<solana_pubkey::Pubkey>,
    total_stake: u64,
}

impl ConnectionQosParams for SimpleQosParams {
    fn peer_type(&self) -> ConnectionPeerType {
        self.peer_type
    }

    fn max_connections_per_peer(&self) -> usize {
        self.max_connections_per_peer
    }

    fn remote_pubkey(&self) -> Option<solana_pubkey::Pubkey> {
        self.remote_pubkey
    }
    fn total_stake(&self) -> u64 {
        self.total_stake
    }
}

impl Qos<SimpleQosParams> for SimpleQos {
    fn derive_qos_params(
        &self,
        connection: &Connection,
        staked_nodes: &RwLock<StakedNodes>,
    ) -> SimpleQosParams {
        let (peer_type, remote_pubkey, total_stake) =
            get_connection_stake(connection, staked_nodes).map_or(
                (ConnectionPeerType::Unstaked, None, 0),
                |(pubkey, stake, total_stake, _max_stake, _min_stake)| {
                    // The heuristic is that the stake should be large engouh to have 1 stream pass throuh within one throttle
                    // interval during which we allow max (MAX_STREAMS_PER_MS * STREAM_THROTTLING_INTERVAL_MS) streams.
                    (ConnectionPeerType::Staked(stake), Some(pubkey), total_stake)
                },
            );

        SimpleQosParams {
            peer_type,
            max_connections_per_peer: self.max_connections_per_peer,
            remote_pubkey,
            total_stake,
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn try_add_connection(
        &self,
        client_connection_tracker: ClientConnectionTracker,
        connection: &quinn::Connection,
        params: &mut SimpleQosParams,
    ) -> impl std::future::Future<
        Output = Option<(
            Arc<AtomicU64>,
            tokio_util::sync::CancellationToken,
            Arc<ConnectionStreamCounter>,
        )>,
    > + Send {
        async move {
            const PRUNE_RANDOM_SAMPLE_SIZE: usize = 2;
            match params.peer_type() {
                ConnectionPeerType::Staked(stake) => {
                    let mut connection_table_l = self.staked_connection_table.lock().await;

                    if connection_table_l.total_size >= self.max_staked_connections {
                        let num_pruned =
                            connection_table_l.prune_random(PRUNE_RANDOM_SAMPLE_SIZE, stake);
                        self.stats
                            .num_evictions
                            .fetch_add(num_pruned, Ordering::Relaxed);
                    }

                    if connection_table_l.total_size < self.max_staked_connections {
                        if let Ok((last_update, cancel_connection, stream_counter)) = self
                            .handle_and_cache_new_connection(
                                client_connection_tracker,
                                connection,
                                connection_table_l,
                                params,
                            )
                        {
                            self.stats
                                .connection_added_from_staked_peer
                                .fetch_add(1, Ordering::Relaxed);
                            return Some((last_update, cancel_connection, stream_counter));
                        }
                    }
                    None
                }
                ConnectionPeerType::Unstaked => None,
            }
        }
    }

    fn on_stream_accepted(&self, _params: &SimpleQosParams) {}

    fn on_stream_error(&self, _params: &SimpleQosParams) {}

    fn on_stream_closed(&self, _params: &SimpleQosParams) {}

    fn max_streams_per_throttling_interval(&self, _params: &SimpleQosParams) -> u64 {
        let interval_ms = STREAM_THROTTLING_INTERVAL.as_millis() as u64;
        (self.max_streams_per_second * interval_ms / 1000).max(1)
    }

    #[allow(clippy::manual_async_fn)]
    fn remove_connection(
        &self,
        params: &SimpleQosParams,
        connection: Connection,
    ) -> impl std::future::Future<Output = usize> + Send {
        async move {
            let stable_id = connection.stable_id();
            let remote_addr = connection.remote_address();

            self.staked_connection_table.lock().await.remove_connection(
                ConnectionTableKey::new(remote_addr.ip(), params.remote_pubkey()),
                remote_addr.port(),
                stable_id,
            )
        }
    }
}
