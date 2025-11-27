use {
    crate::{
        nonblocking::{
            qos::{ConnectionContext, QosController},
            quic::{
                get_connection_stake, update_open_connections_stat, ClientConnectionTracker,
                ConnectionHandlerError, ConnectionPeerType, ConnectionTable, ConnectionTableKey,
                ConnectionTableType, CONNECTION_CLOSE_CODE_DISALLOWED,
                CONNECTION_CLOSE_CODE_EXCEED_MAX_STREAM_COUNT, CONNECTION_CLOSE_REASON_DISALLOWED,
                CONNECTION_CLOSE_REASON_EXCEED_MAX_STREAM_COUNT,
            },
            stream_throttle::{
                throttle_stream, ConnectionStreamCounter, StakedStreamLoadEMA,
                STREAM_THROTTLING_INTERVAL_MS,
            },
        },
        quic::{
            StreamerStats, DEFAULT_MAX_QUIC_CONNECTIONS_PER_STAKED_PEER,
            DEFAULT_MAX_QUIC_CONNECTIONS_PER_UNSTAKED_PEER, DEFAULT_MAX_STAKED_CONNECTIONS,
            DEFAULT_MAX_STREAMS_PER_MS, DEFAULT_MAX_UNSTAKED_CONNECTIONS,
        },
        streamer::StakedNodes,
    },
    percentage::Percentage,
    quinn::{Connection, VarInt},
    solana_quic_definitions::{
        QUIC_MAX_STAKED_CONCURRENT_STREAMS, QUIC_MAX_UNSTAKED_CONCURRENT_STREAMS,
        QUIC_MIN_STAKED_CONCURRENT_STREAMS, QUIC_TOTAL_STAKED_CONCURRENT_STREAMS,
    },
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
pub struct SwQosConfig {
    pub max_streams_per_ms: u64,
    pub max_staked_connections: usize,
    pub max_unstaked_connections: usize,
    pub max_connections_per_staked_peer: usize,
    pub max_connections_per_unstaked_peer: usize,
}

impl Default for SwQosConfig {
    fn default() -> Self {
        SwQosConfig {
            max_streams_per_ms: DEFAULT_MAX_STREAMS_PER_MS,
            max_staked_connections: DEFAULT_MAX_STAKED_CONNECTIONS,
            max_unstaked_connections: DEFAULT_MAX_UNSTAKED_CONNECTIONS,
            max_connections_per_staked_peer: DEFAULT_MAX_QUIC_CONNECTIONS_PER_STAKED_PEER,
            max_connections_per_unstaked_peer: DEFAULT_MAX_QUIC_CONNECTIONS_PER_UNSTAKED_PEER,
        }
    }
}

impl SwQosConfig {
    #[cfg(feature = "dev-context-only-utils")]
    pub fn default_for_tests() -> Self {
        Self {
            max_connections_per_unstaked_peer: 1,
            max_connections_per_staked_peer: 1,
            ..Self::default()
        }
    }
}

pub struct SwQos {
    config: SwQosConfig,
    staked_stream_load_ema: Arc<StakedStreamLoadEMA>,
    stats: Arc<StreamerStats>,
    staked_nodes: Arc<RwLock<StakedNodes>>,
    unstaked_connection_table: Arc<Mutex<ConnectionTable<ConnectionStreamCounter>>>,
    staked_connection_table: Arc<Mutex<ConnectionTable<ConnectionStreamCounter>>>,
}

// QoS Params for Stake weighted QoS
#[derive(Clone)]
pub struct SwQosConnectionContext {
    peer_type: ConnectionPeerType,
    remote_pubkey: Option<solana_pubkey::Pubkey>,
    total_stake: u64,
    in_staked_table: bool,
    last_update: Arc<AtomicU64>,
    remote_address: std::net::SocketAddr,
    stream_counter: Option<Arc<ConnectionStreamCounter>>,
}

impl ConnectionContext for SwQosConnectionContext {
    fn peer_type(&self) -> ConnectionPeerType {
        self.peer_type
    }

    fn remote_pubkey(&self) -> Option<solana_pubkey::Pubkey> {
        self.remote_pubkey
    }
}

impl SwQos {
    pub fn new(
        config: SwQosConfig,
        stats: Arc<StreamerStats>,
        staked_nodes: Arc<RwLock<StakedNodes>>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            config: config.clone(),
            staked_stream_load_ema: Arc::new(StakedStreamLoadEMA::new(
                stats.clone(),
                config.max_unstaked_connections,
                config.max_streams_per_ms,
            )),
            stats,
            staked_nodes,
            unstaked_connection_table: Arc::new(Mutex::new(ConnectionTable::new(
                ConnectionTableType::Unstaked,
                cancel.clone(),
            ))),
            staked_connection_table: Arc::new(Mutex::new(ConnectionTable::new(
                ConnectionTableType::Staked,
                cancel,
            ))),
        }
    }
}

fn compute_max_allowed_uni_streams(peer_type: ConnectionPeerType, total_stake: u64) -> usize {
    match peer_type {
        ConnectionPeerType::Staked(peer_stake) => {
            // No checked math for f64 type. So let's explicitly check for 0 here
            if total_stake == 0 || peer_stake > total_stake {
                warn!(
                    "Invalid stake values: peer_stake: {peer_stake:?}, total_stake: \
                     {total_stake:?}"
                );

                QUIC_MIN_STAKED_CONCURRENT_STREAMS
            } else {
                let delta = (QUIC_TOTAL_STAKED_CONCURRENT_STREAMS
                    - QUIC_MIN_STAKED_CONCURRENT_STREAMS) as f64;

                (((peer_stake as f64 / total_stake as f64) * delta) as usize
                    + QUIC_MIN_STAKED_CONCURRENT_STREAMS)
                    .clamp(
                        QUIC_MIN_STAKED_CONCURRENT_STREAMS,
                        QUIC_MAX_STAKED_CONCURRENT_STREAMS,
                    )
            }
        }
        ConnectionPeerType::Unstaked => QUIC_MAX_UNSTAKED_CONCURRENT_STREAMS,
    }
}

impl SwQos {
    fn cache_new_connection(
        &self,
        client_connection_tracker: ClientConnectionTracker,
        connection: &Connection,
        mut connection_table_l: MutexGuard<ConnectionTable<ConnectionStreamCounter>>,
        conn_context: &SwQosConnectionContext,
    ) -> Result<
        (
            Arc<AtomicU64>,
            CancellationToken,
            Arc<ConnectionStreamCounter>,
        ),
        ConnectionHandlerError,
    > {
        if let Ok(max_uni_streams) = VarInt::from_u64(compute_max_allowed_uni_streams(
            conn_context.peer_type(),
            conn_context.total_stake,
        ) as u64)
        {
            let remote_addr = connection.remote_address();

            debug!(
                "Peer type {:?}, total stake {}, max streams {} from peer {}",
                conn_context.peer_type(),
                conn_context.total_stake,
                max_uni_streams.into_inner(),
                remote_addr,
            );

            let max_connections_per_peer = match conn_context.peer_type() {
                ConnectionPeerType::Unstaked => self.config.max_connections_per_unstaked_peer,
                ConnectionPeerType::Staked(_) => self.config.max_connections_per_staked_peer,
            };
            if let Some((last_update, cancel_connection, stream_counter)) = connection_table_l
                .try_add_connection(
                    ConnectionTableKey::new(remote_addr.ip(), conn_context.remote_pubkey),
                    remote_addr.port(),
                    client_connection_tracker,
                    Some(connection.clone()),
                    conn_context.peer_type(),
                    conn_context.last_update.clone(),
                    max_connections_per_peer,
                    || Arc::new(ConnectionStreamCounter::new()),
                )
            {
                update_open_connections_stat(&self.stats, &connection_table_l);
                drop(connection_table_l);

                connection.set_max_concurrent_uni_streams(max_uni_streams);

                Ok((last_update, cancel_connection, stream_counter))
            } else {
                self.stats
                    .connection_add_failed
                    .fetch_add(1, Ordering::Relaxed);
                Err(ConnectionHandlerError::ConnectionAddError)
            }
        } else {
            connection.close(
                CONNECTION_CLOSE_CODE_EXCEED_MAX_STREAM_COUNT.into(),
                CONNECTION_CLOSE_REASON_EXCEED_MAX_STREAM_COUNT,
            );
            self.stats
                .connection_add_failed_invalid_stream_count
                .fetch_add(1, Ordering::Relaxed);
            Err(ConnectionHandlerError::MaxStreamError)
        }
    }

    fn prune_unstaked_connection_table(
        &self,
        unstaked_connection_table: &mut ConnectionTable<ConnectionStreamCounter>,
        max_unstaked_connections: usize,
        stats: Arc<StreamerStats>,
    ) {
        if unstaked_connection_table.total_size >= max_unstaked_connections {
            const PRUNE_TABLE_TO_PERCENTAGE: u8 = 90;
            let max_percentage_full = Percentage::from(PRUNE_TABLE_TO_PERCENTAGE);

            let max_connections = max_percentage_full.apply_to(max_unstaked_connections);
            let num_pruned = unstaked_connection_table.prune_oldest(max_connections);
            stats
                .num_evictions_unstaked
                .fetch_add(num_pruned, Ordering::Relaxed);
        }
    }

    async fn prune_unstaked_connections_and_add_new_connection(
        &self,
        client_connection_tracker: ClientConnectionTracker,
        connection: &Connection,
        connection_table: Arc<Mutex<ConnectionTable<ConnectionStreamCounter>>>,
        max_connections: usize,
        conn_context: &SwQosConnectionContext,
    ) -> Result<
        (
            Arc<AtomicU64>,
            CancellationToken,
            Arc<ConnectionStreamCounter>,
        ),
        ConnectionHandlerError,
    > {
        let stats = self.stats.clone();
        if max_connections > 0 {
            let mut connection_table = connection_table.lock().await;
            self.prune_unstaked_connection_table(&mut connection_table, max_connections, stats);
            self.cache_new_connection(
                client_connection_tracker,
                connection,
                connection_table,
                conn_context,
            )
        } else {
            connection.close(
                CONNECTION_CLOSE_CODE_DISALLOWED.into(),
                CONNECTION_CLOSE_REASON_DISALLOWED,
            );
            Err(ConnectionHandlerError::ConnectionAddError)
        }
    }

    fn max_streams_per_throttling_interval(&self, conn_context: &SwQosConnectionContext) -> u64 {
        self.staked_stream_load_ema
            .available_load_capacity_in_throttling_duration(
                conn_context.peer_type,
                conn_context.total_stake,
            )
    }
}

impl QosController<SwQosConnectionContext> for SwQos {
    fn build_connection_context(&self, connection: &Connection) -> SwQosConnectionContext {
        get_connection_stake(connection, &self.staked_nodes).map_or(
            SwQosConnectionContext {
                peer_type: ConnectionPeerType::Unstaked,
                total_stake: 0,
                remote_pubkey: None,
                in_staked_table: false,
                remote_address: connection.remote_address(),
                stream_counter: None,
                last_update: Arc::new(AtomicU64::new(timing::timestamp())),
            },
            |(pubkey, stake, total_stake)| {
                // The heuristic is that the stake should be large enough to have 1 stream pass through within one throttle
                // interval during which we allow max (MAX_STREAMS_PER_MS * STREAM_THROTTLING_INTERVAL_MS) streams.

                let peer_type = {
                    let max_streams_per_ms = self.staked_stream_load_ema.max_streams_per_ms();
                    let min_stake_ratio =
                        1_f64 / (max_streams_per_ms * STREAM_THROTTLING_INTERVAL_MS) as f64;
                    let stake_ratio = stake as f64 / total_stake as f64;
                    if stake_ratio < min_stake_ratio {
                        // If it is a staked connection with ultra low stake ratio, treat it as unstaked.
                        ConnectionPeerType::Unstaked
                    } else {
                        ConnectionPeerType::Staked(stake)
                    }
                };

                SwQosConnectionContext {
                    peer_type,
                    total_stake,
                    remote_pubkey: Some(pubkey),
                    in_staked_table: false,
                    remote_address: connection.remote_address(),
                    last_update: Arc::new(AtomicU64::new(timing::timestamp())),
                    stream_counter: None,
                }
            },
        )
    }

    #[allow(clippy::manual_async_fn)]
    fn try_add_connection(
        &self,
        client_connection_tracker: ClientConnectionTracker,
        connection: &quinn::Connection,
        conn_context: &mut SwQosConnectionContext,
    ) -> impl Future<Output = Option<CancellationToken>> + Send {
        async move {
            const PRUNE_RANDOM_SAMPLE_SIZE: usize = 2;

            match conn_context.peer_type() {
                ConnectionPeerType::Staked(stake) => {
                    let mut connection_table_l = self.staked_connection_table.lock().await;

                    if connection_table_l.total_size >= self.config.max_staked_connections {
                        let num_pruned =
                            connection_table_l.prune_random(PRUNE_RANDOM_SAMPLE_SIZE, stake);
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
                            conn_context.in_staked_table = true;
                            conn_context.last_update = last_update;
                            conn_context.stream_counter = Some(stream_counter);
                            return Some(cancel_connection);
                        }
                    } else {
                        // If we couldn't prune a connection in the staked connection table, let's
                        // put this connection in the unstaked connection table. If needed, prune a
                        // connection from the unstaked connection table.
                        if let Ok((last_update, cancel_connection, stream_counter)) = self
                            .prune_unstaked_connections_and_add_new_connection(
                                client_connection_tracker,
                                connection,
                                self.unstaked_connection_table.clone(),
                                self.config.max_unstaked_connections,
                                conn_context,
                            )
                            .await
                        {
                            self.stats
                                .connection_added_from_staked_peer
                                .fetch_add(1, Ordering::Relaxed);
                            conn_context.in_staked_table = false;
                            conn_context.last_update = last_update;
                            conn_context.stream_counter = Some(stream_counter);
                            return Some(cancel_connection);
                        } else {
                            self.stats
                                .connection_add_failed_on_pruning
                                .fetch_add(1, Ordering::Relaxed);
                            self.stats
                                .connection_add_failed_staked_node
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                ConnectionPeerType::Unstaked => {
                    if let Ok((last_update, cancel_connection, stream_counter)) = self
                        .prune_unstaked_connections_and_add_new_connection(
                            client_connection_tracker,
                            connection,
                            self.unstaked_connection_table.clone(),
                            self.config.max_unstaked_connections,
                            conn_context,
                        )
                        .await
                    {
                        self.stats
                            .connection_added_from_unstaked_peer
                            .fetch_add(1, Ordering::Relaxed);
                        conn_context.in_staked_table = false;
                        conn_context.last_update = last_update;
                        conn_context.stream_counter = Some(stream_counter);
                        return Some(cancel_connection);
                    } else {
                        self.stats
                            .connection_add_failed_unstaked_node
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
            }

            None
        }
    }

    fn on_stream_accepted(&self, conn_context: &SwQosConnectionContext) {
        self.staked_stream_load_ema
            .increment_load(conn_context.peer_type);
        conn_context
            .stream_counter
            .as_ref()
            .unwrap()
            .stream_count
            .fetch_add(1, Ordering::Relaxed);
    }

    fn on_stream_error(&self, _conn_context: &SwQosConnectionContext) {
        self.staked_stream_load_ema.update_ema_if_needed();
    }

    fn on_stream_closed(&self, _conn_context: &SwQosConnectionContext) {
        self.staked_stream_load_ema.update_ema_if_needed();
    }

    #[allow(clippy::manual_async_fn)]
    fn remove_connection(
        &self,
        conn_context: &SwQosConnectionContext,
        connection: Connection,
    ) -> impl Future<Output = usize> + Send {
        async move {
            let mut lock = if conn_context.in_staked_table {
                self.staked_connection_table.lock().await
            } else {
                self.unstaked_connection_table.lock().await
            };

            let stable_id = connection.stable_id();
            let remote_addr = connection.remote_address();

            let removed_count = lock.remove_connection(
                ConnectionTableKey::new(remote_addr.ip(), conn_context.remote_pubkey()),
                remote_addr.port(),
                stable_id,
            );
            update_open_connections_stat(&self.stats, &lock);
            removed_count
        }
    }

    fn on_stream_finished(&self, context: &SwQosConnectionContext) {
        context
            .last_update
            .store(timing::timestamp(), Ordering::Relaxed);
    }

    #[allow(clippy::manual_async_fn)]
    fn on_new_stream(&self, context: &SwQosConnectionContext) -> impl Future<Output = ()> + Send {
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

        (self.config.max_staked_connections + self.config.max_unstaked_connections) * 5 / 4
    }
}

#[cfg(test)]
pub mod test {
    use super::*;

    #[test]
    fn test_max_allowed_uni_streams() {
        assert_eq!(
            compute_max_allowed_uni_streams(ConnectionPeerType::Unstaked, 0),
            QUIC_MAX_UNSTAKED_CONCURRENT_STREAMS
        );
        assert_eq!(
            compute_max_allowed_uni_streams(ConnectionPeerType::Staked(10), 0),
            QUIC_MIN_STAKED_CONCURRENT_STREAMS
        );
        let delta =
            (QUIC_TOTAL_STAKED_CONCURRENT_STREAMS - QUIC_MIN_STAKED_CONCURRENT_STREAMS) as f64;
        assert_eq!(
            compute_max_allowed_uni_streams(ConnectionPeerType::Staked(1000), 10000),
            QUIC_MAX_STAKED_CONCURRENT_STREAMS,
        );
        assert_eq!(
            compute_max_allowed_uni_streams(ConnectionPeerType::Staked(100), 10000),
            ((delta / (100_f64)) as usize + QUIC_MIN_STAKED_CONCURRENT_STREAMS)
                .min(QUIC_MAX_STAKED_CONCURRENT_STREAMS)
        );
        assert_eq!(
            compute_max_allowed_uni_streams(ConnectionPeerType::Unstaked, 10000),
            QUIC_MAX_UNSTAKED_CONCURRENT_STREAMS
        );
    }
}
