use {
    crate::{
        nonblocking::{
            load_debt_tracker::LoadDebtTracker,
            qos::{ConnectionContext, MaxStreamsAction, OpaqueStreamerCounter, QosController},
            quic::{
                CONNECTION_CLOSE_CODE_DISALLOWED, CONNECTION_CLOSE_REASON_DISALLOWED,
                ClientConnectionTracker, ConnectionHandlerError, ConnectionPeerType,
                ConnectionTable, ConnectionTableKey, ConnectionTableType, get_connection_stake,
                update_open_connections_stat,
            },
        },
        quic::{
            DEFAULT_MAX_QUIC_CONNECTIONS_PER_STAKED_PEER,
            DEFAULT_MAX_QUIC_CONNECTIONS_PER_UNSTAKED_PEER, DEFAULT_MAX_STAKED_CONNECTIONS,
            DEFAULT_MAX_STREAMS_PER_MS, DEFAULT_MAX_UNSTAKED_CONNECTIONS, StreamerStats,
        },
        streamer::StakedNodes,
    },
    percentage::Percentage,
    quinn::Connection,
    solana_time_utils as timing,
    std::{
        future::Future,
        sync::{
            Arc, RwLock,
            atomic::{AtomicU64, AtomicUsize, Ordering},
        },
        time::Duration,
    },
    tokio::sync::{Mutex, MutexGuard},
    tokio_util::sync::CancellationToken,
};

/// Reference RTT for BDP scaling
const REFERENCE_RTT: Duration = Duration::from_millis(100);

/// Max RTT for BDP scaling. Caps MAX_STREAMS growth on high-latency links.
const MAX_RTT: Duration = Duration::from_millis(200);

/// Min RTT for BDP scaling.
const MIN_RTT: Duration = Duration::from_millis(1);

/// Backward-compat RTT floor for unsaturated staked peers.  Matches the old
/// SWQoS REFERENCE_RTT_MS so that low-RTT staked connections keep the same
/// MAX_STREAMS as before.
const MIN_RTT_STAKED_UNSATURATED: Duration = Duration::from_millis(50);

/// Base MAX_STREAMS at REFERENCE_RTT. At 100ms, 1024 matches the old SWQoS
/// ceiling for the highest-staked peers; scaling remains linear with RTT.
const DEFAULT_BASE_MAX_STREAMS_STAKED: u32 = 1024;
const DEFAULT_BASE_MAX_STREAMS_UNSTAKED: u32 = 20;

/// Per-key connection counter so compute_max_streams can divide quota evenly.
pub(crate) struct SwQosMaxStreamsStreamerCounter {
    connection_count: AtomicUsize,
}
impl OpaqueStreamerCounter for SwQosMaxStreamsStreamerCounter {}

#[derive(Clone)]
pub struct SwQosMaxStreamsConfig {
    pub max_streams_per_ms: u64,
    pub max_staked_connections: usize,
    pub max_unstaked_connections: usize,
    pub max_connections_per_staked_peer: usize,
    pub max_connections_per_unstaked_peer: usize,
    pub base_max_streams_staked: u32,
    pub base_max_streams_unstaked: u32,
}

impl Default for SwQosMaxStreamsConfig {
    fn default() -> Self {
        SwQosMaxStreamsConfig {
            max_streams_per_ms: DEFAULT_MAX_STREAMS_PER_MS,
            max_staked_connections: DEFAULT_MAX_STAKED_CONNECTIONS,
            max_unstaked_connections: DEFAULT_MAX_UNSTAKED_CONNECTIONS,
            max_connections_per_staked_peer: DEFAULT_MAX_QUIC_CONNECTIONS_PER_STAKED_PEER,
            max_connections_per_unstaked_peer: DEFAULT_MAX_QUIC_CONNECTIONS_PER_UNSTAKED_PEER,
            base_max_streams_staked: DEFAULT_BASE_MAX_STREAMS_STAKED,
            base_max_streams_unstaked: DEFAULT_BASE_MAX_STREAMS_UNSTAKED,
        }
    }
}

impl SwQosMaxStreamsConfig {
    #[cfg(feature = "dev-context-only-utils")]
    pub fn default_for_tests() -> Self {
        Self {
            max_connections_per_unstaked_peer: 1,
            max_connections_per_staked_peer: 1,
            ..Self::default()
        }
    }
}

pub struct SwQosMaxStreams {
    config: SwQosMaxStreamsConfig,
    capacity_tps: u64,
    load_tracker: Arc<LoadDebtTracker>,
    stats: Arc<StreamerStats>,
    staked_nodes: Arc<RwLock<StakedNodes>>,
    unstaked_connection_table: Arc<Mutex<ConnectionTable<SwQosMaxStreamsStreamerCounter>>>,
    staked_connection_table: Arc<Mutex<ConnectionTable<SwQosMaxStreamsStreamerCounter>>>,
}

#[derive(Clone)]
pub struct SwQosMaxStreamsConnectionContext {
    peer_type: ConnectionPeerType,
    remote_pubkey: Option<solana_pubkey::Pubkey>,
    total_stake: u64,
    in_staked_table: bool,
    last_update: Arc<AtomicU64>,
    stream_counter: Option<Arc<SwQosMaxStreamsStreamerCounter>>,
}

impl ConnectionContext for SwQosMaxStreamsConnectionContext {
    fn peer_type(&self) -> ConnectionPeerType {
        self.peer_type
    }

    fn remote_pubkey(&self) -> Option<solana_pubkey::Pubkey> {
        self.remote_pubkey
    }
}

impl SwQosMaxStreams {
    pub fn load_tracker(&self) -> &LoadDebtTracker {
        &self.load_tracker
    }

    pub fn new(
        config: SwQosMaxStreamsConfig,
        stats: Arc<StreamerStats>,
        staked_nodes: Arc<RwLock<StakedNodes>>,
        cancel: CancellationToken,
    ) -> Self {
        let max_streams_per_second = config.max_streams_per_ms * 1000;
        let burst_capacity = max_streams_per_second / 10;

        Self {
            config,
            capacity_tps: max_streams_per_second,
            load_tracker: Arc::new(LoadDebtTracker::new(
                max_streams_per_second,
                burst_capacity,
                Duration::from_millis(1),
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

    /// Core MAX_STREAMS computation (testable without a quinn::Connection).
    pub(crate) fn compute_max_streams_for_rtt(
        &self,
        context: &SwQosMaxStreamsConnectionContext,
        rtt: Duration,
        saturated: bool,
    ) -> Option<u32> {
        let scale = |base_max_streams: u32, rtt: Duration| {
            (base_max_streams as f64 * rtt.as_secs_f64() / REFERENCE_RTT.as_secs_f64()) as u32
        };

        let unsat_max = match context.peer_type {
            ConnectionPeerType::Unstaked => {
                let rtt = rtt.clamp(MIN_RTT, MAX_RTT);
                scale(self.config.base_max_streams_unstaked, rtt)
            }
            ConnectionPeerType::Staked(_) => {
                let rtt = rtt.clamp(MIN_RTT_STAKED_UNSATURATED, MAX_RTT);
                scale(self.config.base_max_streams_staked, rtt)
            }
        };

        if saturated {
            match context.peer_type {
                ConnectionPeerType::Unstaked => Some(0), // park
                ConnectionPeerType::Staked(stake) => {
                    // Saturated quota uses true BDP (no 50ms floor) for tight
                    // flow control under load.
                    let sat_rtt = rtt.clamp(MIN_RTT, MAX_RTT);
                    let share_tps = (self.capacity_tps as u128)
                        .saturating_mul(stake as u128)
                        .checked_div(context.total_stake as u128)
                        .unwrap_or(0) as u64;
                    let quota = (share_tps as f64 * sat_rtt.as_secs_f64()) as u32;
                    let num_connections = context
                        .stream_counter
                        .as_ref()
                        .map(|c| c.connection_count.load(Ordering::Relaxed))
                        .unwrap_or(1)
                        .max(1) as u32;
                    // At least 1: peers that passed the min-stake threshold in
                    // build_connection_context should not be parked.
                    let per_conn = (quota / num_connections).max(1);
                    // Don't exceed the unsaturated limit.
                    Some(per_conn.min(unsat_max.max(1)))
                }
            }
        } else {
            Some(unsat_max.max(1))
        }
    }
}

impl SwQosMaxStreams {
    fn cache_new_connection(
        &self,
        client_connection_tracker: ClientConnectionTracker,
        connection: &Connection,
        mut connection_table_l: MutexGuard<ConnectionTable<SwQosMaxStreamsStreamerCounter>>,
        conn_context: &SwQosMaxStreamsConnectionContext,
    ) -> Result<
        (
            Arc<AtomicU64>,
            CancellationToken,
            Arc<SwQosMaxStreamsStreamerCounter>,
        ),
        ConnectionHandlerError,
    > {
        let remote_addr = connection.remote_address();

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
                || {
                    Arc::new(SwQosMaxStreamsStreamerCounter {
                        connection_count: AtomicUsize::new(0),
                    })
                },
            )
        {
            stream_counter
                .connection_count
                .fetch_add(1, Ordering::Relaxed);
            update_open_connections_stat(&self.stats, &connection_table_l);
            drop(connection_table_l);

            debug!(
                "Peer type {:?}, total stake {}, from peer {}",
                conn_context.peer_type(),
                conn_context.total_stake,
                remote_addr,
            );
            Ok((last_update, cancel_connection, stream_counter))
        } else {
            self.stats
                .connection_add_failed
                .fetch_add(1, Ordering::Relaxed);
            Err(ConnectionHandlerError::ConnectionAddError)
        }
    }

    fn prune_unstaked_connection_table(
        &self,
        unstaked_connection_table: &mut ConnectionTable<SwQosMaxStreamsStreamerCounter>,
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
        connection_table: Arc<Mutex<ConnectionTable<SwQosMaxStreamsStreamerCounter>>>,
        max_connections: usize,
        conn_context: &SwQosMaxStreamsConnectionContext,
    ) -> Result<
        (
            Arc<AtomicU64>,
            CancellationToken,
            Arc<SwQosMaxStreamsStreamerCounter>,
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
}

impl QosController<SwQosMaxStreamsConnectionContext> for SwQosMaxStreams {
    fn build_connection_context(
        &self,
        connection: &Connection,
    ) -> SwQosMaxStreamsConnectionContext {
        get_connection_stake(connection, &self.staked_nodes).map_or(
            SwQosMaxStreamsConnectionContext {
                peer_type: ConnectionPeerType::Unstaked,
                total_stake: 0,
                remote_pubkey: None,
                in_staked_table: false,
                last_update: Arc::new(AtomicU64::new(timing::timestamp())),
                stream_counter: None,
            },
            |(pubkey, stake, total_stake)| {
                // Demote ultra-low-stake peers to unstaked: must earn at
                // least 1 stream per 100ms at full capacity.
                let min_stake_ratio = 1_f64 / (self.config.max_streams_per_ms * 100) as f64;
                let stake_ratio = stake as f64 / total_stake as f64;
                let peer_type = if stake_ratio < min_stake_ratio {
                    ConnectionPeerType::Unstaked
                } else {
                    ConnectionPeerType::Staked(stake)
                };

                SwQosMaxStreamsConnectionContext {
                    peer_type,
                    total_stake,
                    remote_pubkey: Some(pubkey),
                    in_staked_table: false,
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
        conn_context: &mut SwQosMaxStreamsConnectionContext,
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
                        //
                        // NOTE: This fallback can place the same staked pubkey in both tables.
                        // The per-peer stream counter is table-local, so saturated per-connection
                        // quota division can temporarily over-allocate for that pubkey (bounded
                        // in practice by two tables). This is an accepted approximation.
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

    fn compute_max_streams(
        &self,
        context: &SwQosMaxStreamsConnectionContext,
        rtt: Duration,
    ) -> MaxStreamsAction {
        let saturated = self.load_tracker.is_saturated();
        match self.compute_max_streams_for_rtt(context, rtt, saturated) {
            Some(0) => MaxStreamsAction::Park,
            Some(max_streams) => MaxStreamsAction::Set(max_streams),
            None => MaxStreamsAction::Unmanaged,
        }
    }

    fn on_stream_accepted(&self, context: &SwQosMaxStreamsConnectionContext) {
        self.load_tracker.acquire();
        if matches!(context.peer_type, ConnectionPeerType::Staked(_))
            && self.load_tracker.is_saturated()
        {
            self.stats
                .saturated_staked_streams
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    fn on_stream_error(&self, _conn_context: &SwQosMaxStreamsConnectionContext) {}

    fn on_stream_closed(&self, _conn_context: &SwQosMaxStreamsConnectionContext) {}

    #[allow(clippy::manual_async_fn)]
    fn remove_connection(
        &self,
        conn_context: &SwQosMaxStreamsConnectionContext,
        connection: Connection,
    ) -> impl Future<Output = usize> + Send {
        async move {
            if let Some(ref counter) = conn_context.stream_counter {
                counter.connection_count.fetch_sub(1, Ordering::Relaxed);
            }

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

    fn on_stream_finished(&self, context: &SwQosMaxStreamsConnectionContext) {
        context
            .last_update
            .store(timing::timestamp(), Ordering::Relaxed);
    }

    #[allow(clippy::manual_async_fn)]
    fn on_new_stream(
        &self,
        _context: &SwQosMaxStreamsConnectionContext,
    ) -> impl Future<Output = ()> + Send {
        async {}
    }

    fn max_concurrent_connections(&self) -> usize {
        // Allow 25% more connections than required to allow for handshake
        (self.config.max_staked_connections + self.config.max_unstaked_connections) * 5 / 4
    }
}

#[cfg(test)]
pub mod test {
    use super::*;

    fn make_swqos(config: SwQosMaxStreamsConfig) -> SwQosMaxStreams {
        let cancel = CancellationToken::new();
        let stats = Arc::new(StreamerStats::default());
        let staked_nodes = Arc::new(RwLock::new(crate::streamer::StakedNodes::default()));
        SwQosMaxStreams::new(config, stats, staked_nodes, cancel)
    }

    fn unstaked_context() -> SwQosMaxStreamsConnectionContext {
        SwQosMaxStreamsConnectionContext {
            peer_type: ConnectionPeerType::Unstaked,
            remote_pubkey: None,
            total_stake: 0,
            in_staked_table: false,
            last_update: Arc::new(AtomicU64::new(0)),
            stream_counter: None,
        }
    }

    fn staked_context(
        stake: u64,
        total_stake: u64,
        num_connections: usize,
    ) -> SwQosMaxStreamsConnectionContext {
        let counter = Arc::new(SwQosMaxStreamsStreamerCounter {
            connection_count: AtomicUsize::new(num_connections),
        });
        SwQosMaxStreamsConnectionContext {
            peer_type: ConnectionPeerType::Staked(stake),
            remote_pubkey: Some(solana_pubkey::Pubkey::new_unique()),
            total_stake,
            in_staked_table: true,
            last_update: Arc::new(AtomicU64::new(0)),
            stream_counter: Some(counter),
        }
    }

    /// Expected saturated quota: capacity_tps * stake/total_stake * rtt_secs / num_connections
    fn expected_saturated_quota(
        max_streams_per_ms: u64,
        stake: u64,
        total_stake: u64,
        rtt: Duration,
        num_connections: u32,
    ) -> u32 {
        let capacity_tps = max_streams_per_ms * 1000;
        let share_tps = (capacity_tps as u128 * stake as u128 / total_stake as u128) as u64;
        let quota = (share_tps as f64 * rtt.as_secs_f64()) as u32;
        (quota / num_connections).max(1)
    }

    // -- Saturated path --

    #[test]
    fn test_saturated_unstaked_returns_zero() {
        let swqos = make_swqos(SwQosMaxStreamsConfig::default());
        let ctx = unstaked_context();
        assert_eq!(
            swqos.compute_max_streams_for_rtt(&ctx, Duration::from_millis(50), true),
            Some(0),
        );
    }

    #[test]
    fn test_saturated_staked_proportional_quota() {
        // 500K/s capacity, 1% stake, 50ms RTT
        let max_streams_per_ms = 500;
        let swqos = make_swqos(SwQosMaxStreamsConfig {
            max_streams_per_ms,
            ..SwQosMaxStreamsConfig::default()
        });
        let (stake, total_stake) = (1_000, 100_000);
        let rtt = Duration::from_millis(50);
        let ctx = staked_context(stake, total_stake, 1);
        let expected = expected_saturated_quota(max_streams_per_ms, stake, total_stake, rtt, 1);
        assert_eq!(
            swqos.compute_max_streams_for_rtt(&ctx, rtt, true),
            Some(expected),
        );
    }

    #[test]
    fn test_saturated_quota_scales_with_rtt() {
        // Same stake, double RTT -> double quota (throughput stays the same)
        let max_streams_per_ms = 500;
        let swqos = make_swqos(SwQosMaxStreamsConfig {
            max_streams_per_ms,
            ..SwQosMaxStreamsConfig::default()
        });
        let (stake, total_stake) = (1_000, 100_000);
        let ctx = staked_context(stake, total_stake, 1);
        let rtt50 = Duration::from_millis(50);
        let rtt100 = Duration::from_millis(100);
        let q50 = swqos.compute_max_streams_for_rtt(&ctx, rtt50, true);
        let q100 = swqos.compute_max_streams_for_rtt(&ctx, rtt100, true);
        assert_eq!(
            q50,
            Some(expected_saturated_quota(
                max_streams_per_ms,
                stake,
                total_stake,
                rtt50,
                1
            ))
        );
        assert_eq!(
            q100,
            Some(expected_saturated_quota(
                max_streams_per_ms,
                stake,
                total_stake,
                rtt100,
                1
            ))
        );
    }

    #[test]
    fn test_saturated_quota_divided_by_connections() {
        let swqos = make_swqos(SwQosMaxStreamsConfig {
            max_streams_per_ms: 500,
            ..SwQosMaxStreamsConfig::default()
        });
        let rtt = Duration::from_millis(50);

        let ctx1 = staked_context(1_000, 100_000, 1);
        let ctx4 = staked_context(1_000, 100_000, 4);
        let q1 = swqos.compute_max_streams_for_rtt(&ctx1, rtt, true).unwrap();
        let q4 = swqos.compute_max_streams_for_rtt(&ctx4, rtt, true).unwrap();

        assert_eq!(q1, 250);
        assert_eq!(q4, 62); // 250 / 4 = 62
        assert!(q4 * 4 <= q1); // multi-conn never exceeds single-conn quota
    }

    #[test]
    fn test_saturated_tiny_stake_gets_minimum_one() {
        // Stake so small that quota rounds to 0, but .max(1) ensures at least 1
        let swqos = make_swqos(SwQosMaxStreamsConfig {
            max_streams_per_ms: 500,
            ..SwQosMaxStreamsConfig::default()
        });
        let ctx = staked_context(1, 1_000_000_000, 1);
        assert_eq!(
            swqos.compute_max_streams_for_rtt(&ctx, Duration::from_millis(50), true),
            Some(1),
        );
    }

    #[test]
    fn test_saturated_total_stake_zero_no_panic() {
        let swqos = make_swqos(SwQosMaxStreamsConfig {
            max_streams_per_ms: 500,
            ..SwQosMaxStreamsConfig::default()
        });
        let ctx = staked_context(1_000, 0, 1);
        // checked_div(0) -> unwrap_or(0) -> quota=0 -> .max(1) -> 1
        assert_eq!(
            swqos.compute_max_streams_for_rtt(&ctx, Duration::from_millis(50), true),
            Some(1),
        );
    }

    // -- Unsaturated path --

    #[test]
    fn test_unsaturated_base_at_reference_rtt() {
        let swqos = make_swqos(SwQosMaxStreamsConfig::default());
        let ctx = staked_context(1_000, 100_000, 1);
        // At REFERENCE_RTT (100ms): rtt_scale=1.0 -> base_max_streams_staked
        assert_eq!(
            swqos.compute_max_streams_for_rtt(&ctx, REFERENCE_RTT, false),
            Some(DEFAULT_BASE_MAX_STREAMS_STAKED),
        );
    }

    #[test]
    fn test_unsaturated_scales_linearly_with_rtt() {
        let swqos = make_swqos(SwQosMaxStreamsConfig::default());
        let ctx = staked_context(1_000, 100_000, 1);
        let q100 = swqos
            .compute_max_streams_for_rtt(&ctx, Duration::from_millis(100), false)
            .unwrap();
        let q200 = swqos
            .compute_max_streams_for_rtt(&ctx, Duration::from_millis(200), false)
            .unwrap();
        assert_eq!(q100, DEFAULT_BASE_MAX_STREAMS_STAKED);
        assert_eq!(q200, DEFAULT_BASE_MAX_STREAMS_STAKED * 2);
        // Ratio should be 2x
        assert!((q200 as f64 / q100 as f64 - 2.0).abs() < 0.01);
    }

    #[test]
    fn test_unsaturated_low_rtt_clamped_for_staked() {
        let swqos = make_swqos(SwQosMaxStreamsConfig::default());
        let ctx = staked_context(1_000, 100_000, 1);
        // Staked RTT is clamped to MIN_RTT_STAKED_UNSATURATED (50ms).
        // 1024 * 50/100 = 512
        assert_eq!(
            swqos.compute_max_streams_for_rtt(&ctx, Duration::from_millis(5), false),
            Some(512),
        );
    }

    #[test]
    fn test_unsaturated_low_rtt_scales_down_for_unstaked() {
        let swqos = make_swqos(SwQosMaxStreamsConfig::default());
        let ctx = unstaked_context();
        // Unstaked uses true BDP, no 50ms floor.
        // 20 * 5/100 = 1
        assert_eq!(
            swqos.compute_max_streams_for_rtt(&ctx, Duration::from_millis(5), false),
            Some(1),
        );
    }

    #[test]
    fn test_unsaturated_rtt_clamped_at_max() {
        let swqos = make_swqos(SwQosMaxStreamsConfig::default());
        let ctx = staked_context(1_000, 100_000, 1);
        // 500ms RTT gets clamped to MAX_RTT (200ms) -> scale = 2.0
        let q_500 = swqos.compute_max_streams_for_rtt(&ctx, Duration::from_millis(500), false);
        let q_max = swqos.compute_max_streams_for_rtt(&ctx, MAX_RTT, false);
        assert_eq!(q_500, q_max);
        assert_eq!(q_max, Some(DEFAULT_BASE_MAX_STREAMS_STAKED * 2));
    }

    #[test]
    fn test_unsaturated_ignores_stake() {
        // In unsaturated mode, quota depends only on RTT, not stake
        let swqos = make_swqos(SwQosMaxStreamsConfig::default());
        let rtt = Duration::from_millis(100);
        let big = staked_context(900_000, 1_000_000, 1);
        let small = staked_context(1_000, 1_000_000, 1);
        assert_eq!(
            swqos.compute_max_streams_for_rtt(&big, rtt, false),
            swqos.compute_max_streams_for_rtt(&small, rtt, false),
        );
    }

    #[test]
    fn test_unsaturated_unstaked_uses_legacy_base() {
        let swqos = make_swqos(SwQosMaxStreamsConfig {
            base_max_streams_unstaked: 128,
            ..SwQosMaxStreamsConfig::default()
        });
        let ctx = unstaked_context();
        // At REFERENCE_RTT (100ms): rtt_scale=1.0 -> base_max_streams_unstaked
        assert_eq!(
            swqos.compute_max_streams_for_rtt(&ctx, REFERENCE_RTT, false),
            Some(128),
        );
    }

    // -- Hard cap --

    #[test]
    fn test_saturated_quota_capped_by_unsaturated_max() {
        // 100% stake at 50ms: proportional = 25000, capped at unsaturated max = 512.
        let swqos = make_swqos(SwQosMaxStreamsConfig {
            max_streams_per_ms: 500,
            ..SwQosMaxStreamsConfig::default()
        });
        let ctx = staked_context(1_000_000, 1_000_000, 1);
        assert_eq!(
            swqos.compute_max_streams_for_rtt(&ctx, Duration::from_millis(50), true),
            Some(512),
        );
    }

    #[test]
    fn test_saturated_quota_proportional_small_stake() {
        // 1% stake at 50ms RTT: quota = 500000 * 0.01 * 0.05 = 250.
        let swqos = make_swqos(SwQosMaxStreamsConfig {
            max_streams_per_ms: 500,
            ..SwQosMaxStreamsConfig::default()
        });
        let ctx = staked_context(1_000, 100_000, 1);
        assert_eq!(
            swqos.compute_max_streams_for_rtt(&ctx, Duration::from_millis(50), true),
            Some(250),
        );
    }

    #[test]
    fn test_saturated_large_lamport_stakes_preserve_proportionality() {
        // Regression: without u128, capacity_tps * stake overflows above ~37K SOL,
        // collapsing all large stakers to the same share_tps.
        let swqos = make_swqos(SwQosMaxStreamsConfig {
            max_streams_per_ms: 500,
            ..SwQosMaxStreamsConfig::default()
        });
        const LAMPORTS_PER_SOL: u64 = 1_000_000_000;
        let stake_large = 10_000_000 * LAMPORTS_PER_SOL; // 10M SOL
        let stake_small = 10_000 * LAMPORTS_PER_SOL; // 10K SOL
        let total = stake_large + stake_small;
        let rtt = Duration::from_millis(100);

        let ctx_large = staked_context(stake_large, total, 1);
        let ctx_small = staked_context(stake_small, total, 1);

        // 10M SOL -> quota capped at DEFAULT_BASE_MAX_STREAMS_STAKED; 10K SOL -> quota 49.
        // Without u128: 10M SOL overflows to quota ~ 183, destroying proportionality.
        let q_large = swqos
            .compute_max_streams_for_rtt(&ctx_large, rtt, true)
            .unwrap();
        let q_small = swqos
            .compute_max_streams_for_rtt(&ctx_small, rtt, true)
            .unwrap();
        // Large staker hits the cap, proving no overflow.
        assert_eq!(q_large, DEFAULT_BASE_MAX_STREAMS_STAKED);
        assert_eq!(q_small, 49);
        assert!(
            q_large > q_small * 20,
            "large/small ratio={}, expected large staker to dominate",
            q_large / q_small
        );
    }
}
