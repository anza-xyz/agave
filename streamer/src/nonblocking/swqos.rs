use {
    crate::{
        nonblocking::{
            quic::{
                get_connection_stake, update_open_connections_stat, ClientConnectionTracker,
                ConnectionContext, ConnectionHandlerError, ConnectionPeerType, ConnectionTable,
                ConnectionTableKey, ConnectionTableType, QosController,
                CONNECTION_CLOSE_CODE_DISALLOWED, CONNECTION_CLOSE_CODE_EXCEED_MAX_STREAM_COUNT,
                CONNECTION_CLOSE_REASON_DISALLOWED,
                CONNECTION_CLOSE_REASON_EXCEED_MAX_STREAM_COUNT,
            },
            stream_throttle::{
                ConnectionStreamCounter, StakedStreamLoadEMA, STREAM_THROTTLING_INTERVAL_MS,
            },
        },
        quic::StreamerStats,
        streamer::StakedNodes,
    },
    percentage::Percentage,
    quinn::{Connection, VarInt, VarIntBoundsExceeded},
    solana_packet::PACKET_DATA_SIZE,
    solana_quic_definitions::{
        QUIC_MAX_STAKED_CONCURRENT_STREAMS, QUIC_MAX_STAKED_RECEIVE_WINDOW_RATIO,
        QUIC_MAX_UNSTAKED_CONCURRENT_STREAMS, QUIC_MIN_STAKED_CONCURRENT_STREAMS,
        QUIC_MIN_STAKED_RECEIVE_WINDOW_RATIO, QUIC_TOTAL_STAKED_CONCURRENT_STREAMS,
        QUIC_UNSTAKED_RECEIVE_WINDOW_RATIO,
    },
    solana_time_utils as timing,
    std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc, RwLock,
    },
    tokio::sync::{Mutex, MutexGuard},
    tokio_util::sync::CancellationToken,
};

pub struct SwQos {
    max_staked_connections: usize,
    max_unstaked_connections: usize,
    max_connections_per_peer: usize,
    staked_stream_load_ema: Arc<StakedStreamLoadEMA>,
    wait_for_chunk_timeout: std::time::Duration,
    stats: Arc<StreamerStats>,
    unstaked_connection_table: Arc<Mutex<ConnectionTable>>,
    staked_connection_table: Arc<Mutex<ConnectionTable>>,
}

// QoS Params for Stake weighted QoS
#[derive(Clone)]
pub struct SwQosParams {
    peer_type: ConnectionPeerType,
    max_connections_per_peer: usize,
    max_stake: u64,
    min_stake: u64,
    remote_pubkey: Option<solana_pubkey::Pubkey>,
    total_stake: u64,
    in_staked_table: bool,
}

impl ConnectionContext for SwQosParams {
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

impl SwQos {
    pub fn new(
        max_streams_per_ms: u64,
        max_staked_connections: usize,
        max_unstaked_connections: usize,
        max_connections_per_peer: usize,
        wait_for_chunk_timeout: std::time::Duration,
        stats: Arc<StreamerStats>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            max_staked_connections,
            max_unstaked_connections,
            max_connections_per_peer,
            wait_for_chunk_timeout,
            staked_stream_load_ema: Arc::new(StakedStreamLoadEMA::new(
                stats.clone(),
                max_unstaked_connections,
                max_streams_per_ms,
            )),
            stats,
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

/// Calculate the ratio for per connection receive window from a staked peer
fn compute_receive_window_ratio_for_staked_node(max_stake: u64, min_stake: u64, stake: u64) -> u64 {
    // Testing shows the maximum througput from a connection is achieved at receive_window =
    // PACKET_DATA_SIZE * 10. Beyond that, there is not much gain. We linearly map the
    // stake to the ratio range from QUIC_MIN_STAKED_RECEIVE_WINDOW_RATIO to
    // QUIC_MAX_STAKED_RECEIVE_WINDOW_RATIO. Where the linear algebra of finding the ratio 'r'
    // for stake 's' is,
    // r(s) = a * s + b. Given the max_stake, min_stake, max_ratio, min_ratio, we can find
    // a and b.

    if stake > max_stake {
        return QUIC_MAX_STAKED_RECEIVE_WINDOW_RATIO;
    }

    let max_ratio = QUIC_MAX_STAKED_RECEIVE_WINDOW_RATIO;
    let min_ratio = QUIC_MIN_STAKED_RECEIVE_WINDOW_RATIO;
    if max_stake > min_stake {
        let a = (max_ratio - min_ratio) as f64 / (max_stake - min_stake) as f64;
        let b = max_ratio as f64 - ((max_stake as f64) * a);
        let ratio = (a * stake as f64) + b;
        ratio.round() as u64
    } else {
        QUIC_MAX_STAKED_RECEIVE_WINDOW_RATIO
    }
}

fn compute_recieve_window(
    max_stake: u64,
    min_stake: u64,
    peer_type: ConnectionPeerType,
) -> Result<VarInt, VarIntBoundsExceeded> {
    match peer_type {
        ConnectionPeerType::Unstaked => {
            VarInt::from_u64(PACKET_DATA_SIZE as u64 * QUIC_UNSTAKED_RECEIVE_WINDOW_RATIO)
        }
        ConnectionPeerType::Staked(peer_stake) => {
            let ratio =
                compute_receive_window_ratio_for_staked_node(max_stake, min_stake, peer_stake);
            VarInt::from_u64(PACKET_DATA_SIZE as u64 * ratio)
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
    fn handle_and_cache_new_connection(
        &self,
        client_connection_tracker: ClientConnectionTracker,
        connection: &Connection,
        mut connection_table_l: MutexGuard<ConnectionTable>,
        params: &SwQosParams,
    ) -> Result<
        (
            Arc<AtomicU64>,
            CancellationToken,
            Arc<ConnectionStreamCounter>,
        ),
        ConnectionHandlerError,
    > {
        if let Ok(max_uni_streams) = VarInt::from_u64(compute_max_allowed_uni_streams(
            params.peer_type(),
            params.total_stake,
        ) as u64)
        {
            let remote_addr = connection.remote_address();
            let receive_window =
                compute_recieve_window(params.max_stake, params.min_stake, params.peer_type());

            debug!(
                "Peer type {:?}, total stake {}, max streams {} receive_window {:?} from peer {}",
                params.peer_type(),
                params.total_stake,
                max_uni_streams.into_inner(),
                receive_window,
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
                update_open_connections_stat(&self.stats, &connection_table_l);
                drop(connection_table_l);

                if let Ok(receive_window) = receive_window {
                    connection.set_receive_window(receive_window);
                }
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
        unstaked_connection_table: &mut ConnectionTable,
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
        connection_table: Arc<Mutex<ConnectionTable>>,
        max_connections: usize,
        params: &SwQosParams,
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
            self.handle_and_cache_new_connection(
                client_connection_tracker,
                connection,
                connection_table,
                params,
            )
        } else {
            connection.close(
                CONNECTION_CLOSE_CODE_DISALLOWED.into(),
                CONNECTION_CLOSE_REASON_DISALLOWED,
            );
            Err(ConnectionHandlerError::ConnectionAddError)
        }
    }
}

impl QosController<SwQosParams> for SwQos {
    fn derive_connection_context(
        &self,
        connection: &Connection,
        staked_nodes: &RwLock<StakedNodes>,
    ) -> SwQosParams {
        get_connection_stake(connection, staked_nodes).map_or(
            SwQosParams {
                peer_type: ConnectionPeerType::Unstaked,
                max_connections_per_peer: self.max_connections_per_peer,
                max_stake: 0,
                min_stake: 0,
                total_stake: 0,
                remote_pubkey: None,
                in_staked_table: false,
            },
            |(pubkey, stake, total_stake, max_stake, min_stake)| {
                // The heuristic is that the stake should be large engouh to have 1 stream pass throuh within one throttle
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

                SwQosParams {
                    peer_type,
                    max_connections_per_peer: self.max_connections_per_peer,
                    max_stake,
                    min_stake,
                    total_stake,
                    remote_pubkey: Some(pubkey),
                    in_staked_table: false,
                }
            },
        )
    }

    #[allow(clippy::manual_async_fn)]
    fn try_add_connection(
        &self,
        client_connection_tracker: ClientConnectionTracker,
        connection: &quinn::Connection,
        params: &mut SwQosParams,
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
                            .num_evictions_staked
                            .fetch_add(num_pruned, Ordering::Relaxed);
                        update_open_connections_stat(&self.stats, &connection_table_l);
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
                            params.in_staked_table = true;
                            return Some((last_update, cancel_connection, stream_counter));
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
                                self.max_unstaked_connections,
                                params,
                            )
                            .await
                        {
                            self.stats
                                .connection_added_from_staked_peer
                                .fetch_add(1, Ordering::Relaxed);
                            params.in_staked_table = false;
                            return Some((last_update, cancel_connection, stream_counter));
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
                            self.max_unstaked_connections,
                            params,
                        )
                        .await
                    {
                        self.stats
                            .connection_added_from_unstaked_peer
                            .fetch_add(1, Ordering::Relaxed);
                        params.in_staked_table = false;
                        return Some((last_update, cancel_connection, stream_counter));
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

    fn on_stream_accepted(&self, params: &SwQosParams) {
        self.staked_stream_load_ema.increment_load(params.peer_type);
    }

    fn on_stream_error(&self, _params: &SwQosParams) {
        self.staked_stream_load_ema.update_ema_if_needed();
    }

    fn on_stream_closed(&self, _params: &SwQosParams) {
        self.staked_stream_load_ema.update_ema_if_needed();
    }

    fn max_streams_per_throttling_interval(&self, params: &SwQosParams) -> u64 {
        self.staked_stream_load_ema
            .available_load_capacity_in_throttling_duration(params.peer_type, params.total_stake)
    }

    #[allow(clippy::manual_async_fn)]
    fn remove_connection(
        &self,
        params: &SwQosParams,
        connection: Connection,
    ) -> impl std::future::Future<Output = usize> + Send {
        async move {
            let mut lock = if params.in_staked_table {
                self.staked_connection_table.lock().await
            } else {
                self.unstaked_connection_table.lock().await
            };

            let stable_id = connection.stable_id();
            let remote_addr = connection.remote_address();

            lock.remove_connection(
                ConnectionTableKey::new(remote_addr.ip(), params.remote_pubkey()),
                remote_addr.port(),
                stable_id,
            )
        }
    }

    fn wait_for_chunk_timeout(&self) -> std::time::Duration {
        self.wait_for_chunk_timeout
    }
}

#[cfg(test)]
pub mod test {
    use super::*;

    #[test]
    fn test_cacluate_receive_window_ratio_for_staked_node() {
        let mut max_stake = 10000;
        let mut min_stake = 0;
        let ratio = compute_receive_window_ratio_for_staked_node(max_stake, min_stake, min_stake);
        assert_eq!(ratio, QUIC_MIN_STAKED_RECEIVE_WINDOW_RATIO);

        let ratio = compute_receive_window_ratio_for_staked_node(max_stake, min_stake, max_stake);
        let max_ratio = QUIC_MAX_STAKED_RECEIVE_WINDOW_RATIO;
        assert_eq!(ratio, max_ratio);

        let ratio =
            compute_receive_window_ratio_for_staked_node(max_stake, min_stake, max_stake / 2);
        let average_ratio =
            (QUIC_MAX_STAKED_RECEIVE_WINDOW_RATIO + QUIC_MIN_STAKED_RECEIVE_WINDOW_RATIO) / 2;
        assert_eq!(ratio, average_ratio);

        max_stake = 10000;
        min_stake = 10000;
        let ratio = compute_receive_window_ratio_for_staked_node(max_stake, min_stake, max_stake);
        assert_eq!(ratio, max_ratio);

        max_stake = 0;
        min_stake = 0;
        let ratio = compute_receive_window_ratio_for_staked_node(max_stake, min_stake, max_stake);
        assert_eq!(ratio, max_ratio);

        max_stake = 1000;
        min_stake = 10;
        let ratio =
            compute_receive_window_ratio_for_staked_node(max_stake, min_stake, max_stake + 10);
        assert_eq!(ratio, max_ratio);
    }

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
